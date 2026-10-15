//! One PostgreSQL connection: startup, authentication, TLS and queries over
//! the v3 wire protocol.
//!
//! Queries use the extended protocol with text-format values. Each distinct
//! SQL text is prepared once per connection (Parse + Describe) and reused by
//! name afterwards, so a repeated query is a single Bind/Execute/Sync write
//! and one round trip.

use crate::scram::Scram;
use crate::types::{Value, decode};
use std::borrow::Cow;
use crate::{Config, Error, SslMode};
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Column {
  pub name: String,
  pub type_oid: u32,
}

/// A query parameter, sent in text format (`None` = SQL NULL). Binary data
/// is passed as bytea hex text by the caller.
pub type Param = Option<String>;

/// Rows are kept as raw text cells in one buffer and decoded on demand, so a
/// caller can convert them straight into its own values.
#[derive(Debug, Default)]
pub struct QueryResult {
  pub columns: Arc<Vec<Column>>,
  /// Cell bytes of all rows, concatenated.
  pub data: Vec<u8>,
  /// Row-major (start, len) spans into `data`; len == u32::MAX is NULL.
  pub cells: Vec<(u32, u32)>,
  /// Per column: values are in binary format (see `types::prefers_binary`).
  pub binary: Vec<bool>,
  /// Command tag verb: SELECT, INSERT, UPDATE, ...
  pub command: String,
  /// Rows returned or affected.
  pub count: u64,
}

impl QueryResult {
  pub fn row_count(&self) -> usize {
    if self.columns.is_empty() { 0 } else { self.cells.len() / self.columns.len() }
  }

  /// Raw text of a cell (`None` for NULL).
  pub fn raw(&self, row: usize, col: usize) -> Option<&[u8]> {
    let (start, len) = self.cells[row * self.columns.len() + col];
    if len == u32::MAX { None } else { Some(&self.data[start as usize..(start + len) as usize]) }
  }

  pub fn value(&self, row: usize, col: usize) -> Value {
    let oid = self.columns[col].type_oid;
    if self.binary.get(col) == Some(&true) {
      crate::types::decode_binary(oid, self.raw(row, col))
    } else {
      decode(oid, self.raw(row, col))
    }
  }

  /// All rows, decoded.
  pub fn rows(&self) -> Vec<Vec<Value>> {
    (0..self.row_count()).map(|r| (0..self.columns.len()).map(|c| self.value(r, c)).collect()).collect()
  }

  fn push_row(&mut self, msg: &[u8]) {
    let n = u16::from_be_bytes([msg[0], msg[1]]) as usize;
    let mut pos = 2;
    for _ in 0..n {
      let len = i32::from_be_bytes([msg[pos], msg[pos + 1], msg[pos + 2], msg[pos + 3]]);
      pos += 4;
      if len < 0 {
        self.cells.push((0, u32::MAX));
      } else {
        let start = self.data.len() as u32;
        self.data.extend_from_slice(&msg[pos..pos + len as usize]);
        self.cells.push((start, len as u32));
        pos += len as usize;
      }
    }
  }
}

/// One statement of a pipelined batch.
pub struct BatchQuery<'a> {
  pub sql: &'a str,
  pub params: &'a [Param],
  /// Parameter type OIDs (0 or missing = inferred by the server).
  pub types: &'a [u32],
}

/// Fields of an ErrorResponse.
#[derive(Debug, Clone, Default)]
pub struct DbError {
  pub severity: String,
  pub code: String,
  pub message: String,
  pub detail: Option<String>,
  pub hint: Option<String>,
  pub position: Option<String>,
  pub schema: Option<String>,
  pub table: Option<String>,
  pub column: Option<String>,
  pub constraint: Option<String>,
}

enum Stream {
  Tcp(TcpStream),
  Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
  #[cfg(unix)]
  Unix(std::os::unix::net::UnixStream),
}

impl Read for Stream {
  fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
    match self {
      Stream::Tcp(s) => s.read(buf),
      Stream::Tls(s) => s.read(buf),
      #[cfg(unix)]
      Stream::Unix(s) => s.read(buf),
    }
  }
}

impl Write for Stream {
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    match self {
      Stream::Tcp(s) => s.write(buf),
      Stream::Tls(s) => s.write(buf),
      #[cfg(unix)]
      Stream::Unix(s) => s.write(buf),
    }
  }
  fn flush(&mut self) -> std::io::Result<()> {
    match self {
      Stream::Tcp(s) => s.flush(),
      Stream::Tls(s) => s.flush(),
      #[cfg(unix)]
      Stream::Unix(s) => s.flush(),
    }
  }
}

struct Stmt {
  name: String,
  columns: Arc<Vec<Column>>,
}

/// Prepared statements kept per connection before old ones are closed.
const MAX_STATEMENTS: usize = 256;

pub struct Connection {
  io: BufReader<Stream>,
  wbuf: Vec<u8>,
  msg: Vec<u8>,
  stmts: HashMap<String, Stmt>,
  stmt_order: std::collections::VecDeque<String>,
  next_stmt: u64,
  /// Server parameters (server_version, TimeZone, ...).
  pub server_params: HashMap<String, String>,
  /// Transaction status from the last ReadyForQuery: I (idle), T (in a
  /// transaction), E (failed transaction).
  pub tx_status: u8,
  /// An I/O or protocol error happened; the connection must be dropped.
  pub broken: bool,
}

// ------------------------------------------------------------ frontend messages

fn put_cstr(buf: &mut Vec<u8>, s: &str) {
  buf.extend_from_slice(s.as_bytes());
  buf.push(0);
}

/// Starts a message with a type byte; returns the length field's offset.
fn begin(buf: &mut Vec<u8>, kind: u8) -> usize {
  buf.push(kind);
  let at = buf.len();
  buf.extend_from_slice(&[0; 4]);
  at
}

fn finish(buf: &mut [u8], at: usize) {
  let len = (buf.len() - at) as u32;
  buf[at..at + 4].copy_from_slice(&len.to_be_bytes());
}

fn tls_config() -> Arc<rustls::ClientConfig> {
  static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
  CONFIG
    .get_or_init(|| {
      let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
      let provider = Arc::new(rustls::crypto::ring::default_provider());
      Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
          .with_safe_default_protocol_versions()
          .expect("TLS protocol versions")
          .with_root_certificates(roots)
          .with_no_client_auth(),
      )
    })
    .clone()
}

impl Connection {
  pub fn connect(config: &Config) -> Result<Connection, Error> {
    let io_err = |e: std::io::Error| Error::Io(format!("connect to {}:{}: {e}", config.host, config.port));
    let stream = if config.host.starts_with('/') {
      #[cfg(unix)]
      {
        let path = format!("{}/.s.PGSQL.{}", config.host, config.port);
        Stream::Unix(std::os::unix::net::UnixStream::connect(&path).map_err(io_err)?)
      }
      #[cfg(not(unix))]
      return Err(Error::Io("Unix sockets are not supported on this platform".into()));
    } else {
      let addrs = std::net::ToSocketAddrs::to_socket_addrs(&(config.host.as_str(), config.port)).map_err(io_err)?;
      let mut last = None;
      let mut tcp = None;
      for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
          Ok(s) => {
            tcp = Some(s);
            break;
          }
          Err(e) => last = Some(e),
        }
      }
      let mut tcp = tcp.ok_or_else(|| io_err(last.unwrap_or_else(|| std::io::ErrorKind::NotFound.into())))?;
      let _ = tcp.set_nodelay(true);
      if config.ssl == SslMode::Disable {
        Stream::Tcp(tcp)
      } else {
        // SSLRequest: length 8, code 80877103.
        tcp.write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f]).map_err(io_err)?;
        let mut answer = [0u8; 1];
        tcp.read_exact(&mut answer).map_err(io_err)?;
        match answer[0] {
          b'S' => {
            let name = rustls::pki_types::ServerName::try_from(config.host.clone())
              .map_err(|e| Error::Io(format!("TLS: invalid server name {}: {e}", config.host)))?;
            let conn = rustls::ClientConnection::new(tls_config(), name).map_err(|e| Error::Io(format!("TLS: {e}")))?;
            Stream::Tls(Box::new(rustls::StreamOwned::new(conn, tcp)))
          }
          _ if config.ssl == SslMode::Require => {
            return Err(Error::Io("server does not support TLS (sslmode=require)".into()));
          }
          _ => Stream::Tcp(tcp),
        }
      }
    };
    let mut conn = Connection {
      io: BufReader::with_capacity(64 * 1024, stream),
      wbuf: Vec::with_capacity(4096),
      msg: Vec::with_capacity(4096),
      stmts: HashMap::new(),
      stmt_order: Default::default(),
      next_stmt: 0,
      server_params: HashMap::new(),
      tx_status: b'I',
      broken: false,
    };
    conn.startup(config)?;
    Ok(conn)
  }

  fn flush(&mut self) -> Result<(), Error> {
    let r = self.io.get_mut().write_all(&self.wbuf).and_then(|_| self.io.get_mut().flush());
    self.wbuf.clear();
    r.map_err(|e| {
      self.broken = true;
      Error::Io(e.to_string())
    })
  }

  /// Reads the next backend message into `self.msg`; returns its type.
  fn read_msg(&mut self) -> Result<u8, Error> {
    let mut head = [0u8; 5];
    let r = self.io.read_exact(&mut head).and_then(|_| {
      let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
      if len < 4 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad message length"));
      }
      self.msg.clear();
      self.msg.resize(len - 4, 0);
      self.io.read_exact(&mut self.msg)
    });
    match r {
      Ok(()) => Ok(head[0]),
      Err(e) => {
        self.broken = true;
        Err(if e.kind() == std::io::ErrorKind::UnexpectedEof {
          Error::Io("connection closed by the server".into())
        } else {
          Error::Io(e.to_string())
        })
      }
    }
  }

  fn startup(&mut self, config: &Config) -> Result<(), Error> {
    // StartupMessage has no type byte.
    let mut m = vec![0, 0, 0, 0, 0, 3, 0, 0];
    for (k, v) in [
      ("user", config.user.as_str()),
      ("database", config.database.as_str()),
      ("application_name", config.application_name.as_str()),
      ("client_encoding", "UTF8"),
      ("DateStyle", "ISO"),
    ] {
      put_cstr(&mut m, k);
      put_cstr(&mut m, v);
    }
    m.push(0);
    finish(&mut m, 0);
    self.wbuf = m;
    self.flush()?;

    let password = || {
      config.password.clone().ok_or_else(|| Error::Auth("the server requires a password, but none was given".into()))
    };
    let mut scram: Option<Scram> = None;
    loop {
      match self.read_msg()? {
        b'R' => {
          let code = u32::from_be_bytes(self.msg[..4].try_into().unwrap());
          match code {
            0 => {}
            3 => {
              let pw = password()?;
              let at = begin(&mut self.wbuf, b'p');
              put_cstr(&mut self.wbuf, &pw);
              finish(&mut self.wbuf, at);
              self.flush()?;
            }
            5 => {
              let pw = password()?;
              let salt = &self.msg[4..8];
              let inner = format!("{:x}", md5::compute(format!("{pw}{}", config.user)));
              let mut outer = inner.into_bytes();
              outer.extend_from_slice(salt);
              let hashed = format!("md5{:x}", md5::compute(outer));
              let at = begin(&mut self.wbuf, b'p');
              put_cstr(&mut self.wbuf, &hashed);
              finish(&mut self.wbuf, at);
              self.flush()?;
            }
            10 => {
              let mechanisms: Vec<&[u8]> = self.msg[4..].split(|&b| b == 0).filter(|m| !m.is_empty()).collect();
              if !mechanisms.contains(&b"SCRAM-SHA-256".as_slice()) {
                return Err(Error::Auth("server offered no supported SASL mechanism".into()));
              }
              let s = Scram::new(&password()?);
              let first = s.client_first();
              let at = begin(&mut self.wbuf, b'p');
              put_cstr(&mut self.wbuf, "SCRAM-SHA-256");
              self.wbuf.extend_from_slice(&(first.len() as u32).to_be_bytes());
              self.wbuf.extend_from_slice(first.as_bytes());
              finish(&mut self.wbuf, at);
              self.flush()?;
              scram = Some(s);
            }
            11 => {
              let server_first = String::from_utf8_lossy(&self.msg[4..]).into_owned();
              let s = scram.as_mut().ok_or_else(|| Error::Protocol("unexpected SASLContinue".into()))?;
              let fin = s.client_final(&server_first).map_err(Error::Auth)?;
              let at = begin(&mut self.wbuf, b'p');
              self.wbuf.extend_from_slice(fin.as_bytes());
              finish(&mut self.wbuf, at);
              self.flush()?;
            }
            12 => {
              let server_final = String::from_utf8_lossy(&self.msg[4..]).into_owned();
              let s = scram.as_ref().ok_or_else(|| Error::Protocol("unexpected SASLFinal".into()))?;
              s.verify_server(&server_final).map_err(Error::Auth)?;
            }
            other => return Err(Error::Auth(format!("unsupported authentication method (code {other})"))),
          }
        }
        b'E' => {
          self.broken = true;
          return Err(Error::Db(Box::new(parse_error(&self.msg))));
        }
        b'S' => self.param_status(),
        b'K' | b'N' => {}
        b'Z' => {
          self.tx_status = self.msg.first().copied().unwrap_or(b'I');
          return Ok(());
        }
        other => return Err(Error::Protocol(format!("unexpected message '{}' during startup", other as char))),
      }
    }
  }

  fn param_status(&mut self) {
    let mut parts = self.msg.split(|&b| b == 0);
    if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
      self.server_params.insert(String::from_utf8_lossy(k).into_owned(), String::from_utf8_lossy(v).into_owned());
    }
  }

  /// Runs a parameterized statement (extended protocol, cached prepare).
  pub fn query(&mut self, sql: &str, params: &[Param]) -> Result<QueryResult, Error> {
    self.query_typed(sql, params, &[])
  }

  /// Like [`query`](Self::query), with parameter type OIDs (0 or missing =
  /// let the server infer). Statements are cached per (SQL, types).
  pub fn query_typed(&mut self, sql: &str, params: &[Param], types: &[u32]) -> Result<QueryResult, Error> {
    self.query_batch(&[BatchQuery { sql, params, types }])?.pop().unwrap()
  }

  /// Runs several statements pipelined: all messages in one write, then the
  /// responses in order. Each statement has its own Sync, so an error only
  /// fails that statement. The outer error is a connection failure.
  pub fn query_batch(&mut self, batch: &[BatchQuery]) -> Result<Vec<Result<QueryResult, Error>>, Error> {
    if self.broken {
      return Err(Error::Io("connection is broken".into()));
    }
    struct Planned {
      key: String,
      name: String,
      columns: Option<Arc<Vec<Column>>>,
      prepares: bool,
    }
    // Statements prepared earlier in this same batch: key -> name.
    let mut preparing: HashMap<String, String> = HashMap::new();
    // Result formats requested for each query.
    let mut bind_formats: Vec<Vec<bool>> = Vec::with_capacity(batch.len());
    let mut plan = Vec::with_capacity(batch.len());
    for q in batch {
      let key = statement_key(q.sql, q.types).into_owned();
      if let Some(s) = self.stmts.get(&key) {
        plan.push(Planned { name: s.name.clone(), columns: Some(s.columns.clone()), prepares: false, key });
      } else if let Some(name) = preparing.get(&key) {
        plan.push(Planned { name: name.clone(), columns: None, prepares: false, key });
      } else {
        self.next_stmt += 1;
        let name = format!("sako_{}", self.next_stmt);
        // Evict the oldest statement when the cache is full.
        if self.stmts.len() >= MAX_STATEMENTS
          && let Some(old_key) = self.stmt_order.pop_front()
          && let Some(old) = self.stmts.remove(&old_key)
        {
          let at = begin(&mut self.wbuf, b'C');
          self.wbuf.push(b'S');
          put_cstr(&mut self.wbuf, &old.name);
          finish(&mut self.wbuf, at);
        }
        let at = begin(&mut self.wbuf, b'P');
        put_cstr(&mut self.wbuf, &name);
        put_cstr(&mut self.wbuf, q.sql);
        self.wbuf.extend_from_slice(&(q.types.len() as u16).to_be_bytes());
        for t in q.types {
          self.wbuf.extend_from_slice(&t.to_be_bytes());
        }
        finish(&mut self.wbuf, at);
        let at = begin(&mut self.wbuf, b'D');
        self.wbuf.push(b'S');
        put_cstr(&mut self.wbuf, &name);
        finish(&mut self.wbuf, at);
        preparing.insert(key.clone(), name.clone());
        plan.push(Planned { name, columns: None, prepares: true, key });
      }
      let planned = plan.last().unwrap();
      let name = planned.name.clone();
      // Result formats: binary for suitable types once the columns are known
      // (from the cache); text on a statement's first use.
      let formats: Vec<bool> = planned
        .columns
        .as_ref()
        .map(|cols| cols.iter().map(|c| crate::types::prefers_binary(c.type_oid)).collect())
        .unwrap_or_default();
      // Bind: unnamed portal, text-format parameters.
      let at = begin(&mut self.wbuf, b'B');
      self.wbuf.push(0);
      put_cstr(&mut self.wbuf, &name);
      self.wbuf.extend_from_slice(&0u16.to_be_bytes());
      self.wbuf.extend_from_slice(&(q.params.len() as u16).to_be_bytes());
      for p in q.params {
        match p {
          None => self.wbuf.extend_from_slice(&(-1i32).to_be_bytes()),
          Some(v) => {
            self.wbuf.extend_from_slice(&(v.len() as i32).to_be_bytes());
            self.wbuf.extend_from_slice(v.as_bytes());
          }
        }
      }
      if formats.iter().any(|&b| b) {
        self.wbuf.extend_from_slice(&(formats.len() as u16).to_be_bytes());
        for &b in &formats {
          self.wbuf.extend_from_slice(&u16::from(b).to_be_bytes());
        }
      } else {
        self.wbuf.extend_from_slice(&0u16.to_be_bytes());
      }
      finish(&mut self.wbuf, at);
      bind_formats.push(formats);
      let at = begin(&mut self.wbuf, b'E');
      self.wbuf.push(0);
      self.wbuf.extend_from_slice(&0u32.to_be_bytes());
      finish(&mut self.wbuf, at);
      let at = begin(&mut self.wbuf, b'S');
      finish(&mut self.wbuf, at);
    }
    self.flush()?;

    // Column descriptions learned in this batch, for later uses of a
    // statement prepared earlier in it.
    let mut learned: HashMap<String, Arc<Vec<Column>>> = HashMap::new();
    let mut results = Vec::with_capacity(plan.len());
    for (p, formats) in plan.iter().zip(bind_formats) {
      let mut columns = p.columns.clone().or_else(|| learned.get(&p.key).cloned());
      let mut result = QueryResult { binary: formats, ..QueryResult::default() };
      let mut error = None;
      let mut parsed = false;
      loop {
        match self.read_msg()? {
          b'1' => parsed = true,
          b'T' => columns = Some(Arc::new(parse_row_description(&self.msg))),
          b'n' => columns = Some(Arc::new(Vec::new())),
          b'D' => result.push_row(&self.msg),
          b'C' => {
            let (command, count) = parse_command_tag(&self.msg);
            result.command = command;
            result.count = count;
          }
          b'E' => error = Some(parse_error(&self.msg)),
          b'S' => self.param_status(),
          b'Z' => {
            self.tx_status = self.msg.first().copied().unwrap_or(b'I');
            break;
          }
          // ParameterDescription, BindComplete, EmptyQuery, notices, ...
          _ => {}
        }
      }
      if p.prepares
        && parsed
        && let Some(cols) = &columns
      {
        learned.insert(p.key.clone(), cols.clone());
        self.stmts.insert(p.key.clone(), Stmt { name: p.name.clone(), columns: cols.clone() });
        self.stmt_order.push_back(p.key.clone());
      }
      results.push(match error {
        Some(e) => Err(Error::Db(Box::new(e))),
        None => {
          result.columns = columns.unwrap_or_default();
          Ok(result)
        }
      });
    }
    Ok(results)
  }

  /// Runs SQL with the simple protocol (several statements allowed, no
  /// parameters). Returns the last statement's result.
  pub fn simple_query(&mut self, sql: &str) -> Result<QueryResult, Error> {
    if self.broken {
      return Err(Error::Io("connection is broken".into()));
    }
    let at = begin(&mut self.wbuf, b'Q');
    put_cstr(&mut self.wbuf, sql);
    finish(&mut self.wbuf, at);
    self.flush()?;
    let mut result = QueryResult::default();
    let mut columns: Arc<Vec<Column>> = Arc::default();
    let mut error = None;
    loop {
      match self.read_msg()? {
        b'T' => {
          columns = Arc::new(parse_row_description(&self.msg));
          result = QueryResult::default();
        }
        b'D' => result.push_row(&self.msg),
        b'C' => {
          let (command, count) = parse_command_tag(&self.msg);
          result.command = command;
          result.count = count;
          result.columns = columns.clone();
        }
        b'E' => error = Some(parse_error(&self.msg)),
        b'S' => self.param_status(),
        b'Z' => {
          self.tx_status = self.msg.first().copied().unwrap_or(b'I');
          break;
        }
        _ => {}
      }
    }
    match error {
      Some(e) => Err(Error::Db(Box::new(e))),
      None => Ok(result),
    }
  }

  /// Sends Terminate (best effort).
  pub fn close(mut self) {
    let at = begin(&mut self.wbuf, b'X');
    finish(&mut self.wbuf, at);
    let _ = self.flush();
  }
}

// ------------------------------------------------------------ backend messages

fn cstr(b: &[u8], pos: &mut usize) -> String {
  let end = b[*pos..].iter().position(|&c| c == 0).map_or(b.len(), |i| *pos + i);
  let s = String::from_utf8_lossy(&b[*pos..end]).into_owned();
  *pos = (end + 1).min(b.len());
  s
}

fn parse_row_description(b: &[u8]) -> Vec<Column> {
  let n = u16::from_be_bytes([b[0], b[1]]) as usize;
  let mut pos = 2;
  let mut cols = Vec::with_capacity(n);
  for _ in 0..n {
    let name = cstr(b, &mut pos);
    // table oid (4), column attr (2), type oid (4), size (2), modifier (4), format (2)
    let type_oid = u32::from_be_bytes([b[pos + 6], b[pos + 7], b[pos + 8], b[pos + 9]]);
    pos += 18;
    cols.push(Column { name, type_oid });
  }
  cols
}

/// Cache key: the SQL text, prefixed by declared parameter types if any.
fn statement_key<'a>(sql: &'a str, types: &[u32]) -> Cow<'a, str> {
  if types.iter().all(|&t| t == 0) {
    return sql.into();
  }
  let mut k = String::with_capacity(sql.len() + types.len() * 5);
  for t in types {
    k.push_str(&t.to_string());
    k.push(',');
  }
  k.push('\0');
  k.push_str(sql);
  k.into()
}

/// "INSERT 0 5" -> ("INSERT", 5); "SELECT 3" -> ("SELECT", 3).
fn parse_command_tag(b: &[u8]) -> (String, u64) {
  let tag = String::from_utf8_lossy(b.strip_suffix(&[0]).unwrap_or(b)).into_owned();
  let count = tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0);
  let command = tag.split(' ').next().unwrap_or("").to_string();
  (command, count)
}

fn parse_error(b: &[u8]) -> DbError {
  let mut e = DbError::default();
  let mut pos = 0;
  while pos < b.len() && b[pos] != 0 {
    let field = b[pos];
    pos += 1;
    let value = cstr(b, &mut pos);
    match field {
      b'V' => e.severity = value,
      b'S' if e.severity.is_empty() => e.severity = value,
      b'C' => e.code = value,
      b'M' => e.message = value,
      b'D' => e.detail = Some(value),
      b'H' => e.hint = Some(value),
      b'P' => e.position = Some(value),
      b's' => e.schema = Some(value),
      b't' => e.table = Some(value),
      b'c' => e.column = Some(value),
      b'n' => e.constraint = Some(value),
      _ => {}
    }
  }
  e
}
