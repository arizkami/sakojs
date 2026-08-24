// SPDX-License-Identifier: BSD-3-Clause

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::read::GzDecoder;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";
const MAXIMUM_PACKAGES: usize = 10_000;
const MAXIMUM_METADATA_ENTRIES: usize = 1_024;
const MAXIMUM_METADATA_BYTES: u64 = 64 * 1024 * 1024;
const MAXIMUM_TARBALL_BYTES: u64 = 512 * 1024 * 1024;
const MAXIMUM_EXTRACTED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAXIMUM_ARCHIVE_ENTRIES: usize = 100_000;
const MAXIMUM_STORE_FILES: usize = 50_000;
const MAXIMUM_STORE_BYTES: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Debug)]
pub struct PackageError(String);

impl fmt::Display for PackageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for PackageError {}

impl From<io::Error> for PackageError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<serde_json::Error> for PackageError {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}

#[derive(Debug)]
pub struct PackageManager {
    root: PathBuf,
    registry: String,
    cache_root: PathBuf,
    metadata: HashMap<String, Metadata>,
    ignore_scripts: bool,
    installed: BTreeMap<String, LockedPackage>,
    active: HashSet<String>,
}

impl PackageManager {
    pub fn new(root: impl Into<PathBuf>, ignore_scripts: bool) -> Result<Self, PackageError> {
        let root = root.into();
        let registry = env::var("SAKO_NPM_REGISTRY").unwrap_or_else(|_| DEFAULT_REGISTRY.into());
        let cache_root = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| PackageError("LOCALAPPDATA is not set".into()))?
            .join("Sako")
            .join("Store")
            .join("sha512");
        Ok(Self {
            root,
            registry: registry.trim_end_matches('/').into(),
            cache_root,
            metadata: HashMap::new(),
            ignore_scripts,
            installed: BTreeMap::new(),
            active: HashSet::new(),
        })
    }

    pub fn install(&mut self) -> Result<(), PackageError> {
        self.installed.clear();
        self.active.clear();
        let manifest = self.read_manifest()?;
        let mut dependencies = manifest_dependencies(&manifest, "dependencies")?;
        for (name, requirement) in manifest_dependencies(&manifest, "devDependencies")? {
            dependencies.entry(name).or_insert(requirement);
        }
        for (name, requirement) in manifest_dependencies(&manifest, "optionalDependencies")? {
            dependencies.entry(name).or_insert(requirement);
        }

        let node_modules = self.root.join("node_modules");
        fs::create_dir_all(&node_modules)?;
        for (name, requirement) in dependencies {
            self.install_dependency(
                &name,
                &requirement,
                &node_modules,
                &format!("node_modules/{name}"),
            )?;
        }
        self.write_lockfile()
    }

    pub fn add(&mut self, specifier: &str, development: bool) -> Result<(), PackageError> {
        let (name, mut requirement) = parse_package_specifier(specifier)?;
        if requirement == "latest" {
            let selected = self.resolve(&name, &requirement)?;
            requirement = format!("^{}", selected.version);
        }
        let mut manifest = self.read_or_create_manifest()?;
        let section = if development {
            "devDependencies"
        } else {
            "dependencies"
        };
        let object = manifest
            .as_object_mut()
            .ok_or_else(|| PackageError("package.json must contain an object".into()))?;
        let dependencies = object
            .entry(section)
            .or_insert_with(|| serde_json::Value::Object(Default::default()))
            .as_object_mut()
            .ok_or_else(|| PackageError(format!("package.json {section} must be an object")))?;
        dependencies.insert(name, serde_json::Value::String(requirement));
        self.write_manifest(&manifest)?;
        self.install()
    }

    pub fn remove(&mut self, name: &str) -> Result<(), PackageError> {
        validate_package_name(name)?;
        let mut manifest = self.read_manifest()?;
        let object = manifest
            .as_object_mut()
            .ok_or_else(|| PackageError("package.json must contain an object".into()))?;
        let mut removed = false;
        for section in ["dependencies", "devDependencies", "optionalDependencies"] {
            if let Some(dependencies) = object
                .get_mut(section)
                .and_then(serde_json::Value::as_object_mut)
            {
                removed |= dependencies.remove(name).is_some();
            }
        }
        if !removed {
            return Err(PackageError(format!(
                "package '{name}' is not a dependency"
            )));
        }
        self.write_manifest(&manifest)?;
        let installed_path = package_install_path(&self.root.join("node_modules"), name)?;
        if installed_path.exists() {
            fs::remove_dir_all(&installed_path)?;
        }
        self.install()
    }

    pub fn update(&mut self) -> Result<(), PackageError> {
        let lockfile = self.root.join("sako.lock");
        if lockfile.exists() {
            fs::remove_file(lockfile)?;
        }
        self.install()
    }

    fn install_dependency(
        &mut self,
        name: &str,
        requirement: &str,
        parent_node_modules: &Path,
        lock_path: &str,
    ) -> Result<(), PackageError> {
        if self.installed.len() >= MAXIMUM_PACKAGES {
            return Err(PackageError("package graph capacity exceeded".into()));
        }
        validate_package_name(name)?;
        let package = self.resolve(name, requirement)?;
        let identity = format!("{}@{}", package.name, package.version);
        if !self.active.insert(identity.clone()) {
            return Ok(());
        }

        let destination = package_install_path(parent_node_modules, name)?;
        let archive = self.fetch_archive(&package.dist)?;
        extract_archive(&archive, &destination)?;

        self.installed.insert(
            lock_path.into(),
            LockedPackage {
                name: package.name.clone(),
                version: package.version.clone(),
                resolved: package.dist.tarball.clone(),
                integrity: package.dist.integrity.clone(),
                dependencies: package.dependencies.clone(),
            },
        );

        let child_node_modules = destination.join("node_modules");
        for (dependency, child_requirement) in package.dependencies.clone() {
            let child_lock_path = format!("{lock_path}/node_modules/{dependency}");
            self.install_dependency(
                &dependency,
                &child_requirement,
                &child_node_modules,
                &child_lock_path,
            )?;
        }
        if !self.ignore_scripts {
            run_lifecycle_scripts(&destination, &package.scripts)?;
        }
        self.active.remove(&identity);
        Ok(())
    }

    fn resolve(&mut self, name: &str, requirement: &str) -> Result<PackageVersion, PackageError> {
        if !self.metadata.contains_key(name) {
            if self.metadata.len() >= MAXIMUM_METADATA_ENTRIES {
                return Err(PackageError(
                    "registry metadata cache capacity exceeded".into(),
                ));
            }
            let encoded = name.replace('/', "%2f");
            let url = format!("{}/{encoded}", self.registry);
            let response = ureq::get(&url)
                .set("Accept", "application/vnd.npm.install-v1+json")
                .call()
                .map_err(|error| {
                    PackageError(format!("registry request failed for {name}: {error}"))
                })?;
            let mut bytes = Vec::new();
            response
                .into_reader()
                .take(MAXIMUM_METADATA_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAXIMUM_METADATA_BYTES {
                return Err(PackageError(format!(
                    "registry metadata for {name} exceeds byte limit"
                )));
            }
            let metadata: Metadata = serde_json::from_slice(&bytes).map_err(|error| {
                PackageError(format!("invalid registry metadata for {name}: {error}"))
            })?;
            self.metadata.insert(name.into(), metadata);
        }
        select_version(self.metadata.get(name).unwrap(), requirement)
    }

    fn fetch_archive(&self, distribution: &Distribution) -> Result<Vec<u8>, PackageError> {
        let expected = parse_sha512_integrity(&distribution.integrity)?;
        let digest_hex = hex(&expected);
        let cache_path = self
            .cache_root
            .join(&digest_hex[..2])
            .join(format!("{digest_hex}.tgz"));
        if cache_path.is_file() {
            let bytes = fs::read(&cache_path)?;
            verify_integrity(&bytes, &expected)?;
            return Ok(bytes);
        }

        let response = ureq::get(&distribution.tarball)
            .call()
            .map_err(|error| PackageError(format!("tarball download failed: {error}")))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(MAXIMUM_TARBALL_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAXIMUM_TARBALL_BYTES {
            return Err(PackageError("package tarball exceeds byte limit".into()));
        }
        verify_integrity(&bytes, &expected)?;
        if let Some(parent) = cache_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = cache_path.with_extension("tmp");
        fs::write(&temporary, &bytes)?;
        fs::rename(temporary, cache_path)?;
        prune_store(&self.cache_root)?;
        Ok(bytes)
    }

    fn read_manifest(&self) -> Result<serde_json::Value, PackageError> {
        let path = self.root.join("package.json");
        let source = fs::read_to_string(&path)
            .map_err(|error| PackageError(format!("cannot read {}: {error}", path.display())))?;
        serde_json::from_str(&source)
            .map_err(|error| PackageError(format!("invalid {}: {error}", path.display())))
    }

    fn read_or_create_manifest(&self) -> Result<serde_json::Value, PackageError> {
        if self.root.join("package.json").is_file() {
            self.read_manifest()
        } else {
            Ok(serde_json::json!({"private": true}))
        }
    }

    fn write_manifest(&self, manifest: &serde_json::Value) -> Result<(), PackageError> {
        fs::create_dir_all(&self.root)?;
        let mut source = serde_json::to_string_pretty(manifest)?;
        source.push('\n');
        fs::write(self.root.join("package.json"), source)?;
        Ok(())
    }

    fn write_lockfile(&self) -> Result<(), PackageError> {
        let lockfile = Lockfile {
            lockfile_version: 1,
            packages: self.installed.clone(),
        };
        let mut source = serde_json::to_string_pretty(&lockfile)?;
        source.push('\n');
        fs::write(self.root.join("sako.lock"), source)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Metadata {
    #[serde(rename = "dist-tags", default)]
    dist_tags: BTreeMap<String, String>,
    versions: BTreeMap<String, PackageVersion>,
}

#[derive(Clone, Debug, Deserialize)]
struct PackageVersion {
    name: String,
    version: String,
    dist: Distribution,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    scripts: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
struct Distribution {
    tarball: String,
    integrity: String,
}

#[derive(Clone, Debug, Serialize)]
struct Lockfile {
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u32,
    packages: BTreeMap<String, LockedPackage>,
}

#[derive(Clone, Debug, Serialize)]
struct LockedPackage {
    name: String,
    version: String,
    resolved: String,
    integrity: String,
    dependencies: BTreeMap<String, String>,
}

fn select_version(metadata: &Metadata, requirement: &str) -> Result<PackageVersion, PackageError> {
    if let Some(version) = metadata.dist_tags.get(requirement) {
        return metadata.versions.get(version).cloned().ok_or_else(|| {
            PackageError(format!(
                "dist-tag '{requirement}' points to a missing version"
            ))
        });
    }
    let requirement = if requirement.is_empty() {
        "*"
    } else {
        requirement
    };
    let parsed = VersionReq::parse(requirement).map_err(|error| {
        PackageError(format!(
            "unsupported semver requirement '{requirement}': {error}"
        ))
    })?;
    metadata
        .versions
        .iter()
        .filter_map(|(version, package)| {
            Version::parse(version)
                .ok()
                .map(|version| (version, package))
        })
        .filter(|(version, _)| parsed.matches(version))
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, package)| package.clone())
        .ok_or_else(|| PackageError(format!("no version satisfies '{requirement}'")))
}

fn parse_package_specifier(specifier: &str) -> Result<(String, String), PackageError> {
    let split = if specifier.starts_with('@') {
        specifier
            .rfind('@')
            .filter(|index| *index > specifier.find('/').unwrap_or(usize::MAX))
    } else {
        specifier.rfind('@').filter(|index| *index != 0)
    };
    let (name, requirement) = split
        .map(|index| (&specifier[..index], &specifier[index + 1..]))
        .unwrap_or((specifier, "latest"));
    validate_package_name(name)?;
    if requirement.is_empty() {
        return Err(PackageError(
            "package version requirement cannot be empty".into(),
        ));
    }
    Ok((name.into(), requirement.into()))
}

fn validate_package_name(name: &str) -> Result<(), PackageError> {
    let scoped_parts = name
        .strip_prefix('@')
        .map(|value| value.split('/').collect::<Vec<_>>());
    let valid = !name.is_empty()
        && !name.contains('\\')
        && !name.contains("..")
        && ((!name.starts_with('@') && !name.contains('/'))
            || scoped_parts.as_ref().is_some_and(|parts| {
                parts.len() == 2 && parts.iter().all(|part| !part.is_empty())
            }));
    if valid {
        Ok(())
    } else {
        Err(PackageError(format!("invalid package name '{name}'")))
    }
}

fn manifest_dependencies(
    manifest: &serde_json::Value,
    section: &str,
) -> Result<BTreeMap<String, String>, PackageError> {
    let Some(value) = manifest.get(section) else {
        return Ok(BTreeMap::new());
    };
    serde_json::from_value(value.clone())
        .map_err(|error| PackageError(format!("package.json {section} is invalid: {error}")))
}

fn package_install_path(node_modules: &Path, name: &str) -> Result<PathBuf, PackageError> {
    validate_package_name(name)?;
    let mut path = node_modules.to_path_buf();
    for component in name.split('/') {
        path.push(component);
    }
    if !path.starts_with(node_modules) {
        return Err(PackageError("package path escapes node_modules".into()));
    }
    Ok(path)
}

fn parse_sha512_integrity(integrity: &str) -> Result<Vec<u8>, PackageError> {
    integrity
        .split_ascii_whitespace()
        .find_map(|token| token.strip_prefix("sha512-"))
        .ok_or_else(|| PackageError("package does not provide sha512 integrity".into()))
        .and_then(|encoded| {
            BASE64
                .decode(encoded)
                .map_err(|error| PackageError(format!("invalid package integrity: {error}")))
        })
}

fn verify_integrity(bytes: &[u8], expected: &[u8]) -> Result<(), PackageError> {
    let actual = Sha512::digest(bytes);
    if actual.as_slice() == expected {
        Ok(())
    } else {
        Err(PackageError("package integrity verification failed".into()))
    }
}

fn extract_archive(bytes: &[u8], destination: &Path) -> Result<(), PackageError> {
    if destination.exists() {
        fs::remove_dir_all(destination)?;
    }
    fs::create_dir_all(destination)?;
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let mut extracted_bytes = 0_u64;

    for (entry_index, entry) in archive.entries()?.enumerate() {
        if entry_index >= MAXIMUM_ARCHIVE_ENTRIES {
            return Err(PackageError("package archive exceeds entry limit".into()));
        }
        let mut entry = entry?;
        let archive_path = entry.path()?.into_owned();
        let mut components = archive_path.components();
        if components.next() != Some(Component::Normal(OsStr::new("package"))) {
            return Err(PackageError("tarball entry is outside package/".into()));
        }
        let mut relative = PathBuf::new();
        for component in components {
            match component {
                Component::Normal(value) => relative.push(value),
                _ => return Err(PackageError("unsafe tarball path".into())),
            }
        }
        if relative.as_os_str().is_empty() {
            continue;
        }
        extracted_bytes = extracted_bytes.saturating_add(entry.size());
        if extracted_bytes > MAXIMUM_EXTRACTED_BYTES {
            return Err(PackageError("extracted package exceeds byte limit".into()));
        }
        let output = destination.join(relative);
        if !output.starts_with(destination) {
            return Err(PackageError("tarball entry escapes destination".into()));
        }
        let entry_type = entry.header().entry_type();
        if !(entry_type.is_file() || entry_type.is_dir()) {
            return Err(PackageError(
                "tarball links and special files are unsupported".into(),
            ));
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(&output)?;
    }
    Ok(())
}

fn prune_store(cache_root: &Path) -> Result<(), PackageError> {
    let mut files = Vec::new();
    let mut total_bytes = 0_u64;
    if !cache_root.is_dir() {
        return Ok(());
    }
    for shard in fs::read_dir(cache_root)? {
        let shard = shard?;
        if !shard.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(shard.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() || entry.path().extension() != Some(OsStr::new("tgz"))
            {
                continue;
            }
            let metadata = entry.metadata()?;
            total_bytes = total_bytes.saturating_add(metadata.len());
            let modified = metadata
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH)
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            files.push((modified, metadata.len(), entry.path()));
        }
    }
    files.sort_by_key(|(modified, _, path)| (*modified, path.clone()));
    let mut file_count = files.len();
    for (_, size, path) in files {
        if file_count <= MAXIMUM_STORE_FILES && total_bytes <= MAXIMUM_STORE_BYTES {
            break;
        }
        fs::remove_file(path)?;
        file_count -= 1;
        total_bytes = total_bytes.saturating_sub(size);
    }
    Ok(())
}

fn run_lifecycle_scripts(
    package_root: &Path,
    scripts: &BTreeMap<String, String>,
) -> Result<(), PackageError> {
    for name in ["preinstall", "install", "postinstall"] {
        let Some(script) = scripts.get(name) else {
            continue;
        };
        let status = Command::new("cmd.exe")
            .args(["/d", "/s", "/c", script])
            .current_dir(package_root)
            .status()
            .map_err(|error| PackageError(format!("cannot start {name} script: {error}")))?;
        if !status.success() {
            return Err(PackageError(format!("{name} script exited with {status}")));
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scoped_and_unscoped_specifiers() {
        assert_eq!(
            parse_package_specifier("lodash").unwrap(),
            ("lodash".into(), "latest".into())
        );
        assert_eq!(
            parse_package_specifier("lodash@^4.0.0").unwrap(),
            ("lodash".into(), "^4.0.0".into())
        );
        assert_eq!(
            parse_package_specifier("@scope/name@~1.2.0").unwrap(),
            ("@scope/name".into(), "~1.2.0".into())
        );
        assert!(parse_package_specifier("../escape").is_err());
    }

    #[test]
    fn selects_highest_matching_version() {
        let metadata: Metadata = serde_json::from_value(serde_json::json!({
            "dist-tags": {"latest": "2.0.0"},
            "versions": {
                "1.0.0": package_version("1.0.0"),
                "1.4.0": package_version("1.4.0"),
                "2.0.0": package_version("2.0.0")
            }
        }))
        .unwrap();
        assert_eq!(
            select_version(&metadata, "^1.0.0").unwrap().version,
            "1.4.0"
        );
        assert_eq!(
            select_version(&metadata, "latest").unwrap().version,
            "2.0.0"
        );
    }

    #[test]
    fn verifies_sha512_integrity() {
        let bytes = b"package bytes";
        let integrity = format!("sha512-{}", BASE64.encode(Sha512::digest(bytes)));
        let expected = parse_sha512_integrity(&integrity).unwrap();
        assert!(verify_integrity(bytes, &expected).is_ok());
        assert!(verify_integrity(b"changed", &expected).is_err());
    }

    fn package_version(version: &str) -> serde_json::Value {
        serde_json::json!({
            "name": "example",
            "version": version,
            "dist": {
                "tarball": "https://example.invalid/package.tgz",
                "integrity": "sha512-AA=="
            }
        })
    }
}
