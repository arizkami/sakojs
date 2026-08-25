// SPDX-License-Identifier: BSD-3-Clause

use sako_diagnostics::process_stats;
use sako_v8::Runtime;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static LIFECYCLE_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn repeated_isolates_release_handles_and_memory() {
    let _guard = LIFECYCLE_LOCK.lock().unwrap();
    // The process-wide V8 platform intentionally remains alive. Warm it before
    // measuring runtime-local isolate and handle convergence.
    {
        let mut runtime = Runtime::new().expect("V8 should initialize");
        runtime
            .execute("Promise.resolve(1 + 1);", "lifecycle-test.js")
            .expect("JavaScript should execute");
    }
    let baseline = process_stats().unwrap();

    for cycle in 0..20 {
        let mut runtime = Runtime::new().expect("another isolate should initialize");
        runtime
            .execute(
                "const values = Array.from({ length: 10000 }, (_, i) => ({ i }));\nsetTimeout(() => values.length, 0);",
                &format!("lifecycle-{cycle}.js"),
            )
            .expect("JavaScript should execute");
    }
    let after = process_stats().unwrap();
    assert!(
        after.os_handles <= baseline.os_handles + 4,
        "isolate shutdown left process handles open: before={}, after={}",
        baseline.os_handles,
        after.os_handles
    );
    assert!(
        after.private_bytes <= baseline.private_bytes + 64 * 1024 * 1024,
        "runtime-local memory did not converge: before={}, after={}",
        baseline.private_bytes,
        after.private_bytes
    );
}

#[test]
fn multiple_isolates_can_coexist() {
    let _guard = LIFECYCLE_LOCK.lock().unwrap();
    let mut first = Runtime::new().unwrap();
    let mut second = Runtime::new().unwrap();
    first.execute("globalThis.value = 1;", "first.js").unwrap();
    second
        .execute("globalThis.value = 2;", "second.js")
        .unwrap();
}

#[test]
fn repeated_http_servers_release_sockets_and_native_buffers() {
    let _guard = LIFECYCLE_LOCK.lock().unwrap();
    {
        let mut runtime = Runtime::new().unwrap();
        runtime
            .execute(
                "const http = __sakoBuiltins['node:http']; const server = http.createServer((_req, res) => res.end('ok')); server.listen(0, () => server.close());",
                "http-lifecycle-warmup.js",
            )
            .unwrap();
    }
    let baseline = process_stats().unwrap();
    for cycle in 0..20 {
        let mut runtime = Runtime::new().unwrap();
        runtime
            .execute(
                "const http = __sakoBuiltins['node:http']; const server = http.createServer((_req, res) => res.end('ok')); server.listen(0, () => server.close());",
                &format!("http-lifecycle-{cycle}.js"),
            )
            .unwrap();
        let stats = runtime.memory_stats();
        assert_eq!(stats.http_servers, 0);
        assert_eq!(stats.sockets, 0);
        assert_eq!(stats.http_buffer_bytes, 0);
        assert_eq!(stats.queued_operations, 0);
    }
    let after = process_stats().unwrap();
    assert!(
        after.os_handles <= baseline.os_handles + 4,
        "HTTP server cycles leaked handles: before={}, after={}",
        baseline.os_handles,
        after.os_handles
    );
    assert!(
        after.private_bytes <= baseline.private_bytes + 64 * 1024 * 1024,
        "HTTP server cycles did not converge: before={}, after={}",
        baseline.private_bytes,
        after.private_bytes
    );
}

#[test]
fn repeated_spawn_sync_releases_process_handles() {
    let _guard = LIFECYCLE_LOCK.lock().unwrap();
    {
        let mut runtime = Runtime::new().unwrap();
        runtime
            .execute(
                "const [f,a]=process.platform==='win32'?['cmd.exe',['/d','/c','echo warmup']]:['/bin/sh',['-c','echo warmup']];__sakoBuiltins['node:child_process'].spawnSync(f,a);",
                "child-warmup.js",
            )
            .unwrap();
    }
    let baseline = process_stats().unwrap();
    for cycle in 0..20 {
        let mut runtime = Runtime::new().unwrap();
        runtime
            .execute(
                "const [f,a]=process.platform==='win32'?['cmd.exe',['/d','/c','echo child']]:['/bin/sh',['-c','echo child']];const result = __sakoBuiltins['node:child_process'].spawnSync(f,a); if (result.status !== 0) throw new Error('child failed');",
                &format!("child-lifecycle-{cycle}.js"),
            )
            .unwrap();
    }
    let after = process_stats().unwrap();
    assert!(
        after.os_handles <= baseline.os_handles + 4,
        "spawnSync cycles leaked handles: before={}, after={}",
        baseline.os_handles,
        after.os_handles
    );
}

#[test]
fn repeated_isolates_close_abandoned_file_descriptors() {
    let _guard = LIFECYCLE_LOCK.lock().unwrap();
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let hello = workspace.join("tests/hello.js");
    let runtime_fixture = workspace.join("tests/runtime.js");
    {
        let mut runtime = Runtime::new().unwrap();
        runtime
            .execute(
                &format!(
                    "__sakoBuiltins['node:fs'].openSync({:?}, 'r');",
                    hello.to_string_lossy()
                ),
                "descriptor-warmup.js",
            )
            .unwrap();
    }
    let baseline = process_stats().unwrap();
    for cycle in 0..20 {
        let mut runtime = Runtime::new().unwrap();
        runtime
            .execute(
                &format!(
                    "const fs = __sakoBuiltins['node:fs']; fs.openSync({:?}, 'r'); fs.openSync({:?}, 'r');",
                    hello.to_string_lossy(),
                    runtime_fixture.to_string_lossy()
                ),
                &format!("descriptor-lifecycle-{cycle}.js"),
            )
            .unwrap();
    }
    let after = process_stats().unwrap();
    assert!(
        after.os_handles <= baseline.os_handles + 4,
        "descriptor cycles leaked handles: before={}, after={}",
        baseline.os_handles,
        after.os_handles
    );
}

#[test]
fn javascript_https_server_owns_tls_through_teardown() {
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
    use std::sync::Arc;

    let _guard = LIFECYCLE_LOCK.lock().unwrap();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let mut roots = RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let socket = loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(socket) => break socket,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => panic!("HTTPS client failed to connect: {error}"),
            }
        };
        let connection = ClientConnection::new(
            Arc::new(client_config),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let mut stream = StreamOwned::new(connection, socket);
        stream
            .write_all(b"GET /secure HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    });

    let source = format!(
        "const https = __sakoBuiltins['node:https']; const server = https.createServer({{ cert: {:?}, key: {:?} }}, (req, res) => {{ if (!req.socket.encrypted) throw new Error('socket is not encrypted'); res.end('secure-js'); server.close(); }}); server.listen({port});",
        cert.pem(),
        signing_key.serialize_pem()
    );
    let mut runtime = Runtime::new().unwrap();
    runtime.execute(&source, "https-lifecycle.js").unwrap();
    let response = client.join().unwrap();
    assert!(
        response.ends_with(b"secure-js"),
        "unexpected HTTPS response: {:?}",
        String::from_utf8_lossy(&response)
    );
    let stats = runtime.memory_stats();
    assert_eq!(stats.http_servers, 0);
    assert_eq!(stats.sockets, 0);
}

#[test]
#[ignore = "scheduled high-cycle memory convergence test"]
fn scheduled_runtime_memory_soak_converges() {
    let _guard = LIFECYCLE_LOCK.lock().unwrap();
    let cycles = std::env::var("SAKO_SOAK_CYCLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(500)
        .clamp(100, 10_000);
    {
        let mut runtime = Runtime::new().unwrap();
        runtime.execute("1 + 1", "soak-warmup.js").unwrap();
    }
    let baseline = process_stats().unwrap();
    let mut samples = Vec::new();
    let sample_every = (cycles / 10).max(1);
    for cycle in 0..cycles {
        let mut runtime = Runtime::new().unwrap();
        runtime
            .execute(
                "const values = Array.from({ length: 25000 }, (_, index) => ({ index, value: 'memory-soak' })); queueMicrotask(() => values.length);",
                &format!("scheduled-soak-{cycle}.js"),
            )
            .unwrap();
        if (cycle + 1) % sample_every == 0 {
            samples.push(process_stats().unwrap());
        }
    }
    let after = process_stats().unwrap();
    let plateau = samples
        .iter()
        .skip(samples.len() / 2)
        .map(|sample| sample.private_bytes)
        .collect::<Vec<_>>();
    let plateau_minimum = plateau.iter().copied().min().unwrap_or(after.private_bytes);
    let plateau_maximum = plateau.iter().copied().max().unwrap_or(after.private_bytes);
    assert!(
        after.os_handles <= baseline.os_handles + 8,
        "scheduled soak leaked handles: before={}, after={}, samples={samples:?}",
        baseline.os_handles,
        after.os_handles
    );
    assert!(
        after.private_bytes <= baseline.private_bytes + 128 * 1024 * 1024,
        "scheduled soak did not converge: before={}, after={}, samples={samples:?}",
        baseline.private_bytes,
        after.private_bytes
    );
    assert!(
        plateau_maximum <= plateau_minimum + 64 * 1024 * 1024,
        "scheduled soak did not reach a stable plateau: min={plateau_minimum}, max={plateau_maximum}, samples={samples:?}"
    );
}
