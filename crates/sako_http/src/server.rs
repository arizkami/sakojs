//! HTTP/1.1 server behind `Sako.serve()`.
//!
//! Thread-per-core I/O: each I/O thread runs its own single-threaded Tokio
//! runtime with its own `SO_REUSEPORT` listener, so the kernel spreads
//! connections and there is no work stealing between threads. hyper parses
//! requests there; each request is handed to a [`RequestSink`] (the runtime's
//! event loop). Replies come back through one queue per I/O thread, so a
//! batch of responses costs a single cross-thread wake-up.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::ToSocketAddrs;
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, oneshot, watch};

/// Receives requests that need a handler (everything but static routes).
pub trait RequestSink: Clone + Send + 'static {
  /// Returns false if the receiver is gone (the client then gets a 503).
  fn send(&self, request: HttpRequest) -> bool;
}

pub struct HttpRequest {
  pub server: u32,
  pub method: hyper::Method,
  pub url: String,
  pub headers: hyper::HeaderMap,
  pub body: Bytes,
  pub reply: ReplyTo,
}

type ReplyQueue = mpsc::UnboundedSender<(oneshot::Sender<HttpReply>, HttpReply)>;

/// Where to send the response for one request: the per-connection oneshot,
/// routed through its I/O thread's reply queue.
pub struct ReplyTo {
  queue: ReplyQueue,
  tx: oneshot::Sender<HttpReply>,
}

impl ReplyTo {
  pub fn send(self, reply: HttpReply) {
    let _ = self.queue.send((self.tx, reply));
  }
}

pub struct HttpReply {
  pub status: u16,
  pub headers: Vec<(String, String)>,
  pub body: Vec<u8>,
}

/// A prebuilt response for a static route (`routes: { "/": new Response(..) }`),
/// answered on the I/O threads without involving JS.
pub struct StaticResponse {
  status: StatusCode,
  headers: hyper::HeaderMap,
  body: Bytes,
}

/// path -> [(method or None for any method, response)]
pub type StaticRoutes = Arc<RwLock<HashMap<String, Vec<(Option<hyper::Method>, StaticResponse)>>>>;

pub struct ServerHandle {
  shutdown: Option<watch::Sender<bool>>,
  pub routes: StaticRoutes,
}

impl ServerHandle {
  pub fn add_static(&self, path: String, method: Option<&str>, reply: HttpReply) {
    let method = method.and_then(|m| hyper::Method::from_bytes(m.as_bytes()).ok());
    let mut headers = hyper::HeaderMap::with_capacity(reply.headers.len());
    for (k, v) in reply.headers {
      if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
        headers.append(k, v);
      }
    }
    let res = StaticResponse {
      status: StatusCode::from_u16(reply.status).unwrap_or(StatusCode::OK),
      headers,
      body: Bytes::from(reply.body),
    };
    let mut routes = self.routes.write().unwrap();
    let list = routes.entry(path).or_default();
    list.retain(|(m, _)| *m != method);
    list.push((method, res));
  }

  pub fn clear_static(&self) {
    self.routes.write().unwrap().clear();
  }
}

fn static_response(routes: &StaticRoutes, method: &hyper::Method, path: &str) -> Option<Response<Full<Bytes>>> {
  let routes = routes.read().unwrap();
  let list = routes.get(path)?;
  // HEAD is served by a GET route (hyper drops the body).
  let lookup = if method == hyper::Method::HEAD { &hyper::Method::GET } else { method };
  let (_, r) = list
    .iter()
    .find(|(m, _)| m.as_ref() == Some(lookup))
    .or_else(|| list.iter().find(|(m, _)| m.is_none()))?;
  let mut res = Response::new(Full::new(r.body.clone()));
  *res.status_mut() = r.status;
  *res.headers_mut() = r.headers.clone();
  Some(res)
}

impl ServerHandle {
  pub fn stop_accepting(&mut self) {
    if let Some(tx) = self.shutdown.take() {
      let _ = tx.send(true);
    }
  }
}

fn io_threads() -> usize {
  if !cfg!(unix) {
    return 1; // SO_REUSEPORT load balancing is Linux/BSD only
  }
  std::env::var("SAKO_HTTP_THREADS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get() / 4))
    .clamp(1, 16)
}

fn bind(addr: std::net::SocketAddr, reuse_port: bool) -> std::io::Result<std::net::TcpListener> {
  use socket2::{Domain, Protocol, Socket, Type};
  let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
  socket.set_reuse_address(true)?;
  #[cfg(unix)]
  if reuse_port {
    socket.set_reuse_port(true)?;
  }
  socket.bind(&addr.into())?;
  socket.listen(1024)?;
  socket.set_nonblocking(true)?;
  Ok(socket.into())
}

/// Binds synchronously (so errors surface to JS immediately) and starts the
/// I/O threads. Returns the bound port.
pub fn listen<S: RequestSink>(
  id: u32,
  hostname: &str,
  port: u16,
  reuse_port: bool,
  events: S,
) -> std::io::Result<(u16, ServerHandle)> {
  let mut addr = (hostname, port)
    .to_socket_addrs()?
    .next()
    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "could not resolve hostname"))?;
  let threads = io_threads();
  if threads > 1 && !reuse_port {
    // Our sockets use SO_REUSEPORT internally, which would let us silently
    // share a port another of the user's processes is listening on. A plain
    // bind first reports EADDRINUSE for any existing listener.
    use socket2::{Domain, Protocol, Socket, Type};
    let probe = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    probe.set_reuse_address(true)?;
    probe.bind(&addr.into())?; // bind only: never accepts connections
    if let Some(port) = probe.local_addr()?.as_socket().map(|a| a.port()) {
      addr.set_port(port);
    }
  }
  let first = bind(addr, threads > 1 || reuse_port)?;
  addr.set_port(first.local_addr()?.port());
  let mut listeners = vec![first];
  for _ in 1..threads {
    listeners.push(bind(addr, true)?);
  }

  let (shutdown_tx, shutdown_rx) = watch::channel(false);
  let routes: StaticRoutes = Arc::default();
  for (i, listener) in listeners.into_iter().enumerate() {
    let events = events.clone();
    let routes = routes.clone();
    let shutdown = shutdown_rx.clone();
    std::thread::Builder::new().name(format!("sako-http-{i}")).spawn(move || {
      let rt = tokio::runtime::Builder::new_current_thread().enable_io().build().expect("start HTTP runtime");
      rt.block_on(accept_loop(id, listener, events, routes, shutdown));
    })?;
  }
  Ok((addr.port(), ServerHandle { shutdown: Some(shutdown_tx), routes }))
}

async fn accept_loop<S: RequestSink>(
  id: u32,
  listener: std::net::TcpListener,
  events: S,
  routes: StaticRoutes,
  mut shutdown: watch::Receiver<bool>,
) {
  let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
  // Replies from the JS thread arrive here in batches and complete each
  // connection's oneshot locally (no further cross-thread wake-ups).
  let (queue, mut replies) = mpsc::unbounded_channel::<(oneshot::Sender<HttpReply>, HttpReply)>();
  tokio::spawn(async move {
    while let Some((tx, reply)) = replies.recv().await {
      let _ = tx.send(reply);
    }
  });
  loop {
    tokio::select! {
      _ = shutdown.changed() => break,
      accepted = listener.accept() => {
        let Ok((stream, _)) = accepted else { continue };
        let _ = stream.set_nodelay(true);
        tokio::spawn(serve_connection(id, stream, events.clone(), routes.clone(), queue.clone()));
      }
    }
  }
}

async fn serve_connection<S: RequestSink>(
  id: u32,
  stream: tokio::net::TcpStream,
  events: S,
  routes: StaticRoutes,
  queue: ReplyQueue,
) {
  let service = service_fn(move |req| handle(id, req, events.clone(), routes.clone(), queue.clone()));
  let _ = http1::Builder::new()
    .keep_alive(true)
    .pipeline_flush(true)
    .serve_connection(TokioIo::new(stream), service)
    .await;
}

fn plain(status: StatusCode, msg: &'static str) -> Response<Full<Bytes>> {
  let mut res = Response::new(Full::new(Bytes::from_static(msg.as_bytes())));
  *res.status_mut() = status;
  res
}

async fn handle<S: RequestSink>(
  id: u32,
  req: Request<Incoming>,
  events: S,
  routes: StaticRoutes,
  queue: ReplyQueue,
) -> Result<Response<Full<Bytes>>, Infallible> {
  if let Some(res) = static_response(&routes, req.method(), req.uri().path()) {
    return Ok(res);
  }
  let (parts, body) = req.into_parts();
  let body = match body.collect().await {
    Ok(b) => b.to_bytes(),
    Err(_) => return Ok(plain(StatusCode::BAD_REQUEST, "Bad Request")),
  };
  let host = parts.headers.get(hyper::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
  let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
  let url = format!("http://{host}{path}");
  let (reply_tx, reply_rx) = oneshot::channel();
  let reply = ReplyTo { queue, tx: reply_tx };
  let request = HttpRequest { server: id, method: parts.method, url, headers: parts.headers, body, reply };
  if !events.send(request) {
    return Ok(plain(StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable"));
  }
  let Ok(reply) = reply_rx.await else {
    return Ok(plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"));
  };
  let mut res = Response::new(Full::new(Bytes::from(reply.body)));
  *res.status_mut() = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
  let headers = res.headers_mut();
  for (k, v) in reply.headers {
    if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
      headers.append(k, v);
    }
  }
  Ok(res)
}
