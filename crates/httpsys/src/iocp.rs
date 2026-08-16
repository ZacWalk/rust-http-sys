//! A dedicated I/O completion port for http.sys request queues.
//!
//! The obvious way to drive overlapped http.sys calls is
//! `BindIoCompletionCallback`, which hands every completion to the legacy
//! Win32 thread pool. That costs a thread-pool dispatch per completion and
//! gives no control over how many threads service the queue.
//!
//! This module instead owns a port and a fixed set of completion threads that
//! drain it with `GetQueuedCompletionStatusEx`, so a burst of ready requests
//! is dequeued in one syscall instead of one syscall each. Associated handles
//! also get `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`, which removes the port
//! round trip entirely for calls the kernel can satisfy inline — the common
//! case once requests are already queued.

use std::thread::JoinHandle;

use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::SetFileCompletionNotificationModes,
    System::{
        IO::{
            CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY,
            PostQueuedCompletionStatus,
        },
        Threading::INFINITE,
    },
};

use crate::{error::Result, overlapped::deliver_completion};

/// Completion packets dequeued per `GetQueuedCompletionStatusEx` call.
const BATCH: usize = 64;

/// Completion key of the sentinel packets posted by [`IoPort::drop`]. A real
/// key is a handle value, and `-1` is `INVALID_HANDLE_VALUE`, so the two can
/// never collide.
const SHUTDOWN_KEY: usize = usize::MAX;

/// `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`: inline completions post no packet.
const FILE_SKIP_COMPLETION_PORT_ON_SUCCESS: u8 = 0x1;
/// `FILE_SKIP_SET_EVENT_ON_HANDLE`: we never wait on the handle itself.
const FILE_SKIP_SET_EVENT_ON_HANDLE: u8 = 0x2;

/// An owned completion port plus the threads draining it.
pub(crate) struct IoPort {
    port: HANDLE,
    threads: Vec<JoinHandle<()>>,
}

// SAFETY: a completion port handle is a kernel object; every operation on it
// used here is thread-safe by contract.
unsafe impl Send for IoPort {}
// SAFETY: as above.
unsafe impl Sync for IoPort {}

impl IoPort {
    /// Create the port and spawn `threads` completion threads.
    pub(crate) fn new(threads: usize) -> Result<Self> {
        let threads = threads.max(1);
        // SAFETY: `INVALID_HANDLE_VALUE` with no existing port is the
        // documented way to create a standalone completion port.
        let port =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, threads as u32) }?;

        let mut port_guard = IoPort {
            port,
            threads: Vec::with_capacity(threads),
        };
        for i in 0..threads {
            let port = SendHandle(port);
            let handle = std::thread::Builder::new()
                .name(format!("httpsys-iocp-{i}"))
                .spawn(move || completion_loop(port))
                // Threads already spawned are joined by `port_guard`'s drop.
                .map_err(|e| {
                    // ERROR_NOT_ENOUGH_MEMORY if the OS did not give a code.
                    crate::Error::Win32(e.raw_os_error().unwrap_or(8) as u32)
                })?;
            port_guard.threads.push(handle);
        }
        Ok(port_guard)
    }

    /// Associate a file handle with this port.
    ///
    /// Returns `true` if the kernel accepted
    /// `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` for the handle, in which case
    /// callers must resolve synchronous successes themselves because no
    /// completion packet will be posted for them.
    pub(crate) fn associate(&self, file: HANDLE) -> Result<bool> {
        // The completion key carries the handle so completion threads can call
        // `GetOverlappedResult` without any side table.
        //
        // SAFETY: `file` is a valid overlapped-capable handle owned by the
        // caller and outlives this port (the queue owns the port).
        unsafe { CreateIoCompletionPort(file, Some(self.port), file.0 as usize, 0) }?;
        // SAFETY: same handle; the call only sets a notification mode and
        // reports failure through its return value on drivers that refuse it.
        let skip = unsafe {
            SetFileCompletionNotificationModes(
                file,
                FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE,
            )
        }
        .is_ok();
        Ok(skip)
    }
}

impl Drop for IoPort {
    fn drop(&mut self) {
        for _ in 0..self.threads.len() {
            // SAFETY: the port is still open and only this type posts packets
            // with `SHUTDOWN_KEY`.
            let _ = unsafe { PostQueuedCompletionStatus(self.port, 0, SHUTDOWN_KEY, None) };
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        // SAFETY: every thread that could touch the port has been joined.
        let _ = unsafe { CloseHandle(self.port) };
    }
}

/// A `HANDLE` that can cross a thread boundary. The pointee is a kernel
/// object, so copying the value is meaningless to Rust's aliasing rules.
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);

// SAFETY: see the type-level comment.
unsafe impl Send for SendHandle {}

fn completion_loop(port: SendHandle) {
    let port = port.0;
    let mut entries = [OVERLAPPED_ENTRY::default(); BATCH];
    loop {
        let mut removed = 0u32;
        // SAFETY: the port outlives every completion thread (`IoPort::drop`
        // joins them before closing it) and `entries` is a live slice.
        let dequeued = unsafe {
            GetQueuedCompletionStatusEx(port, &mut entries, &mut removed, INFINITE, false)
        };
        if dequeued.is_err() {
            return; // Port closed underneath us.
        }

        let mut stop = false;
        for entry in &entries[..removed as usize] {
            if entry.lpCompletionKey == SHUTDOWN_KEY {
                stop = true;
                continue;
            }
            // SAFETY: every other packet on this port comes from an operation
            // submitted through `Op`, keyed by the handle it was issued on.
            unsafe { deliver_completion(entry) };
        }
        // Finish the batch before leaving, so no completion is dropped on the
        // floor while a future is still waiting for it.
        if stop {
            return;
        }
    }
}
