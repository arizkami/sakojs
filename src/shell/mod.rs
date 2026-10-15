//! `Sako.$`: a `Bun.$`-style shell that parses and runs commands itself
//! (no `/bin/sh`). See `parse` (syntax), `exec` (expansion, pipelines,
//! processes) and `builtins` (in-process commands).

mod builtins;
mod exec;
mod parse;

pub use exec::{Env, Output, Value};
pub use exec::normalize as normalize_path;

/// A parsed script, shared between runs of the same template call site.
pub type Parsed = std::sync::Arc<parse::Script>;

pub fn parse_script(texts: &[String], nvals: usize) -> Result<Parsed, String> {
  parse::parse(texts, nvals).map(std::sync::Arc::new)
}

/// A script ready to run.
pub struct Job {
  script: Parsed,
  values: Vec<Value>,
  cwd: std::path::PathBuf,
  env: Env,
}

impl Job {
  pub fn new(
    script: Parsed,
    values: Vec<Value>,
    cwd: std::path::PathBuf,
    env: Option<std::collections::HashMap<String, String>>,
  ) -> Job {
    Job { script, values, cwd, env: Env::new(env) }
  }

  /// Only builtins with arguments known up front: safe to run synchronously
  /// on the JS thread.
  pub fn can_inline(&self) -> bool {
    exec::can_inline(&self.script, &self.values)
  }

  /// Runs the script. With `live`, output is also written to the process's
  /// stdout/stderr as it is produced.
  pub fn run(self, live: bool) -> Output {
    exec::Shell::new(self.cwd, self.env, &self.values, live).run(&self.script)
  }
}

/// Quotes a string for safe use as one shell word (`$.escape`).
pub fn escape(s: &str) -> String {
  if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&b)) {
    return s.to_string();
  }
  let mut out = String::with_capacity(s.len() + 2);
  out.push('"');
  for c in s.chars() {
    if matches!(c, '"' | '\\' | '$' | '`') {
      out.push('\\');
    }
    out.push(c);
  }
  out.push('"');
  out
}
