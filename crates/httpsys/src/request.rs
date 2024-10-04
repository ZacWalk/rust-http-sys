//! High-level [`Request`] type built on top of `HTTP_REQUEST_V2`.
//!
//! The raw kernel struct is *not* exposed in the public API. Callers receive
//! a typed [`Request`] with cooked accessors and an async [`body`] reader.

use std::{borrow::Cow, str::FromStr, sync::Arc};

use windows::Win32::Networking::HttpServer::{
    HttpHeaderConnection, HttpHeaderContentLength, HttpHeaderContentType, HttpHeaderHost,
    HttpHeaderUserAgent, HTTP_REQUEST_V2,
};

use crate::error::Result;

/// HTTP method as parsed by http.sys. Falls back to [`Method::Other`] for
/// extension methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
    Head,
    Options,
    Patch,
    Trace,
    Connect,
    Other(String),
}

impl FromStr for Method {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "HEAD" => Method::Head,
            "OPTIONS" => Method::Options,
            "PATCH" => Method::Patch,
            "TRACE" => Method::Trace,
            "CONNECT" => Method::Connect,
            other => Method::Other(other.to_string()),
        })
    }
}

/// A parsed HTTP request handed to a [`Handler`](crate::Handler).
///
/// The lifetime of the underlying buffer is owned by this `Request`; reading
/// the body consumes the request because the kernel's request id is single
/// use after `HttpReceiveRequestEntityBody` completes.
pub struct Request {
    pub(crate) raw: Box<RawRequest>,
    pub(crate) queue: Arc<crate::queue::RequestQueue>,
}

#[repr(C)]
pub(crate) struct RawRequest {
    pub(crate) raw: HTTP_REQUEST_V2,
    pub(crate) buff: [u8; super::queue::REQUEST_BUFFER_BYTES],
}

impl Default for RawRequest {
    fn default() -> Self {
        Self {
            raw: HTTP_REQUEST_V2::default(),
            buff: [0; super::queue::REQUEST_BUFFER_BYTES],
        }
    }
}

// SAFETY: the kernel-populated buffer is touched on a single thread at a
// time (the receive loop until handed off, then the handler task).
unsafe impl Send for RawRequest {}
unsafe impl Sync for RawRequest {}

impl Request {
    /// Application-defined context value associated with the URL prefix that
    /// matched this request (the value passed to [`UrlGroup::add_url`]).
    pub fn url_context(&self) -> u64 {
        self.raw.raw.Base.UrlContext
    }

    /// Kernel request identifier.
    pub fn request_id(&self) -> u64 {
        self.raw.raw.Base.RequestId
    }

    /// HTTP method.
    pub fn method(&self) -> Method {
        let len = self.raw.raw.Base.UnknownVerbLength as usize;
        if len > 0 {
            // Extension method (rare).
            let ptr = self.raw.raw.Base.pUnknownVerb.0;
            let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
            String::from_utf8_lossy(bytes).parse().unwrap()
        } else {
            // Verb id maps to a known method.
            use windows::Win32::Networking::HttpServer as W;
            let v = self.raw.raw.Base.Verb;
            if v == W::HttpVerbGET {
                Method::Get
            } else if v == W::HttpVerbPOST {
                Method::Post
            } else if v == W::HttpVerbPUT {
                Method::Put
            } else if v == W::HttpVerbDELETE {
                Method::Delete
            } else if v == W::HttpVerbHEAD {
                Method::Head
            } else if v == W::HttpVerbOPTIONS {
                Method::Options
            } else if v == W::HttpVerbTRACE {
                Method::Trace
            } else if v == W::HttpVerbCONNECT {
                Method::Connect
            } else {
                Method::Other(format!("{v:?}"))
            }
        }
    }

    /// Raw request URL as sent on the wire (path + query).
    pub fn url(&self) -> Cow<'_, str> {
        let len = self.raw.raw.Base.RawUrlLength as usize;
        let ptr = self.raw.raw.Base.pRawUrl.0;
        if len == 0 || ptr.is_null() {
            return Cow::Borrowed("");
        }
        // SAFETY: kernel guarantees `pRawUrl` points to `RawUrlLength` valid
        // bytes inside `self.raw.buff` after a successful receive.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        String::from_utf8_lossy(bytes)
    }

    /// `true` if the request indicates a body is present (Content-Length > 0
    /// or chunked Transfer-Encoding indicated by `MoreEntityBodyExists`).
    pub fn has_body(&self) -> bool {
        // Bit field flag 0x01 = MoreEntityBodyExists.
        (self.raw.raw.Base.Flags & 0x0000_0001) != 0
    }

    /// Read a known header by id. Returns `None` if absent.
    fn known_header(&self, id: usize) -> Option<&str> {
        let h = &self.raw.raw.Base.Headers.KnownHeaders[id];
        if h.RawValueLength == 0 || h.pRawValue.0.is_null() {
            return None;
        }
        let bytes = unsafe {
            std::slice::from_raw_parts(h.pRawValue.0 as *const u8, h.RawValueLength as usize)
        };
        std::str::from_utf8(bytes).ok()
    }

    pub fn host(&self) -> Option<&str> {
        self.known_header(HttpHeaderHost.0 as usize)
    }
    pub fn user_agent(&self) -> Option<&str> {
        self.known_header(HttpHeaderUserAgent.0 as usize)
    }
    pub fn content_type(&self) -> Option<&str> {
        self.known_header(HttpHeaderContentType.0 as usize)
    }
    pub fn content_length(&self) -> Option<u64> {
        self.known_header(HttpHeaderContentLength.0 as usize)
            .and_then(|s| s.parse().ok())
    }
    pub fn connection(&self) -> Option<&str> {
        self.known_header(HttpHeaderConnection.0 as usize)
    }

    /// Read the entire request body into memory.
    ///
    /// Returns `Ok(Vec::new())` if the request has no body. For very large
    /// bodies prefer [`Request::stream_body`].
    pub async fn body(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let cap = self.content_length().unwrap_or(0).min(64 * 1024 * 1024) as usize;
        buf.reserve(cap);
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match self
                .queue
                .read_entity_body(self.request_id(), &mut chunk)
                .await?
            {
                Some(n) => buf.extend_from_slice(&chunk[..n]),
                None => return Ok(buf),
            }
        }
    }

    /// Iterate over body chunks. Each call reads up to `chunk_size` bytes
    /// directly from the kernel and returns `None` on EOF.
    pub async fn read_body_chunk(&self, dst: &mut [u8]) -> Result<Option<usize>> {
        self.queue.read_entity_body(self.request_id(), dst).await
    }
}
