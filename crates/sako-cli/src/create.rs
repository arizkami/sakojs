// SPDX-License-Identifier: BSD-3-Clause

//! `sako create` -- scaffold a project from an initializer package.
//!
//! Follows the naming convention npm established and bun and yarn adopted:
//! `create <name>` runs the package `create-<name>`. That indirection is the
//! whole feature, so the mapping lives in `initializer` where it can be tested
//! against the awkward cases (scopes, versions, already-prefixed names).
//!
//! Unlike `sako x`, the package is usually *not* installed locally -- the point
//! is to run it in an empty directory. It is fetched into a cache and executed
//! from there, leaving the user's directory untouched apart from whatever the
//! initializer itself writes.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sako_package::{PackageManager, PackageManagerOptions};

use crate::node_bin;
use crate::style::Painter;

/// Maps a `create` argument onto the package that implements it.
///
/// Returns the specifier to install (version range included, if given) and the
/// bare package name to look up afterwards.
pub fn initializer(argument: &str) -> Result<(String, String), String> {
    if argument.is_empty() {
        return Err("create requires an initializer name".into());
    }
    // Split a trailing version range. The leading @ of a scope is not a
    // separator, so search from position 1.
    let (name, version) = match argument[1..].find('@') {
        Some(offset) => {
            let split = offset + 1;
            (&argument[..split], Some(&argument[split + 1..]))
        }
        None => (argument, None),
    };
    if name.is_empty() {
        return Err(format!("cannot read an initializer name from '{argument}'"));
    }

    let package = if let Some(scoped) = name.strip_prefix('@') {
        match scoped.split_once('/') {
            // @scope/foo -> @scope/create-foo
            Some((scope, rest)) if !rest.is_empty() => {
                if rest.starts_with("create-") || rest == "create" {
                    format!("@{scope}/{rest}")
                } else {
                    format!("@{scope}/create-{rest}")
                }
            }
            // @scope -> @scope/create, the convention for a scope's own tool.
            _ => format!("@{scoped}/create"),
        }
    } else if name.starts_with("create-") {
        name.to_owned()
    } else {
        format!("create-{name}")
    };

    let specifier = match version {
        Some(version) if !version.is_empty() => format!("{package}@{version}"),
        _ => package.clone(),
    };
    Ok((specifier, package))
}

/// Where fetched initializers live.
///
/// Kept out of the user's project on purpose: `create` frequently runs in a
/// directory that is about to become a new project, and writing a node_modules
/// there before the initializer runs would pollute what it scaffolds.
fn cache_root() -> Result<PathBuf, String> {
    let base = if cfg!(windows) {
        env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
    };
    base.map(|base| base.join("sako").join("create"))
        .ok_or_else(|| "cannot locate a cache directory for initializers".to_owned())
}

/// Makes a specifier safe to use as a single directory name.
fn cache_key(specifier: &str) -> String {
    specifier
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '.' => character,
            _ => '_',
        })
        .collect()
}

pub fn run(arguments: &[OsString]) -> Result<u8, String> {
    let painter = Painter::stderr();
    let Some((requested, rest)) = arguments.split_first() else {
        return Err(format!(
            "create requires an initializer\n       {}",
            painter.dim("example: sako create vite my-app"),
        ));
    };
    let requested = requested.to_string_lossy().into_owned();
    let (specifier, package) = initializer(&requested)?;

    let directory =
        env::current_dir().map_err(|error| format!("cannot read current directory: {error}"))?;

    // A locally installed initializer wins, so a repository can pin its own.
    let (executable, search_root) = match node_bin::resolve(&directory, &package) {
        Some(executable) => (executable, directory.clone()),
        None => {
            let cache = cache_root()?.join(cache_key(&specifier));
            let executable = ensure_cached(&cache, &specifier, &package, painter)?;
            (executable, cache)
        }
    };

    let mut path_directories = node_bin::bin_directories(&search_root);
    path_directories.extend(node_bin::bin_directories(&directory));

    let status = Command::new(&executable)
        .args(rest)
        // The initializer scaffolds into the user's directory, not the cache.
        .current_dir(&directory)
        .env("PATH", node_bin::path_with(&path_directories))
        .env(
            "SAKO_EXECUTABLE",
            env::current_exe().unwrap_or_else(|_| PathBuf::from("sako")),
        )
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("cannot run {}: {error}", executable.display()))?;
    Ok(crate::exit_code(status))
}

/// Installs `specifier` into `cache` unless it is already there, and returns
/// the executable to run.
fn ensure_cached(
    cache: &Path,
    specifier: &str,
    package: &str,
    painter: Painter,
) -> Result<PathBuf, String> {
    // Reuse whatever was fetched last time. An explicit version in the
    // specifier produces a different cache key, so pinning still refetches.
    if let Some(executable) = cached_binary(cache, package) {
        return Ok(executable);
    }

    fs::create_dir_all(cache)
        .map_err(|error| format!("cannot create {}: {error}", cache.display()))?;
    eprintln!("{} {}", painter.dim("fetching"), painter.cyan(specifier));

    let mut manager =
        PackageManager::new_with_options(cache.to_path_buf(), PackageManagerOptions::default())
            .map_err(|error| error.to_string())?;
    manager
        .add(specifier, false)
        .map_err(|error| format!("cannot install {specifier}: {error}"))?;

    cached_binary(cache, package).ok_or_else(|| {
        format!(
            "{specifier} installed but provides no executable\n       {}",
            painter.dim("the package may not be a project initializer"),
        )
    })
}

/// Finds the executable an initializer provides.
///
/// Usually it is named after the package, but a package is free to name it
/// anything, so fall back to the sole entry in `.bin` when there is exactly
/// one and the guess did not land.
fn cached_binary(cache: &Path, package: &str) -> Option<PathBuf> {
    let bare = package.rsplit('/').next().unwrap_or(package);
    if let Some(executable) = node_bin::resolve(cache, bare) {
        return Some(executable);
    }
    let names = node_bin::binary_names(&cache.join("node_modules").join(".bin"));
    match names.as_slice() {
        [only] => node_bin::resolve(cache, only),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::initializer;

    fn map(argument: &str) -> (String, String) {
        initializer(argument).expect("initializer name should map")
    }

    #[test]
    fn bare_name_gains_the_create_prefix() {
        assert_eq!(map("vite"), ("create-vite".into(), "create-vite".into()));
    }

    #[test]
    fn an_already_prefixed_name_is_left_alone() {
        assert_eq!(
            map("create-vite"),
            ("create-vite".into(), "create-vite".into())
        );
    }

    #[test]
    fn a_version_is_kept_on_the_specifier_but_not_the_package() {
        assert_eq!(
            map("vite@latest"),
            ("create-vite@latest".into(), "create-vite".into())
        );
        assert_eq!(
            map("vite@^7.1"),
            ("create-vite@^7.1".into(), "create-vite".into())
        );
    }

    #[test]
    fn a_scope_takes_the_prefix_after_the_slash() {
        assert_eq!(
            map("@acme/app"),
            ("@acme/create-app".into(), "@acme/create-app".into())
        );
    }

    #[test]
    fn a_bare_scope_maps_to_the_scopes_own_initializer() {
        assert_eq!(map("@acme"), ("@acme/create".into(), "@acme/create".into()));
    }

    #[test]
    fn a_scoped_name_keeps_a_version_and_is_not_double_prefixed() {
        assert_eq!(
            map("@acme/create-app@2"),
            ("@acme/create-app@2".into(), "@acme/create-app".into())
        );
    }

    #[test]
    fn an_empty_name_is_rejected() {
        assert!(initializer("").is_err());
    }
}
