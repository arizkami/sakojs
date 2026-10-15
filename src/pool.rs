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
  Response(Box<FetchResponse>),
}

pub struct FetchResponse {
  pub status: u16,
  pub status_text: String,
  pub url: String,
  pub redirected: bool,
  pub headers: Vec<(String, String)>,
  pub body: Vec<u8>,
}

pub enum OpError {
  Io { err: std::io::Error, syscall: &'static str, path: String },
  Type(String),
  Plain(String),
}

pub type OpResult = Result<Payload, OpError>;

/// Everything that wakes the event loop from another thread.
pub enum Event {
  /// An async op finished on the thread pool.
  Op(u32, OpResult),
  /// An HTTP request arrived on a `Sako.serve()` server.
  Http(Box<crate::http::HttpRequest>),
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
  let resolver = v8::PromiseResolver::new(scope).unwrap();
  let promise = resolver.get_promise(scope);
  let global = v8::Global::new(scope, resolver);
  let st = state(scope);
  let mut s = st.borrow_mut();
  let pool = &mut s.pool;
  pool.next_id = pool.next_id.wrapping_add(1);
  let id = pool.next_id;
  pool.resolvers.insert(id, global);
  let tx = pool.done_tx.clone();
  pool.submit(Box::new(move || {
    let _ = tx.send(Event::Op(id, work()));
  }));
  promise
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

pub fn error_to_v8<'s>(scope: &mut v8::PinScope<'s, '_>, e: OpError) -> v8::Local<'s, v8::Value> {
  match e {
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
