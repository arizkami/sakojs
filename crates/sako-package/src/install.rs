// SPDX-License-Identifier: BSD-3-Clause

//! Turning a resolved graph into a node_modules tree.
//!
//! The order of this file is the order of the pipeline, and the split between
//! its halves is the whole design:
//!
//! ```text
//!   resolved graph  ->  plan  ->  fetch  ->  store  ->  materialize  ->  link  ->  scripts
//!                       ^^^^      \_______________ bounded pools _______________/
//!                    one thread
//! ```
//!
//! Everything that *decides* anything happens in the plan, on one thread, in a
//! fixed order, reading only from data that is already complete. Everything
//! that is merely *work* happens afterwards, on pools. That is what lets a
//! parallel installer write a byte-identical lockfile every time: no decision
//! is ever made by whichever worker finished first, because by the time there
//! are workers there are no decisions left to make.
//!
//! Both ways of installing end up here. Resolving from `package.json` and
//! replaying `sako.lock` differ only in where the plan comes from -- the
//! registry or the lockfile -- and share every stage after it, which is why
//! the warm path got the same speedup as the cold one.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::Arc;

use crate::profile::{Count, Pool, Stage};
use crate::resolver::{self, Graph};
use crate::scheduler::{self, Cancel, Limits};
use crate::store::ContentStore;
use crate::{
    Checksum, HOST_CPU, HOST_OS, LockedPackage, Lockfile, MAXIMUM_PACKAGES, PackageError,
    PackageManager, PackageVersion, ProgressEvent, WorkspacePackage, copy_workspace,
    installed_scripts, link_binaries, lock_has_ancestor_dependency, package_install_path,
    run_lifecycle_scripts, validate_package_name, validate_peer_dependencies,
    validate_sako_engine, workspace_requirement_matches,
};

/// Where one tree position gets its contents from.
enum Source {
    /// Resolved from the registry this run.
    Registry {
        package: Arc<PackageVersion>,
        checksum: Checksum,
    },
    /// Named by the lockfile, which already knows the exact archive.
    Locked {
        package: Box<LockedPackage>,
        checksum: Checksum,
    },
    Workspace(Box<WorkspacePackage>),
}

/// One position in the tree: one directory, one package.
struct Node {
    lock_path: String,
    destination: PathBuf,
    source: Source,
    /// Which optional dependency this position sits under, if any. A failure
    /// anywhere below an optional edge takes out that edge's subtree and
    /// nothing else, which is what the recursive installer got from wrapping
    /// one call in a `match`.
    group: Option<u32>,
    /// Distance from the root. Positions are filled a level at a time, because
    /// filling one clears its directory and a parent cleared after its
    /// children would delete them.
    depth: usize,
    /// A position the cycle guard stopped at. It keeps its lockfile entry --
    /// dropping it would make the next install report a missing transitive
    /// dependency -- but there is nothing to put on disk, because the copy
    /// further up the tree already satisfies it.
    record_only: bool,
}

impl Node {
    fn content_key(&self) -> Option<String> {
        match &self.source {
            Source::Registry { checksum, .. } | Source::Locked { checksum, .. } => {
                Some(checksum.cache_key())
            }
            Source::Workspace(_) => None,
        }
    }

    fn tarball(&self) -> Option<&str> {
        match &self.source {
            Source::Registry { package, .. } => Some(&package.dist.tarball),
            Source::Locked { package, .. } => Some(&package.resolved),
            Source::Workspace(_) => None,
        }
    }

    fn name(&self) -> &str {
        match &self.source {
            Source::Registry { package, .. } => &package.name,
            Source::Locked { package, .. } => &package.name,
            Source::Workspace(workspace) => &workspace.name,
        }
    }

    /// The lockfile entry for this position.
    fn locked(&self, scripts: &HashMap<String, BTreeMap<String, String>>) -> LockedPackage {
        match &self.source {
            Source::Registry { package, checksum } => LockedPackage {
                name: package.name.clone(),
                version: package.version.clone(),
                resolved: package.dist.tarball.clone(),
                integrity: checksum.to_integrity(),
                dependencies: package.dependencies.clone(),
                optional_dependencies: package.optional_dependencies.clone(),
                peer_dependencies: package.peer_dependencies.clone(),
                optional_peers: package
                    .peer_dependencies_meta
                    .iter()
                    .filter(|(_, metadata)| metadata.optional)
                    .map(|(name, _)| name.clone())
                    .collect(),
                scripts: self
                    .content_key()
                    .and_then(|key| scripts.get(&key).cloned())
                    .unwrap_or_default(),
                engines: package.engines.clone(),
            },
            // A replay records what the lockfile already said, including the
            // scripts it recorded, so replaying runs exactly what the install
            // that wrote it ran.
            Source::Locked { package, .. } => (**package).clone(),
            Source::Workspace(workspace) => LockedPackage {
                name: workspace.name.clone(),
                version: workspace.version.clone(),
                resolved: format!("workspace:{}", workspace.relative_path),
                integrity: "workspace".into(),
                dependencies: workspace.dependencies.clone(),
                optional_dependencies: workspace.optional_dependencies.clone(),
                peer_dependencies: workspace.peer_dependencies.clone(),
                optional_peers: workspace.optional_peers.clone(),
                scripts: workspace.scripts.clone(),
                engines: workspace.engines.clone(),
            },
        }
    }
}

/// The tree, decided.
struct Plan {
    nodes: Vec<Node>,
    /// Node indices in the order their subtrees complete, which is the order
    /// lifecycle scripts have to run in: a package's own script runs only once
    /// everything below it is on disk.
    finish_order: Vec<usize>,
    /// Every optional group's enclosing group, for unwinding a failure out to
    /// the edge that tolerated it.
    group_parents: Vec<Option<u32>>,
    /// Directories that ended up with packages in them, and so need a `.bin`.
    bin_directories: BTreeSet<PathBuf>,
    warnings: Vec<String>,
    /// Set when the plan came from a lockfile, where the scripts to run are
    /// already recorded and nothing needs reading off disk to find them.
    from_lock: bool,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Resolves the graph from `package.json` and installs it.
pub fn install(
    manager: &mut PackageManager,
    dependencies: &BTreeMap<String, String>,
    optional_dependencies: &BTreeMap<String, String>,
) -> Result<(), PackageError> {
    let limits = scheduler::limits();
    let node_modules = manager.root.join("node_modules");
    std::fs::create_dir_all(&node_modules)?;

    let roots: Vec<(String, String)> = dependencies
        .iter()
        .chain(optional_dependencies.iter())
        .map(|(name, requirement)| (name.clone(), requirement.clone()))
        .collect();
    let graph = resolver::resolve_graph(
        &manager.registry,
        &manager.metadata,
        &manager.workspaces,
        &roots,
        limits.metadata,
        manager.reporter.0.as_deref(),
    )?;
    manager
        .profile
        .count(Count::UniqueRequests, graph.len() as u64);

    let plan = manager.profile.time(Stage::Graph, || {
        Planner::new(manager, &graph).build(&node_modules, dependencies, optional_dependencies)
    })?;
    execute(manager, plan, &limits)
}

/// Rebuilds the tree a lockfile already describes.
pub fn replay(
    manager: &mut PackageManager,
    lockfile: &Lockfile,
    dependencies: &BTreeMap<String, String>,
    optional_dependencies: &BTreeMap<String, String>,
) -> Result<(), PackageError> {
    let limits = scheduler::limits();
    let node_modules = manager.root.join("node_modules");
    std::fs::create_dir_all(&node_modules)?;
    let plan = manager.profile.time(Stage::Graph, || {
        Replayer::new(manager, lockfile).build(&node_modules, dependencies, optional_dependencies)
    })?;
    execute(manager, plan, &limits)
}

/// Runs a plan: fetch, store, materialize, record, link, scripts.
fn execute(
    manager: &mut PackageManager,
    plan: Plan,
    limits: &Limits,
) -> Result<(), PackageError> {
    for warning in &plan.warnings {
        manager.warn(warning);
    }
    manager
        .profile
        .count(Count::TreePositions, plan.nodes.len() as u64);
    manager.report(ProgressEvent::Planned {
        total: plan.nodes.len(),
    });

    let store = ContentStore::new(manager.registry.store_root.with_file_name("unpacked"));
    let dropped = Mutex::new(HashSet::<u32>::new());

    populate_store(manager, &plan, &store, limits, &dropped)?;
    materialize(manager, &plan, &store, limits, &dropped)?;

    let scripts = script_index(&plan, &dropped);
    manager.installed.clear();
    for node in &plan.nodes {
        if is_dropped(&dropped, node, &plan) {
            continue;
        }
        manager
            .installed
            .insert(node.lock_path.clone(), node.locked(&scripts));
    }
    validate_peer_dependencies(&manager.installed)?;

    link(manager, &plan, limits)?;
    if !manager.ignore_scripts {
        run_scripts(manager, &plan, &scripts, &dropped)?;
    }

    let _ = store.prune();
    let _ = crate::prune_store(&manager.registry.store_root);
    Ok(())
}

// ---------------------------------------------------------------------------
// Planning from resolved registry metadata
// ---------------------------------------------------------------------------

/// Shared plan-building state, so the two planners differ only in where a
/// position's package comes from.
struct Building {
    nodes: Vec<Node>,
    finish_order: Vec<usize>,
    group_parents: Vec<Option<u32>>,
    bin_directories: BTreeSet<PathBuf>,
    warnings: Vec<String>,
    /// Identities on the path from the root to here, so a dependency cycle
    /// stops instead of nesting `node_modules` for ever.
    active: HashSet<String>,
}

impl Building {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            finish_order: Vec::new(),
            group_parents: Vec::new(),
            bin_directories: BTreeSet::new(),
            warnings: Vec::new(),
            active: HashSet::new(),
        }
    }

    fn finish(self, from_lock: bool) -> Plan {
        Plan {
            nodes: self.nodes,
            finish_order: self.finish_order,
            group_parents: self.group_parents,
            bin_directories: self.bin_directories,
            warnings: self.warnings,
            from_lock,
        }
    }

    /// Opens a group for an optional edge, so a failure below it unwinds only
    /// this far.
    fn open_group(&mut self, parent: Option<u32>) -> u32 {
        let group = self.group_parents.len() as u32;
        self.group_parents.push(parent);
        group
    }

    /// Removes the positions an optional edge planned before it failed.
    fn rewind(&mut self, to: usize) {
        self.nodes.truncate(to);
        self.finish_order.retain(|index| *index < to);
    }
}

struct Planner<'a> {
    manager: &'a PackageManager,
    graph: &'a Graph,
    state: Building,
}

impl<'a> Planner<'a> {
    fn new(manager: &'a PackageManager, graph: &'a Graph) -> Self {
        Self {
            manager,
            graph,
            state: Building::new(),
        }
    }

    fn build(
        mut self,
        node_modules: &Path,
        dependencies: &BTreeMap<String, String>,
        optional_dependencies: &BTreeMap<String, String>,
    ) -> Result<Plan, PackageError> {
        self.expand(
            node_modules,
            "node_modules",
            dependencies,
            optional_dependencies,
            None,
        )?;
        Ok(self.state.finish(false))
    }

    /// Plans one directory's worth of dependencies: the required ones, then
    /// the optional ones, each in name order.
    ///
    /// The order is the one the recursive installer used, and it is load
    /// bearing twice over: it decides which of two positions for a package is
    /// the one that gets a directory, and it decides which failure a user is
    /// shown first.
    fn expand(
        &mut self,
        node_modules: &Path,
        lock_prefix: &str,
        dependencies: &BTreeMap<String, String>,
        optional_dependencies: &BTreeMap<String, String>,
        group: Option<u32>,
    ) -> Result<(), PackageError> {
        let required: Vec<(String, String)> = required_only(dependencies, optional_dependencies)
            .map(|(name, requirement)| (name.clone(), requirement.clone()))
            .collect();
        for (name, requirement) in required {
            let lock_path = child_lock_path(lock_prefix, &name);
            self.plan_one(&name, &requirement, node_modules, &lock_path, group)?;
        }
        for (name, requirement) in optional_dependencies {
            if self.built_for_another_platform(name, requirement) {
                self.manager.profile.bump(Count::PlatformSkipped);
                continue;
            }
            let inner = self.state.open_group(group);
            let lock_path = child_lock_path(lock_prefix, name);
            let before = self.state.nodes.len();
            if let Err(error) =
                self.plan_one(name, requirement, node_modules, &lock_path, Some(inner))
            {
                self.state.rewind(before);
                self.state
                    .warnings
                    .push(format!("skipping optional dependency {name}: {error}"));
            }
        }
        Ok(())
    }

    fn plan_one(
        &mut self,
        name: &str,
        requirement: &str,
        node_modules: &Path,
        lock_path: &str,
        group: Option<u32>,
    ) -> Result<(), PackageError> {
        if self.state.nodes.len() >= MAXIMUM_PACKAGES {
            return Err(PackageError("package graph capacity exceeded".into()));
        }
        validate_package_name(name)?;

        if let Some(workspace) = self.manager.workspaces.get(name) {
            if workspace_requirement_matches(&workspace.version, requirement) {
                let workspace = workspace.clone();
                return self.plan_workspace(workspace, node_modules, lock_path, group);
            }
            if requirement.starts_with("workspace:") {
                return Err(PackageError(format!(
                    "workspace {name}@{} does not satisfy {requirement}",
                    workspace.version
                )));
            }
        } else if requirement.starts_with("workspace:") {
            return Err(PackageError(format!(
                "workspace package {name} was not found"
            )));
        }

        let package = self.graph.select(&self.manager.registry, name, requirement)?;
        if !package.supports_host() {
            return Err(PackageError(format!(
                "{}@{} is not built for {HOST_OS}-{HOST_CPU}",
                package.name, package.version
            )));
        }
        validate_sako_engine(
            &format!("{}@{}", package.name, package.version),
            &package.engines,
        )?;
        let checksum = package.dist.checksum().map_err(|error| {
            PackageError(format!("{}@{}: {error}", package.name, package.version))
        })?;
        let destination = package_install_path(node_modules, name)?;
        let children_root = destination.join("node_modules");
        let identity = format!("{}@{}", package.name, package.version);
        let repeated = !self.state.active.insert(identity.clone());
        let index = self.state.nodes.len();
        self.state.bin_directories.insert(node_modules.to_path_buf());
        self.state.nodes.push(Node {
            lock_path: lock_path.to_owned(),
            destination,
            source: Source::Registry {
                package: Arc::clone(&package),
                checksum,
            },
            group,
            depth: depth_of(lock_path),
            record_only: repeated,
        });
        if repeated {
            return Ok(());
        }

        let outcome = self.expand(
            &children_root,
            lock_path,
            &package.dependencies,
            &package.optional_dependencies,
            group,
        );
        self.state.active.remove(&identity);
        outcome?;
        self.state.finish_order.push(index);
        Ok(())
    }

    fn plan_workspace(
        &mut self,
        workspace: WorkspacePackage,
        node_modules: &Path,
        lock_path: &str,
        group: Option<u32>,
    ) -> Result<(), PackageError> {
        let identity = format!("{}@{}", workspace.name, workspace.version);
        let repeated = !self.state.active.insert(identity.clone());
        let destination = package_install_path(node_modules, &workspace.name)?;
        let children_root = destination.join("node_modules");
        let dependencies = workspace.dependencies.clone();
        let optional_dependencies = workspace.optional_dependencies.clone();
        let index = self.state.nodes.len();
        self.state.bin_directories.insert(node_modules.to_path_buf());
        self.state.nodes.push(Node {
            lock_path: lock_path.to_owned(),
            destination,
            source: Source::Workspace(Box::new(workspace)),
            group,
            depth: depth_of(lock_path),
            record_only: repeated,
        });
        if repeated {
            return Ok(());
        }
        let outcome = self.expand(
            &children_root,
            lock_path,
            &dependencies,
            &optional_dependencies,
            group,
        );
        self.state.active.remove(&identity);
        outcome?;
        self.state.finish_order.push(index);
        Ok(())
    }

    /// Whether an optional dependency is one of the other platforms' builds.
    ///
    /// Not a failure and not worth a warning: shipping one narrowly-targeted
    /// optional dependency per platform is exactly how esbuild, rollup, and
    /// rolldown deliver native binaries, and every one of them but ours is
    /// meant to be passed over in silence.
    fn built_for_another_platform(&self, name: &str, requirement: &str) -> bool {
        self.graph
            .select(&self.manager.registry, name, requirement)
            .is_ok_and(|package| !package.supports_host())
    }
}

// ---------------------------------------------------------------------------
// Planning from a lockfile
// ---------------------------------------------------------------------------

struct Replayer<'a> {
    manager: &'a PackageManager,
    lockfile: &'a Lockfile,
    state: Building,
}

impl<'a> Replayer<'a> {
    fn new(manager: &'a PackageManager, lockfile: &'a Lockfile) -> Self {
        Self {
            manager,
            lockfile,
            state: Building::new(),
        }
    }

    fn build(
        mut self,
        node_modules: &Path,
        dependencies: &BTreeMap<String, String>,
        optional_dependencies: &BTreeMap<String, String>,
    ) -> Result<Plan, PackageError> {
        for name in dependencies.keys() {
            self.plan_one(&format!("node_modules/{name}"), node_modules, None)?;
        }
        for name in optional_dependencies.keys() {
            let lock_path = format!("node_modules/{name}");
            if !self.lockfile.packages.contains_key(&lock_path) {
                continue;
            }
            let group = self.state.open_group(None);
            let before = self.state.nodes.len();
            if let Err(error) = self.plan_one(&lock_path, node_modules, Some(group)) {
                self.state.rewind(before);
                self.state
                    .warnings
                    .push(format!("skipping optional dependency {name}: {error}"));
            }
        }
        Ok(self.state.finish(true))
    }

    fn plan_one(
        &mut self,
        lock_path: &str,
        node_modules: &Path,
        group: Option<u32>,
    ) -> Result<(), PackageError> {
        let package = self.lockfile.packages.get(lock_path).ok_or_else(|| {
            PackageError(format!("lockfile is missing dependency entry {lock_path}"))
        })?;
        validate_sako_engine(
            &format!("{}@{}", package.name, package.version),
            &package.engines,
        )?;

        let destination = package_install_path(node_modules, &package.name)?;
        let children_root = destination.join("node_modules");
        let identity = format!("{}@{}", package.name, package.version);
        let repeated = !self.state.active.insert(identity.clone());
        let index = self.state.nodes.len();

        let source = if package.resolved.starts_with("workspace:") {
            let workspace = self.manager.workspaces.get(&package.name).ok_or_else(|| {
                PackageError(format!("locked workspace {} was not found", package.name))
            })?;
            if workspace.version != package.version {
                return Err(PackageError(format!(
                    "locked workspace {}@{} does not match local version {}",
                    package.name, package.version, workspace.version
                )));
            }
            Source::Workspace(Box::new(workspace.clone()))
        } else {
            let checksum = Checksum::parse(&package.integrity).map_err(|error| {
                PackageError(format!("{}@{}: {error}", package.name, package.version))
            })?;
            Source::Locked {
                package: Box::new(package.clone()),
                checksum,
            }
        };

        self.state.bin_directories.insert(node_modules.to_path_buf());
        self.state.nodes.push(Node {
            lock_path: lock_path.to_owned(),
            destination,
            source,
            group,
            depth: depth_of(lock_path),
            record_only: repeated,
        });
        if repeated {
            return Ok(());
        }

        let outcome = self.expand_locked(package.clone(), lock_path, &children_root, group);
        self.state.active.remove(&identity);
        outcome?;
        self.state.finish_order.push(index);
        Ok(())
    }

    fn expand_locked(
        &mut self,
        package: LockedPackage,
        lock_path: &str,
        children_root: &Path,
        group: Option<u32>,
    ) -> Result<(), PackageError> {
        for (dependency, requirement) in
            required_only(&package.dependencies, &package.optional_dependencies)
        {
            let child_path = format!("{lock_path}/node_modules/{dependency}");
            if self.lockfile.packages.contains_key(&child_path) {
                self.plan_one(&child_path, children_root, group)?;
            } else if !lock_has_ancestor_dependency(
                self.lockfile,
                lock_path,
                dependency,
                requirement,
            ) {
                return Err(PackageError(format!(
                    "lockfile is missing transitive dependency {dependency} for {lock_path}"
                )));
            }
        }
        for dependency in package.optional_dependencies.keys() {
            let child_path = format!("{lock_path}/node_modules/{dependency}");
            if !self.lockfile.packages.contains_key(&child_path) {
                continue;
            }
            let inner = self.state.open_group(group);
            let before = self.state.nodes.len();
            if let Err(error) = self.plan_one(&child_path, children_root, Some(inner)) {
                self.state.rewind(before);
                self.state
                    .warnings
                    .push(format!("skipping optional dependency {dependency}: {error}"));
            }
        }
        Ok(())
    }
}

/// The required dependencies of a package, which is not simply its
/// `dependencies`.
///
/// npm's abbreviated packument -- the one this resolver asks for -- repeats
/// every optional dependency inside `dependencies` as well. Taking that at
/// face value plans the same tree position twice: once as required and once as
/// optional. The recursive installer did exactly that and got away with it,
/// because installing a package twice in a row over the top of itself looks
/// like installing it once. Two workers doing it at the same time do not.
///
/// It was never only cosmetic, either. The required copy was planned first, so
/// an optional native dependency that would not install failed the whole
/// install instead of being skipped -- which is the one thing
/// `optionalDependencies` exists to prevent. `optionalDependencies` wins,
/// which is npm's rule.
fn required_only<'a>(
    dependencies: &'a BTreeMap<String, String>,
    optional: &BTreeMap<String, String>,
) -> impl Iterator<Item = (&'a String, &'a String)> {
    dependencies
        .iter()
        .filter(move |(name, _)| !optional.contains_key(name.as_str()))
        .map(|(name, requirement)| (name, requirement))
}

fn child_lock_path(prefix: &str, name: &str) -> String {
    if prefix == "node_modules" {
        format!("node_modules/{name}")
    } else {
        format!("{prefix}/node_modules/{name}")
    }
}

/// How deep a position sits, read off the lock path it was given.
fn depth_of(lock_path: &str) -> usize {
    lock_path.matches("/node_modules/").count()
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Whether a position was taken out by a failure under the optional edge it
/// sits below.
fn is_dropped(dropped: &Mutex<HashSet<u32>>, node: &Node, plan: &Plan) -> bool {
    let dropped = dropped.lock().unwrap_or_else(|error| error.into_inner());
    if dropped.is_empty() {
        return false;
    }
    let mut group = node.group;
    while let Some(current) = group {
        if dropped.contains(&current) {
            return true;
        }
        group = plan.group_parents.get(current as usize).copied().flatten();
    }
    false
}

/// Records that an optional edge failed, or turns it into a hard error when
/// the position was not optional at all.
fn tolerate(
    manager: &PackageManager,
    dropped: &Mutex<HashSet<u32>>,
    node: &Node,
    error: PackageError,
) -> Result<(), PackageError> {
    let Some(group) = node.group else {
        return Err(error);
    };
    dropped
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(group);
    manager.warn(&format!(
        "skipping optional dependency {}: {error}",
        node.name()
    ));
    Ok(())
}

/// Downloads and unpacks every distinct package the plan needs.
///
/// Distinct by content hash, not by position: this is where a tree of 1,815
/// positions becomes a hundred-odd downloads and a hundred-odd
/// decompressions. The download and extraction pools are separate and both
/// bounded, so one slow tarball occupies one slot and leaves the rest of the
/// graph moving.
fn populate_store(
    manager: &PackageManager,
    plan: &Plan,
    store: &ContentStore,
    limits: &Limits,
    dropped: &Mutex<HashSet<u32>>,
) -> Result<(), PackageError> {
    // First position wins the key, so the error a shared package produces is
    // attributed to a position that does not depend on scheduling.
    let mut unique: Vec<usize> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (index, node) in plan.nodes.iter().enumerate() {
        if node.record_only {
            continue;
        }
        if let Some(key) = node.content_key()
            && seen.insert(key)
        {
            unique.push(index);
        }
    }
    manager
        .profile
        .count(Count::UniqueTarballs, unique.len() as u64);
    manager.report(ProgressEvent::FetchPlanned {
        total: unique.len(),
    });

    let cancel = Cancel::new();
    let failures: Mutex<Vec<(usize, PackageError)>> = Mutex::new(Vec::new());
    scheduler::run(limits.download, unique, &cancel, |index, _| {
        let node = &plan.nodes[index];
        let (Some(key), Some(tarball)) = (node.content_key(), node.tarball()) else {
            return Ok(());
        };
        let checksum = match &node.source {
            Source::Registry { checksum, .. } | Source::Locked { checksum, .. } => checksum,
            Source::Workspace(_) => return Ok(()),
        };
        let outcome = (|| -> Result<(), PackageError> {
            if store.contains(&key) {
                manager.profile.bump(Count::StoreHits);
                manager.report(ProgressEvent::DownloadFinished {
                    name: node.name(),
                    cached: true,
                });
                return Ok(());
            }
            manager.report(ProgressEvent::DownloadStarted { name: node.name() });
            let (archive, downloaded) = manager.registry.archive(tarball, checksum)?;
            manager.report(ProgressEvent::DownloadFinished {
                name: node.name(),
                cached: !downloaded,
            });
            manager.report(ProgressEvent::ExtractStarted { name: node.name() });
            manager.profile.bump(Count::Extractions);
            let _occupancy = manager.profile.occupy(Pool::Extract);
            manager
                .profile
                .time(Stage::Extract, || store.populate(&key, &archive))?;
            manager.report(ProgressEvent::ExtractFinished { name: node.name() });
            Ok(())
        })();
        if let Err(error) = outcome {
            // Held rather than raised: whether this is fatal depends on which
            // edge reached it, and unwinding the pool to find out would lose
            // the other failures beside it.
            failures
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((index, error));
        }
        Ok::<(), PackageError>(())
    })?;

    // Applied in plan order, so which error survives and which warnings appear
    // do not depend on which worker finished first.
    let mut failures = failures
        .into_inner()
        .unwrap_or_else(|error| error.into_inner());
    failures.sort_by_key(|(index, _)| *index);
    for (index, error) in failures {
        tolerate(manager, dropped, &plan.nodes[index], error)?;
    }
    Ok(())
}

/// Fills every tree position from the store, a level at a time.
///
/// The level barrier is not incidental. Filling a position clears its
/// directory first, and a package's dependencies live *inside* that directory,
/// so a parent filled after its children would delete them. Within one level
/// the destinations are distinct paths by construction, so there is nothing to
/// order and the whole level runs at once.
fn materialize(
    manager: &PackageManager,
    plan: &Plan,
    store: &ContentStore,
    limits: &Limits,
    dropped: &Mutex<HashSet<u32>>,
) -> Result<(), PackageError> {
    let positions = plan.nodes.iter().filter(|node| !node.record_only).count();
    manager.report(ProgressEvent::StorePlanned { total: positions });
    let deepest = plan.nodes.iter().map(|node| node.depth).max().unwrap_or(0);
    let cancel = Cancel::new();

    for depth in 0..=deepest {
        let level: Vec<usize> = plan
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.depth == depth && !node.record_only)
            .map(|(index, _)| index)
            .collect();
        if level.is_empty() {
            continue;
        }
        let failures: Mutex<Vec<(usize, PackageError)>> = Mutex::new(Vec::new());
        scheduler::run(limits.materialize, level, &cancel, |index, _| {
            let node = &plan.nodes[index];
            if is_dropped(dropped, node, plan) {
                return Ok(());
            }
            let _occupancy = manager.profile.occupy(Pool::Materialize);
            let outcome = manager
                .profile
                .time(Stage::Materialize, || match &node.source {
                    Source::Workspace(workspace) => {
                        copy_workspace(&workspace.path, &node.destination)
                    }
                    _ => store.materialize(
                        &node.content_key().unwrap_or_default(),
                        &node.destination,
                    ),
                });
            match outcome {
                Ok(()) => {
                    manager.profile.bump(Count::Materializations);
                    manager.report(ProgressEvent::Materialized { name: node.name() });
                }
                Err(error) => failures
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push((index, error)),
            }
            Ok::<(), PackageError>(())
        })?;
        let mut failures = failures
            .into_inner()
            .unwrap_or_else(|error| error.into_inner());
        failures.sort_by_key(|(index, _)| *index);
        for (index, error) in failures {
            tolerate(manager, dropped, &plan.nodes[index], error)?;
        }
    }
    Ok(())
}

/// The lifecycle scripts each distinct package declares, read once.
///
/// The abbreviated packument leaves `scripts` out and reports only a boolean,
/// so the authority is the manifest inside the package -- which means reading
/// a file. Once per position that would be 1,815 reads of 111 distinct files;
/// keyed by content hash it is 111.
fn script_index(
    plan: &Plan,
    dropped: &Mutex<HashSet<u32>>,
) -> HashMap<String, BTreeMap<String, String>> {
    let mut scripts = HashMap::new();
    if plan.from_lock {
        // A replay carries its scripts in the lockfile already.
        return scripts;
    }
    for node in &plan.nodes {
        let (Some(key), Source::Registry { package, .. }) = (node.content_key(), &node.source)
        else {
            continue;
        };
        if node.record_only || scripts.contains_key(&key) || is_dropped(dropped, node, plan) {
            continue;
        }
        scripts.insert(key, installed_scripts(package, &node.destination));
    }
    scripts
}

/// Writes `.bin` shims for every directory that received packages.
///
/// Only those directories: the recursive installer called this once per
/// installed package, including for the great majority that have no
/// dependencies of their own and so no `node_modules` for a `.bin` to go in.
fn link(manager: &PackageManager, plan: &Plan, limits: &Limits) -> Result<(), PackageError> {
    let directories: Vec<PathBuf> = plan
        .bin_directories
        .iter()
        .filter(|directory| directory.is_dir())
        .cloned()
        .collect();
    manager.report(ProgressEvent::LinkPlanned {
        total: directories.len(),
    });
    let cancel = Cancel::new();
    scheduler::run(limits.link, directories, &cancel, |directory, _| {
        let _occupancy = manager.profile.occupy(Pool::Link);
        manager
            .profile
            .time(Stage::Link, || link_binaries(&directory))?;
        manager.report(ProgressEvent::Linked);
        Ok::<(), PackageError>(())
    })
}

/// Runs lifecycle scripts in dependency order, one at a time.
///
/// Deliberately sequential. A postinstall is arbitrary code that reads and
/// writes the tree it was installed into and expects the tools it depends on
/// to be there already; running two at once is safe only once that has been
/// proven for the packages involved, which is not something an installer can
/// know. The order is the order subtrees finished, so nothing runs before what
/// it needs.
fn run_scripts(
    manager: &PackageManager,
    plan: &Plan,
    scripts: &HashMap<String, BTreeMap<String, String>>,
    dropped: &Mutex<HashSet<u32>>,
) -> Result<(), PackageError> {
    for index in &plan.finish_order {
        let node = &plan.nodes[*index];
        if node.record_only || is_dropped(dropped, node, plan) {
            continue;
        }
        let declared = match &node.source {
            Source::Registry { .. } => node
                .content_key()
                .and_then(|key| scripts.get(&key).cloned())
                .unwrap_or_default(),
            Source::Locked { package, .. } => package.scripts.clone(),
            Source::Workspace(workspace) => workspace.scripts.clone(),
        };
        if !["preinstall", "install", "postinstall"]
            .iter()
            .any(|name| declared.contains_key(*name))
        {
            continue;
        }
        let outcome = manager.profile.time(Stage::Lifecycle, || {
            run_lifecycle_scripts(&node.destination, &declared, &manager.root)
        });
        if let Err(error) = outcome {
            tolerate(manager, dropped, node, error)?;
        }
    }
    Ok(())
}
