// SPDX-License-Identifier: BSD-3-Clause

//! Measurement for the installer, off unless `--perf` or `--verbose` asked.
//!
//! Every stage of an install is timed and every unit of work is counted here
//! rather than in the stage itself, so the resolver reads the same whether
//! anyone is watching. The counters are atomic because the stages they measure
//! run on a pool of threads: a `u64` incremented from sixteen workers is not a
//! number, it is a race.
//!
//! Nothing here allocates on the hot path. A disabled profile compiles down to
//! a load and a branch, which is what keeps it honest to leave the calls in
//! the resolver permanently rather than behind a feature gate that rots.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// One timed stage. Named rather than free-form so the report can print them
/// in pipeline order instead of whatever order they happened to finish in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Stage {
    MetadataNetwork,
    MetadataParse,
    MetadataDisk,
    Semver,
    Graph,
    Download,
    Integrity,
    Extract,
    Materialize,
    Link,
    Lifecycle,
}

impl Stage {
    /// Pipeline order, which is also report order.
    const ALL: [Stage; 11] = [
        Stage::MetadataNetwork,
        Stage::MetadataParse,
        Stage::MetadataDisk,
        Stage::Semver,
        Stage::Graph,
        Stage::Download,
        Stage::Integrity,
        Stage::Extract,
        Stage::Materialize,
        Stage::Link,
        Stage::Lifecycle,
    ];

    fn index(self) -> usize {
        self as usize
    }

    /// Which report section the stage belongs under, and its label there.
    fn group(self) -> (&'static str, &'static str) {
        match self {
            Stage::MetadataNetwork => ("resolve", "metadata network"),
            Stage::MetadataParse => ("resolve", "metadata parse"),
            Stage::MetadataDisk => ("resolve", "metadata disk"),
            Stage::Semver => ("resolve", "semver"),
            Stage::Graph => ("resolve", "graph"),
            Stage::Download => ("fetch", "network"),
            Stage::Integrity => ("fetch", "integrity"),
            Stage::Extract => ("store", "extraction"),
            Stage::Materialize => ("store", "materialize"),
            Stage::Link => ("link", ""),
            Stage::Lifecycle => ("scripts", ""),
        }
    }
}

/// One counted quantity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Count {
    DependencyEdges,
    UniqueNames,
    UniqueRequests,
    DuplicateRequests,
    MetadataCacheHits,
    MetadataCacheMisses,
    MetadataDiskHits,
    RegistryRequests,
    TarballRequests,
    StoreHits,
    Retries,
    BytesDownloaded,
    TreePositions,
    UniqueTarballs,
    Extractions,
    Materializations,
    PlatformSkipped,
}

impl Count {
    const ALL: [Count; 17] = [
        Count::DependencyEdges,
        Count::UniqueNames,
        Count::UniqueRequests,
        Count::DuplicateRequests,
        Count::MetadataCacheHits,
        Count::MetadataCacheMisses,
        Count::MetadataDiskHits,
        Count::RegistryRequests,
        Count::TarballRequests,
        Count::StoreHits,
        Count::Retries,
        Count::BytesDownloaded,
        Count::TreePositions,
        Count::UniqueTarballs,
        Count::Extractions,
        Count::Materializations,
        Count::PlatformSkipped,
    ];

    fn index(self) -> usize {
        self as usize
    }

    fn label(self) -> &'static str {
        match self {
            Count::DependencyEdges => "dependency edges",
            Count::UniqueNames => "unique package names",
            Count::UniqueRequests => "unique resolves",
            Count::DuplicateRequests => "duplicate joins",
            Count::MetadataCacheHits => "metadata cache hits",
            Count::MetadataCacheMisses => "metadata cache misses",
            Count::MetadataDiskHits => "metadata disk hits",
            Count::RegistryRequests => "registry requests",
            Count::TarballRequests => "tarball requests",
            Count::StoreHits => "store hits",
            Count::Retries => "retries",
            Count::BytesDownloaded => "bytes downloaded",
            Count::TreePositions => "tree positions",
            Count::UniqueTarballs => "unique tarballs",
            Count::Extractions => "extractions",
            Count::Materializations => "materializations",
            Count::PlatformSkipped => "platform skipped",
        }
    }
}

/// A pool whose high-water mark is worth reporting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pool {
    Metadata,
    Download,
    Extract,
    Materialize,
    Link,
}

impl Pool {
    const ALL: [Pool; 5] = [
        Pool::Metadata,
        Pool::Download,
        Pool::Extract,
        Pool::Materialize,
        Pool::Link,
    ];

    fn index(self) -> usize {
        self as usize
    }

    fn label(self) -> &'static str {
        match self {
            Pool::Metadata => "peak metadata workers",
            Pool::Download => "peak download workers",
            Pool::Extract => "peak extract workers",
            Pool::Materialize => "peak materialize workers",
            Pool::Link => "peak link workers",
        }
    }
}

/// Per-package detail, only collected under `--verbose`.
#[derive(Clone, Debug, Default)]
pub struct PackageTiming {
    pub metadata: Duration,
    pub semver: Duration,
    pub cache_hit: bool,
    pub disk_hit: bool,
    pub version: String,
}

#[derive(Debug)]
pub struct Profile {
    enabled: AtomicBool,
    /// Per-package detail costs a lock per resolve, so it is separate from the
    /// aggregate counters and off unless the detail is going to be shown.
    detailed: AtomicBool,
    stages: [AtomicU64; 11],
    counts: [AtomicU64; 17],
    live: [AtomicU64; 5],
    peak: [AtomicU64; 5],
    packages: Mutex<BTreeMap<String, PackageTiming>>,
    started: Mutex<Option<Instant>>,
}

impl Default for Profile {
    fn default() -> Self {
        Self::new()
    }
}

impl Profile {
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            detailed: AtomicBool::new(false),
            stages: [const { AtomicU64::new(0) }; 11],
            counts: [const { AtomicU64::new(0) }; 17],
            live: [const { AtomicU64::new(0) }; 5],
            peak: [const { AtomicU64::new(0) }; 5],
            packages: Mutex::new(BTreeMap::new()),
            started: Mutex::new(None),
        }
    }

    pub fn enable(&self, detailed: bool) {
        self.enabled.store(true, Ordering::Relaxed);
        self.detailed.store(detailed, Ordering::Relaxed);
        *self.started.lock().unwrap() = Some(Instant::now());
    }

    #[inline]
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    #[inline]
    fn detailed(&self) -> bool {
        self.detailed.load(Ordering::Relaxed)
    }

    /// Times `body`, attributing the elapsed wall time to `stage`.
    ///
    /// Wall time summed across workers, not divided by them: a stage that took
    /// 800 ms spread over sixteen threads reports 800 ms of work done, which is
    /// the quantity worth optimizing. The total install time printed above it
    /// is what shows how much of that overlapped.
    #[inline]
    pub fn time<T>(&self, stage: Stage, body: impl FnOnce() -> T) -> T {
        if !self.enabled() {
            return body();
        }
        let started = Instant::now();
        let value = body();
        self.add(stage, started.elapsed());
        value
    }

    #[inline]
    pub fn add(&self, stage: Stage, elapsed: Duration) {
        if self.enabled() {
            self.stages[stage.index()].fetch_add(
                elapsed.as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
        }
    }

    #[inline]
    pub fn count(&self, count: Count, amount: u64) {
        if self.enabled() {
            self.counts[count.index()].fetch_add(amount, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn bump(&self, count: Count) {
        self.count(count, 1);
    }

    /// Marks a worker as entering `pool`, returning a guard that marks it out.
    pub fn occupy(&self, pool: Pool) -> Occupancy<'_> {
        if self.enabled() {
            let live = self.live[pool.index()].fetch_add(1, Ordering::Relaxed) + 1;
            self.peak[pool.index()].fetch_max(live, Ordering::Relaxed);
        }
        Occupancy {
            profile: self,
            pool,
        }
    }

    pub fn record_package(&self, name: &str, timing: PackageTiming) {
        if !self.detailed() {
            return;
        }
        if let Ok(mut packages) = self.packages.lock() {
            packages.insert(name.to_owned(), timing);
        }
    }

    fn stage(&self, stage: Stage) -> Duration {
        Duration::from_nanos(self.stages[stage.index()].load(Ordering::Relaxed))
    }

    fn value(&self, count: Count) -> u64 {
        self.counts[count.index()].load(Ordering::Relaxed)
    }

    fn peak_of(&self, pool: Pool) -> u64 {
        self.peak[pool.index()].load(Ordering::Relaxed)
    }

    pub fn elapsed(&self) -> Duration {
        self.started
            .lock()
            .ok()
            .and_then(|started| started.map(|started| started.elapsed()))
            .unwrap_or_default()
    }

    /// The report `--perf` prints. Grouped by pipeline stage, and stages that
    /// did no work are left out rather than printed as zero: a warm install
    /// that touched no network should not have to explain a `0 ms` beside it.
    pub fn report(&self) -> String {
        let mut out = String::new();
        let mut group = "";
        for stage in Stage::ALL {
            let elapsed = self.stage(stage);
            if elapsed.is_zero() {
                continue;
            }
            let (section, label) = stage.group();
            if section != group {
                if !group.is_empty() {
                    out.push('\n');
                }
                out.push_str(section);
                out.push('\n');
                group = section;
            }
            out.push_str(&format!("  {label:<22}{:>10}\n", milliseconds(elapsed)));
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("counters\n");
        for count in Count::ALL {
            let value = self.value(count);
            if value == 0 {
                continue;
            }
            if count == Count::BytesDownloaded {
                out.push_str(&format!("  {:<22}{:>10}\n", count.label(), bytes(value)));
            } else {
                out.push_str(&format!("  {:<22}{value:>10}\n", count.label()));
            }
        }
        for pool in Pool::ALL {
            let peak = self.peak_of(pool);
            if peak != 0 {
                out.push_str(&format!("  {:<22}{peak:>10}\n", pool.label()));
            }
        }
        out
    }

    /// The extra per-package block `--verbose` adds above the report.
    pub fn detail(&self) -> String {
        let Ok(packages) = self.packages.lock() else {
            return String::new();
        };
        let mut out = String::new();
        for (name, timing) in packages.iter() {
            out.push_str(&format!("{name}@{}\n", timing.version));
            out.push_str(&format!("  metadata      {:>10}\n", precise(timing.metadata)));
            out.push_str(&format!(
                "  cache         {:>10}\n",
                if timing.cache_hit {
                    "hit"
                } else if timing.disk_hit {
                    "disk"
                } else {
                    "miss"
                }
            ));
            out.push_str(&format!("  semver        {:>10}\n", precise(timing.semver)));
        }
        out
    }
}

/// Decrements a pool's live count when dropped, however the worker leaves --
/// including through an error, which is when an unbalanced counter would
/// otherwise pin the reported peak at whatever it reached before the failure.
pub struct Occupancy<'a> {
    profile: &'a Profile,
    pool: Pool,
}

impl Drop for Occupancy<'_> {
    fn drop(&mut self) {
        if self.profile.enabled() {
            self.profile.live[self.pool.index()].fetch_sub(1, Ordering::Relaxed);
        }
    }
}

fn milliseconds(elapsed: Duration) -> String {
    let millis = elapsed.as_secs_f64() * 1_000.0;
    if millis < 10.0 {
        format!("{millis:.2} ms")
    } else {
        format!("{millis:.0} ms")
    }
}

fn precise(elapsed: Duration) -> String {
    format!("{:.2} ms", elapsed.as_secs_f64() * 1_000.0)
}

fn bytes(value: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_profile_records_nothing() {
        let profile = Profile::new();
        profile.bump(Count::RegistryRequests);
        profile.time(Stage::Semver, || ());
        assert_eq!(profile.value(Count::RegistryRequests), 0);
        assert!(profile.report().contains("counters"));
    }

    #[test]
    fn counts_and_times_once_enabled() {
        let profile = Profile::new();
        profile.enable(false);
        profile.bump(Count::RegistryRequests);
        profile.count(Count::BytesDownloaded, 2048);
        assert_eq!(profile.value(Count::RegistryRequests), 1);
        let report = profile.report();
        assert!(report.contains("registry requests"));
        assert!(report.contains("2.0 KiB"));
    }

    #[test]
    fn peak_occupancy_is_a_high_water_mark() {
        let profile = Profile::new();
        profile.enable(false);
        {
            let _first = profile.occupy(Pool::Metadata);
            let _second = profile.occupy(Pool::Metadata);
            assert_eq!(profile.peak_of(Pool::Metadata), 2);
        }
        let _third = profile.occupy(Pool::Metadata);
        assert_eq!(profile.peak_of(Pool::Metadata), 2);
    }
}
