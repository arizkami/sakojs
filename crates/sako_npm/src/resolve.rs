//! Builds an npm-style hoisted `node_modules` tree.
//!
//! Breadth-first, in waves (each wave's metadata is fetched in parallel). A
//! dependency reuses a satisfying copy visible from its dependent (Node's
//! lookup: own `node_modules`, then each ancestor's); otherwise it is hoisted
//! to the top-level `node_modules`, or nested under its dependent when the top
//! level already holds a different version.

use crate::registry::{Manifest, Registry, Spec, parse_spec, platform_ok, satisfies};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

pub struct Node {
  /// Install path relative to the project: "" (root), "node_modules/a",
  /// "node_modules/a/node_modules/b".
  pub path: String,
  /// Name in node_modules (may be an alias of the registry name).
  pub name: String,
  pub manifest: Option<Arc<Manifest>>,
  /// `file:` / `link:` target (relative to the project).
  pub link: Option<String>,
  /// The node whose `node_modules` directory contains this one.
  pub location: Option<usize>,
  /// Packages inside this node's own `node_modules`.
  pub children: BTreeMap<String, usize>,
}

pub struct Tree {
  pub nodes: Vec<Node>,
  pub warnings: Vec<String>,
}

struct Dep {
  from: usize,
  name: String,
  spec_text: String,
  optional: bool,
  peer: bool,
}

/// Resolves the project's dependencies (`deps`: name, spec, optional) into a
/// tree.
pub fn resolve(registry: &Registry, deps: &[(String, String, bool)]) -> Result<Tree, String> {
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
  let mut queue: VecDeque<Dep> = deps
    .iter()
    .map(|(n, s, optional)| Dep { from: 0, name: n.clone(), spec_text: s.clone(), optional: *optional, peer: false })
    .collect();

  while !queue.is_empty() {
    let wave: Vec<Dep> = queue.drain(..).collect();
    let names: Vec<String> = wave
      .iter()
      .filter_map(|d| match parse_spec(&d.name, &d.spec_text) {
        Ok(Spec::Range { name, .. } | Spec::Tag { name, .. }) => Some(name),
        _ => None,
      })
      .collect();
    registry.prefetch(&names);
    for dep in wave {
      if let Err(e) = place(registry, &mut tree, &dep, &mut queue) {
        if dep.optional {
          tree.warnings.push(format!("skipped optional dependency: {e}"));
        } else {
          return Err(e);
        }
      }
    }
  }
  Ok(tree)
}

/// The nearest copy of `name` visible from node `from`.
fn visible(tree: &Tree, from: usize, name: &str) -> Option<usize> {
  let mut at = Some(from);
  while let Some(i) = at {
    if let Some(&c) = tree.nodes[i].children.get(name) {
      return Some(c);
    }
    at = tree.nodes[i].location;
  }
  None
}

fn add_node(tree: &mut Tree, location: usize, name: &str, manifest: Option<Arc<Manifest>>, link: Option<String>) -> usize {
  let parent_path = &tree.nodes[location].path;
  let path = if parent_path.is_empty() {
    format!("node_modules/{name}")
  } else {
    format!("{parent_path}/node_modules/{name}")
  };
  let idx = tree.nodes.len();
  tree.nodes.push(Node { path, name: name.to_string(), manifest, link, location: Some(location), children: BTreeMap::new() });
  tree.nodes[location].children.insert(name.to_string(), idx);
  idx
}

fn place(registry: &Registry, tree: &mut Tree, dep: &Dep, queue: &mut VecDeque<Dep>) -> Result<(), String> {
  let spec = parse_spec(&dep.name, &dep.spec_text)?;
  let existing = visible(tree, dep.from, &dep.name);

  if let Spec::Link(target) = &spec {
    if existing.is_none() {
      add_node(tree, 0, &dep.name, None, Some(target.clone()));
    }
    return Ok(());
  }

  let real_name = match &spec {
    Spec::Range { name, .. } | Spec::Tag { name, .. } => name.clone(),
    Spec::Link(_) => unreachable!(),
  };
  if let Some(e) = existing {
    let node = &tree.nodes[e];
    let ok = node.link.is_some()
      || node.manifest.as_ref().is_some_and(|m| {
        m.name == real_name && satisfies(&m.version, &spec, registry.cached(&real_name).as_deref())
      });
    if ok {
      return Ok(());
    }
    if dep.peer {
      // Peers must share the visible instance; keep it and warn.
      let have = node.manifest.as_ref().map_or("a link".to_string(), |m| m.version.clone());
      tree.warnings.push(format!(
        "peer dependency {}@{} wanted by {}, found {have}",
        dep.name,
        dep.spec_text,
        tree.nodes[dep.from].name
      ));
      return Ok(());
    }
  }

  let packument = registry.packument(&real_name)?;
  let manifest = packument
    .pick(&spec)
    .ok_or_else(|| format!("no version of {real_name} matches \"{}\"", dep.spec_text))?;
  if !platform_ok(&manifest) {
    if dep.optional {
      return Ok(()); // e.g. a native binary for another OS/CPU
    }
    tree.warnings.push(format!("{}@{} does not list this platform", manifest.name, manifest.version));
  }

  // Hoist to the top level unless a different version is visible there.
  let location = if existing.is_some() { dep.from } else { 0 };
  let idx = add_node(tree, location, &dep.name, Some(manifest.clone()), None);

  let optional_names: std::collections::HashSet<&str> =
    manifest.optional_dependencies.iter().map(|(n, _)| n.as_str()).collect();
  for (n, s) in &manifest.dependencies {
    if !optional_names.contains(n.as_str()) {
      queue.push_back(Dep { from: idx, name: n.clone(), spec_text: s.clone(), optional: false, peer: false });
    }
  }
  for (n, s) in &manifest.optional_dependencies {
    queue.push_back(Dep { from: idx, name: n.clone(), spec_text: s.clone(), optional: true, peer: false });
  }
  for (n, s, optional) in &manifest.peer_dependencies {
    if !optional {
      queue.push_back(Dep { from: idx, name: n.clone(), spec_text: s.clone(), optional: true, peer: true });
    }
  }
  Ok(())
}
