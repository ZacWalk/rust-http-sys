//! `httpsys` — an async Rust wrapper around the Windows `http.sys` server API.
//!
//! ```no_run
//! use httpsys::{Request, Response, ServerBuilder, status};
//!
//! # fn main() -> httpsys::Result<()> {
//! let mut builder = ServerBuilder::new()?;
//! builder.route("http://+:8080/api/", |request: Request| async move {
//!     match request.body().await {
//!         Ok(body) => Response::ok_text(format!("{} bytes", body.len())),
//!         Err(_) => Response::new(status::BAD_REQUEST),
//!     }
//! })?;
//!
//! let mut server = builder.run();
//! // ... do other work ...
//! server.shutdown();
//! server.join();
//! # Ok(())
//! # }
//! ```
//!
//! # How it goes fast
//!
//! * **Its own completion port.** The queue handle is associated with a
//!   dedicated IOCP whose threads drain completions in batches with
//!   `GetQueuedCompletionStatusEx`, instead of paying a Win32 thread-pool
//!   dispatch per completion as `BindIoCompletionCallback` does.
//! * **No round trip for inline completions.** The handle is put into
//!   `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` mode, so a receive that http.sys
//!   can satisfy from an already-queued request never touches the port at all.
//! * **One syscall per request.** Requests are received with
//!   `HTTP_RECEIVE_REQUEST_FLAG_COPY_BODY`, so headers and body arrive
//!   together and [`Request::body`] usually returns a borrowed slice.
//! * **No allocation on the hot path.** Receive buffers are recycled through a
//!   lock-free pool, and [`Response`] holds its body as [`bytes::Bytes`].
//! * **No task churn.** A fixed set of request slots each own a request from
//!   receive to response, so there is no per-request spawn or semaphore.
//!
//! # Safety model
//!
//! Every async operation owns an `OVERLAPPED` whose strong reference is handed
//! to the kernel and reclaimed exactly once — by the completion thread, or by
//! the submitting task when the kernel completes inline or refuses the call.
//! Dropping a future before completion issues `CancelIoEx` and blocks until
//! the completion has fired, so the kernel never writes into a buffer Rust has
//! already reused.

mod buffer;
mod error;
mod init;
mod iocp;
mod overlapped;
mod queue;
mod request;
mod response;
mod server;

pub use error::{Error, Result};
pub use init::{HttpInitializer, ServerSession, UrlGroup};
pub use queue::RequestQueue;
pub use request::{Body, Method, Request};
pub use response::{Response, status};
pub use server::{Handler, Server, ServerBuilder, ServerConfig};
