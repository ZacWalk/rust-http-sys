//! Microbenchmarks for the non-network helpers that sit on the hot path of
//! `Mode::Test` (payload generation and the latency-stability statistics).
//!
//! Run with: `cargo bench`
//!
//! The actual server throughput / latency cannot be benched in-process because
//! binding an `http.sys` URL requires admin or a pre-registered URL ACL. See
//! the README for a recipe driving the running server with `wrk` /
//! `bombardier`.

use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rand::distributions::{Alphanumeric, DistString};

// Bring the helpers into the bench binary without needing a `lib.rs`.
// We only use a subset, so silence dead-code warnings for the rest.
#[path = "../src/util.rs"]
#[allow(dead_code, unused_imports)]
mod util;

fn bench_generate_payload(c: &mut Criterion) {
    let mut group = c.benchmark_group("generate_random_payload");
    for &size in &[1024usize, 64 * 1024, 1024 * 1024, 8 * 1024 * 1024] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                let s = Alphanumeric.sample_string(&mut rand::thread_rng(), black_box(size));
                black_box(s);
            });
        });
    }
    group.finish();
}

fn bench_measure_latency_overhead(c: &mut Criterion) {
    // Measures the per-call overhead of `util::measure_latency` itself with a
    // trivial workload, so regressions in the stats logic show up.
    c.bench_function("measure_latency/noop", |b| {
        b.iter(|| {
            let m = util::measure_latency(|| {
                black_box(0u64.wrapping_add(1));
            });
            black_box(m.latency);
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3));
    targets = bench_generate_payload, bench_measure_latency_overhead
}
criterion_main!(benches);
