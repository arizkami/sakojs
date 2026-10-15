//! HTTP client behind `fetch()`: ureq with rustls and gzip, keep-alive
//! connection pooling, and retries for connections a server already closed.

use std::sync::OnceLock;

/// What to do when the server answers with a redirect.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Redirect {
  Follow,
  /// Return the 3xx response itself.
  Manual,
  /// Fail the request.
  Error,
}

pub struct FetchRequest {
  pub url: String,
  pub method: String,
  pub headers: Vec<(String, String)>,
  pub body: Option<Vec<u8>>,
  pub redirect: Redirect,
}

pub struct FetchResponse {
  pub status: u16,
  pub status_text: String,
  /// Final URL after redirects.
  pub url: String,
  pub redirected: bool,
  pub headers: Vec<(String, String)>,
  pub body: Vec<u8>,
}

static USER_AGENT: OnceLock<String> = OnceLock::new();

/// Sets the User-Agent sent with every request (once, before the first one).
pub fn set_user_agent(ua: impl Into<String>) {
  let _ = USER_AGENT.set(ua.into());
}

fn agent_config() -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
  let ua = USER_AGENT.get().cloned().unwrap_or_else(|| format!("sako/{}", env!("CARGO_PKG_VERSION")));
  ureq::Agent::config_builder()
    .http_status_as_error(false)
    .max_redirects(20)
    .save_redirect_history(true)
    .user_agent(ua)
    // Keep enough idle keep-alive connections for highly concurrent clients;
    // ureq's default (3 per host) forces reconnects under Promise.all.
    .max_idle_connections(1024)
    .max_idle_connections_per_host(256)
}

/// Shared agent with keep-alive connection pooling.
fn agent() -> &'static ureq::Agent {
  static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
  AGENT.get_or_init(|| agent_config().build().new_agent())
}

/// Agent that never reuses connections, used to retry after a pooled
/// connection turned out to be dead.
fn fresh_agent() -> &'static ureq::Agent {
  static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
  AGENT.get_or_init(|| agent_config().max_idle_connections(0).max_idle_connections_per_host(0).build().new_agent())
}

fn is_stale_connection(e: &std::io::Error) -> bool {
  use std::io::ErrorKind::*;
  matches!(e.kind(), UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe)
}

/// Performs a request and reads the whole body. Blocking: call it from a
/// worker thread. Errors are `fetch failed: ...` messages.
pub fn fetch(req: FetchRequest) -> Result<FetchResponse, String> {
  use ureq::ResponseExt;
  let FetchRequest { url, method, headers, body, redirect } = req;
  let fail = |e: &dyn std::fmt::Display| format!("fetch failed: {e}");
  let send = |agent: &ureq::Agent| {
    let mut req = ureq::http::Request::builder().method(method.as_str()).uri(&url);
    for (k, v) in &headers {
      req = req.header(k.as_str(), v.as_str());
    }
    // Per-request config: only needed when redirects must not be followed.
    let follow = redirect == Redirect::Follow;
    match &body {
      Some(b) => {
        let req = req.body(b.as_slice()).map_err(ureq::Error::from)?;
        if follow { agent.run(req) } else { agent.run(agent.configure_request(req).max_redirects(0).build()) }
      }
      None => {
        let req = req.body(()).map_err(ureq::Error::from)?;
        if follow { agent.run(req) } else { agent.run(agent.configure_request(req).max_redirects(0).build()) }
      }
    }
  };
  // A pooled keep-alive connection may already be closed by the server. ureq
  // also pools HTTP/1.0 connections the server is about to close, which can
  // break the next redirect hop. Retry idempotent requests on fresh
  // connections, as browsers do.
  let idempotent = matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS" | "PUT" | "DELETE" | "TRACE");
  let res = match send(agent()) {
    Err(ureq::Error::Io(e)) if idempotent && is_stale_connection(&e) => send(fresh_agent()),
    r => r,
  };
  let mut res = res.map_err(|e| fail(&e))?;
  let status = res.status();
  if redirect == Redirect::Error && status.is_redirection() {
    return Err("fetch failed: unexpected redirect".into());
  }
  let final_url = res.get_uri().to_string();
  let redirected = res.get_redirect_history().is_some_and(|h| h.len() > 1);
  let headers = res
    .headers()
    .iter()
    .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
    .collect();
  let body = res.body_mut().with_config().limit(u64::MAX).read_to_vec().map_err(|e| fail(&e))?;
  Ok(FetchResponse {
    status: status.as_u16(),
    status_text: status.canonical_reason().unwrap_or("").to_string(),
    url: final_url,
    redirected,
    headers,
    body,
  })
}
