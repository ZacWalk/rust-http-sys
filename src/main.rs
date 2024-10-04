use clap::{Parser, Subcommand};
use httpsys::{Response, ServerBuilder};
use plotters::prelude::*;
use plotters::style::{BLUE, WHITE};
use rand::distributions::{Alphanumeric, DistString};
use reqwest::blocking::Client;
use reqwest::{Proxy, Url};
use std::collections::BTreeMap;
use std::error::Error;
use std::time::Instant;
use std::{thread, time::Duration};
use util::print_latency;
use util::{measure_latency, run_this_exe_as_server};

mod util;

/// Network latency tester.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Mode,

    /// Validate SSL certificates. If set to false, invalid certificates will be accepted.
    /// This is useful for development or testing with self-signed certificates but is
    /// not recommended for production environments due to security risks.
    #[arg(
        short,
        long,
        default_value_t = false,
        help = "Don't Validate SSL certificates"
    )]
    no_validate_certs: bool,
}

#[derive(Subcommand, Debug, Clone)]
enum Mode {
    /// Starts the HTTP server.
    #[command(alias = "s")]
    Server {
        #[arg(help = "The URL to receive requests on", default_value = "http://localhost:8080", value_parser = is_valid_url)]
        receive_url: Url,
    },
    /// Sends requests to the server and measures latency.
    #[command(alias = "c")]
    Client {
        #[arg(help = "The URL to send requests to", default_value = "http://localhost:8080", value_parser = is_valid_url)]
        send_url: Url,
        #[arg(help = "Optional proxy server URL (example http://localhost:8080)")]
        proxy_url: Option<Url>,
    },
    /// Sends requests to the server and prints the result.
    #[command(alias = "e")]
    Echo {
        #[arg(help = "The URL to send requests to", default_value = "http://localhost:8080", value_parser = is_valid_url)]
        send_url: Url,
        #[arg(help = "Optional proxy server URL (example http://localhost:8080)")]
        proxy_url: Option<Url>,
    },
    /// Starts this app as a server and measures latency.
    #[command(alias = "t")]
    Test,
}

fn is_valid_url(url: &str) -> Result<Url, String> {
    Url::parse(url).map_err(|error| error.to_string())
}

fn main() {
    let args = Args::parse();

    match &args.command {
        Mode::Server { receive_url } => {
            println!("Server running on {receive_url}test/");
            if let Err(e) = run_server_mode(receive_url) {
                eprintln!("Server failed: {e}");
                std::process::exit(1);
            }
        }
        Mode::Client {
            send_url,
            proxy_url,
        } => {
            println!("Client sending to: {send_url}");
            println!("Validate SSL certificates: {}", !args.no_validate_certs);

            let client = build_client(proxy_url, args.no_validate_certs)
                .expect("failed to build HTTP client");
            let average_latency = measure_latency(|| {
                let _ = send_get_request(&client, send_url);
            });

            print_latency(&average_latency);
        }
        Mode::Echo {
            send_url,
            proxy_url,
        } => {
            println!("Client sending to: {send_url}");
            println!("Validate SSL certificates: {}", !args.no_validate_certs);

            let client = build_client(proxy_url, args.no_validate_certs)
                .expect("failed to build HTTP client");
            let start_time = Instant::now();
            let result = send_get_request(&client, send_url);
            let latency = start_time.elapsed();
            let mut response_size = 0;

            println!("============================================================");

            match result {
                Ok(value) => {
                    println!("{value}");
                    response_size = value.len();
                }
                Err(e) => eprintln!("Error: {e}"),
            };

            println!("============================================================");
            println!("Latency: {latency:?}");
            println!("Response Size: {response_size} chars");
        }
        Mode::Test => {
            println!("Test mode");
            let server_exe = run_this_exe_as_server();

            println!("Server process started");
            println!("Calling server multiple times to measure latency");

            thread::sleep(Duration::from_millis(100));

            let send_url = server_exe.format_req_url("/test/");
            let client =
                build_client(&None, args.no_validate_certs).expect("failed to build HTTP client");
            let mut measurements = Vec::<Measurement>::new();
            let mut payload_size = 1024usize; // Initial payload size
            let target_size = 8 * 1024 * 1024; // 8 MB

            while payload_size <= target_size {
                let random_data = generate_random_payload(payload_size);
                let latency_result = measure_latency(|| {
                    let _ = send_post_request(&client, &send_url, &random_data);
                });

                measurements.push(Measurement {
                    name: "Request",
                    latency: latency_result.latency.as_nanos() as u64,
                    payload_size: payload_size as u64,
                });

                println!(
                    "Average latency: {:?} : size {}",
                    latency_result.latency,
                    format_size(payload_size as u64)
                );

                payload_size += payload_size / 4;
            }

            write_plot(
                &measurements,
                "Same Machine HTTP requests to HTTP-SYS",
                "Average MS",
                "request-latency.svg",
            )
            .expect("failed to plot");
        }
    }
}

fn build_client(
    proxy_url: &Option<Url>,
    accept_invalid_certs: bool,
) -> Result<Client, reqwest::Error> {
    let mut builder = Client::builder().danger_accept_invalid_certs(accept_invalid_certs);
    if let Some(proxy_url) = proxy_url {
        builder = builder.proxy(Proxy::http(proxy_url.as_str())?);
    }
    builder.build()
}

fn send_get_request(client: &Client, url: &Url) -> Result<String, Box<dyn std::error::Error>> {
    let res = client
        .get(url.as_str())
        .header("Cache-Control", "no-cache")
        .send()?;
    Ok(res.text()?)
}

fn send_post_request(
    client: &Client,
    url: &Url,
    body: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let res = client
        .post(url.as_str())
        .header("Cache-Control", "no-cache")
        .body(body.to_owned())
        .send()?;
    Ok(res.text()?)
}

fn generate_random_payload(data_size: usize) -> String {
    Alphanumeric.sample_string(&mut rand::thread_rng(), data_size)
}

/// Spawn the demo server with `/test` and `/kill` routes and block until
/// shutdown is signalled (either Ctrl+C or a `/kill` request).
fn run_server_mode(receive_url: &Url) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let kill_flag = std::sync::Arc::new(AtomicBool::new(false));

    let test_url = {
        let mut u = receive_url.clone();
        u.set_path("/test");
        u.to_string()
    };
    let kill_url = {
        let mut u = receive_url.clone();
        u.set_path("/kill");
        u.to_string()
    };

    let mut builder = match ServerBuilder::new() {
        Ok(b) => b,
        Err(e) => {
            return Err(format!("Failed to initialize HTTP server: {e}").into());
        }
    };

    let kill_clone = kill_flag.clone();
    builder
        .route(&test_url, |_req| async { Response::ok_text("OK") })
        .map_err(|e| {
            format!(
                "Failed to bind URL {test_url} (try running elevated, or register a URL ACL with \
                 `netsh http add urlacl url={receive_url}test/ user=Everyone`): {e}"
            )
        })?;
    builder
        .route(&kill_url, move |_req| {
            let kf = kill_clone.clone();
            async move {
                kf.store(true, Ordering::SeqCst);
                Response::ok_text("OK")
            }
        })
        .map_err(|e| format!("Failed to bind {kill_url}: {e}"))?;

    let mut handle = builder.run();

    // Poll the kill flag in this thread; the server runs on its own.
    while !kill_flag.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }
    handle.shutdown();
    handle.join();
    Ok(())
}

fn format_size(size_in_bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;

    if size_in_bytes >= MB {
        format!("{:.1}mb", size_in_bytes as f64 / MB as f64)
    } else if size_in_bytes >= KB {
        format!("{:.1}kb", size_in_bytes as f64 / KB as f64)
    } else {
        format!("{size_in_bytes}b")
    }
}

const FONT: &str = "Fira Code";
const PLOT_WIDTH: u32 = 800;
const PLOT_HEIGHT: u32 = 400;

pub struct Measurement<'a> {
    pub name: &'a str,
    pub latency: u64,
    pub payload_size: u64,
}

pub fn write_plot(
    records: &Vec<Measurement>,
    caption: &str,
    y_label: &str,
    path: &str,
) -> Result<(), Box<dyn Error>> {
    let mut groups: BTreeMap<&str, Vec<&Measurement>> = BTreeMap::new();

    for record in records.iter() {
        let group = groups.entry(record.name).or_default();
        group.push(record);
    }

    let resolution = (PLOT_WIDTH, PLOT_HEIGHT);
    let root = SVGBackend::new(&path, resolution).into_drawing_area();

    root.fill(&WHITE)?;

    let y_min = records.iter().map(|m| m.latency).min().unwrap();
    let y_max = records.iter().map(|m| m.latency).max().unwrap();
    let y_diff = y_max - y_min;
    let y_padding = (y_diff / 10).min(y_min);

    let x_max = records.iter().map(|m| m.payload_size).max().unwrap();

    let mut chart = ChartBuilder::on(&root)
        .margin(10)
        .caption(caption, (FONT, 20))
        .set_label_area_size(LabelAreaPosition::Left, 70)
        .set_label_area_size(LabelAreaPosition::Right, 70)
        .set_label_area_size(LabelAreaPosition::Bottom, 40)
        .build_cartesian_2d(1..x_max, y_min - y_padding..y_max + y_padding)?;

    chart
        .configure_mesh()
        .disable_y_mesh()
        .x_label_formatter(&|v| format_size(*v))
        .y_label_formatter(&|v| format!("{:.1} ms", *v as f64 / 1_000_000.0))
        .x_labels(20)
        .y_labels(20)
        .y_desc(y_label)
        .x_desc("Size")
        .draw()?;

    for records in groups.values() {
        let color = BLUE;
        chart
            .draw_series(LineSeries::new(
                records
                    .iter()
                    .map(|record| (record.payload_size, record.latency)),
                color,
            ))?
            .label(records[0].name)
            .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], color));
    }

    chart
        .configure_series_labels()
        .position(SeriesLabelPosition::UpperLeft)
        .label_font((FONT, 13))
        .background_style(WHITE.mix(0.8))
        .border_style(BLACK)
        .draw()?;

    Ok(())
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::{thread, time::Duration};

    /// Hits a real http.sys-bound URL. Requires either elevation or a
    /// pre-registered URL ACL (`netsh http add urlacl url=http://+:1919/nop/
    /// user=Everyone`). Run explicitly with:
    ///     cargo test -- --ignored
    #[test]
    #[ignore = "requires http.sys URL ACL or elevation"]
    fn test_basic_request() {
        let port_num = 1919;
        let server_url = Url::parse(&format!("http://localhost:{port_num}/nop/")).unwrap();

        let mut builder = ServerBuilder::new().expect("server init");
        builder
            .route(server_url.as_str(), |_req| async {
                Response::ok_text("OK")
            })
            .expect("bind url");
        let mut handle = builder.run();

        thread::sleep(Duration::from_millis(100));

        let client = build_client(&None, false).unwrap();
        let result = send_post_request(&client, &server_url, "xxx").unwrap();
        assert_eq!(result, "OK");

        handle.shutdown();
        handle.join();
    }

    #[test]
    fn format_size_thresholds() {
        assert_eq!(format_size(0), "0b");
        assert_eq!(format_size(512), "512b");
        assert_eq!(format_size(1024), "1.0kb");
        assert_eq!(format_size(1536), "1.5kb");
        assert_eq!(format_size(1024 * 1024), "1.0mb");
        assert_eq!(format_size(8 * 1024 * 1024), "8.0mb");
    }

    #[test]
    fn build_client_no_proxy_succeeds() {
        let c = build_client(&None, false).expect("client builds");
        // Smoke-check the type is usable.
        let _ = c.get("http://127.0.0.1/").build().unwrap();
    }

    #[test]
    fn build_client_with_proxy_succeeds() {
        let proxy = Some(Url::parse("http://localhost:9").unwrap());
        let _ = build_client(&proxy, true).expect("client builds with proxy");
    }

    #[test]
    fn generate_random_payload_size_matches() {
        let p = generate_random_payload(1234);
        assert_eq!(p.len(), 1234);
        assert!(p.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
