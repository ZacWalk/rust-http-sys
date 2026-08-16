//! Closed-loop HTTP load generator.
//!
//! Each connection is a raw hyper HTTP/1.1 connection that sends one request
//! at a time and waits for the whole response — the classic closed-loop model,
//! so "concurrency" is exactly the number of requests in flight. The client
//! stack is identical for both servers under test, so whatever overhead it
//! adds cancels out of the comparison.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, header};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use crate::Result;

/// What to hammer the server with.
#[derive(Debug, Clone)]
pub struct LoadSpec {
    pub host: String,
    pub port: u16,
    pub path: String,
    /// Request bodies to send. Empty means `GET`.
    pub body: Bytes,
    pub connections: usize,
    pub duration: Duration,
}

impl LoadSpec {
    fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Outcome of one load run.
#[derive(Debug, Default)]
pub struct LoadResult {
    pub elapsed: Duration,
    pub errors: u64,
    /// Sorted ascending, so percentiles are a direct index.
    latencies_ns: Vec<u64>,
}

impl LoadResult {
    pub fn requests(&self) -> u64 {
        self.latencies_ns.len() as u64
    }

    pub fn requests_per_second(&self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds <= 0.0 {
            0.0
        } else {
            self.requests() as f64 / seconds
        }
    }

    /// Latency at the given quantile, e.g. `0.99` for p99.
    pub fn percentile_ms(&self, quantile: f64) -> f64 {
        if self.latencies_ns.is_empty() {
            return 0.0;
        }
        let last = self.latencies_ns.len() - 1;
        let index = ((self.latencies_ns.len() as f64 * quantile) as usize).min(last);
        self.latencies_ns[index] as f64 / 1e6
    }

    pub fn mean_ms(&self) -> f64 {
        if self.latencies_ns.is_empty() {
            return 0.0;
        }
        let total: u128 = self.latencies_ns.iter().map(|&n| u128::from(n)).sum();
        (total as f64 / self.latencies_ns.len() as f64) / 1e6
    }
}

/// Run `spec` and return the merged results.
pub async fn run(spec: &LoadSpec) -> Result<LoadResult> {
    let spec = Arc::new(spec.clone());
    let started = Instant::now();
    let deadline = started + spec.duration;

    let mut connections = Vec::with_capacity(spec.connections);
    for _ in 0..spec.connections.max(1) {
        let spec = Arc::clone(&spec);
        connections.push(tokio::spawn(async move {
            connection_loop(&spec, deadline).await
        }));
    }

    let mut result = LoadResult::default();
    for connection in connections {
        match connection.await {
            Ok(Ok(samples)) => result.latencies_ns.extend(samples),
            // A connection that dies mid-run is a server-side failure, not a
            // harness bug: count it and keep the samples we already have.
            Ok(Err(e)) => {
                eprintln!("httpbench: connection failed: {e}");
                result.errors += 1;
            }
            Err(e) => {
                eprintln!("httpbench: load task panicked: {e}");
                result.errors += 1;
            }
        }
    }
    result.elapsed = started.elapsed();
    result.latencies_ns.sort_unstable();
    Ok(result)
}

async fn connection_loop(spec: &LoadSpec, deadline: Instant) -> Result<Vec<u64>> {
    let stream = TcpStream::connect(spec.authority()).await?;
    // Without this, small responses sit in the kernel waiting for Nagle and
    // the measurement becomes a study of delayed ACKs.
    stream.set_nodelay(true)?;

    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(Box::new)?;
    let pump = tokio::spawn(async move {
        let _ = connection.await;
    });

    let authority = spec.authority();
    let method = if spec.body.is_empty() {
        Method::GET
    } else {
        Method::POST
    };

    let mut latencies = Vec::new();
    while Instant::now() < deadline {
        let request = Request::builder()
            .method(method.clone())
            .uri(&spec.path)
            .header(header::HOST, &authority)
            .body(Full::new(spec.body.clone()))?;

        let sent = Instant::now();
        let response = sender.send_request(request).await.map_err(Box::new)?;
        let status = response.status();
        // Draining the body is part of the request: without it the next
        // request would pipeline behind unread bytes.
        let _ = response.into_body().collect().await.map_err(Box::new)?;
        if !status.is_success() {
            return Err(format!("server replied {status}").into());
        }
        latencies.push(sent.elapsed().as_nanos() as u64);
    }

    drop(sender);
    let _ = pump.await;
    Ok(latencies)
}

/// Poll the server until it answers, so a slow start does not land in the
/// measurement.
pub async fn wait_until_ready(spec: &LoadSpec, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut probe = spec.clone();
    probe.connections = 1;
    probe.duration = Duration::ZERO;
    probe.body = Bytes::new();

    let mut last: Option<String> = None;
    while Instant::now() < deadline {
        match single_request(&probe).await {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(format!(
        "server on port {} did not become ready: {}",
        spec.port,
        last.unwrap_or_else(|| "no response".to_owned())
    )
    .into())
}

/// Send a single request, used both by the readiness probe and to ask a
/// server to shut down.
pub async fn single_request(spec: &LoadSpec) -> Result<()> {
    let stream = TcpStream::connect(spec.authority()).await?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(Box::new)?;
    let pump = tokio::spawn(async move {
        let _ = connection.await;
    });

    let request = Request::builder()
        .method(Method::GET)
        .uri(&spec.path)
        .header(header::HOST, spec.authority())
        .body(Full::new(Bytes::new()))?;
    let response = sender.send_request(request).await.map_err(Box::new)?;
    let status = response.status();
    let _ = response.into_body().collect().await.map_err(Box::new)?;
    drop(sender);
    let _ = pump.await;

    if status.is_success() {
        Ok(())
    } else {
        Err(format!("readiness probe got {status}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result_with(latencies_ns: Vec<u64>, elapsed: Duration) -> LoadResult {
        let mut sorted = latencies_ns;
        sorted.sort_unstable();
        LoadResult {
            elapsed,
            errors: 0,
            latencies_ns: sorted,
        }
    }

    #[test]
    fn percentiles_index_the_sorted_samples() {
        let r = result_with(
            (1..=100).map(|n| n * 1_000_000).collect(),
            Duration::from_secs(1),
        );
        assert!((r.percentile_ms(0.5) - 51.0).abs() < 1e-9);
        assert!((r.percentile_ms(0.99) - 100.0).abs() < 1e-9);
        // The top quantile must never run off the end of the vector.
        assert!((r.percentile_ms(1.0) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn empty_results_report_zero_rather_than_panicking() {
        let r = LoadResult::default();
        assert_eq!(r.requests(), 0);
        assert_eq!(r.requests_per_second(), 0.0);
        assert_eq!(r.percentile_ms(0.5), 0.0);
        assert_eq!(r.mean_ms(), 0.0);
    }

    #[test]
    fn throughput_and_mean_are_arithmetic() {
        let r = result_with(vec![1_000_000, 3_000_000], Duration::from_secs(2));
        assert!((r.requests_per_second() - 1.0).abs() < 1e-9);
        assert!((r.mean_ms() - 2.0).abs() < 1e-9);
    }
}
