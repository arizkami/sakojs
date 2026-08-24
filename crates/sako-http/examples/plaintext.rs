// SPDX-License-Identifier: BSD-3-Clause

//! Serves a fixed plaintext response from the native HTTP stack alone, with no
//! JavaScript runtime attached. Comparing this against `sako run http.mjs`
//! separates transport cost from dispatch cost.

use std::time::Duration;

use sako_http::{HttpResponse, HttpServer, HttpServerConfig};

fn main() -> std::io::Result<()> {
    let port = std::env::args()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    let mut server = HttpServer::bind(("127.0.0.1", port), HttpServerConfig::default())?;
    println!("listening on {}", server.local_addr()?);
    // Reporting the work counters on a deadline turns the steady-state loop
    // into a profile: requests per tick, connections walked per request, and
    // system calls per request.
    let report_after = std::env::args()
        .nth(2)
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| std::time::Instant::now() + Duration::from_secs(seconds));
    loop {
        let handled = server.tick(|_request| HttpResponse {
            status: 200,
            reason: "OK".into(),
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: b"Hello World".to_vec(),
        })?;
        if handled == 0 {
            server.wait(Duration::from_millis(50))?;
        }
        if report_after.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            let counters = server.counters();
            let requests = counters.requests.max(1) as f64;
            eprintln!(
                "requests={} ticks={} idle_ticks={} requests/tick={:.2} visits/request={:.2}                  productive_visits/request={:.2} recv/request={:.2} send/request={:.2}                  completions/request={:.2} completions/dequeue={:.2}",
                counters.requests,
                counters.ticks,
                counters.idle_ticks,
                counters.requests as f64 / counters.ticks.max(1) as f64,
                counters.connection_visits as f64 / requests,
                counters.connection_visits_with_work as f64 / requests,
                counters.receives_submitted as f64 / requests,
                counters.sends_submitted as f64 / requests,
                counters.completions as f64 / requests,
                counters.completions as f64 / counters.completion_dequeues.max(1) as f64,
            );
            eprintln!(
                "recv_inline={} recv_pending={} send_inline={} send_pending={}",
                counters.receives_inline,
                counters.receives_pending,
                counters.sends_inline,
                counters.sends_pending,
            );
            return Ok(());
        }
    }
}
