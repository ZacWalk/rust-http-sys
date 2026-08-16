//! http.sys initialization: process-global init, server sessions, URL groups.

use std::{ffi::c_void, sync::Arc};

use windows::{
    Win32::Networking::HttpServer::{
        HTTP_BINDING_INFO, HTTP_INITIALIZE_CONFIG, HTTP_INITIALIZE_SERVER, HTTP_PROPERTY_FLAGS,
        HTTP_SERVER_PROPERTY, HTTPAPI_VERSION, HttpAddUrlToUrlGroup, HttpCloseServerSession,
        HttpCloseUrlGroup, HttpCreateServerSession, HttpCreateUrlGroup, HttpInitialize,
        HttpServerBindingProperty, HttpSetUrlGroupProperty, HttpTerminate,
    },
    core::HSTRING,
};

use crate::error::{Error, Result};

pub(crate) const HTTPAPI_VERSION_2: HTTPAPI_VERSION = HTTPAPI_VERSION {
    HttpApiMajorVersion: 2,
    HttpApiMinorVersion: 0,
};

/// `HTTP_PROPERTY_FLAGS` with the `Present` bit set.
pub(crate) const PROPERTY_PRESENT: HTTP_PROPERTY_FLAGS = HTTP_PROPERTY_FLAGS { _bitfield: 1 };

/// RAII guard for `HttpInitialize` / `HttpTerminate`.
///
/// Required before any other http.sys call. Keep one alive for the lifetime of
/// any [`Server`](crate::Server) you build — [`ServerBuilder`](crate::ServerBuilder)
/// does this for you.
pub struct HttpInitializer {
    _private: (),
}

impl HttpInitializer {
    /// Initialize http.sys with `HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG`.
    pub fn new() -> Result<Self> {
        // SAFETY: a plain FFI call with no pointer arguments.
        let ec = unsafe {
            HttpInitialize(
                HTTPAPI_VERSION_2,
                HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG,
                None,
            )
        };
        Error::check(ec)?;
        Ok(Self { _private: () })
    }
}

impl Drop for HttpInitializer {
    fn drop(&mut self) {
        // SAFETY: balances the `HttpInitialize` in `new`.
        let ec = unsafe { HttpTerminate(HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG, None) };
        if ec != 0 {
            eprintln!("httpsys: HttpTerminate failed: {}", Error::Win32(ec));
        }
    }
}

/// A server session groups one or more URL groups under shared properties
/// (timeouts, authentication, logging, ...).
pub struct ServerSession {
    pub(crate) id: u64,
}

impl ServerSession {
    pub fn new() -> Result<Self> {
        let mut id: u64 = 0;
        // SAFETY: `id` is a live local for the duration of the call.
        let ec = unsafe { HttpCreateServerSession(HTTPAPI_VERSION_2, &mut id, None) };
        Error::check(ec)?;
        Ok(ServerSession { id })
    }
}

impl Drop for ServerSession {
    fn drop(&mut self) {
        // SAFETY: `id` was produced by `HttpCreateServerSession` and is closed
        // exactly once.
        let ec = unsafe { HttpCloseServerSession(self.id) };
        if ec != 0 {
            eprintln!(
                "httpsys: HttpCloseServerSession failed: {}",
                Error::Win32(ec)
            );
        }
    }
}

/// A URL group ties one or more URL prefixes to a request queue.
pub struct UrlGroup {
    _session: Arc<ServerSession>,
    pub(crate) id: u64,
}

impl UrlGroup {
    pub fn new(session: &Arc<ServerSession>) -> Result<Self> {
        let mut id: u64 = 0;
        // SAFETY: `id` is a live local; the session id is valid.
        let ec = unsafe { HttpCreateUrlGroup(session.id, &mut id, None) };
        Error::check(ec)?;
        Ok(UrlGroup {
            _session: Arc::clone(session),
            id,
        })
    }

    /// Bind this URL group to a request queue.
    pub(crate) fn bind_to_queue(
        &self,
        queue_handle: windows::Win32::Foundation::HANDLE,
    ) -> Result<()> {
        let info = HTTP_BINDING_INFO {
            Flags: PROPERTY_PRESENT,
            RequestQueueHandle: queue_handle,
        };
        // SAFETY: `info` outlives the call and its length is its own size.
        unsafe {
            self.set_property(
                HttpServerBindingProperty,
                std::ptr::from_ref(&info).cast::<c_void>(),
                size_of::<HTTP_BINDING_INFO>() as u32,
            )
        }
    }

    /// # Safety
    ///
    /// `info` must point to at least `len` initialized bytes matching the
    /// layout http.sys expects for `property`.
    unsafe fn set_property(
        &self,
        property: HTTP_SERVER_PROPERTY,
        info: *const c_void,
        len: u32,
    ) -> Result<()> {
        // SAFETY: forwarded from this function's own contract.
        let ec = unsafe { HttpSetUrlGroupProperty(self.id, property, info, len) };
        Error::check(ec)
    }

    /// Add a URL prefix such as `http://+:8080/api/`.
    ///
    /// http.sys requires prefixes to end in `/`; one is appended if missing.
    /// `context` is echoed back in [`Request::url_context`](crate::Request::url_context)
    /// for every request that matches this prefix.
    pub fn add_url(&self, url: &str, context: u64) -> Result<()> {
        let url = normalize_prefix(url);
        let wide = HSTRING::from(url.as_ref());
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
        let ec = unsafe { HttpAddUrlToUrlGroup(self.id, &wide, context, None) };
        Error::check(ec)
    }
}

impl Drop for UrlGroup {
    fn drop(&mut self) {
        // SAFETY: `id` came from `HttpCreateUrlGroup` and is closed once.
        let ec = unsafe { HttpCloseUrlGroup(self.id) };
        if ec != 0 {
            eprintln!("httpsys: HttpCloseUrlGroup failed: {}", Error::Win32(ec));
        }
    }
}

/// http.sys rejects any UrlPrefix that does not end in `/`.
fn normalize_prefix(url: &str) -> std::borrow::Cow<'_, str> {
    if url.ends_with('/') {
        std::borrow::Cow::Borrowed(url)
    } else {
        std::borrow::Cow::Owned(format!("{url}/"))
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_prefix;

    #[test]
    fn prefix_gains_a_trailing_slash() {
        assert_eq!(
            normalize_prefix("http://+:8080/test"),
            "http://+:8080/test/"
        );
        assert_eq!(
            normalize_prefix("http://+:8080/test/"),
            "http://+:8080/test/"
        );
        assert_eq!(normalize_prefix("http://+:8080/"), "http://+:8080/");
    }
}
