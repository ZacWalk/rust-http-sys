//! `httpsys` — async Rust wrapper around the Windows `http.sys` server APIs.
//!
//! ```no_run
//! use httpsys::{Response, ServerBuilder};
//!
//! # fn main() -> httpsys::Result<()> {
//! let mut server = ServerBuilder::new()?;
//! server.route("http://+:8080/api/", |_req| async {
//!     Response::ok_text("hello")
//! })?;
//! let mut handle = server.run();
//! # handle.shutdown();
//! handle.join();
//! # Ok(())
//! # }
//! ```
//!
//! ## Design
//!
//! - The kernel queue handle is bound to the process IOCP via
//!   `BindIoCompletionCallback`. Each async operation owns an
//!   [`overlapped::OverlappedWrap`] whose strong reference is leaked into
//!   the kernel and reclaimed by the IOCP callback.
//! - Cancellation is safe: dropping the future calls `CancelIoEx` and
//!   blocks until the callback releases its reference, so the buffer is
//!   never reused while the kernel may still be writing to it.
//! - The high-level [`Server`] spawns multiple concurrent receivers and
//!   gates handler concurrency with a [`tokio::sync::Semaphore`].
//! - Graceful shutdown calls `HttpShutdownRequestQueue` and waits for
//!   in-flight handlers to drain (configurable timeout).

#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
mod init;
mod overlapped;
mod queue;
mod request;
mod response;
mod server;

pub use error::{Error, Result};
pub use init::{HttpInitializer, ServerSession, UrlGroup};
pub use queue::RequestQueue;
pub use request::{Method, Request};
pub use response::{status, Response};
pub use server::{Handler, Server, ServerBuilder, ServerConfig};
