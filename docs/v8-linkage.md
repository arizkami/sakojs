# V8 linkage strategy

Phase 1 consumes the prebuilt V8 artifact in `.deps/v8`; it never downloads or rebuilds V8.

The `sako-v8` build script locates the artifact at `SAKO_V8_ROOT` when set and otherwise uses `<workspace>/.deps/v8`. It fails early unless it finds `include/v8.h`, `include/libplatform/libplatform.h`, `lib/v8_monolith.lib`, and `bin/icudtl.dat`. It also verifies the supported V8 major version and the Windows x64 MSVC target.

The supplied artifact is V8 14.9.0.0, release mode, built as a static monolith using the static MSVC runtime. The monolith contains the startup snapshot; no external `snapshot_blob.bin` is present. ICU is external and loaded from `bin/icudtl.dat` before V8 platform initialization.

This particular monolith also contains d8's process-wide Windows `malloc` shim. The build script uses the MSVC librarian to create a target-local `v8_embedder.lib` with only `allocator_shim_win_static.obj` removed. This keeps V8's internal PartitionAlloc code but prevents the runtime library from replacing the host process allocator. The source artifact in `.deps/v8` is never modified.

Rust does not bind the C++ V8 ABI directly. `bridge.cc` is compiled as C++20 with pointer compression enabled and the sandbox disabled to match this artifact, then exposes a small C ABI to Rust. `sako-v8` is the sole owner of this boundary. V8 platform, allocator, isolate, scopes, and context teardown remain explicit in the bridge.

Redistribution packaging is separate from development linkage. A packaged runtime will need a relocatable ICU lookup/copy strategy and must preserve V8 and third-party license notices.

## Linux

`.deps/v8` on Linux is a monolithic release build of V8 15 (sandbox
**enabled**, pointer compression enabled) produced against Chromium's
custom-ABI libc++ fork (inline namespace `__Cr`, `_LIBCPP_ABI_VERSION 2`).
Linking `sako-v8`'s bridge against it needs four things beyond a normal
`clang++`/`lld`/`llvm-ar` toolchain, none of which are automated by
`build.rs` — it fails fast with a clear message if any are missing.

### 1. A matching custom-ABI libc++

The system libc++ (`std::__1`, ABI v1) is binary-incompatible with what
`.deps/v8` was linked against. `build.rs` looks for a prebuilt one at
`SAKO_LIBCXX_ROOT` (or `<workspace>/.deps/libcxx-cr`), staged the same way
as `.deps/v8` itself (i.e. not fetched or built by `build.rs`). To produce
one from upstream LLVM:

```bash
git clone --depth 1 --branch llvmorg-22.1.8 https://github.com/llvm/llvm-project.git llvm-project
cmake -S llvm-project/runtimes -B build -G Ninja \
  -DLLVM_ENABLE_RUNTIMES="libcxx;libcxxabi;libunwind" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ \
  -DLIBCXX_ABI_NAMESPACE=__Cr \
  -DLIBCXX_ABI_VERSION=2 \
  -DLIBCXXABI_USE_LLVM_UNWINDER=ON \
  -DCMAKE_CXX_FLAGS="-D_LIBCPP_HARDENING_MODE=_LIBCPP_HARDENING_MODE_NONE -D_LIBCPP_NO_ABI_TAG -D_LIBCPP_HAS_NO_INCOMPLETE_PSTL" \
  -DCMAKE_INSTALL_PREFIX=/path/to/.deps/libcxx-cr
ninja -C build install-cxx install-cxxabi install-unwind
```

Configure directly against `<llvm-project>/runtimes/CMakeLists.txt`, not
`llvm/CMakeLists.txt` with `LLVM_ENABLE_RUNTIMES` set — the latter is a full
LLVM/Clang bootstrap (thousands of build steps just to reach the runtimes)
where the former builds only the runtime libraries against the
already-installed host `clang`.

Both `-DLIBCXX_ABI_NAMESPACE=__Cr` and `-DLIBCXX_ABI_VERSION=2` are
required together. The namespace alone produces byte-identical mangled
names against `.deps/v8` (so it links cleanly) but the wrong internal
container layout (ABI v1) — the two libc++ builds agree on symbol names
while disagreeing on memory layout, so the process segfaults inside V8's
first real use of a libc++ container, not at link time. Chromium's own ABI
configuration is `buildtools/third_party/libc++/__config_site` in
`chromium/src`; that's the authoritative source if a future V8 roll changes
it again.

### 2. `lld`, not GNU `ld`

GNU `ld` (bfd) hard-errors while linking almost every object in
`libv8_monolith.a` (`error in ...(.eh_frame); no .eh_frame_hdr table will
be created`) — unaffected by `--no-eh-frame-hdr`. `lld` links the same
objects cleanly, and is what V8's own `meta/args.gn` records
(`use_lld=true`) for this artifact.

### 3. Only `libv8_monolith.a`

`.deps/v8/lib/libv8_libbase.a` and `libv8_libplatform.a` are **thin
archives** (`!<thin>` magic) whose members are paths like
`v8_libbase/abort-mode.o` relative to the archive, expecting a
`.deps/v8/lib/v8_libbase/` object tree that was never staged — a packaging
gap in whatever produced this V8 build, not something fixable locally.
`libv8_monolith.a` alone already contains everything they would provide (it
has 100k+ symbols on its own), so `build.rs` only links that one.

Separately, `lld` fails to resolve four specific `basic_string<char>`
members (`__assign_external`, `__erase_external_with_move`,
`__init_copy_ctor_external`) when both `libv8_monolith.a` and `libc++.a`
are linked as archives together in the same command — reproducible, cause
not fully root-caused, but `nm` proves the symbols exist in `libc++.a` with
byte-identical mangled names, so it isn't an ABI problem. Force-including
`libc++.a`'s `string.cpp.o` as a plain object (`llvm-ar p libc++.a
string.cpp.o > string.cpp.o`, then pass it directly on the link line
instead of only via `-lc++`) resolves it every time.

### 4. The Temporal (`temporal_rs`) bridge

V8 15 ships the Temporal proposal, implemented by the Rust crate
`temporal_capi` (via `temporal_rs`) — every isolate references its
`extern "C"` surface unconditionally (~258 `temporal_rs_*` symbols),
regardless of whether the embedded JS ever touches `Temporal`, and none of
it is staged in `.deps/v8`.

`crates/sako-temporal-bridge` is a small crate whose only job is compiling
`temporal_capi` (pinned to `=0.2.6`, feature `zoneinfo64` — matching the
version Chromium's own `third_party/rust/temporal_capi` vendors for the V8
commit this `.deps/v8` was built from) and keeping its FFI surface from
being discarded: a plain `cargo build` of `temporal_capi` silently drops
every `temporal_rs_*` function from its compiled output, because nothing in
Rust code calls them and rustc's own reachability analysis has no way to
know an external `.a` will need them by name. `sako-temporal-bridge`'s
`build.rs` runs `nm` on `libv8_monolith.a` to get the exact list V8 needs,
and generates a `#[used]` static array of `unsafe extern "C"` function
pointers referencing every one of them — that's what keeps them compiled
in.

Being a `[build-dependencies]` entry of `sako-v8`/`sako-cli` (not a normal
one — see §5) is still not enough on its own to get those symbols into the
final `sako` binary: Cargo adds `sako-temporal-bridge`'s `.rlib` to the
link line as a normal static archive, and normal single-pass archive
scanning only pulls in members that resolve an *already-known* undefined
symbol — `libv8_monolith.a` usually isn't scanned until after it, so
nothing forces the pull. The build scripts sidestep this the same way they
do for `libc++.a`'s `string.cpp.o`: `temporal_bridge_objects()` in
`crates/sako-v8/build/linux_link.rs` extracts every object file out of
`sako-temporal-bridge`'s `.rlib` and its runtime dependencies' `.rlib`s
(all already built and sitting in the shared `target/<profile>/deps` by
the time either build script runs) and force-includes them as plain
objects next to `libv8_monolith.a`.

A crate in that dependency closure can have more than one hashed `.rlib`
present at once: `resolver = "3"` (set workspace-wide) unifies features
separately for the build-dependency graph and the normal one, and even
two consumers *within* the build-dependency graph can pull in the same
crate with different features — `icu_calendar` and `zerotrie` both do, in
practice. `temporal_bridge_objects()` resolves this the way a linker
would: starting from whichever crates have only one candidate to begin
with, it picks another candidate only once something already-chosen is
shown (via `nm`) to reference a symbol that candidate defines, expanding
outward until nothing new resolves. Crates that only ever export mangled
(`_R`-prefixed, non-`__rustc::`) names are safe to include *every*
candidate for unconditionally instead — see `has_unstable_defined_symbol`'s
doc comment for why mangling makes that safe.

### 5. Where the final link directives actually get emitted

Not from `sako-v8`'s build script, even though it owns `bridge.cc` and (via
its `links = "sako_v8_bridge"` key) is the natural-seeming place. Verified
empirically (`cargo build -p sako-cli -v`, comparing the recorded build
script `output` file against the actual final `cc` invocation): while
`cargo:rustc-link-lib`/`cargo:rustc-link-search` from a dependency's build
script do propagate to a downstream binary that depends on it (the
standard `-sys`-crate mechanism), plain `cargo:rustc-link-arg` does not —
it only applies to targets built *within the emitting package itself*.
That's everything needed here except `-fuse-ld=lld`, the
`--start-group`/`--end-group` archive wrapping, and force-including loose
`.o` paths, none of which have a `link-lib`/`link-search` equivalent.

So `crates/sako-cli/build.rs` duplicates the relevant parts of
`sako-v8/build.rs`'s Linux logic (via the same `include!()`-shared
`crates/sako-v8/build/linux_link.rs`) and emits the actual final
`cargo:rustc-link-arg` sequence itself, since those apply correctly to
`sako-cli`'s own binary target. It finds `sako-v8`'s compiled
`sako_v8_bridge` static library via `DEP_SAKO_V8_BRIDGE_ROOT` (Cargo's
standard convention for a `links`-owning build script — here, `sako-v8` —
to expose its `OUT_DIR` to direct dependents via `cargo:root=...`).
`sako-v8/build.rs` *also* still emits the same directives itself, because
they're needed for its own integration tests (built "within the package").

One more difference between the two: `sako-cli`'s build script must **not**
call `sysroot_objects()` — unlike sako-v8's standalone C++ `bootstrap_cache`
tool (not a normal Rust binary), `sako-cli` *is* one, so rustc's own
automatic linking already provides std/core/alloc/panic_abort, and
re-adding them manually collides with that. It also needs
`-Wl,--allow-multiple-definition`: some `temporal_bridge_objects` crates
are *also* ordinary dependencies elsewhere in `sako-cli`'s real graph
(`idna`/`icu_normalizer` pull in overlapping ICU infrastructure), and when
Cargo's feature unification happens to pick the same hash for both,
rustc's automatic linking and this manual extraction duplicate each
other — harmlessly, since a hash match means byte-identical content.

## Phase 1 shutdown check

The `sako-v8` lifecycle integration test records the Windows process handle count, initializes V8, executes JavaScript and a microtask checkpoint, drops the runtime, and verifies that the handle count returns within a two-handle tolerance. Twenty additional CLI process runs were checked for consistent output, zero exit status, and no lingering `sako` process.

This is a bootstrap sanity check, not proof of leak freedom. Later phases still require Application Verifier or equivalent native diagnostics plus long-running, in-process memory and resource soak tests.
