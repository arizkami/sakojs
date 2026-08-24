# Performance log

Results in this file are local development observations, not general Sako.js performance claims.

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
