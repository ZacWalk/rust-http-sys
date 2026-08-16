//! High-level [`Request`] type built on top of `HTTP_REQUEST_V2`.
//!
//! The raw kernel struct is not exposed. Callers get cooked accessors that
//! borrow directly out of the receive buffer — no copying, no allocation.

use std::{borrow::Cow, str::FromStr, sync::Arc};

use windows::Win32::Networking::HttpServer::{
    HTTP_REQUEST_FLAG_MORE_ENTITY_BODY_EXISTS, HttpHeaderConnection, HttpHeaderContentLength,
    HttpHeaderContentType, HttpHeaderHost, HttpHeaderUserAgent, HttpVerbCONNECT, HttpVerbDELETE,
    HttpVerbGET, HttpVerbHEAD, HttpVerbOPTIONS, HttpVerbPOST, HttpVerbPUT, HttpVerbTRACE,
};

use crate::{
    buffer::{BodyPool, PooledBuffer, PooledVec},
    error::Result,
    queue::RequestQueue,
};

/// Largest `Content-Length` honoured when pre-sizing the body buffer. Beyond
/// this the body grows on demand instead, so a bogus header cannot make the
/// server allocate arbitrarily.
const MAX_BODY_RESERVE: u64 = 8 * 1024 * 1024;

/// Bytes requested per `HttpReceiveRequestEntityBody` call when a body did not
/// arrive inline with the headers.
const BODY_CHUNK_BYTES: usize = 256 * 1024;

/// HTTP method as parsed by http.sys, falling back to [`Method::Other`] for
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
            other => Method::Other(other.to_owned()),
        })
    }
}

/// A parsed HTTP request handed to a [`Handler`](crate::Handler).
///
/// All accessors borrow out of the kernel-filled receive buffer this value
/// owns, so reading headers or an inline body costs nothing.
pub struct Request {
    pub(crate) buffer: PooledBuffer,
    pub(crate) queue: Arc<RequestQueue>,
    pub(crate) bodies: Arc<BodyPool>,
}

impl Request {
    fn base(&self) -> &windows::Win32::Networking::HttpServer::HTTP_REQUEST_V1 {
        // SAFETY: a `Request` is only constructed from a buffer that a
        // successful `HttpReceiveHttpRequest` has filled in.
        &unsafe { self.buffer.request() }.Base
    }

    /// Application-defined value registered with the URL prefix that matched
    /// this request (the `context` passed to [`UrlGroup::add_url`]).
    ///
    /// [`UrlGroup::add_url`]: crate::UrlGroup::add_url
    pub fn url_context(&self) -> u64 {
        self.base().UrlContext
    }

    /// Kernel request identifier.
    pub fn request_id(&self) -> u64 {
        self.base().RequestId
    }

    /// Identifier of the connection this request arrived on. Stable across
    /// keep-alive requests.
    pub fn connection_id(&self) -> u64 {
        self.base().ConnectionId
    }

    /// HTTP method.
    pub fn method(&self) -> Method {
        let base = self.base();
        let unknown_len = base.UnknownVerbLength as usize;
        if unknown_len > 0 {
            // SAFETY: on a successful receive the kernel points `pUnknownVerb`
            // at `UnknownVerbLength` bytes inside our own buffer.
            let bytes = unsafe { std::slice::from_raw_parts(base.pUnknownVerb.0, unknown_len) };
            return String::from_utf8_lossy(bytes)
                .parse()
                .unwrap_or(Method::Get);
        }
        match base.Verb {
            v if v == HttpVerbGET => Method::Get,
            v if v == HttpVerbPOST => Method::Post,
            v if v == HttpVerbPUT => Method::Put,
            v if v == HttpVerbDELETE => Method::Delete,
            v if v == HttpVerbHEAD => Method::Head,
            v if v == HttpVerbOPTIONS => Method::Options,
            v if v == HttpVerbTRACE => Method::Trace,
            v if v == HttpVerbCONNECT => Method::Connect,
            // http.sys has no HTTP_VERB for PATCH or the WebDAV verbs it does
            // not parse; those arrive through the `pUnknownVerb` path above.
            v => Method::Other(format!("{v:?}")),
        }
    }

    /// Raw request target as sent on the wire (path plus query string).
    pub fn url(&self) -> Cow<'_, str> {
        let base = self.base();
        let len = base.RawUrlLength as usize;
        if len == 0 || base.pRawUrl.is_null() {
            return Cow::Borrowed("");
        }
        // SAFETY: the kernel guarantees `pRawUrl` covers `RawUrlLength` bytes
        // inside the receive buffer, which this `Request` owns.
        let bytes = unsafe { std::slice::from_raw_parts(base.pRawUrl.0, len) };
        String::from_utf8_lossy(bytes)
    }

    /// `true` if more body data is waiting in the kernel beyond
    /// [`Request::inline_body`].
    pub fn has_more_body(&self) -> bool {
        self.base().Flags & HTTP_REQUEST_FLAG_MORE_ENTITY_BODY_EXISTS != 0
    }

    /// The part of the body http.sys copied in alongside the headers.
    ///
    /// For the overwhelming majority of requests this is the whole body, and
    /// reading it costs no syscall at all.
    pub fn inline_body(&self) -> &[u8] {
        let base = self.base();
        if base.EntityChunkCount == 0 || base.pEntityChunks.is_null() {
            return &[];
        }
        // http.sys always emits a single `HttpDataChunkFromMemory` chunk here.
        // SAFETY: on a successful receive the chunk array and the memory it
        // points at both live inside this request's buffer.
        let chunk = unsafe { &(*base.pEntityChunks).Anonymous.FromMemory };
        if chunk.pBuffer.is_null() || chunk.BufferLength == 0 {
            return &[];
        }
        // SAFETY: as above — `BufferLength` bytes starting at `pBuffer`.
        unsafe {
            std::slice::from_raw_parts(chunk.pBuffer.cast::<u8>(), chunk.BufferLength as usize)
        }
    }

    fn known_header(&self, id: usize) -> Option<&str> {
        let header = &self.base().Headers.KnownHeaders[id];
        if header.RawValueLength == 0 || header.pRawValue.is_null() {
            return None;
        }
        // SAFETY: header values point into the receive buffer this request owns.
        let bytes = unsafe {
            std::slice::from_raw_parts(header.pRawValue.0, header.RawValueLength as usize)
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

    /// Read the whole request body.
    ///
    /// When the body arrived inline with the headers — which is the common
    /// case, and what [`ServerConfig::request_buffer_bytes`] controls — this
    /// borrows straight out of the receive buffer: no syscall, no copy, no
    /// allocation. Otherwise it fills a pooled buffer.
    ///
    /// [`ServerConfig::request_buffer_bytes`]: crate::ServerConfig::request_buffer_bytes
    pub async fn body(&self) -> Result<Body<'_>> {
        let inline = self.inline_body();
        if !self.has_more_body() {
            return Ok(Body {
                inline,
                owned: None,
            });
        }

        // Size the buffer from Content-Length up front. Growing it inside the
        // read loop would mean reallocating and copying everything received so
        // far on every chunk, which turns a large upload quadratic.
        let declared = self.content_length().unwrap_or(0).min(MAX_BODY_RESERVE) as usize;
        let capacity = if declared > inline.len() {
            declared
        } else {
            // No usable Content-Length: start with one chunk and grow.
            inline.len() + BODY_CHUNK_BYTES
        };

        let mut body = self.bodies.take(capacity);
        body.extend_from_slice(inline);
        self.read_remaining_body_into(&mut body).await?;
        Ok(Body {
            inline,
            owned: Some(body),
        })
    }

    /// Append everything still buffered in the kernel to `body`, reading
    /// straight into its spare capacity so the bytes are never copied twice.
    async fn read_remaining_body_into(&self, body: &mut PooledVec) -> Result<()> {
        loop {
            if body.len() == body.capacity() {
                body.reserve(BODY_CHUNK_BYTES);
            }
            let filled = body.len();
            let spare = body.capacity() - filled;
            // SAFETY: the `spare` bytes past `filled` are allocated but
            // uninitialized, which is exactly what the kernel wants to write
            // into. `set_len` below only ever covers bytes it reported writing.
            let dst =
                unsafe { std::slice::from_raw_parts_mut(body.as_mut_ptr().add(filled), spare) };
            match self.queue.read_entity_body(self.request_id(), dst).await? {
                Some(0) | None => return Ok(()),
                // SAFETY: the kernel just initialized `n` bytes at `filled`.
                Some(n) => unsafe { body.set_len(filled + n) },
            }
        }
    }

    /// Read the next slice of body data into `dst`, returning `None` at
    /// end of body.
    ///
    /// Only useful when [`Request::has_more_body`] is `true`; prefer
    /// [`Request::body`] unless you need to bound memory use.
    pub async fn read_body_chunk(&self, dst: &mut [u8]) -> Result<Option<usize>> {
        self.queue.read_entity_body(self.request_id(), dst).await
    }
}

/// The body of a [`Request`], borrowed from the receive buffer when it fits
/// there and held in a pooled buffer otherwise. Dereferences to `[u8]` either
/// way, and returns its buffer to the pool when dropped.
pub struct Body<'a> {
    inline: &'a [u8],
    owned: Option<PooledVec>,
}

impl Body<'_> {
    /// `true` if the body was read without any syscall or copy.
    pub fn is_inline(&self) -> bool {
        self.owned.is_none()
    }
}

impl std::ops::Deref for Body<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match &self.owned {
            Some(owned) => owned.as_slice(),
            None => self.inline,
        }
    }
}

impl std::fmt::Debug for Body<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Body")
            .field("len", &self.len())
            .field("inline", &self.is_inline())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_methods_parse() {
        assert_eq!("GET".parse::<Method>().unwrap(), Method::Get);
        assert_eq!("PATCH".parse::<Method>().unwrap(), Method::Patch);
    }

    #[test]
    fn extension_method_falls_back_to_other() {
        assert_eq!(
            "PROPFIND".parse::<Method>().unwrap(),
            Method::Other("PROPFIND".to_owned())
        );
    }
}
