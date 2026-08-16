//! The two servers under test.
//!
//! Both expose the same two endpoints and do the same work per request — read
//! the whole request body, reply `200 OK` with a two-byte `text/plain` body —
//! so the only thing that differs between them is the HTTP stack underneath.

use std::sync::{Arc, Mutex, mpsc};

use axum::{
    Router, extract::DefaultBodyLimit, response::IntoResponse, routing::any, serve::ListenerExt,
};
use bytes::Bytes;
use httpsys::{Response, ServerBuilder, ServerConfig};
use tokio::net::TcpListener;

use crate::Result;

/// Which HTTP stack to serve with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Engine {
    /// The `httpsys` crate in this workspace.
    Httpsys,
    /// `axum` on hyper, the reference point.
    Axum,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Httpsys => "httpsys",
            Engine::Axum => "axum",
        }
    }
}

impl std::fmt::Display for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Default path the load generator drives.
pub const BENCH_PATH: &str = "/bench/";

/// The sibling `shutdown/` path for a given bench path.
///
/// Keeping the two under one parent matters for http.sys: a URL ACL is granted
/// per prefix, so both routes have to live inside the reservation the process
/// is allowed to listen on.
pub fn shutdown_path(bench_path: &str) -> String {
    let parent = bench_path
        .trim_end_matches('/')
        .rsplit_once('/')
        .map(|(head, _)| head)
        .unwrap_or("");
    format!("{parent}/shutdown/")
}

const REPLY: &str = "OK";
const REPLY_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

/// Serve with the given engine until a request arrives on the shutdown path.
pub fn serve(engine: Engine, port: u16, path: &str, options: &ServeOptions) -> Result<()> {
    match engine {
        Engine::Httpsys => serve_httpsys(port, path, options),
        Engine::Axum => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(serve_axum(port, path))
        }
    }
}

/// Knobs that only affect the httpsys engine.
#[derive(Debug, Clone, Copy)]
pub struct ServeOptions {
    /// Bytes per receive buffer; anything that fits arrives in one syscall.
    pub request_buffer_bytes: usize,
    pub kernel_cache: bool,
}

fn serve_httpsys(port: u16, path: &str, options: &ServeOptions) -> Result<()> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();

    let kernel_cache = options.kernel_cache;
    let mut builder = ServerBuilder::with_config(ServerConfig {
        request_buffer_bytes: options.request_buffer_bytes,
        ..ServerConfig::default()
    })?;
    builder
        .route(
            &format!("http://+:{port}{path}"),
            move |request: httpsys::Request| async move {
                // Match axum's `Bytes` extractor: consume the whole body.
                if let Err(e) = request.body().await {
                    eprintln!("httpbench: body read failed: {e}");
                }
                let response = Response::new(httpsys::status::OK)
                    .with_content_type(REPLY_CONTENT_TYPE)
                    .with_body(REPLY);
                if kernel_cache {
                    response.cache_in_kernel_for_secs(60)
                } else {
                    response
                }
            },
        )
        .map_err(|e| url_acl_hint(e, port, path))?;
    builder
        .route(
            &format!("http://+:{port}{}", shutdown_path(path)),
            move |_: httpsys::Request| {
                let stop_tx = stop_tx.clone();
                async move {
                    let _ = stop_tx.send(());
                    Response::ok_text(REPLY)
                }
            },
        )
        .map_err(|e| url_acl_hint(e, port, path))?;

    let mut server = builder.run();
    // The sender is cloned into the handler, so this only returns once a
    // shutdown request has arrived (or every clone has been dropped).
    let _ = stop_rx.recv();
    server.shutdown();
    server.join();
    Ok(())
}

async fn serve_axum(port: u16, path: &str) -> Result<()> {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let stop_tx = Arc::new(Mutex::new(Some(stop_tx)));

    let app = Router::new()
        .route(path, any(bench))
        .route(
            &shutdown_path(path),
            any(move || {
                let stop_tx = Arc::clone(&stop_tx);
                async move {
                    if let Some(tx) = stop_tx.lock().expect("shutdown mutex").take() {
                        let _ = tx.send(());
                    }
                    reply()
                }
            }),
        )
        // The size sweep posts bodies far past axum's 2 MiB default.
        .layer(DefaultBodyLimit::disable());

    let listener = TcpListener::bind(("0.0.0.0", port))
        .await?
        // hyper does not set TCP_NODELAY itself; without this axum would be
        // measured against Nagle rather than against httpsys.
        .tap_io(|stream| {
            if let Err(e) = stream.set_nodelay(true) {
                eprintln!("httpbench: set_nodelay failed: {e}");
            }
        });

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = stop_rx.await;
        })
        .await?;
    Ok(())
}

async fn bench(body: Bytes) -> impl IntoResponse {
    let _ = body;
    reply()
}

fn reply() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, REPLY_CONTENT_TYPE)],
        REPLY,
    )
}

/// `ERROR_ACCESS_DENIED` from `HttpAddUrlToUrlGroup` almost always means a
/// missing URL ACL, which is not obvious from the raw message.
fn url_acl_hint(error: httpsys::Error, port: u16, path: &str) -> String {
    const ERROR_ACCESS_DENIED: u32 = 5;
    if error.win32_code() == Some(ERROR_ACCESS_DENIED) {
        format!(
            "cannot reserve http://+:{port}{path} ({error}).\n\
             Either run once as administrator:\n    \
             netsh http add urlacl url=http://+:{port}/ user=Everyone\n\
             or reuse the reservation every user already has:\n    \
             --port 80 --path /Temporary_Listen_Addresses/bench/"
        )
    } else {
        format!("cannot reserve http://+:{port}{path}: {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_labels_match_the_cli_values() {
        assert_eq!(Engine::Httpsys.to_string(), "httpsys");
        assert_eq!(Engine::Axum.to_string(), "axum");
    }

    #[test]
    fn access_denied_explains_the_url_acl() {
        let hint = url_acl_hint(httpsys::Error::Win32(5), 8080, BENCH_PATH);
        assert!(
            hint.contains("netsh http add urlacl url=http://+:8080/"),
            "{hint}"
        );
        assert!(hint.contains("Temporary_Listen_Addresses"), "{hint}");
    }

    #[test]
    fn other_errors_are_passed_through() {
        let hint = url_acl_hint(httpsys::Error::Win32(87), 8080, BENCH_PATH);
        assert!(!hint.contains("netsh"), "{hint}");
    }

    #[test]
    fn shutdown_is_a_sibling_of_the_bench_path() {
        assert_eq!(shutdown_path("/bench/"), "/shutdown/");
        assert_eq!(
            shutdown_path("/Temporary_Listen_Addresses/bench/"),
            "/Temporary_Listen_Addresses/shutdown/"
        );
    }
}
