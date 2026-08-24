# Architecture

Sako keeps V8 API calls inside `sako-v8`. A process-wide engine owner initializes and eventually disposes the V8 platform; each runtime owns its allocator, isolate, context, timers, module caches, synthetic-module values, and persistent handles. Multiple worker-local isolates can coexist without sharing JavaScript state.

The embedded `runtime/js/bootstrap.js` owns compatibility APIs that do not require native resource access. Build-time generation embeds it into the native bridge. Filesystem bytes, UTF-8 conversion, clocks, and V8 handles remain native operations with explicit limits.

Two runtime-wide policies shape hot paths. The event loop never sleeps while an HTTP server is live: it blocks inside that server's completion port, bounded by the next timer deadline, so an arriving request wakes it immediately instead of at the next system timer tick. Large ArrayBuffer storage is pooled: the isolate's allocator retains a few freed blocks of at least 1 MiB, capped at 32 MiB, and reuses them for uninitialized allocations so a repeated whole-file read does not re-fault fresh pages. Zero-initialized allocations bypass the pool because fresh pages arrive zeroed and fault in lazily. Setting `SAKO_V8_FLAGS` passes flags to V8 before initialization for experiments.

Platform components are separated by ownership:

- `sako-platform`: owned IOCP handle, stable overlapped file/socket operations, bounded retained completions, cancellation/drain ownership, and reuse pools for I/O buffers and overlapped records so a submission allocates nothing.
- `sako-process`: direct `CreateProcessW`/`STARTUPINFOEX` launch, explicit inherited standard handles, named capture pipes, and kill-on-close Job Object ownership.
- `sako-typescript`: bounded SWC-backed parsing and JavaScript emission for TypeScript sources before V8 compilation.
- `sako-net`: fixed-capacity generation-safe connection slabs and buffer pools, bounded TCP acceptance, completion-driven overlapped receive/send against the connection's own socket, one completion drain per event-loop turn rather than per call, cancellation before slot reuse, and bounded DNS results.
- `sako-http`: byte-range request parsing, bounded response encoding, owned HTTP/1 servers, and rustls TLS 1.2/1.3 records over the same IOCP transport. A tick reads, answers up to 64 pipelined requests, and flushes in one pass, reusing the connection's parse and response buffers and stopping once queued output reaches the response limit.
- `sako-package`: registry resolution, integrity store, graph installation, and lock replay.
- `sako-diagnostics`: process and component resource counters.
- `sako-accel`: scalar and runtime-selected architecture-specific primitives.

V8 HTTP/HTTPS server bindings and Express 4 compose the bounded native owners. Web Fetch uses a separate bounded synchronous native client; raw public sockets and Node HTTP/HTTPS client APIs remain future compatibility work and must reuse the reactor rather than introduce an unbounded executor or queue.
