// SPDX-License-Identifier: BSD-3-Clause

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join(name)
}

fn copy_tree(source: &std::path::Path, destination: &std::path::Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn module_project(name: &str) -> (PathBuf, PathBuf) {
    let sandbox = std::env::temp_dir().join(format!("sako-{name}-{}", std::process::id()));
    let root = sandbox.join(name);
    let _ = std::fs::remove_dir_all(&sandbox);
    copy_tree(&fixture(name), &root);
    copy_tree(&fixture("fixtures/packages"), &root.join("node_modules"));
    let entry = root.join(if name == "modules" {
        "main.mjs"
    } else {
        "main.cjs"
    });
    (sandbox, entry)
}

#[test]
fn executes_javascript_file() {
    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("hello.js"))
        .output()
        .expect("sako should start");

    assert!(output.status.success(), "status: {}", output.status);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Hello from Sako.js\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn reports_javascript_syntax_errors() {
    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("syntax-error.js"))
        .output()
        .expect("sako should start");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(stderr.contains("SyntaxError"), "stderr: {stderr}");
    assert!(stderr.contains("syntax-error.js"), "stderr: {stderr}");
}

#[test]
fn supports_runtime_commands_and_scheduling() {
    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .args(["run", fixture("runtime.js").to_str().unwrap(), "argument"])
        .output()
        .expect("sako should start");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("win32 x64 argument\n"), "stdout: {stdout}");
    assert!(stdout.contains("microtask\n"), "stdout: {stdout}");
    assert!(stdout.contains("timeout\n"), "stdout: {stdout}");
    assert!(stdout.contains("interval-1\n"), "stdout: {stdout}");
    assert!(stdout.contains("interval-2\n"), "stdout: {stdout}");
    assert!(!stdout.contains("cancelled"), "stdout: {stdout}");
}

#[test]
fn supports_eval_version_repl_and_memory_stats() {
    let eval = Command::new(env!("CARGO_BIN_EXE_sako"))
        .args(["eval", "console.log(6 * 7)"])
        .output()
        .expect("sako eval should start");
    assert!(eval.status.success());
    assert_eq!(String::from_utf8_lossy(&eval.stdout), "42\n");

    let version = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg("--version")
        .output()
        .expect("sako --version should start");
    assert!(version.status.success());
    assert_eq!(String::from_utf8_lossy(&version.stdout), "sako 0.1.0\n");

    let mut repl = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("sako repl should start");
    use std::io::Write as _;
    repl.stdin
        .take()
        .unwrap()
        .write_all(b"let answer = 40;\nconsole.log(answer + 2);\n.exit\n")
        .unwrap();
    let repl = repl.wait_with_output().unwrap();
    assert!(repl.status.success());
    assert!(String::from_utf8_lossy(&repl.stdout).contains("42\n"));

    let stats = Command::new(env!("CARGO_BIN_EXE_sako"))
        .args(["--memory-stats", "eval", "1 + 1"])
        .output()
        .expect("sako memory diagnostics should start");
    assert!(stats.status.success());
    let stdout = String::from_utf8_lossy(&stats.stdout);
    assert!(stdout.contains("V8 heap used"), "stdout: {stdout}");
    assert!(stdout.contains("Persistent handles  1"), "stdout: {stdout}");
    assert!(stdout.contains("Timers              0"), "stdout: {stdout}");
    assert!(stdout.contains("Queued operations   0"), "stdout: {stdout}");

    let leak_check = Command::new(env!("CARGO_BIN_EXE_sako"))
        .args(["--detect-leaks", "eval", "queueMicrotask(() => 42)"])
        .output()
        .expect("sako leak diagnostics should start");
    assert!(
        leak_check.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&leak_check.stderr)
    );
    assert!(String::from_utf8_lossy(&leak_check.stdout).contains("Sako leak check    clean"));

    let module_output =
        std::env::temp_dir().join(format!("sako-leak-module-{}.txt", std::process::id()));
    let module_leak_check = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg("--detect-leaks")
        .arg(fixture("node-core.mjs"))
        .arg(&module_output)
        .output()
        .expect("sako module leak diagnostics should start");
    let _ = std::fs::remove_file(module_output);
    assert!(
        module_leak_check.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&module_leak_check.stderr)
    );
}

#[test]
fn executes_relative_es_modules() {
    let (root, entry) = module_project("modules");
    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(entry)
        .arg("argument")
        .output()
        .expect("sako should start");
    std::fs::remove_dir_all(root).unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "modules 42 import-target wildcard-target commonjs-namespace commonjs-namespace argument dynamic-import file.js commonjs-namespace true import-map-condition\n"
    );
}

#[test]
fn executes_commonjs_modules() {
    let (root, entry) = module_project("commonjs");
    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(entry)
        .output()
        .expect("sako should start");
    std::fs::remove_dir_all(root).unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "42 commonjs package-main require-target true\n"
    );
}

#[test]
fn executes_commonjs_from_a_unicode_path() {
    let root = std::env::temp_dir().join(format!("sako-\u{6d4b}\u{8bd5}-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("value.js"), "module.exports = 42;\n").unwrap();
    std::fs::write(root.join("main.cjs"), "console.log(require('./value'));\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(root.join("main.cjs"))
        .output()
        .expect("sako should start");
    std::fs::remove_dir_all(root).unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n");
}

#[test]
fn executes_typescript_across_module_modes_and_imports() {
    let esm = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("typescript/main.ts"))
        .output()
        .expect("Sako TypeScript ESM should start");
    assert!(
        esm.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&esm.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&esm.stdout),
        "typescript Sako fast 42 42 42 dynamic output\n"
    );

    let mts = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("typescript/entry.mts"))
        .output()
        .expect("Sako .mts should start");
    assert!(mts.status.success());
    assert_eq!(String::from_utf8_lossy(&mts.stdout), "mts\n");

    let cts = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("typescript-commonjs.cts"))
        .output()
        .expect("Sako .cts should start");
    assert!(cts.status.success());
    assert_eq!(String::from_utf8_lossy(&cts.stdout), "cts\n");

    let commonjs = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("typescript-commonjs/main.ts"))
        .output()
        .expect("Sako CommonJS-scoped .ts should start");
    assert!(
        commonjs.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&commonjs.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&commonjs.stdout),
        "typescript-commonjs\n"
    );
}

#[test]
fn reports_typescript_syntax_errors_with_source_locations() {
    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("typescript-broken.ts"))
        .output()
        .expect("Sako invalid TypeScript should start");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("typescript-broken.ts"), "stderr: {stderr}");
    assert!(stderr.contains("3:"), "stderr: {stderr}");
}

#[test]
fn supports_initial_node_modules_and_web_globals() {
    let output_path =
        std::env::temp_dir().join(format!("sako-node-core-{}.txt", std::process::id()));
    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("node-core.mjs"))
        .arg(&output_path)
        .output()
        .expect("sako should start");
    let _ = std::fs::remove_file(output_path);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "node-core 6 42\n");
}

#[test]
fn reads_large_files_byte_for_byte() {
    let directory =
        std::env::temp_dir().join(format!("sako-filesystem-large-{}", std::process::id()));
    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("filesystem-large.mjs"))
        .arg(&directory)
        .output()
        .expect("sako should start");
    let _ = std::fs::remove_dir_all(&directory);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "filesystem-large ok
"
    );
}

#[test]
fn fetches_bounded_http_responses_through_the_native_bridge() {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (request_tx, request_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut headers = Vec::new();
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.trim_end().split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse::<usize>().unwrap();
                }
                headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        request_tx.send((request_line, headers, body)).unwrap();

        let response_body = br#"{"ok":true}"#;
        write!(
            stream,
            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nX-Fetch: yes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response_body.len()
        )
        .unwrap();
        stream.write_all(response_body).unwrap();
    });

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let script = std::env::temp_dir().join(format!("sako-fetch-{nonce}.mjs"));
    std::fs::write(
        &script,
        format!(
            r#"const controller = new AbortController();
controller.abort();
let abortName = "missing";
try {{ await fetch("http://127.0.0.1:{port}/aborted", {{ signal: controller.signal }}); }}
catch (error) {{ abortName = error.name; }}
const response = await fetch("http://127.0.0.1:{port}/native?value=1", {{
  method: "POST",
  headers: {{ "x-request": "sako" }},
  body: "payload",
}});
const cloned = response.clone();
const value = await response.json();
const bytes = new Uint8Array(await cloned.arrayBuffer());
console.log(response.status, response.ok, response.statusText, response.headers.get("x-fetch"), value.ok, bytes.length, abortName);
"#
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(&script)
        .output()
        .expect("Sako fetch fixture should start");
    let _ = std::fs::remove_file(script);
    server.join().unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "201 true Created yes true 11 AbortError\n"
    );
    let (request_line, headers, body) = request_rx.recv().unwrap();
    assert_eq!(request_line, "POST /native?value=1 HTTP/1.1\r\n");
    assert!(headers.contains(&("x-request".into(), "sako".into())));
    assert_eq!(body, b"payload");
}

#[test]
fn respects_windows_file_sharing_violations() {
    use std::os::windows::fs::OpenOptionsExt as _;

    let path = std::env::temp_dir().join(format!("sako-lock-test-{}.txt", std::process::id()));
    std::fs::write(&path, "locked").unwrap();
    let exclusive = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("file-lock.mjs"))
        .arg(&path)
        .output()
        .expect("sako lock fixture should start");
    drop(exclusive);
    let _ = std::fs::remove_file(path);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "locked\n");
}

#[test]
fn serves_http_through_the_native_runtime_bridge() {
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let mut child = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("http-server.mjs"))
        .arg(port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Sako HTTP fixture should start");
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(ready, "http-ready\n");

    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client
        .write_all(
            b"POST /native HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody",
        )
        .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.ends_with("POST /native body"), "{response}");

    assert!(child.wait().unwrap().success());
}

#[test]
fn runs_explicit_worker_local_isolates() {
    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg("--workers=2")
        .arg(fixture("worker.js"))
        .output()
        .expect("sako workers should start");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| *line == "worker-ready")
            .count(),
        2
    );
}

#[test]
fn dispatches_package_scripts() {
    let root = std::env::temp_dir().join(format!("sako-script-test-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"scripts":{"verify":"echo package-script"}}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .args(["run", "verify"])
        .current_dir(&root)
        .output()
        .expect("sako should start");
    std::fs::remove_dir_all(root).unwrap();

    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("package-script"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn accepts_explicit_package_configuration() {
    let root = std::env::temp_dir().join(format!("sako-config-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("package.json"), r#"{"private":true}"#).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .args([
            "install",
            "--ignore-scripts",
            "--registry=https://registry.example/npm",
            "--token",
            "fixture-token",
            "--proxy=http://127.0.0.1:9",
        ])
        .current_dir(&root)
        .output()
        .expect("sako install should parse package options");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.join("sako.lock").is_file());

    let missing = Command::new(env!("CARGO_BIN_EXE_sako"))
        .args(["install", "--registry"])
        .current_dir(&root)
        .output()
        .expect("sako install should reject a missing option value");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--registry needs a value"));
    std::fs::remove_dir_all(root).unwrap();
}
