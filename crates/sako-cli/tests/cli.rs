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
}

#[test]
fn executes_relative_es_modules() {
    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("modules/main.mjs"))
        .arg("argument")
        .output()
        .expect("sako should start");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "modules 42 argument\n"
    );
}

#[test]
fn executes_commonjs_modules() {
    let output = Command::new(env!("CARGO_BIN_EXE_sako"))
        .arg(fixture("commonjs/main.cjs"))
        .output()
        .expect("sako should start");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "42 commonjs package-main true\n"
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
