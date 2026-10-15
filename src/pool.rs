//! Async ops: blocking work runs on a lazily started thread pool, and results
//! come back over a channel that the event loop waits on alongside timers.

use crate::state;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long threads busy-wait for work before blocking.
const SPIN: Duration = Duration::from_micros(50);

/// Plain-Rust result of an op, converted to a JS value on the main thread.
pub enum Payload {
  Undefined,
  Bool(bool),
  Bytes(Vec<u8>),
  Text(Vec<u8>),
  Stat { size: u64, is_file: bool, is_dir: bool, is_link: bool, mtime_ms: f64 },
  Dir(Vec<(String, bool, bool, bool)>),
  Response(Box<sako_http::FetchResponse>),
  /// `Sako.$` result: [exitCode, stdout, stderr].
  Shell(crate::shell::Output),
  /// `Sako.sql` query result; the flag selects arrays instead of objects.
  Pg(Box<sako_pg::QueryResult>, bool),
  /// A connection reserved for a transaction (stored on the JS thread).
  PgTransaction(Box<sako_pg::Pooled>),
}

pub enum OpError {
  Io { err: std::io::Error, syscall: &'static str, path: String },
  Type(String),
  Plain(String),
  Pg(sako_pg::Error),
}

pub type OpResult = Result<Payload, OpError>;

/// Everything that wakes the event loop from another thread.
pub enum Event {
  /// An async op finished on the thread pool.
  Op(u32, OpResult),
  /// An HTTP request arrived on a `Sako.serve()` server.
  Http(Box<sako_http::HttpRequest>),
}
type Job = Box<dyn FnOnce() + Send + 'static>;

pub struct Pool {
  jobs: Option<Sender<Job>>,
  done_tx: Sender<Event>,
  pub done_rx: Receiver<Event>,
  resolvers: HashMap<u32, v8::Global<v8::PromiseResolver>>,
  next_id: u32,
}

impl Pool {
  pub fn new() -> Self {
    let (done_tx, done_rx) = mpsc::channel();
    Pool { jobs: None, done_tx, done_rx, resolvers: HashMap::new(), next_id: 0 }
  }

  /// Number of ops whose promise has not settled yet.
  pub fn pending(&self) -> usize {
    self.resolvers.len()
  }

  fn submit(&mut self, job: Job) {
    let jobs = self.jobs.get_or_insert_with(|| {
      let (tx, rx) = mpsc::channel::<Job>();
      let rx = Arc::new(Mutex::new(rx));
      let n = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(4, 16);
      for i in 0..n {
        let rx = rx.clone();
        std::thread::Builder::new()
          .name(format!("sako-worker-{i}"))
          .spawn(move || {
            loop {
              // Briefly spin for follow-up jobs before parking: waking a
              // parked thread costs several microseconds per op.
              let mut job = None;
              let spin_until = Instant::now() + SPIN;
              while Instant::now() < spin_until {
                match rx.lock().unwrap().try_recv() {
                  Ok(j) => {
                    job = Some(j);
                    break;
                  }
                  Err(TryRecvError::Disconnected) => return,
                  Err(TryRecvError::Empty) => std::hint::spin_loop(),
                }
              }
              let job = match job {
                Some(j) => j,
                None => match rx.lock().unwrap().recv() {
                  Ok(j) => j,
                  Err(_) => return,
                },
              };
              job();
            }
          })
          .expect("spawn worker thread");
      }
      tx
    });
    let _ = jobs.send(job);
  }

  pub fn take_resolver(&mut self, id: u32) -> Option<v8::Global<v8::PromiseResolver>> {
    self.resolvers.remove(&id)
  }

  /// Blocks until a completion arrives or `timeout` elapses (None = forever).
  /// Sender for other event sources (HTTP servers).
  pub fn events(&self) -> Sender<Event> {
    self.done_tx.clone()
  }

  pub fn wait(&self, timeout: Option<Duration>) -> Option<Event> {
    // Fast ops (local I/O, keep-alive requests) often finish within a few
    // microseconds; spinning briefly avoids a park/unpark round trip.
    let spin = timeout.map_or(SPIN, |t| t.min(SPIN));
    let spin_until = Instant::now() + spin;
    while Instant::now() < spin_until {
      match self.done_rx.try_recv() {
        Ok(done) => return Some(done),
        Err(TryRecvError::Disconnected) => return None,
        Err(TryRecvError::Empty) => std::hint::spin_loop(),
      }
    }
    let timeout = timeout.map(|t| t.saturating_sub(spin));
    match timeout {
      Some(t) => self.done_rx.recv_timeout(t).ok(),
      None => self.done_rx.recv().ok(),
    }
  }
}

/// Runs `work` on the pool and returns a promise settled with its result.
pub fn spawn<'s>(
  scope: &mut v8::PinScope<'s, '_>,
  work: impl FnOnce() -> OpResult + Send + 'static,
) -> v8::Local<'s, v8::Promise> {
  let (id, promise, tx) = pending(scope);
  submit_detached(scope, move || {
    let _ = tx.send(Event::Op(id, work()));
  });
  promise
}

/// Registers a promise that another thread settles by sending
/// `Event::Op(id, result)` on the returned sender.
pub fn pending<'s>(scope: &mut v8::PinScope<'s, '_>) -> (u32, v8::Local<'s, v8::Promise>, Sender<Event>) {
  let resolver = v8::PromiseResolver::new(scope).unwrap();
  let promise = resolver.get_promise(scope);
  let global = v8::Global::new(scope, resolver);
  let st = state(scope);
  let mut s = st.borrow_mut();
  let pool = &mut s.pool;
  pool.next_id = pool.next_id.wrapping_add(1);
  let id = pool.next_id;
  pool.resolvers.insert(id, global);
  (id, promise, pool.done_tx.clone())
}

/// Runs a job on the pool; it reports its own results (see [`pending`]).
pub fn submit_detached(scope: &v8::PinScope, job: impl FnOnce() + Send + 'static) {
  state(scope).borrow_mut().pool.submit(Box::new(job));
}

/// Settles the promise for a finished op. Returns false if it was unknown.
pub fn complete(scope: &mut v8::PinScope, id: u32, result: OpResult) -> bool {
  let st = state(scope);
  let Some(resolver) = st.borrow_mut().pool.take_resolver(id) else { return false };
  let resolver = v8::Local::new(scope, resolver);
  match result {
    Ok(p) => {
      let v = to_v8(scope, p);
      resolver.resolve(scope, v);
    }
    Err(e) => {
      let v = error_to_v8(scope, e);
      resolver.reject(scope, v);
    }
  }
  true
}

pub fn new_string<'s>(scope: &mut v8::PinScope<'s, '_>, bytes: &[u8]) -> Option<v8::Local<'s, v8::String>> {
  if bytes.is_ascii() {
    v8::String::new_from_one_byte(scope, bytes, v8::NewStringType::Normal)
  } else {
    v8::String::new_from_utf8(scope, bytes, v8::NewStringType::Normal)
  }
}

pub fn bytes_to_u8array<'s>(scope: &mut v8::PinScope<'s, '_>, data: Vec<u8>) -> v8::Local<'s, v8::Uint8Array> {
  let len = data.len();
  let store = v8::ArrayBuffer::new_backing_store_from_vec(data).make_shared();
  let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
  v8::Uint8Array::new(scope, ab, 0, len).unwrap()
}

fn str_val<'s>(scope: &mut v8::PinScope<'s, '_>, s: &str) -> v8::Local<'s, v8::Value> {
  v8::String::new(scope, s).unwrap().into()
}

pub fn to_v8<'s>(scope: &mut v8::PinScope<'s, '_>, p: Payload) -> v8::Local<'s, v8::Value> {
  match p {
    Payload::Undefined => v8::undefined(scope).into(),
    Payload::Bool(b) => v8::Boolean::new(scope, b).into(),
    Payload::Bytes(b) => bytes_to_u8array(scope, b).into(),
    Payload::Text(b) => match new_string(scope, &b) {
      Some(s) => s.into(),
      None => error_to_v8(scope, OpError::Plain("file too large for a string".into())),
    },
    Payload::Stat { size, is_file, is_dir, is_link, mtime_ms } => {
      let vals = [
        v8::Number::new(scope, size as f64).into(),
        v8::Boolean::new(scope, is_file).into(),
        v8::Boolean::new(scope, is_dir).into(),
        v8::Boolean::new(scope, is_link).into(),
        v8::Number::new(scope, mtime_ms).into(),
      ];
      v8::Array::new_with_elements(scope, &vals).into()
    }
    Payload::Dir(entries) => {
      let mut items = Vec::with_capacity(entries.len());
      for (name, f, d, l) in entries {
        let vals = [
          str_val(scope, &name),
          v8::Boolean::new(scope, f).into(),
          v8::Boolean::new(scope, d).into(),
          v8::Boolean::new(scope, l).into(),
        ];
        items.push(v8::Array::new_with_elements(scope, &vals).into());
      }
      v8::Array::new_with_elements(scope, &items).into()
    }
    Payload::Shell(o) => shell_result(scope, o).into(),
    Payload::Pg(r, _) => pg_result(scope, *r).into(),
    Payload::PgTransaction(conn) => {
      let st = crate::state(scope);
      let mut s = st.borrow_mut();
      s.pg_next += 1;
      let id = s.pg_next;
      s.pg_tx.insert(id, std::sync::Arc::new(std::sync::Mutex::new(*conn)));
      v8::Integer::new_from_unsigned(scope, id).into()
    }
    Payload::Response(r) => {
      let mut headers = Vec::with_capacity(r.headers.len() * 2);
      for (k, v) in &r.headers {
        headers.push(str_val(scope, k));
        headers.push(str_val(scope, v));
      }
      let vals = [
        v8::Integer::new(scope, r.status as i32).into(),
        str_val(scope, &r.status_text),
        str_val(scope, &r.url),
        v8::Boolean::new(scope, r.redirected).into(),
        v8::Array::new_with_elements(scope, &headers).into(),
        bytes_to_u8array(scope, r.body).into(),
      ];
      v8::Array::new_with_elements(scope, &vals).into()
    }
  }
}

pub fn shell_result<'s>(scope: &mut v8::PinScope<'s, '_>, o: crate::shell::Output) -> v8::Local<'s, v8::Array> {
  // Empty streams come back as undefined; UTF-8 output as a string (what
  // .text()/.json()/.lines() want, and cheaper than an external buffer);
  // anything else as bytes. JS builds `stdout` bytes lazily.
  let mut bytes = |b: Vec<u8>| -> v8::Local<'s, v8::Value> {
    if b.is_empty() {
      v8::undefined(scope).into()
    } else if std::str::from_utf8(&b).is_ok()
      && let Some(s) = new_string(scope, &b)
    {
      s.into()
    } else {
      bytes_to_u8array(scope, b).into()
    }
  };
  let (stdout, stderr) = (bytes(o.stdout), bytes(o.stderr));
  let vals = [v8::Integer::new(scope, o.code).into(), stdout, stderr];
  v8::Array::new_with_elements(scope, &vals)
}

fn pg_value<'s>(scope: &mut v8::PinScope<'s, '_>, v: sako_pg::Value) -> v8::Local<'s, v8::Value> {
  use sako_pg::Value as V;
  match v {
    V::Null => v8::null(scope).into(),
    V::Bool(b) => v8::Boolean::new(scope, b).into(),
    V::Int(i) => v8::Integer::new(scope, i).into(),
    V::Float(f) => v8::Number::new(scope, f).into(),
    V::Text(s) => new_string(scope, s.as_bytes()).map_or_else(|| v8::null(scope).into(), Into::into),
    V::Json(s) => {
      let text = new_string(scope, s.as_bytes()).unwrap_or_else(|| v8::String::empty(scope));
      v8::json::parse(scope, text).unwrap_or_else(|| text.into())
    }
    V::Bytes(b) => bytes_to_u8array(scope, b).into(),
    V::Date(ms) => v8::Date::new(scope, ms).map_or_else(|| v8::null(scope).into(), Into::into),
    V::Array(items) => {
      let vals: Vec<v8::Local<v8::Value>> = items.into_iter().map(|i| pg_value(scope, i)).collect();
      v8::Array::new_with_elements(scope, &vals).into()
    }
  }
}

/// One binary-format cell (see `sako_pg::types::prefers_binary`).
fn pg_binary_cell<'s>(scope: &mut v8::PinScope<'s, '_>, type_oid: u32, raw: Option<&[u8]>) -> v8::Local<'s, v8::Value> {
  use sako_pg::types::decode_binary;
  match decode_binary(type_oid, raw) {
    // Dates go to JS as epoch milliseconds (see pg_cell).
    sako_pg::Value::Date(ms) => v8::Number::new(scope, ms).into(),
    v => pg_value(scope, v),
  }
}

/// One text-format cell, decoded straight into a V8 value.
fn pg_cell<'s>(scope: &mut v8::PinScope<'s, '_>, type_oid: u32, raw: Option<&[u8]>) -> v8::Local<'s, v8::Value> {
  use sako_pg::types::{decode, decode_bytea, element_oid, oid, parse_timestamp};
  let Some(raw) = raw else { return v8::null(scope).into() };
  let text = |scope: &mut v8::PinScope<'s, '_>| -> v8::Local<'s, v8::Value> {
    new_string(scope, raw).map_or_else(|| v8::null(scope).into(), Into::into)
  };
  let str = || std::str::from_utf8(raw).ok();
  match type_oid {
    oid::BOOL => v8::Boolean::new(scope, raw == b"t").into(),
    oid::INT2 | oid::INT4 => match str().and_then(|s| s.parse::<i32>().ok()) {
      Some(i) => v8::Integer::new(scope, i).into(),
      None => text(scope),
    },
    oid::OID => match str().and_then(|s| s.parse::<u32>().ok()) {
      Some(i) => v8::Number::new(scope, i as f64).into(),
      None => text(scope),
    },
    oid::FLOAT4 | oid::FLOAT8 => {
      let f = match str() {
        Some("Infinity") => f64::INFINITY,
        Some("-Infinity") => f64::NEG_INFINITY,
        s => s.and_then(|s| s.parse().ok()).unwrap_or(f64::NAN),
      };
      v8::Number::new(scope, f).into()
    }
    oid::JSON | oid::JSONB => {
      let s = new_string(scope, raw).unwrap_or_else(|| v8::String::empty(scope));
      v8::json::parse(scope, s).unwrap_or_else(|| s.into())
    }
    oid::BYTEA => match decode_bytea(raw) {
      Some(b) => bytes_to_u8array(scope, b).into(),
      None => text(scope),
    },
    // As epoch milliseconds: JS wraps them in `new Date()` (much cheaper
    // than creating Dates through the C++ API).
    oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ => match str().and_then(parse_timestamp) {
      Some(ms) => v8::Number::new(scope, ms).into(),
      None => text(scope),
    },
    oid::INT8 => text(scope),
    t if element_oid(t).is_some() => pg_value(scope, decode(t, Some(raw))),
    _ => text(scope),
  }
}

/// [columnNames, flatValues, command, count, dateColumns]: row-major values
/// in one array. JS turns them into row objects with a per-column-set literal
/// constructor (V8's fastest way to create same-shaped objects), or arrays;
/// `dateColumns` flags columns whose numbers become Dates.
fn pg_result<'s>(scope: &mut v8::PinScope<'s, '_>, r: sako_pg::QueryResult) -> v8::Local<'s, v8::Array> {
  let names: Vec<v8::Local<v8::Value>> = r
    .columns
    .iter()
    .map(|c| v8::String::new_from_utf8(scope, c.name.as_bytes(), v8::NewStringType::Internalized).unwrap().into())
    .collect();
  let width = r.columns.len();
  let binary: Vec<bool> = (0..width).map(|c| r.binary.get(c) == Some(&true)).collect();
  let mut flat = Vec::with_capacity(r.cells.len());
  for row in 0..r.row_count() {
    for col in 0..width {
      let oid = r.columns[col].type_oid;
      flat.push(if binary[col] { pg_binary_cell(scope, oid, r.raw(row, col)) } else { pg_cell(scope, oid, r.raw(row, col)) });
    }
  }
  use sako_pg::types::oid;
  let dates: Vec<v8::Local<v8::Value>> = r
    .columns
    .iter()
    .map(|c| v8::Boolean::new(scope, matches!(c.type_oid, oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ)).into())
    .collect();
  let out = [
    v8::Array::new_with_elements(scope, &names).into(),
    v8::Array::new_with_elements(scope, &flat).into(),
    v8::String::new(scope, &r.command).unwrap().into(),
    v8::Number::new(scope, r.count as f64).into(),
    v8::Array::new_with_elements(scope, &dates).into(),
  ];
  v8::Array::new_with_elements(scope, &out)
}

pub fn error_to_v8<'s>(scope: &mut v8::PinScope<'s, '_>, e: OpError) -> v8::Local<'s, v8::Value> {
  match e {
    OpError::Pg(e) => {
      let m = v8::String::new(scope, &e.to_string()).unwrap();
      let err = v8::Exception::error(scope, m);
      if let Ok(obj) = v8::Local::<v8::Object>::try_from(err) {
        let set = |k: &str, v: &str| {
          let k = v8::String::new(scope, k).unwrap();
          let v = v8::String::new(scope, v).unwrap();
          obj.set(scope, k.into(), v.into());
        };
        set("name", "PostgresError");
        match &e {
          sako_pg::Error::Db(d) => {
            set("code", &d.code);
            set("severity", &d.severity);
            for (k, v) in [
              ("detail", &d.detail),
              ("hint", &d.hint),
              ("position", &d.position),
              ("schema", &d.schema),
              ("table", &d.table),
              ("column", &d.column),
              ("constraint", &d.constraint),
            ] {
              if let Some(v) = v {
                set(k, v);
              }
            }
          }
          sako_pg::Error::PoolClosed => set("code", "ERR_POSTGRES_CONNECTION_CLOSED"),
          sako_pg::Error::Auth(_) => set("code", "ERR_POSTGRES_AUTHENTICATION"),
          _ => set("code", "ERR_POSTGRES_CONNECTION"),
        }
      }
      err
    }
    OpError::Type(msg) => {
      let m = v8::String::new(scope, &msg).unwrap();
      v8::Exception::type_error(scope, m)
    }
    OpError::Plain(msg) => {
      let m = v8::String::new(scope, &msg).unwrap();
      v8::Exception::error(scope, m)
    }
    OpError::Io { err, syscall, path } => {
      use std::io::ErrorKind::*;
      let code = match err.kind() {
        NotFound => "ENOENT",
        PermissionDenied => "EACCES",
        AlreadyExists => "EEXIST",
        IsADirectory => "EISDIR",
        NotADirectory => "ENOTDIR",
        DirectoryNotEmpty => "ENOTEMPTY",
        InvalidInput | InvalidData => "EINVAL",
        AddrInUse => "EADDRINUSE",
        AddrNotAvailable => "EADDRNOTAVAIL",
        _ => "EIO",
      };
      let mut desc = err.to_string();
      if let Some(i) = desc.find(" (os error") {
        desc.truncate(i);
      }
      let msg = format!("{code}: {}, {syscall} '{path}'", desc.to_lowercase());
      let m = v8::String::new(scope, &msg).unwrap();
      let e = v8::Exception::error(scope, m);
      if let Ok(obj) = v8::Local::<v8::Object>::try_from(e) {
        for (k, v) in [("code", code), ("syscall", syscall), ("path", path.as_str())] {
          let k = v8::String::new(scope, k).unwrap();
          let v = v8::String::new(scope, v).unwrap();
          obj.set(scope, k.into(), v.into());
        }
      }
      e
    }
  }
}
