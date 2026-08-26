// SPDX-License-Identifier: BSD-3-Clause

//! A content-addressed store of *unpacked* packages.
//!
//! The tarball store already meant a package was downloaded once however many
//! times the graph mentioned it. It did not mean it was unpacked once: a tree
//! with 1,815 positions built from 111 distinct packages ran gzip and tar
//! 1,815 times, because unpacking was something a tree position did rather
//! than something a package had. That was the single largest cost in an
//! install after the network.
//!
//! Here a package is unpacked once, under the hash of the archive it came
//! from, and each tree position that wants it is filled by copying that
//! directory. Copying a dozen small files is far cheaper than decompressing
//! and re-parsing an archive, and it survives the install: the second install
//! of the same package does not unpack anything at all.
//!
//! Promotion is a rename of a fully-written directory, so a store object is
//! either absent or complete. There is no window in which a half-unpacked
//! package is reachable under the name of a finished one -- including when two
//! workers, or two processes, unpack the same content at the same moment.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{MAXIMUM_STORE_BYTES, MAXIMUM_STORE_FILES, PackageError, extract_archive};

#[derive(Debug)]
pub struct ContentStore {
    root: PathBuf,
    temporary: AtomicU64,
}

impl ContentStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            temporary: AtomicU64::new(0),
        }
    }

    /// Where the unpacked form of `key` lives, whether or not it is there yet.
    ///
    /// Sharded on the first two characters the same way the tarball store is,
    /// because a flat directory of tens of thousands of entries is slow to
    /// open on every filesystem this runs on.
    pub fn object(&self, key: &str) -> PathBuf {
        self.root.join(&key[..2]).join(key)
    }

    /// Whether the unpacked form is already there.
    pub fn contains(&self, key: &str) -> bool {
        self.object(key).is_dir()
    }

    /// Unpacks `bytes` under `key`, or leaves the copy already there alone.
    ///
    /// Returns the store object's path either way. The caller does not learn
    /// which of the two happened because it must not care: losing the race is
    /// the ordinary outcome when a graph mentions one package from several
    /// places, and the winner's object is the same content by construction --
    /// the key is the hash of the archive both unpacked.
    pub fn populate(&self, key: &str, bytes: &[u8]) -> Result<PathBuf, PackageError> {
        let object = self.object(key);
        if object.is_dir() {
            return Ok(object);
        }
        let Some(parent) = object.parent() else {
            return Err(PackageError("content store path has no parent".into()));
        };
        fs::create_dir_all(parent)?;

        let staging = self.staging_path(&object);
        // A leftover from an interrupted install would otherwise make the
        // extraction fail on a directory it did not write.
        let _ = fs::remove_dir_all(&staging);
        if let Err(error) = extract_archive(bytes, &staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        if fs::rename(&staging, &object).is_err() {
            // Either another worker promoted the same content first, which is
            // fine and expected, or the rename genuinely failed and the
            // directory is still not there, which the caller will see.
            let _ = fs::remove_dir_all(&staging);
        }
        if object.is_dir() {
            Ok(object)
        } else {
            Err(PackageError(format!(
                "could not promote {key} into the content store"
            )))
        }
    }

    /// Fills a tree position from the store object.
    ///
    /// The destination is replaced rather than merged: a position left behind
    /// by a previous install may hold files this version does not ship, and
    /// keeping them would leave a package that is neither version.
    pub fn materialize(&self, key: &str, destination: &Path) -> Result<(), PackageError> {
        let object = self.object(key);
        if !object.is_dir() {
            return Err(PackageError(format!(
                "content store is missing an object for {key}"
            )));
        }
        if destination.exists() {
            fs::remove_dir_all(destination)?;
        }
        copy_tree(&object, destination)
    }

    /// A staging path nobody else will pick.
    ///
    /// Both parts matter: the process id keeps two `sako` processes apart, and
    /// the counter keeps two workers inside one process apart.
    fn staging_path(&self, object: &Path) -> PathBuf {
        let ticket = self.temporary.fetch_add(1, Ordering::Relaxed);
        let process = std::process::id();
        let mut name = object.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{process}.{ticket}.staging"));
        object.with_file_name(name)
    }

    /// Drops the oldest objects once the store is past its limits, and clears
    /// staging directories an interrupted install left behind.
    ///
    /// Counted in objects rather than files: a store object is one package, so
    /// the package-count limit means the same thing here as the archive-count
    /// limit means for tarballs.
    pub fn prune(&self) -> Result<(), PackageError> {
        if !self.root.is_dir() {
            return Ok(());
        }
        let mut objects = Vec::new();
        let mut total_bytes = 0_u64;
        for shard in fs::read_dir(&self.root)?.flatten() {
            if !shard.path().is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())?.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                if path.extension() == Some(OsStr::new("staging")) {
                    let _ = fs::remove_dir_all(&path);
                    continue;
                }
                let metadata = entry.metadata()?;
                let size = directory_size(&path);
                total_bytes = total_bytes.saturating_add(size);
                let modified = metadata
                    .modified()
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                objects.push((modified, size, path));
            }
        }
        objects.sort_by(|(left, _, left_path), (right, _, right_path)| {
            left.cmp(right).then_with(|| left_path.cmp(right_path))
        });
        let mut count = objects.len();
        for (_, size, path) in objects {
            if count <= MAXIMUM_STORE_FILES && total_bytes <= MAXIMUM_STORE_BYTES {
                break;
            }
            if fs::remove_dir_all(&path).is_ok() {
                count -= 1;
                total_bytes = total_bytes.saturating_sub(size);
            }
        }
        Ok(())
    }
}

/// Bytes a store object occupies, near enough for an eviction decision.
fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0_u64;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        total = total.saturating_add(if metadata.is_dir() {
            directory_size(&entry.path())
        } else {
            metadata.len()
        });
    }
    total
}

/// Copies a store object into place.
///
/// Files rather than links. A hard link would be faster still and is what a
/// store like this invites, but it would also make every copy of a package the
/// same bytes on disk: a postinstall script that rewrites a file in its own
/// `node_modules` would silently rewrite it for every other project on the
/// machine. The store is shared, so the tree has to own its files.
fn copy_tree(source: &Path, destination: &Path) -> Result<(), PackageError> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)?.flatten() {
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            copy_tree(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)?;
        }
        // Anything else was already dropped on the way into the store.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("sako-store-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn copies_a_tree_including_nested_directories() {
        let root = temporary_root("copy");
        let source = root.join("source");
        fs::create_dir_all(source.join("lib")).unwrap();
        fs::write(source.join("package.json"), b"{}").unwrap();
        fs::write(source.join("lib").join("index.js"), b"module.exports=1").unwrap();

        let destination = root.join("destination");
        copy_tree(&source, &destination).unwrap();

        assert_eq!(fs::read(destination.join("package.json")).unwrap(), b"{}");
        assert_eq!(
            fs::read(destination.join("lib").join("index.js")).unwrap(),
            b"module.exports=1"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn materializing_replaces_whatever_was_there() {
        let root = temporary_root("replace");
        let store = ContentStore::new(root.join("unpacked"));
        let key = "ab".repeat(32);
        let object = store.object(&key);
        fs::create_dir_all(&object).unwrap();
        fs::write(object.join("kept.js"), b"new").unwrap();

        let destination = root.join("node_modules").join("thing");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("stale.js"), b"old").unwrap();

        store.materialize(&key, &destination).unwrap();

        assert!(destination.join("kept.js").is_file());
        assert!(!destination.join("stale.js").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_object_is_an_error_rather_than_an_empty_directory() {
        let root = temporary_root("missing");
        let store = ContentStore::new(root.join("unpacked"));
        let destination = root.join("thing");
        assert!(store.materialize(&"cd".repeat(32), &destination).is_err());
        assert!(!destination.exists());
        let _ = fs::remove_dir_all(&root);
    }
}
