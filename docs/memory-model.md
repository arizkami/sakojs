# Memory model

Sako applies three rules to every native subsystem: one explicit owner, a configured capacity, and deterministic teardown.

V8 contexts, modules, CommonJS records, synthetic exports, timer callbacks, and callback arguments use persistent handles owned by one runtime. Runtime destruction resets each handle before disposing its isolate. The process-wide V8 platform outlives isolates and is disposed once at process shutdown.

Module source retention is capped at 64 MiB and 4,096 modules per runtime. Timers are capped at 65,536 and runtime-owned file descriptors at 1,024. Package metadata, archives, extraction size and entry count, package graph size, and the content store all have explicit limits. IOCP posts, pending operations, retained completions, worker count, connection slots, and pooled buffers are similarly bounded.

TLS reuses each HTTP connection's bounded plaintext input, response, and wire
buffers. Rustls plaintext buffering is capped at the configured maximum response
size, and retained encrypted output is rejected above that size plus 256 KiB of
record and handshake allowance.

HTTP request heads borrow their input buffer and store only method, target, and header byte ranges inside Rust. The V8 bridge transfers one bounded owned header-byte buffer plus numeric ranges and an owned body-byte buffer. JavaScript header strings/maps are decoded on first property access, and a body `Buffer` is created only when a data listener consumes it.

`sako --memory-stats` reports RSS, private bytes, OS handles, V8 heap usage/commit/limit, V8 external memory, conservative native-owned capacity, persistent handles, timers, live HTTP servers and sockets, queued shutdown operations, module entries, and retained HTTP response-buffer capacity. Component counters remain available for future subsystems.
