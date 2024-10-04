//! Builder-style [`Response`] type. The raw `HTTP_RESPONSE_V2` is hidden.

use std::ffi::c_void;

use bytes::Bytes;
use windows::{
    core::PCSTR,
    Win32::Networking::HttpServer::{
        HttpDataChunkFromMemory, HttpHeaderContentType, HTTP_DATA_CHUNK, HTTP_RESPONSE_V2,
    },
};

/// HTTP status codes used commonly enough to deserve constants.
pub mod status {
    pub const OK: u16 = 200;
    pub const NO_CONTENT: u16 = 204;
    pub const BAD_REQUEST: u16 = 400;
    pub const NOT_FOUND: u16 = 404;
    pub const METHOD_NOT_ALLOWED: u16 = 405;
    pub const INTERNAL_SERVER_ERROR: u16 = 500;
    pub const SERVICE_UNAVAILABLE: u16 = 503;
}

/// Builder for an HTTP response.
///
/// The body is owned by the response (`bytes::Bytes`); the response holds a
/// stable raw pointer to it, so the response must not be moved between the
/// pointer being set and the kernel sending it. The crate enforces this by
/// only ever passing `&Response` to the kernel.
pub struct Response {
    pub(crate) raw: HTTP_RESPONSE_V2,
    pub(crate) chunk: Box<HTTP_DATA_CHUNK>,
    pub(crate) body: Bytes,
    pub(crate) reason: &'static str,
    pub(crate) content_type: Option<&'static str>,
}

// SAFETY: `Response` owns its body via `Bytes` (refcounted heap buffer).
// The `pBuffer` raw pointer references that heap buffer; `Bytes` keeps the
// allocation pinned in place across moves of the `Response` value.
unsafe impl Send for Response {}
unsafe impl Sync for Response {}

impl Default for Response {
    fn default() -> Self {
        Self::new(status::OK)
    }
}

impl Response {
    /// New response with the given status code and an empty body.
    pub fn new(status: u16) -> Self {
        let mut r = Self {
            raw: HTTP_RESPONSE_V2::default(),
            chunk: Box::new(HTTP_DATA_CHUNK::default()),
            body: Bytes::new(),
            reason: reason_phrase(status),
            content_type: None,
        };
        r.raw.Base.StatusCode = status;
        r.set_reason(r.reason);
        r
    }

    /// Convenience: 200 OK with `application/json` body.
    pub fn ok_json(body: impl Into<Bytes>) -> Self {
        Self::new(status::OK)
            .with_content_type("application/json")
            .with_body(body)
    }

    /// Convenience: 200 OK with `text/plain; charset=utf-8` body.
    pub fn ok_text(body: impl Into<Bytes>) -> Self {
        Self::new(status::OK)
            .with_content_type("text/plain; charset=utf-8")
            .with_body(body)
    }

    /// Set the body, taking ownership without copying when the input is
    /// already `Bytes` / `&'static [u8]`.
    pub fn with_body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self.chunk.DataChunkType = HttpDataChunkFromMemory;
        self.chunk.Anonymous.FromMemory.BufferLength = self.body.len() as u32;
        self.chunk.Anonymous.FromMemory.pBuffer = self.body.as_ptr() as *mut c_void;
        self.raw.Base.EntityChunkCount = 1;
        self.raw.Base.pEntityChunks = &mut *self.chunk;
        self
    }

    /// Set the `Content-Type` header. Must be a `'static` string (the kernel
    /// reads from this address during send).
    pub fn with_content_type(mut self, content_type: &'static str) -> Self {
        self.content_type = Some(content_type);
        let idx = HttpHeaderContentType.0 as usize;
        self.raw.Base.Headers.KnownHeaders[idx].RawValueLength = content_type.len() as u16;
        self.raw.Base.Headers.KnownHeaders[idx].pRawValue = PCSTR(content_type.as_ptr());
        self
    }

    fn set_reason(&mut self, reason: &'static str) {
        self.raw.Base.pReason = PCSTR(reason.as_ptr());
        self.raw.Base.ReasonLength = reason.len() as u16;
    }

    pub(crate) fn raw_ptr(&self) -> *const HTTP_RESPONSE_V2 {
        &self.raw
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
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
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "OK",
    }
}
