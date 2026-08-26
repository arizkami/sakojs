// SPDX-License-Identifier: BSD-3-Clause

//! Resolving the dependency graph, concurrently.
//!
//! The installer used to resolve by recursing: fetch a package's metadata,
//! download it, unpack it, then start on its first dependency. Every registry
//! round trip in the graph happened one after another, so a hundred packages
//! at a hundred milliseconds each cost ten seconds no matter how independent
//! they were.
//!
//! This walks the same graph as a work queue instead. Resolution is keyed by
//! *what is being asked for* rather than by where in the tree it was asked
//! from, which is what makes the walk both concurrent and small: a tree of
//! 1,815 positions asks about roughly 160 distinct things, and the 1,655
//! repeats join a result that is already there or already on its way.
//!
//! What this stage deliberately does **not** do is decide anything about the
//! tree. It produces a map from question to answer, and nothing else. Building
//! the tree from that map is a separate, single-threaded, entirely
//! deterministic pass -- which is what lets the graph be walked in whatever
//! order the workers happen to finish in without the lockfile ever noticing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::registry::{MetadataCache, Registry};
use crate::scheduler::{self, Cancel};
use crate::{
    MAXIMUM_METADATA_ENTRIES, MAXIMUM_PACKAGES, PackageError, PackageVersion, ProgressEvent,
    ProgressReporter, Stage, WorkspacePackage, select_version, validate_package_name,
    workspace_requirement_matches,
};

/// The canonical identity of a resolution request.
///
/// The registry is part of the key, not an implementation detail of the
/// lookup: the same name from a private registry and from the public one are
/// different packages, and joining their requests would hand one project's
/// scoped package to another's.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResolveKey {
    pub registry: String,
    pub name: String,
    pub requirement: String,
}

impl ResolveKey {
    fn new(registry: &Registry, name: &str, requirement: &str) -> Self {
        Self {
            registry: registry.registry_for(name).to_owned(),
            name: name.to_owned(),
            requirement: requirement.to_owned(),
        }
    }
}

/// Every question the graph asked, and the answer to it.
///
/// Failures are recorded rather than thrown, because at the time a worker
/// discovers one it does not yet know whether anything actually needs it: the
/// same package can be reached through a required edge and an optional one,
/// and only the tree walk knows which. Deciding there would make the outcome
/// depend on which thread got there first.
#[derive(Debug, Default)]
pub struct Graph {
    entries: HashMap<ResolveKey, Result<Arc<PackageVersion>, String>>,
}

impl Graph {
    /// The answer for one edge, as the tree walk asks for it.
    pub fn select(
        &self,
        registry: &Registry,
        name: &str,
        requirement: &str,
    ) -> Result<Arc<PackageVersion>, PackageError> {
        match self.entries.get(&ResolveKey::new(registry, name, requirement)) {
            Some(Ok(package)) => Ok(Arc::clone(package)),
            Some(Err(reason)) => Err(PackageError(reason.clone())),
            // Only reachable for an edge the walk reached but the resolver did
            // not, which would be a bug in one of the two rather than anything
            // a user did.
            None => Err(PackageError(format!(
                "{name}@{requirement} was never resolved"
            ))),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// One request in the queue.
struct Request {
    name: String,
    requirement: String,
}

/// Walks the whole reachable graph, resolving independent packages at the same
/// time.
///
/// `workspaces` are matched here so a dependency satisfied by a local package
/// never becomes a registry request, exactly as the sequential walk did.
pub fn resolve_graph(
    registry: &Arc<Registry>,
    metadata: &Arc<MetadataCache>,
    workspaces: &BTreeMap<String, WorkspacePackage>,
    roots: &[(String, String)],
    workers: usize,
    reporter: Option<&dyn ProgressReporter>,
) -> Result<Graph, PackageError> {
    let entries: Mutex<HashMap<ResolveKey, Result<Arc<PackageVersion>, String>>> =
        Mutex::new(HashMap::new());
    // Enqueue guard. Separate from `entries` because a key is claimed when it
    // is queued, long before there is an answer to put in the map -- without
    // that, sixteen workers expanding sixteen branches would queue the same
    // popular package sixteen times over.
    let claimed: Mutex<HashSet<ResolveKey>> = Mutex::new(HashSet::new());
    let cancel = Cancel::new();

    let initial: Vec<Request> = roots
        .iter()
        .filter(|(name, requirement)| {
            claim(&claimed, registry, name, requirement) && !is_workspace(workspaces, name, requirement)
        })
        .map(|(name, requirement)| Request {
            name: name.clone(),
            requirement: requirement.clone(),
        })
        .collect();

    scheduler::run(workers, initial, &cancel, |request, queue| {
        let Request { name, requirement } = request;
        validate_package_name(&name)?;

        if let Some(reporter) = reporter {
            reporter.report(ProgressEvent::ResolveStarted { name: &name });
        }
        let resolved = metadata
            .get(registry, &name, MAXIMUM_METADATA_ENTRIES)
            .and_then(|metadata| {
                registry
                    .profile
                    .time(Stage::Semver, || select_version(&metadata, &requirement))
            });
        if let Some(reporter) = reporter {
            reporter.report(ProgressEvent::ResolveFinished { name: &name });
        }

        let key = ResolveKey::new(registry, &name, &requirement);
        let expand = match &resolved {
            // A package built for another platform is a leaf whichever kind of
            // edge reached it: a required one fails on it and an optional one
            // steps over it, and neither installs anything underneath. Not
            // expanding is also what keeps a Windows install from fetching the
            // metadata of every Linux and macOS package below every native
            // binary it was never going to use.
            Ok(package) => package.supports_host().then(|| {
                // Both maps, because a name in `optionalDependencies` still has
                // to be resolved -- that is how the tree walk finds out whether
                // it is a build for this platform. Deduplicated, because the
                // abbreviated packument lists optional dependencies in both and
                // they are the same question asked twice.
                let mut children: BTreeMap<String, String> = package.dependencies.clone();
                children.extend(
                    package
                        .optional_dependencies
                        .iter()
                        .map(|(name, requirement)| (name.clone(), requirement.clone())),
                );
                children.into_iter().collect::<Vec<_>>()
            }),
            Err(_) => None,
        };

        {
            let mut entries = entries.lock().unwrap_or_else(|error| error.into_inner());
            if entries.len() >= MAXIMUM_PACKAGES {
                return Err(PackageError("package graph capacity exceeded".into()));
            }
            entries.insert(
                key,
                resolved.map_err(|error: PackageError| error.to_string()),
            );
        }

        for (child, child_requirement) in expand.unwrap_or_default() {
            if is_workspace(workspaces, &child, &child_requirement) {
                continue;
            }
            if claim(&claimed, registry, &child, &child_requirement) {
                queue.push(Request {
                    name: child,
                    requirement: child_requirement,
                });
            }
        }
        Ok(())
    })?;

    Ok(Graph {
        entries: entries
            .into_inner()
            .unwrap_or_else(|error| error.into_inner()),
    })
}

/// Whether this edge is served by a local workspace package, in which case the
/// registry never hears about it.
fn is_workspace(
    workspaces: &BTreeMap<String, WorkspacePackage>,
    name: &str,
    requirement: &str,
) -> bool {
    workspaces
        .get(name)
        .is_some_and(|workspace| workspace_requirement_matches(&workspace.version, requirement))
        || requirement.starts_with("workspace:")
}

/// Takes ownership of a key for whoever calls first, so it is queued once.
fn claim(
    claimed: &Mutex<HashSet<ResolveKey>>,
    registry: &Registry,
    name: &str,
    requirement: &str,
) -> bool {
    claimed
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(ResolveKey::new(registry, name, requirement))
}
