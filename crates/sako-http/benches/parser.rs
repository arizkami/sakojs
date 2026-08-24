// SPDX-License-Identifier: BSD-3-Clause

use std::hint::black_box;
use std::time::Instant;

use sako_accel::implementation;
use sako_http::{ParserLimits, parse_request_head};

fn main() {
    let iterations = std::env::var("SAKO_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(5_000_000);
    let request = b"GET /plaintext?value=42 HTTP/1.1\r\nHost: localhost\r\nUser-Agent: sako-bench\r\nAccept: */*\r\nConnection: keep-alive\r\n\r\n";
    let limits = ParserLimits::default();

    for _ in 0..100_000 {
        black_box(parse_request_head(black_box(request), limits).unwrap());
    }
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(parse_request_head(black_box(request), limits).unwrap());
    }
    let elapsed = started.elapsed();
    let nanoseconds_per_operation = elapsed.as_nanos() as f64 / iterations as f64;
    println!(
        "{{\"benchmark\":\"http_request_head\",\"implementation\":\"{:?}\",\"iterations\":{iterations},\"elapsed_ns\":{},\"ns_per_operation\":{nanoseconds_per_operation:.2}}}",
        implementation(),
        elapsed.as_nanos()
    );
}
