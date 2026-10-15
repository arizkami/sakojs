//! `sako install` / `sako add` / `sako remove`: an npm-compatible package
//! installer.
//!
//! - Resolution against the npm registry (semver ranges, dist-tags, `npm:`
//!   aliases, `file:` links), with npm-style hoisting ([`resolve`]).
//! - A global store of extracted packages, hard-linked into `node_modules`
//!   ([`store`]), so warm installs copy no file data.
//! - `sako.lock` pins the tree; while package.json's dependencies are
//!   unchanged, installs skip the registry entirely.
//! - Lifecycle scripts (postinstall, ...) are not run; they are reported.

pub mod registry;
pub mod resolve;
pub mod store;

use registry::{Manifest, Registry, parse_bin};
use resolve::{Node, Tree};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Options {
  /// Project directory (containing package.json).
  pub dir: PathBuf,
  /// Skip devDependencies.
  pub production: bool,
}

#[derive(Default)]
pub struct Summary {
  /// Direct dependencies as `name@version`.
  pub top_level: Vec<String>,
  pub packages: usize,
  /// Packages that had to be downloaded (not already in the store).
  pub downloaded: usize,
  pub warnings: Vec<String>,
  /// Packages with install scripts that were not run.
  pub scripts_skipped: Vec<String>,
  /// The lockfile was used without contacting the registry.
  pub from_lockfile: bool,
  /// node_modules already matched the lockfile.
  pub up_to_date: bool,
}

const DEP_FIELDS: [&str; 3] = ["dependencies", "devDependencies", "optionalDependencies"];

fn read_package_json(dir: &Path) -> Result<Value, String> {
  let path = dir.join("package.json");
  let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
  serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn write_package_json(dir: &Path, pkg: &Value) -> Result<(), String> {
  let mut text = serde_json::to_string_pretty(pkg).map_err(|e| e.to_string())?;
  text.push('\n');
  std::fs::write(dir.join("package.json"), text).map_err(|e| e.to_string())
}

/// The root dependency specs that drive resolution.
fn root_specs(pkg: &Value, production: bool) -> Value {
  let mut out = Map::new();
  for field in DEP_FIELDS {
    if production && field == "devDependencies" {
      continue;
    }
    if let Some(deps) = pkg.get(field).and_then(Value::as_object)
      && !deps.is_empty()
    {
      out.insert(field.to_string(), Value::Object(deps.clone()));
    }
  }
  Value::Object(out)
}

fn root_deps(specs: &Value) -> Vec<(String, String, bool)> {
  let mut deps: Vec<(String, String, bool)> = Vec::new();
  for field in DEP_FIELDS {
    if let Some(map) = specs.get(field).and_then(Value::as_object) {
      for (name, spec) in map {
        let spec = spec.as_str().unwrap_or("*").to_string();
        let optional = field == "optionalDependencies";
        match deps.iter_mut().find(|(n, _, _)| n == name) {
          Some(d) => *d = (name.clone(), spec, optional),
          None => deps.push((name.clone(), spec, optional)),
        }
      }
    }
  }
  deps
}

// ------------------------------------------------------------------ lockfile

fn lock_json(tree: &Tree, specs: &Value) -> Value {
  let mut packages = BTreeMap::new();
  for node in &tree.nodes[1..] {
    let mut e = Map::new();
    if let Some(link) = &node.link {
      e.insert("link".into(), json!(link));
    } else if let Some(m) = &node.manifest {
      if m.name != node.name {
        e.insert("package".into(), json!(m.name));
      }
      e.insert("version".into(), json!(m.version));
      e.insert("resolved".into(), json!(m.tarball));
      e.insert("integrity".into(), json!(m.integrity));
      if !m.bin.is_empty() {
        let bin: Map<String, Value> = m.bin.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        e.insert("bin".into(), Value::Object(bin));
      }
      if m.has_install_script {
        e.insert("hasInstallScript".into(), json!(true));
      }
    }
    packages.insert(node.path.clone(), Value::Object(e));
  }
  json!({ "lockfileVersion": 1, "root": specs, "packages": packages })
}

/// Rebuilds a tree from `sako.lock` (no registry access).
fn tree_from_lock(lock: &Value) -> Option<Tree> {
  let packages = lock.get("packages")?.as_object()?;
  let mut tree = Tree {
    nodes: vec![Node {
      path: String::new(),
      name: String::new(),
      manifest: None,
      link: None,
      location: None,
      children: BTreeMap::new(),
    }],
    warnings: Vec::new(),
  };
  let mut by_path: BTreeMap<String, usize> = BTreeMap::new();
  by_path.insert(String::new(), 0);
  // Shallow paths first so locations exist before their children.
  let mut paths: Vec<&String> = packages.keys().collect();
  paths.sort_by_key(|p| (p.matches("node_modules/").count(), p.as_str()));
  for path in paths {
    let e = &packages[path];
    let (location_path, name) = match path.rfind("/node_modules/") {
      Some(i) => (path[..i].to_string(), path[i + "/node_modules/".len()..].to_string()),
      None => (String::new(), path.strip_prefix("node_modules/")?.to_string()),
    };
    let location = *by_path.get(&location_path)?;
    let (manifest, link) = if let Some(link) = e.get("link").and_then(Value::as_str) {
      (None, Some(link.to_string()))
    } else {
      let real = e.get("package").and_then(Value::as_str).unwrap_or(&name).to_string();
      let m = Manifest {
        name: real.clone(),
        version: e.get("version")?.as_str()?.to_string(),
        dependencies: Vec::new(),
        optional_dependencies: Vec::new(),
        peer_dependencies: Vec::new(),
        bin: parse_bin(&real, e.get("bin")),
        os: Vec::new(),
        cpu: Vec::new(),
        tarball: e.get("resolved")?.as_str()?.to_string(),
        integrity: e.get("integrity").and_then(Value::as_str).unwrap_or("").to_string(),
        has_install_script: e.get("hasInstallScript").and_then(Value::as_bool) == Some(true),
      };
      (Some(Arc::new(m)), None)
    };
    let idx = tree.nodes.len();
    tree.nodes.push(Node { path: path.clone(), name: name.clone(), manifest, link, location: Some(location), children: BTreeMap::new() });
    tree.nodes[location].children.insert(name, idx);
    by_path.insert(path.clone(), idx);
  }
  Some(tree)
}

// ------------------------------------------------------------------ install

/// Installs package.json's dependencies into `node_modules`.
pub fn install(opts: &Options) -> Result<Summary, String> {
  let pkg = read_package_json(&opts.dir)?;
  let specs = root_specs(&pkg, opts.production);
  let lock_path = opts.dir.join("sako.lock");
  let mut summary = Summary::default();

  // Reuse the lockfile while the root specs are unchanged.
  let existing_lock: Option<Value> =
    std::fs::read_to_string(&lock_path).ok().and_then(|t| serde_json::from_str(&t).ok());
  let tree = match existing_lock.as_ref().filter(|l| l.get("root") == Some(&specs)).and_then(tree_from_lock) {
    Some(tree) => {
      summary.from_lockfile = true;
      tree
    }
    None => {
      let registry = Registry::new();
      resolve::resolve(&registry, &root_deps(&specs))?
    }
  };
  summary.warnings = tree.warnings.clone();
  let lock = lock_json(&tree, &specs);
  let lock_text = serde_json::to_string_pretty(&lock).map_err(|e| e.to_string())? + "\n";
  if !opts.production && !summary.from_lockfile {
    std::fs::write(&lock_path, &lock_text).map_err(|e| format!("{}: {e}", lock_path.display()))?;
  }

  summary.packages = tree.nodes.len() - 1;
  let direct: Vec<String> = root_deps(&specs).into_iter().map(|(n, _, _)| n).collect();
  for (name, &i) in &tree.nodes[0].children {
    if !direct.contains(name) {
      continue; // hoisted transitive dependency
    }
    let n = &tree.nodes[i];
    summary.top_level.push(match &n.manifest {
      Some(m) => format!("{}@{}", n.name, m.version),
      None => format!("{} -> {}", n.name, n.link.as_deref().unwrap_or("")),
    });
  }
  for n in &tree.nodes[1..] {
    if let Some(m) = &n.manifest
      && m.has_install_script
    {
      summary.scripts_skipped.push(format!("{}@{}", m.name, m.version));
    }
  }

  // node_modules already matches this lockfile: nothing to do.
  let node_modules = opts.dir.join("node_modules");
  let marker = node_modules.join(".sako-lock");
  if std::fs::read_to_string(&marker).ok().as_deref() == Some(lock_text.as_str()) {
    summary.up_to_date = true;
    return Ok(summary);
  }

  // Fetch everything missing from the store, in parallel.
  let store = store::Store::new();
  let mut unique: Vec<Arc<Manifest>> = Vec::new();
  let mut seen = std::collections::HashSet::new();
  for n in &tree.nodes[1..] {
    if let Some(m) = &n.manifest
      && seen.insert((m.name.clone(), m.version.clone()))
    {
      unique.push(m.clone());
    }
  }
  let missing: Vec<&Arc<Manifest>> = unique.iter().filter(|m| !store.has(m)).collect();
  summary.downloaded = missing.len();
  parallel(&missing, |m| store.ensure(m).map(|_| ()))?;

  // Rebuild node_modules from the store with hard links.
  if node_modules.exists() {
    std::fs::remove_dir_all(&node_modules).map_err(|e| format!("{}: {e}", node_modules.display()))?;
  }
  std::fs::create_dir_all(&node_modules).map_err(|e| e.to_string())?;
  let nodes: Vec<&Node> = tree.nodes[1..].iter().collect();
  parallel(&nodes, |n| {
    let dest = opts.dir.join(&n.path);
    if let Some(link) = &n.link {
      if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
      }
      let target = opts.dir.join(link);
      #[cfg(unix)]
      std::os::unix::fs::symlink(&target, &dest).map_err(|e| format!("{}: {e}", n.name))?;
      #[cfg(not(unix))]
      store::link_tree(&target, &dest).map_err(|e| format!("{}: {e}", n.name))?;
      return Ok(());
    }
    let m = n.manifest.as_ref().unwrap();
    store::link_tree(&store.package_dir(m), &dest).map_err(|e| format!("{}: {e}", n.name))?;
    Ok(())
  })?;
  // .bin links after all packages exist.
  for n in &nodes {
    if let Some(m) = &n.manifest {
      store::link_bins(&opts.dir.join(&n.path), &n.name, &m.bin).map_err(|e| format!("{}: bin: {e}", n.name))?;
    }
  }
  std::fs::write(&marker, &lock_text).map_err(|e| e.to_string())?;
  Ok(summary)
}

/// Runs `f` over `items` on up to 16 threads; returns the first error.
fn parallel<T: Sync>(items: &[T], f: impl Fn(&T) -> Result<(), String> + Sync) -> Result<(), String> {
  if items.is_empty() {
    return Ok(());
  }
  let next = std::sync::atomic::AtomicUsize::new(0);
  let error = std::sync::Mutex::new(None);
  std::thread::scope(|s| {
    for _ in 0..items.len().min(16) {
      s.spawn(|| {
        loop {
          let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
          let Some(item) = items.get(i) else { break };
          if let Err(e) = f(item) {
            error.lock().unwrap().get_or_insert(e);
            break;
          }
        }
      });
    }
  });
  match error.into_inner().unwrap() {
    Some(e) => Err(e),
    None => Ok(()),
  }
}

/// Splits `name@spec` (scoped names keep their leading `@`).
pub fn split_name_spec(arg: &str) -> (String, Option<String>) {
  let at = if let Some(rest) = arg.strip_prefix('@') { rest.find('@').map(|i| i + 1) } else { arg.find('@') };
  match at {
    Some(i) => (arg[..i].to_string(), Some(arg[i + 1..].to_string())),
    None => (arg.to_string(), None),
  }
}

/// Adds packages to package.json (`^latest` when no version is given) and
/// installs.
pub fn add(opts: &Options, args: &[String], dev: bool) -> Result<Summary, String> {
  let mut pkg = read_package_json(&opts.dir).unwrap_or_else(|_| json!({}));
  let registry = Registry::new();
  let names: Vec<String> = args.iter().map(|a| split_name_spec(a).0).collect();
  registry.prefetch(&names);
  let field = if dev { "devDependencies" } else { "dependencies" };
  for arg in args {
    let (name, spec) = split_name_spec(arg);
    let spec = match spec {
      Some(s) if !s.is_empty() => s,
      _ => {
        let p = registry.packument(&name)?;
        let m = p
          .pick(&registry::Spec::Tag { name: name.clone(), tag: "latest".into() })
          .ok_or_else(|| format!("{name}: no latest version"))?;
        format!("^{}", m.version)
      }
    };
    registry::parse_spec(&name, &spec)?;
    let obj = pkg.as_object_mut().ok_or("package.json is not an object")?;
    for other in DEP_FIELDS {
      if other != field
        && let Some(deps) = obj.get_mut(other).and_then(Value::as_object_mut)
      {
        deps.remove(&name);
      }
    }
    let deps = obj.entry(field).or_insert_with(|| json!({}));
    deps.as_object_mut().ok_or(format!("package.json {field} is not an object"))?.insert(name, json!(spec));
  }
  write_package_json(&opts.dir, &pkg)?;
  install(opts)
}

/// Removes packages from package.json and reinstalls.
pub fn remove(opts: &Options, names: &[String]) -> Result<Summary, String> {
  let mut pkg = read_package_json(&opts.dir)?;
  let mut found = false;
  if let Some(obj) = pkg.as_object_mut() {
    for field in DEP_FIELDS {
      if let Some(deps) = obj.get_mut(field).and_then(Value::as_object_mut) {
        for n in names {
          found |= deps.remove(n).is_some();
        }
      }
    }
  }
  if !found {
    return Err(format!("not a dependency: {}", names.join(", ")));
  }
  write_package_json(&opts.dir, &pkg)?;
  install(opts)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn name_spec() {
    assert_eq!(split_name_spec("react"), ("react".into(), None));
    assert_eq!(split_name_spec("react@^18"), ("react".into(), Some("^18".into())));
    assert_eq!(split_name_spec("@types/node"), ("@types/node".into(), None));
    assert_eq!(split_name_spec("@types/node@20"), ("@types/node".into(), Some("20".into())));
  }
}
