//! Executor for parsed `Sako.$` scripts.
//!
//! Builtins run in-process. External programs are spawned directly (no
//! `/bin/sh`) and connected with OS pipes. A script made only of builtins
//! whose arguments are known up front can run inline on the JS thread
//! (`can_inline`); a script that may spawn a process runs on the thread pool.

use super::builtins;
use super::parse::{AndOr, Command, Fd, Part, Pipeline, RedirOp, Redirect, Script, Word};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

/// An interpolated JS value.
#[derive(Debug, Clone)]
pub enum Value {
  Str(String),
  List(Vec<String>),
  Bytes(Arc<Vec<u8>>),
}

pub struct Output {
  pub code: i32,
  pub stdout: Vec<u8>,
  pub stderr: Vec<u8>,
}

/// Environment: the process environment (or a replacement from `.env()`)
/// plus changes made by the script.
#[derive(Clone, Default)]
pub struct Env {
  base: Option<Arc<HashMap<String, String>>>,
  overrides: HashMap<String, Option<String>>,
}

impl Env {
  pub fn new(base: Option<HashMap<String, String>>) -> Self {
    Env { base: base.map(Arc::new), overrides: HashMap::new() }
  }

  pub fn get(&self, k: &str) -> Option<String> {
    if let Some(v) = self.overrides.get(k) {
      return v.clone();
    }
    match &self.base {
      Some(m) => m.get(k).cloned(),
      None => std::env::var(k).ok(),
    }
  }

  pub fn set(&mut self, k: &str, v: Option<String>) {
    self.overrides.insert(k.to_string(), v);
  }

  fn apply(&self, cmd: &mut std::process::Command) {
    if let Some(base) = &self.base {
      cmd.env_clear();
      cmd.envs(base.iter());
    }
    for (k, v) in &self.overrides {
      match v {
        Some(v) => cmd.env(k, v),
        None => cmd.env_remove(k),
      };
    }
  }
}

/// Where a command's standard input comes from.
pub enum Input {
  Null,
  Bytes(Vec<u8>),
  Shared(Arc<Vec<u8>>),
  Pipe(std::io::PipeReader),
  File(File),
}

impl Input {
  pub fn read_all(self) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    match self {
      Input::Null => {}
      Input::Bytes(b) => return Ok(b),
      Input::Shared(b) => return Ok(b.as_ref().clone()),
      Input::Pipe(mut r) => {
        r.read_to_end(&mut buf)?;
      }
      Input::File(mut f) => {
        f.read_to_end(&mut buf)?;
      }
    }
    Ok(buf)
  }
}

type SharedFile = Arc<Mutex<File>>;

enum OutTarget {
  Default,
  File(SharedFile),
  ToStderr,
}

enum ErrTarget {
  Default,
  File(SharedFile),
  ToStdout,
}

struct Redirs {
  stdin: Option<Input>,
  stdout: OutTarget,
  stderr: ErrTarget,
}

enum Prepared {
  AssignOnly,
  Run { argv: Vec<String>, assigns: Vec<(String, String)>, redirs: Redirs },
}

type ErrPipe = Option<(std::io::PipeReader, std::io::PipeWriter)>;

/// Unwinds the script on `exit`.
pub struct Exit(pub i32);

pub struct Shell<'v> {
  pub cwd: PathBuf,
  pub env: Env,
  pub vars: HashMap<String, String>,
  pub last: i32,
  values: &'v [Value],
  pub out: Vec<u8>,
  pub err: Vec<u8>,
  /// Copy output to the process's stdout/stderr as it is produced.
  live: bool,
}

impl<'v> Shell<'v> {
  pub fn new(cwd: PathBuf, env: Env, values: &'v [Value], live: bool) -> Self {
    Shell { cwd, env, vars: HashMap::new(), last: 0, values, out: Vec::new(), err: Vec::new(), live }
  }

  pub fn run(mut self, script: &Script) -> Output {
    let code = match self.run_script(script) {
      Ok(()) => self.last,
      Err(Exit(code)) => code,
    };
    Output { code, stdout: self.out, stderr: self.err }
  }

  pub fn write_out(&mut self, data: &[u8]) {
    if data.is_empty() {
      return;
    }
    if self.live {
      let mut so = std::io::stdout().lock();
      let _ = so.write_all(data);
      let _ = so.flush();
    }
    self.out.extend_from_slice(data);
  }

  pub fn write_err(&mut self, data: &[u8]) {
    if data.is_empty() {
      return;
    }
    if self.live {
      let _ = std::io::stderr().write_all(data);
    }
    self.err.extend_from_slice(data);
  }

  pub fn lookup(&self, name: &str) -> Option<String> {
    if name == "?" {
      return Some(self.last.to_string());
    }
    if let Some(v) = self.vars.get(name) {
      return Some(v.clone());
    }
    self.env.get(name)
  }

  pub fn resolve_path(&self, p: &str) -> PathBuf {
    normalize(&self.cwd.join(p))
  }

  fn run_script(&mut self, script: &Script) -> Result<(), Exit> {
    for item in &script.0 {
      self.run_and_or(item)?;
    }
    Ok(())
  }

  fn run_and_or(&mut self, item: &AndOr) -> Result<(), Exit> {
    self.last = self.run_pipeline(&item.first)?;
    for (is_and, p) in &item.rest {
      if *is_and == (self.last == 0) {
        self.last = self.run_pipeline(p)?;
      }
    }
    Ok(())
  }

  fn run_pipeline(&mut self, p: &Pipeline) -> Result<i32, Exit> {
    let n = p.0.len();
    let mut input = Input::Null;
    let mut children: Vec<Child> = Vec::new();
    // One pipe collects stderr of all external processes in the pipeline.
    let mut err_pipe: ErrPipe = None;
    let mut err_thread: Option<std::thread::JoinHandle<Vec<u8>>> = None;
    // The last external command's stdout, read at the end.
    let mut capture: Option<std::io::PipeReader> = None;
    let mut status = 0;
    let mut last_is_child = false;
    let mut exit = None;

    for (i, cmd) in p.0.iter().enumerate() {
      let is_last = i + 1 == n;
      let stage_input = std::mem::replace(&mut input, Input::Null);
      last_is_child = false;
      let (argv, assigns, redirs) = match self.prepare(cmd) {
        Err(msg) => {
          self.write_err(format!("sako: {msg}\n").as_bytes());
          status = 1;
          continue;
        }
        Ok(Prepared::AssignOnly) => {
          status = 0;
          continue;
        }
        Ok(Prepared::Run { argv, assigns, redirs }) => (argv, assigns, redirs),
      };
      let Redirs { stdin, stdout, stderr } = redirs;
      let stdin = stdin.unwrap_or(stage_input);

      if builtins::is_builtin(&argv[0]) && builtins::supported(&argv, &self.env) {
        // A builtin reading an external command's output blocks until that
        // command exits; keep its stderr flowing meanwhile.
        if matches!(stdin, Input::Pipe(_)) && err_thread.is_none() {
          err_thread = err_pipe.as_ref().and_then(|(r, _)| r.try_clone().ok()).map(|r| self.stderr_thread(r));
        }
        let ret = builtins::run(self, &argv, &assigns, stdin);
        // Route stderr first, then stdout (merged streams keep that order).
        let mut out = ret.out;
        match &stderr {
          ErrTarget::Default => self.write_err(&ret.err),
          ErrTarget::File(f) => write_shared(f, &ret.err),
          ErrTarget::ToStdout => out.extend_from_slice(&ret.err),
        }
        match &stdout {
          OutTarget::File(f) => write_shared(f, &out),
          OutTarget::ToStderr => match &stderr {
            ErrTarget::File(f) => write_shared(f, &out),
            _ => self.write_err(&out),
          },
          OutTarget::Default if !is_last => input = Input::Bytes(out),
          OutTarget::Default => self.write_out(&out),
        }
        status = ret.code;
        if ret.exit {
          exit = Some(ret.code);
          break;
        }
        continue;
      }

      match self.spawn(&argv, &assigns, stdin, &stdout, &stderr, &mut err_pipe) {
        Ok((child, out_reader)) => {
          children.push(child);
          if let Some(r) = out_reader {
            if is_last {
              capture = Some(r);
            } else {
              input = Input::Pipe(r);
            }
          }
          last_is_child = true;
        }
        Err(code) => status = code,
      }
    }
    // Close the read end handed to a stage that never consumed it.
    drop(input);

    // Drain the last command's stdout and the shared stderr pipe together
    // (one poll loop, no helper thread), unless a thread already owns stderr.
    let err_reader = err_pipe.and_then(|(r, w)| {
      drop(w);
      err_thread.is_none().then_some(r)
    });
    self.drain(capture, err_reader);
    let mut child_status = 0;
    for mut c in children {
      child_status = c.wait().map_or(1, exit_code);
    }
    if let Some(t) = err_thread
      && let Ok(err) = t.join()
    {
      // Shown live already if requested; keep it for the result.
      self.err.extend_from_slice(&err);
    }
    if last_is_child {
      status = child_status;
    }
    if let Some(code) = exit {
      self.last = code;
      return Err(Exit(code));
    }
    Ok(status)
  }

  fn stderr_thread(&self, mut r: std::io::PipeReader) -> std::thread::JoinHandle<Vec<u8>> {
    let live = self.live;
    std::thread::spawn(move || {
      let mut all = Vec::new();
      let mut buf = [0u8; 16 * 1024];
      loop {
        match r.read(&mut buf) {
          Ok(0) => break,
          Ok(n) => {
            if live {
              let _ = std::io::stderr().write_all(&buf[..n]);
            }
            all.extend_from_slice(&buf[..n]);
          }
          Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
          Err(_) => break,
        }
      }
      all
    })
  }

  /// Reads stdout and stderr pipes to EOF without a helper thread.
  fn drain(&mut self, out: Option<std::io::PipeReader>, err: Option<std::io::PipeReader>) {
    if out.is_none() && err.is_none() {
      return; // builtins only: nothing to read (and no buffer to allocate)
    }
    let mut readers = [out, err];
    let mut buf = vec![0u8; 64 * 1024];
    let emit = |sh: &mut Self, stream: usize, data: &[u8]| {
      if stream == 0 { sh.write_out(data) } else { sh.write_err(data) }
    };
    loop {
      let open: Vec<usize> = (0..2).filter(|&i| readers[i].is_some()).collect();
      if open.len() == 1 {
        // A single stream: plain blocking reads.
        let i = open[0];
        let mut r = readers[i].take().unwrap();
        loop {
          match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => emit(self, i, &buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
          }
        }
        return;
      }
      if open.is_empty() {
        return;
      }
      #[cfg(unix)]
      {
        use std::os::fd::AsRawFd;
        let mut fds: Vec<libc::pollfd> = open
          .iter()
          .map(|&i| libc::pollfd { fd: readers[i].as_ref().unwrap().as_raw_fd(), events: libc::POLLIN, revents: 0 })
          .collect();
        // SAFETY: `fds` holds valid pollfd entries for open descriptors.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
          if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
          }
          return;
        }
        for (k, pfd) in fds.iter().enumerate() {
          if pfd.revents == 0 {
            continue;
          }
          let i = open[k];
          match readers[i].as_mut().unwrap().read(&mut buf) {
            Ok(0) | Err(_) => readers[i] = None,
            Ok(n) => emit(self, i, &buf[..n]),
          }
        }
      }
      #[cfg(not(unix))]
      {
        // No poll(2): drain stderr on a thread, stdout here.
        let t = self.stderr_thread(readers[1].take().unwrap());
        let r = readers[0].take();
        self.drain(r, None);
        if let Ok(err) = t.join() {
          self.err.extend_from_slice(&err);
        }
        return;
      }
    }
  }

  /// Spawns an external program. Returns the child and, when its stdout is
  /// not redirected, the read end of its stdout pipe.
  fn spawn(
    &mut self,
    argv: &[String],
    assigns: &[(String, String)],
    stdin: Input,
    stdout: &OutTarget,
    stderr: &ErrTarget,
    err_pipe: &mut ErrPipe,
  ) -> Result<(Child, Option<std::io::PipeReader>), i32> {
    let Some(path) = resolve_command(&argv[0], &self.cwd, &self.env) else {
      self.write_err(format!("sako: command not found: {}\n", argv[0]).as_bytes());
      return Err(127);
    };
    match self.spawn_inner(&path, argv, assigns, stdin, stdout, stderr, err_pipe) {
      Ok(r) => Ok(r),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
        self.write_err(format!("sako: command not found: {}\n", argv[0]).as_bytes());
        Err(127)
      }
      Err(e) => {
        self.write_err(format!("sako: {}: {}\n", argv[0], io_msg(&e)).as_bytes());
        Err(126)
      }
    }
  }

  #[allow(clippy::too_many_arguments)]
  fn spawn_inner(
    &mut self,
    path: &Path,
    argv: &[String],
    assigns: &[(String, String)],
    stdin: Input,
    stdout: &OutTarget,
    stderr: &ErrTarget,
    err_pipe: &mut ErrPipe,
  ) -> std::io::Result<(Child, Option<std::io::PipeReader>)> {
    let mut cmd = std::process::Command::new(path);
    set_arg0(&mut cmd, &argv[0]);
    cmd.args(&argv[1..]).current_dir(&self.cwd);
    self.env.apply(&mut cmd);
    for (k, v) in assigns {
      cmd.env(k, v);
    }

    let mut feed: Option<Vec<u8>> = None;
    let mut shared_feed: Option<Arc<Vec<u8>>> = None;
    cmd.stdin(match stdin {
      Input::Null => Stdio::null(),
      Input::Bytes(b) => {
        feed = Some(b);
        Stdio::piped()
      }
      Input::Shared(b) => {
        shared_feed = Some(b);
        Stdio::piped()
      }
      Input::Pipe(r) => Stdio::from(r),
      Input::File(f) => Stdio::from(f),
    });

    fn err_writer(err_pipe: &mut ErrPipe) -> std::io::Result<Stdio> {
      if err_pipe.is_none() {
        *err_pipe = Some(std::io::pipe()?);
      }
      Ok(Stdio::from(err_pipe.as_ref().unwrap().1.try_clone()?))
    }
    fn file_stdio(f: &SharedFile) -> std::io::Result<Stdio> {
      Ok(Stdio::from(f.lock().unwrap().try_clone()?))
    }

    let mut out_reader = None;
    let mut merged_writer: Option<std::io::PipeWriter> = None;
    let out_stdio = match stdout {
      OutTarget::File(f) => file_stdio(f)?,
      OutTarget::ToStderr => match stderr {
        ErrTarget::File(f) => file_stdio(f)?,
        _ => err_writer(err_pipe)?,
      },
      OutTarget::Default => {
        let (r, w) = std::io::pipe()?;
        if matches!(stderr, ErrTarget::ToStdout) {
          merged_writer = Some(w.try_clone()?);
        }
        out_reader = Some(r);
        Stdio::from(w)
      }
    };
    cmd.stdout(out_stdio);
    cmd.stderr(match stderr {
      ErrTarget::File(f) => file_stdio(f)?,
      ErrTarget::ToStdout => match (merged_writer, stdout) {
        (Some(w), _) => Stdio::from(w),
        (None, OutTarget::File(f)) => file_stdio(f)?,
        _ => err_writer(err_pipe)?,
      },
      ErrTarget::Default => err_writer(err_pipe)?,
    });

    let mut child = cmd.spawn()?;
    // Close our copies of the child's pipe ends.
    drop(cmd);
    if feed.is_some() || shared_feed.is_some() {
      let mut w = child.stdin.take().unwrap();
      let len = feed.as_ref().map_or(0, |b| b.len()) + shared_feed.as_ref().map_or(0, |b| b.len());
      let mut write = move || {
        let _ = match (&feed, &shared_feed) {
          (Some(b), _) => w.write_all(b),
          (_, Some(b)) => w.write_all(b),
          _ => Ok(()),
        };
      };
      // Small inputs fit in the pipe buffer; larger ones need a writer
      // thread so the child can drain stdout meanwhile.
      if len <= 16 * 1024 {
        write();
      } else {
        std::thread::spawn(write);
      }
    }
    Ok((child, out_reader))
  }

  fn prepare(&mut self, cmd: &Command) -> Result<Prepared, String> {
    let argv = self.expand_words(&cmd.words)?;
    let mut assigns = Vec::with_capacity(cmd.assigns.len());
    for (k, w) in &cmd.assigns {
      assigns.push((k.clone(), self.expand_joined(w)?));
    }
    let redirs = self.open_redirects(&cmd.redirects)?;
    if argv.is_empty() {
      // `NAME=value` on its own sets a shell variable.
      for (k, v) in assigns {
        self.vars.insert(k, v);
      }
      return Ok(Prepared::AssignOnly);
    }
    Ok(Prepared::Run { argv, assigns, redirs })
  }

  fn open_redirects(&mut self, redirects: &[Redirect]) -> Result<Redirs, String> {
    let mut r = Redirs { stdin: None, stdout: OutTarget::Default, stderr: ErrTarget::Default };
    for redir in redirects {
      match redir {
        Redirect::Dup { from: 2, to: 1 } => {
          r.stderr = match &r.stdout {
            OutTarget::File(f) => ErrTarget::File(f.clone()),
            _ => ErrTarget::ToStdout,
          }
        }
        Redirect::Dup { from: 1, to: 2 } => {
          r.stdout = match &r.stderr {
            ErrTarget::File(f) => OutTarget::File(f.clone()),
            _ => OutTarget::ToStderr,
          }
        }
        Redirect::Dup { .. } => return Err("unsupported file descriptor duplication".into()),
        Redirect::File { fd, op, target } => {
          if *fd == Fd::Stdin
            && let [Part::Interp(i)] = target.0.as_slice()
            && let Some(Value::Bytes(b)) = self.values.get(*i)
          {
            r.stdin = Some(Input::Shared(b.clone()));
            continue;
          }
          let name = self.expand_joined(target)?;
          let path = self.resolve_path(&name);
          let file = match op {
            RedirOp::In => File::open(&path),
            RedirOp::Out => File::create(&path),
            RedirOp::Append => std::fs::OpenOptions::new().create(true).append(true).open(&path),
          }
          .map_err(|e| format!("{name}: {}", io_msg(&e)))?;
          match fd {
            Fd::Stdin => r.stdin = Some(Input::File(file)),
            Fd::Stdout => r.stdout = OutTarget::File(Arc::new(Mutex::new(file))),
            Fd::Stderr => r.stderr = ErrTarget::File(Arc::new(Mutex::new(file))),
            Fd::Both => {
              let f = Arc::new(Mutex::new(file));
              r.stdout = OutTarget::File(f.clone());
              r.stderr = ErrTarget::File(f);
            }
          }
        }
      }
    }
    Ok(r)
  }

  // ------------------------------------------------------------ expansion

  fn expand_words(&mut self, words: &[Word]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(words.len());
    for w in words {
      self.expand_word(w, &mut out)?;
    }
    Ok(out)
  }

  /// Expands a word without field splitting or globbing (assignment values,
  /// redirect targets).
  fn expand_joined(&mut self, w: &Word) -> Result<String, String> {
    let mut s = String::new();
    for (i, part) in w.0.iter().enumerate() {
      match part {
        Part::Lit(l) if i == 0 && (l == "~" || l.starts_with("~/")) => {
          s.push_str(&self.lookup("HOME").unwrap_or_default());
          s.push_str(&l[1..]);
        }
        Part::Lit(l) | Part::Quoted(l) => s.push_str(l),
        Part::Var(name, _) => s.push_str(&self.lookup(name).unwrap_or_default()),
        Part::Interp(i) => match self.values.get(*i) {
          Some(Value::Str(v)) => s.push_str(v),
          Some(Value::List(v)) => s.push_str(&v.join(" ")),
          Some(Value::Bytes(_)) => return Err("a buffer can only be used as `< ${buffer}`".into()),
          None => {}
        },
        Part::Subst(script, _) => s.push_str(&self.substitute(script)),
      }
    }
    Ok(s)
  }

  fn expand_word(&mut self, w: &Word, out: &mut Vec<String>) -> Result<(), String> {
    // Fast paths: a plain literal without glob characters or tilde, or a
    // single quoted string.
    match w.0.as_slice() {
      [Part::Lit(l)] if !has_glob(l) && !l.starts_with('~') => {
        out.push(l.clone());
        return Ok(());
      }
      [Part::Quoted(q)] => {
        out.push(q.clone());
        return Ok(());
      }
      _ => {}
    }

    // Fields being built: text, glob pattern (with `\` escapes for literal
    // characters), whether it globs, and whether it has solid content (so an
    // empty unquoted expansion produces no word, but "" does).
    struct Field {
      text: String,
      pattern: String,
      glob: bool,
      solid: bool,
    }
    fn new_field() -> Field {
      Field { text: String::new(), pattern: String::new(), glob: false, solid: false }
    }
    fn push_escaped(f: &mut Field, s: &str) {
      f.text.push_str(s);
      for c in s.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
          f.pattern.push('\\');
        }
        f.pattern.push(c);
      }
      f.solid = true;
    }
    // Unquoted expansion results split on whitespace into separate fields.
    fn push_split(fields: &mut Vec<Field>, s: &str) {
      if s.trim().is_empty() {
        return;
      }
      if s.starts_with(|c: char| c.is_ascii_whitespace()) && fields.last().is_some_and(|f| f.solid) {
        fields.push(new_field());
      }
      for (k, piece) in s.split_ascii_whitespace().enumerate() {
        if k > 0 {
          fields.push(new_field());
        }
        push_escaped(fields.last_mut().unwrap(), piece);
      }
      if s.ends_with(|c: char| c.is_ascii_whitespace()) {
        fields.push(new_field());
      }
    }

    let mut fields = vec![new_field()];
    for (i, part) in w.0.iter().enumerate() {
      match part {
        Part::Lit(l) => {
          let f = fields.last_mut().unwrap();
          let l = if i == 0 && (l == "~" || l.starts_with("~/")) {
            push_escaped(f, &self.lookup("HOME").unwrap_or_default());
            &l[1..]
          } else {
            l.as_str()
          };
          f.text.push_str(l);
          f.pattern.push_str(l);
          f.glob |= has_glob(l);
          f.solid = true;
        }
        Part::Quoted(q) => push_escaped(fields.last_mut().unwrap(), q),
        Part::Interp(idx) => match self.values.get(*idx) {
          Some(Value::Str(v)) => push_escaped(fields.last_mut().unwrap(), v),
          Some(Value::List(items)) => {
            for (j, item) in items.iter().enumerate() {
              if j > 0 {
                fields.push(new_field());
              }
              push_escaped(fields.last_mut().unwrap(), item);
            }
          }
          Some(Value::Bytes(_)) => return Err("a buffer can only be used as `< ${buffer}`".into()),
          None => {}
        },
        Part::Var(name, quoted) => {
          let v = self.lookup(name).unwrap_or_default();
          if *quoted {
            push_escaped(fields.last_mut().unwrap(), &v);
          } else {
            push_split(&mut fields, &v);
          }
        }
        Part::Subst(script, quoted) => {
          let v = self.substitute(script);
          if *quoted {
            push_escaped(fields.last_mut().unwrap(), &v);
          } else {
            push_split(&mut fields, &v);
          }
        }
      }
    }
    for f in fields {
      if f.glob {
        let matches = glob(&f.pattern, &self.cwd);
        if matches.is_empty() {
          out.push(f.text);
        } else {
          out.extend(matches);
        }
      } else if f.solid {
        out.push(f.text);
      }
    }
    Ok(())
  }

  /// Runs `$(...)` in a copy of this shell and returns its stdout without
  /// trailing newlines. Its stderr passes through.
  fn substitute(&mut self, script: &Script) -> String {
    let mut sub = Shell::new(self.cwd.clone(), self.env.clone(), self.values, false);
    sub.vars = self.vars.clone();
    sub.last = self.last;
    let out = sub.run(script);
    self.write_err(&out.stderr);
    self.last = out.code;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    while s.ends_with('\n') {
      s.pop();
    }
    s
  }
}

fn write_shared(f: &SharedFile, data: &[u8]) {
  if !data.is_empty() {
    let _ = f.lock().unwrap().write_all(data);
  }
}

fn exit_code(s: std::process::ExitStatus) -> i32 {
  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    if let Some(sig) = s.signal() {
      return 128 + sig;
    }
  }
  s.code().unwrap_or(1)
}

fn set_arg0(cmd: &mut std::process::Command, name: &str) {
  #[cfg(unix)]
  {
    use std::os::unix::process::CommandExt;
    cmd.arg0(name);
  }
  #[cfg(not(unix))]
  let _ = (cmd, name);
}

pub fn io_msg(e: &std::io::Error) -> String {
  use std::io::ErrorKind::*;
  match e.kind() {
    NotFound => "No such file or directory".into(),
    PermissionDenied => "Permission denied".into(),
    AlreadyExists => "File exists".into(),
    IsADirectory => "Is a directory".into(),
    NotADirectory => "Not a directory".into(),
    DirectoryNotEmpty => "Directory not empty".into(),
    _ => {
      let mut s = e.to_string();
      if let Some(i) = s.find(" (os error") {
        s.truncate(i);
      }
      s
    }
  }
}

/// Lexically resolves `.` and `..`.
pub fn normalize(p: &Path) -> PathBuf {
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

/// PATH lookup with a small cache (one `stat` to revalidate).
pub fn resolve_command(name: &str, cwd: &Path, env: &Env) -> Option<PathBuf> {
  if name.contains('/') {
    let p = cwd.join(name);
    return p.is_file().then_some(p);
  }
  static CACHE: OnceLock<Mutex<HashMap<(String, String), PathBuf>>> = OnceLock::new();
  let path_var = env.get("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
  let key = (path_var, name.to_string());
  let cache = CACHE.get_or_init(Default::default);
  if let Some(p) = cache.lock().unwrap().get(&key)
    && p.is_file()
  {
    return Some(p.clone());
  }
  for dir in key.0.split(':') {
    let dir = if dir.is_empty() { Path::new(".") } else { Path::new(dir) };
    let candidate = dir.join(name);
    if is_executable(&candidate) {
      let candidate = if candidate.is_relative() { cwd.join(candidate) } else { candidate };
      cache.lock().unwrap().insert(key, candidate.clone());
      return Some(candidate);
    }
  }
  None
}

fn is_executable(p: &Path) -> bool {
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
  }
  #[cfg(not(unix))]
  p.is_file()
}

// ----------------------------------------------------------------- globbing

fn has_glob(s: &str) -> bool {
  s.contains(['*', '?', '['])
}

/// Case-insensitive name order (matches `Bun.$`'s `ls` and globs).
pub fn name_order(a: &str, b: &str) -> std::cmp::Ordering {
  let la = a.chars().flat_map(char::to_lowercase);
  let lb = b.chars().flat_map(char::to_lowercase);
  la.cmp(lb).then_with(|| a.cmp(b))
}

/// Expands a glob pattern (with `\` escapes) relative to `cwd`.
fn glob(pattern: &str, cwd: &Path) -> Vec<String> {
  let absolute = pattern.starts_with('/');
  let comps: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty()).collect();
  // (display prefix, filesystem path)
  let mut current: Vec<(String, PathBuf)> =
    vec![if absolute { ("/".to_string(), PathBuf::from("/")) } else { (String::new(), cwd.to_path_buf()) }];
  for (i, comp) in comps.iter().enumerate() {
    let last = i + 1 == comps.len();
    let mut next = Vec::new();
    for (shown, dir) in &current {
      let join = |name: &str| {
        if shown.is_empty() || shown.ends_with('/') { format!("{shown}{name}") } else { format!("{shown}/{name}") }
      };
      if !has_glob_unescaped(comp) {
        let name = unescape(comp);
        let p = dir.join(&name);
        if last && !p.exists() {
          continue;
        }
        next.push((join(&name), p));
        continue;
      }
      let Ok(rd) = std::fs::read_dir(dir) else { continue };
      let pat: Vec<char> = comp.chars().collect();
      let mut names: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| (!n.starts_with('.') || comp.starts_with('.')) && wildmatch(&pat, &n.chars().collect::<Vec<_>>()))
        .collect();
      names.sort_by(|a, b| name_order(a, b));
      for n in names {
        let p = dir.join(&n);
        if !last && !p.is_dir() {
          continue;
        }
        next.push((join(&n), p));
      }
    }
    current = next;
    if current.is_empty() {
      break;
    }
  }
  if pattern.ends_with('/') {
    current.into_iter().map(|(s, _)| s + "/").collect()
  } else {
    current.into_iter().map(|(s, _)| s).collect()
  }
}

fn has_glob_unescaped(s: &str) -> bool {
  let mut escaped = false;
  for c in s.chars() {
    if escaped {
      escaped = false;
    } else if c == '\\' {
      escaped = true;
    } else if matches!(c, '*' | '?' | '[') {
      return true;
    }
  }
  false
}

fn unescape(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  let mut escaped = false;
  for c in s.chars() {
    if !escaped && c == '\\' {
      escaped = true;
      continue;
    }
    escaped = false;
    out.push(c);
  }
  out
}

/// Matches `*`, `?`, `[...]` (with ranges and `!`/`^`) and `\` escapes.
fn wildmatch(p: &[char], s: &[char]) -> bool {
  let (mut pi, mut si) = (0, 0);
  let (mut star_p, mut star_s) = (usize::MAX, 0);
  while si < s.len() {
    if pi < p.len() {
      match p[pi] {
        '*' => {
          star_p = pi;
          star_s = si;
          pi += 1;
          continue;
        }
        '?' => {
          pi += 1;
          si += 1;
          continue;
        }
        '[' => {
          if let Some((matched, len)) = class_match(&p[pi..], s[si]) {
            if matched {
              pi += len;
              si += 1;
              continue;
            }
          } else if s[si] == '[' {
            pi += 1;
            si += 1;
            continue;
          }
        }
        '\\' if pi + 1 < p.len() => {
          if p[pi + 1] == s[si] {
            pi += 2;
            si += 1;
            continue;
          }
        }
        c if c == s[si] => {
          pi += 1;
          si += 1;
          continue;
        }
        _ => {}
      }
    }
    if star_p != usize::MAX {
      pi = star_p + 1;
      star_s += 1;
      si = star_s;
      continue;
    }
    return false;
  }
  while pi < p.len() && p[pi] == '*' {
    pi += 1;
  }
  pi == p.len()
}

/// Matches a `[...]` class at the start of `p`; returns (matched, length).
fn class_match(p: &[char], c: char) -> Option<(bool, usize)> {
  let mut i = 1;
  let negate = matches!(p.get(1), Some('!' | '^'));
  if negate {
    i += 1;
  }
  let mut matched = false;
  let mut first = true;
  while i < p.len() {
    if p[i] == ']' && !first {
      return Some((matched != negate, i + 1));
    }
    first = false;
    if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
      if p[i] <= c && c <= p[i + 2] {
        matched = true;
      }
      i += 3;
    } else {
      if p[i] == c {
        matched = true;
      }
      i += 1;
    }
  }
  None
}

// ------------------------------------------------------------ inline check

/// True when the script only runs builtins with arguments known before
/// execution, so it can run synchronously on the JS thread.
pub fn can_inline(script: &Script, values: &[Value]) -> bool {
  let env = Env::default();
  script.0.iter().all(|ao| {
    std::iter::once(&ao.first)
      .chain(ao.rest.iter().map(|(_, p)| p))
      .all(|p| p.0.iter().all(|c| command_inline(c, values, &env)))
  })
}

fn command_inline(cmd: &Command, values: &[Value], env: &Env) -> bool {
  let mut argv = Vec::with_capacity(cmd.words.len());
  for w in &cmd.words {
    if !static_word(w, values, &mut argv) {
      return false;
    }
  }
  let redirects_ok = cmd.redirects.iter().all(|r| match r {
    Redirect::Dup { .. } => true,
    Redirect::File { target, .. } => {
      let mut tmp = Vec::new();
      matches!(target.0.as_slice(), [Part::Interp(_)]) || static_word(target, values, &mut tmp)
    }
  });
  let assigns_ok = cmd.assigns.iter().all(|(_, w)| {
    let mut tmp = Vec::new();
    static_word(w, values, &mut tmp)
  });
  if !redirects_ok || !assigns_ok {
    return false;
  }
  match argv.first() {
    None => true,
    Some(name) => builtins::is_builtin(name) && builtins::supported(&argv, env),
  }
}

/// Expands a word that has no variables, globs, tildes or substitutions.
fn static_word(w: &Word, values: &[Value], out: &mut Vec<String>) -> bool {
  if let [Part::Interp(i)] = w.0.as_slice()
    && let Some(Value::List(items)) = values.get(*i)
  {
    out.extend(items.iter().cloned());
    return true;
  }
  let mut s = String::new();
  for (i, part) in w.0.iter().enumerate() {
    match part {
      Part::Lit(l) => {
        if has_glob(l) || (i == 0 && l.starts_with('~')) {
          return false;
        }
        s.push_str(l);
      }
      Part::Quoted(q) => s.push_str(q),
      Part::Interp(idx) => match values.get(*idx) {
        Some(Value::Str(v)) => s.push_str(v),
        _ => return false,
      },
      Part::Var(..) | Part::Subst(..) => return false,
    }
  }
  out.push(s);
  true
}
