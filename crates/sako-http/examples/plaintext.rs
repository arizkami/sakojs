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
    }
}
