//! Builder-style [`Response`] type. The raw `HTTP_RESPONSE_V2` is hidden.

use std::{borrow::Cow, ffi::c_void};

use bytes::Bytes;
use windows::{
    Win32::Networking::HttpServer::{
        HTTP_DATA_CHUNK, HTTP_RESPONSE_V2, HTTP_UNKNOWN_HEADER, HttpDataChunkFromMemory,
        HttpHeaderContentType,
    },
    core::PCSTR,
};

/// HTTP status codes used often enough to deserve constants.
pub mod status {
    pub const OK: u16 = 200;
    pub const NO_CONTENT: u16 = 204;
    pub const BAD_REQUEST: u16 = 400;
    pub const NOT_FOUND: u16 = 404;
    pub const METHOD_NOT_ALLOWED: u16 = 405;
    pub const PAYLOAD_TOO_LARGE: u16 = 413;
    pub const HEADERS_TOO_LARGE: u16 = 431;
    pub const INTERNAL_SERVER_ERROR: u16 = 500;
    pub const SERVICE_UNAVAILABLE: u16 = 503;
}

/// An HTTP response.
///
/// This is plain data: nothing is pinned and nothing points at anything else.
/// The `HTTP_RESPONSE_V2` the kernel wants is assembled inside
/// [`RequestQueue::send_response`](crate::RequestQueue::send_response) and
/// lives only for the duration of that call.
#[derive(Debug, Clone)]
pub struct Response {
    status: u16,
    reason: &'static str,
    content_type: Option<&'static str>,
    headers: Vec<(&'static str, Cow<'static, str>)>,
    body: Bytes,
    cache_seconds: Option<u32>,
}

impl Default for Response {
    /// An empty `200 OK`.
    fn default() -> Self {
        Self::new(status::OK)
    }
}

impl Response {
    /// New response with the given status code and an empty body.
    pub fn new(status: u16) -> Self {
        Self {
            status,
            reason: reason_phrase(status),
            content_type: None,
            headers: Vec::new(),
            body: Bytes::new(),
            cache_seconds: None,
        }
    }

    /// 200 OK with an `application/json` body.
    pub fn ok_json(body: impl Into<Bytes>) -> Self {
        Self::new(status::OK)
            .with_content_type("application/json")
            .with_body(body)
    }

    /// 200 OK with a `text/plain; charset=utf-8` body.
    pub fn ok_text(body: impl Into<Bytes>) -> Self {
        Self::new(status::OK)
            .with_content_type("text/plain; charset=utf-8")
            .with_body(body)
    }

    /// Set the body. `&'static [u8]`, `&'static str` and `Bytes` are all
    /// taken without copying.
    #[must_use]
    pub fn with_body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    /// Set the `Content-Type` header.
    #[must_use]
    pub fn with_content_type(mut self, content_type: &'static str) -> Self {
        self.content_type = Some(content_type);
        self
    }

    /// Add an arbitrary response header.
    #[must_use]
    pub fn with_header(mut self, name: &'static str, value: impl Into<Cow<'static, str>>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// Let http.sys serve this response from its kernel-mode cache for
    /// `seconds`, answering identical subsequent requests without ever
    /// entering user mode.
    ///
    /// This is a large throughput win, but it is only correct for responses
    /// that are genuinely a pure function of the request URL — the kernel will
    /// not call your handler again until the entry expires. See
    /// [`HttpSendHttpResponse`] for the full list of conditions under which
    /// http.sys declines to cache.
    ///
    /// [`HttpSendHttpResponse`]: https://learn.microsoft.com/windows/win32/api/http/nf-http-httpsendhttpresponse
    #[must_use]
    pub fn cache_in_kernel_for_secs(mut self, seconds: u32) -> Self {
        self.cache_seconds = Some(seconds);
        self
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn body(&self) -> &Bytes {
        &self.body
    }

    pub(crate) fn cache_seconds(&self) -> Option<u32> {
        self.cache_seconds
    }

    /// Build the kernel view of this response.
    ///
    /// The result borrows from `self`, and links to its own interior, so it
    /// must not move between [`RawResponse::as_ptr`] and the end of the send.
    pub(crate) fn to_raw(&self) -> RawResponse<'_> {
        let mut raw = HTTP_RESPONSE_V2::default();
        raw.Base.StatusCode = self.status;
        raw.Base.pReason = PCSTR(self.reason.as_ptr());
        raw.Base.ReasonLength = self.reason.len() as u16;

        if let Some(content_type) = self.content_type {
            let slot = &mut raw.Base.Headers.KnownHeaders[HttpHeaderContentType.0 as usize];
            slot.pRawValue = PCSTR(content_type.as_ptr());
            slot.RawValueLength = content_type.len() as u16;
        }

        let unknown: Vec<HTTP_UNKNOWN_HEADER> = self
            .headers
            .iter()
            .map(|(name, value)| HTTP_UNKNOWN_HEADER {
                NameLength: name.len() as u16,
                pName: PCSTR(name.as_ptr()),
                RawValueLength: value.len() as u16,
                pRawValue: PCSTR(value.as_ptr()),
            })
            .collect();

        let mut chunk = HTTP_DATA_CHUNK {
            DataChunkType: HttpDataChunkFromMemory,
            ..Default::default()
        };
        chunk.Anonymous.FromMemory.pBuffer = self.body.as_ptr().cast_mut().cast::<c_void>();
        chunk.Anonymous.FromMemory.BufferLength = self.body.len() as u32;

        RawResponse {
            raw,
            chunk,
            unknown,
            _borrow: std::marker::PhantomData,
        }
    }
}

/// The `HTTP_RESPONSE_V2` handed to http.sys, plus the arrays it points at.
///
/// Both the chunk array and the unknown-header array live inline, so this type
/// becomes self-referential the moment [`RawResponse::as_ptr`] is called.
pub(crate) struct RawResponse<'a> {
    raw: HTTP_RESPONSE_V2,
    chunk: HTTP_DATA_CHUNK,
    unknown: Vec<HTTP_UNKNOWN_HEADER>,
    /// Ties the lifetime to the [`Response`] whose body and header strings the
    /// raw pointers above refer to.
    _borrow: std::marker::PhantomData<&'a Response>,
}

// SAFETY: the raw pointers inside only ever address this value's own fields or
// the borrowed `Response`, both of which are `Send`. Nothing here is shared,
// so moving the whole bundle to another thread aliases nothing.
unsafe impl Send for RawResponse<'_> {}

impl RawResponse<'_> {
    /// Link the interior arrays and return the pointer for `HttpSendHttpResponse`.
    ///
    /// # Safety
    ///
    /// The returned pointer stays valid only while `self` remains at its
    /// current address and the borrowed [`Response`] is alive. Callers must
    /// keep both in place until the send has completed or been cancelled.
    pub(crate) unsafe fn as_ptr(&mut self) -> *const HTTP_RESPONSE_V2 {
        // SAFETY: `FromMemory` is the variant `to_raw` wrote, and the whole
        // union was zero-initialized before that.
        let body_len = unsafe { self.chunk.Anonymous.FromMemory.BufferLength };
        if body_len > 0 {
            self.raw.Base.EntityChunkCount = 1;
            self.raw.Base.pEntityChunks = &raw mut self.chunk;
        }
        if !self.unknown.is_empty() {
            self.raw.Base.Headers.UnknownHeaderCount = self.unknown.len() as u16;
            self.raw.Base.Headers.pUnknownHeaders = self.unknown.as_mut_ptr();
        }
        &raw const self.raw
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_set_the_expected_fields() {
        let r = Response::ok_text("hello").with_header("X-Trace", "abc");
        assert_eq!(r.status(), 200);
        assert_eq!(&r.body()[..], b"hello");
        assert_eq!(r.content_type, Some("text/plain; charset=utf-8"));
        assert_eq!(r.headers, vec![("X-Trace", Cow::Borrowed("abc"))]);
    }

    #[test]
    fn raw_view_points_at_the_body_and_headers() {
        let response = Response::ok_json("{}").with_header("X-A", "1");
        let mut raw = response.to_raw();
        // SAFETY: `raw` is a live local and `response` outlives it.
        let ptr = unsafe { raw.as_ptr() };
        // SAFETY: `ptr` came from `raw`, which is still borrowed here.
        let view = unsafe { &*ptr };
        assert_eq!(view.Base.StatusCode, 200);
        assert_eq!(view.Base.EntityChunkCount, 1);
        assert_eq!(view.Base.Headers.UnknownHeaderCount, 1);
        // SAFETY: linked above from `raw.chunk`.
        let chunk = unsafe { &(*view.Base.pEntityChunks).Anonymous.FromMemory };
        assert_eq!(chunk.BufferLength, 2);
        assert_eq!(chunk.pBuffer, response.body().as_ptr().cast_mut().cast());
    }

    #[test]
    fn empty_body_emits_no_entity_chunk() {
        let response = Response::new(status::NO_CONTENT);
        let mut raw = response.to_raw();
        // SAFETY: as above.
        let view = unsafe { &*raw.as_ptr() };
        assert_eq!(view.Base.EntityChunkCount, 0);
        assert!(view.Base.pEntityChunks.is_null());
    }

    #[test]
    fn unknown_status_still_gets_a_reason_phrase() {
        assert_eq!(Response::new(599).reason, "Unknown");
    }
}
