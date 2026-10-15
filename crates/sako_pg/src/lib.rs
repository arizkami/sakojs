//! PostgreSQL client for Sako (`Sako.sql`).
//!
//! - Wire protocol v3 over TCP, TLS (rustls, webpki roots) or Unix sockets.
//! - Authentication: SCRAM-SHA-256, MD5, cleartext.
//! - Extended-protocol queries with a per-connection prepared statement
//!   cache; text-format values decoded by type ([`types`]).
//! - A blocking connection [`Pool`] (callers run queries on worker threads).

mod conn;
mod scram;
pub mod types;

pub use conn::{BatchQuery, Column, Connection, DbError, Param, QueryResult};
pub use types::Value;

use std::sync::{Arc, Condvar, Mutex};

#[derive(Debug)]
pub enum Error {
  Io(String),
  Protocol(String),
  Auth(String),
  Config(String),
  /// An error reported by the server.
  Db(Box<DbError>),
  PoolClosed,
}

impl std::fmt::Display for Error {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Error::Io(m) => write!(f, "connection error: {m}"),
      Error::Protocol(m) => write!(f, "protocol error: {m}"),
      Error::Auth(m) => write!(f, "authentication failed: {m}"),
      Error::Config(m) => write!(f, "invalid connection settings: {m}"),
      Error::Db(e) => f.write_str(&e.message),
      Error::PoolClosed => f.write_str("the connection pool was closed (sql.end())"),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SslMode {
  Disable,
  /// Use TLS if the server offers it.
  Prefer,
  Require,
}

#[derive(Debug, Clone)]
pub struct Config {
  /// Host name, IP address, or a Unix socket directory (starting with "/").
  pub host: String,
  pub port: u16,
  pub user: String,
  pub password: Option<String>,
  pub database: String,
  pub ssl: SslMode,
  pub application_name: String,
  /// Maximum pool size.
  pub max: usize,
}

fn percent_decode(s: &str) -> String {
  let b = s.as_bytes();
  let mut out = Vec::with_capacity(b.len());
  let mut i = 0;
  while i < b.len() {
    if b[i] == b'%'
      && i + 2 < b.len()
      && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
    {
      out.push(v);
      i += 3;
      continue;
    }
    out.push(b[i]);
    i += 1;
  }
  String::from_utf8_lossy(&out).into_owned()
}

fn parse_ssl(v: &str) -> Result<SslMode, Error> {
  match v {
    "disable" | "false" => Ok(SslMode::Disable),
    "allow" | "prefer" => Ok(SslMode::Prefer),
    "require" | "verify-ca" | "verify-full" | "true" => Ok(SslMode::Require),
    other => Err(Error::Config(format!("unknown sslmode: {other}"))),
  }
}

impl Config {
  /// Defaults from the libpq environment variables (PGHOST, PGPORT, PGUSER,
  /// PGPASSWORD, PGDATABASE, PGSSLMODE, PGAPPNAME).
  pub fn from_env() -> Config {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let user = env("PGUSER").or_else(|| env("USER")).unwrap_or_else(|| "postgres".into());
    let host = env("PGHOST").unwrap_or_else(|| "localhost".into());
    let local = host == "localhost" || host == "127.0.0.1" || host == "::1" || host.starts_with('/');
    Config {
      port: env("PGPORT").and_then(|p| p.parse().ok()).unwrap_or(5432),
      password: env("PGPASSWORD"),
      database: env("PGDATABASE").unwrap_or_else(|| user.clone()),
      ssl: env("PGSSLMODE")
        .and_then(|v| parse_ssl(&v).ok())
        .unwrap_or(if local { SslMode::Disable } else { SslMode::Prefer }),
      application_name: env("PGAPPNAME").unwrap_or_else(|| "sako".into()),
      max: 10,
      user,
      host,
    }
  }

  /// Parses `postgres://user:pass@host:port/db?sslmode=...&max=...`. Missing
  /// parts fall back to the PG* environment variables.
  pub fn parse(url: &str) -> Result<Config, Error> {
    let mut c = Config::from_env();
    let rest = url
      .strip_prefix("postgres://")
      .or_else(|| url.strip_prefix("postgresql://"))
      .ok_or_else(|| Error::Config(format!("expected a postgres:// URL, got {url:?}")))?;
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, db) = rest.split_once('/').unwrap_or((rest, ""));
    let (userinfo, hostport) = match authority.rsplit_once('@') {
      Some((u, h)) => (Some(u), h),
      None => (None, authority),
    };
    if let Some(u) = userinfo {
      let (user, pass) = u.split_once(':').unwrap_or((u, ""));
      if !user.is_empty() {
        c.user = percent_decode(user);
        c.database = c.user.clone();
      }
      if !pass.is_empty() {
        c.password = Some(percent_decode(pass));
      }
    }
    if !hostport.is_empty() {
      let (host, port) = if let Some(h) = hostport.strip_prefix('[') {
        let (h, p) = h.split_once(']').ok_or_else(|| Error::Config("unterminated [ipv6] host".into()))?;
        (h.to_string(), p.strip_prefix(':'))
      } else {
        match hostport.rsplit_once(':') {
          Some((h, p)) => (h.to_string(), Some(p)),
          None => (hostport.to_string(), None),
        }
      };
      if !host.is_empty() {
        c.host = percent_decode(&host);
      }
      if let Some(p) = port {
        c.port = p.parse().map_err(|_| Error::Config(format!("bad port: {p}")))?;
      }
    }
    if !db.is_empty() {
      c.database = percent_decode(db);
    }
    let local = c.host == "localhost" || c.host == "127.0.0.1" || c.host == "::1" || c.host.starts_with('/');
    if std::env::var("PGSSLMODE").is_err() {
      c.ssl = if local { SslMode::Disable } else { SslMode::Prefer };
    }
    for pair in query.split('&').filter(|p| !p.is_empty()) {
      let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
      let v = percent_decode(v);
      match k {
        "sslmode" | "ssl" => c.ssl = parse_ssl(&v)?,
        "application_name" => c.application_name = v,
        "host" => c.host = v,
        "max" | "pool_max" => c.max = v.parse().map_err(|_| Error::Config(format!("bad max: {v}")))?,
        _ => {}
      }
    }
    if c.max == 0 {
      c.max = 1;
    }
    Ok(c)
  }
}

// ---------------------------------------------------------------------- pool

pub struct Pool {
  config: Config,
  state: Mutex<PoolState>,
  available: Condvar,
}

struct PoolState {
  idle: Vec<Connection>,
  open: usize,
  closed: bool,
}

impl Pool {
  pub fn new(config: Config) -> Arc<Pool> {
    Arc::new(Pool { config, state: Mutex::new(PoolState { idle: Vec::new(), open: 0, closed: false }), available: Condvar::new() })
  }

  pub fn config(&self) -> &Config {
    &self.config
  }

  /// Takes an idle connection, opens a new one, or waits for one.
  pub fn get(self: &Arc<Self>) -> Result<Pooled, Error> {
    let mut st = self.state.lock().unwrap();
    loop {
      if st.closed {
        return Err(Error::PoolClosed);
      }
      if let Some(conn) = st.idle.pop() {
        return Ok(Pooled { conn: Some(conn), pool: self.clone() });
      }
      if st.open < self.config.max {
        st.open += 1;
        drop(st);
        return match Connection::connect(&self.config) {
          Ok(conn) => Ok(Pooled { conn: Some(conn), pool: self.clone() }),
          Err(e) => {
            let mut st = self.state.lock().unwrap();
            st.open -= 1;
            self.available.notify_one();
            Err(e)
          }
        };
      }
      st = self.available.wait(st).unwrap();
    }
  }

  /// Closes idle connections and refuses new queries. Connections in use
  /// close when released.
  pub fn close(&self) {
    let idle = {
      let mut st = self.state.lock().unwrap();
      st.closed = true;
      st.open -= st.idle.len();
      std::mem::take(&mut st.idle)
    };
    for c in idle {
      c.close();
    }
    self.available.notify_all();
  }

  fn release(&self, mut conn: Connection) {
    // Never hand out a connection stuck inside a transaction.
    if !conn.broken && conn.tx_status != b'I' {
      let _ = conn.simple_query("ROLLBACK");
    }
    let mut st = self.state.lock().unwrap();
    if conn.broken || st.closed {
      st.open -= 1;
      drop(st);
      if !conn.broken {
        conn.close();
      }
    } else {
      st.idle.push(conn);
    }
    self.available.notify_one();
  }
}

/// A connection borrowed from a [`Pool`]; returned when dropped.
pub struct Pooled {
  conn: Option<Connection>,
  pool: Arc<Pool>,
}

impl std::ops::Deref for Pooled {
  type Target = Connection;
  fn deref(&self) -> &Connection {
    self.conn.as_ref().unwrap()
  }
}

impl std::ops::DerefMut for Pooled {
  fn deref_mut(&mut self) -> &mut Connection {
    self.conn.as_mut().unwrap()
  }
}

impl Drop for Pooled {
  fn drop(&mut self) {
    if let Some(c) = self.conn.take() {
      self.pool.release(c);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_url() {
    let c = Config::parse("postgres://al%40ice:p%3Ass@db.example.com:6543/app?sslmode=require&max=4").unwrap();
    assert_eq!((c.user.as_str(), c.password.as_deref()), ("al@ice", Some("p:ss")));
    assert_eq!((c.host.as_str(), c.port, c.database.as_str()), ("db.example.com", 6543, "app"));
    assert_eq!((c.ssl, c.max), (SslMode::Require, 4));
    let c = Config::parse("postgresql://u@[::1]:5433/d").unwrap();
    assert_eq!((c.host.as_str(), c.port, c.ssl), ("::1", 5433, SslMode::Disable));
    assert!(Config::parse("mysql://x").is_err());
  }
}
