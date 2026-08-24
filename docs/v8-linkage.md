# V8 linkage strategy

Phase 1 consumes the prebuilt V8 artifact in `.deps/v8`; it never downloads or rebuilds V8.

The `sako-v8` build script locates the artifact at `SAKO_V8_ROOT` when set and otherwise uses `<workspace>/.deps/v8`. It fails early unless it finds `include/v8.h`, `include/libplatform/libplatform.h`, `lib/v8_monolith.lib`, and `bin/icudtl.dat`. It also verifies the supported V8 major version and the Windows x64 MSVC target.

The supplied artifact is V8 14.9.0.0, release mode, built as a static monolith using the static MSVC runtime. The monolith contains the startup snapshot; no external `snapshot_blob.bin` is present. ICU is external and loaded from `bin/icudtl.dat` before V8 platform initialization.

This particular monolith also contains d8's process-wide Windows `malloc` shim. The build script uses the MSVC librarian to create a target-local `v8_embedder.lib` with only `allocator_shim_win_static.obj` removed. This keeps V8's internal PartitionAlloc code but prevents the runtime library from replacing the host process allocator. The source artifact in `.deps/v8` is never modified.

Rust does not bind the C++ V8 ABI directly. `bridge.cc` is compiled as C++20 with pointer compression enabled and the sandbox disabled to match this artifact, then exposes a small C ABI to Rust. `sako-v8` is the sole owner of this boundary. V8 platform, allocator, isolate, scopes, and context teardown remain explicit in the bridge.

Redistribution packaging is separate from development linkage. A packaged runtime will need a relocatable ICU lookup/copy strategy and must preserve V8 and third-party license notices.

## Phase 1 shutdown check

The `sako-v8` lifecycle integration test records the Windows process handle count, initializes V8, executes JavaScript and a microtask checkpoint, drops the runtime, and verifies that the handle count returns within a two-handle tolerance. Twenty additional CLI process runs were checked for consistent output, zero exit status, and no lingering `sako` process.

This is a bootstrap sanity check, not proof of leak freedom. Later phases still require Application Verifier or equivalent native diagnostics plus long-running, in-process memory and resource soak tests.
