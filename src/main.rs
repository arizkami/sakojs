//! Sako: a small, fast JavaScript runtime on top of V8 (via Deno's rusty_v8
//! prebuilt static library).
//!
//! Design notes for speed:
//! - No snapshot rebuild, no TypeScript, no Node compat layer: startup is
//!   isolate creation (V8's own built-in snapshot) plus one small bootstrap.
//! - ES modules are compiled with V8 code cache stored on disk, so repeat runs
//!   skip parsing/bytecode generation.
//! - stdout is buffered in-process and written with few syscalls.
//! - The event loop is a binary heap of timers driven from Rust; callbacks
//!   live on the JS side so there are no per-timer `Global` handles.

mod ops;
mod pool;
mod shell;

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::hash::{Hash, Hasher};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

// mimalloc handles the cross-thread frees between the JS thread and the I/O
// threads far better than glibc malloc (whose arena locks showed up hot).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// JS runtime library, generated from crates/sako_jslib/js/*.js. It is loaded
// as an external one-byte (Latin-1) string; sako_jslib guarantees ASCII.
use sako_jslib::BOOTSTRAP;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const OUT_BUF_LIMIT: usize = 64 * 1024;

pub struct State {
  pub start: Instant,
  pub out: Vec<u8>,
  pub stdout_tty: bool,
  pub stderr_tty: bool,
  modules: HashMap<PathBuf, v8::Global<v8::Module>>,
  by_hash: HashMap<i32, Vec<(v8::Global<v8::Module>, PathBuf)>>,
  pending_cache: Vec<(v8::Global<v8::Module>, PathBuf)>,
  cache_dir: Option<PathBuf>,
  main_path: Option<PathBuf>,
  pub timers: BinaryHeap<Reverse<(Instant, u64, u32)>>,
  /// id -> (seq, refed). Unref'd timers don't keep the process alive.
  pub active_timers: HashMap<u32, (u64, bool)>,
  pub timer_seq: u64,
  pub dispatch: Option<v8::Global<v8::Function>>,
  pub inspect: Option<v8::Global<v8::Function>>,
  pub import_helper: Option<v8::Global<v8::Function>>,
  pub run_ticks: Option<v8::Global<v8::Function>>,
  pub ticks_pending: bool,
  pub start_immediates: Option<v8::Global<v8::Function>>,
  pub run_immediate: Option<v8::Global<v8::Function>>,
  pub immediates_pending: bool,
  rejections: Vec<(v8::Global<v8::Promise>, v8::Global<v8::Value>)>,
  pub argv: Vec<String>,
  pub script_args: Vec<String>,
  pub pool: pool::Pool,
  /// Parsed `Sako.$` scripts by id (one per template call site).
  pub shell_scripts: Vec<shell::Parsed>,
  /// Cached process working directory (reset by `process.chdir`).
  pub cwd_cache: Option<PathBuf>,
  /// Source maps of loaded TypeScript modules, for stack traces.
  pub ts_maps: HashMap<PathBuf, sako_typescript::LineMap>,
  /// `Sako.sql` connection pools and open transactions, by id.
  pub pg_pools: HashMap<u32, std::sync::Arc<ops::PgPool>>,
  pub pg_tx: HashMap<u32, std::sync::Arc<std::sync::Mutex<sako_pg::Pooled>>>,
  pub pg_next: u32,
  pub servers: HashMap<u32, sako_http::ServerHandle>,
  http_pending: HashMap<u32, sako_http::ReplyTo>,
  /// Replies produced during this turn, delivered together by `flush_http`.
  http_replies: Vec<(sako_http::ReplyTo, sako_http::HttpReply)>,
  http_next_id: u32,
  pub http_dispatch: Option<v8::Global<v8::Function>>,
  pub enable_ts_stacks: Option<v8::Global<v8::Function>>,
}

pub type StateRc = Rc<RefCell<State>>;

impl State {
  fn new(
    cache_dir: Option<PathBuf>,
    main_path: Option<PathBuf>,
    argv: Vec<String>,
    script_args: Vec<String>,
  ) -> Self {
    State {
      start: Instant::now(),
      out: Vec::with_capacity(OUT_BUF_LIMIT),
      stdout_tty: std::io::stdout().is_terminal(),
      stderr_tty: std::io::stderr().is_terminal(),
      modules: HashMap::new(),
      by_hash: HashMap::new(),
      pending_cache: Vec::new(),
      cache_dir,
      main_path,
      timers: BinaryHeap::new(),
      active_timers: HashMap::new(),
      timer_seq: 0,
      dispatch: None,
      inspect: None,
      import_helper: None,
      run_ticks: None,
      ticks_pending: false,
      start_immediates: None,
      run_immediate: None,
      immediates_pending: false,
      rejections: Vec::new(),
      argv,
      script_args,
      pool: pool::Pool::new(),
      shell_scripts: Vec::new(),
      cwd_cache: None,
      ts_maps: HashMap::new(),
      pg_pools: HashMap::new(),
      pg_tx: HashMap::new(),
      pg_next: 0,
      servers: HashMap::new(),
      http_pending: HashMap::new(),
      http_replies: Vec::new(),
      http_next_id: 0,
      http_dispatch: None,
      enable_ts_stacks: None,
    }
  }

  pub fn flush(&mut self) {
    if !self.out.is_empty() {
      let mut so = std::io::stdout().lock();
      let _ = so.write_all(&self.out);
      let _ = so.flush();
      self.out.clear();
    }
  }
}

pub fn state(scope: &v8::PinScope) -> StateRc {
  scope.get_slot::<StateRc>().expect("runtime state").clone()
}

struct Options {
  file: Option<String>,
  eval: Option<String>,
  print: bool,
  no_cache: bool,
  v8_flags: Vec<String>,
  script_args: Vec<String>,
}

fn usage() -> ! {
  println!(
    "sako {VERSION} - a fast JavaScript runtime (V8 {})

Usage:
  sako [options] <file.js|file.ts> [args...]
  sako -e <code>           evaluate code as an ES module
  sako -p <expr>           evaluate expression and print the result
  sako                     start a REPL (or run stdin when piped)
  sako install [--production]      install package.json dependencies
  sako add [-D] <pkg[@range]>...   add dependencies (devDependencies with -D)
  sako remove <pkg>...             remove dependencies

Options:
  --no-cache               disable the on-disk V8 code cache
  --v8-flags=<flags>       pass flags to V8 (comma separated)
  -v, --version            print version
  -h, --help               print this help

Environment:
  SAKO_CACHE_DIR           code cache location (default ~/.cache/sako)
  SAKO_NO_CACHE=1          same as --no-cache",
    v8::V8::get_version()
  );
  std::process::exit(0)
}

fn parse_args() -> Options {
  let mut opts = Options {
    file: None,
    eval: None,
    print: false,
    no_cache: std::env::var_os("SAKO_NO_CACHE").is_some_and(|v| v != "0" && !v.is_empty()),
    v8_flags: Vec::new(),
    script_args: Vec::new(),
  };
  let mut args = std::env::args().skip(1);
  while let Some(a) = args.next() {
    match a.as_str() {
      "-h" | "--help" => usage(),
      "-v" | "--version" => {
        println!("sako {VERSION}\nv8 {}", v8::V8::get_version());
        std::process::exit(0);
      }
      "-e" | "--eval" | "-p" | "--print" => {
        opts.print = a == "-p" || a == "--print";
        opts.eval = Some(args.next().unwrap_or_else(|| fatal(&format!("{a} requires an argument"))));
        opts.script_args.extend(args.by_ref().skip_while(|a| a == "--"));
      }
      "--no-cache" => opts.no_cache = true,
      "--" => {
        if opts.file.is_none() && opts.eval.is_none() {
          opts.file = args.next();
        }
        opts.script_args.extend(args.by_ref());
      }
      _ if a.starts_with("--v8-flags=") => {
        opts.v8_flags.extend(a["--v8-flags=".len()..].split(',').filter(|s| !s.is_empty()).map(|s| {
          if s.starts_with("--") { s.to_string() } else { format!("--{s}") }
        }));
      }
      _ if a.starts_with('-') && a.len() > 1 && opts.file.is_none() => {
        fatal(&format!("unknown option: {a}"));
      }
      _ => {
        opts.file = Some(a);
        opts.script_args.extend(args.by_ref());
      }
    }
  }
  opts
}

/// `sako install` / `add` / `remove`.
fn package_command(cmd: &str, args: &[String]) -> i32 {
  let start = Instant::now();
  let dir = std::env::current_dir().unwrap_or_default();
  let production = args.iter().any(|a| a == "--production" || a == "-p");
  let dev = args.iter().any(|a| a == "-D" || a == "--dev" || a == "--save-dev");
  let names: Vec<String> = args.iter().filter(|a| !a.starts_with('-')).cloned().collect();
  let opts = sako_npm::Options { dir, production };
  println!("sako {cmd} v{VERSION}");
  let result = match cmd {
    "add" if names.is_empty() => Err("usage: sako add [-D] <package[@range]>...".to_string()),
    "remove" | "rm" if names.is_empty() => Err("usage: sako remove <package>...".to_string()),
    "add" => sako_npm::add(&opts, &names, dev),
    "remove" | "rm" => sako_npm::remove(&opts, &names),
    _ => sako_npm::install(&opts),
  };
  match result {
    Ok(s) => {
      for w in &s.warnings {
        eprintln!("warn: {w}");
      }
      if s.up_to_date {
        println!("\nChecked {} packages: node_modules is up to date [{:.0}ms]", s.packages, start.elapsed().as_secs_f64() * 1000.0);
        return 0;
      }
      println!();
      for p in &s.top_level {
        println!("+ {p}");
      }
      let source = if s.from_lockfile { ", from sako.lock" } else { "" };
      println!(
        "\n{} packages installed ({} downloaded{source}) [{:.0}ms]",
        s.packages,
        s.downloaded,
        start.elapsed().as_secs_f64() * 1000.0
      );
      if !s.scripts_skipped.is_empty() {
        println!(
          "note: install scripts were not run for: {}",
          s.scripts_skipped.join(", ")
        );
      }
      0
    }
    Err(e) => {
      eprintln!("error: {e}");
      1
    }
  }
}

fn fatal(msg: &str) -> ! {
  eprintln!("error: {msg}");
  std::process::exit(1)
}

fn cache_dir(no_cache: bool) -> Option<PathBuf> {
  if no_cache {
    return None;
  }
  let dir = if let Some(d) = std::env::var_os("SAKO_CACHE_DIR") {
    PathBuf::from(d)
  } else if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
    PathBuf::from(d).join("sako")
  } else {
    PathBuf::from(std::env::var_os("HOME")?).join(".cache").join("sako")
  };
  // Code cache is only valid for the exact V8 build + flags.
  Some(dir.join(format!("v8-{:08x}", v8::script_compiler::cached_data_version_tag())))
}

fn main() {
  // Package manager subcommands don't need V8.
  let args: Vec<String> = std::env::args().skip(1).collect();
  if let Some(cmd) = args.first().map(String::as_str)
    && matches!(cmd, "install" | "i" | "add" | "remove" | "rm")
  {
    std::process::exit(package_command(cmd, &args[1..]));
  }
  let opts = parse_args();

  let mut flags = String::from("--harmony-import-attributes ");
  for f in &opts.v8_flags {
    flags.push_str(f);
    flags.push(' ');
  }
  if let Ok(f) = std::env::var("SAKO_V8_FLAGS") {
    flags.push_str(&f);
  }
  v8::V8::set_flags_from_string(&flags);
  let platform = v8::new_default_platform(0, false).make_shared();
  v8::V8::initialize_platform(platform.clone());
  v8::V8::initialize();

  let code = run(opts, &platform);
  std::process::exit(code);
}

fn run(opts: Options, platform: &v8::SharedRef<v8::Platform>) -> i32 {
  let isolate = &mut v8::Isolate::new(v8::CreateParams::default());
  isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
  isolate.set_host_import_module_dynamically_callback(dynamic_import_cb);
  isolate.set_host_initialize_import_meta_object_callback(import_meta_cb);
  isolate.set_promise_reject_callback(promise_reject_cb);
  isolate.set_capture_stack_trace_for_uncaught_exceptions(true, 10);

  let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
  let (main_path, main_source): (PathBuf, Option<String>) = match (&opts.eval, &opts.file) {
    (Some(code), _) => {
      let code = if opts.print {
        format!("const __sako_r = await ({code}\n);\nconsole.log(typeof __sako_r === 'string' ? __sako_r : Sako.inspect(__sako_r));")
      } else {
        code.clone()
      };
      (cwd.join("[eval]"), Some(code))
    }
    (None, Some(file)) => {
      let p = resolve_path(file, &cwd).unwrap_or_else(|e| fatal(&e));
      (p, None)
    }
    (None, None) => {
      if std::io::stdin().is_terminal() {
        return repl(isolate, platform);
      }
      let mut src = String::new();
      let _ = std::io::stdin().read_to_string(&mut src);
      (cwd.join("[stdin]"), Some(src))
    }
  };

  let mut argv = vec![std::env::current_exe().map(|p| p.display().to_string()).unwrap_or("sako".into())];
  if main_source.is_none() {
    argv.push(main_path.display().to_string());
  }
  argv.extend(opts.script_args.iter().cloned());

  let state = Rc::new(RefCell::new(State::new(
    cache_dir(opts.no_cache),
    Some(main_path.clone()),
    argv,
    opts.script_args.clone(),
  )));
  isolate.set_slot(state.clone());

  v8::scope!(let scope, isolate);
  let context = v8::Context::new(scope, Default::default());
  let scope = &mut v8::ContextScope::new(scope, context);

  if !bootstrap(scope) {
    return 1;
  }

  // Load, link and evaluate the entry module.
  let promise = {
    v8::tc_scope!(let tc, scope);
    let module = match main_source {
      Some(src) => compile_module(tc, &main_path, src, false),
      None => load_module(tc, &main_path),
    };
    let Some(module) = module else {
      report_exception(tc);
      return finish(tc, 1);
    };
    if module.instantiate_module(tc, resolve_cb).is_none() {
      report_exception(tc);
      return finish(tc, 1);
    }
    let Some(result) = module.evaluate(tc) else {
      report_exception(tc);
      return finish(tc, 1);
    };
    match v8::Local::<v8::Promise>::try_from(result) {
      Ok(p) => v8::Global::new(tc, p),
      Err(_) => {
        let r = v8::PromiseResolver::new(tc).unwrap();
        r.resolve(tc, result);
        v8::Global::new(tc, r.get_promise(tc))
      }
    }
  };

  let code = event_loop(scope, platform, Some(&promise));
  finish(scope, code)
}

/// Flush output, persist code caches and pick the exit code.
fn finish(scope: &mut v8::PinScope, code: i32) -> i32 {
  write_code_caches(scope);
  let st = state(scope);
  st.borrow_mut().flush();
  if code != 0 {
    return code;
  }
  exit_code_from_js(scope)
}

fn exit_code_from_js(scope: &mut v8::PinScope) -> i32 {
  let global = scope.get_current_context().global(scope);
  let key = v8::String::new(scope, "process").unwrap();
  let Some(process) = global.get(scope, key.into()).and_then(|v| v8::Local::<v8::Object>::try_from(v).ok()) else {
    return 0;
  };
  let key = v8::String::new(scope, "exitCode").unwrap();
  process
    .get(scope, key.into())
    .and_then(|v| v.int32_value(scope))
    .unwrap_or(0)
}

fn bootstrap(scope: &mut v8::PinScope) -> bool {
  v8::tc_scope!(let tc, scope);
  let source = v8::String::new_external_onebyte_static(tc, BOOTSTRAP.as_bytes()).unwrap();
  let name = v8::String::new(tc, "sako:bootstrap").unwrap();
  let origin = v8::ScriptOrigin::new(tc, name.into(), 0, 0, false, 0, None, false, false, false, None);
  let f = v8::Script::compile(tc, source, Some(&origin))
    .and_then(|s| s.run(tc))
    .and_then(|f| v8::Local::<v8::Function>::try_from(f).ok());
  let Some(f) = f else {
    report_exception(tc);
    return false;
  };
  let ops = ops::create(tc);
  let undefined = v8::undefined(tc).into();
  if f.call(tc, undefined, &[ops.into()]).is_none() {
    report_exception(tc);
    return false;
  }
  true
}

/// Drains the nextTick queue and microtasks until both are empty. Returns
/// an exit code if a nextTick callback threw.
#[must_use]
fn run_microtasks(scope: &mut v8::PinScope) -> Option<i32> {
  let st = state(scope);
  loop {
    let ticks = {
      let mut s = st.borrow_mut();
      std::mem::take(&mut s.ticks_pending).then(|| s.run_ticks.clone()).flatten()
    };
    if let Some(run_ticks) = ticks {
      v8::tc_scope!(let tc, scope);
      let f = v8::Local::new(tc, run_ticks);
      let undefined = v8::undefined(tc).into();
      if f.call(tc, undefined, &[]).is_none() {
        report_exception(tc);
        return Some(1);
      }
    }
    scope.perform_microtask_checkpoint();
    if !st.borrow().ticks_pending {
      return None;
    }
  }
}

/// Runs timers, microtasks and V8 platform tasks until nothing is left.
/// Returns a process exit code.
fn event_loop(
  outer: &mut v8::PinScope,
  platform: &v8::SharedRef<v8::Platform>,
  main: Option<&v8::Global<v8::Promise>>,
) -> i32 {
  let st = state(outer);
  loop {
    // Fresh handle scope per turn: handles created while dispatching timers,
    // op results and HTTP batches must not outlive the turn, or they pin
    // garbage for the life of the process.
    v8::scope!(let scope, outer);
    if let Some(code) = run_microtasks(scope) {
      return code;
    }
    while v8::Platform::pump_message_loop(platform, scope, false) {
      if let Some(code) = run_microtasks(scope) {
        return code;
      }
    }
    if let Some(code) = check_errors(scope, main) {
      return code;
    }

    // Fire every timer that is due, draining microtasks between each one.
    let now = Instant::now();
    let mut fired = false;
    loop {
      let id = {
        let mut s = st.borrow_mut();
        match s.timers.peek() {
          Some(Reverse((deadline, seq, id))) if *deadline <= now => {
            let (seq, id) = (*seq, *id);
            s.timers.pop();
            if s.active_timers.get(&id).map(|t| t.0) != Some(seq) {
              continue;
            }
            s.active_timers.remove(&id);
            id
          }
          _ => break,
        }
      };
      fired = true;
      let dispatch = st.borrow().dispatch.clone().expect("timer dispatcher");
      v8::tc_scope!(let tc, scope);
      let f = v8::Local::new(tc, dispatch);
      let undefined = v8::undefined(tc).into();
      let arg = v8::Integer::new_from_unsigned(tc, id).into();
      if f.call(tc, undefined, &[arg]).is_none() {
        if tc.has_terminated() {
          return 1;
        }
        report_exception(tc);
        return 1;
      }
      if let Some(code) = run_microtasks(tc) {
        return code;
      }
      if let Some(code) = check_errors(tc, main) {
        return code;
      }
    }

    // Settle finished async ops and dispatch arrived HTTP requests.
    let events: Vec<pool::Event> = {
      let s = st.borrow();
      std::iter::from_fn(|| s.pool.done_rx.try_recv().ok()).take(4096).collect()
    };
    if !events.is_empty() {
      fired = true;
      if let Some(code) = handle_events(scope, events, main) {
        return code;
      }
    }
    flush_http(&st);
    // Check phase: run the immediates queued before this point.
    if std::mem::take(&mut st.borrow_mut().immediates_pending) {
      if let Some(code) = run_immediates(scope, main) {
        return code;
      }
      fired = true;
    }
    if fired {
      continue;
    }

    // Nothing ready: wait for the next timer or op completion, or exit if idle.
    let next = {
      let mut s = st.borrow_mut();
      loop {
        match s.timers.peek() {
          Some(Reverse((_, seq, id))) if s.active_timers.get(id).map(|t| t.0) != Some(*seq) => {
            s.timers.pop();
          }
          Some(Reverse((deadline, _, _))) => break Some(*deadline),
          None => break None,
        }
      }
    };
    flush_http(&st);
    let pending_ops = st.borrow().pool.pending();
    let pending_bg = scope.has_pending_background_tasks();
    let refed_timers = st.borrow().active_timers.values().any(|t| t.1);
    let serving = !st.borrow().servers.is_empty();
    if !refed_timers && pending_ops == 0 && !pending_bg && !serving {
      break;
    }
    st.borrow_mut().flush();
    let mut timeout = next.map(|d| d.saturating_duration_since(Instant::now()));
    if pending_bg {
      // V8 background work (e.g. async wasm compile) posts foreground tasks
      // we must pump, so poll at a short interval.
      timeout = Some(timeout.map_or(Duration::from_millis(1), |t| t.min(Duration::from_millis(1))));
    }
    if pending_ops > 0 || serving {
      let event = st.borrow().pool.wait(timeout);
      if let Some(event) = event
        && let Some(code) = handle_events(scope, vec![event], main)
      {
        return code;
      }
    } else if let Some(t) = timeout {
      std::thread::sleep(t);
    }
  }

  if let Some(main) = main {
    let p = v8::Local::new(outer, main);
    if p.state() == v8::PromiseState::Pending {
      st.borrow_mut().flush();
      eprintln!("error: Top-level await promise never resolved");
      return 13;
    }
  }
  0
}

fn handle_events(scope: &mut v8::PinScope, events: Vec<pool::Event>, main: Option<&v8::Global<v8::Promise>>) -> Option<i32> {
  let mut batch = Vec::new();
  for event in events {
    match event {
      pool::Event::Op(id, result) => {
        pool::complete(scope, id, result);
        if let Some(code) = run_microtasks(scope) {
          return Some(code);
        }
        if let Some(code) = check_errors(scope, main) {
          return Some(code);
        }
      }
      pool::Event::Http(req) => batch.push(req),
    }
  }
  if !batch.is_empty() {
    if let Some(code) = dispatch_http(scope, batch) {
      return Some(code);
    }
    if let Some(code) = run_microtasks(scope) {
      return Some(code);
    }
    if let Some(code) = check_errors(scope, main) {
      return Some(code);
    }
  }
  None
}

/// Hands a batch of requests to JS in one call. The batch is a flat array of
/// [serverId, requestId, method, url, [k, v, ...], body | null] records.
fn dispatch_http(scope: &mut v8::PinScope, batch: Vec<Box<sako_http::HttpRequest>>) -> Option<i32> {
  let st = state(scope);
  let dispatch = st.borrow().http_dispatch.clone()?;
  let mut items: Vec<v8::Local<v8::Value>> = Vec::with_capacity(batch.len() * 6);
  for req in batch {
    let req = *req;
    let id = {
      let mut s = st.borrow_mut();
      s.http_next_id = s.http_next_id.wrapping_add(1);
      let id = s.http_next_id;
      s.http_pending.insert(id, req.reply);
      id
    };
    items.push(v8::Integer::new_from_unsigned(scope, req.server).into());
    items.push(v8::Integer::new_from_unsigned(scope, id).into());
    items.push(v8::String::new(scope, req.method.as_str()).unwrap().into());
    items.push(pool::new_string(scope, req.url.as_bytes()).unwrap().into());
    let mut headers = Vec::with_capacity(req.headers.len() * 2);
    for (k, v) in &req.headers {
      headers.push(v8::String::new_from_one_byte(scope, k.as_str().as_bytes(), v8::NewStringType::Normal).unwrap().into());
      headers.push(pool::new_string(scope, v.as_bytes()).unwrap().into());
    }
    items.push(v8::Array::new_with_elements(scope, &headers).into());
    items.push(if req.body.is_empty() {
      v8::null(scope).into()
    } else {
      pool::bytes_to_u8array(scope, req.body.to_vec()).into()
    });
  }
  let arr = v8::Array::new_with_elements(scope, &items);
  v8::tc_scope!(let tc, scope);
  let f = v8::Local::new(tc, dispatch);
  let undefined = v8::undefined(tc).into();
  if f.call(tc, undefined, &[arr.into()]).is_none() {
    report_exception(tc);
    return Some(1);
  }
  None
}

/// Queues a response for a request handed out by `dispatch_http`.
pub fn http_respond(scope: &v8::PinScope, id: u32, reply: sako_http::HttpReply) -> bool {
  let st = state(scope);
  let mut s = st.borrow_mut();
  match s.http_pending.remove(&id) {
    Some(tx) => {
      s.http_replies.push((tx, reply));
      true
    }
    None => false,
  }
}

/// Delivers queued responses to the I/O threads' reply queues. A burst of
/// sends wakes each I/O thread at most once.
fn flush_http(st: &StateRc) {
  let replies = std::mem::take(&mut st.borrow_mut().http_replies);
  for (to, reply) in replies {
    to.send(reply);
  }
}

fn run_immediates(scope: &mut v8::PinScope, main: Option<&v8::Global<v8::Promise>>) -> Option<i32> {
  let st = state(scope);
  let (start, run) = {
    let s = st.borrow();
    (s.start_immediates.clone()?, s.run_immediate.clone()?)
  };
  v8::tc_scope!(let tc, scope);
  let undefined: v8::Local<v8::Value> = v8::undefined(tc).into();
  let start = v8::Local::new(tc, start);
  let count = start.call(tc, undefined, &[]).and_then(|n| n.uint32_value(tc)).unwrap_or(0);
  let run = v8::Local::new(tc, run);
  for i in 0..count {
    let arg = v8::Integer::new_from_unsigned(tc, i).into();
    if run.call(tc, undefined, &[arg]).is_none() {
      report_exception(tc);
      return Some(1);
    }
    if let Some(code) = run_microtasks(tc) {
      return Some(code);
    }
    if let Some(code) = check_errors(tc, main) {
      return Some(code);
    }
  }
  None
}

/// Reports a rejected entry module or an unhandled promise rejection.
fn check_errors(scope: &mut v8::PinScope, main: Option<&v8::Global<v8::Promise>>) -> Option<i32> {
  let st = state(scope);
  if let Some(main) = main {
    let p = v8::Local::new(scope, main);
    if p.state() == v8::PromiseState::Rejected {
      let err = p.result(scope);
      print_error(scope, err);
      return Some(1);
    }
  }
  let first = {
    let mut s = st.borrow_mut();
    if s.rejections.is_empty() {
      return None;
    }
    s.rejections.remove(0)
  };
  let err = v8::Local::new(scope, first.1);
  print_error(scope, err);
  Some(1)
}

extern "C" fn promise_reject_cb(msg: v8::PromiseRejectMessage) {
  let promise = msg.get_promise();
  v8::callback_scope!(unsafe scope, &msg);
  let st = state(scope);
  match msg.get_event() {
    v8::PromiseRejectEvent::PromiseRejectWithNoHandler => {
      let value = msg.get_value().unwrap_or_else(|| v8::undefined(scope).into());
      let entry = (v8::Global::new(scope, promise), v8::Global::new(scope, value));
      st.borrow_mut().rejections.push(entry);
    }
    v8::PromiseRejectEvent::PromiseHandlerAddedAfterReject => {
      let mut s = st.borrow_mut();
      s.rejections.retain(|(p, _)| v8::Local::new(scope, p) != promise);
    }
    _ => {}
  }
}

pub fn report_exception(tc: &mut v8::PinnedRef<v8::TryCatch<v8::HandleScope>>) {
  if let Some(exception) = tc.exception() {
    let message = tc.message();
    let is_syntax = exception.is_native_error() && {
      let name_key = v8::String::new(tc, "name").unwrap();
      v8::Local::<v8::Object>::try_from(exception)
        .ok()
        .and_then(|o| o.get(tc, name_key.into()))
        .is_some_and(|n| n.to_rust_string_lossy(tc) == "SyntaxError")
    };
    print_error(tc, exception);
    // Syntax errors have no useful stack; show where the parser failed.
    // Only for parser errors that point into a script (a SyntaxError we
    // threw ourselves, e.g. from the TypeScript compiler, has no location).
    if is_syntax
      && let Some(message) = message
      && let Some(line) = message.get_line_number(tc).filter(|&l| l > 0)
      && let Some(src) = message.get_source_line(tc).map(|s| s.to_rust_string_lossy(tc))
    {
      let file = message
        .get_script_resource_name(tc)
        .map(|n| n.to_rust_string_lossy(tc))
        .unwrap_or_default();
      let start = message.get_start_column().min(src.len());
      let caret = " ".repeat(start);
      eprintln!("    at {file}:{line}:{}\n\n  {src}\n  {caret}^", start + 1);
    }
  }
}

pub fn print_error(scope: &mut v8::PinScope, err: v8::Local<v8::Value>) {
  let st = state(scope);
  st.borrow_mut().flush();
  let text = inspect_value(scope, err);
  eprintln!("error: Uncaught {text}");
}

pub fn inspect_value(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> String {
  let st = state(scope);
  let inspect = st.borrow().inspect.clone();
  if let Some(inspect) = inspect {
    v8::tc_scope!(let tc, scope);
    let f = v8::Local::new(tc, inspect);
    let undefined = v8::undefined(tc).into();
    if let Some(s) = f.call(tc, undefined, &[v]) {
      return s.to_rust_string_lossy(tc);
    }
  }
  v.to_rust_string_lossy(scope)
}

// ------------------------------------------------------------------ modules

fn throw_error(scope: &mut v8::PinScope, msg: &str) {
  let m = v8::String::new(scope, msg).unwrap();
  let e = v8::Exception::error(scope, m);
  scope.throw_exception(e);
}

fn file_url(p: &Path) -> String {
  let mut s = String::from("file://");
  for c in p.to_string_lossy().chars() {
    match c {
      ' ' => s.push_str("%20"),
      '%' => s.push_str("%25"),
      '#' => s.push_str("%23"),
      '?' => s.push_str("%3F"),
      _ => s.push(c),
    }
  }
  s
}

fn url_to_path(url: &str) -> PathBuf {
  let raw = url.strip_prefix("file://").unwrap_or(url);
  let bytes = raw.as_bytes();
  let mut out = Vec::with_capacity(bytes.len());
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'%'
      && i + 2 < bytes.len()
      && let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16)
    {
      out.push(b);
      i += 3;
      continue;
    }
    out.push(bytes[i]);
    i += 1;
  }
  PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

fn try_file(p: &Path) -> Option<PathBuf> {
  if p.is_file() {
    return Some(p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
  }
  // TypeScript ESM imports name the output file ("./x.js") while the source
  // is "./x.ts".
  if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
    let ts_ext = match ext {
      "js" => &["ts", "tsx"][..],
      "mjs" => &["mts"][..],
      "cjs" => &["cts"][..],
      "jsx" => &["tsx"][..],
      _ => &[][..],
    };
    for e in ts_ext {
      let c = p.with_extension(e);
      if c.is_file() {
        return Some(c.canonicalize().unwrap_or(c));
      }
    }
  }
  let s = p.as_os_str();
  for ext in [".js", ".mjs", ".json", ".ts", ".mts", ".tsx"] {
    let mut c = s.to_owned();
    c.push(ext);
    let c = PathBuf::from(c);
    if c.is_file() {
      return Some(c.canonicalize().unwrap_or(c));
    }
  }
  if p.is_dir() {
    for idx in ["index.js", "index.mjs", "index.ts", "index.tsx"] {
      let c = p.join(idx);
      if c.is_file() {
        return Some(c.canonicalize().unwrap_or(c));
      }
    }
  }
  None
}

/// Lexically resolve `.` and `..` components (no filesystem access).
fn normalize(p: &Path) -> PathBuf {
  use std::path::Component;
  let mut out = PathBuf::new();
  for c in p.components() {
    match c {
      Component::CurDir => {}
      Component::ParentDir => {
        out.pop();
      }
      c => out.push(c),
    }
  }
  out
}

fn resolve_path(specifier: &str, base: &Path) -> Result<PathBuf, String> {
  let p = normalize(&if specifier.starts_with("file://") {
    url_to_path(specifier)
  } else {
    base.join(specifier)
  });
  try_file(&p).ok_or_else(|| format!("Cannot find module '{}'", p.display()))
}

/// Minimal package.json reading using V8's JSON parser.
fn package_entry(scope: &mut v8::PinScope, pkg_dir: &Path) -> Option<String> {
  let text = std::fs::read_to_string(pkg_dir.join("package.json")).ok()?;
  let text = v8::String::new(scope, &text)?;
  let json = v8::json::parse(scope, text)?;
  let obj = v8::Local::<v8::Object>::try_from(json).ok()?;
  fn get<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    o: v8::Local<'s, v8::Object>,
    k: &str,
  ) -> Option<v8::Local<'s, v8::Value>> {
    let key = v8::String::new(scope, k).unwrap();
    o.get(scope, key.into()).filter(|v| !v.is_null_or_undefined())
  }
  fn pick(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> Option<String> {
    if v.is_string() {
      return Some(v.to_rust_string_lossy(scope));
    }
    let o = v8::Local::<v8::Object>::try_from(v).ok()?;
    for cond in [".", "import", "sako", "node", "default", "module"] {
      let key = v8::String::new(scope, cond).unwrap();
      if let Some(inner) = o.get(scope, key.into()).filter(|v| !v.is_null_or_undefined())
        && let Some(s) = pick(scope, inner)
      {
        return Some(s);
      }
    }
    None
  }
  if let Some(exports) = get(scope, obj, "exports")
    && let Some(s) = pick(scope, exports)
  {
    return Some(s);
  }
  for k in ["module", "main"] {
    if let Some(v) = get(scope, obj, k)
      && v.is_string()
    {
      return Some(v.to_rust_string_lossy(scope));
    }
  }
  None
}

fn resolve_bare(scope: &mut v8::PinScope, specifier: &str, base: &Path) -> Result<PathBuf, String> {
  let mut parts = specifier.splitn(if specifier.starts_with('@') { 3 } else { 2 }, '/');
  let mut name = parts.next().unwrap_or("").to_string();
  if specifier.starts_with('@') {
    name = format!("{}/{}", name, parts.next().unwrap_or(""));
  }
  let sub = parts.next();
  let mut dir = Some(base);
  while let Some(d) = dir {
    let pkg = d.join("node_modules").join(&name);
    if pkg.is_dir() {
      let found = match sub {
        Some(sub) => try_file(&pkg.join(sub)),
        None => package_entry(scope, &pkg)
          .and_then(|e| try_file(&pkg.join(e)))
          .or_else(|| try_file(&pkg.join("index.js"))),
      };
      if let Some(f) = found {
        return Ok(f);
      }
    }
    dir = d.parent();
  }
  Err(format!("Cannot find package '{name}' imported from {}", base.display()))
}

fn resolve(scope: &mut v8::PinScope, specifier: &str, referrer: &Path) -> Result<PathBuf, String> {
  let base = referrer.parent().unwrap_or(Path::new("/"));
  if specifier.starts_with("./")
    || specifier.starts_with("../")
    || specifier.starts_with('/')
    || specifier.starts_with("file://")
    || specifier == "."
    || specifier == ".."
  {
    resolve_path(specifier, base).map_err(|e| format!("{e} imported from {}", referrer.display()))
  } else if specifier.contains(':') {
    Err(format!("Unsupported module specifier '{specifier}'"))
  } else {
    resolve_bare(scope, specifier, base)
  }
}

fn cache_file(dir: &Path, path: &Path, src: &str) -> PathBuf {
  let mut h = std::collections::hash_map::DefaultHasher::new();
  path.hash(&mut h);
  src.hash(&mut h);
  dir.join(format!("{:016x}.bin", h.finish()))
}

fn new_source_string<'s>(scope: &v8::PinScope<'s, '_>, src: &str) -> Option<v8::Local<'s, v8::String>> {
  if src.is_ascii() {
    v8::String::new_from_one_byte(scope, src.as_bytes(), v8::NewStringType::Normal)
  } else {
    v8::String::new_from_utf8(scope, src.as_bytes(), v8::NewStringType::Normal)
  }
}

/// Compiles and registers a module. Returns None with an exception pending.
fn compile_module<'s>(
  scope: &mut v8::PinScope<'s, '_>,
  path: &Path,
  mut src: String,
  use_cache: bool,
) -> Option<v8::Local<'s, v8::Module>> {
  if path.extension().is_some_and(|e| e == "json") {
    src = format!("export default {src}\n;");
  }
  let st = state(scope);
  let cache_path = if use_cache {
    st.borrow().cache_dir.as_ref().map(|d| cache_file(d, path, &src))
  } else {
    None
  };
  let cached = cache_path.as_ref().and_then(|p| std::fs::read(p).ok());

  let Some(code) = new_source_string(scope, &src) else {
    throw_error(scope, "module source too large");
    return None;
  };
  let name = v8::String::new(scope, &path.to_string_lossy()).unwrap();
  let origin = v8::ScriptOrigin::new(scope, name.into(), 0, 0, false, 0, None, false, false, true, None);
  let (module, rejected) = match &cached {
    Some(bytes) => {
      let data = v8::CachedData::new(bytes);
      let mut source = v8::script_compiler::Source::new_with_cached_data(code, Some(&origin), data);
      let m = v8::script_compiler::compile_module2(
        scope,
        &mut source,
        v8::script_compiler::CompileOptions::ConsumeCodeCache,
        v8::script_compiler::NoCacheReason::NoReason,
      );
      let rejected = source.get_cached_data().is_none_or(|c| c.rejected());
      (m, rejected)
    }
    None => {
      let mut source = v8::script_compiler::Source::new(code, Some(&origin));
      (v8::script_compiler::compile_module(scope, &mut source), true)
    }
  };
  let module = module?;
  let global = v8::Global::new(scope, module);
  let mut s = st.borrow_mut();
  if rejected && let Some(cp) = cache_path {
    s.pending_cache.push((global.clone(), cp));
  }
  s.by_hash
    .entry(module.get_identity_hash().get())
    .or_default()
    .push((global.clone(), path.to_path_buf()));
  s.modules.insert(path.to_path_buf(), global);
  Some(module)
}

fn load_module<'s>(scope: &mut v8::PinScope<'s, '_>, path: &Path) -> Option<v8::Local<'s, v8::Module>> {
  let st = state(scope);
  if let Some(m) = st.borrow().modules.get(path) {
    return Some(v8::Local::new(scope, m));
  }
  let src = match std::fs::read_to_string(path) {
    Ok(s) => s,
    Err(e) => {
      throw_error(scope, &format!("Cannot load module '{}': {e}", path.display()));
      return None;
    }
  };
  // Strip a shebang line, keeping line numbers intact.
  let src = if src.starts_with("#!") {
    let end = src.find('\n').unwrap_or(src.len());
    format!("//{}", &src[2..end]) + &src[end..]
  } else {
    src
  };
  let src = if sako_typescript::is_typescript(path) {
    let cache_dir = st.borrow().cache_dir.clone();
    match transpile_cached(cache_dir.as_deref(), path, &src) {
      Ok((js, map)) => {
        let first = st.borrow().ts_maps.is_empty();
        st.borrow_mut().ts_maps.insert(path.to_path_buf(), map);
        if first {
          enable_ts_stacks(scope);
        }
        js
      }
      Err(msg) => {
        let m = v8::String::new(scope, &format!("{}\n{msg}", path.display())).unwrap();
        let e = v8::Exception::syntax_error(scope, m);
        scope.throw_exception(e);
        return None;
      }
    }
  } else {
    src
  };
  compile_module(scope, path, src, true)
}

/// Installs `Error.prepareStackTrace` so `error.stack` reports TypeScript
/// positions (once, when the first TypeScript module loads).
fn enable_ts_stacks(scope: &mut v8::PinScope) {
  let Some(hook) = state(scope).borrow().enable_ts_stacks.clone() else { return };
  let f = v8::Local::new(scope, hook);
  let undefined = v8::undefined(scope).into();
  let _ = f.call(scope, undefined, &[]);
}

/// Bumped when TypeScript output changes, to invalidate cached transpiles.
const TS_CACHE_VERSION: u32 = 1;

/// Transpiles TypeScript, reusing the on-disk result when the source and
/// path are unchanged.
fn transpile_cached(
  cache_dir: Option<&Path>,
  path: &Path,
  src: &str,
) -> Result<(String, sako_typescript::LineMap), String> {
  let key = cache_dir.map(|d| {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    TS_CACHE_VERSION.hash(&mut h);
    path.hash(&mut h);
    src.hash(&mut h);
    d.join(format!("ts-{:016x}", h.finish()))
  });
  if let Some(key) = &key
    && let (Ok(js), Ok(map)) = (std::fs::read_to_string(key.with_extension("js")), std::fs::read(key.with_extension("map")))
    && let Some(map) = sako_typescript::LineMap::from_bytes(&map)
  {
    return Ok((js, map));
  }
  let out = sako_typescript::transpile(src, path)?;
  if let Some(key) = key {
    if let Some(dir) = key.parent() {
      let _ = std::fs::create_dir_all(dir);
    }
    let tmp = |ext: &str| key.with_extension(format!("{ext}.tmp{}", std::process::id()));
    if std::fs::write(tmp("map"), out.line_map.to_bytes()).is_ok() {
      let _ = std::fs::rename(tmp("map"), key.with_extension("map"));
    }
    if std::fs::write(tmp("js"), &out.code).is_ok() {
      let _ = std::fs::rename(tmp("js"), key.with_extension("js"));
    }
  }
  Ok((out.code, out.line_map))
}

/// Rewrites `file.ts:line:col` positions in a stack trace from generated
/// JavaScript back to the TypeScript source.
pub fn map_stack(st: &State, stack: &str) -> String {
  use std::sync::OnceLock;
  static RE: OnceLock<regex::Regex> = OnceLock::new();
  let re = RE.get_or_init(|| regex::Regex::new(r"([^\s(]+\.(?:ts|tsx|mts|cts)):(\d+):(\d+)").unwrap());
  re.replace_all(stack, |c: &regex::Captures| {
    let file = &c[1];
    let line: u32 = c[2].parse().unwrap_or(0);
    let col: u32 = c[3].parse().unwrap_or(0);
    match st.ts_maps.get(Path::new(file)).and_then(|m| m.lookup(line.saturating_sub(1), col.saturating_sub(1))) {
      Some((l, c)) => format!("{file}:{}:{}", l + 1, c + 1),
      None => c[0].to_string(),
    }
  })
  .into_owned()
}

fn module_path(scope: &v8::PinScope, module: v8::Local<v8::Module>) -> Option<PathBuf> {
  let st = state(scope);
  let s = st.borrow();
  let list = s.by_hash.get(&module.get_identity_hash().get())?;
  list
    .iter()
    .find(|(g, _)| v8::Local::new(scope, g) == module)
    .map(|(_, p)| p.clone())
}

fn resolve_cb<'s>(
  context: v8::Local<'s, v8::Context>,
  specifier: v8::Local<'s, v8::String>,
  _import_attributes: v8::Local<'s, v8::FixedArray>,
  referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
  v8::callback_scope!(unsafe scope, context);
  let spec = specifier.to_rust_string_lossy(scope);
  let referrer_path = module_path(scope, referrer).unwrap_or_else(|| PathBuf::from("/"));
  match resolve(scope, &spec, &referrer_path) {
    Ok(path) => load_module(scope, &path),
    Err(e) => {
      throw_error(scope, &e);
      None
    }
  }
}

fn dynamic_import_cb<'s>(
  scope: &mut v8::PinScope<'s, '_>,
  _host_defined_options: v8::Local<'s, v8::Data>,
  resource_name: v8::Local<'s, v8::Value>,
  specifier: v8::Local<'s, v8::String>,
  _import_attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
  let resolver = v8::PromiseResolver::new(scope)?;
  let promise = resolver.get_promise(scope);
  let spec = specifier.to_rust_string_lossy(scope);
  let referrer = if resource_name.is_string() {
    PathBuf::from(resource_name.to_rust_string_lossy(scope))
  } else {
    std::env::current_dir().unwrap_or_default().join("[eval]")
  };

  v8::tc_scope!(let tc, scope);
  let result = (|| {
    let path = match resolve(tc, &spec, &referrer) {
      Ok(p) => p,
      Err(e) => {
        throw_error(tc, &e);
        return None;
      }
    };
    let module = load_module(tc, &path)?;
    if module.get_status() == v8::ModuleStatus::Uninstantiated {
      module.instantiate_module(tc, resolve_cb)?;
    }
    let eval = module.evaluate(tc)?;
    let ns = module.get_module_namespace();
    let st = state(tc);
    let helper = st.borrow().import_helper.clone()?;
    let helper = v8::Local::new(tc, helper);
    let undefined = v8::undefined(tc).into();
    helper.call(tc, undefined, &[eval, ns])
  })();
  match result {
    Some(p) => {
      resolver.resolve(tc, p);
    }
    None => {
      let exception = tc.exception().unwrap_or_else(|| v8::undefined(tc).into());
      tc.reset();
      resolver.reject(tc, exception);
    }
  }
  Some(promise)
}

extern "C" fn import_meta_cb(
  context: v8::Local<v8::Context>,
  module: v8::Local<v8::Module>,
  meta: v8::Local<v8::Object>,
) {
  v8::callback_scope!(unsafe scope, context);
  let Some(path) = module_path(scope, module) else { return };
  let st = state(scope);
  let is_main = st.borrow().main_path.as_deref() == Some(path.as_path());
  let set = |k: &str, v: v8::Local<v8::Value>| {
    let key = v8::String::new(scope, k).unwrap();
    meta.create_data_property(scope, key.into(), v);
  };
  let url = v8::String::new(scope, &file_url(&path)).unwrap().into();
  set("url", url);
  let filename = v8::String::new(scope, &path.to_string_lossy()).unwrap().into();
  set("filename", filename);
  let dirname = v8::String::new(scope, &path.parent().unwrap_or(Path::new("/")).to_string_lossy())
    .unwrap()
    .into();
  set("dirname", dirname);
  let main = v8::Boolean::new(scope, is_main).into();
  set("main", main);
}

fn write_code_caches(scope: &mut v8::PinScope) {
  let st = state(scope);
  let pending = std::mem::take(&mut st.borrow_mut().pending_cache);
  if pending.is_empty() {
    return;
  }
  let mut created_dir = false;
  for (module, path) in pending {
    let module = v8::Local::new(scope, module);
    if module.get_status() == v8::ModuleStatus::Errored || !module.is_source_text_module() {
      continue;
    }
    let unbound = module.get_unbound_module_script(scope);
    let Some(data) = unbound.create_code_cache() else { continue };
    if !created_dir {
      if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
      }
      created_dir = true;
    }
    // Write atomically so concurrent runs never read a torn file.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, &data[..]).is_ok() {
      let _ = std::fs::rename(&tmp, &path);
    }
  }
}

/// Called from `process.exit()`.
pub fn exit_now(scope: &mut v8::PinScope, code: i32) -> ! {
  write_code_caches(scope);
  state(scope).borrow_mut().flush();
  std::process::exit(code)
}

// --------------------------------------------------------------------- REPL

fn repl(isolate: &mut v8::OwnedIsolate, platform: &v8::SharedRef<v8::Platform>) -> i32 {
  let state = Rc::new(RefCell::new(State::new(None, None, vec!["sako".into()], Vec::new())));
  isolate.set_slot(state.clone());
  v8::scope!(let scope, isolate);
  let context = v8::Context::new(scope, Default::default());
  let scope = &mut v8::ContextScope::new(scope, context);
  if !bootstrap(scope) {
    return 1;
  }
  println!("Sako {VERSION} (V8 {}). Ctrl+D to exit.", v8::V8::get_version());
  let stdin = std::io::stdin();
  let mut buf = String::new();
  loop {
    print!("{}", if buf.is_empty() { "> " } else { "... " });
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if stdin.read_line(&mut line).unwrap_or(0) == 0 {
      println!();
      return 0;
    }
    buf.push_str(&line);
    v8::tc_scope!(let tc, scope);
    let src = v8::String::new(tc, &buf).unwrap();
    let name = v8::String::new(tc, "[repl]").unwrap();
    let origin = v8::ScriptOrigin::new(tc, name.into(), 0, 0, false, 0, None, false, false, false, None);
    let Some(script) = v8::Script::compile(tc, src, Some(&origin)) else {
      // Unterminated input: keep reading lines.
      let msg = tc.message().map(|m| m.get(tc).to_rust_string_lossy(tc)).unwrap_or_default();
      if msg.contains("Unexpected end of input") || msg.contains("Unterminated template") {
        tc.reset();
        continue;
      }
      buf.clear();
      report_exception(tc);
      continue;
    };
    buf.clear();
    match script.run(tc) {
      Some(v) => {
        let s = inspect_value(tc, v);
        state.borrow_mut().flush();
        println!("{s}");
      }
      None => report_exception(tc),
    }
    tc.reset();
    let _ = event_loop(tc, platform, None);
    state.borrow_mut().rejections.clear();
    state.borrow_mut().flush();
  }
}
