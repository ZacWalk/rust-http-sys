//! HTTP server initialization: process-global init, sessions, URL groups.

use std::{ffi::c_void, sync::Arc};

use windows::{
    core::HSTRING,
    Win32::Networking::HttpServer::{
        HttpAddUrlToUrlGroup, HttpCloseServerSession, HttpCloseUrlGroup, HttpCreateServerSession,
        HttpCreateUrlGroup, HttpInitialize, HttpServerBindingProperty, HttpSetUrlGroupProperty,
        HttpTerminate, HTTPAPI_VERSION, HTTP_BINDING_INFO, HTTP_INITIALIZE_CONFIG,
        HTTP_INITIALIZE_SERVER, HTTP_PROPERTY_FLAGS, HTTP_SERVER_PROPERTY,
    },
};

use crate::error::{Error, Result};

pub(crate) const HTTPAPI_VERSION_2: HTTPAPI_VERSION = HTTPAPI_VERSION {
    HttpApiMajorVersion: 2,
    HttpApiMinorVersion: 0,
};

/// RAII handle for `HttpInitialize` / `HttpTerminate`. Required before any
/// other http.sys call. Construct one and keep it alive for the lifetime of
/// any `Server` you build.
pub struct HttpInitializer {
    _private: (),
}

impl HttpInitializer {
    /// Initialize http.sys with `HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG`.
    pub fn new() -> Result<Self> {
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
        let ec = unsafe { HttpTerminate(HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG, None) };
        if ec != 0 {
            eprintln!("HttpTerminate failed: 0x{ec:08x}");
        }
    }
}

/// A server session groups one or more URL groups under shared properties
/// (timeouts, auth, logging, ...).
pub struct ServerSession {
    pub(crate) id: u64,
}

impl ServerSession {
    pub fn new() -> Result<Self> {
        let mut id: u64 = 0;
        let ec = unsafe { HttpCreateServerSession(HTTPAPI_VERSION_2, &mut id, 0) };
        Error::check(ec)?;
        Ok(ServerSession { id })
    }
}

impl Drop for ServerSession {
    fn drop(&mut self) {
        let ec = unsafe { HttpCloseServerSession(self.id) };
        if ec != 0 {
            eprintln!("HttpCloseServerSession failed: 0x{ec:08x}");
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
        let ec = unsafe { HttpCreateUrlGroup(session.id, &mut id, 0) };
        Error::check(ec)?;
        Ok(UrlGroup {
            _session: Arc::clone(session),
            id,
        })
    }

    /// Bind this URL group to a request queue. Calls
    /// `HttpSetUrlGroupProperty(HttpServerBindingProperty)`.
    pub(crate) fn bind_to_queue(
        &self,
        queue_handle: windows::Win32::Foundation::HANDLE,
    ) -> Result<()> {
        let info = HTTP_BINDING_INFO {
            Flags: HTTP_PROPERTY_FLAGS { _bitfield: 1 },
            RequestQueueHandle: queue_handle,
        };
        unsafe {
            self.set_property(
                HttpServerBindingProperty,
                (&info as *const HTTP_BINDING_INFO) as *const c_void,
                std::mem::size_of::<HTTP_BINDING_INFO>() as u32,
            )
        }
    }

    unsafe fn set_property(
        &self,
        property: HTTP_SERVER_PROPERTY,
        propertyinformation: *const c_void,
        propertyinformationlength: u32,
    ) -> Result<()> {
        let ec = unsafe {
            HttpSetUrlGroupProperty(
                self.id,
                property,
                propertyinformation,
                propertyinformationlength,
            )
        };
        Error::check(ec)
    }

    /// Add a URL prefix (e.g. `http://+:8080/api/`). The `context` value is
    /// returned in the `UrlContext` field of every request matching this URL.
    pub fn add_url(&self, url: &str, context: u64) -> Result<()> {
        let h = HSTRING::from(url);
        let ec = unsafe { HttpAddUrlToUrlGroup(self.id, &h, context, 0) };
        Error::check(ec)
    }
}

impl Drop for UrlGroup {
    fn drop(&mut self) {
        let ec = unsafe { HttpCloseUrlGroup(self.id) };
        if ec != 0 {
            eprintln!("HttpCloseUrlGroup failed: 0x{ec:08x}");
        }
    }
}
