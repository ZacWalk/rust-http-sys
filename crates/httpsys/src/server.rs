//! High-level HTTP server: typed async handlers, a fixed pool of request
//! slots, and graceful shutdown.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::available_parallelism,
};

use tokio::{
    sync::broadcast,
    task::JoinSet,
    time::{Duration, timeout},
};
use windows::Win32::Foundation::{
    ERROR_INVALID_HANDLE, ERROR_NO_MORE_FILES, ERROR_OPERATION_ABORTED,
};

use crate::{
    buffer::{BodyPool, BufferPool, PooledBuffer},
    error::{Error, Result},
    init::{HttpInitializer, ServerSession, UrlGroup},
    queue::{ReceiveMode, RequestQueue},
    request::Request,
    response::{Response, status},
};

/// Asynchronous handler invoked once per request.
///
/// Implemented automatically for `Fn(Request) -> impl Future<Output = Response>`,
/// so plain async closures work. Implement it directly when the handler needs
/// to own state.
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, request: Request) -> impl Future<Output = Response> + Send;
}

impl<F, Fut> Handler for F
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    fn handle(&self, request: Request) -> impl Future<Output = Response> + Send {
        (self)(request)
    }
}

/// Tuning knobs for [`Server`].
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Number of request slots. Each slot keeps one `HttpReceiveHttpRequest`
    /// posted against the queue and runs one handler to completion, so this is
    /// both the receive depth and the handler concurrency limit.
    ///
    /// Defaults to `32 x available_parallelism()`.
    pub concurrency: usize,
    /// Threads draining the completion port. Completions are cheap — they only
    /// wake a task — so a small number saturates easily.
    ///
    /// Defaults to `available_parallelism() / 2`, at least 2.
    pub io_threads: usize,
    /// Bytes per receive buffer. Anything that fits — headers plus body —
    /// arrives in a single syscall.
    ///
    /// Defaults to 16 KiB.
    pub request_buffer_bytes: usize,
    /// Ceiling that a receive buffer may grow to when a request slot keeps
    /// seeing bodies too large for [`ServerConfig::request_buffer_bytes`].
    /// Raising it trades memory for fewer syscalls on upload-heavy workloads.
    ///
    /// Defaults to 128 KiB.
    pub max_request_buffer_bytes: usize,
    /// Requests http.sys will queue before rejecting connections with 503.
    ///
    /// Defaults to 3000.
    pub queue_length: u32,
    /// Largest body buffer kept for reuse once its request is done. Bodies
    /// above this size are freed immediately rather than held per slot.
    ///
    /// Defaults to 1 MiB.
    pub max_pooled_body_bytes: usize,
    /// How long [`Server::shutdown`] waits for in-flight handlers to finish.
    ///
    /// Defaults to 30 seconds.
    pub shutdown_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        let cores = available_parallelism().map(|n| n.get()).unwrap_or(2);
        Self {
            concurrency: cores * 32,
            io_threads: (cores / 2).max(2),
            request_buffer_bytes: 16 * 1024,
            max_request_buffer_bytes: 128 * 1024,
            queue_length: 3000,
            max_pooled_body_bytes: 1024 * 1024,
            shutdown_timeout: Duration::from_secs(30),
        }
    }
}

/// Builder for a [`Server`]: owns the http.sys handles while routes are
/// registered.
pub struct ServerBuilder {
    init: HttpInitializer,
    session: Arc<ServerSession>,
    group: Arc<UrlGroup>,
    queue: Arc<RequestQueue>,
    routes: HashMap<u64, Arc<dyn ErasedHandler>>,
    next_id: u64,
    config: ServerConfig,
}

impl ServerBuilder {
    /// Initialize http.sys and create an empty server.
    pub fn new() -> Result<Self> {
        Self::with_config(ServerConfig::default())
    }

    pub fn with_config(config: ServerConfig) -> Result<Self> {
        let init = HttpInitializer::new()?;
        let session = Arc::new(ServerSession::new()?);
        let group = Arc::new(UrlGroup::new(&session)?);
        let queue = Arc::new(RequestQueue::new(config.io_threads, config.concurrency)?);
        queue.bind_url_group(&group)?;
        queue.set_queue_length(config.queue_length)?;
        Ok(Self {
            init,
            session,
            group,
            queue,
            routes: HashMap::new(),
            next_id: 1,
            config,
        })
    }

    /// Register a handler for a URL prefix such as `http://+:8080/api/`.
    ///
    /// Returns the kernel error if the prefix cannot be reserved — most often
    /// `ERROR_ACCESS_DENIED` (5) when no URL ACL exists for the current user.
    pub fn route<H>(&mut self, url: &str, handler: H) -> Result<&mut Self>
    where
        H: Handler,
    {
        let id = self.next_id;
        self.next_id += 1;
        self.group.add_url(url, id)?;
        self.routes.insert(
            id,
            Arc::new(HandlerWrapper(handler)) as Arc<dyn ErasedHandler>,
        );
        Ok(self)
    }

    /// Start serving on a dedicated runtime thread.
    pub fn run(self) -> Server {
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let worker_shutdown = shutdown_tx.clone();

        let queue = Arc::clone(&self.queue);
        let routes = Arc::new(self.routes);
        let config = self.config;
        let handles = OwnedHandles {
            _init: self.init,
            _session: self.session,
            _group: self.group,
            _queue: self.queue,
        };

        let worker = std::thread::Builder::new()
            .name("httpsys-server".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    // No tokio I/O driver: every socket lives in the kernel.
                    .enable_time()
                    .thread_name("httpsys-worker")
                    .build()
                    .expect("tokio runtime");
                runtime.block_on(serve(queue, routes, config, worker_shutdown));
                drop(handles);
            })
            .expect("spawn server thread");

        Server {
            worker: Some(worker),
            shutdown_tx,
        }
    }
}

/// A running server. Call [`Server::shutdown`] or drop it to stop.
pub struct Server {
    worker: Option<std::thread::JoinHandle<()>>,
    shutdown_tx: broadcast::Sender<()>,
}

impl Server {
    /// Stop accepting requests and let in-flight handlers finish, subject to
    /// [`ServerConfig::shutdown_timeout`].
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    /// Wait for the server thread to exit.
    pub fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
        self.join();
    }
}

/// Handles the server thread keeps alive for the lifetime of the runtime, and
/// tears down in the right order once it returns.
struct OwnedHandles {
    _queue: Arc<RequestQueue>,
    _group: Arc<UrlGroup>,
    _session: Arc<ServerSession>,
    _init: HttpInitializer,
}

// SAFETY: every contained handle is `Send`.
unsafe impl Send for OwnedHandles {}

/// Object-safe view of [`Handler`], so routes can live in one map.
trait ErasedHandler: Send + Sync {
    fn invoke(&self, request: Request) -> Pin<Box<dyn Future<Output = Response> + Send + '_>>;
}

struct HandlerWrapper<H>(H);

impl<H: Handler> ErasedHandler for HandlerWrapper<H> {
    fn invoke(&self, request: Request) -> Pin<Box<dyn Future<Output = Response> + Send + '_>> {
        Box::pin(self.0.handle(request))
    }
}

type Routes = Arc<HashMap<u64, Arc<dyn ErasedHandler>>>;

async fn serve(
    queue: Arc<RequestQueue>,
    routes: Routes,
    config: ServerConfig,
    shutdown_tx: broadcast::Sender<()>,
) {
    // Cap the body free list independently of the slot count: retaining one
    // megabyte-scale buffer per slot on a many-core box would be gigabytes of
    // idle memory for a burst that has long since passed.
    let bodies = BodyPool::new(config.concurrency.min(64), config.max_pooled_body_bytes);
    let stop = Arc::new(AtomicBool::new(false));

    let mut slots = JoinSet::new();
    for _ in 0..config.concurrency {
        let queue = Arc::clone(&queue);
        let routes = Arc::clone(&routes);
        // One buffer per slot rather than one shared free list: the buffer
        // resizes itself to the traffic the slot actually sees, which only
        // pays off if the same slot gets it back, and nothing is contended.
        let buffers = BufferPool::new(
            1,
            config.request_buffer_bytes,
            config.max_request_buffer_bytes,
        );
        let bodies = Arc::clone(&bodies);
        let stop = Arc::clone(&stop);
        slots.spawn(async move { slot_loop(queue, routes, buffers, bodies, stop).await });
    }

    let mut shutdown = shutdown_tx.subscribe();
    let _ = shutdown.recv().await;
    stop.store(true, Ordering::Relaxed);

    // Fail the outstanding receives so every slot parked in the kernel wakes
    // up; the flag above only covers slots between requests.
    if let Err(e) = queue.shutdown() {
        eprintln!("httpsys: HttpShutdownRequestQueue failed: {e}");
    }

    let drain = async { while slots.join_next().await.is_some() {} };
    if timeout(config.shutdown_timeout, drain).await.is_err() {
        eprintln!("httpsys: handlers did not drain within the shutdown timeout; aborting");
        slots.abort_all();
        while slots.join_next().await.is_some() {}
    }
}

/// One request slot: keep a receive posted, run the matching handler, reply,
/// repeat. Handling the request on this task rather than spawning a fresh one
/// is what keeps the per-request cost down — the slot count already bounds
/// concurrency, so there is nothing for a spawn to buy.
async fn slot_loop(
    queue: Arc<RequestQueue>,
    routes: Routes,
    buffers: Arc<BufferPool>,
    bodies: Arc<BodyPool>,
    stop: Arc<AtomicBool>,
) {
    let mut mode = ReceiveMode::default();
    // A relaxed load per request, rather than selecting over a shutdown
    // channel: with a slot per core times thirty-two, taking the channel's
    // shared lock on every request is itself a scalability limit.
    while !stop.load(Ordering::Relaxed) {
        let mut buffer = buffers.take();

        match queue.receive(&mut buffer, &mut mode).await {
            Ok(()) => {}
            Err(Error::HeadersTooLarge { needed }) => {
                eprintln!("httpsys: request headers need {needed} bytes; replying 431");
                reply(&queue, &buffer, Response::new(status::HEADERS_TOO_LARGE)).await;
                continue;
            }
            Err(e) if is_terminal(&e) => return,
            Err(e) => {
                eprintln!("httpsys: receive failed: {e}");
                continue;
            }
        }

        let request_id = buffer.request_id();
        let request = Request {
            buffer,
            queue: Arc::clone(&queue),
            bodies: Arc::clone(&bodies),
        };

        let response = match routes.get(&request.url_context()) {
            Some(handler) => handler.invoke(request).await,
            None => {
                // Only reachable if a URL is removed from the group while a
                // request for it is in flight.
                drop(request);
                Response::new(status::NOT_FOUND)
            }
        };

        if let Err(e) = queue.send_response(request_id, &response).await {
            eprintln!("httpsys: send_response failed for request {request_id}: {e}");
        }
    }
}

async fn reply(queue: &RequestQueue, buffer: &PooledBuffer, response: Response) {
    let request_id = buffer.request_id();
    if request_id != 0
        && let Err(e) = queue.send_response(request_id, &response).await
    {
        eprintln!("httpsys: send_response failed for request {request_id}: {e}");
    }
}

/// Errors that mean the queue is gone and the slot should stop looping.
fn is_terminal(e: &Error) -> bool {
    matches!(
        e.win32_code(),
        Some(code)
            if code == ERROR_OPERATION_ABORTED.0
                || code == ERROR_INVALID_HANDLE.0
                || code == ERROR_NO_MORE_FILES.0
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_self_consistent() {
        let config = ServerConfig::default();
        assert!(config.concurrency >= 32);
        assert!(config.io_threads >= 2);
        assert!(config.request_buffer_bytes >= 4096);
        assert!(config.max_request_buffer_bytes >= config.request_buffer_bytes);
        assert!(config.queue_length > 0);
        assert!(config.max_pooled_body_bytes >= config.request_buffer_bytes);
    }

    #[test]
    fn terminal_errors_are_recognised() {
        assert!(is_terminal(&Error::Win32(ERROR_OPERATION_ABORTED.0)));
        assert!(is_terminal(&Error::Win32(ERROR_INVALID_HANDLE.0)));
        assert!(!is_terminal(&Error::Win32(234))); // ERROR_MORE_DATA
        assert!(!is_terminal(&Error::HeadersTooLarge { needed: 1 }));
    }
}
