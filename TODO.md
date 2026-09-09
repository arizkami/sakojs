# TODO

Snapshot of what's left, based on [docs/roadmap.md](docs/roadmap.md) and
[docs/node-compatibility.md](docs/node-compatibility.md) as of 2026-09-08.
Roadmap is still the authoritative source for phase-level detail; this file
is the actionable short list.

## Build / environment

- [x] Document Linux build prerequisites in README/docs — partially resolved:
      installing the distro `libc++` package (e.g. Arch `extra/libc++`) fixes
      the earlier `'cstdio' file not found` error. Still needs a README/docs
      note.
- [ ] **Blocker found 2026-09-08**: `build.rs` passes `-D_LIBCPP_ABI_NAMESPACE=Cr`
      to rename libc++'s inline namespace to match the Chromium-custom-libc++
      ABI the staged V8 archive was built with, but on Arch's packaged
      `libc++` this define has no effect. `/usr/include/c++/v1/__config_site`
      unconditionally does `#define _LIBCPP_ABI_NAMESPACE __1` with no
      `#ifndef` guard, so it silently overrides the command-line `-D` and
      everything still mangles under `std::__1`. The staged
      `libv8_monolith.a` exports symbols mangled under `std::__Cr` (verified
      with `nm`), so linking `sako_bootstrap_cache` fails with
      `undefined reference to v8::platform::NewDefaultPlatform(...)`
      (ABI/namespace mismatch, not a missing symbol).
      Fix options: (a) build libc++ from source with
      `-DLIBCXX_ABI_NAMESPACE=Cr` baked in at libc++'s own build time (CMake
      option, distro packages don't expose this), (b) vendor/statically link
      the same libc++ archive Chromium built V8 against instead of relying on
      the system package, or (c) re-stage a V8 build that doesn't rename the
      libc++ ABI namespace. Needs a decision before Linux is actually usable.
- [ ] Verify `cargo build --workspace` / `cargo test --workspace` actually
      passes on Linux end-to-end once the ABI-namespace blocker above is
      resolved (native Linux support landed in the "Add native Linux
      support" commit but has not built successfully in this environment).
- [x] **2026-09-08, resolved with caveats**: built a `__Cr`-namespaced
      libc++/libc++abi/libunwind from LLVM 22.1.8 source, installed at
      `~/.cache/sako-libcxx-build/install` (build tree at
      `~/.cache/sako-libcxx-build/{src,build}`, ~2.5 GiB, kept for reuse).
      Key steps that worked:
      - Configure directly against `<llvm-project>/runtimes/CMakeLists.txt`
        (NOT `llvm/CMakeLists.txt` with `LLVM_ENABLE_RUNTIMES=...`, which
        drags in a full core-LLVM bootstrap — ~3450 ninja steps including
        `lib/Analysis/*` — and blew through this machine's 3.7 GiB `/tmp`
        tmpfs). The `runtimes/` entry point builds only the runtime libs
        against the already-installed system `clang`/`clang++`, ~1990 steps.
      - `-DLIBCXX_ABI_NAMESPACE=__Cr` is the CMake variable (not
        `_LIBCPP_ABI_NAMESPACE`), and the value must start with `__`
        (verified via `libcxx/CMakeLists.txt`'s regex check). Confirmed with
        `nm`: our build mangles as `std::__Cr`, matching
        `.deps/v8/lib/libv8_monolith.a`'s exports exactly.
      - GNU `ld` (bfd) cannot link the staged V8 objects at all — it hard
        errors on essentially every object's `.eh_frame` section
        (`no .eh_frame_hdr table will be created`), unaffected by
        `--no-eh-frame-hdr`. Switching to `lld` (`-fuse-ld=lld`, matching
        `use_lld=true` in the V8 build's own `meta` file) avoids this
        entirely.
      - `.deps/v8/lib/libv8_libbase.a` and `libv8_libplatform.a` are
        **thin archives** (`!<thin>` magic) whose members are relative-path
        references to `.deps/v8/lib/v8_libbase/*.o` /
        `v8_libplatform/*.o` — directories that were never staged. Linking
        either archive fails with e.g. `could not get the buffer for a
        child of the archive: 'v8_libbase/abort-mode.o': No such file or
        directory`. This is a packaging defect in the staged `.deps/v8`
        artifact, not fixable locally. Workaround: don't link them —
        `libv8_monolith.a` alone already contains every symbol they'd
        provide (113k+ symbols; verified base/platform symbols resolve from
        it directly).
      - Even with everything above correct, `lld` fails to resolve exactly
        four `basic_string<char>` out-of-line members
        (`__assign_external`, `__erase_external_with_move`,
        `__init_copy_ctor_external`) when `libv8_monolith.a` and `libc++.a`
        are both linked as archives together — despite `nm` proving the
        symbols exist with byte-identical mangled names. Confirmed this is
        an archive-vs-archive resolution quirk (not an ABI mismatch) by
        extracting `string.cpp.o` from `libc++.a` with `llvm-ar p` and
        passing it as a direct object on the link line alongside the `-lc++`
        archive — the same four symbols then resolve immediately.
      - With all of the above (`lld`, monolith-only, forced `string.cpp.o`,
        full static `-Wl,-Bstatic -lc++ -lc++abi -lunwind -Wl,-Bdynamic` so
        the binary doesn't silently pick up Arch's system `libc++.so.1` at
        runtime), **the link fully succeeds** (a standalone
        `v8::platform::NewDefaultPlatform()` smoke test compiles and links
        clean, exit 0).
- [x] **2026-09-08, root cause found and fully resolved.** The segfault was
      caused by a *second*, separate ABI knob: Chromium's `__config_site`
      (fetched from `chromium.googlesource.com/chromium/src/+/main/buildtools/third_party/libc%2B%2B/__config_site`)
      sets `#define _LIBCPP_ABI_VERSION 2`, which we hadn't set (our first
      build defaulted to ABI v1). ABI v2 changes internal container layouts
      (this is the actual "other ABI-affecting patch" referenced below), so
      v1 vs v2 produced identical mangled names but incompatible memory
      layouts — hence link-clean, crash-at-runtime. Rebuilt libc++ with
      **both** `-DLIBCXX_ABI_NAMESPACE=__Cr` and `-DLIBCXX_ABI_VERSION=2`
      (install prefix `~/.cache/sako-libcxx-build/install2`) and the
      `v8::platform::NewDefaultPlatform()` smoke test now runs clean, no
      crash.
      A full V8 smoke test (`Isolate::New` → `Context::New` → compile+run
      `'Hello from Sako.js on Linux, ABI v2 fixed: ' + (21*2)` → print
      result) **passes end-to-end**, after also adding `-DV8_ENABLE_SANDBOX`
      to the compile flags (V8's pointer-compression sandbox must be
      enabled on the embedder side too — a plain `V8_FATAL` config check,
      unrelated to ABI, easy to miss since only `V8_COMPRESS_POINTERS` is
      mentioned in the repo's existing build.rs comments).

## Linux build: the complete working recipe (2026-09-08)

Everything below is proven working end-to-end (full `Isolate`/`Context`/JS
smoke test passes). Nothing here is committed to the repo yet — see
"Remaining integration work" below for what it takes to fold this into
`crates/sako-v8/build.rs`.

1. **Custom libc++/libc++abi/libunwind**, built from LLVM 22.1.8 source
   (`git clone --depth 1 --branch llvmorg-22.1.8 https://github.com/llvm/llvm-project.git`),
   configured directly against `<repo>/runtimes/CMakeLists.txt` (not
   `llvm/CMakeLists.txt` — see above for why), with:
   ```
   -DLLVM_ENABLE_RUNTIMES="libcxx;libcxxabi;libunwind"
   -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++
   -DLIBCXX_ABI_NAMESPACE=__Cr
   -DLIBCXX_ABI_VERSION=2
   -DLIBCXXABI_USE_LLVM_UNWINDER=ON
   -DCMAKE_CXX_FLAGS="-D_LIBCPP_HARDENING_MODE=_LIBCPP_HARDENING_MODE_NONE -D_LIBCPP_NO_ABI_TAG -D_LIBCPP_HAS_NO_INCOMPLETE_PSTL"
   ```
   Installed at `~/.cache/sako-libcxx-build/install2`.
2. **Link with `lld`** (`-fuse-ld=lld`), not GNU `ld`/bfd (bfd hard-errors
   on `.eh_frame` in the staged V8 objects).
3. **Link only `libv8_monolith.a`** — `libv8_libbase.a`/`libv8_libplatform.a`
   in `.deps/v8/lib` are broken thin archives (see above); monolith alone
   has everything.
4. **Force-include one object from libc++.a directly** alongside the normal
   `-lc++` archive link:
   `llvm-ar p install2/lib/libc++.a string.cpp.o > force_string.o`, then
   pass `force_string.o` as a plain object on the link line. Without this,
   `lld` fails to resolve 4 specific `basic_string<char>` members
   (`__assign_external`, `__erase_external_with_move`,
   `__init_copy_ctor_external`) even though `nm` proves they exist in
   `libc++.a` — a genuine `lld` archive-vs-archive resolution quirk
   reproduced consistently, cause not fully root-caused, workaround is
   100% reliable.
5. **Link libc++/libc++abi/libunwind statically**
   (`-Wl,-Bstatic -lc++ -lc++abi -lunwind -Wl,-Bdynamic`) so the binary
   doesn't pick up Arch's ABI-v1 system `libc++.so.1` at runtime instead of
   the ABI-v2 one we built.
6. **`temporal_rs_*` symbols** (V8 15.4 ships the Temporal proposal, which
   depends on the `temporal_capi` Rust crate — ~258 `extern "C"` functions
   V8's monolith needs unconditionally, not staged anywhere in `.deps/v8`):
   - Version pin: `temporal_capi = "=0.2.6"`, feature `zoneinfo64` (matches
     `chromium.googlesource.com/chromium/src/third_party/rust/temporal_capi`
     at the DEPS revision for V8 commit
     `12db7cb804c67ba49b7e30ff68e33921db6889db`, i.e. the commit
     `.deps/v8/meta` says this V8 build came from).
   - `cargo build --release` a small wrapper crate
     (`crate-type = ["staticlib"]`) depending on `temporal_capi`, **with a
     `#[used] static` array of `unsafe extern "C"` function pointers for
     every `temporal_rs_*` name `nm libv8_monolith.a` reports as
     undefined** (258 names — get them via
     `nm libv8_monolith.a | grep ' U temporal_rs_' | awk '{print $2}' | sort -u`).
     This step is required: normal `cargo build` of `temporal_capi` alone
     silently drops all these symbols from the final staticlib/cdylib —
     they exist in the intermediate `.rlib` (verified with `nm`) but
     rustc's own reachability-based pruning when assembling the *final*
     artifact discards anything not referenced by real Rust-level code
     (raw `extern "C"` declarations without a call don't count on their
     own — the `#[used]` static referencing them does).
     Also needs `#[global_allocator] static GLOBAL: std::alloc::System = ...;`
     in the same wrapper crate (temporal_capi is `#![no_std]`; without an
     explicit global allocator the `__rust_alloc`/`__rust_dealloc`/etc.
     shims never get emitted).
   - Extract every `.o` from `llvm-ar x` on: the wrapper's own
     `target/release/libprobe.a` (this alone supplies std/core/alloc/
     panic_abort/addr2line/gimli/etc. — rustc's own staticlib assembly
     bundles those correctly, don't extract them separately from the
     sysroot or you get duplicate-symbol errors), plus every crate's
     `.rlib` under `target/release/deps` **except** proc-macro/build-only
     crates (`proc_macro2`, `quote`, `syn`, `synstructure`, `diplomat_core`,
     `unicode_ident` *when only needed by those*, `autocfg`,
     `serde_derive`, `displaydoc`, `*_derive`) — but note `unicode_ident`,
     `serde`, and `serde_core` **are** needed at runtime too (by `strck`
     and `zoneinfo64` respectively), so re-add those three specifically.
   - Pass the full `.o` list to the same `clang++ ... -fuse-ld=lld` command
     as plain positional objects (inside the `--start-group`/`--end-group`
     with `libv8_monolith.a`).

Scratch artifacts kept for reuse: libc++ build at
`~/.cache/sako-libcxx-build/{src,build2,install2}`; the Rust wrapper crate
and extracted `.o` tree at
`/tmp/claude-1000/.../scratchpad/temporal/{probe,objs}` (this one **will**
be cleaned up when the session's `/tmp` scratchpad is reclaimed — the
recipe above is what matters, not the specific files).

## Linux build: DONE — wired into the real build (2026-09-09)

`cargo build --workspace` and `cargo test --workspace` now pass on Linux
for real (not just the standalone smoke test above). `target/debug/sako
tests/hello.js` prints `Hello from Sako.js`, exit 0 — the exact Phase 1
exit criterion the README states. Confirmed with more than the smoke test:
JSON, array methods, and async/await + the microtask queue all work
(`console.log(JSON.stringify(...))`, `.map().join()`, `async fn` + `.then`
all produced correct output in an ad-hoc script). `cargo test -p sako-v8`
passes its full lifecycle suite (isolate reuse, HTTP+TLS teardown, spawned
process cleanup). `cargo test --workspace --no-fail-fast` passes
everywhere except the pre-existing, unrelated failures listed below.

What's committed:

- `.deps/libcxx-cr/` — the custom `__Cr`/ABI-v2 libc++ build, staged the
  same way `.deps/v8` is (gitignored, not built by `build.rs`). Recipe to
  reproduce it is in docs/v8-linkage.md.
- `crates/sako-temporal-bridge/` — new crate, `[build-dependencies]` of
  both `sako-v8` and `sako-cli`. Its `build.rs` greps
  `.deps/v8/lib/libv8_monolith.a` for the exact undefined `temporal_rs_*`
  symbol list (no more hand-maintained 258-name list) and generates the
  `#[used]` keepalive array; `src/lib.rs` also defines the
  `#[global_allocator]` and a hand-pinned `__rust_no_alloc_shim_is_unstable_v2`
  (see the comment there — this one's the most fragile piece, pinned to
  the current rustc's exact mangled name).
- `crates/sako-v8/build/linux_link.rs` — the shared extraction/linking
  logic (libcxx discovery, `string.cpp.o` force-include, the
  ambiguous-candidate resolver for `temporal_bridge_objects`, sysroot
  object extraction), `include!()`-shared between `sako-v8/build.rs` and
  `sako-cli/build.rs`.
- `crates/sako-cli/build.rs` — new. **This is where the actual final link
  directives for the `sako` binary are emitted, not sako-v8/build.rs.**
  Found out the hard way: `cargo:rustc-link-arg` (needed for `-fuse-ld=lld`,
  `--start-group`/`--end-group`, and force-including loose `.o` paths)
  only applies to targets built *within the emitting package* — confirmed
  empirically with `cargo build -p sako-cli -v`, directives sako-v8's
  build script emitted were faithfully recorded in its own `output` file
  but never reached sako-cli's actual `cc` invocation. `cargo:rustc-link-lib`/
  `-search` propagate fine (that's the standard `-sys` crate mechanism);
  raw link args don't. `sako-v8/build.rs` still emits the same directives
  too, because *its own* integration tests are "within the package" and do
  need them.
- Two more real (if narrow) gotchas fixed along the way, both explained in
  code comments where fixed: (a) some `temporal_bridge_objects` crates
  (`zerovec`, `icu_calendar`, etc.) are *also* ordinary dependencies
  elsewhere in `sako-cli`'s real graph (`idna`/`icu_normalizer`), and when
  Cargo's `resolver = "3"` feature unification happens to pick the same
  hash for both, rustc's own automatic linking and our manual extraction
  duplicate each other — fixed with `-Wl,--allow-multiple-definition`
  (safe here: a duplicate only ever means byte-identical content, given
  the hash match). (b) `sysroot_objects()` is needed only for the
  standalone C++ `bootstrap_cache` tool sako-v8's build script links (not
  a normal Rust binary) — including it for `sako-cli` itself collided with
  rustc's own automatic std/core/alloc linking, since `sako-cli` *is* one.

## Pre-existing, unrelated: hardcoded Windows paths in tests (found 2026-09-09)

Discovered by `cargo test --workspace --no-fail-fast` now actually
*reaching* these tests for the first time on Linux — not a Linux-port bug,
these tests were written assuming Windows path conventions and were never
exercised on POSIX before:

- [ ] `crates/sako-cli/tests/cli.rs::executes_relative_es_modules` (around
      line 179) expects a dynamic-import resolution result of literal
      `C:\dynamic\file.js`; on Linux it correctly resolves to `file.js`.
- [ ] `crates/sako-typescript/src/lib.rs` unit tests
      (`reports_typescript_syntax_locations`,
      `transpiles_tsx_and_commonjs_syntax`,
      `transpiles_types_enums_and_namespaces`) construct module URLs from
      hardcoded `C:/project/...` paths, which fail to parse as a valid URL
      on a system with no concept of drive letters
      (`"C:/project/main.ts: cannot create a TypeScript module URL"`).

Fix is presumably to build the expected paths via `Path`/URL construction
that reflects the host platform rather than a literal Windows string, but
these weren't investigated further — out of scope for the Linux
build/linking work above.

## Remaining follow-up (smaller)

- [ ] `.deps/libcxx-cr` and the `temporal_capi` version pin
      (`crates/sako-temporal-bridge/Cargo.toml`, currently `=0.2.6`) are
      both tied to the exact `.deps/v8` build in the repo right now
      (V8 commit `12db7cb804c67ba49b7e30ff68e33921db6889db`). Re-staging
      `.deps/v8` to a newer V8 will likely need both bumped/rebuilt to
      match — check `chromium.googlesource.com/chromium/src/third_party/rust/temporal_capi`
      at the new commit's DEPS revision, and Chromium's
      `buildtools/third_party/libc++/__config_site` for ABI version/
      namespace changes.
- [ ] The `__rust_no_alloc_shim_is_unstable_v2` `export_name` pin in
      `crates/sako-temporal-bridge/src/lib.rs` is read off the *current*
      rustc's mangling of a `#[rustc_std_internal_symbol]` item (unstable,
      not available via a stable attribute) — a future rustc upgrade could
      silently change it, which would resurface as a single undefined
      symbol at final-link time. No stable fix available; just something
      to know to look for if that specific link error reappears after a
      toolchain bump.
- [ ] Windows CI gate (last unchecked roadmap item): commit/push the CI
      workflow and configure the `SAKO_V8_ARTIFACT_URL` repo secret so the
      fail-closed workflow actually runs remotely instead of only locally.

## Node.js compatibility gaps (all "Partial" or "Stub" per docs/node-compatibility.md)

- [ ] `node:net` — no JS-level raw TCP socket API yet (only `isIP*` helpers).
- [ ] HTTP/HTTPS **client** APIs (server-side already works).
- [ ] `node:child_process` — `spawn`/`exec`/`execFile` still block the event
      loop and deliver output only at process completion; no true streaming
      pipes, signals, or IPC.
- [ ] `node:crypto` — only bounded SHA-1 via Windows CNG; no other hashes,
      random generation, keys, or TLS primitives (and it's Windows-only).
- [ ] `node:zlib` — stub only, throws when invoked.
- [ ] `node:stream` — backpressure, async iteration, and destroy semantics
      incomplete.
- [ ] Express 5.1 blocked — supplied V8 build is missing Unicode
      `ID_Start`/`ID_Continue` regex properties that the router needs.
- [ ] TypeScript — no type-checking, no `tsconfig.json`, no path aliases, no
      declaration files, no source-mapped stack traces.

## Package manager

- [ ] Recursive `**` workspace globs.
- [ ] Live workspace links (currently copied, not linked).
- [ ] `NO_PROXY` matching, hardlink materialization.
- [ ] Full npm configuration parity, importing third-party lockfiles,
      non-npm package sources.

## Platform scope

- [x] Confirm parity of the new Linux port (epoll reactor, POSIX process
      spawn, `/proc` diagnostics) against the existing Windows behavior —
      the build blocker is resolved (see above); `sako-net`, `sako-platform`,
      and `sako-process`'s own unit test suites all pass on Linux now
      (5/5, 4/4, 6/6 respectively), plus `sako-v8`'s integration tests that
      exercise the epoll-backed HTTP+TLS server and spawned-process
      lifecycle end to end. Full behavioral parity with the Windows IOCP
      path (rather than "its own tests pass") hasn't been separately
      audited.
