//! The head-to-head sweep: spawn each server as a child process, drive it,
//! measure it, and turn the results into charts.
//!
//! Two sweeps are run against each engine:
//!
//! * **concurrency** — a tiny request body, with the number of in-flight
//!   requests doubling each step. This is the "many small requests" case.
//! * **size** — a fixed concurrency with the request body doubling each step.
//!   This is the "large request" case, and the one where receiving headers and
//!   body in a single syscall should show up.
//!
//! Each measured point is bracketed by `GetProcessTimes` snapshots of the
//! *server* process, so the CPU figures exclude the load generator.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command},
    time::Duration,
};

use bytes::Bytes;

use crate::{
    Result,
    chart::{AXUM_COLOR, Chart, HTTPSYS_COLOR, Series},
    cpu::{ProcessCpu, cores_between, cpu_between},
    fmt,
    load::{self, LoadSpec},
    servers::{Engine, shutdown_path},
};

const ENGINES: [Engine; 2] = [Engine::Httpsys, Engine::Axum];

/// How the sweep should be run.
#[derive(Debug, Clone)]
pub struct SweepOptions {
    /// One port per engine, in [`ENGINES`] order.
    pub ports: Vec<u16>,
    /// Path both engines serve on.
    pub path: String,
    pub duration: Duration,
    pub warmup: Duration,
    pub connections: Vec<usize>,
    pub sizes: Vec<usize>,
    /// Concurrency held constant while sweeping body size.
    pub size_sweep_connections: usize,
    /// Body size held constant while sweeping concurrency.
    pub concurrency_sweep_bytes: usize,
    pub out_dir: PathBuf,
    pub kernel_cache: bool,
}

impl SweepOptions {
    fn port_for(&self, index: usize) -> Result<u16> {
        self.ports.get(index).copied().ok_or_else(|| {
            format!(
                "expected {} ports, one per engine, got {}",
                ENGINES.len(),
                self.ports.len()
            )
            .into()
        })
    }

    fn spec(&self, port: u16, body: Bytes, connections: usize) -> LoadSpec {
        LoadSpec {
            host: "127.0.0.1".to_owned(),
            port,
            path: self.path.clone(),
            body,
            connections,
            duration: self.duration,
        }
    }
}

/// One measured point.
#[derive(Debug, Clone)]
pub struct Sample {
    pub engine: Engine,
    /// Connections for the concurrency sweep, body bytes for the size sweep.
    pub x: f64,
    pub body_bytes: usize,
    pub connections: usize,
    pub requests: u64,
    pub rps: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub cores: f64,
    pub cpu_us_per_request: f64,
}

impl Sample {
    /// Request-body throughput, the number that matters for the size sweep.
    fn mib_per_second(&self) -> f64 {
        self.rps * self.body_bytes as f64 / (1024.0 * 1024.0)
    }
}

pub fn run(options: &SweepOptions) -> Result<()> {
    fs::create_dir_all(&options.out_dir)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let mut concurrency = Vec::new();
    let mut size = Vec::new();
    for (index, engine) in ENGINES.iter().enumerate() {
        let port = options.port_for(index)?;
        println!(
            "\n=== {engine} on http://127.0.0.1:{port}{} ===",
            options.path
        );
        concurrency.extend(runtime.block_on(concurrency_sweep(*engine, port, options))?);
        size.extend(runtime.block_on(size_sweep(*engine, port, options))?);
    }

    report(&concurrency, &size);
    write_csv(&options.out_dir.join("results.csv"), &concurrency, &size)?;
    write_charts(&options.out_dir, &concurrency, &size)?;
    println!(
        "\nCharts and results.csv written to {}",
        options.out_dir.display()
    );
    Ok(())
}

async fn concurrency_sweep(
    engine: Engine,
    port: u16,
    options: &SweepOptions,
) -> Result<Vec<Sample>> {
    let server = ServerProcess::spawn(engine, port, options).await?;
    let body = payload(options.concurrency_sweep_bytes);
    let mut samples = Vec::new();
    for &connections in &options.connections {
        let spec = options.spec(port, body.clone(), connections);
        let sample = measure(&server, engine, connections as f64, &spec, options).await?;
        println!(
            "  conn {:>4}  {:>9} req/s  p50 {:>9}  p99 {:>9}  cpu {:>5} cores",
            connections,
            fmt::rate(sample.rps),
            fmt::millis(sample.p50_ms),
            fmt::millis(sample.p99_ms),
            fmt::cores(sample.cores),
        );
        samples.push(sample);
    }
    server.stop(options, port).await;
    Ok(samples)
}

async fn size_sweep(engine: Engine, port: u16, options: &SweepOptions) -> Result<Vec<Sample>> {
    let server = ServerProcess::spawn(engine, port, options).await?;
    let mut samples = Vec::new();
    for &size in &options.sizes {
        let spec = options.spec(port, payload(size), options.size_sweep_connections);
        let sample = measure(&server, engine, size as f64, &spec, options).await?;
        println!(
            "  body {:>8}  {:>9} req/s  {:>6} MiB/s  mean {:>9}  cpu {:>5} cores",
            fmt::bytes(size as f64),
            fmt::rate(sample.rps),
            fmt::mib_per_second(sample.mib_per_second()),
            fmt::millis(sample.mean_ms),
            fmt::cores(sample.cores),
        );
        samples.push(sample);
    }
    server.stop(options, port).await;
    Ok(samples)
}

async fn measure(
    server: &ServerProcess,
    engine: Engine,
    x: f64,
    spec: &LoadSpec,
    options: &SweepOptions,
) -> Result<Sample> {
    if !options.warmup.is_zero() {
        let mut warmup = spec.clone();
        warmup.duration = options.warmup;
        let _ = load::run(&warmup).await?;
    }

    let before = server.cpu.snapshot()?;
    let result = load::run(spec).await?;
    let after = server.cpu.snapshot()?;

    let requests = result.requests();
    let cpu_us_per_request = if requests == 0 {
        0.0
    } else {
        cpu_between(before, after).as_secs_f64() * 1e6 / requests as f64
    };

    Ok(Sample {
        engine,
        x,
        body_bytes: spec.body.len(),
        connections: spec.connections,
        requests,
        rps: result.requests_per_second(),
        mean_ms: result.mean_ms(),
        p50_ms: result.percentile_ms(0.50),
        p99_ms: result.percentile_ms(0.99),
        cores: cores_between(before, after),
        cpu_us_per_request,
    })
}

/// A deterministic, incompressible-enough payload. Content does not matter to
/// either server, but keeping it identical across runs does.
fn payload(size: usize) -> Bytes {
    let mut buffer = Vec::with_capacity(size);
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    while buffer.len() < size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        buffer.extend_from_slice(&state.to_le_bytes());
    }
    buffer.truncate(size);
    Bytes::from(buffer)
}

/// A server running as a child process, with a handle for CPU accounting.
struct ServerProcess {
    child: Child,
    cpu: ProcessCpu,
}

impl ServerProcess {
    /// Spawn the server and block until it answers a request.
    async fn spawn(engine: Engine, port: u16, options: &SweepOptions) -> Result<Self> {
        let exe = std::env::current_exe()?;
        let mut command = Command::new(exe);
        command
            .arg("serve")
            .arg("--engine")
            .arg(engine.label())
            .arg("--port")
            .arg(port.to_string())
            .arg("--path")
            .arg(&options.path);
        if options.kernel_cache && engine == Engine::Httpsys {
            command.arg("--kernel-cache");
        }
        let child = command.spawn()?;
        let cpu = ProcessCpu::open(child.id())?;
        let server = Self { child, cpu };
        wait_ready(options, port).await?;
        Ok(server)
    }

    async fn stop(mut self, options: &SweepOptions, port: u16) {
        let mut spec = options.spec(port, Bytes::new(), 1);
        spec.path = shutdown_path(&options.path);
        spec.duration = Duration::ZERO;
        let _ = load::single_request(&spec).await;
        for _ in 0..40 {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        // Only reached when the sweep bailed out early; `stop` consumes self.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Wait for a freshly spawned server to start answering.
async fn wait_ready(options: &SweepOptions, port: u16) -> Result<()> {
    let spec = options.spec(port, Bytes::new(), 1);
    load::wait_until_ready(&spec, Duration::from_secs(20)).await
}

fn series_for(samples: &[Sample], engine: Engine, value: fn(&Sample) -> f64) -> Vec<(f64, f64)> {
    let mut points: Vec<(f64, f64)> = samples
        .iter()
        .filter(|s| s.engine == engine)
        .map(|s| (s.x, value(s)))
        .collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    points
}

fn engine_series(
    samples: &[Sample],
    value: fn(&Sample) -> f64,
    suffix: &str,
    dashed: bool,
) -> Vec<Series> {
    ENGINES
        .iter()
        .map(|&engine| Series {
            name: if suffix.is_empty() {
                engine.label().to_owned()
            } else {
                format!("{engine} {suffix}")
            },
            color: color_of(engine),
            dashed,
            points: series_for(samples, engine, value),
        })
        .collect()
}

fn color_of(engine: Engine) -> plotters::style::RGBColor {
    match engine {
        Engine::Httpsys => HTTPSYS_COLOR,
        Engine::Axum => AXUM_COLOR,
    }
}

fn write_charts(dir: &Path, concurrency: &[Sample], size: &[Sample]) -> Result<()> {
    let machine = machine_summary();

    let mut latency_series = engine_series(concurrency, |s| s.p50_ms, "p50", false);
    latency_series.extend(engine_series(concurrency, |s| s.p99_ms, "p99", true));
    Chart {
        caption: "Latency vs concurrent requests",
        subtitle: &format!("solid = median, dashed = p99. {machine}"),
        x_desc: "Concurrent requests",
        y_desc: "Latency",
        y_log: true,
        x_format: fmt::count,
        y_format: fmt::millis,
        series: latency_series,
    }
    .render(&dir.join("latency-vs-concurrency.svg"))?;

    Chart {
        caption: "Server CPU vs concurrent requests",
        subtitle: &format!("CPU cores burned by the server process alone. {machine}"),
        x_desc: "Concurrent requests",
        y_desc: "CPU cores in use",
        y_log: false,
        x_format: fmt::count,
        y_format: fmt::cores,
        series: engine_series(concurrency, |s| s.cores, "", false),
    }
    .render(&dir.join("cpu-vs-concurrency.svg"))?;

    Chart {
        caption: "Throughput vs concurrent requests",
        subtitle: &machine,
        x_desc: "Concurrent requests",
        y_desc: "Requests per second",
        y_log: false,
        x_format: fmt::count,
        y_format: fmt::rate,
        series: engine_series(concurrency, |s| s.rps, "", false),
    }
    .render(&dir.join("throughput-vs-concurrency.svg"))?;

    let mut size_latency = engine_series(size, |s| s.mean_ms, "mean", false);
    size_latency.extend(engine_series(size, |s| s.p99_ms, "p99", true));
    Chart {
        caption: "Latency vs request size",
        subtitle: &format!("solid = mean, dashed = p99. {machine}"),
        x_desc: "Request body size",
        y_desc: "Latency",
        y_log: true,
        x_format: fmt::bytes,
        y_format: fmt::millis,
        series: size_latency,
    }
    .render(&dir.join("latency-vs-size.svg"))?;

    Chart {
        caption: "Server CPU vs request size",
        subtitle: &format!("CPU cores burned by the server process alone. {machine}"),
        x_desc: "Request body size",
        y_desc: "CPU cores in use",
        y_log: false,
        x_format: fmt::bytes,
        y_format: fmt::cores,
        series: engine_series(size, |s| s.cores, "", false),
    }
    .render(&dir.join("cpu-vs-size.svg"))?;

    Chart {
        caption: "Ingest throughput vs request size",
        subtitle: &machine,
        x_desc: "Request body size",
        y_desc: "MiB/s received",
        y_log: false,
        x_format: fmt::bytes,
        y_format: fmt::mib_per_second,
        series: engine_series(size, Sample::mib_per_second, "", false),
    }
    .render(&dir.join("throughput-vs-size.svg"))?;

    Ok(())
}

fn write_csv(path: &Path, concurrency: &[Sample], size: &[Sample]) -> Result<()> {
    let mut out = String::from(
        "sweep,engine,connections,body_bytes,requests,rps,mean_ms,p50_ms,p99_ms,cpu_cores,cpu_us_per_request\n",
    );
    for (sweep, samples) in [("concurrency", concurrency), ("size", size)] {
        for s in samples {
            writeln!(
                out,
                "{sweep},{},{},{},{},{:.1},{:.4},{:.4},{:.4},{:.4},{:.2}",
                s.engine,
                s.connections,
                s.body_bytes,
                s.requests,
                s.rps,
                s.mean_ms,
                s.p50_ms,
                s.p99_ms,
                s.cores,
                s.cpu_us_per_request
            )?;
        }
    }
    fs::write(path, out)?;
    Ok(())
}

fn report(concurrency: &[Sample], size: &[Sample]) {
    println!("\n=== Summary ===");
    compare(
        "Peak throughput (small requests)",
        best(concurrency, |s| s.rps),
        |s| format!("{} req/s @ {} conns", fmt::rate(s.rps), s.connections),
        |a, b| a.rps / b.rps,
    );
    compare(
        "Peak ingest throughput (large requests)",
        best(size, Sample::mib_per_second),
        |s| {
            format!(
                "{} MiB/s @ {} body",
                fmt::mib_per_second(s.mib_per_second()),
                fmt::bytes(s.body_bytes as f64)
            )
        },
        |a, b| a.mib_per_second() / b.mib_per_second(),
    );
    compare(
        "CPU per request at peak load",
        best(concurrency, |s| s.rps),
        |s| format!("{:.1} us/req", s.cpu_us_per_request),
        // Lower is better here, so invert.
        |a, b| b.cpu_us_per_request / a.cpu_us_per_request.max(f64::MIN_POSITIVE),
    );
    compare(
        "CPU cost of the largest uploads",
        best(size, |s| s.body_bytes as f64),
        |s| {
            format!(
                "{:.2} cores for {} MiB/s at {} body",
                s.cores,
                fmt::mib_per_second(s.mib_per_second()),
                fmt::bytes(s.body_bytes as f64)
            )
        },
        // Cores per MiB/s: lower is better, so invert.
        |a, b| cores_per_mib(b) / cores_per_mib(a).max(f64::MIN_POSITIVE),
    );
}

fn cores_per_mib(sample: &Sample) -> f64 {
    sample.cores / sample.mib_per_second().max(f64::MIN_POSITIVE)
}

/// Best sample per engine by some metric.
fn best(samples: &[Sample], metric: fn(&Sample) -> f64) -> Vec<Sample> {
    ENGINES
        .iter()
        .filter_map(|&engine| {
            samples
                .iter()
                .filter(|s| s.engine == engine)
                .max_by(|a, b| metric(a).total_cmp(&metric(b)))
                .cloned()
        })
        .collect()
}

fn compare(
    title: &str,
    samples: Vec<Sample>,
    describe: impl Fn(&Sample) -> String,
    ratio: impl Fn(&Sample, &Sample) -> f64,
) {
    println!("\n{title}");
    for sample in &samples {
        println!("  {:<8} {}", sample.engine.label(), describe(sample));
    }
    if let ([httpsys], [axum]) = (&samples[..1], &samples[1..]) {
        let factor = ratio(httpsys, axum);
        let verdict = if factor >= 1.0 { "faster" } else { "slower" };
        println!("  -> httpsys is {factor:.2}x {verdict} than axum");
    }
}

fn machine_summary() -> String {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    format!(
        "{cores} logical cores, loopback, {}",
        std::env::consts::ARCH
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(engine: Engine, x: f64, rps: f64) -> Sample {
        Sample {
            engine,
            x,
            body_bytes: 1024,
            connections: x as usize,
            requests: 100,
            rps,
            mean_ms: 1.0,
            p50_ms: 1.0,
            p99_ms: 2.0,
            cores: 0.5,
            cpu_us_per_request: 10.0,
        }
    }

    #[test]
    fn payload_has_the_requested_length() {
        for size in [0usize, 1, 7, 8, 4096, 4097] {
            assert_eq!(payload(size).len(), size);
        }
    }

    #[test]
    fn payload_is_deterministic() {
        assert_eq!(payload(64), payload(64));
    }

    #[test]
    fn series_are_sorted_and_filtered_by_engine() {
        let samples = vec![
            sample(Engine::Httpsys, 8.0, 80.0),
            sample(Engine::Axum, 4.0, 40.0),
            sample(Engine::Httpsys, 2.0, 20.0),
        ];
        let points = series_for(&samples, Engine::Httpsys, |s| s.rps);
        assert_eq!(points, vec![(2.0, 20.0), (8.0, 80.0)]);
    }

    #[test]
    fn best_picks_the_maximum_per_engine() {
        let samples = vec![
            sample(Engine::Httpsys, 1.0, 10.0),
            sample(Engine::Httpsys, 2.0, 90.0),
            sample(Engine::Axum, 1.0, 50.0),
        ];
        let best = best(&samples, |s| s.rps);
        assert_eq!(best.len(), 2);
        assert_eq!(best[0].rps, 90.0);
        assert_eq!(best[1].rps, 50.0);
    }

    #[test]
    fn ingest_throughput_scales_with_body_size() {
        let mut s = sample(Engine::Httpsys, 1.0, 1024.0);
        s.body_bytes = 1024 * 1024;
        assert!((s.mib_per_second() - 1024.0).abs() < 1e-9);
    }
}
