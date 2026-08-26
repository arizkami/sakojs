// SPDX-License-Identifier: BSD-3-Clause

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Cursor, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod profile;
mod install;
mod registry;
mod resolver;
mod scheduler;
mod store;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::read::GzDecoder;
use profile::{Count, Profile, Stage};
use registry::{Credentials, MetadataCache, Registry};
use sako_process::spawn_native_with_bounded_output_in;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

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

    let mut linked: HashSet<String> = HashSet::new();
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
            linked.insert(command);
        }
    }
    link_node_shim(&bin_dir, &mut linked)?;
    prune_binary_shims(&bin_dir, &linked)?;
    Ok(())
}

/// Puts a `node` on PATH that is this runtime.
///
/// Published lifecycle scripts say `node install.js` in plain text --
/// esbuild's does, and it is one of the first things a Vite project installs.
/// Sako is not Node and there may be no Node on the machine at all, so without
/// this the script simply fails. The shim is only written when nothing else
/// claimed the name, so a project that really does depend on a `node` package
/// keeps its own.
fn link_node_shim(bin_dir: &Path, linked: &mut HashSet<String>) -> Result<(), PackageError> {
    if linked.contains("node") || !bin_dir.is_dir() {
        return Ok(());
    }
    let executable = env::current_exe()
        .ok()
        .unwrap_or_else(|| PathBuf::from("sako"));
    // Not written through write_binary_shim: that one forwards a *script* path
    // to Sako, while this has to forward the caller's own arguments.
    let posix = format!(
        "#!/bin/sh\nexec \"${{SAKO_EXECUTABLE:-{}}}\" \"$@\"\n",
        executable.display().to_string().replace('\\', "/"),
    );
    let posix_path = bin_dir.join("node");
    fs::write(&posix_path, posix)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&posix_path, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    fs::write(
        bin_dir.join("node.cmd"),
        "@ECHO off\r\n\
         SETLOCAL\r\n\
         IF DEFINED SAKO_EXECUTABLE (SET \"_sako=%SAKO_EXECUTABLE%\") ELSE (SET \"_sako=sako\")\r\n\
         \"%_sako%\" %*\r\n\
         EXIT /B %ERRORLEVEL%\r\n",
    )?;
    linked.insert("node".into());
    Ok(())
}

/// Removes shims for packages that are no longer installed.
///
/// Linking only ever added before, so removing a package left a live shim
/// pointing at a deleted file -- and `sako x <name>` then failed inside the
/// shim rather than saying the tool was gone.
fn prune_binary_shims(bin_dir: &Path, linked: &HashSet<String>) -> Result<(), PackageError> {
    let Ok(entries) = fs::read_dir(bin_dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // `tsc` and `tsc.cmd` are one command; either spelling keeps both.
        let stem = path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_owned();
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_owned();
        if linked.contains(&stem) || linked.contains(&name) {
            continue;
        }
        fs::remove_file(&path)?;
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
        // EXIT /B carries the tool's exit code out of the SETLOCAL scope. Without
        // it a failing linter or type-check inside `sako run` reported success,
        // because cmd.exe reports the last *statement* rather than the child.
        let cmd = format!(
            "@ECHO off\r\n\
             SETLOCAL\r\n\
             IF DEFINED SAKO_EXECUTABLE (SET \"_sako=%SAKO_EXECUTABLE%\") ELSE (SET \"_sako=sako\")\r\n\
             \"%_sako%\" \"%~dp0{windows_target}\" %*\r\n\
             EXIT /B %ERRORLEVEL%\r\n"
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
/// installed a reporter decides between a progress tree, a log line, or
/// nothing at all.
///
/// The events come from a pool of worker threads, several at once, which is
/// why a reporter has to be `Sync` and why every one of these is cheap. A
/// reporter that draws something on each of them would make the terminal the
/// slowest part of an install; the one Sako ships keeps counters and redraws
/// on a timer instead.
pub enum ProgressEvent<'a> {
    /// The tree is decided: this many positions will be filled.
    Planned { total: usize },
    /// Asking a registry which versions of `name` exist -- or joining a
    /// request for it that another worker already started.
    ResolveStarted { name: &'a str },
    ResolveFinished { name: &'a str },
    /// This many distinct packages have to be brought in, before counting
    /// which of them the store already has.
    FetchPlanned { total: usize },
    DownloadStarted { name: &'a str },
    /// `cached` separates a package the local store already held from one
    /// fetched from the registry.
    DownloadFinished { name: &'a str, cached: bool },
    ExtractStarted { name: &'a str },
    ExtractFinished { name: &'a str },
    /// This many tree positions have to be filled from the store.
    StorePlanned { total: usize },
    Materialized { name: &'a str },
    /// This many directories need a `.bin`.
    LinkPlanned { total: usize },
    Linked,
    /// A problem that did not stop the install, usually an optional dependency
    /// that would not build. Routed through the reporter so it can be printed
    /// without tearing a half-drawn progress view.
    Warning { message: &'a str },
    /// Everything is on disk.
    Finished { installed: usize },
}

pub trait ProgressReporter: Send + Sync {
    fn report(&self, event: ProgressEvent<'_>);
}

/// Holds the optional reporter. Exists only so `PackageManager` can keep its
/// derived `Debug`, which a bare trait object would deny it.
///
/// An `Arc` rather than a `Box` because the resolver hands the reporter to
/// every worker in the pool.
#[derive(Clone, Default)]
pub(crate) struct Reporter(pub(crate) Option<Arc<dyn ProgressReporter>>);

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
    /// Everything that talks to a registry, behind an `Arc` because the
    /// resolver hands it to a pool of workers.
    registry: Arc<Registry>,
    /// One packument per name however many edges ask for it, and one request
    /// however many workers ask at once.
    metadata: Arc<MetadataCache>,
    workspaces: BTreeMap<String, WorkspacePackage>,
    ignore_scripts: bool,
    omit_dev: bool,
    legacy_peer_deps: bool,
    installed: BTreeMap<String, LockedPackage>,
    reporter: Reporter,
    profile: Arc<Profile>,
}

#[derive(Clone, Debug, Default)]
pub struct PackageManagerOptions {
    pub ignore_scripts: bool,
    /// Leave `devDependencies` out of the tree, the way `npm install
    /// --omit=dev` does. An install that deliberately holds part of the graph
    /// back never rewrites the lockfile.
    pub omit_dev: bool,
    /// Validate `peerDependencies` without installing the missing ones, which
    /// is how Sako behaved before it installed them at all.
    pub legacy_peer_deps: bool,
    /// Print the stage breakdown when the install finishes.
    pub perf: bool,
    /// Add the per-package detail above that breakdown.
    pub verbose: bool,
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
        let store = cache_directory()?.join("Sako").join("Store");
        let mut agent = ureq::AgentBuilder::new();
        if let Some(proxy) = registry_config.proxy {
            agent = agent.proxy(
                ureq::Proxy::new(proxy)
                    .map_err(|error| PackageError(format!("invalid npm proxy: {error}")))?,
            );
        }
        let profile = Profile::new();
        if options.perf || options.verbose {
            profile.enable(options.verbose);
        }
        let profile = Arc::new(profile);
        Ok(Self {
            root,
            registry: Arc::new(Registry::new(
                agent.build(),
                registry_config.registry.trim_end_matches('/').into(),
                registry_config.scoped_registries,
                Credentials {
                    auth_tokens: registry_config.auth_tokens,
                    basic_auth: registry_config.basic_auth,
                },
                store.join("sha512"),
                store.join("metadata"),
                Arc::clone(&profile),
            )),
            metadata: Arc::new(MetadataCache::new()),
            workspaces: BTreeMap::new(),
            ignore_scripts: options.ignore_scripts,
            omit_dev: options.omit_dev,
            legacy_peer_deps: options.legacy_peer_deps,
            installed: BTreeMap::new(),
            reporter: Reporter::default(),
            profile,
        })
    }

    /// A manager pointed at a store the caller seeded and a registry that does
    /// not answer, so a test can prove an install needs neither.
    #[cfg(test)]
    fn for_test(root: PathBuf, store_root: PathBuf) -> Self {
        let profile = Arc::new(Profile::new());
        Self {
            registry: Arc::new(Registry::new(
                ureq::AgentBuilder::new().build(),
                "https://invalid.example".into(),
                BTreeMap::new(),
                Credentials::default(),
                store_root.clone(),
                store_root.with_file_name("metadata"),
                Arc::clone(&profile),
            )),
            root,
            metadata: Arc::new(MetadataCache::new()),
            workspaces: BTreeMap::new(),
            ignore_scripts: true,
            omit_dev: false,
            legacy_peer_deps: false,
            installed: BTreeMap::new(),
            reporter: Reporter::default(),
            profile,
        }
    }

    /// Attaches a progress reporter. Installs are otherwise silent until they
    /// either finish or fail, which on a cold cache is a long time to look
    /// like nothing is happening.
    pub fn set_reporter(&mut self, reporter: Arc<dyn ProgressReporter>) {
        self.reporter = Reporter(Some(reporter));
    }

    /// The measurement report, empty unless `perf` or `verbose` was asked for.
    ///
    /// Returned rather than printed so the caller keeps the decision about
    /// where install diagnostics go, the same way progress events do.
    pub fn performance_report(&self) -> String {
        if !self.profile.enabled() {
            return String::new();
        }
        format!("{}{}", self.profile.detail(), self.profile.report())
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
        let manifest = self.read_manifest()?;
        let root_engines = manifest_engines(&manifest)?;
        validate_sako_engine("root package", &root_engines)?;
        self.workspaces = discover_workspaces(&self.root, &manifest)?;
        let mut dependencies = manifest_dependencies(&manifest, "dependencies")?;
        // `dependencies` wins a name declared twice, which is npm's rule and
        // the reason each of these is `or_insert` rather than `insert`.
        if !self.omit_dev {
            for (name, requirement) in manifest_dependencies(&manifest, "devDependencies")? {
                dependencies.entry(name).or_insert(requirement);
            }
        }
        // A root package's own peers are its to satisfy: nothing sits above it
        // to provide them, so they are installed like any other requirement.
        // Below the root they are handled by the planner, which can see what
        // the surrounding directories already provide.
        for (name, requirement) in manifest_dependencies(&manifest, "peerDependencies")? {
            dependencies.entry(name).or_insert(requirement);
        }
        for name in self.workspaces.keys() {
            dependencies
                .entry(name.clone())
                .or_insert_with(|| "workspace:*".into());
        }
        let optional_dependencies = manifest_dependencies(&manifest, "optionalDependencies")?;
        // Before installing, not after: whatever is pruned here would otherwise
        // still be on disk when `.bin` is written, and its shims would be
        // rewritten for a package that is on its way out.
        self.prune_omitted(&manifest, &dependencies, &optional_dependencies)?;

        if self.install_from_lock(&dependencies, &optional_dependencies)? {
            self.report(ProgressEvent::Finished {
                installed: self.installed.len(),
            });
            return Ok(());
        }

        install::install(self, &dependencies, &optional_dependencies)?;
        // A partial install describes part of the graph, and writing that over
        // the lockfile would delete every development dependency from it.
        if !self.omit_dev {
            self.write_lockfile()?;
        }
        self.report(ProgressEvent::Finished {
            installed: self.installed.len(),
        });
        Ok(())
    }

    /// Removes packages the manifest declares but this install is not going to
    /// place at the top of the tree.
    ///
    /// Which in practice means `--omit=dev`: without this, a production install
    /// over a tree that a full install had already built would plan the smaller
    /// graph, materialize it, and leave every development dependency sitting
    /// exactly where it was -- a "production" `node_modules` with the whole
    /// toolchain still in it.
    ///
    /// Only names the manifest itself mentions are considered. A directory this
    /// project never declared belongs to whoever put it there.
    fn prune_omitted(
        &self,
        manifest: &serde_json::Value,
        dependencies: &BTreeMap<String, String>,
        optional_dependencies: &BTreeMap<String, String>,
    ) -> Result<(), PackageError> {
        let node_modules = self.root.join("node_modules");
        if !node_modules.is_dir() {
            return Ok(());
        }
        for section in [
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ] {
            for name in manifest_dependencies(manifest, section)?.keys() {
                if dependencies.contains_key(name) || optional_dependencies.contains_key(name) {
                    continue;
                }
                let installed = package_install_path(&node_modules, name)?;
                if installed.is_dir() {
                    fs::remove_dir_all(&installed)?;
                }
            }
        }
        Ok(())
    }

    pub fn add(&mut self, specifier: &str, development: bool) -> Result<(), PackageError> {
        self.add_all(std::slice::from_ref(&specifier), development)
    }

    /// Records every specifier in package.json, then installs once.
    ///
    /// Adding them one at a time meant `sako add a b c` resolved and unpacked
    /// the whole graph three times over, and printed three progress reports for
    /// what the user asked for as one operation.
    pub fn add_all(&mut self, specifiers: &[&str], development: bool) -> Result<(), PackageError> {
        let section = if development {
            "devDependencies"
        } else {
            "dependencies"
        };
        let mut manifest = self.read_or_create_manifest()?;
        for specifier in specifiers {
            let (name, mut requirement) = parse_package_specifier(specifier)?;
            if requirement == "latest" {
                let selected = self.resolve(&name, &requirement)?;
                requirement = format!("^{}", selected.version);
            }
            let object = manifest
                .as_object_mut()
                .ok_or_else(|| PackageError("package.json must contain an object".into()))?;
            let dependencies = object
                .entry(section)
                .or_insert_with(|| serde_json::Value::Object(Default::default()))
                .as_object_mut()
                .ok_or_else(|| PackageError(format!("package.json {section} must be an object")))?;
            dependencies.insert(name, serde_json::Value::String(requirement));
        }
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
        // A lockfile is a cache, not a source of truth: one written by a
        // different version, or half-written by an interrupted install, used to
        // abort the install outright when resolving from package.json would
        // have produced a correct tree and a fresh lockfile.
        let lockfile: Lockfile = match serde_json::from_str(&source) {
            Ok(lockfile) => lockfile,
            Err(error) => {
                self.warn(&format!("ignoring {}: {error}", path.display()));
                return Ok(false);
            }
        };
        if lockfile.lockfile_version != 2
            || !lock_matches_manifest(&lockfile, root_dependencies, root_optional_dependencies)
        {
            return Ok(false);
        }

        // Filled by the plan rather than copied wholesale, so it ends up
        // describing what is on disk. Copying meant a package the manifest no
        // longer asks for -- one just removed -- stayed in the lockfile and in
        // the count reported at the end, describing a tree that was not there.
        self.installed.clear();
        install::replay(
            self,
            &lockfile,
            root_dependencies,
            root_optional_dependencies,
        )?;
        // Keys, because LockedPackage carries no equality and the question is
        // only which packages the tree actually holds. Both maps are ordered,
        // so this compares the sets. Skipped when development dependencies
        // were held back on purpose: the lockfile is meant to describe the
        // whole graph, and this tree is deliberately not the whole graph.
        if !self.omit_dev && self.installed.keys().ne(lockfile.packages.keys()) {
            self.write_lockfile()?;
        }
        Ok(true)
    }

    /// Resolves one requirement, going to the registry only if no worker has
    /// already brought the packument back.
    ///
    /// Kept for the paths that resolve a single package outside the graph
    /// walk -- `sako add react@latest` pinning a range, and the platform check
    /// an optional dependency needs. The graph itself no longer comes through
    /// here; it goes through the resolver's pool.
    fn resolve(&self, name: &str, requirement: &str) -> Result<Arc<PackageVersion>, PackageError> {
        self.profile.bump(Count::DependencyEdges);
        let (metadata, _) = self
            .metadata
            .get(&self.registry, name, MAXIMUM_METADATA_ENTRIES)?;
        self.profile
            .time(Stage::Semver, || select_version(&metadata, requirement))
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

/// A packument exactly as the registry sent it.
///
/// Version entries stay unparsed here on purpose. A packument lists every
/// version a package has ever published, and serde deserializing straight into
/// the typed map makes the oldest, least relevant entry able to fail the whole
/// document -- which is what `vue` did, on a 2014 release nothing was asking
/// for.
#[derive(Clone, Debug, Deserialize)]
struct RawMetadata {
    #[serde(rename = "dist-tags", default)]
    dist_tags: BTreeMap<String, String>,
    #[serde(default)]
    versions: BTreeMap<String, serde_json::Value>,
}

/// A packument, normalized once and then shared.
///
/// The shape here is what makes a packument cheap to query rather than cheap
/// to build. Twenty dependency edges asking `^6` of the same package used to
/// re-parse every one of its several hundred version strings, twenty times
/// over, and clone the whole selected entry at the end of it. Parsing and
/// ordering the versions once at construction turns each of those queries into
/// a scan that stops at the first match, and holding the entries behind an
/// `Arc` turns the clone into a refcount bump.
#[derive(Clone, Debug, Default)]
struct Metadata {
    dist_tags: BTreeMap<String, String>,
    /// Every readable entry, by its published version string. Needed as
    /// written because a dist-tag names an exact string, including one that is
    /// not valid semver.
    versions: BTreeMap<String, Arc<PackageVersion>>,
    /// The semver-parseable entries, highest first. Selection walks this and
    /// takes the first match, which is the highest match.
    ordered: Vec<(Version, Arc<PackageVersion>)>,
    /// Versions this resolver could not read, and why. Kept rather than
    /// discarded so a requirement that matches only unusable entries can say
    /// what was wrong with them instead of claiming the version does not
    /// exist.
    unusable: BTreeMap<String, String>,
}

impl From<RawMetadata> for Metadata {
    fn from(raw: RawMetadata) -> Self {
        let mut versions = BTreeMap::new();
        let mut ordered = Vec::new();
        let mut unusable = BTreeMap::new();
        for (version, entry) in raw.versions {
            match serde_json::from_value::<PackageVersion>(entry) {
                Ok(package) => {
                    let package = Arc::new(package);
                    if let Ok(parsed) = Version::parse(&version) {
                        ordered.push((parsed, Arc::clone(&package)));
                    }
                    versions.insert(version, package);
                }
                Err(error) => {
                    unusable.insert(version, error.to_string());
                }
            }
        }
        // Descending, so the first match a requirement finds is the highest
        // one and the scan can stop there.
        ordered.sort_by(|(left, _), (right, _)| right.cmp(left));
        Self {
            dist_tags: raw.dist_tags,
            versions,
            ordered,
            unusable,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct PackageVersion {
    name: String,
    version: String,
    dist: Distribution,
    /// Which platforms the package is for.
    ///
    /// This is how esbuild and rollup ship native binaries: one optional
    /// dependency per platform, each narrowed by `os` and `cpu`, and the
    /// installer is expected to take only the matching one. Without the
    /// filter a Windows install of a stock Vite project fetched and unpacked
    /// every Linux and macOS binary too.
    #[serde(default)]
    os: Vec<String>,
    #[serde(default)]
    cpu: Vec<String>,
    #[serde(default)]
    libc: Vec<String>,
    /// Set by npm's abbreviated document in place of `scripts`, which it does
    /// not send. See `installed_scripts`.
    #[serde(rename = "hasInstallScript", default)]
    has_install_script: bool,
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

impl PackageVersion {
    /// Whether this build is for the machine doing the installing.
    ///
    /// npm's rule, which the registry's metadata is written against: an empty
    /// list means "anywhere"; an entry may be negated with a leading `!`; and a
    /// list holding any positive entry requires one of them to match.
    fn supports_host(&self) -> bool {
        matches_platform(&self.os, HOST_OS)
            && matches_platform(&self.cpu, HOST_CPU)
            && matches_platform(&self.libc, HOST_LIBC)
    }
}

/// npm's spelling of the current platform, which is Node's rather than Rust's.
const HOST_OS: &str = if cfg!(target_os = "windows") {
    "win32"
} else if cfg!(target_os = "macos") {
    "darwin"
} else {
    std::env::consts::OS
};
const HOST_CPU: &str = if cfg!(target_arch = "x86_64") {
    "x64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else if cfg!(target_arch = "x86") {
    "ia32"
} else {
    std::env::consts::ARCH
};
/// Only meaningful on Linux; elsewhere no package narrows by it, so an empty
/// name simply never matches a positive list -- and never needs to.
const HOST_LIBC: &str = if cfg!(target_env = "musl") {
    "musl"
} else if cfg!(target_os = "linux") {
    "glibc"
} else {
    ""
};

fn matches_platform(allowed: &[String], host: &str) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let mut positive = false;
    for entry in allowed {
        match entry.strip_prefix('!') {
            Some(excluded) => {
                if excluded == host {
                    return false;
                }
            }
            None => {
                positive = true;
                if entry == host {
                    return true;
                }
            }
        }
    }
    // Only exclusions, none of which matched.
    !positive
}

#[derive(Clone, Debug, Default, Deserialize)]
struct PeerDependencyMetadata {
    #[serde(default)]
    optional: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct Distribution {
    tarball: String,
    /// Absent on everything published before npm 5, and on some private
    /// registries to this day; `shasum` is the fallback those still carry.
    #[serde(default)]
    integrity: Option<String>,
    #[serde(default)]
    shasum: Option<String>,
}

impl Distribution {
    fn checksum(&self) -> Result<Checksum, PackageError> {
        match (&self.integrity, &self.shasum) {
            (Some(integrity), _) if !integrity.is_empty() => Checksum::parse(integrity),
            (_, Some(shasum)) if !shasum.is_empty() => Checksum::from_shasum(shasum),
            _ => Err(PackageError(format!(
                "the registry published {} without a checksum",
                self.tarball
            ))),
        }
    }
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
    #[serde(default)]
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
    // Only an `x` standing in for a whole version part is a wildcard. A blanket
    // replace also rewrote the letters inside prerelease and build identifiers,
    // so `1.0.0-next` became `1.0.0-ne*t` and matched nothing.
    let comparator = comparator
        .split_inclusive('.')
        .map(|part| {
            let (digits, separator) = match part.strip_suffix('.') {
                Some(digits) => (digits, "."),
                None => (part, ""),
            };
            let wildcard = matches!(digits, "x" | "X" | "*");
            format!("{}{separator}", if wildcard { "*" } else { digits })
        })
        .collect::<String>();
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

fn select_version(
    metadata: &Metadata,
    requirement: &str,
) -> Result<Arc<PackageVersion>, PackageError> {
    if let Some(version) = metadata.dist_tags.get(requirement) {
        if let Some(package) = metadata.versions.get(version) {
            return Ok(Arc::clone(package));
        }
        // "missing" is the wrong word when the version is right there in the
        // packument and was only dropped because this resolver could not read
        // it. Say which, and why.
        return Err(match metadata.unusable.get(version) {
            Some(reason) => PackageError(format!(
                "dist-tag '{requirement}' points at {version}, which the registry described in a way Sako cannot read: {reason}"
            )),
            None => PackageError(format!(
                "dist-tag '{requirement}' points to a missing version"
            )),
        });
    }
    let requirement = if requirement.is_empty() {
        "*"
    } else {
        requirement
    };
    let parsed = npm_version_requirements(requirement)?;
    metadata
        .ordered
        .iter()
        .find(|(version, _)| {
            parsed
                .iter()
                .any(|requirement| requirement.matches(version))
        })
        .map(|(_, package)| Arc::clone(package))
        .ok_or_else(|| unsatisfied_requirement(metadata, requirement))
}

/// Explains a requirement that matched nothing.
///
/// Dropping an unreadable version entry keeps one bad record from failing a
/// whole packument, but it must not turn into a silent lie: if the version the
/// caller wanted is exactly one of the entries that was dropped, say so and
/// say why, rather than reporting it as never published.
fn unsatisfied_requirement(metadata: &Metadata, requirement: &str) -> PackageError {
    let mut message = format!("no version satisfies '{requirement}'");
    let Ok(parsed) = npm_version_requirements(requirement) else {
        return PackageError(message);
    };
    let mut blocked = metadata.unusable.iter().filter(|(version, _)| {
        Version::parse(version)
            .is_ok_and(|version| parsed.iter().any(|range| range.matches(&version)))
    });
    if let Some((version, reason)) = blocked.next() {
        let others = blocked.count();
        message.push_str(&format!(
            "
       {version} would have matched, but the registry described it in a way Sako cannot read: {reason}"
        ));
        if others > 0 {
            message.push_str(&format!(
                "
       ({others} more like it)"
            ));
        }
    }
    PackageError(message)
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

/// The hash algorithms a registry may name in a Subresource Integrity string.
///
/// Ordered weakest to strongest so `max` picks the best one on offer: npm
/// publishes several for the same tarball and the choice should not depend on
/// the order they happen to appear in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Algorithm {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Algorithm {
    fn from_prefix(prefix: &str) -> Option<Self> {
        match prefix {
            "sha1" => Some(Self::Sha1),
            "sha256" => Some(Self::Sha256),
            "sha384" => Some(Self::Sha384),
            "sha512" => Some(Self::Sha512),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
        }
    }

    fn digest(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => Sha1::digest(bytes).to_vec(),
            Self::Sha256 => Sha256::digest(bytes).to_vec(),
            Self::Sha384 => Sha384::digest(bytes).to_vec(),
            Self::Sha512 => Sha512::digest(bytes).to_vec(),
        }
    }
}

/// One tarball checksum: an algorithm and the digest it produced.
///
/// npm has published `dist.integrity` since 2017, but the registry still
/// serves the packages from before then with only `dist.shasum`, a bare hex
/// SHA-1. Demanding SHA-512 rejected those, and because a packument carries
/// every version a package has ever had, one ancient entry failed the whole
/// document -- which is why `sako install` could not read `vue` at all.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Checksum {
    algorithm: Algorithm,
    digest: Vec<u8>,
}

impl Checksum {
    /// Parses the strongest hash out of a Subresource Integrity string.
    ///
    /// The format allows several space-separated entries and per-entry options
    /// after a `?`, both of which npm itself emits, so neither can be assumed
    /// away. Unknown algorithms are skipped rather than rejected: a registry
    /// adding one should not stop an install that already has a usable hash.
    fn parse(integrity: &str) -> Result<Self, PackageError> {
        integrity
            .split_ascii_whitespace()
            .filter_map(|token| {
                let (prefix, encoded) = token.split_once('-')?;
                let algorithm = Algorithm::from_prefix(prefix)?;
                let encoded = encoded.split('?').next().unwrap_or(encoded);
                let digest = BASE64.decode(encoded).ok()?;
                (digest.len() == algorithm.digest(b"").len()).then_some(Self { algorithm, digest })
            })
            .max_by_key(|checksum| checksum.algorithm)
            .ok_or_else(|| PackageError(format!("no usable checksum in integrity '{integrity}'")))
    }

    /// Reads npm's legacy `dist.shasum`: SHA-1 as forty hex digits.
    fn from_shasum(shasum: &str) -> Result<Self, PackageError> {
        let digest = decode_hex(shasum).filter(|digest| digest.len() == 20);
        digest
            .map(|digest| Self {
                algorithm: Algorithm::Sha1,
                digest,
            })
            .ok_or_else(|| PackageError(format!("invalid package shasum '{shasum}'")))
    }

    /// The Subresource Integrity spelling, which is what goes in the lockfile
    /// so a replay verifies against exactly what resolution did.
    fn to_integrity(&self) -> String {
        format!("{}-{}", self.algorithm.name(), BASE64.encode(&self.digest))
    }

    /// Names the cache entry. Every algorithm produces a different digest
    /// length, so one flat namespace cannot collide across them.
    fn cache_key(&self) -> String {
        hex(&self.digest)
    }

    fn verify(&self, bytes: &[u8]) -> Result<(), PackageError> {
        if self.algorithm.digest(bytes) == self.digest {
            Ok(())
        } else {
            Err(PackageError("package integrity verification failed".into()))
        }
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) || value.is_empty() {
        return None;
    }
    value
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
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
        // A hard link inside a published tarball is a duplicate of a file the
        // archive already carries -- the way tar deduplicates identical files.
        // Copying it reproduces what the publisher packed.
        if entry_type.is_hard_link()
            && let Some(link) = entry.link_name()?
        {
            let mut source = destination.to_path_buf();
            for component in link.components().skip(1) {
                match component {
                    Component::Normal(value) => source.push(value),
                    _ => {
                        return Err(PackageError(format!(
                            "unsafe tarball link target: {}",
                            link.display()
                        )));
                    }
                }
            }
            if !source.starts_with(destination) || !source.is_file() {
                return Err(PackageError(format!(
                    "tarball hard link points outside the package: {}",
                    archive_path.display()
                )));
            }
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &output)?;
            continue;
        }
        if !(entry_type.is_file() || entry_type.is_dir()) {
            // Symlinks and device nodes are dropped rather than fatal. npm
            // publishes tarballs containing both -- a symlinked licence file is
            // enough -- and rejecting the archive failed the whole install over
            // an entry nothing needs to resolve.
            continue;
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

/// The lifecycle scripts a package actually declares.
///
/// Not the ones the packument listed, because it lists none: the abbreviated
/// document Sako asks for (`application/vnd.npm.install-v1+json`) leaves
/// `scripts` out entirely and reports only the boolean `hasInstallScript`.
/// Reading the resolved metadata therefore found an empty map for every
/// registry package, so no dependency's `preinstall`, `install`, or
/// `postinstall` had ever run -- silently, since there was nothing to report.
/// The unpacked manifest is the authority, the same way `link_binaries`
/// already reads `bin` from it.
fn installed_scripts(package: &PackageVersion, destination: &Path) -> BTreeMap<String, String> {
    if !package.scripts.is_empty() {
        return package.scripts.clone();
    }
    if !package.has_install_script {
        return BTreeMap::new();
    }
    let Ok(source) = fs::read_to_string(destination.join("package.json")) else {
        return BTreeMap::new();
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&source) else {
        return BTreeMap::new();
    };
    manifest
        .get("scripts")
        .and_then(serde_json::Value::as_object)
        .map(|scripts| {
            scripts
                .iter()
                .filter_map(|(name, command)| {
                    // A non-string entry is malformed rather than fatal; npm
                    // ignores it too.
                    command
                        .as_str()
                        .map(|command| (name.clone(), command.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The environment npm gives a lifecycle script.
///
/// Scripts are written against it and simply fail without it. The important
/// part is PATH: `"postinstall": "prebuild-install || node-gyp rebuild"` calls
/// tools that live in `node_modules/.bin`, and previously the script inherited
/// Sako's own environment unchanged, so none of them resolved. The `npm_*`
/// variables are the other half -- packages read `npm_lifecycle_event` to tell
/// which hook they are in, and `INIT_CWD` to find the project that pulled them
/// in.
fn lifecycle_environment(
    package_root: &Path,
    project_root: &Path,
    event: &str,
) -> Vec<(OsString, OsString)> {
    let mut variables = vec![
        (OsString::from("PATH"), script_path(package_root)),
        (OsString::from("npm_lifecycle_event"), event.into()),
        (OsString::from("npm_config_user_agent"), user_agent().into()),
        (OsString::from("INIT_CWD"), project_root.into()),
    ];
    // The path Sako itself was launched from, so a script that re-enters the
    // runtime gets this build rather than whichever one is on PATH -- the same
    // contract the generated shims use.
    if let Ok(executable) = env::current_exe() {
        variables.push((
            OsString::from("SAKO_EXECUTABLE"),
            executable.clone().into_os_string(),
        ));
        variables.push((OsString::from("npm_execpath"), executable.into_os_string()));
    }
    if let Ok(source) = fs::read_to_string(package_root.join("package.json"))
        && let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&source)
    {
        for (key, field) in [
            ("npm_package_name", "name"),
            ("npm_package_version", "version"),
        ] {
            if let Some(value) = manifest.get(field).and_then(serde_json::Value::as_str) {
                variables.push((OsString::from(key), value.into()));
            }
        }
    }
    variables
}

/// How Sako identifies itself to the wider npm ecosystem.
///
/// Initializers read this to decide which commands to print afterwards, which
/// is why `sako create vite` used to finish by telling the user to run
/// `npm install`. The shape is npm's: `name/version node/version platform arch`.
pub fn user_agent() -> String {
    format!("sako/{SAKO_VERSION} node/{SAKO_VERSION} {HOST_OS} {HOST_CPU}")
}

/// PATH with every `node_modules/.bin` from `package_root` upwards in front of
/// it, plus the directory holding the running Sako executable.
fn script_path(package_root: &Path) -> OsString {
    let mut directories: Vec<PathBuf> = package_root
        .ancestors()
        .map(|directory| directory.join("node_modules").join(".bin"))
        .filter(|directory| directory.is_dir())
        .collect();
    if let Ok(executable) = env::current_exe()
        && let Some(parent) = executable.parent()
    {
        directories.push(parent.to_path_buf());
    }
    let existing = env::var_os("PATH").unwrap_or_default();
    match env::join_paths(directories) {
        Ok(mut joined) if !joined.is_empty() => {
            if !existing.is_empty() {
                joined.push(if cfg!(windows) { ";" } else { ":" });
                joined.push(&existing);
            }
            joined
        }
        _ => existing,
    }
}

fn run_lifecycle_scripts(
    package_root: &Path,
    scripts: &BTreeMap<String, String>,
    project_root: &Path,
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
        let output = spawn_native_with_bounded_output_in(
            shell,
            &arguments,
            Some(package_root),
            MAXIMUM_SCRIPT_OUTPUT_BYTES,
            // The arguments are built here, not by the script, so they still
            // want the usual quoting.
            false,
            &lifecycle_environment(package_root, project_root, name),
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
        let metadata: Metadata = serde_json::from_value::<RawMetadata>(serde_json::json!({
            "dist-tags": {"latest": "2.0.0"},
            "versions": {
                "1.0.0": package_version("1.0.0"),
                "1.4.0": package_version("1.4.0"),
                "2.0.0": package_version("2.0.0")
            }
        }))
        .unwrap()
        .into();
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
    fn keeps_only_a_platform_the_host_can_run() {
        let platform = |os: &[&str], cpu: &[&str]| {
            serde_json::from_value::<PackageVersion>(serde_json::json!({
                "name": "native",
                "version": "1.0.0",
                "dist": {"tarball": "https://example.invalid/p.tgz", "integrity": "sha512-AA=="},
                "os": os,
                "cpu": cpu,
            }))
            .unwrap()
        };
        assert!(platform(&[], &[]).supports_host());
        assert!(platform(&[HOST_OS], &[HOST_CPU]).supports_host());
        assert!(!platform(&["plan9"], &[]).supports_host());
        assert!(!platform(&[], &["sparc"]).supports_host());
        // A list of exclusions admits everything it does not name.
        assert!(platform(&["!plan9"], &[]).supports_host());
        assert!(!platform(&[&format!("!{HOST_OS}")], &[]).supports_host());
    }

    #[test]
    fn reads_lifecycle_scripts_off_the_unpacked_package() {
        // The abbreviated packument carries hasInstallScript instead of the
        // scripts themselves, so the manifest on disk is the only source.
        let root = env::temp_dir().join(format!("sako-scripts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"native","version":"1.0.0","scripts":{"postinstall":"node install.js","test":42}}"#,
        )
        .unwrap();
        let abbreviated: PackageVersion = serde_json::from_value(serde_json::json!({
            "name": "native",
            "version": "1.0.0",
            "dist": {"tarball": "https://example.invalid/p.tgz", "integrity": "sha512-AA=="},
            "hasInstallScript": true,
        }))
        .unwrap();
        let scripts = installed_scripts(&abbreviated, &root);
        assert_eq!(
            scripts.get("postinstall").map(String::as_str),
            Some("node install.js")
        );
        // A non-string entry is skipped rather than failing the install.
        assert!(!scripts.contains_key("test"));

        let quiet: PackageVersion = serde_json::from_value(serde_json::json!({
            "name": "plain",
            "version": "1.0.0",
            "dist": {"tarball": "https://example.invalid/p.tgz", "integrity": "sha512-AA=="},
        }))
        .unwrap();
        assert!(installed_scripts(&quiet, &root).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_wildcard_is_a_whole_version_part_not_any_letter_x() {
        assert!(requirement_matches("1.4.5", "1.4.x"));
        assert!(requirement_matches("1.4.5", "1.4.X"));
        // The x inside a prerelease identifier is part of the name.
        assert_eq!(normalize_npm_comparator("1.0.0-next"), "=1.0.0-next");
        assert_eq!(normalize_npm_comparator("1.2.x"), "1.2.*");
        assert_eq!(normalize_npm_comparator("1.0.0-alpha.x"), "1.0.0-alpha.*");
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
        let expected = Checksum::parse(&integrity).unwrap();
        assert!(expected.verify(bytes).is_ok());
        assert!(expected.verify(b"changed").is_err());
    }

    #[test]
    fn prefers_the_strongest_hash_a_registry_offers() {
        let bytes = b"package bytes";
        let integrity = format!(
            "sha1-{} sha512-{}?foo=bar sha999-nonsense",
            BASE64.encode(Sha1::digest(bytes)),
            BASE64.encode(Sha512::digest(bytes)),
        );
        let checksum = Checksum::parse(&integrity).unwrap();
        assert_eq!(checksum.algorithm, Algorithm::Sha512);
        assert!(checksum.verify(bytes).is_ok());
    }

    #[test]
    fn falls_back_to_the_legacy_shasum() {
        let bytes = b"package bytes";
        let distribution = Distribution {
            tarball: "https://example.invalid/package.tgz".into(),
            integrity: None,
            shasum: Some(hex(&Sha1::digest(bytes))),
        };
        let checksum = distribution.checksum().unwrap();
        assert_eq!(checksum.algorithm, Algorithm::Sha1);
        assert!(checksum.verify(bytes).is_ok());
        assert!(checksum.to_integrity().starts_with("sha1-"));

        let neither = Distribution {
            tarball: "https://example.invalid/package.tgz".into(),
            integrity: None,
            shasum: None,
        };
        assert!(neither.checksum().is_err());
    }

    #[test]
    fn one_unreadable_version_does_not_fail_the_packument() {
        // vue's packument really does look like this: releases from before
        // npm 5 carry a shasum and no integrity, and the 2014 entries have no
        // dist at all worth reading.
        let metadata: Metadata = serde_json::from_value::<RawMetadata>(serde_json::json!({
            "dist-tags": {"latest": "3.5.41"},
            "versions": {
                "0.8.6": {"name": "vue", "version": "0.8.6", "dist": {
                    "shasum": "a8d10dc5550a89db4f054da991a8f2ab7c196f55",
                    "tarball": "https://example.invalid/vue-0.8.6.tgz"
                }},
                "0.0.0": {"name": "vue"},
                "3.5.41": package_version("3.5.41")
            }
        }))
        .unwrap()
        .into();
        assert_eq!(metadata.versions.len(), 2);
        assert_eq!(metadata.unusable.len(), 1);
        assert_eq!(
            select_version(&metadata, "latest").unwrap().version,
            "3.5.41"
        );
        assert_eq!(
            select_version(&metadata, "^0.8.0")
                .unwrap()
                .dist
                .checksum()
                .unwrap()
                .algorithm,
            Algorithm::Sha1
        );
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

        let mut manager = PackageManager::for_test(root.clone(), cache_root);
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
