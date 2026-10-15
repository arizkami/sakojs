//! Native functions exposed to the bootstrap as the `ops` object.

use crate::pool::{self, OpError, OpResult, Payload};
use crate::{state, State};
use std::io::Write;
use std::mem::MaybeUninit;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type Args<'a> = v8::FunctionCallbackArguments<'a>;

macro_rules! op {
  ($name:ident($scope:ident, $args:ident, $rv:ident) $body:block) => {
    fn $name($scope: &mut v8::PinScope, $args: Args, #[allow(unused_mut)] mut $rv: v8::ReturnValue) $body
  };
}

pub fn create<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Object> {
  let ops = v8::Object::new(scope);

  macro_rules! reg {
    ($($name:literal => $f:expr),* $(,)?) => {$(
      let f = v8::Function::builder($f).build(scope).unwrap();
      let k = v8::String::new(scope, $name).unwrap();
      f.set_name(k);
      ops.set(scope, k.into(), f.into());
    )*};
  }
  reg! {
    "print" => op_print,
    "timerStart" => op_timer_start,
    "timerClear" => op_timer_clear,
    "now" => op_now,
    "reportError" => op_report_error,
    "encode" => op_encode,
    "decode" => op_decode,
    "promiseState" => op_promise_state,
    "exit" => op_exit,
    "cwd" => op_cwd,
    "chdir" => op_chdir,
    "memoryUsage" => op_memory_usage,
    "readTextFile" => op_read_text_file,
    "readFile" => op_read_file,
    "writeFile" => op_write_file,
    "exists" => op_exists,
    "readDir" => op_read_dir,
    "mkdir" => op_mkdir,
    "remove" => op_remove,
    "stat" => op_stat,
    "readTextFileAsync" => op_read_text_file_async,
    "readFileAsync" => op_read_file_async,
    "writeFileAsync" => op_write_file_async,
    "existsAsync" => op_exists_async,
    "readDirAsync" => op_read_dir_async,
    "mkdirAsync" => op_mkdir_async,
    "removeAsync" => op_remove_async,
    "statAsync" => op_stat_async,
    "fetch" => op_fetch,
    "urlParse" => op_url_parse,
    "shell" => op_shell,
    "pgOpen" => op_pg_open,
    "pgQuery" => op_pg_query,
    "pgBegin" => op_pg_begin,
    "pgFinish" => op_pg_finish,
    "pgClose" => op_pg_close,
    "mapStack" => op_map_stack,
    "shellParse" => op_shell_parse,
    "shellEscape" => op_shell_escape,
    "serve" => op_serve,
    "serverStop" => op_server_stop,
    "httpRespond" => op_http_respond,
    "serveStatic" => op_serve_static,
    "serveStaticClear" => op_serve_static_clear,
    "urlHref" => op_url_href,
    "urlSet" => op_url_set,
    "setHooks" => op_set_hooks,
    "ticksPending" => op_ticks_pending,
    "immediatesPending" => op_immediates_pending,
  }

  let st = state(scope);
  let set = |scope: &mut v8::PinScope<'s, '_>, k: &str, v: v8::Local<v8::Value>| {
    let key = v8::String::new(scope, k).unwrap();
    ops.set(scope, key.into(), v);
  };

  let origin_ms = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs_f64() * 1000.0)
    .unwrap_or(0.0)
    - st.borrow().start.elapsed().as_secs_f64() * 1000.0;
  let v = v8::Number::new(scope, origin_ms).into();
  set(scope, "timeOrigin", v);

  let argv: Vec<v8::Local<v8::Value>> = st
    .borrow()
    .argv
    .iter()
    .map(|a| v8::String::new(scope, a).unwrap().into())
    .collect();
  let v = v8::Array::new_with_elements(scope, &argv).into();
  set(scope, "argv", v);
  let script_args: Vec<v8::Local<v8::Value>> = st
    .borrow()
    .script_args
    .iter()
    .map(|a| v8::String::new(scope, a).unwrap().into())
    .collect();
  let v = v8::Array::new_with_elements(scope, &script_args).into();
  set(scope, "args", v);

  let env = v8::Object::new(scope);
  for (k, v) in std::env::vars_os() {
    let k = v8::String::new(scope, &k.to_string_lossy()).unwrap();
    let v = v8::String::new(scope, &v.to_string_lossy()).unwrap();
    env.set(scope, k.into(), v.into());
  }
  set(scope, "env", env.into());

  let v = v8::Integer::new_from_unsigned(scope, std::process::id()).into();
  set(scope, "pid", v);
  let platform = match std::env::consts::OS {
    "macos" => "darwin",
    "windows" => "win32",
    o => o,
  };
  let v = v8::String::new(scope, platform).unwrap().into();
  set(scope, "platform", v);
  let arch = match std::env::consts::ARCH {
    "x86_64" => "x64",
    "aarch64" => "arm64",
    a => a,
  };
  let v = v8::String::new(scope, arch).unwrap().into();
  set(scope, "arch", v);
  let v = v8::String::new(scope, crate::VERSION).unwrap().into();
  set(scope, "version", v);
  let v = v8::String::new(scope, v8::V8::get_version()).unwrap().into();
  set(scope, "v8Version", v);
  let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
  let v = v8::String::new(scope, &exe).unwrap().into();
  set(scope, "execPath", v);
  let (o, e) = (st.borrow().stdout_tty, st.borrow().stderr_tty);
  let v = v8::Boolean::new(scope, o).into();
  set(scope, "stdoutIsTTY", v);
  let v = v8::Boolean::new(scope, e).into();
  set(scope, "stderrIsTTY", v);
  ops
}

// ------------------------------------------------------------------ helpers

fn throw(scope: &mut v8::PinScope, e: OpError) {
  let v = pool::error_to_v8(scope, e);
  scope.throw_exception(v);
}

fn throw_type_error(scope: &mut v8::PinScope, msg: &str) {
  throw(scope, OpError::Type(msg.to_string()));
}

fn io_err(err: std::io::Error, syscall: &'static str, path: String) -> OpError {
  OpError::Io { err, syscall, path }
}

fn arg_string(scope: &mut v8::PinScope, args: &Args, i: i32) -> Option<String> {
  let v = args.get(i);
  if !v.is_string() {
    throw_type_error(scope, &format!("argument {} must be a string", i + 1));
    return None;
  }
  Some(v.to_rust_string_lossy(scope))
}

/// Append a JS string to `out` as UTF-8 without an intermediate allocation.
fn append_utf8(scope: &v8::PinScope, s: v8::Local<v8::String>, out: &mut Vec<u8>) {
  let len = s.utf8_length(scope);
  out.reserve(len);
  let spare: &mut [MaybeUninit<u8>] = &mut out.spare_capacity_mut()[..len];
  let n = s.write_utf8_uninit_v2(scope, spare, v8::WriteFlags::kReplaceInvalidUtf8, None);
  // SAFETY: V8 initialized exactly `n` bytes of spare capacity.
  unsafe { out.set_len(out.len() + n) };
}

/// Copies a string or ArrayBufferView argument into owned bytes.
fn arg_bytes(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> Option<Vec<u8>> {
  if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(v) {
    let mut storage = [0u8; v8::TYPED_ARRAY_MAX_SIZE_IN_HEAP];
    return Some(view.get_contents(&mut storage).to_vec());
  }
  if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(v) {
    let len = ab.byte_length();
    return Some(match ab.data() {
      // SAFETY: the buffer is alive for the duration of this call.
      Some(ptr) => unsafe { std::slice::from_raw_parts(ptr.as_ptr() as *const u8, len) }.to_vec(),
      None => Vec::new(),
    });
  }
  let s = v.to_string(scope)?;
  let mut out = Vec::new();
  append_utf8(scope, s, &mut out);
  Some(out)
}

// ---------------------------------------------------------------------- ops

op!(op_print(scope, args, _rv) {
  let Some(s) = args.get(0).to_string(scope) else { return };
  let is_err = args.get(1).is_true();
  let st = state(scope);
  let mut st = st.borrow_mut();
  let st: &mut State = &mut st;
  if is_err {
    st.flush();
    let mut buf = Vec::new();
    append_utf8(scope, s, &mut buf);
    let _ = std::io::stderr().write_all(&buf);
  } else {
    append_utf8(scope, s, &mut st.out);
    if st.stdout_tty || st.out.len() >= crate::OUT_BUF_LIMIT {
      st.flush();
    }
  }
});

op!(op_timer_start(scope, args, _rv) {
  let id = args.get(0).uint32_value(scope).unwrap_or(0);
  let ms = args.get(1).number_value(scope).unwrap_or(0.0).max(0.0);
  let refed = !args.get(2).is_true();
  let st = state(scope);
  let mut s = st.borrow_mut();
  s.timer_seq += 1;
  let seq = s.timer_seq;
  let deadline = Instant::now() + Duration::from_secs_f64(ms / 1000.0);
  s.active_timers.insert(id, (seq, refed));
  s.timers.push(std::cmp::Reverse((deadline, seq, id)));
});

op!(op_timer_clear(scope, args, _rv) {
  let id = args.get(0).uint32_value(scope).unwrap_or(0);
  state(scope).borrow_mut().active_timers.remove(&id);
});

op!(op_now(scope, _args, rv) {
  let ms = state(scope).borrow().start.elapsed().as_secs_f64() * 1000.0;
  rv.set_double(ms);
});

op!(op_report_error(scope, args, _rv) {
  crate::print_error(scope, args.get(0));
  crate::exit_now(scope, 1);
});

op!(op_exit(scope, args, _rv) {
  let code = args.get(0).int32_value(scope).unwrap_or(0);
  crate::exit_now(scope, code);
});

op!(op_encode(scope, args, rv) {
  let Some(s) = args.get(0).to_string(scope) else { return };
  let mut buf = Vec::new();
  append_utf8(scope, s, &mut buf);
  let arr = pool::bytes_to_u8array(scope, buf);
  rv.set(arr.into());
});

op!(op_decode(scope, args, rv) {
  let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(args.get(0)) else {
    return throw_type_error(scope, "expected an ArrayBufferView");
  };
  let fatal = args.get(1).is_true();
  let ignore_bom = args.get(2).is_true();
  let mut storage = [0u8; v8::TYPED_ARRAY_MAX_SIZE_IN_HEAP];
  let mut bytes = view.get_contents(&mut storage);
  if !ignore_bom && bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
    bytes = &bytes[3..];
  }
  if fatal && std::str::from_utf8(bytes).is_err() {
    return throw_type_error(scope, "The encoded data was not valid for encoding utf-8");
  }
  match pool::new_string(scope, bytes) {
    Some(s) => rv.set(s.into()),
    None => throw_type_error(scope, "string too long"),
  }
});

op!(op_promise_state(scope, args, rv) {
  let Ok(p) = v8::Local::<v8::Promise>::try_from(args.get(0)) else { return };
  let (n, result) = match p.state() {
    v8::PromiseState::Pending => (0, v8::undefined(scope).into()),
    v8::PromiseState::Fulfilled => (1, p.result(scope)),
    v8::PromiseState::Rejected => (2, p.result(scope)),
  };
  let n = v8::Integer::new(scope, n).into();
  let arr = v8::Array::new_with_elements(scope, &[n, result]);
  rv.set(arr.into());
});

op!(op_cwd(scope, _args, rv) {
  let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
  let s = v8::String::new(scope, &cwd).unwrap();
  rv.set(s.into());
});

op!(op_chdir(scope, args, _rv) {
  let Some(p) = arg_string(scope, &args, 0) else { return };
  state(scope).borrow_mut().cwd_cache = None;
  if let Err(err) = std::env::set_current_dir(&p) {
    throw(scope, io_err(err, "chdir", p));
  }
});

op!(op_memory_usage(scope, _args, rv) {
  let stats = scope.get_heap_statistics();
  let rss = std::fs::read_to_string("/proc/self/statm")
    .ok()
    .and_then(|s| s.split_whitespace().nth(1).and_then(|n| n.parse::<f64>().ok()))
    .map(|pages| pages * 4096.0)
    .unwrap_or(0.0);
  let obj = v8::Object::new(scope);
  for (k, v) in [
    ("rss", rss),
    ("heapTotal", stats.total_heap_size() as f64),
    ("heapUsed", stats.used_heap_size() as f64),
    ("external", stats.external_memory() as f64),
  ] {
    let key = v8::String::new(scope, k).unwrap();
    let val = v8::Number::new(scope, v);
    obj.set(scope, key.into(), val.into());
  }
  rv.set(obj.into());
});

op!(op_ticks_pending(scope, _args, _rv) {
  state(scope).borrow_mut().ticks_pending = true;
});

op!(op_immediates_pending(scope, _args, _rv) {
  state(scope).borrow_mut().immediates_pending = true;
});

op!(op_set_hooks(scope, args, _rv) {
  let get = |scope: &mut v8::PinScope, i: i32| {
    v8::Local::<v8::Function>::try_from(args.get(i)).ok().map(|f| v8::Global::new(scope, f))
  };
  let dispatch = get(scope, 0);
  let inspect = get(scope, 1);
  let helper = get(scope, 2);
  let run_ticks = get(scope, 3);
  let start_immediates = get(scope, 4);
  let run_immediate = get(scope, 5);
  let http_dispatch = get(scope, 6);
  let enable_ts_stacks = get(scope, 7);
  let st = state(scope);
  let mut s = st.borrow_mut();
  s.dispatch = dispatch;
  s.inspect = inspect;
  s.import_helper = helper;
  s.run_ticks = run_ticks;
  s.start_immediates = start_immediates;
  s.run_immediate = run_immediate;
  s.http_dispatch = http_dispatch;
  s.enable_ts_stacks = enable_ts_stacks;
});


// ----------------------------------------------------------------------- fs
//
// Each fs op is a "prep" function that validates JS arguments on the main
// thread and returns a Send closure doing the blocking work. `fs_op!` turns it
// into a sync op (runs inline, throws) and an async op (runs on the pool,
// returns a promise).

type Job = Box<dyn FnOnce() -> OpResult + Send + 'static>;

macro_rules! fs_op {
  ($sync:ident, $async_:ident, $prep:ident) => {
    fn $sync(scope: &mut v8::PinScope, args: Args, mut rv: v8::ReturnValue) {
      if let Some(job) = $prep(scope, &args) {
        match job() {
          Ok(p) => {
            let v = pool::to_v8(scope, p);
            rv.set(v);
          }
          Err(e) => throw(scope, e),
        }
      }
    }
    fn $async_(scope: &mut v8::PinScope, args: Args, mut rv: v8::ReturnValue) {
      if let Some(job) = $prep(scope, &args) {
        let p = pool::spawn(scope, job);
        rv.set(p.into());
      }
    }
  };
}

fn prep_read_text(scope: &mut v8::PinScope, args: &Args) -> Option<Job> {
  let p = arg_string(scope, args, 0)?;
  Some(Box::new(move || std::fs::read(&p).map(Payload::Text).map_err(|e| io_err(e, "open", p))))
}

fn prep_read(scope: &mut v8::PinScope, args: &Args) -> Option<Job> {
  let p = arg_string(scope, args, 0)?;
  Some(Box::new(move || std::fs::read(&p).map(Payload::Bytes).map_err(|e| io_err(e, "open", p))))
}

fn prep_write(scope: &mut v8::PinScope, args: &Args) -> Option<Job> {
  let p = arg_string(scope, args, 0)?;
  let data = arg_bytes(scope, args.get(1))?;
  let append = args.get(2).is_true();
  Some(Box::new(move || {
    let res = if append {
      std::fs::OpenOptions::new().create(true).append(true).open(&p).and_then(|mut f| f.write_all(&data))
    } else {
      std::fs::write(&p, &data)
    };
    res.map(|_| Payload::Undefined).map_err(|e| io_err(e, "open", p))
  }))
}

fn prep_exists(scope: &mut v8::PinScope, args: &Args) -> Option<Job> {
  let p = arg_string(scope, args, 0)?;
  Some(Box::new(move || Ok(Payload::Bool(std::fs::symlink_metadata(&p).is_ok()))))
}

fn prep_read_dir(scope: &mut v8::PinScope, args: &Args) -> Option<Job> {
  let p = arg_string(scope, args, 0)?;
  Some(Box::new(move || {
    let rd = std::fs::read_dir(&p).map_err(|e| io_err(e, "scandir", p))?;
    let entries = rd
      .flatten()
      .map(|e| {
        let ft = e.file_type().ok();
        (
          e.file_name().to_string_lossy().into_owned(),
          ft.is_some_and(|t| t.is_file()),
          ft.is_some_and(|t| t.is_dir()),
          ft.is_some_and(|t| t.is_symlink()),
        )
      })
      .collect();
    Ok(Payload::Dir(entries))
  }))
}

fn prep_mkdir(scope: &mut v8::PinScope, args: &Args) -> Option<Job> {
  let p = arg_string(scope, args, 0)?;
  let recursive = args.get(1).is_true();
  Some(Box::new(move || {
    let res = if recursive { std::fs::create_dir_all(&p) } else { std::fs::create_dir(&p) };
    res.map(|_| Payload::Undefined).map_err(|e| io_err(e, "mkdir", p))
  }))
}

fn prep_remove(scope: &mut v8::PinScope, args: &Args) -> Option<Job> {
  let p = arg_string(scope, args, 0)?;
  let recursive = args.get(1).is_true();
  Some(Box::new(move || {
    let res = match std::fs::symlink_metadata(&p) {
      Ok(m) if m.is_dir() => {
        if recursive { std::fs::remove_dir_all(&p) } else { std::fs::remove_dir(&p) }
      }
      Ok(_) => std::fs::remove_file(&p),
      Err(e) => Err(e),
    };
    res.map(|_| Payload::Undefined).map_err(|e| io_err(e, "rm", p))
  }))
}

fn prep_stat(scope: &mut v8::PinScope, args: &Args) -> Option<Job> {
  let p = arg_string(scope, args, 0)?;
  Some(Box::new(move || {
    let meta = std::fs::metadata(&p).map_err(|e| io_err(e, "stat", p.clone()))?;
    let is_link = std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink());
    let mtime_ms = meta
      .modified()
      .ok()
      .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
      .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
    Ok(Payload::Stat { size: meta.len(), is_file: meta.is_file(), is_dir: meta.is_dir(), is_link, mtime_ms })
  }))
}

fs_op!(op_read_text_file, op_read_text_file_async, prep_read_text);
fs_op!(op_read_file, op_read_file_async, prep_read);
fs_op!(op_write_file, op_write_file_async, prep_write);
fs_op!(op_exists, op_exists_async, prep_exists);
fs_op!(op_read_dir, op_read_dir_async, prep_read_dir);
fs_op!(op_mkdir, op_mkdir_async, prep_mkdir);
fs_op!(op_remove, op_remove_async, prep_remove);
fs_op!(op_stat, op_stat_async, prep_stat);

// -------------------------------------------------------------------- fetch

// fetch(url, method, [k, v, k, v...], body | undefined, redirect) ->
// Promise<[status, statusText, url, redirected, [k, v...], Uint8Array]>
op!(op_fetch(scope, args, rv) {
  let Some(url) = arg_string(scope, &args, 0) else { return };
  let Some(method) = arg_string(scope, &args, 1) else { return };
  let mut headers = Vec::new();
  if let Ok(arr) = v8::Local::<v8::Array>::try_from(args.get(2)) {
    let mut i = 0;
    while i + 1 < arr.length() {
      let k = arr.get_index(scope, i).map(|v| v.to_rust_string_lossy(scope)).unwrap_or_default();
      let v = arr.get_index(scope, i + 1).map(|v| v.to_rust_string_lossy(scope)).unwrap_or_default();
      headers.push((k, v));
      i += 2;
    }
  }
  let body_arg = args.get(3);
  let body = if body_arg.is_null_or_undefined() {
    None
  } else {
    let Some(b) = arg_bytes(scope, body_arg) else { return };
    Some(b)
  };
  let redirect = match args.get(4).uint32_value(scope).unwrap_or(0) {
    1 => sako_http::Redirect::Manual,
    2 => sako_http::Redirect::Error,
    _ => sako_http::Redirect::Follow,
  };
  sako_http::set_user_agent(format!("sako/{}", crate::VERSION));
  let req = sako_http::FetchRequest { url, method, headers, body, redirect };
  let p = pool::spawn(scope, move || {
    sako_http::fetch(req).map(|r| Payload::Response(Box::new(r))).map_err(OpError::Type)
  });
  rv.set(p.into());
});

// ---------------------------------------------------------------------- URL

fn url_parts<'s>(scope: &mut v8::PinScope<'s, '_>, u: &url::Url) -> v8::Local<'s, v8::Array> {
  use url::quirks as q;
  let origin = q::origin(u);
  let parts = [
    q::href(u),
    &origin,
    q::protocol(u),
    q::username(u),
    q::password(u),
    q::host(u),
    q::hostname(u),
    q::port(u),
    q::pathname(u),
    q::search(u),
    q::hash(u),
  ];
  let vals: Vec<v8::Local<v8::Value>> = parts.iter().map(|s| v8::String::new(scope, s).unwrap().into()).collect();
  v8::Array::new_with_elements(scope, &vals)
}

// urlParse(input, base?) -> parts array, or undefined if invalid.
op!(op_url_parse(scope, args, rv) {
  let input = args.get(0).to_rust_string_lossy(scope);
  let base = args.get(1);
  let parsed = if base.is_undefined() {
    url::Url::parse(&input)
  } else {
    let base = base.to_rust_string_lossy(scope);
    url::Url::parse(&base).and_then(|b| b.join(&input))
  };
  if let Ok(u) = parsed {
    let arr = url_parts(scope, &u);
    rv.set(arr.into());
  }
});

// urlHref(input) -> normalized href, or undefined if invalid. Cheaper than
// urlParse when only validation is needed (fetch/Request).
op!(op_url_href(scope, args, rv) {
  let input = args.get(0).to_rust_string_lossy(scope);
  if let Ok(u) = url::Url::parse(&input) {
    let s = v8::String::new(scope, u.as_str()).unwrap();
    rv.set(s.into());
  }
});

// urlSet(href, field, value) -> parts array (unchanged if the setter fails).
op!(op_url_set(scope, args, rv) {
  use url::quirks as q;
  let href = args.get(0).to_rust_string_lossy(scope);
  let field = args.get(1).int32_value(scope).unwrap_or(-1);
  let value = args.get(2).to_rust_string_lossy(scope);
  let Ok(mut u) = url::Url::parse(&href) else { return };
  match field {
    0 => {
      if q::set_href(&mut u, &value).is_err() {
        return throw_type_error(scope, &format!("Invalid URL: {value}"));
      }
    }
    2 => drop(q::set_protocol(&mut u, &value)),
    3 => drop(q::set_username(&mut u, &value)),
    4 => drop(q::set_password(&mut u, &value)),
    5 => drop(q::set_host(&mut u, &value)),
    6 => drop(q::set_hostname(&mut u, &value)),
    7 => drop(q::set_port(&mut u, &value)),
    8 => q::set_pathname(&mut u, &value),
    9 => q::set_search(&mut u, &value),
    10 => q::set_hash(&mut u, &value),
    _ => {}
  }
  let arr = url_parts(scope, &u);
  rv.set(arr.into());
});

// --------------------------------------------------------------------- HTTP

/// Delivers server requests to the event loop.
#[derive(Clone)]
struct EventSink(std::sync::mpsc::Sender<pool::Event>);

impl sako_http::RequestSink for EventSink {
  fn send(&self, request: sako_http::HttpRequest) -> bool {
    self.0.send(pool::Event::Http(Box::new(request))).is_ok()
  }
}

// serve(serverId, hostname, port, reusePort) -> bound port
op!(op_serve(scope, args, rv) {
  let id = args.get(0).uint32_value(scope).unwrap_or(0);
  let Some(hostname) = arg_string(scope, &args, 1) else { return };
  let port = args.get(2).uint32_value(scope).unwrap_or(0);
  let Ok(port) = u16::try_from(port) else {
    return throw(scope, OpError::Type(format!("Invalid port: {port}")));
  };
  let reuse_port = args.get(3).is_true();
  let events = EventSink(state(scope).borrow().pool.events());
  match sako_http::listen(id, &hostname, port, reuse_port, events) {
    Ok((port, handle)) => {
      state(scope).borrow_mut().servers.insert(id, handle);
      rv.set_uint32(port as u32);
    }
    Err(err) => throw(scope, io_err(err, "listen", format!("{hostname}:{port}"))),
  }
});

op!(op_server_stop(scope, args, _rv) {
  let id = args.get(0).uint32_value(scope).unwrap_or(0);
  if let Some(mut handle) = state(scope).borrow_mut().servers.remove(&id) {
    handle.stop_accepting();
  }
});

/// Reads (status, [k, v, ...] | undefined, body) starting at argument `i`.
fn arg_reply(scope: &mut v8::PinScope, args: &Args, i: i32) -> Option<sako_http::HttpReply> {
  let status = args.get(i).uint32_value(scope).unwrap_or(200) as u16;
  let mut headers = Vec::new();
  if let Ok(arr) = v8::Local::<v8::Array>::try_from(args.get(i + 1)) {
    let len = arr.length();
    headers.reserve(len as usize / 2);
    let mut i = 0;
    while i + 1 < len {
      let k = arr.get_index(scope, i).map(|v| v.to_rust_string_lossy(scope)).unwrap_or_default();
      let v = arr.get_index(scope, i + 1).map(|v| v.to_rust_string_lossy(scope)).unwrap_or_default();
      headers.push((k, v));
      i += 2;
    }
  }
  let body_arg = args.get(i + 2);
  let body = if body_arg.is_null_or_undefined() {
    Vec::new()
  } else if let Ok(s) = v8::Local::<v8::String>::try_from(body_arg) {
    let mut out = Vec::new();
    append_utf8(scope, s, &mut out);
    out
  } else {
    arg_bytes(scope, body_arg)?
  };
  Some(sako_http::HttpReply { status, headers, body })
}

// httpRespond(requestId, status, [k, v, ...] | undefined, body: string | ArrayBufferView | null)
op!(op_http_respond(scope, args, _rv) {
  let id = args.get(0).uint32_value(scope).unwrap_or(0);
  if let Some(reply) = arg_reply(scope, &args, 1) {
    crate::http_respond(scope, id, reply);
  }
});

// serveStatic(serverId, path, method | "", status, [k, v, ...] | undefined, body)
op!(op_serve_static(scope, args, _rv) {
  let id = args.get(0).uint32_value(scope).unwrap_or(0);
  let Some(path) = arg_string(scope, &args, 1) else { return };
  let method = args.get(2).to_rust_string_lossy(scope);
  let Some(reply) = arg_reply(scope, &args, 3) else { return };
  if let Some(server) = state(scope).borrow().servers.get(&id) {
    server.add_static(path, (!method.is_empty()).then_some(method.as_str()), reply);
  }
});

op!(op_serve_static_clear(scope, args, _rv) {
  let id = args.get(0).uint32_value(scope).unwrap_or(0);
  if let Some(server) = state(scope).borrow().servers.get(&id) {
    server.clear_static();
  }
});

// -------------------------------------------------------------------- shell

fn string_list(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> Vec<String> {
  let Ok(arr) = v8::Local::<v8::Array>::try_from(v) else { return Vec::new() };
  (0..arr.length()).map(|i| arr.get_index(scope, i).map(|v| v.to_rust_string_lossy(scope)).unwrap_or_default()).collect()
}

// shell(texts, values, cwd | undefined, [k, v, ...] | undefined, quiet)
//   -> [exitCode, stdout, stderr], or a Promise of it when the script may
//      spawn processes (it then runs on the thread pool).
op!(op_shell(scope, args, rv) {
  use crate::shell::{Job, Value};
  let mut values = Vec::new();
  if let Ok(arr) = v8::Local::<v8::Array>::try_from(args.get(1)) {
    for i in 0..arr.length() {
      let v = arr.get_index(scope, i).unwrap_or_else(|| v8::undefined(scope).into());
      values.push(if v.is_array() {
        Value::List(string_list(scope, v))
      } else if v.is_array_buffer_view() || v.is_array_buffer() {
        let Some(b) = arg_bytes(scope, v) else { return };
        Value::Bytes(std::sync::Arc::new(b))
      } else {
        Value::Str(v.to_rust_string_lossy(scope))
      });
    }
  }
  let st = state(scope);
  let cwd = match args.get(2) {
    v if v.is_string() => {
      let dir = std::path::PathBuf::from(v.to_rust_string_lossy(scope));
      if dir.is_absolute() { crate::shell::normalize_path(&dir) } else { crate::shell::normalize_path(&process_cwd(&st).join(dir)) }
    }
    _ => process_cwd(&st),
  };
  let env = if args.get(3).is_array() {
    let flat = string_list(scope, args.get(3));
    Some(flat.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect())
  } else {
    None
  };
  let quiet = args.get(4).is_true();
  // The script is a cached parse id (from shellParse) or the template texts.
  let script = if let Some(id) = args.get(0).uint32_value(scope).filter(|_| args.get(0).is_number()) {
    match st.borrow().shell_scripts.get(id as usize) {
      Some(s) => s.clone(),
      None => return throw(scope, OpError::Plain("Sako.$: unknown script".into())),
    }
  } else {
    let texts = string_list(scope, args.get(0));
    match crate::shell::parse_script(&texts, values.len()) {
      Ok(s) => s,
      Err(msg) => return throw(scope, OpError::Plain(format!("Sako.$: {msg}"))),
    }
  };
  let job = Job::new(script, values, cwd, env);
  if job.can_inline() {
    let out = job.run(false);
    if !quiet {
      let mut s = st.borrow_mut();
      if !out.stdout.is_empty() {
        s.out.extend_from_slice(&out.stdout);
        if s.stdout_tty || s.out.len() >= crate::OUT_BUF_LIMIT {
          s.flush();
        }
      }
      if !out.stderr.is_empty() {
        s.flush();
        let _ = std::io::stderr().write_all(&out.stderr);
      }
    }
    let arr = pool::shell_result(scope, out);
    rv.set(arr.into());
  } else {
    if !quiet {
      // Keep ordering with earlier console output; the job prints live.
      st.borrow_mut().flush();
    }
    let p = pool::spawn(scope, move || Ok(Payload::Shell(job.run(!quiet))));
    rv.set(p.into());
  }
});

// shellParse(texts, valueCount) -> script id, for reuse by one call site.
op!(op_shell_parse(scope, args, rv) {
  let texts = string_list(scope, args.get(0));
  let nvals = args.get(1).uint32_value(scope).unwrap_or(0) as usize;
  match crate::shell::parse_script(&texts, nvals) {
    Ok(s) => {
      let st = state(scope);
      let mut st = st.borrow_mut();
      st.shell_scripts.push(s);
      rv.set_uint32((st.shell_scripts.len() - 1) as u32);
    }
    Err(msg) => throw(scope, OpError::Plain(format!("Sako.$: {msg}"))),
  }
});

fn process_cwd(st: &crate::StateRc) -> std::path::PathBuf {
  let mut s = st.borrow_mut();
  s.cwd_cache.get_or_insert_with(|| std::env::current_dir().unwrap_or_default()).clone()
}

// mapStack(stack) -> stack with TypeScript positions restored.
op!(op_map_stack(scope, args, rv) {
  let stack = args.get(0).to_rust_string_lossy(scope);
  let mapped = crate::map_stack(&state(scope).borrow(), &stack);
  let v = v8::String::new(scope, &mapped).unwrap();
  rv.set(v.into());
});

op!(op_shell_escape(scope, args, rv) {
  let s = args.get(0).to_rust_string_lossy(scope);
  let v = v8::String::new(scope, &crate::shell::escape(&s)).unwrap();
  rv.set(v.into());
});

// --------------------------------------------------------------- PostgreSQL

// pgOpen(url | undefined, max | undefined) -> pool id. No connection is made
// until the first query.
op!(op_pg_open(scope, args, rv) {
  let url = args.get(0);
  let config = if url.is_string() {
    sako_pg::Config::parse(&url.to_rust_string_lossy(scope))
  } else {
    let env = ["DATABASE_URL", "POSTGRES_URL", "PGURL"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
    match env {
      Some(u) => sako_pg::Config::parse(&u),
      None => Ok(sako_pg::Config::from_env()),
    }
  };
  let mut config = match config {
    Ok(c) => c,
    Err(e) => return throw(scope, OpError::Pg(e)),
  };
  if let Some(max) = args.get(1).uint32_value(scope).filter(|_| args.get(1).is_number()) {
    config.max = (max as usize).max(1);
  }
  let st = state(scope);
  let mut s = st.borrow_mut();
  s.pg_next += 1;
  let id = s.pg_next;
  let pool = sako_pg::Pool::new(config);
  s.pg_pools.insert(id, std::sync::Arc::new(PgPool { pool, queue: Default::default() }));
  rv.set_uint32(id);
});

/// A `Sako.sql` pool: queries queue here; up to `max` drainer jobs each hold
/// a connection and send whatever has queued up as one pipelined batch.
pub struct PgPool {
  pool: std::sync::Arc<sako_pg::Pool>,
  queue: std::sync::Mutex<PgQueue>,
}

#[derive(Default)]
struct PgQueue {
  jobs: std::collections::VecDeque<PgJob>,
  drainers: usize,
}

struct PgJob {
  id: u32,
  sql: String,
  params: Vec<sako_pg::Param>,
  types: Vec<u32>,
  simple: bool,
  values: bool,
}

const PG_BATCH: usize = 64;

fn pg_drain(pg: std::sync::Arc<PgPool>, events: std::sync::mpsc::Sender<pool::Event>) {
  let send = |job: &PgJob, r: Result<sako_pg::QueryResult, sako_pg::Error>| {
    let _ = events.send(pool::Event::Op(job.id, r.map(|r| Payload::Pg(Box::new(r), job.values)).map_err(OpError::Pg)));
  };
  loop {
    let mut conn = match pg.pool.get() {
      Ok(c) => c,
      Err(e) => {
        // Can't connect: fail what is queued now and stop.
        let mut q = pg.queue.lock().unwrap();
        q.drainers -= 1;
        let jobs: Vec<PgJob> = q.jobs.drain(..).collect();
        drop(q);
        for job in &jobs {
          send(job, Err(sako_pg::Error::Io(e.to_string())));
        }
        return;
      }
    };
    loop {
      // Take a batch; simple-protocol queries run on their own.
      let batch: Vec<PgJob> = {
        let mut q = pg.queue.lock().unwrap();
        if q.jobs.is_empty() {
          q.drainers -= 1;
          return; // connection goes back to the pool
        }
        let mut batch = Vec::new();
        while batch.len() < PG_BATCH {
          match q.jobs.front() {
            Some(j) if j.simple && !batch.is_empty() => break,
            Some(_) => {
              let job = q.jobs.pop_front().unwrap();
              let simple = job.simple;
              batch.push(job);
              if simple {
                break;
              }
            }
            None => break,
          }
        }
        batch
      };
      if batch[0].simple {
        let r = conn.simple_query(&batch[0].sql);
        let broken = conn.broken;
        send(&batch[0], r);
        if broken {
          break;
        }
        continue;
      }
      let queries: Vec<sako_pg::BatchQuery> =
        batch.iter().map(|j| sako_pg::BatchQuery { sql: &j.sql, params: &j.params, types: &j.types }).collect();
      match conn.query_batch(&queries) {
        Ok(results) => {
          for (job, r) in batch.iter().zip(results) {
            send(job, r);
          }
        }
        Err(e) => {
          for job in &batch {
            send(job, Err(sako_pg::Error::Io(e.to_string())));
          }
          break; // broken connection: get a new one
        }
      }
    }
  }
}

/// Query parameters: null/undefined -> NULL, Uint8Array -> bytea hex,
/// anything else -> its string form (JS has already stringified).
fn pg_params(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> Option<Vec<sako_pg::Param>> {
  let arr = v8::Local::<v8::Array>::try_from(v).ok()?;
  let mut out = Vec::with_capacity(arr.length() as usize);
  for i in 0..arr.length() {
    let v = arr.get_index(scope, i)?;
    out.push(if v.is_null_or_undefined() {
      None
    } else if v.is_array_buffer_view() || v.is_array_buffer() {
      let bytes = arg_bytes(scope, v)?;
      let mut hex = String::with_capacity(2 + bytes.len() * 2);
      hex.push_str("\\x");
      for b in bytes {
        hex.push_str(&format!("{b:02x}"));
      }
      Some(hex)
    } else {
      Some(v.to_rust_string_lossy(scope))
    });
  }
  Some(out)
}

// pgQuery(poolId, txId, sql, params, simple, values, types) -> Promise<[rows, command, count]>
op!(op_pg_query(scope, args, rv) {
  let pool_id = args.get(0).uint32_value(scope).unwrap_or(0);
  let tx_id = args.get(1).uint32_value(scope).unwrap_or(0);
  let sql = args.get(2).to_rust_string_lossy(scope);
  let Some(params) = pg_params(scope, args.get(3)) else {
    return throw_type_error(scope, "invalid query parameters");
  };
  let simple = args.get(4).is_true();
  let values = args.get(5).is_true();
  let types: Vec<u32> = match v8::Local::<v8::Array>::try_from(args.get(6)) {
    Ok(arr) => (0..arr.length()).map(|i| arr.get_index(scope, i).and_then(|v| v.uint32_value(scope)).unwrap_or(0)).collect(),
    Err(_) => Vec::new(),
  };
  let st = state(scope);
  if tx_id != 0 {
    // Transactions use their reserved connection directly.
    let Some(tx) = st.borrow().pg_tx.get(&tx_id).cloned() else {
      return throw(scope, OpError::Plain("transaction already finished".into()));
    };
    let p = pool::spawn(scope, move || {
      let mut conn = tx.lock().unwrap();
      let r = if simple { conn.simple_query(&sql) } else { conn.query_typed(&sql, &params, &types) };
      r.map(|r| Payload::Pg(Box::new(r), values)).map_err(OpError::Pg)
    });
    return rv.set(p.into());
  }
  let Some(pg) = st.borrow().pg_pools.get(&pool_id).cloned() else {
    return throw(scope, OpError::Pg(sako_pg::Error::PoolClosed));
  };
  let (id, promise, events) = pool::pending(scope);
  let start = {
    let mut q = pg.queue.lock().unwrap();
    q.jobs.push_back(PgJob { id, sql, params, types, simple, values });
    let start = q.drainers < pg.pool.config().max;
    if start {
      q.drainers += 1;
    }
    start
  };
  if start {
    pool::submit_detached(scope, move || pg_drain(pg, events));
  }
  rv.set(promise.into());
});

// pgBegin(poolId, beginSql) -> Promise<txId>: reserves a connection.
op!(op_pg_begin(scope, args, rv) {
  let pool_id = args.get(0).uint32_value(scope).unwrap_or(0);
  let begin = args.get(1).to_rust_string_lossy(scope);
  let Some(pg) = state(scope).borrow().pg_pools.get(&pool_id).cloned() else {
    return throw(scope, OpError::Pg(sako_pg::Error::PoolClosed));
  };
  let p = pool::spawn(scope, move || {
    let mut conn = pg.pool.get().map_err(OpError::Pg)?;
    conn.simple_query(&begin).map_err(OpError::Pg)?;
    Ok(Payload::PgTransaction(Box::new(conn)))
  });
  rv.set(p.into());
});

// pgFinish(txId, commit) -> Promise: COMMIT or ROLLBACK, then release.
op!(op_pg_finish(scope, args, rv) {
  let tx_id = args.get(0).uint32_value(scope).unwrap_or(0);
  let commit = args.get(1).is_true();
  let Some(conn) = state(scope).borrow_mut().pg_tx.remove(&tx_id) else {
    return throw(scope, OpError::Plain("transaction already finished".into()));
  };
  let p = pool::spawn(scope, move || {
    let mut conn = conn.lock().unwrap();
    let r = conn.simple_query(if commit { "COMMIT" } else { "ROLLBACK" });
    r.map(|_| Payload::Undefined).map_err(OpError::Pg)
  });
  rv.set(p.into());
});

// pgClose(poolId) -> Promise: closes idle connections, refuses new queries.
op!(op_pg_close(scope, args, rv) {
  let pool_id = args.get(0).uint32_value(scope).unwrap_or(0);
  let pg = state(scope).borrow_mut().pg_pools.remove(&pool_id);
  let p = pool::spawn(scope, move || {
    if let Some(pg) = pg {
      pg.pool.close();
    }
    Ok(Payload::Undefined)
  });
  rv.set(p.into());
});
