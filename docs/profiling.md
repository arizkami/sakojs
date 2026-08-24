# Runtime profiling

Sako uses Windows Performance Recorder (WPR) and Windows Performance Analyzer
(WPA) for system-level investigation. Profiling artifacts are local evidence,
not portable performance claims, and are excluded from version control because
ETL files can contain host-specific paths and process data.

## Capture

Build the release runtime before capturing a profile, then run one of the fixed
workloads:

```powershell
cargo build --release -p sako-cli
./benchmarks/profile/run.ps1 -Workload filesystem -Iterations 100 -SizeMiB 16
./benchmarks/profile/run.ps1 -Workload startup -Iterations 100
./benchmarks/profile/run.ps1 -Workload package-cache -Iterations 5
./benchmarks/profile/analyze.ps1 -TracePath ./benchmark-results/sako-filesystem-<timestamp>.etl
```

The harness refuses to disturb an existing WPR recording. It writes an ETL and
JSON metadata file under `benchmark-results/`. Open the ETL in WPA and inspect:

- CPU Usage (Sampled) by process, stack, and module for parser and bridge cost.
- Disk Usage and File I/O for file opens, metadata traversal, reads, and writes.
- System Calls and CPU Usage (Precise) for native crossing and wait behavior.
- VirtualAlloc Commit and process lifetime for allocation and teardown patterns.

V8 heap allocation details require a dedicated V8 build with allocation tracing;
the runtime's `--memory-stats` and `--detect-leaks` counters provide the bounded
component view in ordinary builds.

## Crossing inventory

The HTTP request path performs one native callback into JavaScript per parsed
request. Method and URL strings are created for the callback, while headers cross
as one owned byte buffer plus integer ranges and are materialized only when read.
A request body crosses as an owned byte buffer and becomes a JavaScript `Buffer`
only when a data listener consumes it. A response crosses back once when ended.

Filesystem, DNS, and synchronous child-process calls currently make one native
call per JavaScript operation. Timer and HTTP event-loop polling each make one
bounded native poll per loop turn. These locations define the stack frames and
operation counts to compare in WPA before changing a hot path.

`analyze.ps1` also writes raw `xperf` sampled-profile, CPU/disk, and disk-I/O
CSV reports beside the trace. The JSON output aggregates sampled weight by Sako
process module without discarding the complete reports needed for investigation.

## Current findings

The fixed HTTP parser experiment identified request-head scanning as measurable
CPU work. AVX2 reduced the local parser time, while SSE4.2 regressed, so only AVX2
is selected automatically. The package-cache smoke workload took seconds while a
request-head parse took hundreds of nanoseconds; this is strong evidence to
investigate filesystem, archive, and materialization stacks before applying CPU
acceleration to package installation. HTTP throughput profiling remains pending
until `oha` is installed on the profiling host.
