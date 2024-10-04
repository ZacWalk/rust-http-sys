//! High-level HTTP server with async handler trait, concurrent receivers,
//! and graceful shutdown.

use std::{collections::HashMap, future::Future, sync::Arc, thread::available_parallelism};

use tokio::{
    sync::{broadcast, Semaphore},
    task::JoinSet,
    time::{timeout, Duration},
};
use windows::Win32::Foundation::{ERROR_HANDLE_EOF, ERROR_INVALID_HANDLE, ERROR_OPERATION_ABORTED};

use crate::{
    error::{Error, Result},
    init::{HttpInitializer, ServerSession, UrlGroup},
    queue::RequestQueue,
    request::{RawRequest, Request},
    response::{status, Response},
};

/// Asynchronous handler invoked once per request.
///
/// Implemented automatically for `async fn(Request) -> Response` closures via
/// the blanket impl on `Fn`. For custom state, implement the trait directly.
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, req: Request) -> impl Future<Output = Response> + Send;
}

impl<F, Fut> Handler for F
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    fn handle(&self, req: Request) -> impl Future<Output = Response> + Send {
        (self)(req)
    }
}

/// Configuration knobs for [`Server`].
pub struct ServerConfig {
    /// Number of concurrent `HttpReceiveHttpRequest` calls posted against
    /// the queue. Defaults to `available_parallelism()`.
    pub receivers: usize,
    /// Maximum number of in-flight handler tasks. Excess requests park on a
    /// semaphore. Defaults to 1024.
    pub max_in_flight: usize,
    /// Maximum time to wait for in-flight handlers to drain on graceful
    /// shutdown. Defaults to 30 seconds.
    pub shutdown_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            receivers: available_parallelism().map(|n| n.get()).unwrap_or(2),
            max_in_flight: 1024,
            shutdown_timeout: Duration::from_secs(30),
        }
    }
}

/// Builder for a [`Server`]. Holds the http.sys init handles and the routes
/// being assembled.
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
    /// Initialize http.sys and create an empty server. Returns an error if
    /// `HttpInitialize` or queue creation fails.
    pub fn new() -> Result<Self> {
        Self::with_config(ServerConfig::default())
    }

    pub fn with_config(config: ServerConfig) -> Result<Self> {
        let init = HttpInitializer::new()?;
        let session = Arc::new(ServerSession::new()?);
        let group = Arc::new(UrlGroup::new(&session)?);
        let queue = Arc::new(RequestQueue::new()?);
        queue.bind_url_group(&group)?;
        Ok(Self {
            init,
            session,
            group,
            queue,
            routes: HashMap::new(),
            next_id: 1000,
            config,
        })
    }

    /// Register a handler for a URL prefix. Returns the kernel error if the
    /// URL cannot be added (commonly `ERROR_ACCESS_DENIED` when no URL ACL
    /// has been registered for the user).
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

    /// Spawn the server. Returns a [`Server`] handle that owns the worker
    /// thread and lets you trigger graceful shutdown.
    pub fn run(self) -> Server {
        let queue = self.queue.clone();
        let routes = Arc::new(self.routes);
        let cfg = self.config;
        let (shutdown_tx, _) = broadcast::channel::<()>(8);
        let shutdown_tx_thread = shutdown_tx.clone();

        let _holders = OwnedHandles {
            init: self.init,
            session: self.session,
            group: self.group,
            queue: queue.clone(),
        };

        let worker = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(run_server(queue, routes, cfg, shutdown_tx_thread, _holders));
        });

        Server {
            worker: Some(worker),
            shutdown_tx,
        }
    }
}

/// A running server. Drop or call [`Server::shutdown`] to stop it.
pub struct Server {
    worker: Option<std::thread::JoinHandle<()>>,
    shutdown_tx: broadcast::Sender<()>,
}

impl Server {
    /// Trigger graceful shutdown: stop accepting new connections and let
    /// in-flight handlers finish (subject to the configured timeout).
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    /// Wait for the worker thread to exit. Returns immediately if it
    /// already has.
    pub fn join(&mut self) {
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
        self.join();
    }
}

/// RAII bag of handles the worker thread keeps alive for the duration of
/// the runtime. Dropped *after* `run_server` returns.
#[allow(dead_code)]
struct OwnedHandles {
    init: HttpInitializer,
    session: Arc<ServerSession>,
    group: Arc<UrlGroup>,
    queue: Arc<RequestQueue>,
}

// SAFETY: every contained type is Send.
unsafe impl Send for OwnedHandles {}

trait ErasedHandler: Send + Sync {
    fn invoke(&self, req: Request)
        -> std::pin::Pin<Box<dyn Future<Output = Response> + Send + '_>>;
}

struct HandlerWrapper<H>(H);

impl<H: Handler> ErasedHandler for HandlerWrapper<H> {
    fn invoke(
        &self,
        req: Request,
    ) -> std::pin::Pin<Box<dyn Future<Output = Response> + Send + '_>> {
        Box::pin(self.0.handle(req))
    }
}

async fn run_server(
    queue: Arc<RequestQueue>,
    routes: Arc<HashMap<u64, Arc<dyn ErasedHandler>>>,
    cfg: ServerConfig,
    shutdown_tx: broadcast::Sender<()>,
    _holders: OwnedHandles,
) {
    let semaphore = Arc::new(Semaphore::new(cfg.max_in_flight));
    let mut receivers = JoinSet::new();

    for _ in 0..cfg.receivers {
        let queue = queue.clone();
        let routes = routes.clone();
        let semaphore = semaphore.clone();
        let mut shutdown = shutdown_tx.subscribe();
        receivers.spawn(async move {
            receiver_loop(queue, routes, semaphore, &mut shutdown).await;
        });
    }

    // Wait for shutdown signal, then drain.
    let mut shutdown = shutdown_tx.subscribe();
    let _ = shutdown.recv().await;

    // Initiate graceful kernel-side shutdown so receive_request returns
    // ERROR_OPERATION_ABORTED in each receiver.
    if let Err(e) = queue.shutdown() {
        eprintln!("HttpShutdownRequestQueue failed: {e}");
    }

    // Wait for receivers to exit (they break on aborted/eof errors).
    let drain = async { while receivers.join_next().await.is_some() {} };
    if timeout(cfg.shutdown_timeout, drain).await.is_err() {
        eprintln!("Receivers did not drain within shutdown timeout; aborting them.");
        receivers.abort_all();
        while receivers.join_next().await.is_some() {}
    }

    // Drop holders here implicitly — happens when this fn returns.
}

async fn receiver_loop(
    queue: Arc<RequestQueue>,
    routes: Arc<HashMap<u64, Arc<dyn ErasedHandler>>>,
    semaphore: Arc<Semaphore>,
    shutdown: &mut broadcast::Receiver<()>,
) {
    loop {
        let mut raw = Box::<RawRequest>::default();

        let res = tokio::select! {
            biased;
            _ = shutdown.recv() => return,
            r = queue.receive_request(&mut raw) => r,
        };

        match res {
            Err(e) if is_terminal(&e) => return,
            Err(e) => {
                eprintln!("receive_request failed: {e}");
                continue;
            }
            Ok(_) => {
                let url_context = raw.raw.Base.UrlContext;
                let request_id = raw.raw.Base.RequestId;

                let Some(handler) = routes.get(&url_context).cloned() else {
                    eprintln!("Unknown URL context {url_context}; replying 404");
                    let resp = Response::new(status::NOT_FOUND);
                    let _ = queue.send_response(request_id, 0, &resp).await;
                    continue;
                };

                // Acquire a permit to bound concurrency.
                let permit = match semaphore.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return, // semaphore closed
                };

                let queue_inner = queue.clone();
                let req = Request {
                    raw,
                    queue: queue_inner.clone(),
                };
                tokio::spawn(async move {
                    let resp = handler.invoke(req).await;
                    if let Err(e) = queue_inner.send_response(request_id, 0, &resp).await {
                        eprintln!("send_response failed for request {request_id}: {e}");
                    }
                    drop(permit);
                });
            }
        }
    }
}

fn is_terminal(e: &Error) -> bool {
    let Error::Win32 { code, .. } = e else {
        return false;
    };
    let win = (*code as u32) & 0xFFFF;
    win == ERROR_OPERATION_ABORTED.0 || win == ERROR_HANDLE_EOF.0 || win == ERROR_INVALID_HANDLE.0
}
