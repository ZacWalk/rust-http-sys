//! `httpbench` — a head-to-head benchmark harness for the `httpsys` server in
//! this workspace against `axum`.
//!
//! ```text
//! httpbench bench                       # full sweep, writes SVG charts
//! httpbench serve --engine httpsys      # run one server
//! httpbench load  --port 8080 -c 64     # ad-hoc load against a running server
//! ```

use std::{path::PathBuf, time::Duration};

use bytes::Bytes;
use clap::{Args, Parser, Subcommand};

mod chart;
mod cpu;
mod fmt;
mod load;
mod servers;
mod sweep;

use servers::{BENCH_PATH, Engine};

/// Errors are only ever reported to a human on the console, so one boxed type
/// is enough.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the full sweep against every engine and write charts.
    Bench(BenchArgs),
    /// Run a single server until it is asked to shut down.
    Serve(ServeArgs),
    /// Drive an already-running server and print one set of numbers.
    Load(LoadArgs),
}

#[derive(Args, Debug)]
struct BenchArgs {
    /// One port per engine, in the order httpsys,axum.
    #[arg(long, value_delimiter = ',', default_value = "8080,8081")]
    ports: Vec<u16>,
    /// Path both engines serve on. Must end in `/`.
    #[arg(long, default_value = BENCH_PATH)]
    path: String,
    /// Measurement window per data point.
    #[arg(long, default_value = "3", value_parser = parse_seconds)]
    duration: Duration,
    /// Unmeasured window before each data point.
    #[arg(long, default_value = "1", value_parser = parse_seconds)]
    warmup: Duration,
    /// Concurrency levels to sweep.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "1,2,4,8,16,32,64,128,256"
    )]
    connections: Vec<usize>,
    /// Request body sizes to sweep.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "1KiB,4KiB,16KiB,64KiB,256KiB,1MiB,4MiB,8MiB",
        value_parser = fmt::parse_bytes
    )]
    sizes: Vec<usize>,
    /// Concurrency held constant while sweeping body size.
    #[arg(long, default_value_t = 32)]
    size_sweep_connections: usize,
    /// Body size held constant while sweeping concurrency.
    #[arg(long, default_value = "0", value_parser = fmt::parse_bytes)]
    concurrency_sweep_bytes: usize,
    /// Where the charts and results.csv go.
    #[arg(long, default_value = "charts")]
    out: PathBuf,
    /// Let httpsys serve replies from the kernel response cache. Off by
    /// default: axum has no equivalent, so it is not a like-for-like number.
    #[arg(long)]
    kernel_cache: bool,
}

#[derive(Args, Debug)]
struct ServeArgs {
    #[arg(long, value_enum)]
    engine: Engine,
    #[arg(long, default_value_t = 8080)]
    port: u16,
    /// Path to serve on. Must end in `/`.
    #[arg(long, default_value = BENCH_PATH)]
    path: String,
    /// httpsys receive buffer size. Requests that fit arrive in one syscall.
    #[arg(long, default_value = "16KiB", value_parser = fmt::parse_bytes)]
    request_buffer: usize,
    /// Serve replies from the http.sys kernel cache (httpsys only).
    #[arg(long)]
    kernel_cache: bool,
}

#[derive(Args, Debug)]
struct LoadArgs {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8080)]
    port: u16,
    #[arg(long, default_value = BENCH_PATH)]
    path: String,
    /// Concurrent requests in flight.
    #[arg(short = 'c', long, default_value_t = 32)]
    connections: usize,
    /// Measurement window.
    #[arg(short = 'd', long, default_value = "5", value_parser = parse_seconds)]
    duration: Duration,
    /// Request body size; zero sends GET.
    #[arg(short = 's', long, default_value = "0", value_parser = fmt::parse_bytes)]
    size: usize,
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("httpbench: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Bench(args) => sweep::run(&sweep::SweepOptions {
            ports: args.ports,
            path: args.path,
            duration: args.duration,
            warmup: args.warmup,
            connections: args.connections,
            sizes: args.sizes,
            size_sweep_connections: args.size_sweep_connections,
            concurrency_sweep_bytes: args.concurrency_sweep_bytes,
            out_dir: args.out,
            kernel_cache: args.kernel_cache,
        }),
        Command::Serve(args) => servers::serve(
            args.engine,
            args.port,
            &args.path,
            &servers::ServeOptions {
                request_buffer_bytes: args.request_buffer,
                kernel_cache: args.kernel_cache,
            },
        ),
        Command::Load(args) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(ad_hoc_load(args))
        }
    }
}

async fn ad_hoc_load(args: LoadArgs) -> Result<()> {
    let spec = load::LoadSpec {
        host: args.host,
        port: args.port,
        path: args.path,
        body: Bytes::from(vec![b'x'; args.size]),
        connections: args.connections,
        duration: args.duration,
    };
    load::wait_until_ready(&spec, Duration::from_secs(5)).await?;

    let result = load::run(&spec).await?;
    println!(
        "{} requests in {:.2}s over {} connections",
        result.requests(),
        result.elapsed.as_secs_f64(),
        spec.connections
    );
    println!(
        "  throughput  {} req/s",
        fmt::rate(result.requests_per_second())
    );
    println!("  mean        {}", fmt::millis(result.mean_ms()));
    println!("  p50         {}", fmt::millis(result.percentile_ms(0.50)));
    println!("  p99         {}", fmt::millis(result.percentile_ms(0.99)));
    if result.errors > 0 {
        println!("  errors      {}", result.errors);
    }
    Ok(())
}

fn parse_seconds(text: &str) -> std::result::Result<Duration, String> {
    text.trim()
        .trim_end_matches('s')
        .parse::<f64>()
        .map_err(|_| format!("'{text}' is not a number of seconds"))
        .and_then(|s| {
            if s.is_finite() && s >= 0.0 {
                Ok(Duration::from_secs_f64(s))
            } else {
                Err(format!("'{text}' is not a valid duration"))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn seconds_parse_with_and_without_the_suffix() {
        assert_eq!(parse_seconds("3"), Ok(Duration::from_secs(3)));
        assert_eq!(parse_seconds("2.5s"), Ok(Duration::from_millis(2500)));
        assert!(parse_seconds("-1").is_err());
        assert!(parse_seconds("soon").is_err());
    }

    #[test]
    fn bench_defaults_sweep_both_axes() {
        let cli = Cli::try_parse_from(["httpbench", "bench"]).unwrap();
        let Command::Bench(args) = cli.command else {
            panic!("expected bench");
        };
        assert_eq!(args.connections.first(), Some(&1));
        assert_eq!(args.sizes.last(), Some(&(8 * 1024 * 1024)));
        assert_eq!(args.ports, vec![8080, 8081]);
        assert!(!args.kernel_cache);
    }

    #[test]
    fn serve_requires_an_engine() {
        assert!(Cli::try_parse_from(["httpbench", "serve"]).is_err());
        let cli = Cli::try_parse_from(["httpbench", "serve", "--engine", "axum"]).unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("expected serve");
        };
        assert_eq!(args.engine, Engine::Axum);
    }
}
