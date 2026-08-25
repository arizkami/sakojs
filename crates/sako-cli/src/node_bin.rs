// SPDX-License-Identifier: BSD-3-Clause

//! Locating executables inside `node_modules`, the way npm and bun do.
//!
//! Two consumers: package scripts (`sako run dev`), which need the directories
//! on `PATH` so a bare `vite` resolves, and `sako x`, which needs the concrete
//! file so it can be executed directly.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Every `node_modules/.bin` from `start` up to the filesystem root, nearest
/// first. npm searches the same chain, which is what lets a script in a
/// workspace package reach a hoisted binary at the repository root.
pub fn bin_directories(start: &Path) -> Vec<PathBuf> {
    start
        .ancestors()
        .map(|directory| directory.join("node_modules").join(".bin"))
        .filter(|directory| directory.is_dir())
        .collect()
}

/// `PATH` with the `.bin` chain prepended, plus the directory holding the
/// running Sako executable.
///
/// Sako's own directory is included because the shims written by
/// `sako-package` invoke `sako`; without it, `sako run dev` would only work
/// when Sako happened to be installed globally.
pub fn augmented_path(start: &Path) -> OsString {
    path_with(&bin_directories(start))
}

/// `PATH` with `directories` prepended, plus the directory holding the running
/// Sako executable. Duplicates are dropped so repeated calls cannot grow it.
pub fn path_with(directories: &[PathBuf]) -> OsString {
    let mut prefix: Vec<PathBuf> = Vec::new();
    for directory in directories {
        if !prefix.contains(directory) {
            prefix.push(directory.clone());
        }
    }
    if let Ok(executable) = env::current_exe()
        && let Some(parent) = executable.parent()
    {
        let parent = parent.to_path_buf();
        if !prefix.contains(&parent) {
            prefix.push(parent);
        }
    }
    match env::var_os("PATH") {
        Some(existing) => {
            let mut joined = env::join_paths(prefix).unwrap_or_default();
            if !joined.is_empty() && !existing.is_empty() {
                joined.push(PATH_SEPARATOR);
            }
            joined.push(existing);
            joined
        }
        None => env::join_paths(prefix).unwrap_or_default(),
    }
}

/// Command names present in a `.bin` directory, with Windows extensions
/// folded away so `tsc.cmd` and `tsc` count once.
pub fn binary_names(bin_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(bin_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let stem = if cfg!(windows) {
                path.file_stem()
            } else {
                path.file_name()
            };
            stem.and_then(|value| value.to_str()).map(str::to_owned)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(windows)]
const PATH_SEPARATOR: &str = ";";
#[cfg(not(windows))]
const PATH_SEPARATOR: &str = ":";

/// Windows has no execute bit, so an "executable" is decided by extension.
/// PATHEXT is consulted so a `.bat`/`.exe` shim is found as readily as `.cmd`.
#[cfg(windows)]
fn candidate_names(command: &str) -> Vec<String> {
    let mut names = vec![format!("{command}.cmd")];
    let pathext = env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
        .to_lowercase();
    for extension in pathext.split(';').filter(|value| !value.is_empty()) {
        let name = format!("{command}{extension}");
        if !names.contains(&name) {
            names.push(name);
        }
    }
    // The extensionless shim last: usable from Git Bash, not by cmd.exe.
    names.push(command.to_owned());
    names
}

#[cfg(not(windows))]
fn candidate_names(command: &str) -> Vec<String> {
    vec![command.to_owned()]
}

/// Finds `command` in the `.bin` chain rooted at `start`.
pub fn resolve(start: &Path, command: &str) -> Option<PathBuf> {
    // A path-like request is not a package binary; let the caller handle it.
    if command.contains(['/', '\\']) {
        return None;
    }
    for directory in bin_directories(start) {
        for name in candidate_names(command) {
            let candidate = directory.join(&name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Package binaries that exist but were never linked into `.bin`.
///
/// Used only to improve the error message: knowing the package is present but
/// unlinked points at `sako install`, whereas a genuinely absent package
/// points at `sako add`.
pub fn installed_packages_providing(start: &Path, command: &str) -> Vec<String> {
    let mut found = Vec::new();
    for directory in start.ancestors() {
        let node_modules = directory.join("node_modules");
        if !node_modules.is_dir() {
            continue;
        }
        let candidate = node_modules.join(command);
        if candidate.join("package.json").is_file() {
            found.push(command.to_owned());
        }
    }
    found
}
