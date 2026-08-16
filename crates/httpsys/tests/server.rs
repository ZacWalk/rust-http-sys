//! End-to-end tests against a real http.sys queue.
//!
//! These bind `http://+:80/Temporary_Listen_Addresses/...`, the URL
//! reservation every Windows install grants to `\Everyone`, so they need no
//! elevation and no `netsh` setup. They are still `#[ignore]`d because port 80
//! may already be taken by IIS or another listener on a given machine.
//!
//! ```text
//! cargo test -p httpsys --test server -- --ignored --test-threads=1
//! ```

use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

use httpsys::{Method, Response, ServerBuilder, ServerConfig, status};

const PREFIX: &str = "/Temporary_Listen_Addresses/httpsys-test";

fn url(path: &str) -> String {
    format!("http://+:80{PREFIX}{path}")
}

/// Minimal HTTP/1.1 client: no dependency on the benchmark harness, and it
/// keeps the tests honest about what actually goes over the wire.
fn request(path: &str, method: &str, body: &[u8]) -> std::io::Result<(u16, Vec<u8>)> {
    let mut stream = TcpStream::connect("127.0.0.1:80")?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let head = format!(
        "{method} {PREFIX}{path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response has a header terminator");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("status line has a code");
    Ok((status, raw[split + 4..].to_vec()))
}

/// Covers the whole request lifecycle in one server so the tests do not fight
/// over port 80: routing, methods, inline bodies, streamed bodies and 404.
#[test]
#[ignore = "binds port 80; run with --ignored --test-threads=1"]
fn serves_requests_end_to_end() {
    let mut builder = ServerBuilder::with_config(ServerConfig {
        // Small enough that the 1 MiB case below is forced down the streaming
        // path, and small enough that growth has somewhere to go.
        request_buffer_bytes: 8 * 1024,
        max_request_buffer_bytes: 32 * 1024,
        concurrency: 8,
        ..ServerConfig::default()
    })
    .expect("http.sys init");

    builder
        .route(&url("/echo/"), |request: httpsys::Request| async move {
            let method = request.method();
            let body = request.body().await.expect("read body");
            Response::ok_text(format!("{method:?}:{}", body.len()))
        })
        .expect("reserve /echo/");
    builder
        .route(&url("/digest/"), |request: httpsys::Request| async move {
            let body = request.body().await.expect("read body");
            // Sum the bytes so a truncated or duplicated body is detectable,
            // not just a wrong length.
            let sum: u64 = body.iter().map(|&b| u64::from(b)).sum();
            Response::ok_text(format!("{}:{sum}", body.len()))
        })
        .expect("reserve /digest/");

    let mut server = builder.run();
    std::thread::sleep(Duration::from_millis(200));

    let (status, body) = request("/echo/", "GET", b"").expect("GET /echo/");
    assert_eq!(status, status::OK);
    assert_eq!(body, b"Get:0");

    let payload = vec![b'a'; 1024];
    let (status, body) = request("/echo/", "POST", &payload).expect("small POST");
    assert_eq!(status, status::OK);
    assert_eq!(body, b"Post:1024");

    // Larger than max_request_buffer_bytes, so this exercises the fallback
    // receive plus the streaming body reader.
    let large: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let expected: u64 = large.iter().map(|&b| u64::from(b)).sum();
    let (status, body) = request("/digest/", "POST", &large).expect("large POST");
    assert_eq!(status, status::OK);
    assert_eq!(
        String::from_utf8_lossy(&body),
        format!("{}:{expected}", large.len()),
        "large body must arrive intact"
    );

    // A path under the reservation but outside any registered prefix is
    // rejected by http.sys itself, before it ever reaches a handler.
    let (status, _) = request("/missing/", "GET", b"").expect("unrouted path");
    assert_ne!(status, status::OK);

    server.shutdown();
    server.join();

    // The queue is gone, so nothing should still be answering.
    let after_shutdown = request("/echo/", "GET", b"").map(|(status, _)| status);
    assert!(
        !matches!(after_shutdown, Ok(status::OK)),
        "server kept serving after shutdown: {after_shutdown:?}"
    );
}

/// A slot must survive a request whose headers do not fit, and answer 431
/// rather than wedging the queue.
#[test]
#[ignore = "binds port 80; run with --ignored --test-threads=1"]
fn oversized_headers_get_431_and_the_server_keeps_going() {
    let mut builder = ServerBuilder::with_config(ServerConfig {
        request_buffer_bytes: 4 * 1024,
        max_request_buffer_bytes: 4 * 1024,
        concurrency: 4,
        ..ServerConfig::default()
    })
    .expect("http.sys init");
    builder
        .route(&url("/small/"), |_: httpsys::Request| async {
            Response::ok_text("OK")
        })
        .expect("reserve /small/");

    let mut server = builder.run();
    std::thread::sleep(Duration::from_millis(200));

    let mut stream = TcpStream::connect("127.0.0.1:80").expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let padding = "x".repeat(16 * 1024);
    let head = format!(
        "GET {PREFIX}/small/ HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Padding: {padding}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).expect("write");
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let head = String::from_utf8_lossy(&raw);
    assert!(
        head.contains(" 431 ") || head.contains(" 400 "),
        "expected a rejection, got: {}",
        head.lines().next().unwrap_or_default()
    );

    // The important part: the queue is still usable afterwards.
    let (status, body) = request("/small/", "GET", b"").expect("follow-up request");
    assert_eq!(status, status::OK);
    assert_eq!(body, b"OK");

    server.shutdown();
    server.join();
}

/// `Method` should survive the round trip through http.sys's verb table.
#[test]
#[ignore = "binds port 80; run with --ignored --test-threads=1"]
fn extension_methods_round_trip() {
    let mut builder = ServerBuilder::new().expect("http.sys init");
    builder
        .route(&url("/verb/"), |request: httpsys::Request| async move {
            Response::ok_text(format!("{:?}", request.method()))
        })
        .expect("reserve /verb/");
    let mut server = builder.run();
    std::thread::sleep(Duration::from_millis(200));

    for (verb, expected) in [
        ("GET", format!("{:?}", Method::Get)),
        ("DELETE", format!("{:?}", Method::Delete)),
        ("PATCH", format!("{:?}", Method::Patch)),
    ] {
        let (status, body) = request("/verb/", verb, b"").expect("verb request");
        assert_eq!(status, status::OK, "{verb}");
        assert_eq!(String::from_utf8_lossy(&body), expected, "{verb}");
    }

    server.shutdown();
    server.join();
}
