//! Async wrappers around the four http.sys calls that carry the request
//! lifecycle: receive, read body, send response, shut down.

use windows::{
    Win32::{
        Foundation::HANDLE,
        Networking::HttpServer::{
            HTTP_CACHE_POLICY, HTTP_RECEIVE_HTTP_REQUEST_FLAGS,
            HTTP_RECEIVE_REQUEST_ENTITY_BODY_FLAG_FILL_BUFFER, HTTP_RECEIVE_REQUEST_FLAG_COPY_BODY,
            HttpCachePolicyTimeToLive, HttpCloseRequestQueue, HttpCreateRequestQueue,
            HttpReceiveHttpRequest, HttpReceiveRequestEntityBody, HttpSendHttpResponse,
            HttpServerQueueLengthProperty, HttpSetRequestQueueProperty, HttpShutdownRequestQueue,
        },
    },
    core::PCWSTR,
};

use crate::{
    buffer::PooledBuffer,
    error::{Error, Result},
    init::{HTTPAPI_VERSION_2, UrlGroup},
    iocp::IoPort,
    overlapped::{Op, OpPool},
    response::Response,
};

/// Win32 `ERROR_HANDLE_EOF`: the entity body has been fully consumed.
const ERROR_HANDLE_EOF: u32 = 38;
/// Win32 `ERROR_MORE_DATA`: the receive buffer was too small. The request
/// stays queued and can be re-received by id.
const ERROR_MORE_DATA: u32 = 234;

/// Whether the next receive should ask http.sys to copy the body in with the
/// headers.
///
/// Asking for it is a clear win when the body fits — one syscall instead of
/// two or more — and a clear loss when it does not, because the receive fails
/// with `ERROR_MORE_DATA` and has to be reissued. Each request slot therefore
/// carries the outcome of its last request forward, so a workload settles on
/// the right answer after at most one mistake.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReceiveMode {
    copy_body: bool,
}

impl Default for ReceiveMode {
    fn default() -> Self {
        Self { copy_body: true }
    }
}

/// An http.sys request queue bound to its own completion port.
pub struct RequestQueue {
    handle: HANDLE,
    /// Recycled completion state, so no operation allocates.
    ops: OpPool,
    /// Owns the completion threads. Dropped after [`RequestQueue::drop`] has
    /// closed the handle, so the threads are still there to reclaim the
    /// references held by operations the close cancels.
    _io: IoPort,
    /// Whether the kernel suppresses completion packets for calls it satisfies
    /// inline. See [`IoPort::associate`].
    skip_on_success: bool,
}

// SAFETY: every field is a kernel object or a plain flag, and all the `Http*`
// entry points used here are safe to call concurrently on one queue handle.
unsafe impl Send for RequestQueue {}
// SAFETY: as above.
unsafe impl Sync for RequestQueue {}

impl RequestQueue {
    /// Create an anonymous request queue served by `io_threads` completion
    /// threads, sized to keep `concurrency` operations in flight.
    pub fn new(io_threads: usize, concurrency: usize) -> Result<Self> {
        let mut handle = HANDLE::default();
        // SAFETY: a null name creates an anonymous queue; `handle` is a live
        // local for the duration of the call.
        let ec = unsafe {
            HttpCreateRequestQueue(HTTPAPI_VERSION_2, PCWSTR::null(), None, None, &mut handle)
        };
        Error::check(ec)?;
        debug_assert!(!handle.is_invalid());

        let io = IoPort::new(io_threads)?;
        let skip_on_success = match io.associate(handle) {
            Ok(skip) => skip,
            Err(e) => {
                // SAFETY: `handle` was just created and is not yet shared.
                let _ = unsafe { HttpCloseRequestQueue(handle) };
                return Err(e);
            }
        };

        Ok(Self {
            handle,
            // Two per slot: a receive and a send can be live at the same time
            // across different slots.
            ops: OpPool::new(concurrency * 2),
            _io: io,
            skip_on_success,
        })
    }

    /// Bind a URL group to this queue.
    pub fn bind_url_group(&self, url_group: &UrlGroup) -> Result<()> {
        url_group.bind_to_queue(self.handle)
    }

    /// Set how many requests http.sys will hold for this queue before it
    /// starts rejecting connections with 503. The default is 1000.
    pub fn set_queue_length(&self, requests: u32) -> Result<()> {
        // SAFETY: `HttpServerQueueLengthProperty` takes a single ULONG, and
        // `requests` is a live local for the duration of the call.
        let ec = unsafe {
            HttpSetRequestQueueProperty(
                self.handle,
                HttpServerQueueLengthProperty,
                std::ptr::from_ref(&requests).cast(),
                size_of::<u32>() as u32,
                None,
                None,
            )
        };
        Error::check(ec)
    }

    /// Receive the next request into `buffer`.
    ///
    /// When `mode` says so, http.sys is asked to copy the entity body in
    /// alongside the headers, which makes the whole request a single syscall
    /// for anything that fits. If it does not fit, the same request is
    /// re-received headers-only, the body is streamed on demand by
    /// [`RequestQueue::read_entity_body`], and `mode` is updated so the next
    /// request on this slot does not repeat the wasted call.
    pub(crate) async fn receive(
        &self,
        buffer: &mut PooledBuffer,
        mode: &mut ReceiveMode,
    ) -> Result<()> {
        let mut needed = 0u32;
        // Zero means "whatever request is next". Only a copy attempt that came
        // back ERROR_MORE_DATA leaves a specific request sitting in the queue
        // for us to ask for again.
        let mut request_id = 0u64;

        if mode.copy_body {
            match self
                .receive_once(buffer, 0, HTTP_RECEIVE_REQUEST_FLAG_COPY_BODY, &mut needed)
                .await
            {
                Err(Error::Win32(ERROR_MORE_DATA)) => {
                    mode.copy_body = false;
                    request_id = buffer.request_id();
                }
                other => return other,
            }
        }

        let result = self
            .receive_once(
                buffer,
                request_id,
                HTTP_RECEIVE_HTTP_REQUEST_FLAGS::default(),
                &mut needed,
            )
            .await;

        match result {
            // Not even the headers fit; the caller must reject the request.
            Err(Error::Win32(ERROR_MORE_DATA)) => Err(Error::HeadersTooLarge { needed }),
            Ok(()) => {
                // Resize this slot's buffer to whatever this connection is
                // actually sending, and go back to single-syscall receives if
                // that now fits.
                mode.copy_body = buffer.fit_body_inline();
                Ok(())
            }
            other => other,
        }
    }

    async fn receive_once(
        &self,
        buffer: &mut PooledBuffer,
        request_id: u64,
        flags: HTTP_RECEIVE_HTTP_REQUEST_FLAGS,
        needed: &mut u32,
    ) -> Result<()> {
        let op = Op::new(self.handle, &self.ops, self.skip_on_success);
        let capacity = buffer.capacity();
        let target = buffer.prepare();
        // SAFETY: `target` addresses `capacity` writable bytes owned by
        // `buffer`, which the caller keeps alive across the await below, and
        // `op` owns both the OVERLAPPED and the byte-count out-parameter.
        let ec = unsafe {
            HttpReceiveHttpRequest(
                self.handle,
                request_id,
                flags,
                target,
                capacity,
                Some(op.bytes_ptr()),
                Some(op.overlapped()),
            )
        };
        if ec == ERROR_MORE_DATA {
            *needed = op.reported_bytes();
        }
        op.finish(ec).await.map(|_| ())
    }

    /// Read the next slice of the request body into `dst`, or `None` at end
    /// of body.
    ///
    /// `FILL_BUFFER` makes http.sys keep going until `dst` is full or the body
    /// ends, rather than returning whatever happens to be buffered. For a body
    /// read into a buffer sized from `Content-Length` that turns an upload
    /// into a single call.
    pub(crate) async fn read_entity_body(
        &self,
        request_id: u64,
        dst: &mut [u8],
    ) -> Result<Option<usize>> {
        if dst.is_empty() {
            return Ok(Some(0));
        }
        let op = Op::new(self.handle, &self.ops, self.skip_on_success);
        // SAFETY: `dst` stays borrowed across the await, and `Op`'s drop guard
        // cancels and waits if this future is dropped early, so the kernel can
        // never write into it after it has been released.
        let ec = unsafe {
            HttpReceiveRequestEntityBody(
                self.handle,
                request_id,
                HTTP_RECEIVE_REQUEST_ENTITY_BODY_FLAG_FILL_BUFFER,
                dst.as_mut_ptr().cast(),
                dst.len() as u32,
                Some(op.bytes_ptr()),
                Some(op.overlapped()),
            )
        };
        match op.finish(ec).await {
            Ok(read) => Ok(Some(read as usize)),
            Err(Error::Win32(ERROR_HANDLE_EOF)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Send `response` as the complete reply to `request_id`.
    pub async fn send_response(&self, request_id: u64, response: &Response) -> Result<u32> {
        let mut raw = response.to_raw();
        let cache = response.cache_seconds().map(|seconds| HTTP_CACHE_POLICY {
            Policy: HttpCachePolicyTimeToLive,
            SecondsToLive: seconds,
        });

        let op = Op::new(self.handle, &self.ops, self.skip_on_success);
        // SAFETY: `raw`, `cache` and the borrowed `response` are all locals of
        // this async fn, so they live inside the future and cannot move once
        // it has been polled. If the future is dropped before the send
        // completes, `Op`'s guard cancels the operation and waits for the
        // kernel to let go of them.
        let ec = unsafe {
            let raw_ptr = raw.as_ptr();
            HttpSendHttpResponse(
                self.handle,
                request_id,
                0,
                raw_ptr,
                cache.as_ref().map(std::ptr::from_ref),
                Some(op.bytes_ptr()),
                None,
                None,
                Some(op.overlapped()),
                None,
            )
        };
        op.finish(ec).await
    }

    /// Stop accepting new connections and let in-flight work drain. Pending
    /// `receive` calls surface `ERROR_OPERATION_ABORTED`.
    pub fn shutdown(&self) -> Result<()> {
        // SAFETY: `handle` is valid until `drop`.
        let ec = unsafe { HttpShutdownRequestQueue(self.handle) };
        Error::check(ec)
    }
}

impl Drop for RequestQueue {
    fn drop(&mut self) {
        // Close the queue first: that completes or cancels everything still
        // outstanding, and the completion threads (still running, since `io`
        // is dropped after this body) reclaim the references those operations
        // are holding.
        // SAFETY: closed exactly once, and no operation can be submitted
        // afterwards because `self` is being destroyed.
        let ec = unsafe { HttpCloseRequestQueue(self.handle) };
        if ec != 0 {
            eprintln!(
                "httpsys: HttpCloseRequestQueue failed: {}",
                Error::Win32(ec)
            );
        }
    }
}
