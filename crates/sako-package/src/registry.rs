// SPDX-License-Identifier: BSD-3-Clause

//! Everything the installer says to a registry, and everything it remembers
//! about what came back.
//!
//! Split out of the package manager because the resolver stopped being one
//! thread. `Registry` holds no mutable state, so a pool of workers can share
//! one behind an `Arc` and ask it for packuments and tarballs at the same
//! time; `MetadataCache` holds the state that used to be a plain `HashMap`
//! field, and holds it in a way that survives sixteen threads asking for the
//! same package at once.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::profile::{Count, Pool, Profile, Stage};
use crate::{
    Checksum, MAXIMUM_METADATA_BYTES, MAXIMUM_TARBALL_BYTES, Metadata, PackageError, RawMetadata,
    hex, registry_auth_key,
};

/// How many times a request that failed for a reason that might not repeat is
/// tried again. Small on purpose: a registry that is down stays down for
/// longer than an install is willing to wait, and the useful case is the one
/// dropped connection in a burst of two hundred.
const MAXIMUM_ATTEMPTS: u32 = 3;
/// Base delay between attempts, doubled each time.
const RETRY_BACKOFF: Duration = Duration::from_millis(250);
/// Longest a `Retry-After` will be honoured before the install gives up on
/// waiting it out and reports the rate limit instead.
const MAXIMUM_RETRY_AFTER: Duration = Duration::from_secs(10);

/// The credentials an install may present, keyed by the registry path prefix
/// they apply to.
#[derive(Debug, Default)]
pub struct Credentials {
    pub auth_tokens: BTreeMap<String, String>,
    pub basic_auth: BTreeMap<String, String>,
}

/// Everything needed to talk to the registries this install uses, shareable
/// across the worker pool because none of it changes once built.
#[derive(Debug)]
pub struct Registry {
    pub agent: ureq::Agent,
    pub default: String,
    pub scoped: BTreeMap<String, String>,
    pub credentials: Credentials,
    /// Content-addressed tarballs.
    pub store_root: PathBuf,
    /// Packuments, kept apart from tarballs because they expire and tarballs
    /// never do: a tarball is named by its own hash, while a packument is
    /// named by a package whose contents change every time someone publishes.
    pub metadata_root: PathBuf,
    pub profile: Arc<Profile>,
    /// Distinguishes one worker's half-written temporary file from another's.
    temporary: AtomicU64,
}

impl Registry {
    pub fn new(
        agent: ureq::Agent,
        default: String,
        scoped: BTreeMap<String, String>,
        credentials: Credentials,
        store_root: PathBuf,
        metadata_root: PathBuf,
        profile: Arc<Profile>,
    ) -> Self {
        Self {
            agent,
            default,
            scoped,
            credentials,
            store_root,
            metadata_root,
            profile,
            temporary: AtomicU64::new(0),
        }
    }

    /// Which registry serves `name`, honouring a scope's own registry.
    pub fn registry_for(&self, name: &str) -> &str {
        if let Some(scope) = name
            .strip_prefix('@')
            .and_then(|name| name.split('/').next())
        {
            let scope = format!("@{scope}");
            if let Some(registry) = self.scoped.get(&scope) {
                return registry;
            }
        }
        &self.default
    }

    pub fn request(&self, url: &str) -> ureq::Request {
        let mut request = self.agent.get(url);
        let key = registry_auth_key(url);
        let bearer = self
            .credentials
            .auth_tokens
            .iter()
            .filter(|(prefix, _)| key.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len());
        let basic = self
            .credentials
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

    /// A unique temporary path beside `final_path`.
    ///
    /// Two workers finishing the same content at the same moment used to write
    /// the same `.tmp` name, so one of them renamed a file the other was still
    /// filling and the store gained a truncated archive that verified as
    /// nothing.
    fn temporary_path(&self, final_path: &Path) -> PathBuf {
        let ticket = self.temporary.fetch_add(1, Ordering::Relaxed);
        let process = std::process::id();
        let mut name = final_path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{process}.{ticket}.tmp"));
        final_path.with_file_name(name)
    }

    /// Fetches a packument, preferring a cached copy the registry said is
    /// still fresh and revalidating one it did not.
    ///
    /// This is the only place an install can avoid a network round trip per
    /// package name, and it is why a second `sako update` inside the
    /// registry's own freshness window resolves without touching the network
    /// at all. It is the registry's `max-age` that decides, not a number
    /// invented here: caching a packument for longer than npm says to is how
    /// an installer starts resolving to versions that were replaced.
    pub fn packument(&self, name: &str) -> Result<Metadata, PackageError> {
        let registry = self.registry_for(name);
        let cached = self.read_cached_metadata(registry, name);

        if let Some(entry) = &cached
            && entry.fresh()
        {
            self.profile.bump(Count::MetadataDiskHits);
            let started = Instant::now();
            let metadata = self.parse_metadata(name, &entry.body)?;
            self.profile.add(Stage::MetadataDisk, started.elapsed());
            return Ok(metadata);
        }

        let encoded = name.replace('/', "%2f");
        let url = format!("{}/{encoded}", registry.trim_end_matches('/'));
        let etag = cached.as_ref().and_then(|entry| entry.head.etag.clone());

        let started = Instant::now();
        let occupancy = self.profile.occupy(Pool::Metadata);
        self.profile.bump(Count::RegistryRequests);
        let outcome = self.attempt(|| {
            let mut request = self
                .request(&url)
                .set("Accept", "application/vnd.npm.install-v1+json");
            if let Some(etag) = &etag {
                request = request.set("If-None-Match", etag);
            }
            request.call().map_err(Box::new)
        });
        drop(occupancy);

        let response = outcome.map_err(|error| {
            PackageError(format!(
                "registry request failed for {name}: {}",
                describe(&error)
            ))
        })?;

        // Not modified: the copy on disk is the current one, so only its
        // freshness stamp needs rewriting.
        //
        // Checked on the success path rather than caught as an error, because
        // a 304 *is* a success as far as the HTTP client is concerned -- it
        // only reports 4xx and 5xx as errors. Waiting for it in an error arm
        // meant every revalidation fell through to reading the body of a
        // response that, by definition, has none.
        if response.status() == 304 {
            self.profile.add(Stage::MetadataNetwork, started.elapsed());
            let entry = cached.ok_or_else(|| {
                PackageError(format!(
                    "registry answered 304 for {name} with nothing cached to revalidate"
                ))
            })?;
            let head = CacheHead {
                etag: entry.head.etag.clone(),
                fetched: now_seconds(),
                max_age: max_age_of(&response).unwrap_or(entry.head.max_age),
            };
            self.write_cached_head(registry, name, &head);
            self.profile.bump(Count::MetadataDiskHits);
            let parse_started = Instant::now();
            let metadata = self.parse_metadata(name, &entry.body)?;
            self.profile.add(Stage::MetadataDisk, parse_started.elapsed());
            return Ok(metadata);
        }

        let etag = response.header("ETag").map(str::to_owned);
        let max_age = max_age_of(&response);
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(MAXIMUM_METADATA_BYTES + 1)
            .read_to_end(&mut bytes)?;
        self.profile.add(Stage::MetadataNetwork, started.elapsed());
        self.profile
            .count(Count::BytesDownloaded, bytes.len() as u64);
        if bytes.len() as u64 > MAXIMUM_METADATA_BYTES {
            return Err(PackageError(format!(
                "registry metadata for {name} exceeds byte limit"
            )));
        }

        let metadata = self.parse_metadata(name, &bytes)?;
        // Written after parsing, so a body this resolver cannot read is never
        // promoted into the cache to be re-read and re-rejected next time.
        self.write_cached_metadata(
            registry,
            name,
            &bytes,
            &CacheHead {
                etag,
                fetched: now_seconds(),
                max_age: max_age.unwrap_or(0),
            },
        );
        Ok(metadata)
    }

    fn parse_metadata(&self, name: &str, bytes: &[u8]) -> Result<Metadata, PackageError> {
        self.profile.time(Stage::MetadataParse, || {
            serde_json::from_slice::<RawMetadata>(bytes)
                .map(Metadata::from)
                .map_err(|error| {
                    PackageError(format!("invalid registry metadata for {name}: {error}"))
                })
        })
    }

    /// Returns the tarball bytes and whether they came off the network, which
    /// is the difference between a warm and a cold store as far as anything
    /// watching the install is concerned.
    pub fn archive(
        &self,
        tarball: &str,
        expected: &Checksum,
    ) -> Result<(Vec<u8>, bool), PackageError> {
        let key = expected.cache_key();
        let cache_path = self.store_root.join(&key[..2]).join(format!("{key}.tgz"));
        if cache_path.is_file()
            && let Ok(bytes) = fs::read(&cache_path)
        {
            // A cache entry that no longer matches its own name is corrupt
            // rather than hostile, so it is replaced from the network instead
            // of failing the install.
            if self
                .profile
                .time(Stage::Integrity, || expected.verify(&bytes))
                .is_ok()
            {
                self.profile.bump(Count::StoreHits);
                return Ok((bytes, false));
            }
            let _ = fs::remove_file(&cache_path);
        }

        let started = Instant::now();
        let occupancy = self.profile.occupy(Pool::Download);
        self.profile.bump(Count::TarballRequests);
        let response = self
            .attempt(|| self.request(tarball).call().map_err(Box::new))
            .map_err(|error| {
                PackageError(format!("tarball download failed: {}", describe(&error)))
            })?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(MAXIMUM_TARBALL_BYTES + 1)
            .read_to_end(&mut bytes)?;
        drop(occupancy);
        self.profile.add(Stage::Download, started.elapsed());
        self.profile
            .count(Count::BytesDownloaded, bytes.len() as u64);
        if bytes.len() as u64 > MAXIMUM_TARBALL_BYTES {
            return Err(PackageError("package tarball exceeds byte limit".into()));
        }
        self.profile
            .time(Stage::Integrity, || expected.verify(&bytes))?;

        // Promoted only after it verified, and promoted by rename, so nothing
        // half-written is ever reachable under the name of a valid entry.
        if let Some(parent) = cache_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = self.temporary_path(&cache_path);
        if fs::write(&temporary, &bytes).is_ok() && fs::rename(&temporary, &cache_path).is_err() {
            // Losing the race is the normal outcome when two positions want
            // the same content: the winner's entry is just as good.
            let _ = fs::remove_file(&temporary);
        }
        Ok((bytes, true))
    }

    /// Retries a request whose failure might not repeat, with backoff.
    ///
    /// A 4xx other than 429 is the registry answering the question, so it is
    /// returned immediately: retrying a 404 three times only makes a missing
    /// optional dependency take a second longer to skip.
    fn attempt<T>(
        &self,
        mut request: impl FnMut() -> Result<T, Box<ureq::Error>>,
    ) -> Result<T, Box<ureq::Error>> {
        let mut attempt = 1;
        loop {
            let outcome = request();
            let Err(error) = outcome else {
                return outcome;
            };
            let retry_after = match error.as_ref() {
                ureq::Error::Status(429, response) | ureq::Error::Status(503, response) => {
                    Some(retry_after_of(response).unwrap_or(RETRY_BACKOFF * attempt))
                }
                ureq::Error::Status(status, _) if *status >= 500 => {
                    Some(RETRY_BACKOFF * attempt)
                }
                ureq::Error::Status(_, _) => None,
                // A transport error is a dropped connection, a timeout, or DNS:
                // all worth one more try.
                ureq::Error::Transport(_) => Some(RETRY_BACKOFF * attempt),
            };
            let Some(delay) = retry_after.filter(|_| attempt < MAXIMUM_ATTEMPTS) else {
                return Err(error);
            };
            self.profile.bump(Count::Retries);
            std::thread::sleep(delay.min(MAXIMUM_RETRY_AFTER));
            attempt += 1;
        }
    }

    fn metadata_paths(&self, registry: &str, name: &str) -> (PathBuf, PathBuf) {
        // Keyed by registry as well as name so a package pulled from a private
        // registry never reads a copy cached from the public one -- the two
        // are different packages that happen to share a name.
        let mut digest = Sha256::new();
        digest.update(registry.trim_end_matches('/').as_bytes());
        digest.update(b"\0");
        digest.update(name.as_bytes());
        let key = hex(&digest.finalize());
        let directory = self.metadata_root.join(&key[..2]);
        (
            directory.join(format!("{key}.json")),
            directory.join(format!("{key}.head")),
        )
    }

    fn read_cached_metadata(&self, registry: &str, name: &str) -> Option<CacheEntry> {
        let (body_path, head_path) = self.metadata_paths(registry, name);
        let head: CacheHead = serde_json::from_slice(&fs::read(head_path).ok()?).ok()?;
        let body = fs::read(body_path).ok()?;
        Some(CacheEntry { head, body })
    }

    fn write_cached_metadata(&self, registry: &str, name: &str, body: &[u8], head: &CacheHead) {
        let (body_path, _) = self.metadata_paths(registry, name);
        let Some(parent) = body_path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let temporary = self.temporary_path(&body_path);
        if fs::write(&temporary, body).is_err() {
            return;
        }
        if fs::rename(&temporary, &body_path).is_err() {
            let _ = fs::remove_file(&temporary);
            return;
        }
        // Head last: a body with no head is simply a miss next time, while a
        // head with no body would claim a cached copy that is not there.
        self.write_cached_head(registry, name, head);
    }

    fn write_cached_head(&self, registry: &str, name: &str, head: &CacheHead) {
        let (_, head_path) = self.metadata_paths(registry, name);
        let Ok(encoded) = serde_json::to_vec(head) else {
            return;
        };
        let temporary = self.temporary_path(&head_path);
        if fs::write(&temporary, &encoded).is_ok() && fs::rename(&temporary, &head_path).is_err() {
            let _ = fs::remove_file(&temporary);
        }
    }
}

/// What the disk cache remembers about one packument besides its bytes.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CacheHead {
    #[serde(default)]
    etag: Option<String>,
    /// Unix seconds.
    fetched: u64,
    /// Seconds the registry said the answer stays good for.
    #[serde(rename = "maxAge", default)]
    max_age: u64,
}

struct CacheEntry {
    head: CacheHead,
    body: Vec<u8>,
}

impl CacheEntry {
    /// Whether the registry's own freshness window has not run out.
    ///
    /// A cache stamped in the future is a clock that moved, not a packument
    /// that is good forever, so it counts as stale.
    fn fresh(&self) -> bool {
        let now = now_seconds();
        now >= self.head.fetched && now - self.head.fetched < self.head.max_age
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// The `max-age` of a `Cache-Control`, ignoring a response that says not to
/// store it at all.
fn max_age_of(response: &ureq::Response) -> Option<u64> {
    let header = response.header("Cache-Control")?.to_ascii_lowercase();
    if header.contains("no-store") || header.contains("no-cache") {
        return Some(0);
    }
    header
        .split(',')
        .filter_map(|directive| directive.trim().strip_prefix("max-age=").map(str::to_owned))
        .find_map(|value| value.parse().ok())
}

/// `Retry-After` in its seconds form. The HTTP-date form is rare from
/// registries and a missing value simply falls back to the backoff.
fn retry_after_of(response: &ureq::Response) -> Option<Duration> {
    response
        .header("Retry-After")?
        .trim()
        .parse()
        .ok()
        .map(Duration::from_secs)
}

/// A registry failure in one line.
///
/// `ureq`'s own `Display` for a status error prints the URL and the status but
/// not the body, and the body is where a registry explains an authentication
/// failure. Reading it costs nothing on the failure path.
fn describe(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, _) => format!("registry answered {status}"),
        ureq::Error::Transport(transport) => transport.to_string(),
    }
}

/// What a cache slot holds while, and after, one worker fetches it.
enum Slot {
    /// A worker is fetching this packument; everyone else waits for it.
    Pending,
    Ready(Arc<Metadata>),
    /// Remembered so a registry that is down costs one request per package
    /// name rather than one per dependency edge that mentions it.
    Failed(Arc<str>),
}

/// One packument per registry and name, however many edges ask for it.
///
/// The point of this type is the `Pending` state. Without it, twenty
/// dependencies on `@changesets/types@^6` arriving at once on twenty threads
/// would each find an empty cache and each start a request: the cache would
/// dedupe perfectly and still make twenty round trips. Holding the slot before
/// releasing the lock means the first worker fetches and the other nineteen
/// park on the condvar until it is there.
#[derive(Debug, Default)]
pub struct MetadataCache {
    entries: Mutex<HashMap<(String, String), SlotState>>,
    ready: Condvar,
}

/// `Slot` has no `Debug`, and `MetadataCache` wants one; this keeps the
/// derive without asking `Metadata` to describe itself in a log.
struct SlotState(Slot);

impl std::fmt::Debug for SlotState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.0 {
            Slot::Pending => "pending",
            Slot::Ready(_) => "ready",
            Slot::Failed(_) => "failed",
        })
    }
}

impl MetadataCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The packument for `name`, fetched at most once however many callers
    /// want it.
    pub fn get(
        &self,
        registry: &Registry,
        name: &str,
        capacity: usize,
    ) -> Result<Arc<Metadata>, PackageError> {
        let key = (registry.registry_for(name).to_owned(), name.to_owned());
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        loop {
            match entries.get(&key) {
                Some(SlotState(Slot::Ready(metadata))) => {
                    registry.profile.bump(Count::MetadataCacheHits);
                    return Ok(Arc::clone(metadata));
                }
                Some(SlotState(Slot::Failed(reason))) => {
                    registry.profile.bump(Count::MetadataCacheHits);
                    return Err(PackageError(reason.to_string()));
                }
                Some(SlotState(Slot::Pending)) => {
                    // Someone else is already asking. Joining their request is
                    // the whole point; this is the "shared future" wait.
                    registry.profile.bump(Count::MetadataCacheHits);
                    entries = self
                        .ready
                        .wait(entries)
                        .unwrap_or_else(|error| error.into_inner());
                }
                None => break,
            }
        }
        if entries.len() >= capacity {
            return Err(PackageError(
                "registry metadata cache capacity exceeded".into(),
            ));
        }
        registry.profile.bump(Count::MetadataCacheMisses);
        registry.profile.bump(Count::UniqueNames);
        entries.insert(key.clone(), SlotState(Slot::Pending));
        drop(entries);

        // Outside the lock: this is a network round trip, and holding the map
        // across it would serialize every other package name behind this one.
        let fetched = registry.packument(name);

        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let outcome = match fetched {
            Ok(metadata) => {
                let metadata = Arc::new(metadata);
                entries.insert(key, SlotState(Slot::Ready(Arc::clone(&metadata))));
                Ok(metadata)
            }
            Err(error) => {
                let reason: Arc<str> = Arc::from(error.to_string());
                entries.insert(key, SlotState(Slot::Failed(Arc::clone(&reason))));
                Err(PackageError(reason.to_string()))
            }
        };
        // Everyone parked above is waiting on this slot, and a condvar cannot
        // wake only the ones that wanted this key.
        self.ready.notify_all();
        outcome
    }
}
