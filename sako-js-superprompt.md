# Sako.js — JavaScript Runtime Super Prompt

You are building **Sako.js**, a new high-performance JavaScript/TypeScript runtime written primarily in **Rust**, powered by **Google V8**.

Sako.js is **Windows-first**.

The primary goals are:

- Extremely high performance
- Extremely low memory overhead
- Stable long-running memory usage
- No runtime memory leaks
- Strong npm ecosystem compatibility
- Native Windows integration
- Minimal Rust ↔ V8 boundary overhead
- Native asynchronous I/O
- Aggressive optimization only where profiling proves it useful
- Clean, maintainable architecture
- BSD 3-Clause licensed

Sako.js does **not** claim to be the fastest JavaScript runtime in the world.

Its philosophy is:

> Fast where it matters.

and:

> Pay only for what you use.

The runtime must prioritize correctness and compatibility before benchmark numbers.

---

# 1. Core Technology

Primary language:

```text
Rust
```

JavaScript engine:

```text
Google V8
```

Prebuilt V8 binaries and headers are located at:

```text
.deps/v8/
```

Do NOT automatically download or rebuild V8.

The build system must discover and link against the existing V8 build located under:

```text
.deps/v8/
```

Expected structure may contain V8 headers, libraries, snapshot files, ICU data, and related runtime assets.

Implement detection instead of hardcoding one exact V8 build layout whenever practical.

Initial target:

```text
Windows 11 x86_64
MSVC ABI
```

Future targets:

```text
Windows ARM64
Linux x86_64
Linux ARM64
macOS ARM64
```

Do not implement Linux/macOS yet unless needed for abstractions.

Windows is the reference platform.

---

# 2. License

Use:

```text
BSD 3-Clause License
SPDX-License-Identifier: BSD-3-Clause
```

Add the appropriate SPDX identifier to first-party source files where appropriate.

Create:

```text
LICENSE
THIRD_PARTY_NOTICES.md
```

Sako itself is BSD-3-Clause.

Third-party licenses must remain intact.

Do not accidentally relicense V8 or dependencies.

---

# 3. Initial Repository Architecture

Design the repository approximately as:

```text
sako/
├── Cargo.toml
├── Cargo.lock
├── LICENSE
├── README.md
├── THIRD_PARTY_NOTICES.md
│
├── .deps/
│   └── v8/
│
├── crates/
│   ├── sako-cli/
│   ├── sako-runtime/
│   ├── sako-v8/
│   ├── sako-platform/
│   ├── sako-http/
│   ├── sako-net/
│   ├── sako-fs/
│   ├── sako-process/
│   ├── sako-node/
│   ├── sako-package/
│   ├── sako-accel/
│   └── sako-diagnostics/
│
├── runtime/
│   └── js/
│       ├── bootstrap.js
│       ├── globals.js
│       ├── modules/
│       └── node/
│
├── benchmarks/
│   ├── http/
│   ├── express/
│   ├── filesystem/
│   ├── startup/
│   └── memory/
│
├── tests/
│   ├── runtime/
│   ├── node/
│   ├── npm/
│   ├── http/
│   └── memory/
│
└── docs/
    ├── architecture.md
    ├── memory-model.md
    ├── node-compatibility.md
    └── benchmark-methodology.md
```

Keep crates reasonably separated.

Do not create hundreds of tiny crates without architectural benefit.

---

# 4. V8 Integration

Create a dedicated V8 abstraction layer.

Example:

```text
sako-v8
```

Responsibilities:

- V8 initialization
- platform initialization
- isolate creation
- isolate configuration
- context creation
- snapshot loading
- script compilation
- JavaScript execution
- module compilation
- promise/microtask integration
- exception handling
- stack traces
- native function registration
- external ArrayBuffer ownership
- persistent handle lifetime management
- isolate disposal

The rest of Sako should not directly scatter raw V8 calls throughout the codebase.

Prefer a controlled abstraction boundary.

---

# 5. V8 Memory Configuration

Sako must minimize unnecessary V8 memory use.

Do NOT aggressively allocate large heaps at startup.

Tune:

- initial heap size
- young generation sizing
- external memory reporting
- idle GC behavior
- snapshot usage

Do not force frequent garbage collections in production.

Expose memory statistics through runtime diagnostics.

Track separately:

```text
V8 heap used
V8 heap committed
native live allocations
external backing memory
buffer pools
connection slabs
RSS
private bytes
```

Remember:

```text
Virtual address reservation != physical RAM usage
```

---

# 6. Fundamental Memory Rule

Sako's core memory philosophy:

> Rust owns the data. V8 sees the data.

Avoid unnecessary copying into the V8 heap.

Examples:

```text
Network packet
    ↓
Rust-owned buffer
    ↓
native parser
    ↓
External ArrayBuffer / lazy accessor
    ↓
JavaScript only when requested
```

Do NOT eagerly convert every native structure into JavaScript objects.

---

# 7. No-Leak Requirement

Memory stability is a release requirement.

Rust memory safety does NOT automatically imply leak freedom.

Explicitly audit:

- `Arc` cycles
- global caches
- static collections
- unbounded HashMaps
- unbounded channels
- unbounded queues
- leaked boxes
- persistent V8 handles
- external backing stores
- callback registries
- timers
- async cancellation
- sockets
- child processes
- WebSockets
- HTTP keep-alive connections

Avoid:

```rust
Box::leak(...)
mem::forget(...)
```

unless extremely justified and documented.

---

# 8. V8 Persistent Handle Ownership

Any:

```text
v8::Global<T>
Persistent<T>
Function handle
Object handle
Promise resolver
```

must have explicit ownership.

When the owning runtime object is destroyed:

```text
Persistent handle → Reset / Drop
```

Do not leave handles inside abandoned routing tables, callback maps, timers, module caches, or workers.

Implement diagnostic counters for live persistent handles.

---

# 9. External ArrayBuffer Lifetime

Zero-copy buffers are desirable, but ownership must be exact.

Desired lifecycle:

```text
Rust Buffer
   ↓
V8 BackingStore
   ↓
JavaScript reference
   ↓
V8 GC
   ↓
BackingStore destructor/finalizer
   ↓
Return memory to pool or allocator
```

Never produce dangling pointers.

Never permanently retain buffers because JavaScript once referenced them.

External memory must be reported to V8 where relevant.

---

# 10. Native Memory Architecture

Use appropriate native memory structures such as:

```text
Request Arena
Response Arena
Connection Slab
Buffer Pool
Immutable Shared Buffers
Content-Addressed Cache
```

For temporary request memory:

```text
request starts
    ↓
allocate from request arena
    ↓
handle request
    ↓
send response
    ↓
arena.reset()
```

Avoid dozens of heap allocations per simple request.

For connections:

```text
ConnectionSlab
[slot 0]
[slot 1]
[slot 2]
...
```

Reuse slots rather than repeatedly allocating/deallocating connection objects.

---

# 11. Bounded Memory

No queue or cache may grow forever.

Every cache must have:

- maximum entries
- maximum bytes
- eviction policy

Every asynchronous queue must have:

- capacity
- backpressure behavior

When overwhelmed:

```text
apply backpressure
or
reject/shed work
```

Do NOT solve overload by allowing unlimited RAM growth.

---

# 12. Windows-First Platform Layer

Implement Windows natively.

Do not emulate Unix APIs unnecessarily.

Use:

```text
IOCP
Overlapped I/O
GetQueuedCompletionStatusEx
AcceptEx
ConnectEx
WSASend
WSARecv
CreateProcessW
Job Objects
Named Pipes
ConPTY where appropriate
BCrypt / CNG
Schannel where appropriate
ETW
```

Create a clean platform abstraction for future operating systems.

Example:

```rust
trait PlatformRuntime {
    fn poll(&mut self);
    fn wake(&self);
}
```

But do not over-engineer hypothetical Linux/macOS implementations before the Windows implementation works.

---

# 13. Event Loop

Sako should have its own runtime event-loop integration.

Conceptually:

```text
IOCP
  ↓
native completion queue
  ↓
timers
  ↓
native callbacks
  ↓
V8 JS callbacks
  ↓
V8 microtasks
  ↓
next poll
```

Minimize unnecessary executor layers.

Do not introduce Tokio across the entire runtime by default simply because it is convenient.

Tokio may be used selectively if benchmarks and architectural analysis justify it.

Prefer a purpose-built runtime reactor where possible.

---

# 14. JavaScript Runtime

Initial executable:

```bash
sako app.js
```

Also support:

```bash
sako run app.js
sako run dev
sako eval "console.log('hello')"
sako repl
sako --version
```

Implement basic globals:

```text
globalThis
console
process
Buffer
setTimeout
clearTimeout
setInterval
clearInterval
queueMicrotask
fetch
URL
URLSearchParams
TextEncoder
TextDecoder
AbortController
AbortSignal
```

Do not fake APIs.

Incomplete APIs should either:

- work correctly
- clearly return unsupported behavior
- or be excluded until implemented

---

# 15. ES Modules

Support standard ESM early.

Example:

```js
import express from "express";
import { readFile } from "node:fs/promises";
```

Implement:

```text
relative imports
absolute imports
package imports
node: specifiers
package.json type
exports
imports
conditional exports
```

Use V8 native module support.

---

# 16. CommonJS

npm compatibility requires CommonJS.

Support:

```js
const express = require("express");
```

Implement:

```text
require()
module
exports
__dirname
__filename
require.resolve()
```

Correctly bridge:

```text
CJS ↔ ESM
```

Do not implement module semantics through unsafe source transformations where native module support is more appropriate.

---

# 17. Node.js Compatibility Layer

Create:

```text
sako-node
```

Initial important modules:

```text
node:assert
node:buffer
node:console
node:events
node:fs
node:fs/promises
node:path
node:process
node:querystring
node:stream
node:string_decoder
node:timers
node:url
node:util
node:http
node:https
node:net
node:dns
node:crypto
node:child_process
node:os
```

Implement iteratively.

Track compatibility in:

```text
docs/node-compatibility.md
```

with:

```text
✅ supported
🟡 partial
❌ unsupported
```

Do not claim full Node compatibility prematurely.

---

# 18. npm Ecosystem Integration

npm integration is mandatory.

Sako is NOT creating a new isolated package ecosystem.

Sako should consume existing npm packages.

Commands:

```bash
sako install
sako add express
sako add react
sako remove lodash
sako update
```

Use the npm registry protocol.

Support:

```text
package.json
dependencies
devDependencies
optionalDependencies
peerDependencies
engines
workspaces
semver
dist-tags
integrity
scoped packages
private registries
authentication
```

---

# 19. Package Sources

Eventually support:

```text
npm:
file:
git:
github:
workspace:
```

Implement npm first.

Do not attempt every protocol simultaneously.

---

# 20. Package Cache

Build a native Rust package cache.

Suggested location on Windows:

```text
%LOCALAPPDATA%\Sako\Cache\
```

or:

```text
%LOCALAPPDATA%\Sako\Store\
```

Use a content-addressed structure.

Example:

```text
Store/
└── sha512/
    └── ab/
        └── abcdef...
```

Use package integrity information.

Avoid duplicate copies wherever possible.

Investigate:

- NTFS hardlinks
- efficient directory creation
- batched filesystem operations
- memory-mapped package indexes
- archive extraction efficiency

Do not assume filesystem optimization without benchmarks.

---

# 21. Lockfile

Create:

```text
sako.lock
```

Requirements:

- deterministic
- human-inspectable if practical
- fast to parse
- reproducible installs
- integrity information
- dependency graph information

Future import support:

```text
package-lock.json
yarn.lock
pnpm-lock.yaml
bun.lock
```

Do not make lockfile import a blocker for 0.1.

---

# 22. Lifecycle Scripts

npm packages may require:

```text
preinstall
install
postinstall
```

Support this carefully.

Provide:

```bash
sako install --ignore-scripts
```

Design script execution with security in mind.

Do not silently weaken npm package compatibility.

---

# 23. N-API / Native Addons

Long-term compatibility should target Node-API / N-API.

Do NOT attempt direct compatibility with unstable internal Node/V8 addon APIs first.

Architecture:

```text
addon.node
    ↓
Sako N-API compatibility layer
    ↓
Sako Runtime + V8
```

This is not required for the earliest bootstrapping stage but architecture must leave room for it.

---

# 24. Native HTTP Engine

Create a first-party HTTP implementation in Rust.

Goals:

- very low allocation
- low latency
- high throughput
- HTTP keep-alive
- pipelining where valid
- efficient header parsing
- buffer reuse
- vectored I/O
- minimal JS/native transitions

Architecture:

```text
TCP
 ↓
IOCP
 ↓
Rust HTTP parser
 ↓
NativeRequest
 ↓
JavaScript handler
 ↓
NativeResponse
 ↓
WSASend
```

Do NOT parse the entire request into JavaScript objects eagerly.

---

# 25. Lazy Request Materialization

Example request representation:

```text
NativeRequest
├── method
├── raw URL slice
├── header offsets
├── body slice
├── connection ID
└── native metadata
```

JavaScript:

```js
req.method
req.url
req.headers
```

Materialize values only when accessed.

Example:

```js
app.get("/", (_, res) => {
  res.send("pong");
});
```

This route should NOT require eagerly allocating a complete JS request headers object.

---

# 26. Native Response Fast Path

For simple responses:

```js
res.send("Hello");
```

Avoid unnecessary:

- string copies
- object allocations
- header maps
- intermediate buffers

Cache or precompute immutable response representations when semantics permit.

Never compromise correctness for benchmark tricks.

---

# 27. Express Compatibility Goal

A major target is:

```js
import express from "express";

const app = express();

app.get("/", (req, res) => {
    res.send("Hello World");
});

app.listen(3000);
```

The same source code should eventually run under:

```bash
sako app.js
```

without modification.

Start by making real Express work normally through Node-compatible HTTP APIs.

Do NOT monkey-patch Express specifically to fake benchmark numbers.

Future optimization may recognize common runtime patterns only when semantics remain identical.

---

# 28. Performance Targets

Targets are aspirational engineering objectives.

They are NOT guaranteed benchmark claims.

Initial goals:

```text
Sako.js 0.1.x
Express/plaintext target:
~400,000 req/s under an explicitly documented benchmark environment
```

Longer-term experimental goal:

```text
Sako.js 0.4.x
multi-core aggregate plaintext throughput:
~4,000,000 req/s
```

The 4M target should be treated primarily as a multicore scaling objective.

Do NOT attempt benchmark-specific cheating.

Document:

```text
CPU
RAM
Windows version
Sako version
Express version
connections
keep-alive
HTTP version
pipelining
TLS enabled/disabled
response size
benchmark duration
client hardware
worker count
```

---

# 29. Worker Architecture

Single runtime execution must remain understandable.

Default:

```bash
sako app.js
```

should not secretly create many V8 isolates unless explicitly designed and documented.

Optional multicore:

```bash
sako --workers=auto app.js
```

Architecture:

```text
IOCP
 │
 ├── Worker 0 → V8 isolate
 ├── Worker 1 → V8 isolate
 ├── Worker 2 → V8 isolate
 └── Worker N → V8 isolate
```

Prefer worker-local structures:

```text
worker-local arenas
worker-local connection state
worker-local buffers
worker-local caches
```

Avoid global mutexes in request hot paths.

---

# 30. SIMD and Assembly Acceleration

Sako may use architecture-specific acceleration.

Do NOT handwrite assembly blindly.

Optimization progression:

```text
correct scalar implementation
        ↓
profile
        ↓
Rust std::arch / SIMD intrinsics
        ↓
profile again
        ↓
handwritten assembly only if measurable benefit remains
```

Potential SIMD acceleration:

```text
HTTP delimiters
header parsing
URL scanning
UTF-8 validation
ASCII detection
WebSocket XOR masking
Base64
JSON structural scanning
hashing
PostgreSQL protocol parsing
```

x86_64 paths:

```text
Scalar
SSE4.2
AVX2
AVX-512 where beneficial
```

Future ARM64:

```text
Scalar
NEON
SVE/SVE2 if appropriate
```

Use runtime CPU feature detection.

---

# 31. Accelerator Architecture

Suggested structure:

```text
sako-accel/
├── scalar/
├── x86/
│   ├── sse42/
│   ├── avx2/
│   ├── avx512/
│   └── asm/
└── arm/
    ├── neon/
    └── sve/
```

Fallback must always exist.

Correctness tests must execute all available implementations against identical inputs.

---

# 32. Filesystem

Windows filesystem support must be treated seriously.

Support:

```text
C:\...
UNC paths
\\server\share
\\?\ extended paths
Unicode paths
junctions
symlinks
reparse points
NTFS case sensitivity
file locking behavior
```

Avoid Unix-centric path assumptions.

---

# 33. Process API

Implement process spawning using Windows-native APIs.

Use:

```text
CreateProcessW
STARTUPINFOEX
Job Objects
Named Pipes
```

Job Objects should be used where useful to prevent orphan processes.

Example:

```text
sako run dev
 ├── process A
 ├── process B
 └── process C

terminate parent
 ↓
terminate job
 ↓
all children exit
```

---

# 34. Diagnostics

Create developer diagnostics from early versions.

Example:

```bash
sako --memory-stats app.js
```

Possible output:

```text
Sako Memory
────────────────────────
RSS                   42 MB
Private bytes         38 MB

V8 heap:
  used                 7 MB
  committed           11 MB

Native:
  request arenas       1 MB
  network buffers      4 MB
  connection slabs     1 MB
  external memory      2 MB

Handles:
  persistent V8       35
  sockets             12
  timers               3
```

Exact formatting may change.

---

# 35. Leak Detection Mode

Provide a heavy development-only mode eventually:

```bash
sako --detect-leaks app.js
```

Track native runtime allocations/resources by component.

Potential categories:

```text
HTTP
WebSocket
FS
Timer
Process
V8 handle
External buffer
Module
Package cache
Worker
```

Do not impose heavy allocation tracking overhead in normal production mode.

---

# 36. Memory Soak Tests

Memory regression tests are mandatory.

Tests should include:

```text
1M HTTP requests
10M HTTP requests
WebSocket connect/disconnect loops
aborted HTTP requests
timeouts
invalid requests
large headers
cancelled streams
rejected promises
route hot reload
worker create/destroy
child process create/destroy
database connect/disconnect
```

After load finishes and temporary work settles:

```text
live memory should converge
```

It must not continuously grow every cycle.

Example desired pattern:

```text
42.1 MB
42.4 MB
42.2 MB
42.3 MB
42.4 MB
```

Bad:

```text
42 MB
49 MB
57 MB
66 MB
75 MB
```

A persistent monotonic increase must fail CI investigation.

---

# 37. Memory Performance Goals

Initial aspirational targets:

```text
Idle runtime:
as low as reasonably possible

Simple HTTP service:
preferably below ~50 MB RSS

Long-running service:
stable memory plateau

Leak:
0 known runtime leaks
```

Do not hardcode artificial heap limits merely to produce pretty benchmark screenshots.

Memory efficiency must come from architecture.

---

# 38. Benchmark Philosophy

Benchmarks must test reality.

Categories:

```text
startup
HTTP plaintext
JSON HTTP
Express
middleware
filesystem
package cache
npm install
WebSocket
memory
worker scaling
```

Compare where useful against:

```text
Node.js
Deno
Bun
```

But do not design APIs solely to win comparisons.

---

# 39. Cache Benchmark

Current experimental reference:

```text
Bun cache: ~210 ms
Sako cache prototype: ~75 ms
```

Treat this only as a preliminary observation.

Reproduce with proper methodology.

Test:

```text
cold OS cache
warm OS cache
hot process cache
large sequential files
many tiny files
1 MB
10 MB
100 MB
1 GB
10,000 files
100,000 files
```

Record:

```text
wall time
CPU time
read bytes
write bytes
I/O operations
peak memory
```

On Windows use ETW/WPA or equivalent tooling when deeper investigation is required.

---

# 40. Optimize I/O Before Assembly

Before accelerating cache/filesystem routines with SIMD or assembly:

measure whether the bottleneck is:

```text
NtCreateFile/CreateFileW
metadata traversal
filesystem cache
antivirus scanning
disk throughput
decompression
hashing
JavaScript materialization
```

Do not spend days optimizing a 2 ms CPU routine if 70 ms is filesystem latency.

---

# 41. Security

Treat npm packages and runtime inputs as untrusted.

Avoid:

- buffer overreads
- dangling native pointers
- unchecked lengths
- integer overflow in parser offsets
- unsafe UTF-8 assumptions
- path traversal
- archive extraction traversal
- unsafe lifecycle script behavior

Any `unsafe` Rust code must be:

- narrow
- documented
- justified
- tested

---

# 42. Unsafe Rust Policy

`unsafe` is allowed when required for:

```text
V8 FFI
Windows FFI
SIMD
assembly
zero-copy buffers
high-performance native data structures
```

But every unsafe block should have a clear safety invariant.

Example:

```rust
// SAFETY:
// `ptr` points to `len` initialized bytes owned by `BackingStore`.
// The backing store outlives every JS reference to this buffer.
unsafe {
    ...
}
```

Avoid giant unsafe modules.

---

# 43. CI

Initial Windows CI should run:

```text
cargo fmt --check
cargo clippy
cargo test
runtime tests
module tests
npm resolver tests
HTTP tests
memory sanity tests
```

Long-running soak benchmarks may execute in separate scheduled CI.

Do not require rebuilding V8 in normal CI if the binary artifact strategy can avoid it.

---

# 44. Development Rules

Do not:

- rewrite everything at once after every benchmark
- introduce speculative abstraction layers
- add dependencies without purpose
- optimize without profiling
- claim unsupported compatibility
- hide failing tests
- use benchmark-specific hardcoded responses
- knowingly accept memory leaks

Prefer:

```text
measure
identify bottleneck
change one subsystem
benchmark
verify correctness
verify memory
commit
```

---

# 45. Initial Development Milestone

First milestone:

```text
Sako.js boots V8 on Windows
```

Required:

1. Discover `.deps/v8/`
2. Link V8 successfully
3. Initialize V8 platform
4. Create isolate
5. Create context
6. Execute:

```js
console.log("Hello from Sako.js");
```

7. Exit cleanly
8. Dispose isolate
9. Dispose V8 platform
10. Verify no obvious native resource leaks

---

# 46. Second Milestone

Implement:

```bash
sako app.js
```

with:

```js
console.log("Sako.js");
```

Add:

```text
script loading
exception reporting
stack traces
basic timers
microtask execution
```

---

# 47. Third Milestone

Implement basic module support:

```js
import "./foo.js";
```

then:

```js
import { readFile } from "node:fs/promises";
```

---

# 48. Fourth Milestone

Implement package resolution:

```bash
sako install
```

Then run a simple pure-JavaScript npm package.

Target eventually:

```bash
sako add express
sako app.js
```

---

# 49. Fifth Milestone

Implement native HTTP and enough Node HTTP compatibility for Express.

Do not optimize Express before Express behaves correctly.

Then establish a baseline benchmark.

---

# 50. Sixth Milestone

Profile the runtime.

Investigate:

```text
Rust ↔ V8 crossings
allocation count
V8 object creation
HTTP parsing
IOCP polling
syscalls
header handling
response generation
```

Only after profiling, implement SIMD hot paths.

---

# 51. Coding Style

Rust should be:

- idiomatic
- explicit
- low-overhead
- readable
- strongly typed
- documented around unsafe boundaries

Prefer:

```rust
Result<T, SakoError>
```

over widespread panics.

Panics inside runtime internals should be considered bugs except for proven unreachable invariants.

---

# 52. Error Handling

Errors exposed to JavaScript should become meaningful JS exceptions.

Examples:

```text
TypeError
RangeError
SyntaxError
Error
system error
```

Include Windows error information where useful without leaking implementation garbage to normal users.

---

# 53. Runtime Startup Output

Normal runtime should stay quiet.

Debug mode may show:

```text
Sako.js 0.1.0
V8: <version>
Platform: Windows x64
I/O: IOCP
CPU acceleration: AVX2
```

Do not print banners during ordinary script execution unless explicitly requested.

---

# 54. README Positioning

Use wording similar to:

```text
# Sako.js

A fast, memory-efficient JavaScript runtime powered by V8 and Rust.

Sako.js is Windows-first and designed around native asynchronous I/O,
low runtime overhead, npm compatibility, and predictable memory usage.

Status: Experimental
License: BSD-3-Clause
```

Do NOT claim:

```text
fastest runtime in the world
```

unless independently demonstrated and intentionally approved later.

---

# 55. Guiding Principles

Keep these principles visible throughout development:

```text
Correctness before benchmark numbers.

Compatibility before ecosystem fragmentation.

Rust owns data; V8 sees data.

Avoid allocations before optimizing allocations.

Avoid I/O before accelerating I/O.

No unbounded caches.

No unbounded queues.

Every native resource has an owner.

Every persistent V8 handle has a lifetime.

Memory must converge after load.

Use SIMD where it helps.

Use assembly only where profiling proves it helps.

Windows is a first-class platform, not a compatibility layer.

No benchmark-specific hacks.
```

---

# 56. Immediate Task

Start implementing the repository now.

First inspect the existing project tree.

Do not destroy existing working code.

Then inspect:

```text
.deps/v8/
```

Determine:

- include paths
- libraries
- V8 build configuration where detectable
- debug/release state
- snapshot availability
- ICU availability
- DLL/static linkage requirements

Create the minimum clean Rust/V8 integration required to boot V8.

Do not begin HTTP, npm, Express, SIMD, or package-manager work until the V8 bootstrap is proven stable.

The first successful acceptance test is:

```bash
cargo run -p sako-cli -- tests/hello.js
```

where:

```js
console.log("Hello from Sako.js");
```

prints:

```text
Hello from Sako.js
```

and exits with code:

```text
0
```

After this works:

1. document the V8 linkage strategy
2. add tests
3. commit the bootstrap architecture
4. proceed to script execution and module loading

Do not skip directly to optimization.

Build the foundation correctly first.
