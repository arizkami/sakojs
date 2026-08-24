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

- [ ] Support `sako app.js` and `sako run app.js` with script arguments.
- [ ] Support `sako eval`, `sako repl`, `sako --version`, and package-script dispatch.
- [ ] Establish the runtime bootstrap JS layer and basic globals.
- [ ] Implement correct timers, cancellation, queued microtasks, and event-loop draining.
- [ ] Add structured JS/system error mapping and stable stack traces.
- [ ] Add startup, exception, timer, and microtask integration tests.
- [ ] Add initial memory statistics and persistent-handle counters.

## Phase 3: Modules and core Node compatibility

- [ ] Implement V8-native ESM compilation, linking, evaluation, and relative/absolute resolution.
- [ ] Implement `package.json` `type`, `exports`, `imports`, and conditional resolution.
- [ ] Implement CommonJS loading and the CJS/ESM interoperability rules.
- [ ] Add the first `node:` modules: assert, buffer, console, events, fs, fs/promises, path, process, timers, URL, and util.
- [ ] Track each API honestly in `docs/node-compatibility.md`.
- [ ] Cover Windows paths, Unicode paths, UNC paths, extended paths, links, and locking behavior.

## Phase 4: npm packages

- [ ] Implement npm registry metadata and tarball fetching with integrity verification.
- [ ] Implement semver, dist-tags, dependency classes, peer constraints, scopes, auth, and private registries.
- [ ] Implement a bounded content-addressed Windows package store.
- [ ] Design and implement deterministic `sako.lock` serialization.
- [ ] Implement `install`, `add`, `remove`, and `update`.
- [ ] Support lifecycle scripts with `--ignore-scripts` and documented security behavior.
- [ ] Run a real pure-JavaScript npm package without source changes.

## Phase 5: Windows I/O, networking, and Express

- [ ] Implement the Windows platform reactor with IOCP and bounded completion queues.
- [ ] Implement filesystem and process APIs with native Windows semantics and resource ownership.
- [ ] Implement TCP, DNS, HTTP, HTTPS, and stream compatibility incrementally.
- [ ] Build the allocation-conscious native HTTP parser and response path.
- [ ] Materialize request headers and bodies lazily across the Rust/V8 boundary.
- [ ] Run unmodified Express through compatible Node HTTP APIs.
- [ ] Add overload/backpressure, cancellation, keep-alive, invalid-input, and resource-lifetime tests.

## Phase 6: Stability, profiling, and measured acceleration

- [ ] Add component-level diagnostics for V8 heap, native memory, external memory, handles, sockets, timers, queues, and RSS/private bytes.
- [ ] Add heavy development leak detection and scheduled memory soak tests.
- [ ] Require bounded caches, queues, pools, slabs, and registries with explicit eviction/backpressure.
- [ ] Establish reproducible startup, HTTP, Express, filesystem, package-cache, install, worker, and memory benchmarks.
- [ ] Profile Rust/V8 crossings, allocations, syscalls, parsing, and I/O before optimizing.
- [ ] Add scalar acceleration baselines and only then measured SSE4.2/AVX2 paths with differential tests.
- [ ] Add optional worker-per-isolate multicore scaling without global hot-path locks.
- [ ] Publish benchmark methodology and results without unsupported performance claims.

## Cross-cutting release gates

- [ ] Every first-party source file carries the BSD-3-Clause SPDX identifier where appropriate.
- [ ] Every native resource and persistent V8 handle has an explicit owner and teardown path.
- [ ] No cache, queue, channel, callback registry, or connection table is unbounded.
- [ ] Every `unsafe` Rust block and native pointer lifetime has a documented invariant and targeted tests.
- [ ] Memory converges after repeated load cycles; monotonic growth blocks release.
- [ ] Windows CI passes formatting, Clippy, unit/integration tests, runtime tests, module tests, npm resolver tests, HTTP tests, and memory sanity tests appropriate to the implemented phase.
