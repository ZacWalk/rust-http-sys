//! End-to-end CLI tests that don't require http.sys URL ACLs.
//!
//! These exercise the binary as a subprocess and verify the surface contract
//! (help, version, argument parsing, error paths).

use std::process::Command;

fn bin() -> Command {
    let path = env!("CARGO_BIN_EXE_test-httpsys");
    Command::new(path)
}

#[test]
fn prints_help() {
    let out = bin().arg("--help").output().expect("spawn binary");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("server"));
    assert!(stdout.contains("client"));
    assert!(stdout.contains("echo"));
    assert!(stdout.contains("test"));
}

#[test]
fn prints_version() {
    let out = bin().arg("--version").output().expect("spawn binary");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("test-httpsys"), "got: {stdout}");
}

#[test]
fn rejects_invalid_url() {
    // `not a url` should fail clap's value_parser=is_valid_url.
    let out = bin()
        .args(["client", "::::not a url::::"])
        .output()
        .expect("spawn binary");
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error") || stderr.contains("invalid"),
        "stderr did not look like a clap error: {stderr}"
    );
}

#[test]
fn echo_against_unreachable_host_fails_gracefully() {
    // Port 1 is reserved + unbound; reqwest should fail fast without panicking.
    let out = bin()
        .args(["echo", "http://127.0.0.1:1/"])
        .output()
        .expect("spawn binary");
    // The binary itself exits 0 (it prints "Error: ..." but doesn't propagate),
    // so the contract we assert is "no panic / no crash".
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("panicked at"),
        "binary panicked: {combined}"
    );
}
