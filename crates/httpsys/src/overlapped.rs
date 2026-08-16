//! `OVERLAPPED` plumbing shared by every async http.sys operation.
//!
//! The contract: an [`Op`] allocates an [`OverlappedWrap`] and immediately
//! transfers one strong reference to the kernel via [`Arc::into_raw`] (as the
//! `OVERLAPPED` pointer). Exactly one of three things then happens:
//!
//! * the kernel rejects the submission — [`Op::finish`] reclaims the reference;
//! * the kernel completes it inline and the queue handle has
//!   `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` — [`Op::finish`] reclaims it;
//! * the kernel queues it — an [`IoPort`](crate::iocp::IoPort) completion
//!   thread reclaims it and wakes the [`AsyncOverlappedFuture`].
//!
//! Dropping the future before completion cancels the operation with
//! `CancelIoEx` and blocks until the completion has fired, so the kernel never
//! writes into a buffer that Rust has already reused.

use std::{
    cell::UnsafeCell,
    ffi::c_void,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    task::{Context, Poll},
};

use atomic_waker::AtomicWaker;
use crossbeam_queue::ArrayQueue;
use windows::Win32::{
    Foundation::HANDLE,
    System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED, OVERLAPPED_ENTRY},
};

use crate::error::{Error, Result};

/// Win32 `ERROR_IO_PENDING`: the operation was queued and will complete later.
pub(crate) const ERROR_IO_PENDING: u32 = 997;

/// `OVERLAPPED` plus the primitives used to deliver its completion to a Rust
/// future. `#[repr(C)]` with `OVERLAPPED` first so that an
/// `*mut OverlappedWrap` is castable to and from `*mut OVERLAPPED`.
#[repr(C)]
pub(crate) struct OverlappedWrap {
    /// Must stay the first field — see the type-level note above.
    inner: UnsafeCell<OVERLAPPED>,
    /// `pBytesReturned` out-parameter. It lives here, behind the `Arc` the
    /// kernel holds, rather than on the submitting task's stack, so a late
    /// kernel write can never land on a frame that has already been reused.
    bytes: UnsafeCell<u32>,
    /// Set to `true` (Release) by the completion path once `err` and `len`
    /// have been written; loaded (Acquire) by the future.
    completed: AtomicBool,
    waker: AtomicWaker,
    err: AtomicU32,
    len: AtomicU32,
}

// SAFETY: `overlapped` and `bytes` are only written by the kernel while it
// holds the strong reference handed to it at submission time, and are only
// read by the future after the Acquire load of `completed` (or, on the
// synchronous path, before any submission is outstanding). That pairs with the
// Release store in `complete`, so there is no data race on either `UnsafeCell`.
unsafe impl Send for OverlappedWrap {}
// SAFETY: as above.
unsafe impl Sync for OverlappedWrap {}

impl OverlappedWrap {
    fn new() -> Arc<Self> {
        Arc::new(OverlappedWrap {
            inner: UnsafeCell::new(OVERLAPPED::default()),
            bytes: UnsafeCell::new(0),
            completed: AtomicBool::new(false),
            waker: AtomicWaker::new(),
            err: AtomicU32::new(0),
            len: AtomicU32::new(0),
        })
    }

    /// Record a completion and wake the awaiting future. Runs exactly once
    /// per accepted submission.
    fn complete(&self, win32_err: u32, bytes: u32) {
        self.err.store(win32_err, Ordering::Relaxed);
        self.len.store(bytes, Ordering::Relaxed);
        self.completed.store(true, Ordering::Release);
        self.waker.wake();
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }

    fn result(&self) -> Result<u32> {
        // Ordering::Relaxed is enough: the Acquire load of `completed` by the
        // caller already synchronised with the Release store in `complete`.
        match self.err.load(Ordering::Relaxed) {
            0 => Ok(self.len.load(Ordering::Relaxed)),
            e => Err(Error::Win32(e)),
        }
    }

    fn poll_complete(&self, cx: &Context<'_>) -> Poll<Result<u32>> {
        if self.is_completed() {
            return Poll::Ready(self.result());
        }
        self.waker.register(cx.waker());
        // Re-check: the completion may have landed between the first load and
        // the registration, in which case nobody will wake us again.
        if self.is_completed() {
            return Poll::Ready(self.result());
        }
        Poll::Pending
    }

    /// Spin until the completion path has run. Only reached on cancellation,
    /// where the wait is bounded by IOCP dispatch latency.
    fn block_until_complete(&self) {
        while !self.is_completed() {
            std::hint::spin_loop();
            std::thread::yield_now();
        }
    }

    fn overlapped(&self) -> *mut OVERLAPPED {
        self.inner.get()
    }

    /// Put a finished wrap back into its starting state so it can be reused.
    ///
    /// Takes `&mut self`, which is only obtainable through `Arc::get_mut`, so
    /// this cannot run while the kernel still holds a reference.
    fn reset(&mut self) {
        *self.inner.get_mut() = OVERLAPPED::default();
        *self.bytes.get_mut() = 0;
        self.completed = AtomicBool::new(false);
        self.waker = AtomicWaker::new();
        self.err = AtomicU32::new(0);
        self.len = AtomicU32::new(0);
    }
}

/// Free list of finished [`OverlappedWrap`]s.
///
/// Every async operation needs one, so without recycling the server would do a
/// heap allocation per receive, per body chunk and per response.
pub(crate) struct OpPool {
    free: ArrayQueue<Arc<OverlappedWrap>>,
}

impl OpPool {
    pub(crate) fn new(slots: usize) -> Self {
        Self {
            free: ArrayQueue::new(slots.max(1)),
        }
    }

    fn take(&self) -> Arc<OverlappedWrap> {
        self.free.pop().unwrap_or_else(OverlappedWrap::new)
    }

    fn give_back(&self, mut wrap: Arc<OverlappedWrap>) {
        // A second reference means a completion thread has not finished with
        // it yet; let that one go rather than risk reusing it underneath.
        let Some(unique) = Arc::get_mut(&mut wrap) else {
            return;
        };
        unique.reset();
        let _ = self.free.push(wrap);
    }
}

/// Deliver one `GetQueuedCompletionStatusEx` entry to its awaiting future.
///
/// # Safety
///
/// `entry` must be a completion packet produced by an operation whose
/// `OVERLAPPED` came from [`Op::new`], and whose completion key is the file
/// handle the operation was issued against. The kernel posts at most one
/// packet per accepted submission, so the strong reference reclaimed here is
/// reclaimed exactly once.
pub(crate) unsafe fn deliver_completion(entry: &OVERLAPPED_ENTRY) {
    // SAFETY: precondition — the pointer is the one leaked by `Op::new`, and
    // `OverlappedWrap` is `#[repr(C)]` with `OVERLAPPED` first.
    let wrap = unsafe { Arc::from_raw(entry.lpOverlapped.cast_const().cast::<OverlappedWrap>()) };
    let file = HANDLE(entry.lpCompletionKey as *mut c_void);
    // `GetOverlappedResult` is only here for the status; the byte count comes
    // from the completion packet itself.
    let mut ignored = 0u32;
    // SAFETY: the operation is finished (we just dequeued its packet), so this
    // reads the already-recorded status without blocking.
    let status = unsafe { GetOverlappedResult(file, entry.lpOverlapped, &mut ignored, false) };
    let err = match status {
        Ok(()) => 0,
        Err(e) => Error::from(e).win32_code().unwrap_or(0),
    };
    wrap.complete(err, entry.dwNumberOfBytesTransferred);
}

/// One in-flight overlapped operation.
///
/// Construct it, hand [`Op::overlapped`] and [`Op::bytes_ptr`] to the `Http*`
/// call, then pass the returned status code to [`Op::finish`]. `finish` must
/// be called, otherwise the reference transferred to the kernel is leaked.
#[must_use = "the reference handed to the kernel is only reclaimed by `finish`"]
pub(crate) struct Op<'a> {
    handle: HANDLE,
    wrap: Arc<OverlappedWrap>,
    /// The strong reference transferred to the kernel.
    leaked: *const OverlappedWrap,
    pool: &'a OpPool,
    skip_on_success: bool,
}

// SAFETY: `leaked` merely names the `Send + Sync` `OverlappedWrap` that
// `wrap` also points at; the handle is a kernel object.
unsafe impl Send for Op<'_> {}

impl<'a> Op<'a> {
    /// Take completion state from `pool` and transfer one strong reference to
    /// the kernel. `skip_on_success` must reflect whether the handle was
    /// accepted for `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`.
    pub(crate) fn new(handle: HANDLE, pool: &'a OpPool, skip_on_success: bool) -> Self {
        let wrap = pool.take();
        let leaked = Arc::into_raw(Arc::clone(&wrap));
        Op {
            handle,
            wrap,
            leaked,
            pool,
            skip_on_success,
        }
    }

    pub(crate) fn overlapped(&self) -> *mut OVERLAPPED {
        self.wrap.overlapped()
    }

    pub(crate) fn bytes_ptr(&self) -> *mut u32 {
        self.wrap.bytes.get()
    }

    /// The byte count the kernel reported through [`Op::bytes_ptr`].
    ///
    /// Only meaningful once the `Http*` call has returned something other than
    /// `ERROR_IO_PENDING`, because until then the kernel may still write it.
    pub(crate) fn reported_bytes(&self) -> u32 {
        // SAFETY: no completion is outstanding, so this is the only reader.
        unsafe { *self.wrap.bytes.get() }
    }

    /// Resolve the operation from the status code the `Http*` call returned.
    pub(crate) async fn finish(self, ec: u32) -> Result<u32> {
        match ec {
            // Completed inline and the I/O manager suppressed the completion
            // packet, so the byte count is already in place and nobody else
            // will ever touch this `OverlappedWrap`.
            0 if self.skip_on_success => {
                let bytes = self.reported_bytes();
                self.reclaim();
                self.pool.give_back(self.wrap);
                Ok(bytes)
            }
            0 | ERROR_IO_PENDING => {
                let result = AsyncOverlappedFuture {
                    handle: self.handle,
                    wrap: Arc::clone(&self.wrap),
                }
                .await;
                self.pool.give_back(self.wrap);
                result
            }
            // The kernel refused the operation, so no packet will arrive.
            other => {
                self.reclaim();
                self.pool.give_back(self.wrap);
                Err(Error::Win32(other))
            }
        }
    }

    /// Take back the reference handed to the kernel. Only valid when no
    /// completion packet can arrive for this operation.
    fn reclaim(&self) {
        // SAFETY: `leaked` came from `Arc::into_raw` in `new`, and `finish`
        // consumes `self`, so this runs at most once per `Op`.
        unsafe { drop(Arc::from_raw(self.leaked)) };
    }
}

/// Awaits a queued overlapped operation. Dropping it before completion
/// cancels the operation and waits for the kernel to relinquish the buffer.
struct AsyncOverlappedFuture {
    handle: HANDLE,
    wrap: Arc<OverlappedWrap>,
}

// SAFETY: `handle` is a kernel object and `OverlappedWrap` is `Send + Sync`,
// so the future can be polled from whichever worker thread picks it up.
unsafe impl Send for AsyncOverlappedFuture {}

impl Future for AsyncOverlappedFuture {
    type Output = Result<u32>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.wrap.poll_complete(cx)
    }
}

impl Drop for AsyncOverlappedFuture {
    fn drop(&mut self) {
        if self.wrap.is_completed() {
            return;
        }
        // SAFETY: this is the exact `OVERLAPPED` submitted against `handle`,
        // and we still hold a strong reference to it.
        unsafe {
            let _ = CancelIoEx(self.handle, Some(self.wrap.overlapped()));
        }
        self.wrap.block_until_complete();
    }
}
