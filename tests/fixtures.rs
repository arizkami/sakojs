//! Runs each `tests/fixtures/<name>.js` with the built `sako` binary and
//! compares stdout with `<name>.out`, plus the exit code and a required
//! stderr snippet. Every fixture runs twice against a fresh code-cache
//! directory (cold, then warm) so the V8 code cache path is covered too.
//!
//! Regenerate expectations with `SAKO_UPDATE_EXPECT=1 cargo test`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

fn fixtures_dir() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

fn tmp_dir(name: &str) -> PathBuf {
  let dir = std::env::temp_dir().join(format!("sako-test-{}-{name}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir
}

fn run_fixture(name: &str, expect_code: i32, expect_stderr: &str) {
  let dir = fixtures_dir();
  let script = ["js", "ts"]
    .iter()
    .map(|ext| dir.join(format!("{name}.{ext}")))
    .find(|p| p.exists())
    .unwrap_or_else(|| panic!("no fixture {name}.js or {name}.ts"));
  let expected_path = dir.join(format!("{name}.out"));
  let tmp = tmp_dir(name);
  let cache = tmp.join("cache");

  for pass in ["cold cache", "warm cache"] {
    let out = Command::new(env!("CARGO_BIN_EXE_sako"))
      .arg(&script)
      .current_dir(&dir)
      .env("SAKO_CACHE_DIR", &cache)
      .env("SAKO_TEST_TMP", &tmp)
      .env("SAKO_TEST_SERVER", test_server())
      .env_remove("SAKO_NO_CACHE")
      .output()
      .expect("run sako");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    if std::env::var_os("SAKO_UPDATE_EXPECT").is_some() && pass == "cold cache" {
      std::fs::write(&expected_path, stdout.as_bytes()).unwrap();
    }
    let expected = std::fs::read_to_string(&expected_path)
      .unwrap_or_else(|_| panic!("missing {} (run with SAKO_UPDATE_EXPECT=1)", expected_path.display()));

    assert_eq!(stdout, expected, "{name} ({pass}): stdout mismatch\nstderr:\n{stderr}");
    assert_eq!(out.status.code(), Some(expect_code), "{name} ({pass}): exit code\nstderr:\n{stderr}");
    assert!(
      stderr.contains(expect_stderr),
      "{name} ({pass}): stderr should contain {expect_stderr:?}, got:\n{stderr}"
    );
  }
  if expect_code == 0 {
    assert!(cache.exists(), "{name}: code cache directory was not created");
  }
  let _ = std::fs::remove_dir_all(&tmp);
}

macro_rules! fixtures {
  ($($name:ident => ($code:expr, $stderr:expr)),* $(,)?) => {$(
    #[test]
    fn $name() {
      run_fixture(stringify!($name), $code, $stderr);
    }
  )*};
}

fixtures! {
  console => (0, "Assertion failed: shown on stderr"),
  modules => (0, ""),
  timers => (0, ""),
  encoding => (0, ""),
  url => (0, ""),
  fs => (0, ""),
  events => (0, ""),
  fetch => (0, ""),
  serve => (0, ""),
  shell => (0, ""),
  typescript => (0, ""),
  throw => (1, "error: Uncaught Error: boom\n    at inner ("),
  reject => (1, "error: Uncaught TypeError: unhandled"),
  tla => (13, "Top-level await promise never resolved"),
  syntax => (1, "SyntaxError: Unexpected token ';'"),
  exit => (7, ""),
  exitcode => (3, ""),
  timer_throw => (1, "error: Uncaught RangeError: from timer"),
}

/// Needs a PostgreSQL server: set SAKO_PG_TEST_URL (for example
/// postgres://user:pass@127.0.0.1:5432/postgres). Skipped otherwise.
#[test]
fn postgres() {
  if std::env::var_os("SAKO_PG_TEST_URL").is_none() {
    eprintln!("SAKO_PG_TEST_URL not set; skipping");
    return;
  }
  run_fixture("postgres", 0, "");
}

#[test]
fn eval_and_print_flags() {
  let sako = env!("CARGO_BIN_EXE_sako");
  let out = Command::new(sako).args(["-p", "[1, 2].map((x) => x * 2)"]).output().unwrap();
  assert_eq!(String::from_utf8_lossy(&out.stdout), "[ 2, 4 ]\n");
  let out = Command::new(sako).args(["-e", "console.log(Sako.args)", "--", "a", "b"]).output().unwrap();
  assert_eq!(String::from_utf8_lossy(&out.stdout), "[ 'a', 'b' ]\n");
  let mut child = Command::new(sako)
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .spawn()
    .unwrap();
  child.stdin.take().unwrap().write_all(b"console.log('from stdin', 6 * 7)").unwrap();
  let out = child.wait_with_output().unwrap();
  assert_eq!(String::from_utf8_lossy(&out.stdout), "from stdin 42\n");
}

#[test]
fn large_output_is_complete() {
  let out = Command::new(env!("CARGO_BIN_EXE_sako"))
    .args(["-e", "for (let i = 0; i < 100000; i++) console.log(i)"])
    .output()
    .unwrap();
  let stdout = String::from_utf8_lossy(&out.stdout);
  assert_eq!(stdout.lines().count(), 100000);
  assert_eq!(stdout.lines().last(), Some("99999"));
}

// ------------------------------------------------------- tiny HTTP/1.1 server

/// Starts (once) a keep-alive HTTP/1.1 server for the fetch fixture and
/// returns its base URL.
fn test_server() -> &'static str {
  static URL: OnceLock<String> = OnceLock::new();
  URL.get_or_init(|| {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
      for conn in listener.incoming().flatten() {
        std::thread::spawn(move || serve_test_conn(conn));
      }
    });
    url
  })
}

fn serve_test_conn(conn: TcpStream) {
  let mut reader = BufReader::new(conn.try_clone().unwrap());
  let mut conn = conn;
  loop {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
      return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
      let mut line = String::new();
      if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
      }
      let line = line.trim_end();
      if line.is_empty() {
        break;
      }
      if let Some((k, v)) = line.split_once(':') {
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
      }
    }
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("");
    let len: usize = header("content-length").parse().unwrap_or(0);
    let mut body = vec![0; len];
    if reader.read_exact(&mut body).is_err() {
      return;
    }
    let (status, extra, resp_body) = match path.as_str() {
      "/json" => ("200 OK", "content-type: application/json\r\n".to_string(), r#"{"ok":true}"#.to_string()),
      "/redirect" => ("302 Found", "location: /json\r\n".to_string(), String::new()),
      "/echo" => (
        "200 OK",
        "content-type: text/plain\r\n".to_string(),
        format!(
          "{method} body={} x-test={} type={}",
          String::from_utf8_lossy(&body),
          header("x-test"),
          header("content-type")
        ),
      ),
      _ => ("404 Not Found", String::new(), "not found".to_string()),
    };
    let head = format!("HTTP/1.1 {status}\r\n{extra}content-length: {}\r\n\r\n", resp_body.len());
    let mut out = head.into_bytes();
    if method != "HEAD" {
      out.extend_from_slice(resp_body.as_bytes());
    }
    if conn.write_all(&out).is_err() {
      return;
    }
  }
}
