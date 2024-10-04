//! `OVERLAPPED` plumbing shared by every async http.sys operation.
//!
//! The contract: every async operation allocates an [`OverlappedWrap`],
//! transfers one strong reference to the kernel via [`Arc::into_raw`] (in the
//! `OVERLAPPED` pointer), then awaits an [`AsyncOverlappedFuture`]. The IOCP
//! callback reclaims the strong reference via [`Arc::from_raw`] and signals
//! completion via an [`AtomicWaker`]. Dropping the future before completion
//! cancels the operation with `CancelIoEx` and blocks until the callback
//! has fired so the underlying buffer is no longer aliased by the kernel.

use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};

use std::{
    cell::UnsafeCell,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use atomic_waker::AtomicWaker;
use windows::{
    core::{Error as WinError, HRESULT},
    Win32::{
        Foundation::{HANDLE, WIN32_ERROR},
        System::IO::{BindIoCompletionCallback, CancelIoEx, OVERLAPPED},
    },
};

/// Bind an `http.sys` request-queue handle to the process IOCP and route
/// completions to [`private_callback`].
pub(crate) fn register_iocp_handle(h: HANDLE) -> Result<(), WinError> {
    // SAFETY: `h` is a kernel HANDLE supplied by the caller; passing a
    // static function pointer is always sound.
    let ok = unsafe { BindIoCompletionCallback(h, Some(private_callback), 0) };
    ok.ok()
}

/// IOCP completion callback.
///
/// SAFETY: Win32 guarantees `lpoverlapped` is the exact pointer that was
/// passed to the originating `Http*` call and that it remains alive until
/// this callback fires exactly once. We balance the leaked refcount with
/// `Arc::from_raw`. `OverlappedWrap` is `#[repr(C)]` with `OVERLAPPED`
/// first, so the pointer is layout-compatible.
unsafe extern "system" fn private_callback(
    dwerrorcode: u32,
    dwnumberofbytestransferred: u32,
    lpoverlapped: *mut OVERLAPPED,
) {
    let wrap_ptr = lpoverlapped as *const OverlappedWrap;
    let arc: Arc<OverlappedWrap> = unsafe { Arc::from_raw(wrap_ptr) };
    arc.complete(dwerrorcode, dwnumberofbytestransferred);
}

fn ec_to_hresult(ec: u32) -> HRESULT {
    if ec == 0 {
        HRESULT(0)
    } else {
        WinError::from(WIN32_ERROR(ec)).code()
    }
}

/// `OVERLAPPED` plus the primitives used to deliver its completion to a Rust
/// future. `#[repr(C)]` with `OVERLAPPED` as the first field so that an
/// `*mut OverlappedWrap` is castable to/from `*mut OVERLAPPED`.
#[repr(C)]
pub(crate) struct OverlappedWrap {
    o: UnsafeCell<OVERLAPPED>,
    /// Set to `true` (Release) by the callback once `err` and `len` have
    /// been written. Polled (Acquire) by the future.
    completed: AtomicBool,
    waker: AtomicWaker,
    /// Written by the callback before `completed` is set; read by the
    /// future once `completed == true`.
    err: UnsafeCell<HRESULT>,
    len: AtomicU32,
}

// SAFETY: All cross-thread access to `o` and `err` is gated by the
// Acquire/Release synchronization on `completed` plus `AtomicWaker`, which
// establishes the happens-before relationship required for `UnsafeCell`.
// The kernel writes `o` before posting the IOCP completion that triggers
// `complete()`, which sets `completed = true` after writing `err`.
unsafe impl Send for OverlappedWrap {}
unsafe impl Sync for OverlappedWrap {}

impl Default for OverlappedWrap {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlappedWrap {
    pub(crate) fn new() -> Self {
        OverlappedWrap {
            o: UnsafeCell::new(OVERLAPPED::default()),
            completed: AtomicBool::new(false),
            waker: AtomicWaker::new(),
            err: UnsafeCell::new(HRESULT(0)),
            len: AtomicU32::new(0),
        }
    }

    /// Pointer to the embedded `OVERLAPPED` for handing to Win32.
    pub(crate) fn overlapped(&self) -> *mut OVERLAPPED {
        self.o.get()
    }

    /// Transfer one strong reference to the kernel; returns a raw pointer
    /// suitable for the IOCP callback to reclaim with `Arc::from_raw`.
    pub(crate) fn leak_for_kernel(this: &Arc<Self>) -> *const OverlappedWrap {
        Arc::into_raw(Arc::clone(this))
    }

    /// Called by the IOCP callback exactly once. Records the result and
    /// wakes any awaiting future.
    pub(crate) fn complete(&self, win32_err: u32, bytes: u32) {
        // SAFETY: only the callback writes `err`, and it runs at most once
        // per `OverlappedWrap`. The Release on `completed` publishes the
        // write before any future-side reader can observe it.
        unsafe {
            *self.err.get() = ec_to_hresult(win32_err);
        }
        self.len.store(bytes, Ordering::Relaxed);
        self.completed.store(true, Ordering::Release);
        self.waker.wake();
    }

    pub(crate) fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }

    pub(crate) fn poll_complete(&self, cx: &Context<'_>) -> Poll<(HRESULT, u32)> {
        if self.is_completed() {
            return Poll::Ready(self.read_result());
        }
        self.waker.register(cx.waker());
        if self.is_completed() {
            return Poll::Ready(self.read_result());
        }
        Poll::Pending
    }

    fn read_result(&self) -> (HRESULT, u32) {
        // SAFETY: `completed` is Acquire-true here, synchronizing the
        // writes performed by `complete`.
        let hr = unsafe { *self.err.get() };
        let len = self.len.load(Ordering::Relaxed);
        (hr, len)
    }

    /// Spin-wait for completion; used only on cancellation paths. Bounded in
    /// practice by IOCP latency (microseconds).
    pub(crate) fn block_until_complete(&self) {
        while !self.is_completed() {
            std::thread::yield_now();
        }
    }
}

/// Future that awaits an IOCP-backed Win32 operation. Owns one strong ref
/// to the [`OverlappedWrap`]; the second is held by the kernel until the
/// completion callback fires. Cancellation-safe: dropping before completion
/// invokes `CancelIoEx` and waits for the callback.
pub(crate) struct AsyncOverlappedFuture {
    handle: HANDLE,
    optr: Arc<OverlappedWrap>,
}

impl AsyncOverlappedFuture {
    pub(crate) fn new(handle: HANDLE, optr: Arc<OverlappedWrap>) -> Self {
        Self { handle, optr }
    }
}

impl Future for AsyncOverlappedFuture {
    type Output = (HRESULT, u32);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.optr.poll_complete(cx)
    }
}

impl Drop for AsyncOverlappedFuture {
    fn drop(&mut self) {
        if !self.optr.is_completed() {
            // SAFETY: `self.optr.overlapped()` is the same pointer that was
            // submitted to Win32 and is still alive (we hold an Arc ref).
            unsafe {
                let _ = CancelIoEx(self.handle, Some(self.optr.overlapped()));
            }
            self.optr.block_until_complete();
        }
    }
}

/// Newtype around the leaked `*const OverlappedWrap` so the future type
/// returned by submit-and-await stays `Send` even with the raw pointer in
/// scope. The pointer itself is a kernel-borrowed reference to a
/// `Send + Sync` value.
#[derive(Copy, Clone)]
pub(crate) struct LeakedOverlapped(pub(crate) *const OverlappedWrap);

// SAFETY: the pointee `OverlappedWrap` is Send + Sync; the raw pointer
// merely identifies it for the kernel.
unsafe impl Send for LeakedOverlapped {}
unsafe impl Sync for LeakedOverlapped {}

impl LeakedOverlapped {
    /// Reclaim the leaked Arc strong-ref. Used when the kernel rejected
    /// the submission so the callback will never fire.
    ///
    /// SAFETY: must be called at most once per leak, and only when the
    /// kernel did not accept the submission.
    pub(crate) unsafe fn reclaim(self) {
        unsafe { drop(Arc::from_raw(self.0)) };
    }
}
