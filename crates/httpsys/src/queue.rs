//! Async wrappers around `HttpReceiveHttpRequest`,
//! `HttpReceiveRequestEntityBody`, `HttpSendHttpResponse`, and
//! `HttpShutdownRequestQueue`.

use std::sync::Arc;

use windows::{
    core::Error as WinError,
    Win32::{
        Foundation::{HANDLE, NO_ERROR, WIN32_ERROR},
        Networking::HttpServer::{
            HttpCloseRequestQueue, HttpCreateRequestQueue, HttpReceiveHttpRequest,
            HttpReceiveRequestEntityBody, HttpSendHttpResponse, HttpShutdownRequestQueue,
            HTTP_RECEIVE_HTTP_REQUEST_FLAGS,
        },
    },
};

use crate::{
    error::{Error, Result},
    init::{UrlGroup, HTTPAPI_VERSION_2},
    overlapped::{register_iocp_handle, AsyncOverlappedFuture, LeakedOverlapped, OverlappedWrap},
    request::RawRequest,
    response::Response,
};

/// Inline buffer size embedded in every `Request`. 16 KiB covers all typical
/// header sizes; oversized requests surface as `Error::BufferTooSmall`.
pub(crate) const REQUEST_BUFFER_BYTES: usize = 16 * 1024;

/// Win32 status code "I/O operation pending"; treated as success by
/// IOCP-driven submissions.
const ERROR_IO_PENDING: u32 = 997;
/// Win32 status code "more data available". Returned by
/// `HttpReceiveRequestEntityBody` once the body has been fully consumed.
const ERROR_HANDLE_EOF: u32 = 38;

/// Wrapper around the kernel `HANDLE` for an http.sys request queue.
///
/// Cloneable across threads via `Arc<RequestQueue>`. Drop closes the queue.
pub struct RequestQueue {
    h: HANDLE,
}

// SAFETY: HANDLE is a kernel handle; access is mediated by the kernel.
unsafe impl Send for RequestQueue {}
unsafe impl Sync for RequestQueue {}

impl RequestQueue {
    pub fn new() -> Result<Self> {
        let mut h: HANDLE = HANDLE::default();
        let ec = unsafe { HttpCreateRequestQueue(HTTPAPI_VERSION_2, None, None, 0, &mut h) };
        Error::check(ec)?;
        debug_assert!(!h.is_invalid());
        register_iocp_handle(h).map_err(Error::from)?;
        Ok(Self { h })
    }

    /// Bind the given URL group to this queue.
    pub fn bind_url_group(&self, url_group: &UrlGroup) -> Result<()> {
        url_group.bind_to_queue(self.h)
    }

    #[allow(dead_code)]
    pub(crate) fn handle(&self) -> HANDLE {
        self.h
    }

    /// Receive the next request. Internal — drives [`Server`](crate::Server).
    pub(crate) async fn receive_request(&self, requestbuffer: &mut RawRequest) -> Result<u32> {
        let optr = Arc::new(OverlappedWrap::new());
        let leaked = LeakedOverlapped(OverlappedWrap::leak_for_kernel(&optr));
        let ec = unsafe {
            HttpReceiveHttpRequest(
                self.h,
                0,
                HTTP_RECEIVE_HTTP_REQUEST_FLAGS::default(),
                &mut requestbuffer.raw,
                std::mem::size_of::<RawRequest>() as u32,
                None,
                Some(leaked.0 as *const _),
            )
        };
        Self::resolve_submission(self.h, optr, leaked, ec).await
    }

    /// Read up to `dst.len()` bytes of the request body. Returns `Ok(None)`
    /// once the body has been fully consumed (`ERROR_HANDLE_EOF`).
    pub(crate) async fn read_entity_body(
        &self,
        request_id: u64,
        dst: &mut [u8],
    ) -> Result<Option<usize>> {
        let optr = Arc::new(OverlappedWrap::new());
        let leaked = LeakedOverlapped(OverlappedWrap::leak_for_kernel(&optr));
        let ec = unsafe {
            HttpReceiveRequestEntityBody(
                self.h,
                request_id,
                0,
                dst.as_mut_ptr() as *mut _,
                dst.len() as u32,
                None,
                Some(leaked.0 as *const _),
            )
        };
        let err = WIN32_ERROR(ec);
        // Synchronous EOF — kernel did not consume the leak.
        if err.0 == ERROR_HANDLE_EOF {
            unsafe { leaked.reclaim() };
            return Ok(None);
        }
        if err == NO_ERROR || err.0 == ERROR_IO_PENDING {
            let (hr, len) = AsyncOverlappedFuture::new(self.h, optr).await;
            if hr.is_ok() {
                Ok(Some(len as usize))
            } else if hr.0 as u32 & 0xFFFF == ERROR_HANDLE_EOF {
                // Async EOF.
                Ok(None)
            } else {
                Err(Error::Win32 {
                    code: hr.0,
                    message: Some(WinError::from(hr).message().to_string_lossy()),
                })
            }
        } else {
            unsafe { leaked.reclaim() };
            Err(Error::from_win32(ec))
        }
    }

    /// Send a response.
    pub async fn send_response(
        &self,
        request_id: u64,
        flags: u32,
        response: &Response,
    ) -> Result<u32> {
        let optr = Arc::new(OverlappedWrap::new());
        let leaked = LeakedOverlapped(OverlappedWrap::leak_for_kernel(&optr));
        let ec = unsafe {
            HttpSendHttpResponse(
                self.h,
                request_id,
                flags,
                response.raw_ptr(),
                None,
                None,
                None,
                0,
                Some(leaked.0 as *const _),
                None,
            )
        };
        Self::resolve_submission(self.h, optr, leaked, ec).await
    }

    /// Initiate graceful shutdown: stop accepting new connections, but allow
    /// in-flight responses to drain. Returns immediately; existing
    /// `receive_request` calls will surface `ERROR_OPERATION_ABORTED`.
    pub fn shutdown(&self) -> Result<()> {
        let ec = unsafe { HttpShutdownRequestQueue(self.h) };
        Error::check(ec)
    }

    async fn resolve_submission(
        h: HANDLE,
        optr: Arc<OverlappedWrap>,
        leaked: LeakedOverlapped,
        ec: u32,
    ) -> Result<u32> {
        let err = WIN32_ERROR(ec);
        if err == NO_ERROR || err.0 == ERROR_IO_PENDING {
            let (hr, len) = AsyncOverlappedFuture::new(h, optr).await;
            if hr.is_ok() {
                Ok(len)
            } else {
                Err(Error::Win32 {
                    code: hr.0,
                    message: Some(WinError::from(hr).message().to_string_lossy()),
                })
            }
        } else {
            // SAFETY: leak was just made; kernel rejected — reclaim now.
            unsafe { leaked.reclaim() };
            Err(Error::from_win32(ec))
        }
    }
}

impl Drop for RequestQueue {
    fn drop(&mut self) {
        if self.h.is_invalid() {
            return;
        }
        let ec = unsafe { HttpCloseRequestQueue(self.h) };
        if ec != 0 {
            eprintln!("HttpCloseRequestQueue failed: 0x{ec:08x}");
        }
        self.h = HANDLE(0);
    }
}
