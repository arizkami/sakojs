// SPDX-License-Identifier: BSD-3-Clause

use std::path::PathBuf;
use std::process::Command;

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
