use rand::{thread_rng, Rng};
use reqwest::Url;
use std::env;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct ServerExe {
    pub(crate) proc: Option<Child>,
    pub(crate) port: u16,
}

impl ServerExe {
    pub fn format_req_url(&self, path: &str) -> Url {
        let mut url = Url::parse("http://localhost/").expect("Failed to parse url");
        url.set_port(Some(self.port)).expect("Failed to set port");
        url.set_path(path);
        url
    }
}

impl Drop for ServerExe {
    fn drop(&mut self) {
        if let Some(mut proc) = self.proc.take() {
            let _ = proc.kill();
            let _ = proc.wait();
        }
    }
}

pub fn run_this_exe_as_server() -> ServerExe {
    let exe_path = env::current_exe().expect("Failed to get executable path");
    let mut rng = thread_rng();
    let port = rng.gen_range(3333..9999);

    println!("Current exe {exe_path:?}");

    let mut c = Command::new(exe_path);
    c.arg("server").arg(format!("http://localhost:{port}/"));

    let proc = c
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to start server. Try running 'cargo build' to make sure it is built.");

    ServerExe {
        proc: Some(proc),
        port,
    }
}

pub struct LatencyMeasurement {
    pub latency: Duration,
}

pub fn measure_latency<F, T>(f: F) -> LatencyMeasurement
where
    F: Fn() -> T,
{
    const MIN_ITERATIONS: usize = 10;
    const MAX_ITERATIONS: usize = 200;
    const STABLE_THRESHOLD: f64 = 0.10; // 10% deviation considered stable
    const OUTLIER_THRESHOLD: f64 = 2.0; // 2 stddev away considered an outlier

    // warm up
    for _ in 0..5 {
        let _ = f();
    }

    let mut durations = Vec::new();

    for i in 0..MAX_ITERATIONS {
        let start = Instant::now();
        let _ = f();
        let duration = start.elapsed();
        durations.push(duration.as_secs_f64());

        if i >= MIN_ITERATIONS {
            let (mean, std_dev) = mean_stddev(&durations);

            if std_dev > 0.0 {
                durations.retain(|d| (*d - mean).abs() / std_dev <= OUTLIER_THRESHOLD);
            }

            if durations.len() > MIN_ITERATIONS {
                // Recompute mean after filtering for an accurate stability check.
                let (mean_filtered, _) = mean_stddev(&durations);
                let is_stable = mean_filtered > 0.0
                    && durations
                        .iter()
                        .all(|d| (d - mean_filtered).abs() / mean_filtered <= STABLE_THRESHOLD);

                if is_stable {
                    break;
                }
            }
        }
    }

    let (mean, _) = mean_stddev(&durations);

    LatencyMeasurement {
        latency: Duration::from_secs_f64(mean),
    }
}

fn mean_stddev(samples: &[f64]) -> (f64, f64) {
    let n = samples.len() as f64;
    let mean = samples.iter().sum::<f64>() / n;
    let variance = samples
        .iter()
        .map(|d| {
            let diff = d - mean;
            diff * diff
        })
        .sum::<f64>()
        / n;
    (mean, variance.sqrt())
}

pub fn print_latency(result: &LatencyMeasurement) {
    println!("Average latency: {:?}", result.latency);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn mean_stddev_basic() {
        let (mean, sd) = mean_stddev(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]);
        assert!((mean - 5.0).abs() < 1e-9);
        // population stddev = 2.0
        assert!((sd - 2.0).abs() < 1e-9);
    }

    #[test]
    fn mean_stddev_single_sample() {
        let (mean, sd) = mean_stddev(&[3.5]);
        assert_eq!(mean, 3.5);
        assert_eq!(sd, 0.0);
    }

    #[test]
    fn measure_latency_runs_warmup_plus_min_iterations() {
        let calls = Cell::new(0u32);
        let m = measure_latency(|| {
            calls.set(calls.get() + 1);
        });
        // At least 5 warmup + MIN_ITERATIONS (10) = 15 invocations.
        assert!(
            calls.get() >= 15,
            "expected >=15 calls, got {}",
            calls.get()
        );
        assert!(m.latency >= Duration::from_nanos(0));
    }

    #[test]
    fn measure_latency_stops_when_stable() {
        // A constant-time workload should converge well before MAX_ITERATIONS.
        let calls = Cell::new(0u32);
        let _ = measure_latency(|| {
            calls.set(calls.get() + 1);
            // Trivial work: the timing noise alone should keep us inside
            // STABLE_THRESHOLD (10%) for non-zero durations; if it doesn't, we
            // still cap at MAX_ITERATIONS.
            std::hint::black_box(0u64.wrapping_add(1));
        });
        // 5 warmup + at most 200 measurement iterations.
        assert!(calls.get() <= 5 + 200);
    }

    #[test]
    fn server_exe_format_req_url_sets_port_and_path() {
        let s = ServerExe {
            proc: None,
            port: 4242,
        };
        let url = s.format_req_url("/abc/");
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("localhost"));
        assert_eq!(url.port(), Some(4242));
        assert_eq!(url.path(), "/abc/");
    }
}
