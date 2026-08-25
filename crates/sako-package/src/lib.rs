// SPDX-License-Identifier: BSD-3-Clause

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::read::GzDecoder;
use sako_process::spawn_native_with_bounded_output;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";
const SAKO_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAXIMUM_PACKAGES: usize = 10_000;
const MAXIMUM_METADATA_ENTRIES: usize = 1_024;
const MAXIMUM_METADATA_BYTES: u64 = 64 * 1024 * 1024;
const MAXIMUM_TARBALL_BYTES: u64 = 512 * 1024 * 1024;
const MAXIMUM_EXTRACTED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAXIMUM_ARCHIVE_ENTRIES: usize = 100_000;
const MAXIMUM_WORKSPACE_FILES: usize = 100_000;
const MAXIMUM_WORKSPACE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAXIMUM_STORE_FILES: usize = 50_000;
const MAXIMUM_STORE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MAXIMUM_SCRIPT_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Populates `node_modules/.bin` from every installed package's `bin` field.
///
/// Package scripts and `sako x` find executables the way npm does: by looking
/// up `node_modules/.bin` on `PATH`. Nothing created that directory before, so
/// a manifest script like `"dev": "vite"` had no `vite` to run even with the
/// package installed.
///
/// The shims re-enter Sako rather than Node. They prefer `$SAKO_EXECUTABLE`,
/// which the CLI sets to its own path before spawning, so a script gets the
/// exact binary the user invoked instead of whichever `sako` is on `PATH`.
fn link_binaries(node_modules: &Path) -> Result<(), PackageError> {
    if !node_modules.is_dir() {
        return Ok(());
    }
    let bin_dir = node_modules.join(".bin");
    let mut packages: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(node_modules)?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if name == ".bin" || !path.is_dir() {
            continue;
        }
        if name.starts_with('@') {
            // Scoped packages nest one level deeper: node_modules/@scope/name.
            for scoped in fs::read_dir(&path)?.flatten() {
                if scoped.path().is_dir() {
                    packages.push(scoped.path());
                }
            }
        } else {
            packages.push(path);
        }
    }

    for package in packages {
        let manifest_path = package.join("package.json");
        let Ok(source) = fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&source) else {
            continue;
        };
        for (command, relative) in package_binaries(&manifest) {
            // A package cannot claim a name that escapes .bin.
            if command.is_empty() || command.contains(['/', '\\']) || command.starts_with('.') {
                continue;
            }
            let target = package.join(&relative);
            if !target.is_file() {
                continue;
            }
            fs::create_dir_all(&bin_dir)?;
            write_binary_shim(&bin_dir, &command, &target)?;
        }
    }
    Ok(())
}

/// Normalizes the two shapes npm allows: `"bin": "cli.js"` names the command
/// after the package (scope stripped), `"bin": {"a": "a.js"}` names each one.
fn package_binaries(manifest: &serde_json::Value) -> Vec<(String, String)> {
    let Some(bin) = manifest.get("bin") else {
        return Vec::new();
    };
    if let Some(path) = bin.as_str() {
        let name = manifest
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let command = name.rsplit('/').next().unwrap_or(name);
        if command.is_empty() {
            return Vec::new();
        }
        return vec![(command.to_owned(), path.to_owned())];
    }
    bin.as_object()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(command, path)| {
                    path.as_str().map(|path| (command.clone(), path.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn write_binary_shim(bin_dir: &Path, command: &str, target: &Path) -> Result<(), PackageError> {
    // Relative so the tree stays movable, matching npm's shims.
    let relative = pathdiff_from(bin_dir, target);
    let posix_target = relative.replace('\\', "/");

    let posix = format!(
        "#!/bin/sh\n\
         basedir=$(dirname \"$(echo \"$0\" | sed -e 's,\\\\,/,g')\")\n\
         exec \"${{SAKO_EXECUTABLE:-sako}}\" \"$basedir/{posix_target}\" \"$@\"\n"
    );
    let posix_path = bin_dir.join(command);
    fs::write(&posix_path, posix)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&posix_path, fs::Permissions::from_mode(0o755))?;
    }

    #[cfg(windows)]
    {
        let windows_target = relative.replace('/', "\\");
        let cmd = format!(
            "@ECHO off\r\n\
             SETLOCAL\r\n\
             IF DEFINED SAKO_EXECUTABLE (SET \"_sako=%SAKO_EXECUTABLE%\") ELSE (SET \"_sako=sako\")\r\n\
             \"%_sako%\" \"%~dp0{windows_target}\" %*\r\n"
        );
        fs::write(bin_dir.join(format!("{command}.cmd")), cmd)?;
    }
    Ok(())
}

/// Expresses `target` relative to `base`. Both come from the same install tree,
/// so this only has to handle the shared-prefix case.
fn pathdiff_from(base: &Path, target: &Path) -> String {
    let base_parts: Vec<_> = base.components().collect();
    let target_parts: Vec<_> = target.components().collect();
    let shared = base_parts
        .iter()
        .zip(target_parts.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts: Vec<String> = vec!["..".into(); base_parts.len() - shared];
    parts.extend(
        target_parts[shared..]
            .iter()
            .map(|part| part.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

#[derive(Debug)]
struct RegistryConfig {
    registry: String,
    scoped_registries: BTreeMap<String, String>,
    auth_tokens: BTreeMap<String, String>,
    basic_auth: BTreeMap<String, String>,
    default_auth_token: Option<String>,
    default_basic_auth: Option<String>,
    auth_usernames: BTreeMap<String, String>,
    auth_passwords: BTreeMap<String, Vec<u8>>,
    default_username: Option<String>,
    default_password: Option<Vec<u8>>,
    proxy: Option<String>,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            registry: DEFAULT_REGISTRY.into(),
            scoped_registries: BTreeMap::new(),
            auth_tokens: BTreeMap::new(),
            basic_auth: BTreeMap::new(),
            default_auth_token: None,
            default_basic_auth: None,
            auth_usernames: BTreeMap::new(),
            auth_passwords: BTreeMap::new(),
            default_username: None,
            default_password: None,
            proxy: None,
        }
    }
}

fn read_npmrc(path: &Path, config: &mut RegistryConfig) -> Result<(), PackageError> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = expand_environment(value.trim().trim_matches(['"', '\'']));
        if key == "registry" {
            config.registry = value;
        } else if key == "https-proxy" || key == "proxy" {
            config.proxy = match value.to_ascii_lowercase().as_str() {
                "" | "false" | "null" => None,
                _ => Some(value),
            };
        } else if let Some(scope) = key.strip_suffix(":registry") {
            if scope.starts_with('@') {
                config.scoped_registries.insert(scope.into(), value);
            }
        } else if key == "_authToken" {
            config.default_auth_token = Some(value);
        } else if key == "_auth" {
            validate_basic_auth(&value)?;
            config.default_basic_auth = Some(value);
            config.default_username = None;
            config.default_password = None;
        } else if key == "username" {
            config.default_username = Some(value);
            update_default_user_password(config);
        } else if key == "_password" {
            config.default_password = Some(decode_password(&value)?);
            update_default_user_password(config);
        } else if let Some(prefix) = key.strip_suffix(":_authToken") {
            config.auth_tokens.insert(registry_auth_key(prefix), value);
        } else if let Some(prefix) = key.strip_suffix(":_auth") {
            validate_basic_auth(&value)?;
            let key = registry_auth_key(prefix);
            config.basic_auth.insert(key.clone(), value);
            config.auth_usernames.remove(&key);
            config.auth_passwords.remove(&key);
        } else if let Some(prefix) = key.strip_suffix(":username") {
            let key = registry_auth_key(prefix);
            config.auth_usernames.insert(key.clone(), value);
            update_scoped_user_password(config, &key);
        } else if let Some(prefix) = key.strip_suffix(":_password") {
            let key = registry_auth_key(prefix);
            config
                .auth_passwords
                .insert(key.clone(), decode_password(&value)?);
            update_scoped_user_password(config, &key);
        }
    }
    Ok(())
}

fn decode_password(value: &str) -> Result<Vec<u8>, PackageError> {
    BASE64
        .decode(value)
        .map_err(|_| PackageError("npm _password is not valid base64".into()))
}

fn encode_user_password(username: &str, password: &[u8]) -> String {
    let mut credentials = Vec::with_capacity(username.len() + password.len() + 1);
    credentials.extend_from_slice(username.as_bytes());
    credentials.push(b':');
    credentials.extend_from_slice(password);
    BASE64.encode(credentials)
}

fn update_default_user_password(config: &mut RegistryConfig) {
    if let (Some(username), Some(password)) = (&config.default_username, &config.default_password) {
        config.default_basic_auth = Some(encode_user_password(username, password));
    }
}

fn update_scoped_user_password(config: &mut RegistryConfig, key: &str) {
    if let (Some(username), Some(password)) = (
        config.auth_usernames.get(key),
        config.auth_passwords.get(key),
    ) {
        config
            .basic_auth
            .insert(key.to_owned(), encode_user_password(username, password));
    }
}

fn validate_basic_auth(value: &str) -> Result<(), PackageError> {
    let decoded = BASE64
        .decode(value)
        .map_err(|_| PackageError("npm basic authentication is not valid base64".into()))?;
    if !decoded.contains(&b':') {
        return Err(PackageError(
            "npm basic authentication must encode username:password".into(),
        ));
    }
    Ok(())
}

fn expand_environment(value: &str) -> String {
    let mut output = value.to_owned();
    while let Some(start) = output.find("${") {
        let Some(relative_end) = output[start + 2..].find('}') else {
            break;
        };
        let end = start + 2 + relative_end;
        let name = &output[start + 2..end];
        let replacement = env::var(name).unwrap_or_default();
        output.replace_range(start..=end, &replacement);
    }
    output
}

fn registry_auth_key(url: &str) -> String {
    url.trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))
        .or_else(|| url.trim().strip_prefix("//"))
        .unwrap_or(url.trim())
        .to_ascii_lowercase()
}

fn home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(unix)]
    {
        env::var_os("HOME").map(PathBuf::from)
    }
}

/// The base directory package downloads are cached under: `LOCALAPPDATA` on
/// Windows, matching the XDG base directory spec (`XDG_CACHE_HOME`, falling
/// back to `~/.cache`) on Unix.
fn cache_directory() -> Result<PathBuf, PackageError> {
    #[cfg(windows)]
    {
        env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| PackageError("LOCALAPPDATA is not set".into()))
    }
    #[cfg(unix)]
    {
        if let Some(xdg_cache) = env::var_os("XDG_CACHE_HOME") {
            return Ok(PathBuf::from(xdg_cache));
        }
        home_directory()
            .map(|home| home.join(".cache"))
            .ok_or_else(|| PackageError("neither XDG_CACHE_HOME nor HOME is set".into()))
    }
}

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

/// What the installer is doing right now, for a caller that wants to show it.
///
/// Modelled as events rather than a shared counter so the package manager
/// never learns whether anything is being drawn: it reports, and whoever
/// installed a reporter decides between a progress bar, a log line, or
/// nothing at all.
pub enum ProgressEvent<'a> {
    /// The lockfile named the whole graph up front. Not sent when the graph is
    /// being resolved from the registry instead, where the total is only known
    /// once the walk has finished.
    Planned { total: usize },
    /// Asking the registry which versions of `name` exist. This is the phase
    /// with nothing on disk to show for it, so it is worth surfacing.
    Resolving { name: &'a str },
    /// `name@version` is unpacked under node_modules. `downloaded` separates a
    /// tarball fetched from the registry from one the local store already had.
    Installed {
        name: &'a str,
        version: &'a str,
        downloaded: bool,
    },
    /// A problem that did not stop the install, usually an optional dependency
    /// that would not build. Routed through the reporter so it can be printed
    /// without tearing a half-drawn progress line.
    Warning { message: &'a str },
    /// Everything is on disk.
    Finished { installed: usize },
}

pub trait ProgressReporter {
    fn report(&self, event: ProgressEvent<'_>);
}

/// Holds the optional reporter. Exists only so `PackageManager` can keep its
/// derived `Debug`, which a bare `Box<dyn ProgressReporter>` would deny it.
#[derive(Default)]
struct Reporter(Option<Box<dyn ProgressReporter>>);

impl fmt::Debug for Reporter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.0 {
            Some(_) => "Reporter(installed)",
            None => "Reporter(none)",
        })
    }
}

#[derive(Debug)]
pub struct PackageManager {
    root: PathBuf,
    registry: String,
    scoped_registries: BTreeMap<String, String>,
    auth_tokens: BTreeMap<String, String>,
    basic_auth: BTreeMap<String, String>,
    agent: ureq::Agent,
    cache_root: PathBuf,
    metadata: HashMap<String, Metadata>,
    workspaces: BTreeMap<String, WorkspacePackage>,
    ignore_scripts: bool,
    installed: BTreeMap<String, LockedPackage>,
    active: HashSet<String>,
    reporter: Reporter,
}

#[derive(Clone, Debug, Default)]
pub struct PackageManagerOptions {
    pub ignore_scripts: bool,
    pub registry: Option<String>,
    pub auth_token: Option<String>,
    pub proxy: Option<String>,
}

impl PackageManager {
    pub fn new(root: impl Into<PathBuf>, ignore_scripts: bool) -> Result<Self, PackageError> {
        Self::new_with_options(
            root,
            PackageManagerOptions {
                ignore_scripts,
                ..PackageManagerOptions::default()
            },
        )
    }

    pub fn new_with_options(
        root: impl Into<PathBuf>,
        options: PackageManagerOptions,
    ) -> Result<Self, PackageError> {
        let root = root.into();
        let mut registry_config = RegistryConfig::default();
        if let Some(global_config) = env::var_os("NPM_CONFIG_GLOBALCONFIG") {
            read_npmrc(&PathBuf::from(global_config), &mut registry_config)?;
        }
        if let Some(user_config) = env::var_os("NPM_CONFIG_USERCONFIG") {
            read_npmrc(&PathBuf::from(user_config), &mut registry_config)?;
        } else if let Some(home) = home_directory() {
            read_npmrc(&home.join(".npmrc"), &mut registry_config)?;
        }
        read_npmrc(&root.join(".npmrc"), &mut registry_config)?;
        if let Ok(registry) = env::var("NPM_CONFIG_REGISTRY") {
            registry_config.registry = registry;
        }
        if let Ok(registry) = env::var("SAKO_NPM_REGISTRY") {
            registry_config.registry = registry;
        }
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "NPM_CONFIG_PROXY",
            "NPM_CONFIG_HTTPS_PROXY",
        ] {
            if let Ok(proxy) = env::var(name) {
                registry_config.proxy = Some(proxy);
            }
        }
        if let Ok(credentials) = env::var("NPM_CONFIG__AUTH") {
            validate_basic_auth(&credentials)?;
            registry_config.default_basic_auth = Some(credentials);
            registry_config.default_username = None;
            registry_config.default_password = None;
        }
        if let Ok(username) = env::var("NPM_CONFIG_USERNAME") {
            registry_config.default_username = Some(username);
            update_default_user_password(&mut registry_config);
        }
        if let Ok(password) = env::var("NPM_CONFIG__PASSWORD") {
            registry_config.default_password = Some(decode_password(&password)?);
            update_default_user_password(&mut registry_config);
        }
        for name in [
            "NPM_CONFIG__AUTH_TOKEN",
            "NODE_AUTH_TOKEN",
            "NPM_TOKEN",
            "SAKO_NPM_TOKEN",
        ] {
            if let Ok(token) = env::var(name) {
                registry_config.default_auth_token = Some(token);
            }
        }
        if let Some(registry) = options.registry {
            registry_config.registry = registry;
        }
        if let Some(token) = options.auth_token {
            registry_config.default_auth_token = Some(token);
        }
        if let Some(proxy) = options.proxy {
            registry_config.proxy = Some(proxy);
        }
        let default_auth_key = registry_auth_key(&registry_config.registry);
        if let Some(credentials) = registry_config.default_basic_auth.take() {
            registry_config
                .basic_auth
                .insert(default_auth_key.clone(), credentials);
        }
        if let Some(token) = registry_config.default_auth_token.take() {
            registry_config.auth_tokens.insert(default_auth_key, token);
        }
        let cache_root = cache_directory()?.join("Sako").join("Store").join("sha512");
        let mut agent = ureq::AgentBuilder::new();
        if let Some(proxy) = registry_config.proxy {
            agent = agent.proxy(
                ureq::Proxy::new(proxy)
                    .map_err(|error| PackageError(format!("invalid npm proxy: {error}")))?,
            );
        }
        Ok(Self {
            root,
            registry: registry_config.registry.trim_end_matches('/').into(),
            scoped_registries: registry_config.scoped_registries,
            auth_tokens: registry_config.auth_tokens,
            basic_auth: registry_config.basic_auth,
            agent: agent.build(),
            cache_root,
            metadata: HashMap::new(),
            workspaces: BTreeMap::new(),
            ignore_scripts: options.ignore_scripts,
            installed: BTreeMap::new(),
            active: HashSet::new(),
            reporter: Reporter::default(),
        })
    }

    /// Attaches a progress reporter. Installs are otherwise silent until they
    /// either finish or fail, which on a cold cache is a long time to look
    /// like nothing is happening.
    pub fn set_reporter(&mut self, reporter: Box<dyn ProgressReporter>) {
        self.reporter = Reporter(Some(reporter));
    }

    fn report(&self, event: ProgressEvent<'_>) {
        if let Some(reporter) = &self.reporter.0 {
            reporter.report(event);
        }
    }

    /// Reports something that did not stop the install. Falls back to stderr
    /// when no reporter is attached, which is what these messages did before
    /// there was one.
    fn warn(&self, message: &str) {
        match &self.reporter.0 {
            Some(reporter) => reporter.report(ProgressEvent::Warning { message }),
            None => eprintln!("sako: {message}"),
        }
    }

    pub fn install(&mut self) -> Result<(), PackageError> {
        self.installed.clear();
        self.active.clear();
        let manifest = self.read_manifest()?;
        let root_engines = manifest_engines(&manifest)?;
        validate_sako_engine("root package", &root_engines)?;
        self.workspaces = discover_workspaces(&self.root, &manifest)?;
        let mut dependencies = manifest_dependencies(&manifest, "dependencies")?;
        for (name, requirement) in manifest_dependencies(&manifest, "devDependencies")? {
            dependencies.entry(name).or_insert(requirement);
        }
        for name in self.workspaces.keys() {
            dependencies
                .entry(name.clone())
                .or_insert_with(|| "workspace:*".into());
        }
        let optional_dependencies = manifest_dependencies(&manifest, "optionalDependencies")?;

        if self.install_from_lock(&dependencies, &optional_dependencies)? {
            link_binaries(&self.root.join("node_modules"))?;
            self.report(ProgressEvent::Finished {
                installed: self.installed.len(),
            });
            return Ok(());
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
        for (name, requirement) in optional_dependencies {
            if let Err(error) = self.install_dependency(
                &name,
                &requirement,
                &node_modules,
                &format!("node_modules/{name}"),
            ) {
                self.warn(&format!("skipping optional dependency {name}: {error}"));
            }
        }
        validate_peer_dependencies(&self.installed)?;
        link_binaries(&node_modules)?;
        self.write_lockfile()?;
        self.report(ProgressEvent::Finished {
            installed: self.installed.len(),
        });
        Ok(())
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
        if let Some(workspace) = self.workspaces.get(name).cloned() {
            if workspace_requirement_matches(&workspace.version, requirement) {
                return self.install_workspace(workspace, parent_node_modules, lock_path);
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
        let package = self.resolve(name, requirement)?;
        validate_sako_engine(
            &format!("{}@{}", package.name, package.version),
            &package.engines,
        )?;
        let identity = format!("{}@{}", package.name, package.version);
        if !self.active.insert(identity.clone()) {
            return Ok(());
        }

        let destination = package_install_path(parent_node_modules, name)?;
        let (archive, downloaded) = self.fetch_archive(&package.dist)?;
        extract_archive(&archive, &destination)?;
        self.report(ProgressEvent::Installed {
            name: &package.name,
            version: &package.version,
            downloaded,
        });

        self.installed.insert(
            lock_path.into(),
            LockedPackage {
                name: package.name.clone(),
                version: package.version.clone(),
                resolved: package.dist.tarball.clone(),
                integrity: package.dist.integrity.clone(),
                dependencies: package.dependencies.clone(),
                optional_dependencies: package.optional_dependencies.clone(),
                peer_dependencies: package.peer_dependencies.clone(),
                optional_peers: package
                    .peer_dependencies_meta
                    .iter()
                    .filter(|(_, metadata)| metadata.optional)
                    .map(|(name, _)| name.clone())
                    .collect(),
                scripts: package.scripts.clone(),
                engines: package.engines.clone(),
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
        for (dependency, child_requirement) in package.optional_dependencies.clone() {
            let child_lock_path = format!("{lock_path}/node_modules/{dependency}");
            if let Err(error) = self.install_dependency(
                &dependency,
                &child_requirement,
                &child_node_modules,
                &child_lock_path,
            ) {
                self.warn(&format!("skipping optional dependency {dependency}: {error}"));
            }
        }
        if !self.ignore_scripts {
            run_lifecycle_scripts(&destination, &package.scripts)?;
        }
        self.active.remove(&identity);
        Ok(())
    }

    fn install_workspace(
        &mut self,
        workspace: WorkspacePackage,
        parent_node_modules: &Path,
        lock_path: &str,
    ) -> Result<(), PackageError> {
        if self.installed.len() >= MAXIMUM_PACKAGES {
            return Err(PackageError("package graph capacity exceeded".into()));
        }
        let identity = format!("{}@{}", workspace.name, workspace.version);
        if !self.active.insert(identity.clone()) {
            return Ok(());
        }
        let destination = package_install_path(parent_node_modules, &workspace.name)?;
        copy_workspace(&workspace.path, &destination)?;
        self.report(ProgressEvent::Installed {
            name: &workspace.name,
            version: &workspace.version,
            downloaded: false,
        });
        self.installed.insert(
            lock_path.into(),
            LockedPackage {
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
        );
        let child_node_modules = destination.join("node_modules");
        for (dependency, requirement) in workspace.dependencies.clone() {
            self.install_dependency(
                &dependency,
                &requirement,
                &child_node_modules,
                &format!("{lock_path}/node_modules/{dependency}"),
            )?;
        }
        for (dependency, requirement) in workspace.optional_dependencies.clone() {
            if let Err(error) = self.install_dependency(
                &dependency,
                &requirement,
                &child_node_modules,
                &format!("{lock_path}/node_modules/{dependency}"),
            ) {
                self.warn(&format!("skipping optional dependency {dependency}: {error}"));
            }
        }
        if !self.ignore_scripts {
            run_lifecycle_scripts(&destination, &workspace.scripts)?;
        }
        self.active.remove(&identity);
        Ok(())
    }

    fn install_from_lock(
        &mut self,
        root_dependencies: &BTreeMap<String, String>,
        root_optional_dependencies: &BTreeMap<String, String>,
    ) -> Result<bool, PackageError> {
        let path = self.root.join("sako.lock");
        if !path.is_file() {
            return Ok(false);
        }
        let source = fs::read_to_string(&path)?;
        let lockfile: Lockfile = serde_json::from_str(&source)
            .map_err(|error| PackageError(format!("invalid {}: {error}", path.display())))?;
        if lockfile.lockfile_version != 2
            || !lock_matches_manifest(&lockfile, root_dependencies, root_optional_dependencies)
        {
            return Ok(false);
        }

        self.report(ProgressEvent::Planned {
            total: lockfile.packages.len(),
        });
        self.installed = lockfile.packages.clone();
        self.active.clear();
        let node_modules = self.root.join("node_modules");
        fs::create_dir_all(&node_modules)?;
        for name in root_dependencies.keys() {
            self.install_locked_dependency(
                &lockfile,
                &format!("node_modules/{name}"),
                &node_modules,
            )?;
        }
        for name in root_optional_dependencies.keys() {
            let lock_path = format!("node_modules/{name}");
            if lockfile.packages.contains_key(&lock_path)
                && let Err(error) =
                    self.install_locked_dependency(&lockfile, &lock_path, &node_modules)
            {
                self.warn(&format!("skipping optional dependency {name}: {error}"));
            }
        }
        validate_peer_dependencies(&self.installed)?;
        Ok(true)
    }

    fn install_locked_dependency(
        &mut self,
        lockfile: &Lockfile,
        lock_path: &str,
        parent_node_modules: &Path,
    ) -> Result<(), PackageError> {
        let package = lockfile.packages.get(lock_path).cloned().ok_or_else(|| {
            PackageError(format!("lockfile is missing dependency entry {lock_path}"))
        })?;
        validate_sako_engine(
            &format!("{}@{}", package.name, package.version),
            &package.engines,
        )?;
        let identity = format!("{}@{}", package.name, package.version);
        if !self.active.insert(identity.clone()) {
            return Ok(());
        }

        let destination = package_install_path(parent_node_modules, &package.name)?;
        if package.resolved.starts_with("workspace:") {
            let workspace = self.workspaces.get(&package.name).ok_or_else(|| {
                PackageError(format!("locked workspace {} was not found", package.name))
            })?;
            if workspace.version != package.version {
                return Err(PackageError(format!(
                    "locked workspace {}@{} does not match local version {}",
                    package.name, package.version, workspace.version
                )));
            }
            copy_workspace(&workspace.path, &destination)?;
            self.report(ProgressEvent::Installed {
                name: &package.name,
                version: &package.version,
                downloaded: false,
            });
        } else {
            let (archive, downloaded) = self.fetch_archive(&Distribution {
                tarball: package.resolved.clone(),
                integrity: package.integrity.clone(),
            })?;
            extract_archive(&archive, &destination)?;
            self.report(ProgressEvent::Installed {
                name: &package.name,
                version: &package.version,
                downloaded,
            });
        }
        let child_node_modules = destination.join("node_modules");
        for (dependency, requirement) in &package.dependencies {
            let child_path = format!("{lock_path}/node_modules/{dependency}");
            if lockfile.packages.contains_key(&child_path) {
                self.install_locked_dependency(lockfile, &child_path, &child_node_modules)?;
            } else if !lock_has_ancestor_dependency(lockfile, lock_path, dependency, requirement) {
                return Err(PackageError(format!(
                    "lockfile is missing transitive dependency {dependency} for {lock_path}"
                )));
            }
        }
        for dependency in package.optional_dependencies.keys() {
            let child_path = format!("{lock_path}/node_modules/{dependency}");
            if lockfile.packages.contains_key(&child_path)
                && let Err(error) =
                    self.install_locked_dependency(lockfile, &child_path, &child_node_modules)
            {
                self.warn(&format!("skipping optional dependency {dependency}: {error}"));
            }
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
            self.report(ProgressEvent::Resolving { name });
            let encoded = name.replace('/', "%2f");
            let registry = self.registry_for(name);
            let url = format!("{}/{encoded}", registry.trim_end_matches('/'));
            let response = self
                .request(&url)
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

    /// Returns the tarball bytes and whether they came off the network, which
    /// is the difference between a warm and a cold store as far as anything
    /// watching the install is concerned.
    fn fetch_archive(
        &self,
        distribution: &Distribution,
    ) -> Result<(Vec<u8>, bool), PackageError> {
        let expected = parse_sha512_integrity(&distribution.integrity)?;
        let digest_hex = hex(&expected);
        let cache_path = self
            .cache_root
            .join(&digest_hex[..2])
            .join(format!("{digest_hex}.tgz"));
        if cache_path.is_file() {
            let bytes = fs::read(&cache_path)?;
            verify_integrity(&bytes, &expected)?;
            return Ok((bytes, false));
        }

        let response = self
            .request(&distribution.tarball)
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
        Ok((bytes, true))
    }

    fn registry_for(&self, name: &str) -> &str {
        if let Some(scope) = name
            .strip_prefix('@')
            .and_then(|name| name.split('/').next())
        {
            let scope = format!("@{scope}");
            if let Some(registry) = self.scoped_registries.get(&scope) {
                return registry;
            }
        }
        &self.registry
    }

    fn request(&self, url: &str) -> ureq::Request {
        let mut request = self.agent.get(url);
        let key = registry_auth_key(url);
        let bearer = self
            .auth_tokens
            .iter()
            .filter(|(prefix, _)| key.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len());
        let basic = self
            .basic_auth
            .iter()
            .filter(|(prefix, _)| key.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len());
        if let Some((_, token)) = bearer.filter(|(prefix, _)| {
            basic.is_none_or(|(basic_prefix, _)| prefix.len() >= basic_prefix.len())
        }) {
            request = request.set("Authorization", &format!("Bearer {token}"));
        } else if let Some((_, credentials)) = basic {
            request = request.set("Authorization", &format!("Basic {credentials}"));
        }
        request
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
            lockfile_version: 2,
            packages: self.installed.clone(),
        };
        let mut source = serde_json::to_string_pretty(&lockfile)?;
        source.push('\n');
        fs::write(self.root.join("sako.lock"), source)?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct WorkspacePackage {
    name: String,
    version: String,
    path: PathBuf,
    relative_path: String,
    dependencies: BTreeMap<String, String>,
    optional_dependencies: BTreeMap<String, String>,
    peer_dependencies: BTreeMap<String, String>,
    optional_peers: Vec<String>,
    scripts: BTreeMap<String, String>,
    engines: BTreeMap<String, String>,
}

fn discover_workspaces(
    root: &Path,
    manifest: &serde_json::Value,
) -> Result<BTreeMap<String, WorkspacePackage>, PackageError> {
    let Some(workspaces) = manifest.get("workspaces") else {
        return Ok(BTreeMap::new());
    };
    let patterns = if let Some(patterns) = workspaces.as_array() {
        patterns
    } else if let Some(patterns) = workspaces
        .get("packages")
        .and_then(|value| value.as_array())
    {
        patterns
    } else {
        return Err(PackageError(
            "package.json workspaces must be an array or contain a packages array".into(),
        ));
    };
    let canonical_root = fs::canonicalize(root)?;
    let mut packages = BTreeMap::new();
    for pattern in patterns {
        let pattern = pattern
            .as_str()
            .ok_or_else(|| PackageError("workspace patterns must be strings".into()))?;
        for path in expand_workspace_pattern(&canonical_root, pattern)? {
            if packages.len() >= MAXIMUM_PACKAGES {
                return Err(PackageError("workspace count exceeds package limit".into()));
            }
            let manifest_path = path.join("package.json");
            let source = fs::read_to_string(&manifest_path).map_err(|error| {
                PackageError(format!("cannot read {}: {error}", manifest_path.display()))
            })?;
            let manifest: serde_json::Value = serde_json::from_str(&source).map_err(|error| {
                PackageError(format!("invalid {}: {error}", manifest_path.display()))
            })?;
            let name = manifest
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    PackageError(format!("workspace {} has no package name", path.display()))
                })?
                .to_owned();
            validate_package_name(&name)?;
            let version = manifest
                .get("version")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("0.0.0")
                .to_owned();
            Version::parse(&version).map_err(|error| {
                PackageError(format!("workspace {name} has invalid version: {error}"))
            })?;
            let peer_dependencies = manifest_dependencies(&manifest, "peerDependencies")?;
            let optional_peers = manifest
                .get("peerDependenciesMeta")
                .and_then(serde_json::Value::as_object)
                .map(|metadata| {
                    metadata
                        .iter()
                        .filter(|(_, value)| {
                            value.get("optional").and_then(serde_json::Value::as_bool) == Some(true)
                        })
                        .map(|(name, _)| name.clone())
                        .collect()
                })
                .unwrap_or_default();
            let relative_path = path
                .strip_prefix(&canonical_root)
                .map_err(|_| PackageError("workspace path escapes project root".into()))?
                .to_string_lossy()
                .replace('\\', "/");
            let workspace = WorkspacePackage {
                name: name.clone(),
                version,
                path,
                relative_path,
                dependencies: manifest_dependencies(&manifest, "dependencies")?,
                optional_dependencies: manifest_dependencies(&manifest, "optionalDependencies")?,
                peer_dependencies,
                optional_peers,
                scripts: manifest_dependencies(&manifest, "scripts")?,
                engines: manifest_engines(&manifest)?,
            };
            validate_sako_engine(
                &format!("workspace {name}@{}", workspace.version),
                &workspace.engines,
            )?;
            if packages.insert(name.clone(), workspace).is_some() {
                return Err(PackageError(format!(
                    "duplicate workspace package name {name}"
                )));
            }
        }
    }
    Ok(packages)
}

fn expand_workspace_pattern(root: &Path, pattern: &str) -> Result<Vec<PathBuf>, PackageError> {
    if pattern.contains("..") || Path::new(pattern).is_absolute() {
        return Err(PackageError(format!("unsafe workspace pattern {pattern}")));
    }
    let wildcard_count = pattern.bytes().filter(|byte| *byte == b'*').count();
    if wildcard_count == 0 {
        let path = fs::canonicalize(root.join(pattern))?;
        return Ok(if path.starts_with(root) && path.is_dir() {
            vec![path]
        } else {
            Vec::new()
        });
    }
    if wildcard_count != 1 {
        return Err(PackageError(format!(
            "workspace pattern supports one '*' segment: {pattern}"
        )));
    }
    let path = Path::new(pattern);
    let file_pattern = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| PackageError(format!("invalid workspace pattern {pattern}")))?;
    let (prefix, suffix) = file_pattern
        .split_once('*')
        .ok_or_else(|| PackageError(format!("invalid workspace pattern {pattern}")))?;
    let parent = root.join(path.parent().unwrap_or(Path::new("")));
    let mut matches = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() && name.starts_with(prefix) && name.ends_with(suffix) {
            let candidate = fs::canonicalize(entry.path())?;
            if candidate.starts_with(root) {
                matches.push(candidate);
            }
        }
    }
    matches.sort();
    Ok(matches)
}

fn workspace_requirement_matches(version: &str, requirement: &str) -> bool {
    let Some(workspace_requirement) = requirement.strip_prefix("workspace:") else {
        return requirement_matches(version, requirement);
    };
    match workspace_requirement {
        "" | "*" | "^" | "~" => true,
        requirement => requirement_matches(version, requirement),
    }
}

fn copy_workspace(source: &Path, destination: &Path) -> Result<(), PackageError> {
    if destination.exists() {
        fs::remove_dir_all(destination)?;
    }
    let mut files = 0_usize;
    let mut bytes = 0_u64;
    copy_workspace_directory(source, destination, &mut files, &mut bytes)
}

fn copy_workspace_directory(
    source: &Path,
    destination: &Path,
    files: &mut usize,
    bytes: &mut u64,
) -> Result<(), PackageError> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_str(), Some("node_modules" | "target" | ".git")) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(PackageError(format!(
                "workspace links are unsupported: {}",
                entry.path().display()
            )));
        }
        let output = destination.join(&name);
        if file_type.is_dir() {
            copy_workspace_directory(&entry.path(), &output, files, bytes)?;
        } else if file_type.is_file() {
            *files += 1;
            *bytes = bytes.saturating_add(entry.metadata()?.len());
            if *files > MAXIMUM_WORKSPACE_FILES || *bytes > MAXIMUM_WORKSPACE_BYTES {
                return Err(PackageError("workspace exceeds copy limits".into()));
            }
            fs::copy(entry.path(), output)?;
        }
    }
    Ok(())
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
    #[serde(rename = "optionalDependencies", default)]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(rename = "peerDependencies", default)]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(rename = "peerDependenciesMeta", default)]
    peer_dependencies_meta: BTreeMap<String, PeerDependencyMetadata>,
    #[serde(default)]
    scripts: BTreeMap<String, String>,
    #[serde(default)]
    engines: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct PeerDependencyMetadata {
    #[serde(default)]
    optional: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct Distribution {
    tarball: String,
    integrity: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Lockfile {
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u32,
    packages: BTreeMap<String, LockedPackage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LockedPackage {
    name: String,
    version: String,
    resolved: String,
    integrity: String,
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_peers: Vec<String>,
    #[serde(default)]
    scripts: BTreeMap<String, String>,
    #[serde(default)]
    engines: BTreeMap<String, String>,
}

fn manifest_engines(
    manifest: &serde_json::Value,
) -> Result<BTreeMap<String, String>, PackageError> {
    let Some(engines) = manifest.get("engines") else {
        return Ok(BTreeMap::new());
    };
    let engines = engines
        .as_object()
        .ok_or_else(|| PackageError("package.json engines must be an object".into()))?;
    engines
        .iter()
        .map(|(name, requirement)| {
            requirement
                .as_str()
                .map(|requirement| (name.clone(), requirement.to_owned()))
                .ok_or_else(|| PackageError(format!("engine {name} must be a string")))
        })
        .collect()
}

fn validate_sako_engine(
    package: &str,
    engines: &BTreeMap<String, String>,
) -> Result<(), PackageError> {
    let Some(requirement) = engines.get("sako") else {
        return Ok(());
    };
    let requirements = npm_version_requirements(requirement)?;
    let version = Version::parse(SAKO_VERSION)
        .map_err(|error| PackageError(format!("invalid Sako version: {error}")))?;
    if requirements
        .iter()
        .any(|requirement| requirement.matches(&version))
    {
        Ok(())
    } else {
        Err(PackageError(format!(
            "{package} requires Sako {requirement}, current version is {SAKO_VERSION}"
        )))
    }
}

fn requirement_matches(version: &str, requirement: &str) -> bool {
    if requirement == "latest" || requirement.is_empty() {
        return true;
    }
    if requirement == version {
        return true;
    }
    npm_version_requirements(requirement)
        .ok()
        .zip(Version::parse(version).ok())
        .is_some_and(|(requirements, version)| {
            requirements
                .iter()
                .any(|requirement| requirement.matches(&version))
        })
}

fn npm_version_requirements(requirement: &str) -> Result<Vec<VersionReq>, PackageError> {
    let mut parsed = Vec::new();
    for alternative in requirement.split("||") {
        let alternative = alternative.trim();
        if alternative.is_empty() {
            return Err(PackageError(format!(
                "unsupported semver requirement '{requirement}'"
            )));
        }
        let normalized = normalize_npm_range(alternative)?;
        parsed.push(VersionReq::parse(&normalized).map_err(|error| {
            PackageError(format!(
                "unsupported semver requirement '{requirement}': {error}"
            ))
        })?);
    }
    Ok(parsed)
}

fn normalize_npm_range(requirement: &str) -> Result<String, PackageError> {
    if let Some((minimum, maximum)) = requirement.split_once(" - ") {
        let minimum = Version::parse(minimum.trim())
            .map_err(|error| PackageError(format!("invalid hyphen range lower bound: {error}")))?;
        let maximum = Version::parse(maximum.trim())
            .map_err(|error| PackageError(format!("invalid hyphen range upper bound: {error}")))?;
        return Ok(format!(">={minimum}, <={maximum}"));
    }

    let raw = requirement.split_ascii_whitespace().collect::<Vec<_>>();
    let mut comparators = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        let token = raw[index].trim_matches(',');
        if matches!(token, ">" | ">=" | "<" | "<=" | "=" | "~" | "^") {
            let Some(version) = raw.get(index + 1) else {
                return Err(PackageError(format!(
                    "semver comparator '{token}' has no version"
                )));
            };
            comparators.push(format!("{token}{}", version.trim_matches(',')));
            index += 2;
            continue;
        }
        comparators.push(normalize_npm_comparator(token));
        index += 1;
    }
    Ok(comparators.join(", "))
}

fn normalize_npm_comparator(comparator: &str) -> String {
    let comparator = comparator.replace(['x', 'X'], "*");
    if comparator == "*"
        || comparator.starts_with(['>', '<', '=', '~', '^'])
        || comparator.contains('*')
    {
        return comparator;
    }
    if Version::parse(&comparator).is_ok() {
        return format!("={comparator}");
    }
    match comparator.bytes().filter(|byte| *byte == b'.').count() {
        0 if comparator.bytes().all(|byte| byte.is_ascii_digit()) => {
            format!("^{comparator}")
        }
        1 if comparator
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.') =>
        {
            format!("~{comparator}")
        }
        _ => comparator,
    }
}

fn lock_matches_manifest(
    lockfile: &Lockfile,
    root_dependencies: &BTreeMap<String, String>,
    root_optional_dependencies: &BTreeMap<String, String>,
) -> bool {
    let required_match = root_dependencies.iter().all(|(name, requirement)| {
        lockfile
            .packages
            .get(&format!("node_modules/{name}"))
            .is_some_and(|package| {
                package.name == *name && locked_requirement_matches(package, requirement)
            })
    });
    required_match
        && root_optional_dependencies
            .iter()
            .all(|(name, requirement)| {
                lockfile
                    .packages
                    .get(&format!("node_modules/{name}"))
                    .is_none_or(|package| {
                        package.name == *name && locked_requirement_matches(package, requirement)
                    })
            })
}

fn locked_requirement_matches(package: &LockedPackage, requirement: &str) -> bool {
    if package.resolved.starts_with("workspace:") {
        workspace_requirement_matches(&package.version, requirement)
    } else {
        requirement_matches(&package.version, requirement)
    }
}

fn validate_peer_dependencies(
    packages: &BTreeMap<String, LockedPackage>,
) -> Result<(), PackageError> {
    for (lock_path, package) in packages {
        for (peer, requirement) in &package.peer_dependencies {
            let resolved = find_ancestor_dependency(packages, lock_path, peer);
            if resolved.is_none() && package.optional_peers.contains(peer) {
                continue;
            }
            let Some(resolved) = resolved else {
                return Err(PackageError(format!(
                    "{}@{} requires peer {peer}@{requirement}",
                    package.name, package.version
                )));
            };
            if !requirement_matches(&resolved.version, requirement) {
                return Err(PackageError(format!(
                    "{}@{} requires peer {peer}@{requirement}, but {} is installed",
                    package.name, package.version, resolved.version
                )));
            }
        }
    }
    Ok(())
}

fn find_ancestor_dependency<'a>(
    packages: &'a BTreeMap<String, LockedPackage>,
    lock_path: &str,
    dependency: &str,
) -> Option<&'a LockedPackage> {
    let direct = format!("{lock_path}/node_modules/{dependency}");
    if let Some(package) = packages.get(&direct) {
        return Some(package);
    }
    let mut ancestor = lock_path;
    while let Some(index) = ancestor.rfind("/node_modules/") {
        let candidate = format!("{}/node_modules/{dependency}", &ancestor[..index]);
        if let Some(package) = packages.get(&candidate) {
            return Some(package);
        }
        ancestor = &ancestor[..index];
    }
    packages.get(&format!("node_modules/{dependency}"))
}

fn lock_has_ancestor_dependency(
    lockfile: &Lockfile,
    lock_path: &str,
    dependency: &str,
    requirement: &str,
) -> bool {
    let mut ancestor = lock_path;
    while let Some(index) = ancestor.rfind("/node_modules/") {
        let candidate = format!("{}/node_modules/{dependency}", &ancestor[..index]);
        if lockfile.packages.get(&candidate).is_some_and(|package| {
            package.name == dependency && requirement_matches(&package.version, requirement)
        }) {
            return true;
        }
        ancestor = &ancestor[..index];
    }
    lockfile
        .packages
        .get(&format!("node_modules/{dependency}"))
        .is_some_and(|package| {
            package.name == dependency && requirement_matches(&package.version, requirement)
        })
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
    let parsed = npm_version_requirements(requirement)?;
    metadata
        .versions
        .iter()
        .filter_map(|(version, package)| {
            Version::parse(version)
                .ok()
                .map(|version| (version, package))
        })
        .filter(|(version, _)| {
            parsed
                .iter()
                .any(|requirement| requirement.matches(version))
        })
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
        // npm strips one leading component whatever it is called, and that is
        // the behaviour packages are published against. Most use `package/`,
        // but plenty do not -- GitHub-generated tarballs use
        // `<repo>-<sha>/`, and some publishers ship their own name -- so
        // demanding `package/` rejected perfectly ordinary dependencies.
        // Safety does not depend on the name: every remaining component is
        // checked below, and the result is confined to `destination`.
        match components.next() {
            Some(Component::Normal(_)) => {}
            // Anything else is absolute, a parent escape, or a prefix.
            Some(_) => {
                return Err(PackageError(format!(
                    "unsafe tarball path: {}",
                    archive_path.display()
                )));
            }
            None => continue,
        }
        let mut relative = PathBuf::new();
        for component in components {
            match component {
                Component::Normal(value) => relative.push(value),
                _ => {
                    return Err(PackageError(format!(
                        "unsafe tarball path: {}",
                        archive_path.display()
                    )));
                }
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
        // PAX and GNU long-name records are metadata the tar reader has
        // already applied to the following entry; they are not content.
        if entry_type.is_pax_global_extensions()
            || entry_type.is_pax_local_extensions()
            || entry_type.is_gnu_longname()
            || entry_type.is_gnu_longlink()
        {
            continue;
        }
        if !(entry_type.is_file() || entry_type.is_dir()) {
            return Err(PackageError(format!(
                "tarball links and special files are unsupported: {}",
                archive_path.display()
            )));
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
        #[cfg(windows)]
        let (shell, arguments) = (
            "cmd.exe",
            vec![
                "/d".to_owned(),
                "/s".to_owned(),
                "/c".to_owned(),
                script.to_owned(),
            ],
        );
        #[cfg(unix)]
        let (shell, arguments) = ("/bin/sh", vec!["-c".to_owned(), script.to_owned()]);
        let output = spawn_native_with_bounded_output(
            shell,
            &arguments,
            Some(package_root),
            MAXIMUM_SCRIPT_OUTPUT_BYTES,
        )
        .map_err(|error| PackageError(format!("cannot run {name} script: {error}")))?;
        io::stdout().write_all(&output.stdout)?;
        io::stderr().write_all(&output.stderr)?;
        if !output.status.success() {
            return Err(PackageError(format!(
                "{name} script exited with {}",
                output.status
            )));
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
    use flate2::Compression;
    use flate2::write::GzEncoder;

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
    fn supports_npm_comparator_or_hyphen_and_x_ranges() {
        let matches = |version, requirement| requirement_matches(version, requirement);
        assert!(matches("2.4.0", ">= 2.1.2 < 3.0.0"));
        assert!(!matches("3.0.0", ">= 2.1.2 < 3.0.0"));
        assert!(matches("3.2.1", "^1.0.0 || ^3.0.0"));
        assert!(matches("1.4.5", "1.4.x"));
        assert!(!matches("1.5.0", "1.4.x"));
        assert!(matches("1.2.3", "1.2.3 - 2.0.0"));
        assert!(matches("2.0.0", "1.2.3 - 2.0.0"));
        assert!(!matches("2.0.1", "1.2.3 - 2.0.0"));
        assert!(matches("1.2.9", "1.2"));
        assert!(!matches("1.3.0", "1.2"));
        assert!(matches("1.2.3", "1.2.3"));
        assert!(!matches("1.2.4", "1.2.3"));
    }

    #[test]
    fn verifies_sha512_integrity() {
        let bytes = b"package bytes";
        let integrity = format!("sha512-{}", BASE64.encode(Sha512::digest(bytes)));
        let expected = parse_sha512_integrity(&integrity).unwrap();
        assert!(verify_integrity(bytes, &expected).is_ok());
        assert!(verify_integrity(b"changed", &expected).is_err());
    }

    #[test]
    fn replays_a_compatible_lockfile_without_registry_metadata() {
        let root = env::temp_dir().join(format!("sako-lock-replay-{}", std::process::id()));
        let cache_root = root.join("cache");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"dependencies":{"fixture":"^1.0.0"}}"#,
        )
        .unwrap();

        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let source = b"module.exports = 42;\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("package/index.js").unwrap();
        header.set_size(source.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append(&header, &source[..]).unwrap();
        let bytes = archive.into_inner().unwrap().finish().unwrap();
        let digest = Sha512::digest(&bytes).to_vec();
        let digest_hex = hex(&digest);
        let cache_path = cache_root
            .join(&digest_hex[..2])
            .join(format!("{digest_hex}.tgz"));
        fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        fs::write(cache_path, &bytes).unwrap();

        let lockfile = Lockfile {
            lockfile_version: 2,
            packages: BTreeMap::from([(
                "node_modules/fixture".into(),
                LockedPackage {
                    name: "fixture".into(),
                    version: "1.2.3".into(),
                    resolved: "https://invalid.example/fixture.tgz".into(),
                    integrity: format!("sha512-{}", BASE64.encode(&digest)),
                    dependencies: BTreeMap::new(),
                    optional_dependencies: BTreeMap::new(),
                    peer_dependencies: BTreeMap::new(),
                    optional_peers: Vec::new(),
                    scripts: BTreeMap::new(),
                    engines: BTreeMap::new(),
                },
            )]),
        };
        fs::write(
            root.join("sako.lock"),
            serde_json::to_string_pretty(&lockfile).unwrap(),
        )
        .unwrap();

        let mut manager = PackageManager {
            root: root.clone(),
            registry: "https://invalid.example".into(),
            scoped_registries: BTreeMap::new(),
            auth_tokens: BTreeMap::new(),
            basic_auth: BTreeMap::new(),
            agent: ureq::AgentBuilder::new().build(),
            cache_root,
            metadata: HashMap::new(),
            workspaces: BTreeMap::new(),
            ignore_scripts: true,
            installed: BTreeMap::new(),
            active: HashSet::new(),
            reporter: Reporter::default(),
        };
        manager.install().unwrap();
        assert_eq!(
            fs::read_to_string(root.join("node_modules/fixture/index.js")).unwrap(),
            "module.exports = 42;\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validates_peer_requirements_against_ancestor_packages() {
        let empty = || LockedPackage {
            name: String::new(),
            version: String::new(),
            resolved: String::new(),
            integrity: String::new(),
            dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            optional_peers: Vec::new(),
            scripts: BTreeMap::new(),
            engines: BTreeMap::new(),
        };
        let mut peer = empty();
        peer.name = "peer".into();
        peer.version = "2.0.0".into();
        let mut plugin = empty();
        plugin.name = "plugin".into();
        plugin.version = "1.0.0".into();
        plugin.peer_dependencies.insert("peer".into(), "^2".into());
        let mut packages = BTreeMap::from([
            ("node_modules/peer".into(), peer),
            ("node_modules/plugin".into(), plugin),
        ]);
        assert!(validate_peer_dependencies(&packages).is_ok());
        packages.get_mut("node_modules/peer").unwrap().version = "3.0.0".into();
        assert!(validate_peer_dependencies(&packages).is_err());
    }

    #[test]
    fn reads_default_scoped_and_token_npmrc_settings() {
        let root = env::temp_dir().join(format!("sako-npmrc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join(".npmrc");
        fs::write(
            &path,
            "registry=https://packages.example/npm/\n@private:registry=https://private.example/\n//private.example/:_authToken=secret\n//packages.example/npm/:username=user\n//packages.example/npm/:_password=cGFzcw==\nusername=default-user\n_password=ZGVmYXVsdC1wYXNz\n//override.example/:username=ignored\n//override.example/:_password=aWdub3JlZA==\n//override.example/:_auth=ZmluYWw6c2VjcmV0\n",
        )
        .unwrap();
        let mut config = RegistryConfig::default();
        read_npmrc(&path, &mut config).unwrap();
        assert_eq!(config.registry, "https://packages.example/npm/");
        assert_eq!(
            config.scoped_registries.get("@private").unwrap(),
            "https://private.example/"
        );
        assert_eq!(
            config.auth_tokens.get("private.example/").unwrap(),
            "secret"
        );
        assert_eq!(
            config.basic_auth.get("packages.example/npm/").unwrap(),
            "dXNlcjpwYXNz"
        );
        assert_eq!(
            config.default_basic_auth.as_deref(),
            Some("ZGVmYXVsdC11c2VyOmRlZmF1bHQtcGFzcw==")
        );
        assert_eq!(
            config.basic_auth.get("override.example/").unwrap(),
            "ZmluYWw6c2VjcmV0"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validates_sako_engine_ranges() {
        let compatible = BTreeMap::from([("sako".into(), "^0.1.0".into())]);
        assert!(validate_sako_engine("fixture", &compatible).is_ok());

        let incompatible = BTreeMap::from([("sako".into(), ">=2.0.0".into())]);
        let error = validate_sako_engine("fixture", &incompatible).unwrap_err();
        assert!(error.to_string().contains("requires Sako >=2.0.0"));

        let node_only = BTreeMap::from([("node".into(), ">=22".into())]);
        assert!(validate_sako_engine("fixture", &node_only).is_ok());
    }

    #[test]
    fn installs_and_replays_local_workspaces() {
        let root = env::temp_dir().join(format!("sako-workspaces-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("packages/tool")).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"private":true,"workspaces":["packages/*"]}"#,
        )
        .unwrap();
        fs::write(
            root.join("packages/tool/package.json"),
            r#"{"name":"workspace-tool","version":"1.2.3","main":"index.js"}"#,
        )
        .unwrap();
        fs::write(
            root.join("packages/tool/index.js"),
            "module.exports = 42;\n",
        )
        .unwrap();

        let mut manager = PackageManager::new(&root, true).unwrap();
        manager.install().unwrap();
        let installed = root.join("node_modules/workspace-tool/index.js");
        assert_eq!(
            fs::read_to_string(&installed).unwrap(),
            "module.exports = 42;\n"
        );
        let lock_source = fs::read_to_string(root.join("sako.lock")).unwrap();
        assert!(lock_source.contains("workspace:packages/tool"));

        fs::remove_dir_all(root.join("node_modules")).unwrap();
        manager.install().unwrap();
        assert!(installed.is_file());
        fs::remove_dir_all(root).unwrap();
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
