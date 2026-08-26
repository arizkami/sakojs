# Sako.js Roadmap

This roadmap translates `sako-js-superprompt.md` into ordered, testable work. A phase is complete only when its acceptance checks pass; performance work follows correctness and profiling.

## Phase 1: Windows V8 bootstrap

- [x] Inventory the existing repository without deleting working code.
- [x] Detect `.deps/v8` from the workspace or `SAKO_V8_ROOT`.
- [x] Validate the required headers, monolithic library, ICU data, target ABI, and V8 major version at build time.
- [x] Create a Cargo workspace with separate `sako-cli` and `sako-v8` ownership boundaries.
- [x] Keep raw V8 API calls behind a narrow native bridge in `sako-v8`.
- [x] Add explicit owners for the V8 platform, ArrayBuffer allocator, isolate, handle scopes, and context.
- [x] Add `console.log` and execute a UTF-8 JavaScript source file.
- [x] Report compilation/runtime exceptions and JavaScript stack traces.
- [x] Perform an explicit microtask checkpoint after top-level execution.
- [x] Dispose the isolate, V8 platform, allocator, and native wrapper on normal exit.
- [x] Pass `cargo run -p sako-cli -- tests/hello.js` with exact output `Hello from Sako.js` and exit code 0.
- [x] Pass a syntax-error smoke test with a nonzero exit code and useful source location.
- [x] Pass `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`.
- [x] Record a basic clean-exit resource check and document any tooling limitations.

Phase 1 exit criterion: Sako boots the supplied V8 build on Windows x64, runs the hello fixture, and shuts down cleanly.

## Phase 2: Script runtime and CLI

- [x] Support `sako app.js` and `sako run app.js` with script arguments.
- [x] Support `sako eval`, `sako repl`, `sako --version`, and package-script dispatch.
- [x] Establish the runtime bootstrap JS layer and basic globals, including bounded `fetch`, `Headers`, `Request`, and `Response` over native HTTP/HTTPS transport.
- [x] Run bounded parser-backed TypeScript for `.ts`, `.mts`, `.cts`, and `.tsx` entries and transitive modules across ESM/CommonJS package scopes.
- [x] Implement correct timers, cancellation, queued microtasks, and event-loop draining.
- [x] Add structured JS/system error mapping and stable stack traces.
- [x] Add startup, exception, timer, and microtask integration tests.
- [x] Add initial memory statistics and persistent-handle counters.

## Phase 3: Modules and core Node compatibility

- [x] Implement V8-native ESM compilation, linking, evaluation, and relative/absolute resolution.
- [x] Select `.js` entry mode from the nearest `package.json` `type`.
- [x] Implement exact/wildcard package `exports`, `imports`, and `import`/`require`/`node`/`default` conditional resolution.
- [x] Implement cached CommonJS loading, JSON modules, package `main`, `node_modules` lookup, and `require.resolve`.
- [x] Implement basic CJS-to-ESM synthetic namespaces and synchronous ESM-to-CJS namespaces.
- [x] Complete Node-compatible named CJS export snapshots and the targeted module edge cases. (Default exports retain the live `module.exports` object; conditional maps, dynamic import, missing-import rejection, and timer-backed top-level await are covered.)
- [x] Add initial partial `node:` modules for assert, buffer, console, events, fs, fs/promises, path, process, timers, URL, and util.
- [x] Track each API honestly in `docs/node-compatibility.md`.
- [x] Cover Windows paths, Unicode paths, UNC paths, extended paths, links, and locking behavior. (Automated tests cover Unicode, extended roots, hard/symbolic links, and sharing violations; a real `\\localhost\c$` read was validated on the development host.)

## Phase 4: npm packages

- [x] Implement npm registry metadata and tarball fetching with integrity verification.
- [x] Implement semver ranges, dist-tags, dependency classes, scoped packages, and reproducible `engines.sako` validation.
- [x] Validate peer constraints and support default/scoped `.npmrc` registries with path-scoped bearer tokens.
- [x] Discover, bounded-copy, install, and replay local workspace packages.
- [x] Implement richer npm authentication modes and complete precedence for the supported configuration surface. (Path-scoped Bearer, encoded Basic, username/password pairs, proxies, environment settings, and explicit CLI overrides are ordered and tested.)
- [x] Implement a bounded content-addressed Windows package store.
- [x] Design and implement deterministic `sako.lock` serialization.
- [x] Implement `install`, `add`, `remove`, and `update`.
- [x] Support lifecycle scripts with `--ignore-scripts` and documented security behavior.
- [x] Run a real pure-JavaScript npm package without source changes.

## Phase 5: Windows I/O, networking, and Express

- [x] Add an owned Windows IOCP reactor with bounded posted completions and drain tests.
- [x] Integrate overlapped socket/file operations, cancellation, and backpressure with the IOCP reactor. (Owned file operations and completion-driven TCP receive/send retain stable handles, buffers, and `OVERLAPPED` storage; admission and retained completions are bounded, and `CancelIoEx` completions drain before release.)
- [x] Implement filesystem and process APIs with native Windows semantics and resource ownership. (Bounded Windows path/directory/link APIs, runtime-owned descriptors, polling watchers, direct `CreateProcessW`/`STARTUPINFOEX` launch with an explicit inherited-handle list, named capture pipes, Job Object ownership, and lifecycle tests are implemented. `spawn` now returns while the child runs: bidirectional pipes, output delivered on the turn it arrives, bounded undelivered output, kill, and ref/unref, which is what esbuild's service protocol needs. Native filesystem change notifications remain a compatibility extension.)
- [x] Implement TCP, DNS, HTTP, HTTPS, and stream compatibility incrementally. (Completion-driven TCP, native DNS, owned HTTP/1, bounded request bodies, keep-alive/graceful close, TLS 1.2/1.3 HTTPS servers, and initial stream shapes are implemented; raw public sockets and HTTP/HTTPS clients remain compatibility extensions.)
- [x] Build a bounded zero-copy HTTP/1 request-head parser and validated response encoder.
- [x] Materialize request headers and bodies lazily across the Rust/V8 boundary. (The bridge transfers bounded owned byte buffers/ranges; header strings/maps and body Buffers are created only when consumed.)
- [x] Run unmodified Express 4.21.2 through compatible Node HTTP APIs.
- [x] Add overload/backpressure, cancellation, keep-alive, invalid-input, and resource-lifetime tests. (Tests cover retained-completion backpressure, socket receive cancellation/drain, connection overload, fragmented bodies, framing rejection, keep-alive, graceful close, Job Objects, descriptors, and isolate lifetime.)

## Phase 5b: Sako libraries

- [x] Establish `libs/` and the `sako:` module scheme: one TypeScript entry per library, embedded at build time, transpiled and cached on first import, with unknown specifiers reported as such rather than searched for on disk.
- [x] Ship `sako:http`, a `Request`-in/`Response`-out server over the same native owner `node:http` uses, with `onError`, `onListen`, TLS, and a close that resolves.
- [x] Answer inline from `sako:http` when a handler returns a `Response` rather than a promise. A raw dispatch path hands the handler a `Request` and takes the `Response` back as its own return value; the node-shaped objects, the second call into JavaScript, and the parked connection are all gone from the ordinary request. Measured against Elysia on Bun, `sako:http` went from 0.60x to about 0.72x of its throughput.
- [x] Stop building what a handler does not read. A served `Request` defers its URL and its header map, and a `Response` keeps a string body a string all the way to the encoder rather than converting it three times.
- [ ] Close the rest of the gap to the transport. The dispatch machinery answers about 72,000 requests a second on the benchmark host with no `Request` built at all, and `sako:http` reaches roughly 51,000-63,000 depending on the run: what remains is the `Request` and `Response` objects themselves.
- [ ] Give `sako:http` streaming request and response bodies, which the whole-body native bridge does not yet carry.

## Phase 5c: Databases

- [x] Implement the PostgreSQL v3 wire protocol in Rust: startup, cleartext/`md5`/SCRAM-SHA-256 authentication with the server signature verified, and the extended query protocol so parameters are always bound.
- [x] Ship `sako:psql` over it, with a tagged-template API, OID-driven type conversion, transactions, and bounded results.
- [ ] Give the driver TLS. Without it a managed database -- which is most of them -- cannot be reached at all.
- [ ] Move the driver onto the IOCP reactor. It blocks the event loop today, which is the same limitation the Fetch client has and the same fix.
- [ ] Add connection pooling, `LISTEN`/`NOTIFY`, cursors, and prepared statement reuse.
- [ ] Run the driver against real PostgreSQL versions in CI. The protocol is covered by a fake server today, which cannot catch a real server disagreeing with the specification.

## Phase 6: Stability, profiling, and measured acceleration

- [x] Add component-level diagnostics for V8 heap, native memory, external memory, handles, sockets, timers, queues, and RSS/private bytes. (CLI snapshots also report module entries, HTTP owners/buffers, and OS handles.)
- [x] Add heavy development leak detection and scheduled memory soak tests. (`--detect-leaks`, repeated isolate/HTTP/child cycles, and a weekly configurable plateau soak are implemented.)
- [x] Require bounded caches, queues, pools, slabs, and registries with explicit eviction/backpressure. (Limits and rejection/eviction paths are documented in code and exercised by capacity tests.)
- [x] Establish reproducible startup, HTTP, Express, filesystem, package-cache/install, worker, and memory benchmarks. (`oha` is an explicit prerequisite for load runs; no throughput result is claimed on the current host.)
- [x] Profile Rust/V8 crossings, allocations, syscalls, parsing, and I/O before optimizing. (The native-crossing inventory, component memory counters, parser experiments, and reproducible WPR/`xperf` startup and filesystem traces are documented.)
- [x] Add scalar acceleration baselines and only then measured SSE4.2/AVX2 paths with differential tests. (AVX2 is selected after a measured improvement; the measured SSE4.2 path regressed and remains benchmark-only with scalar fallback.)
- [x] Add optional worker-per-isolate multicore scaling without global hot-path locks.
- [x] Publish benchmark methodology and measured local results without unsupported performance claims.

## Cross-cutting release gates

- [x] Every first-party source file carries the BSD-3-Clause SPDX identifier where appropriate.
- [x] Every currently implemented native resource and persistent V8 handle has an explicit owner and teardown path.
- [x] Every currently implemented cache, queue, callback registry, connection table, and pool is bounded.
- [x] Every `unsafe` Rust block and native pointer lifetime has a documented invariant and targeted tests.
- [x] Memory converges after repeated load cycles; monotonic growth blocks release. (Local lifecycle and 100-cycle scheduled-soak validation pass; CI uses 1,000 cycles and a plateau bound.)
- [ ] Windows CI passes formatting, Clippy, unit/integration tests, runtime tests, module tests, npm resolver tests, HTTP tests, and memory sanity tests appropriate to the implemented phase. (The fail-closed workflow passes `actionlint` 1.7.12; the complete equivalent gate and a 100-cycle soak pass locally on Windows. A remote result requires committing/pushing the currently untracked workflow and configuring the `SAKO_V8_ARTIFACT_URL` repository secret.)
