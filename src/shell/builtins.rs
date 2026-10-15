//! In-process builtins for `Sako.$`.
//!
//! Besides the usual shell builtins (echo, cd, pwd, ...) and `Bun.$`'s file
//! commands (cat, ls, rm, mkdir, ...), common text tools (wc, head, tail,
//! grep, sort, uniq) run in-process too, so pipelines like `cat f | wc -l`
//! never spawn a process. A builtin that sees options it doesn't implement
//! reports itself unsupported (`supported`), and the real program runs.

use super::exec::{Env, Input, Shell, io_msg, resolve_command};
use std::io::Write;
use std::path::Path;

pub struct Ret {
  pub out: Vec<u8>,
  pub err: Vec<u8>,
  pub code: i32,
  /// `exit` was called.
  pub exit: bool,
}

impl Ret {
  fn ok(out: Vec<u8>) -> Self {
    Ret { out, err: Vec::new(), code: 0, exit: false }
  }
  fn code(code: i32) -> Self {
    Ret { out: Vec::new(), err: Vec::new(), code, exit: false }
  }
  fn fail(err: String) -> Self {
    Ret { out: Vec::new(), err: err.into_bytes(), code: 1, exit: false }
  }
}

const BUILTINS: &[&str] = &[
  "echo", "pwd", "cd", "true", "false", "exit", "export", "unset", "cat", "ls", "mkdir", "rm", "touch", "mv", "cp",
  "which", "dirname", "basename", "seq", "wc", "head", "tail", "grep", "sort", "uniq", ":",
];

pub fn is_builtin(name: &str) -> bool {
  BUILTINS.contains(&name)
}

/// Splits leading `-abc` options into chars. Stops at `--` or the first
/// operand. Returns (flags, index of first operand).
fn short_flags(argv: &[String]) -> (Vec<char>, usize) {
  let mut flags = Vec::new();
  let mut i = 1;
  while i < argv.len() {
    let a = &argv[i];
    if a == "--" {
      i += 1;
      break;
    }
    if a.len() > 1 && a.starts_with('-') {
      flags.extend(a[1..].chars());
      i += 1;
    } else {
      break;
    }
  }
  (flags, i)
}

fn only_flags(argv: &[String], allowed: &str) -> bool {
  let (flags, _) = short_flags(argv);
  flags.iter().all(|c| allowed.contains(*c))
}

/// Parses `-n N`, `-nN`, `-N` (and `+N` for tail) line counts.
/// Returns (count, from_start, operands) or None if unsupported.
fn line_count_args(argv: &[String], allow_plus: bool) -> Option<(usize, bool, Vec<String>)> {
  let mut n = 10usize;
  let mut from_start = false;
  let mut operands = Vec::new();
  let mut i = 1;
  let parse = |s: &str, from_start: &mut bool| -> Option<usize> {
    if let Some(rest) = s.strip_prefix('+') {
      if !allow_plus {
        return None;
      }
      *from_start = true;
      return rest.parse().ok();
    }
    s.parse().ok()
  };
  while i < argv.len() {
    let a = argv[i].as_str();
    if a == "-n" {
      n = parse(argv.get(i + 1)?, &mut from_start)?;
      i += 2;
    } else if let Some(v) = a.strip_prefix("-n") {
      n = parse(v, &mut from_start)?;
      i += 1;
    } else if a.len() > 1 && a.starts_with('-') && a[1..].bytes().all(|b| b.is_ascii_digit()) {
      n = a[1..].parse().ok()?;
      i += 1;
    } else if a.starts_with('-') && a != "-" {
      return None;
    } else {
      operands.push(a.to_string());
      i += 1;
    }
  }
  if operands.len() > 1 {
    return None; // multi-file headers: use the real program
  }
  Some((n, from_start, operands))
}

/// Whether the builtin implements this invocation; if not, the real program
/// runs instead.
pub fn supported(argv: &[String], env: &Env) -> bool {
  let name = argv[0].as_str();
  match name {
    "cat" => argv[1..].iter().all(|a| a == "-" || !a.starts_with('-')),
    "ls" => only_flags(argv, "a1A"),
    "mkdir" => only_flags(argv, "p"),
    "rm" => only_flags(argv, "rRf"),
    "touch" | "mv" => only_flags(argv, "f"),
    "cp" => only_flags(argv, "rRf"),
    "seq" => argv.len() >= 2 && argv.len() <= 4 && argv[1..].iter().all(|a| a.parse::<i64>().is_ok()),
    "wc" => only_flags(argv, "lwc"),
    "head" => {
      if argv.iter().any(|a| a.starts_with("-c")) {
        return false;
      }
      line_count_args(argv, false).is_some()
    }
    "tail" => line_count_args(argv, true).is_some(),
    "grep" => grep_options(argv).is_some(),
    "sort" => only_flags(argv, "rnu") && c_locale(env),
    "uniq" => {
      let (_, first) = short_flags(argv);
      only_flags(argv, "cdu") && argv.len() - first <= 1
    }
    _ => true,
  }
}

/// Byte-order sorting matches the real `sort` only in the C locale.
fn c_locale(env: &Env) -> bool {
  let v = env.get("LC_ALL").filter(|v| !v.is_empty())
    .or_else(|| env.get("LC_COLLATE").filter(|v| !v.is_empty()))
    .or_else(|| env.get("LANG").filter(|v| !v.is_empty()));
  match v {
    None => true,
    Some(v) => v == "C" || v == "POSIX" || v.starts_with("C.") || v.starts_with("POSIX."),
  }
}

pub fn run(sh: &mut Shell, argv: &[String], assigns: &[(String, String)], stdin: Input) -> Ret {
  let _ = assigns;
  let args = &argv[1..];
  match argv[0].as_str() {
    "echo" => echo(args),
    "pwd" => Ret::ok(format!("{}\n", sh.cwd.display()).into_bytes()),
    "cd" => cd(sh, args),
    "true" | ":" => Ret::code(0),
    "false" => Ret::code(1),
    "exit" => {
      let code = match args.first() {
        None => sh.last,
        Some(a) => match a.parse::<i32>() {
          Ok(n) => n & 0xff,
          Err(_) => return Ret { exit: true, ..Ret::fail(format!("exit: numeric argument required: {a}\n")) },
        },
      };
      Ret { out: Vec::new(), err: Vec::new(), code, exit: true }
    }
    "export" => {
      for a in args {
        match a.split_once('=') {
          Some((k, v)) => sh.env.set(k, Some(v.to_string())),
          None => {
            if let Some(v) = sh.vars.get(a).cloned() {
              sh.env.set(a, Some(v));
            }
          }
        }
      }
      Ret::code(0)
    }
    "unset" => {
      for a in args {
        sh.vars.remove(a);
        sh.env.set(a, None);
      }
      Ret::code(0)
    }
    "cat" => cat(sh, args, stdin),
    "ls" => ls(sh, argv),
    "mkdir" => mkdir(sh, argv),
    "rm" => rm(sh, argv),
    "touch" => touch(sh, argv),
    "mv" => mv(sh, argv),
    "cp" => cp(sh, argv),
    "which" => which(sh, args),
    "dirname" => match args.first() {
      Some(p) => Ret::ok(format!("{}\n", dirname(p)).into_bytes()),
      None => Ret::fail("dirname: missing operand\n".into()),
    },
    "basename" => match args.first() {
      Some(p) => {
        let mut b = basename(p);
        if let Some(suffix) = args.get(1)
          && b.len() > suffix.len()
          && b.ends_with(suffix.as_str())
        {
          b.truncate(b.len() - suffix.len());
        }
        Ret::ok(format!("{b}\n").into_bytes())
      }
      None => Ret::fail("basename: missing operand\n".into()),
    },
    "seq" => seq(args),
    "wc" => wc(sh, argv, stdin),
    "head" => head_tail(sh, argv, stdin, true),
    "tail" => head_tail(sh, argv, stdin, false),
    "grep" => grep(sh, argv, stdin),
    "sort" => sort(sh, argv, stdin),
    "uniq" => uniq(sh, argv, stdin),
    other => Ret::fail(format!("sako: builtin not implemented: {other}\n")),
  }
}

// ------------------------------------------------------------------ helpers

/// Reads the named files (or stdin for none / "-"), collecting errors.
fn read_inputs(sh: &Shell, cmd: &str, files: &[String], stdin: Input) -> (Vec<(String, Vec<u8>)>, Vec<u8>, i32) {
  let mut out = Vec::new();
  let mut err = Vec::new();
  let mut code = 0;
  if files.is_empty() {
    match stdin.read_all() {
      Ok(b) => out.push((String::new(), b)),
      Err(e) => {
        err.extend(format!("{cmd}: stdin: {}\n", io_msg(&e)).bytes());
        code = 1;
      }
    }
    return (out, err, code);
  }
  let mut stdin = Some(stdin);
  for f in files {
    if f == "-" {
      let data = stdin.take().map_or(Ok(Vec::new()), |s| s.read_all());
      match data {
        Ok(b) => out.push(("-".into(), b)),
        Err(e) => {
          err.extend(format!("{cmd}: -: {}\n", io_msg(&e)).bytes());
          code = 1;
        }
      }
      continue;
    }
    match std::fs::read(sh.resolve_path(f)) {
      Ok(b) => out.push((f.clone(), b)),
      Err(e) => {
        err.extend(format!("{cmd}: {f}: {}\n", io_msg(&e)).bytes());
        code = 1;
      }
    }
  }
  (out, err, code)
}

/// Splits into lines without their '\n' (a final unterminated line counts).
fn lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
  let trimmed = data.strip_suffix(b"\n").unwrap_or(data);
  let empty = data.is_empty();
  trimmed.split(|&b| b == b'\n').filter(move |_| !empty)
}

// ---------------------------------------------------------------- builtins

fn echo(args: &[String]) -> Ret {
  let mut newline = true;
  let mut escapes = false;
  let mut i = 0;
  while i < args.len() {
    let a = &args[i];
    if a.len() > 1 && a.starts_with('-') && a[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E')) {
      for c in a[1..].chars() {
        match c {
          'n' => newline = false,
          'e' => escapes = true,
          _ => escapes = false,
        }
      }
      i += 1;
    } else {
      break;
    }
  }
  let mut out = Vec::new();
  for (j, a) in args[i..].iter().enumerate() {
    if j > 0 {
      out.push(b' ');
    }
    if escapes {
      if !echo_escapes(a, &mut out) {
        return Ret::ok(out); // \c: stop output
      }
    } else {
      out.extend_from_slice(a.as_bytes());
    }
  }
  if newline {
    out.push(b'\n');
  }
  Ret::ok(out)
}

/// Expands echo -e escapes; returns false on `\c`.
fn echo_escapes(s: &str, out: &mut Vec<u8>) -> bool {
  let b = s.as_bytes();
  let mut i = 0;
  while i < b.len() {
    if b[i] != b'\\' || i + 1 == b.len() {
      out.push(b[i]);
      i += 1;
      continue;
    }
    i += 1;
    let c = b[i];
    i += 1;
    match c {
      b'n' => out.push(b'\n'),
      b't' => out.push(b'\t'),
      b'r' => out.push(b'\r'),
      b'a' => out.push(7),
      b'b' => out.push(8),
      b'e' => out.push(27),
      b'f' => out.push(12),
      b'v' => out.push(11),
      b'\\' => out.push(b'\\'),
      b'c' => return false,
      b'0' => {
        let mut v = 0u32;
        let mut k = 0;
        while k < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
          v = v * 8 + (b[i] - b'0') as u32;
          i += 1;
          k += 1;
        }
        out.push(v as u8);
      }
      b'x' => {
        let mut v = 0u32;
        let mut k = 0;
        while k < 2 && i < b.len() && b[i].is_ascii_hexdigit() {
          v = v * 16 + (b[i] as char).to_digit(16).unwrap();
          i += 1;
          k += 1;
        }
        if k == 0 {
          out.extend_from_slice(b"\\x");
        } else {
          out.push(v as u8);
        }
      }
      other => {
        out.push(b'\\');
        out.push(other);
      }
    }
  }
  true
}

fn cd(sh: &mut Shell, args: &[String]) -> Ret {
  let target = match args.first().map(String::as_str) {
    None | Some("~") => sh.lookup("HOME").unwrap_or_else(|| "/".into()),
    Some("-") => match sh.lookup("OLDPWD") {
      Some(p) => p,
      None => return Ret::fail("cd: OLDPWD not set\n".into()),
    },
    Some(p) => p.to_string(),
  };
  let path = sh.resolve_path(&target);
  match std::fs::metadata(&path) {
    Ok(m) if m.is_dir() => {
      let old = sh.cwd.display().to_string();
      sh.vars.insert("OLDPWD".into(), old);
      sh.cwd = path;
      Ret::code(0)
    }
    Ok(_) => Ret::fail(format!("cd: not a directory: {target}\n")),
    Err(_) => Ret::fail(format!("cd: no such file or directory: {target}\n")),
  }
}

fn cat(sh: &Shell, args: &[String], stdin: Input) -> Ret {
  let (inputs, err, code) = read_inputs(sh, "cat", args, stdin);
  let mut out = Vec::with_capacity(inputs.iter().map(|(_, b)| b.len()).sum());
  for (_, b) in inputs {
    if out.is_empty() {
      out = b;
    } else {
      out.extend_from_slice(&b);
    }
  }
  Ret { out, err, code, exit: false }
}

/// Directory entry names packed into one buffer: (bytes, [start, end) spans).
struct Names {
  buf: Vec<u8>,
  spans: Vec<(u32, u32)>,
}

impl Names {
  fn get(&self, i: usize) -> &[u8] {
    let (s, e) = self.spans[i];
    &self.buf[s as usize..e as usize]
  }
}

/// Lists a directory with raw `getdents64` into a reused buffer: no `fstat`,
/// no per-call 32 KB `DIR` allocation, no per-entry allocations. Includes
/// `.` and `..`.
#[cfg(target_os = "linux")]
fn list_dir(path: &Path) -> std::io::Result<Names> {
  use std::cell::RefCell;
  use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
  use std::os::unix::ffi::OsStrExt;
  thread_local! {
    static DENTS: RefCell<Vec<u8>> = RefCell::new(vec![0u8; 32 * 1024]);
  }
  let c = std::ffi::CString::new(path.as_os_str().as_bytes())
    .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
  // SAFETY: valid C string; the returned fd is owned and closed on drop.
  let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
  if fd < 0 {
    return Err(std::io::Error::last_os_error());
  }
  // SAFETY: `fd` was just opened and is not owned elsewhere.
  let fd = unsafe { OwnedFd::from_raw_fd(fd) };
  let mut names = Names { buf: Vec::with_capacity(1024), spans: Vec::with_capacity(64) };
  DENTS.with(|dents| {
    let mut dents = dents.borrow_mut();
    loop {
      // SAFETY: the buffer is valid for `len` bytes for the duration of the call.
      let n = unsafe {
        libc::syscall(libc::SYS_getdents64, fd.as_raw_fd(), dents.as_mut_ptr(), dents.len())
      };
      if n < 0 {
        return Err(std::io::Error::last_os_error());
      }
      if n == 0 {
        return Ok(());
      }
      // struct linux_dirent64 { u64 ino; i64 off; u16 reclen; u8 type; char name[]; }
      let mut off = 0usize;
      while off < n as usize {
        let rec = &dents[off..];
        let reclen = u16::from_ne_bytes([rec[16], rec[17]]) as usize;
        let name = &rec[19..reclen];
        let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        let start = names.buf.len() as u32;
        names.buf.extend_from_slice(&name[..len]);
        names.spans.push((start, names.buf.len() as u32));
        off += reclen;
      }
    }
  })?;
  Ok(names)
}

/// Lists a directory without per-entry allocations (`readdir` straight into
/// one buffer). Includes `.` and `..`.
#[cfg(all(unix, not(target_os = "linux")))]
fn list_dir(path: &Path) -> std::io::Result<Names> {
  use std::os::unix::ffi::OsStrExt;
  let c = std::ffi::CString::new(path.as_os_str().as_bytes())
    .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
  let mut names = Names { buf: Vec::with_capacity(1024), spans: Vec::with_capacity(64) };
  // SAFETY: `c` is a valid C string; the DIR handle is closed before return
  // and each dirent is only read before the next readdir call.
  unsafe {
    let d = libc::opendir(c.as_ptr());
    if d.is_null() {
      return Err(std::io::Error::last_os_error());
    }
    loop {
      let e = libc::readdir(d);
      if e.is_null() {
        break;
      }
      let name = std::ffi::CStr::from_ptr((*e).d_name.as_ptr()).to_bytes();
      let start = names.buf.len() as u32;
      names.buf.extend_from_slice(name);
      names.spans.push((start, names.buf.len() as u32));
    }
    libc::closedir(d);
  }
  Ok(names)
}

#[cfg(not(unix))]
fn list_dir(path: &Path) -> std::io::Result<Names> {
  let mut names = Names { buf: Vec::new(), spans: Vec::new() };
  for n in [".", ".."].into_iter().map(String::from).chain(
    std::fs::read_dir(path)?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()),
  ) {
    let start = names.buf.len() as u32;
    names.buf.extend_from_slice(n.as_bytes());
    names.spans.push((start, names.buf.len() as u32));
  }
  Ok(names)
}

/// Case-insensitive byte order (like `Bun.$`'s ls), ties broken by bytes.
fn cmp_names(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
  if a.is_ascii() && b.is_ascii() {
    a.iter().map(u8::to_ascii_lowercase).cmp(b.iter().map(u8::to_ascii_lowercase)).then_with(|| a.cmp(b))
  } else {
    match (std::str::from_utf8(a), std::str::from_utf8(b)) {
      (Ok(a), Ok(b)) => super::exec::name_order(a, b),
      _ => a.cmp(b),
    }
  }
}

fn ls(sh: &Shell, argv: &[String]) -> Ret {
  let (flags, first) = short_flags(argv);
  let all = flags.contains(&'a');
  let almost = flags.contains(&'A');
  let mut paths: Vec<String> = argv[first..].to_vec();
  if paths.is_empty() {
    paths.push(".".into());
  }
  let mut out = Vec::new();
  let mut err = Vec::new();
  let mut code = 0;
  let mut files = Vec::new();
  let mut dirs = Vec::new();
  // Open each path as a directory directly (no separate stat).
  for p in &paths {
    match list_dir(&sh.resolve_path(p)) {
      Ok(names) => dirs.push((p.clone(), names)),
      Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => files.push(p.clone()),
      Err(e) => {
        err.extend(format!("ls: {p}: {}\n", io_msg(&e)).bytes());
        code = 1;
      }
    }
  }
  sort_names(&mut files);
  for f in &files {
    out.extend(f.bytes());
    out.push(b'\n');
  }
  let headers = paths.len() > 1;
  for (i, (d, names)) in dirs.into_iter().enumerate() {
    let mut order: Vec<usize> = (0..names.spans.len())
      .filter(|&k| {
        let n = names.get(k);
        if n == b"." || n == b".." {
          all
        } else {
          all || almost || !n.starts_with(b".")
        }
      })
      .collect();
    order.sort_unstable_by(|&x, &y| cmp_names(names.get(x), names.get(y)));
    if headers {
      if i > 0 || !files.is_empty() {
        out.push(b'\n');
      }
      out.extend(format!("{d}:\n").bytes());
    }
    out.reserve(names.buf.len() + order.len());
    for k in order {
      out.extend_from_slice(names.get(k));
      out.push(b'\n');
    }
  }
  Ret { out, err, code, exit: false }
}

/// Case-insensitive order (like `Bun.$`'s ls); allocation-free for ASCII.
fn sort_names(names: &mut [String]) {
  names.sort_unstable_by(|a, b| {
    if a.is_ascii() && b.is_ascii() {
      let la = a.bytes().map(|c| c.to_ascii_lowercase());
      let lb = b.bytes().map(|c| c.to_ascii_lowercase());
      la.cmp(lb).then_with(|| a.cmp(b))
    } else {
      super::exec::name_order(a, b)
    }
  });
}

fn mkdir(sh: &Shell, argv: &[String]) -> Ret {
  let (flags, first) = short_flags(argv);
  let parents = flags.contains(&'p');
  let mut err = Vec::new();
  for d in &argv[first..] {
    let p = sh.resolve_path(d);
    let res = if parents { std::fs::create_dir_all(&p) } else { std::fs::create_dir(&p) };
    if let Err(e) = res {
      err.extend(format!("mkdir: {d}: {}\n", io_msg(&e)).bytes());
    }
  }
  let code = i32::from(!err.is_empty());
  Ret { out: Vec::new(), err, code, exit: false }
}

fn rm(sh: &Shell, argv: &[String]) -> Ret {
  let (flags, first) = short_flags(argv);
  let recursive = flags.contains(&'r') || flags.contains(&'R');
  let force = flags.contains(&'f');
  let mut err = Vec::new();
  for f in &argv[first..] {
    let p = sh.resolve_path(f);
    let res = match std::fs::symlink_metadata(&p) {
      Ok(m) if m.is_dir() => {
        if recursive {
          std::fs::remove_dir_all(&p)
        } else {
          err.extend(format!("rm: {f}: is a directory\n").bytes());
          continue;
        }
      }
      Ok(_) => std::fs::remove_file(&p),
      Err(e) => Err(e),
    };
    if let Err(e) = res
      && !(force && e.kind() == std::io::ErrorKind::NotFound)
    {
      err.extend(format!("rm: {f}: {}\n", io_msg(&e)).bytes());
    }
  }
  let code = i32::from(!err.is_empty());
  Ret { out: Vec::new(), err, code, exit: false }
}

fn touch(sh: &Shell, argv: &[String]) -> Ret {
  let (_, first) = short_flags(argv);
  let mut err = Vec::new();
  for f in &argv[first..] {
    let p = sh.resolve_path(f);
    let res = std::fs::OpenOptions::new()
      .create(true)
      .append(true)
      .open(&p)
      .and_then(|file| file.set_modified(std::time::SystemTime::now()));
    if let Err(e) = res {
      err.extend(format!("touch: {f}: {}\n", io_msg(&e)).bytes());
    }
  }
  let code = i32::from(!err.is_empty());
  Ret { out: Vec::new(), err, code, exit: false }
}

/// `dest` for `src` when `dest` may be a directory.
fn target_path(sh: &Shell, src: &str, dest: &str) -> std::path::PathBuf {
  let d = sh.resolve_path(dest);
  if d.is_dir() { d.join(basename(src)) } else { d }
}

fn mv(sh: &Shell, argv: &[String]) -> Ret {
  let (_, first) = short_flags(argv);
  let ops = &argv[first..];
  if ops.len() < 2 {
    return Ret::fail("mv: missing destination operand\n".into());
  }
  let (srcs, dest) = ops.split_at(ops.len() - 1);
  let dest = &dest[0];
  if srcs.len() > 1 && !sh.resolve_path(dest).is_dir() {
    return Ret::fail(format!("mv: {dest}: Not a directory\n"));
  }
  let mut err = Vec::new();
  for s in srcs {
    let from = sh.resolve_path(s);
    let to = target_path(sh, s, dest);
    let res = std::fs::rename(&from, &to).or_else(|e| {
      // Across filesystems: copy then remove.
      if from.is_file() {
        std::fs::copy(&from, &to).and_then(|_| std::fs::remove_file(&from))
      } else {
        Err(e)
      }
    });
    if let Err(e) = res {
      err.extend(format!("mv: {s}: {}\n", io_msg(&e)).bytes());
    }
  }
  let code = i32::from(!err.is_empty());
  Ret { out: Vec::new(), err, code, exit: false }
}

fn copy_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
  if from.is_dir() {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
      let e = e?;
      copy_recursive(&e.path(), &to.join(e.file_name()))?;
    }
    Ok(())
  } else {
    std::fs::copy(from, to).map(|_| ())
  }
}

fn cp(sh: &Shell, argv: &[String]) -> Ret {
  let (flags, first) = short_flags(argv);
  let recursive = flags.contains(&'r') || flags.contains(&'R');
  let ops = &argv[first..];
  if ops.len() < 2 {
    return Ret::fail("cp: missing destination operand\n".into());
  }
  let (srcs, dest) = ops.split_at(ops.len() - 1);
  let dest = &dest[0];
  let mut err = Vec::new();
  for s in srcs {
    let from = sh.resolve_path(s);
    let to = target_path(sh, s, dest);
    let res = if from.is_dir() {
      if recursive {
        copy_recursive(&from, &to)
      } else {
        err.extend(format!("cp: {s}: is a directory (not copied)\n").bytes());
        continue;
      }
    } else {
      std::fs::copy(&from, &to).map(|_| ())
    };
    if let Err(e) = res {
      err.extend(format!("cp: {s}: {}\n", io_msg(&e)).bytes());
    }
  }
  let code = i32::from(!err.is_empty());
  Ret { out: Vec::new(), err, code, exit: false }
}

fn which(sh: &Shell, args: &[String]) -> Ret {
  let mut out = Vec::new();
  let mut code = 0;
  for name in args {
    if is_builtin(name) && !name.contains('/') && resolve_command(name, &sh.cwd, &sh.env).is_none() {
      out.extend(format!("{name}: shell built-in command\n").bytes());
      continue;
    }
    match resolve_command(name, &sh.cwd, &sh.env) {
      Some(p) => {
        out.extend(p.display().to_string().bytes());
        out.push(b'\n');
      }
      None => code = 1,
    }
  }
  Ret { out, err: Vec::new(), code, exit: false }
}

pub fn dirname(p: &str) -> String {
  let t = p.trim_end_matches('/');
  if t.is_empty() {
    return if p.starts_with('/') { "/".into() } else { ".".into() };
  }
  match t.rfind('/') {
    None => ".".into(),
    Some(i) => {
      let d = t[..i].trim_end_matches('/');
      if d.is_empty() { "/".into() } else { d.to_string() }
    }
  }
}

pub fn basename(p: &str) -> String {
  let t = p.trim_end_matches('/');
  if t.is_empty() {
    return if p.starts_with('/') { "/".into() } else { String::new() };
  }
  t.rsplit('/').next().unwrap_or(t).to_string()
}

fn seq(args: &[String]) -> Ret {
  let n: Vec<i64> = args.iter().filter_map(|a| a.parse().ok()).collect();
  let (first, step, last) = match n.as_slice() {
    [l] => (1, 1, *l),
    [f, l] => (*f, 1, *l),
    [f, s, l] => (*f, *s, *l),
    _ => return Ret::fail("seq: invalid arguments\n".into()),
  };
  if step == 0 {
    return Ret::fail("seq: invalid Zero increment value: '0'\n".into());
  }
  let mut out = Vec::new();
  let mut i = first;
  while (step > 0 && i <= last) || (step < 0 && i >= last) {
    let _ = writeln!(out, "{i}");
    i += step;
  }
  Ret::ok(out)
}

fn wc(sh: &Shell, argv: &[String], stdin: Input) -> Ret {
  let (flags, first) = short_flags(argv);
  let (mut l, mut w, mut c) = (flags.contains(&'l'), flags.contains(&'w'), flags.contains(&'c'));
  if !l && !w && !c {
    (l, w, c) = (true, true, true);
  }
  let files = &argv[first..];
  let (inputs, err, code) = read_inputs(sh, "wc", files, stdin);
  let counts: Vec<(String, [usize; 3])> = inputs
    .iter()
    .map(|(name, data)| {
      let lines = data.iter().filter(|&&b| b == b'\n').count();
      let mut words = 0;
      let mut in_word = false;
      for &b in data {
        let space = matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
        if !space && !in_word {
          words += 1;
        }
        in_word = !space;
      }
      (name.clone(), [lines, words, data.len()])
    })
    .collect();
  let selected = [l, w, c];
  let nsel = selected.iter().filter(|&&s| s).count();
  // GNU wc column width: 1 for a single count of a single input, else the
  // digits of the total size of regular files (7 when reading a pipe).
  let width = if nsel == 1 && files.len() <= 1 {
    1
  } else if files.is_empty() || files.iter().any(|f| f == "-") {
    7
  } else {
    let total: usize = counts.iter().map(|(_, c)| c[2]).sum();
    total.to_string().len().max(1)
  };
  let mut out = Vec::new();
  let mut totals = [0usize; 3];
  let emit = |out: &mut Vec<u8>, counts: &[usize; 3], name: &str| {
    let mut cols = Vec::new();
    for k in 0..3 {
      if selected[k] {
        cols.push(format!("{:>width$}", counts[k]));
      }
    }
    out.extend(cols.join(" ").bytes());
    if !name.is_empty() {
      out.push(b' ');
      out.extend(name.bytes());
    }
    out.push(b'\n');
  };
  for (name, cnt) in &counts {
    for k in 0..3 {
      totals[k] += cnt[k];
    }
    emit(&mut out, cnt, name);
  }
  if counts.len() > 1 {
    emit(&mut out, &totals, "total");
  }
  Ret { out, err, code, exit: false }
}

fn head_tail(sh: &Shell, argv: &[String], stdin: Input, head: bool) -> Ret {
  let Some((n, from_start, operands)) = line_count_args(argv, !head) else {
    return Ret::fail(format!("{}: unsupported options\n", argv[0]));
  };
  let (inputs, err, code) = read_inputs(sh, &argv[0], &operands, stdin);
  let mut out = Vec::new();
  for (_, data) in inputs {
    if head {
      let mut seen = 0;
      let mut end = data.len();
      for (i, &b) in data.iter().enumerate() {
        if seen == n {
          end = i;
          break;
        }
        if b == b'\n' {
          seen += 1;
        }
      }
      if n == 0 {
        end = 0;
      }
      out.extend_from_slice(&data[..end]);
    } else if from_start {
      // tail -n +N: from line N on.
      let skip = n.saturating_sub(1);
      let mut seen = 0;
      let mut start = if skip == 0 { 0 } else { data.len() };
      for (i, &b) in data.iter().enumerate() {
        if seen == skip {
          start = i;
          break;
        }
        if b == b'\n' {
          seen += 1;
        }
      }
      out.extend_from_slice(&data[start..]);
    } else {
      if n == 0 {
        continue;
      }
      // Last N lines: scan back over newlines (ignoring a final one).
      let body = data.strip_suffix(b"\n").unwrap_or(&data);
      let mut start = 0;
      let mut count = 0;
      for i in (0..body.len()).rev() {
        if body[i] == b'\n' {
          count += 1;
          if count == n {
            start = i + 1;
            break;
          }
        }
      }
      out.extend_from_slice(&data[start..]);
    }
  }
  Ret { out, err, code, exit: false }
}

struct GrepOpts {
  invert: bool,
  ignore_case: bool,
  count: bool,
  line_numbers: bool,
  fixed: bool,
  extended: bool,
  whole_line: bool,
  word: bool,
  quiet: bool,
  no_messages: bool,
  with_name: Option<bool>,
  patterns: Vec<String>,
  files: Vec<String>,
}

fn grep_options(argv: &[String]) -> Option<GrepOpts> {
  let mut o = GrepOpts {
    invert: false,
    ignore_case: false,
    count: false,
    line_numbers: false,
    fixed: false,
    extended: false,
    whole_line: false,
    word: false,
    quiet: false,
    no_messages: false,
    with_name: None,
    patterns: Vec::new(),
    files: Vec::new(),
  };
  let mut i = 1;
  let mut operands = Vec::new();
  while i < argv.len() {
    let a = &argv[i];
    if a == "--" {
      operands.extend(argv[i + 1..].iter().cloned());
      break;
    }
    if a.len() > 1 && a.starts_with('-') && !a.starts_with("--") {
      let chars: Vec<char> = a[1..].chars().collect();
      let mut k = 0;
      while k < chars.len() {
        match chars[k] {
          'v' => o.invert = true,
          'i' => o.ignore_case = true,
          'c' => o.count = true,
          'n' => o.line_numbers = true,
          'F' => o.fixed = true,
          'E' => o.extended = true,
          'G' => o.extended = false,
          'x' => o.whole_line = true,
          'w' => o.word = true,
          'q' => o.quiet = true,
          's' => o.no_messages = true,
          'h' => o.with_name = Some(false),
          'H' => o.with_name = Some(true),
          'e' => {
            let rest: String = chars[k + 1..].iter().collect();
            if rest.is_empty() {
              i += 1;
              o.patterns.push(argv.get(i)?.clone());
            } else {
              o.patterns.push(rest);
            }
            k = chars.len();
            continue;
          }
          _ => return None, // -r, -o, -P, -A/-B/-C, -l, ...: use real grep
        }
        k += 1;
      }
    } else if a.starts_with("--") {
      return None;
    } else {
      operands.push(a.clone());
    }
    i += 1;
  }
  if o.patterns.is_empty() {
    if operands.is_empty() {
      return None;
    }
    o.patterns.push(operands.remove(0));
  }
  o.files = operands;
  Some(o)
}

/// Translates a POSIX basic regular expression to `regex` syntax.
fn bre_to_ere(p: &str) -> String {
  let mut out = String::with_capacity(p.len() + 8);
  let chars: Vec<char> = p.chars().collect();
  let mut i = 0;
  let mut in_class = false;
  while i < chars.len() {
    let c = chars[i];
    if in_class {
      out.push(c);
      if c == ']' && !out.ends_with("[]") && !out.ends_with("[^]") {
        in_class = false;
      }
      i += 1;
      continue;
    }
    match c {
      '\\' if i + 1 < chars.len() => {
        let n = chars[i + 1];
        match n {
          '(' | ')' | '{' | '}' | '|' | '+' | '?' => out.push(n),
          _ => {
            out.push('\\');
            out.push(n);
          }
        }
        i += 2;
        continue;
      }
      '(' | ')' | '{' | '}' | '|' | '+' | '?' => {
        out.push('\\');
        out.push(c);
      }
      '*' if out.is_empty() || out.ends_with('^') => out.push_str("\\*"),
      '[' => {
        in_class = true;
        out.push('[');
      }
      c => out.push(c),
    }
    i += 1;
  }
  out
}

fn grep(sh: &Shell, argv: &[String], stdin: Input) -> Ret {
  let Some(o) = grep_options(argv) else {
    return Ret { code: 2, ..Ret::fail("grep: unsupported options\n".into()) };
  };
  let alternatives: Vec<String> = o
    .patterns
    .iter()
    .flat_map(|p| p.split('\n').map(str::to_string).collect::<Vec<_>>())
    .map(|p| if o.fixed { regex::escape(&p) } else if o.extended { p } else { bre_to_ere(&p) })
    .collect();
  let mut pattern = alternatives.iter().map(|p| format!("(?:{p})")).collect::<Vec<_>>().join("|");
  if o.whole_line {
    pattern = format!("^(?:{pattern})$");
  } else if o.word {
    pattern = format!(r"\b(?:{pattern})\b");
  }
  let re = match regex::bytes::RegexBuilder::new(&pattern).case_insensitive(o.ignore_case).build() {
    Ok(r) => r,
    Err(e) => return Ret { code: 2, ..Ret::fail(format!("grep: invalid pattern: {e}\n")) },
  };
  let (inputs, mut err, read_code) = read_inputs(sh, "grep", &o.files, stdin);
  if o.no_messages {
    err.clear();
  }
  let show_name = o.with_name.unwrap_or(o.files.len() > 1);
  let mut out = Vec::new();
  let mut any = false;
  for (name, data) in &inputs {
    let mut count = 0;
    for (ln, line) in lines(data).enumerate() {
      if re.is_match(line) == o.invert {
        continue;
      }
      any = true;
      count += 1;
      if o.quiet {
        return Ret::code(0);
      }
      if o.count {
        continue;
      }
      if show_name {
        out.extend(if name.is_empty() { "(standard input)" } else { name }.bytes());
        out.push(b':');
      }
      if o.line_numbers {
        let _ = write!(out, "{}:", ln + 1);
      }
      out.extend_from_slice(line);
      out.push(b'\n');
    }
    if o.count {
      if show_name {
        out.extend(if name.is_empty() { "(standard input)" } else { name }.bytes());
        out.push(b':');
      }
      let _ = writeln!(out, "{count}");
    }
  }
  let code = if read_code != 0 && !any { 2 } else if any { 0 } else { 1 };
  Ret { out, err, code, exit: false }
}

/// Leading numeric value as GNU `sort -n` sees it (non-numbers sort as 0).
fn numeric_key(line: &[u8]) -> f64 {
  let s = std::str::from_utf8(line).unwrap_or("");
  let s = s.trim_start();
  let end = s
    .char_indices()
    .take_while(|(i, c)| c.is_ascii_digit() || *c == '.' || (*i == 0 && *c == '-'))
    .map(|(i, c)| i + c.len_utf8())
    .last()
    .unwrap_or(0);
  s[..end].parse().unwrap_or(0.0)
}

fn sort(sh: &Shell, argv: &[String], stdin: Input) -> Ret {
  let (flags, first) = short_flags(argv);
  let (reverse, numeric, unique) = (flags.contains(&'r'), flags.contains(&'n'), flags.contains(&'u'));
  let (inputs, err, code) = read_inputs(sh, "sort", &argv[first..], stdin);
  let mut all: Vec<&[u8]> = inputs.iter().flat_map(|(_, d)| lines(d)).collect();
  if numeric {
    all.sort_by(|a, b| numeric_key(a).partial_cmp(&numeric_key(b)).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(b)));
  } else {
    all.sort_unstable();
  }
  if reverse {
    all.reverse();
  }
  if unique {
    if numeric {
      all.dedup_by(|a, b| numeric_key(a) == numeric_key(b));
    } else {
      all.dedup();
    }
  }
  let mut out = Vec::with_capacity(all.iter().map(|l| l.len() + 1).sum());
  for l in all {
    out.extend_from_slice(l);
    out.push(b'\n');
  }
  Ret { out, err, code, exit: false }
}

fn uniq(sh: &Shell, argv: &[String], stdin: Input) -> Ret {
  let (flags, first) = short_flags(argv);
  let (count, dups, uniques) = (flags.contains(&'c'), flags.contains(&'d'), flags.contains(&'u'));
  let (inputs, err, code) = read_inputs(sh, "uniq", &argv[first..], stdin);
  let mut out = Vec::new();
  for (_, data) in &inputs {
    let mut iter = lines(data).peekable();
    while let Some(line) = iter.next() {
      let mut n = 1;
      while iter.peek() == Some(&line) {
        iter.next();
        n += 1;
      }
      if (dups && n == 1) || (uniques && n > 1) {
        continue;
      }
      if count {
        let _ = write!(out, "{n:>7} ");
      }
      out.extend_from_slice(line);
      out.push(b'\n');
    }
  }
  Ret { out, err, code, exit: false }
}
