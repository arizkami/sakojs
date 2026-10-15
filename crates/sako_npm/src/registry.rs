//! npm registry access: package metadata ("packuments") and version picking.

use nodejs_semver::{Range, Version};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// One published version of a package (the fields an installer needs).
#[derive(Debug, Clone)]
pub struct Manifest {
  pub name: String,
  pub version: String,
  pub dependencies: Vec<(String, String)>,
  pub optional_dependencies: Vec<(String, String)>,
  /// (name, range, optional)
  pub peer_dependencies: Vec<(String, String, bool)>,
  /// (command name, path inside the package)
  pub bin: Vec<(String, String)>,
  pub os: Vec<String>,
  pub cpu: Vec<String>,
  pub tarball: String,
  /// SRI string (`sha512-...`) or `sha1-<hex>` from the legacy shasum.
  pub integrity: String,
  pub has_install_script: bool,
}

pub struct Packument {
  pub name: String,
  pub dist_tags: HashMap<String, String>,
  /// Parsed versions, ascending, with their manifests.
  pub versions: Vec<(Version, Arc<Manifest>)>,
}

/// How a dependency is requested in package.json.
#[derive(Debug, Clone, PartialEq)]
pub enum Spec {
  /// A semver range on the registry package `name`.
  Range { name: String, range: String },
  /// A dist-tag such as `latest`.
  Tag { name: String, tag: String },
  /// `file:` / `link:` path to a local directory.
  Link(String),
}

/// Parses a package.json dependency value for package `name`.
pub fn parse_spec(name: &str, value: &str) -> Result<Spec, String> {
  let value = value.trim();
  if let Some(alias) = value.strip_prefix("npm:") {
    // npm:real-name@range (the scope's @ is not the separator)
    let (real, range) = match alias[1..].find('@') {
      Some(i) => (&alias[..i + 1], &alias[i + 2..]),
      None => (alias, "latest"),
    };
    return parse_spec(real, range);
  }
  if let Some(p) = value.strip_prefix("file:").or_else(|| value.strip_prefix("link:")) {
    return Ok(Spec::Link(p.to_string()));
  }
  if value.starts_with("git") || value.starts_with("http:") || value.starts_with("https:") || value.starts_with("workspace:")
    || value.starts_with("github:") || (value.contains('/') && !value.contains(' ') && !value.starts_with('.'))
  {
    return Err(format!("{name}@{value}: only registry versions, npm: aliases and file: links are supported"));
  }
  let range = if value.is_empty() || value == "*" || value == "x" { "*" } else { value };
  if Range::parse(range).is_ok() {
    return Ok(Spec::Range { name: name.to_string(), range: range.to_string() });
  }
  if range.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_') {
    return Ok(Spec::Tag { name: name.to_string(), tag: range.to_string() });
  }
  Err(format!("{name}@{value}: invalid version range"))
}

pub fn registry_url() -> String {
  let url = std::env::var("SAKO_NPM_REGISTRY")
    .or_else(|_| std::env::var("npm_config_registry"))
    .unwrap_or_else(|_| "https://registry.npmjs.org/".into());
  if url.ends_with('/') { url } else { format!("{url}/") }
}

pub fn agent() -> &'static ureq::Agent {
  static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
  AGENT.get_or_init(|| {
    ureq::Agent::config_builder()
      .user_agent(format!("sako/{} (npm installer)", env!("CARGO_PKG_VERSION")))
      .timeout_global(Some(std::time::Duration::from_secs(120)))
      .max_idle_connections_per_host(64)
      .build()
      .new_agent()
  })
}

/// Fetches package metadata (abbreviated install format), memoized in
/// memory and cached on disk: reused without a request for 5 minutes (the
/// registry's own max-age), then revalidated with the ETag.
pub struct Registry {
  base: String,
  cache: Mutex<HashMap<String, Arc<Packument>>>,
  disk: std::path::PathBuf,
}

const FRESH_SECS: u64 = 300;

fn now_secs() -> u64 {
  std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Registry {
  pub fn new() -> Self {
    let disk = crate::store::cache_root().join("manifests");
    Registry { base: registry_url(), cache: Mutex::new(HashMap::new()), disk }
  }

  /// Raw metadata, from the disk cache when fresh (or still valid).
  fn fetch_body(&self, name: &str) -> Result<Vec<u8>, String> {
    let file = self.disk.join(format!("{}.json", name.replace('/', "+")));
    let meta_file = file.with_extension("meta");
    // meta: "<fetched-at secs>\n<etag>"
    let meta = std::fs::read_to_string(&meta_file).ok();
    let (fetched, etag) = meta
      .as_deref()
      .and_then(|m| m.split_once('\n'))
      .map(|(t, e)| (t.parse::<u64>().unwrap_or(0), e.to_string()))
      .unwrap_or((0, String::new()));
    let cached = || std::fs::read(&file).ok();
    if now_secs().saturating_sub(fetched) < FRESH_SECS
      && let Some(body) = cached()
    {
      return Ok(body);
    }
    let url = format!("{}{}", self.base, name.replace('/', "%2f"));
    let mut req = agent()
      .get(&url)
      .header("accept", "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8");
    if !etag.is_empty() {
      req = req.header("if-none-match", etag.as_str());
    }
    let mut res = req.call().map_err(|e| match e {
      ureq::Error::StatusCode(404) => format!("package not found in registry: {name}"),
      e => format!("{name}: {e}"),
    })?;
    let write_meta = |etag: &str| {
      let _ = std::fs::create_dir_all(&self.disk);
      let _ = std::fs::write(&meta_file, format!("{}\n{etag}", now_secs()));
    };
    if res.status() == 304
      && let Some(body) = cached()
    {
      write_meta(&etag);
      return Ok(body);
    }
    let new_etag = res.headers().get("etag").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = res.body_mut().with_config().limit(256 * 1024 * 1024).read_to_vec().map_err(|e| format!("{name}: {e}"))?;
    let _ = std::fs::create_dir_all(&self.disk);
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, &body).is_ok() && std::fs::rename(&tmp, &file).is_ok() {
      write_meta(&new_etag);
    }
    Ok(body)
  }

  pub fn cached(&self, name: &str) -> Option<Arc<Packument>> {
    self.cache.lock().unwrap().get(name).cloned()
  }

  pub fn packument(&self, name: &str) -> Result<Arc<Packument>, String> {
    if let Some(p) = self.cached(name) {
      return Ok(p);
    }
    let body = self.fetch_body(name)?;
    let json: Value = serde_json::from_slice(&body).map_err(|e| format!("{name}: bad registry response: {e}"))?;
    let p = Arc::new(parse_packument(name, &json));
    self.cache.lock().unwrap().insert(name.to_string(), p.clone());
    Ok(p)
  }

  /// Fetches several packuments concurrently (ignoring errors; callers
  /// retry through `packument` and report them).
  pub fn prefetch(&self, names: &[String]) {
    let todo: Vec<&String> = {
      let cache = self.cache.lock().unwrap();
      let mut seen = std::collections::HashSet::new();
      names.iter().filter(|n| !cache.contains_key(*n) && seen.insert(*n)).collect()
    };
    if todo.len() <= 1 {
      return;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
      for _ in 0..todo.len().min(32) {
        s.spawn(|| {
          loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(name) = todo.get(i) else { break };
            let _ = self.packument(name);
          }
        });
      }
    });
  }
}

impl Default for Registry {
  fn default() -> Self {
    Self::new()
  }
}

fn str_pairs(v: Option<&Value>) -> Vec<(String, String)> {
  v.and_then(Value::as_object)
    .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("*").to_string())).collect())
    .unwrap_or_default()
}

fn str_list(v: Option<&Value>) -> Vec<String> {
  v.and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// `bin` may be a string (command named after the package) or an object.
pub fn parse_bin(name: &str, bin: Option<&Value>) -> Vec<(String, String)> {
  match bin {
    Some(Value::String(path)) => {
      let cmd = name.rsplit('/').next().unwrap_or(name);
      vec![(cmd.to_string(), path.clone())]
    }
    Some(Value::Object(o)) => o.iter().filter_map(|(k, v)| v.as_str().map(|p| (k.clone(), p.to_string()))).collect(),
    _ => Vec::new(),
  }
}

pub fn parse_manifest(name: &str, v: &Value) -> Manifest {
  let dist = v.get("dist");
  let integrity = dist
    .and_then(|d| d.get("integrity"))
    .and_then(Value::as_str)
    .map(String::from)
    .or_else(|| dist.and_then(|d| d.get("shasum")).and_then(Value::as_str).map(|s| format!("sha1-{s}")))
    .unwrap_or_default();
  let peer_meta = v.get("peerDependenciesMeta");
  let peer_dependencies = str_pairs(v.get("peerDependencies"))
    .into_iter()
    .map(|(k, r)| {
      let optional = peer_meta.and_then(|m| m.get(&k)).and_then(|m| m.get("optional")).and_then(Value::as_bool) == Some(true);
      (k, r, optional)
    })
    .collect();
  let scripts = v.get("scripts");
  let has_install_script = v.get("hasInstallScript").and_then(Value::as_bool) == Some(true)
    || ["preinstall", "install", "postinstall"].iter().any(|s| scripts.and_then(|x| x.get(*s)).is_some());
  Manifest {
    name: name.to_string(),
    version: v.get("version").and_then(Value::as_str).unwrap_or("0.0.0").to_string(),
    dependencies: str_pairs(v.get("dependencies")),
    optional_dependencies: str_pairs(v.get("optionalDependencies")),
    peer_dependencies,
    bin: parse_bin(name, v.get("bin")),
    os: str_list(v.get("os")),
    cpu: str_list(v.get("cpu")),
    tarball: dist.and_then(|d| d.get("tarball")).and_then(Value::as_str).unwrap_or("").to_string(),
    integrity,
    has_install_script,
  }
}

fn parse_packument(name: &str, json: &Value) -> Packument {
  let dist_tags = json
    .get("dist-tags")
    .and_then(Value::as_object)
    .map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
    .unwrap_or_default();
  let mut versions: Vec<(Version, Arc<Manifest>)> = json
    .get("versions")
    .and_then(Value::as_object)
    .map(|o| {
      o.iter()
        .filter_map(|(ver, m)| Some((Version::parse(ver).ok()?, Arc::new(parse_manifest(name, m)))))
        .collect()
    })
    .unwrap_or_default();
  versions.sort_by(|a, b| a.0.cmp(&b.0));
  Packument { name: name.to_string(), dist_tags, versions }
}

impl Packument {
  /// Picks a version like npm: a dist-tag directly; for a range, `latest`
  /// if it satisfies, else the highest satisfying version.
  pub fn pick(&self, spec: &Spec) -> Option<Arc<Manifest>> {
    match spec {
      Spec::Tag { tag, .. } => {
        let v = self.dist_tags.get(tag)?;
        self.versions.iter().find(|(_, m)| &m.version == v).map(|(_, m)| m.clone())
      }
      Spec::Range { range, .. } => {
        let range = Range::parse(range).ok()?;
        if let Some(latest) = self.dist_tags.get("latest")
          && let Ok(lv) = Version::parse(latest)
          && range.satisfies(&lv)
          && let Some((_, m)) = self.versions.iter().find(|(v, _)| *v == lv)
        {
          return Some(m.clone());
        }
        self.versions.iter().rev().find(|(v, _)| range.satisfies(v)).map(|(_, m)| m.clone())
      }
      Spec::Link(_) => None,
    }
  }
}

/// Whether an installed `version` satisfies `spec` (for deduplication).
pub fn satisfies(version: &str, spec: &Spec, packument: Option<&Packument>) -> bool {
  match spec {
    Spec::Range { range, .. } => match (Version::parse(version), Range::parse(range)) {
      (Ok(v), Ok(r)) => r.satisfies(&v),
      _ => false,
    },
    Spec::Tag { tag, .. } => packument.and_then(|p| p.dist_tags.get(tag)).is_some_and(|v| v == version),
    Spec::Link(_) => false,
  }
}

/// npm `os` / `cpu` fields against this machine.
pub fn platform_ok(m: &Manifest) -> bool {
  let os = match std::env::consts::OS {
    "macos" => "darwin",
    "windows" => "win32",
    o => o,
  };
  let cpu = match std::env::consts::ARCH {
    "x86_64" => "x64",
    "aarch64" => "arm64",
    "x86" => "ia32",
    a => a,
  };
  fn allowed(list: &[String], value: &str) -> bool {
    if list.is_empty() {
      return true;
    }
    let denied = list.iter().any(|e| e.strip_prefix('!') == Some(value));
    let positives: Vec<&String> = list.iter().filter(|e| !e.starts_with('!')).collect();
    !denied && (positives.is_empty() || positives.iter().any(|e| *e == value))
  }
  allowed(&m.os, os) && allowed(&m.cpu, cpu)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn specs() {
    assert_eq!(parse_spec("a", "^1.2.0").unwrap(), Spec::Range { name: "a".into(), range: "^1.2.0".into() });
    assert_eq!(parse_spec("a", "").unwrap(), Spec::Range { name: "a".into(), range: "*".into() });
    assert_eq!(parse_spec("a", "next").unwrap(), Spec::Tag { name: "a".into(), tag: "next".into() });
    assert_eq!(
      parse_spec("alias", "npm:@scope/real@~2.0").unwrap(),
      Spec::Range { name: "@scope/real".into(), range: "~2.0".into() }
    );
    assert_eq!(parse_spec("a", "file:../a").unwrap(), Spec::Link("../a".into()));
    assert!(parse_spec("a", "github:user/repo").is_err());
    assert!(parse_spec("a", ">=1.0.0 <2.0.0 || 3.x").is_ok());
  }
}
