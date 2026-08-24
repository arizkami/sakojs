# Performance log

Results in this file are local development observations, not general Sako.js performance claims.

## 2026-08-24 matched runtime comparison

The release Sako executable was compared on the same host against Node.js
24.14.0, Deno 2.9.5, and Bun 1.3.11. The run used 40 measured process starts
after three warmups, five compute and warm-cache filesystem samples, and three
8-second HTTP samples at 100 connections, with the processor clock pinned by
`-HighPerformance` (observed 108.4% of nominal). All compute checksums matched
and the HTTP run recorded no non-deadline request errors.

| Runtime | JS startup median (ms) | TS startup median (ms) | Compute median (M ops/s) | Warm read median (MiB/s) | HTTP median (req/s) | HTTP p95 (ms) | Idle working set (MiB) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Sako.js 0.1.0 | 15.84 | 16.53 | 833.33 | 2,666.67 | 66,404.69 | 1.65 | 14.43 |
| Node.js 24.14.0 | 38.52 | 70.27 | 833.33 | 3,037.97 | 33,723.93 | 3.32 | 47.21 |
| Deno 2.9.5 | 43.55 | 43.16 | 833.33 | 2,008.37 | 39,161.99 | 2.96 | 41.64 |
| Bun 1.3.11 | 47.36 | 47.56 | 833.33 | 2,840.24 | 49,129.02 | 2.27 | 124.52 |

Sako had the lowest startup, the lowest idle working set, and the highest HTTP
throughput in this run. Node had the highest warm-file rate. All four runtimes
returned the same integer rate, which is discussed below. Startup includes
PowerShell process-launch overhead; filesystem reads use the warm OS cache; and
HTTP deliberately uses the common `node:http` surface rather than
runtime-specific native server APIs. The standalone report is
[`reports/runtime-comparison-2026-08-24.html`](../reports/runtime-comparison-2026-08-24.html),
with machine-readable data beside it.

## 2026-08-24 runtime optimization pass

The previous published comparison recorded Sako at 6,152.66 req/s, 659.34 MiB/s
warm reads, and an 18.38 ms HTTP p95 on the same host. The work below addressed
those three results. Because the earlier run used the Balanced power scheme and
the run above pins the clock, the fairest same-scheme comparison is the HTTP
sample taken on Balanced before and after the change: 6,153 req/s to roughly
45,000 req/s. Pinning the clock accounts for the rest of the published gain.

### HTTP throughput: 6,153 to 66,405 req/s

| Change | Mechanism |
| --- | --- |
| Event loop blocks in the completion port | `DrainEventLoop` called `Sleep(1)` whenever a tick handled no request. At the default Windows timer resolution that is roughly 15.6 ms, so each idle turn stalled every connection until the next timer tick, which fixed throughput near `connections / 15.6 ms`. The loop now waits on the server's IOCP through `sako_http_server_wait`, so an arriving request wakes it immediately, and a pending timer still caps the wait. |
| One poll per tick instead of one per socket call | `TcpAcceptor::read`/`write`/`close` each called `drain_completions`, which issued a `GetQueuedCompletionStatusEx` syscall and then scanned all 1,024 connection slots. Completions are now drained once per tick by `poll_io`, and each completion is routed to its connection by operation identifier. |
| No handle duplication per operation | Every receive and send called `TcpStream::try_clone`, duplicating the socket handle for the reactor to own. Submissions now borrow the connection's socket, which the connection keeps alive until its pending operations drain. |
| Pooled I/O buffers and OVERLAPPED records | Each receive allocated and zeroed a fresh 16 KiB buffer, each send copied into a fresh `Vec`, and each submission boxed a new OVERLAPPED. All three now come from reuse pools in the reactor. |
| Integer-keyed hash maps | The reactor's operation tables are keyed by operation identifiers and OVERLAPPED addresses, which are already unique; they now hash by multiplication instead of SipHash. |
| Tick does the whole request | A connection's response used to wait for the following tick to be written, and each tick allocated a 16 KiB read buffer per connection plus a fresh response buffer. A tick now reads into the connection buffer directly, answers up to 64 pipelined requests, reuses the response buffer, and flushes in the same pass. |
| One ArrayBuffer per request at most | Each dispatch allocated three ArrayBuffers with their backing stores and views, one of them for an empty body. An empty body and empty header set now share cached zero-length views, and the header bytes and ranges share a single buffer. |
| Positional response tuple | The finalizer returned an object whose four fields were read by name, creating four V8 strings and four generic property lookups per request. It now returns `[status, reason, headers, body]`. |
| Cached dispatch functions | `__sakoDispatchHttpRequest` and `__sakoFinalizeHttpResponse` were looked up on the global object per request; they are resolved once per runtime. |
| Request objects without per-instance accessors | `IncomingMessage` defined `headers` and `rawHeaders` with two `Object.defineProperty` calls per request. They are prototype accessors that decode lazily from the parser's bytes, so a handler that ignores headers decodes none. |
| No microtask for an unread body | The dispatcher always queued a microtask to emit `data` and `end`. It now queues one only when a listener is waiting. |
| String bodies stay strings | `response.end("...")` converted the body to a Buffer and then `Buffer.concat` copied it again. A lone UTF-8 string chunk is passed through to the native encoder. |

Measured with `benchmarks/comparison/bench-http.ps1` on the same pinned clock,
the native HTTP stack alone (`cargo run --release -p sako-http --example
plaintext`) serves the same response at 88,513 to 89,811 req/s against 66,405
through `node:http`. The dispatch surface therefore costs roughly 4 us of the
15 us per-request budget; before this pass the transport, not the surface, was
the limit.

Express 4.21.2 through the same surface (`benchmarks/express/app.js`, 4 seconds
at 50 connections, pinned clock) served 22,938 req/s with no non-deadline
errors, which is the first recorded throughput for that harness.

### Warm filesystem reads: 659 to 2,667 MiB/s

`readFileSync` read the file through an `ifstream` into a `std::string`, then
copied that string into a freshly zeroed V8 backing store. A 16 MiB read
therefore made four passes over 16 MiB: zero the string, read into it, zero the
backing store, copy. It now opens the file with `CreateFileW`, sizes it with
`GetFileSizeEx`, and reads once directly into an uninitialized backing store.

The remaining cost was page faults on fresh storage, which a repeated read pays
every iteration. `PooledArrayBufferAllocator` retains up to four freed blocks of
at least 1 MiB, capped at 32 MiB total, and hands them back to uninitialized
allocations. Zero-initialized allocations deliberately bypass the pool: fresh
pages arrive zeroed and fault in only as they are touched, so pooling them would
replace a free operation with a full memset. Allocating thirty 16 MiB arrays and
touching one byte per page went from 117 ms to 49 ms; allocating the same arrays
without touching them stayed at 2 ms.

### Integer compute: at the machine's ceiling

The compute fixture is a serial LCG chain: each iteration's `Math.imul` depends
on the previous iteration's result. On this Zen 2 host that is a 5-cycle
dependency chain per iteration — xor 1, imul 3, add 1 — which caps the fixture
near 4.17 GHz / 5 ≈ 833 M ops/s regardless of code generation. All four runtimes
measured exactly 833.33 M ops/s in the run above, and Sako's earlier 657.89 M
ops/s reflects the unpinned clock rather than a code difference. Reaching 1,000 M
ops/s on this fixture would require roughly a 5 GHz clock; no runtime change can
shorten a chain the benchmark defines. A `SAKO_V8_FLAGS` environment hook was
added so V8 flag experiments can be run against this and other fixtures without
rebuilding.

### Power scheme

Under the Balanced scheme this host idles near 46% of nominal clock and ramps
only after a workload has been running, which moved identical samples by up to
40% between runs and made short fixtures unreliable. `run.ps1 -HighPerformance`
pins the clock for the run, restores the previous scheme afterwards, and records
both the scheme and the observed processor performance in the report.

## 2026-08-24 HTTP delimiter scan

Environment:

- CPU: AMD Ryzen 7 3800X, 8 cores
- RAM: 17,063,759,872 bytes
- OS: Windows Server 2025 Standard, build 26100
- Compiler: rustc 1.94.0
- Profile: Cargo `bench` optimized profile
- Workload: 1,000,000 parses after 100,000 warmup parses of a fixed 121-byte HTTP/1.1 request head

Results:

| Implementation | ns/request-head |
| --- | ---: |
| Scalar delimiter scan | 416.69 |
| Runtime-selected AVX2 scan | 384.20 |

The observed reduction was approximately 7.8%. Differential tests cover input lengths 0 through 256, every matching-byte offset, sequence boundaries, and absent delimiters. The scalar fallback remains active on CPUs without AVX2.

## 2026-08-24 SSE4.2 evaluation

The same request-head benchmark was repeated in separate release processes with
`SAKO_ACCEL_FORCE=scalar|sse42|avx2`, 100,000 warmups, and 2,000,000 measured parses:

| Forced implementation | ns/request-head |
| --- | ---: |
| Scalar | 559.47 |
| SSE4.2 | 596.90 |
| AVX2 | 404.31 |

SSE4.2 regressed approximately 6.7% against scalar on this workload, so it is
retained for differential testing and explicit benchmarking but is not selected
automatically. AVX2 remains the only accelerated default on supported CPUs;
other CPUs use scalar. These short local runs guide dispatch policy and are not
general throughput claims.

## 2026-08-24 startup smoke baseline

Using the same host and a release build, `benchmarks/startup/run.ps1` ran the hello fixture ten times after one warmup:

| Samples | Median | p95 |
| ---: | ---: | ---: |
| 10 | 16.451 ms | 22.347 ms |

This small run only verifies the harness and establishes an initial local observation. Release comparisons require the documented 100 or more samples and raw result retention.

## 2026-08-24 harness validation

Small release-mode smoke runs verified the new harnesses on the same host. These
parameters are intentionally too small to serve as performance baselines:

| Harness | Validation parameters | Observation |
| --- | --- | ---: |
| Filesystem hot read | 1 MiB, 3 reads | 4 ms elapsed |
| Worker scaling | 1 worker, 1 sample | 46.33 ms |
| Worker scaling | 2 workers, 1 sample | 31.99 ms |
| Locked cached Express install | 1 replay | 3,674.91 ms |
| Memory workload | 100,000 objects | 23,203,840 byte RSS; leak check clean |

`oha` was unavailable, so the native HTTP and Express load harnesses were not
run and no request-throughput result is recorded.

## 2026-08-24 ETW startup profile

Windows Performance Recorder captured `GeneralProfile` while the release Sako
binary executed the hello fixture 100 times after one warmup. The run reported a
19.457 ms median and 23.286 ms p95. The 250,609,664-byte trace covered 95 Sako
processes with sampled CPU rows; very short processes can exit between samples.

Aggregated `xperf` sampled weights for the leading modules were:

| Module | Sampled weight |
| --- | ---: |
| `ntoskrnl.exe` | 1,325,498 |
| `ntdll.dll` | 369,472 |
| `sako.exe` | 273,370 |
| `Ntfs.sys` | 46,337 |
| `FLTMGR.SYS` | 35,705 |

This points startup investigation toward process/kernel transitions, image and
filesystem activity, and V8 initialization before micro-optimizing first-party
Rust code. Raw ETL, CPU/module, CPU/disk, and disk-I/O reports remain in the
ignored local `benchmark-results` directory; the checked-in harness reproduces
both capture and analysis.
