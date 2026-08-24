# Benchmark methodology

Sako benchmark results are engineering measurements, not general performance claims. Record the commit, CPU, RAM, Windows build, power plan, compiler toolchain, V8 revision, worker count, and all workload parameters with every result.

## Required controls

- Build with `cargo build --release`; do not compare debug binaries.
- Keep the machine plugged in, use a fixed power plan, and record background load.
- Pin the processor clock for the length of a run. On the Balanced scheme this
  host idles near 46% of nominal clock and ramps only after a workload has been
  running, which moved otherwise identical samples by up to 40% between runs.
  `benchmarks/comparison/run.ps1 -HighPerformance` switches to the High
  performance scheme for the run, restores the previous scheme afterwards, and
  records both the scheme and the observed `% Processor Performance` in the
  report environment block.
- Separate cold OS-cache, warm OS-cache, and hot-process measurements.
- Run a warmup before samples and publish iteration count, median, p95, and peak memory.
- Run comparison runtimes on the same machine and workload without runtime-specific response shortcuts.
- Retain raw machine-readable output with the commit being measured.

## Current baselines

Scalar HTTP request-head parsing:

```powershell
$env:SAKO_BENCH_ITERATIONS = 5000000
cargo bench -p sako-http --bench parser
```

Use `SAKO_ACCEL_FORCE=scalar|sse42|avx2` in separate processes when comparing
accelerators. Unsupported forced features fall back to the normal safe dispatch.

Startup, including process creation, V8 initialization, script execution, and shutdown:

```powershell
cargo build --release -p sako-cli
./benchmarks/startup/run.ps1 -Iterations 100
```

The parser benchmark reports JSON containing total elapsed nanoseconds and nanoseconds per operation. The startup script reports JSON containing median and p95 milliseconds. Every release performance statement must retain the raw outputs from all relevant harnesses below.

Additional Windows harnesses:

```powershell
./benchmarks/filesystem/run.ps1 -SizeMiB 16 -Iterations 100
./benchmarks/package-cache/run.ps1 -Iterations 5
./benchmarks/worker/run.ps1 -Workers 1,2,4 -Samples 5
./benchmarks/memory/run.ps1
./benchmarks/http/run.ps1 -Duration 30s -Connections 100
./benchmarks/express/run.ps1 -Duration 30s -Connections 100
./benchmarks/profile/run.ps1 -Workload filesystem -Iterations 100 -SizeMiB 16
```

Matched runtime comparison:

```powershell
cargo build --release -p sako-cli
./benchmarks/comparison/run.ps1 -HighPerformance
```

The comparison runner executes the same JavaScript and TypeScript startup,
deterministic integer, synchronous `node:fs`, and `node:http` fixtures with
Sako.js, Node.js, Deno, and Bun. It uses the portable Deno and `oha` binaries in
`.deps/tools/comparison`, retains raw `oha` output and sample JSON under the
ignored `benchmark-results/comparison/` directory, and publishes a standalone
HTML report plus its JSON source under `reports/`. The filesystem workload is a
warm-cache test, HTTP is loopback HTTP/1.1 keep-alive, and idle server memory is
a post-readiness snapshot rather than peak load memory.

A single HTTP sample, for iterating on the server path without a full
comparison run:

```powershell
./benchmarks/comparison/bench-http.ps1                     # Sako through node:http
./benchmarks/comparison/bench-http.ps1 `
    -Executable target\release\examples\plaintext.exe -Arguments @()
```

The second form drives `cargo run --release -p sako-http --example plaintext`,
which serves the same response from the native HTTP stack with no JavaScript
runtime attached. The gap between the two is the dispatch cost of the
`node:http` surface, and it is the measurement that separates transport work
from bridge work. Both forms report requests per second, p95 latency, idle
working set, and the server's total processor time, which is the least noisy of
the four when the server saturates a single core.

Node.js and Bun must be on `PATH`. Deno and `oha` resolve from explicit
`-DenoPath` / `-OhaPath` parameters, then `PATH`, then
`.deps/tools/comparison/`.

The HTTP and Express harnesses require `oha` on `PATH`, probe readiness before
load, and always stop their hidden server process. The cache harness replays the
pinned Express lock from the content-addressed store and recreates only the
verified `benchmarks/express/node_modules` path. Filesystem results are hot-cache
unless the operator separately flushes the OS cache and records that procedure.

These harnesses establish startup, scalar parser, native HTTP, Express,
filesystem, cached install, worker, memory, and matched comparison-runtime
coverage. Middleware, JSON HTTP, WebSocket, and cold-cache suites remain future
work before a broad performance claim.

The WPR harness records a system ETW trace and JSON environment metadata under
the ignored `benchmark-results/` directory. Analysis dimensions and the current
native-crossing inventory are documented in [runtime profiling](profiling.md).

Measured optimization experiments are recorded in [the performance log](performance-log.md) with their machine and workload context.
