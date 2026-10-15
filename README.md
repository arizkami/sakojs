# sako

A small, fast JavaScript runtime written in Rust on top of V8, linked from
Deno's prebuilt [`rusty_v8`](https://github.com/denoland/rusty_v8) static
library (no V8 compile needed).

```bash
cargo build --release
./target/release/sako examples/hello.js
cargo test --release
```

```js
// server.js: run with `sako server.js`
Sako.serve({
  port: 3000,
  routes: {
    "/": new Response("Hello World!"),         // static: answered in Rust, no JS
    "/users/:id": (req) => Response.json({ id: req.params.id }),
    "/echo": { POST: async (req) => Response.json(await req.json()) },
  },
  fetch: () => new Response("Not Found", { status: 404 }),
});
```

## Why it's fast

- **Tiny startup.** One isolate from V8's built-in snapshot plus a bootstrap
  compiled lazily. No Node compatibility layer and no TypeScript toolchain
  to load. Thread pools, the HTTP client and the HTTP server start only when
  first used.
- **On-disk code cache.** ES modules are compiled with V8 code cache stored
  in `~/.cache/sako/v8-<tag>/`, so later runs skip parsing and bytecode
  generation. The cache is written after evaluation, so it includes functions
  that ran lazily.
- **Buffered stdout.** `console.log` writes UTF-8 straight from V8 into a
  64 KB in-process buffer. It is flushed when full, when the event loop goes
  idle, before stderr output, and at exit. Output to a TTY is flushed on
  every write.
- **Lean event loop.** Timers sit in a Rust binary heap and callbacks stay on
  the JS side. Immediates have their own phase and are not run as timers.
  `setTimeout(fn, 0)` is not clamped to 1 ms. Every loop turn runs in its own
  V8 handle scope, so long-running servers don't accumulate handles.
- **Async I/O on a thread pool.** File system calls and `fetch` run on worker
  threads that report back over a channel. The loop waits on that channel
  and on timers at the same time. Threads spin for about 50 µs before
  parking, which avoids a wake-up round trip for fast operations.
- **HTTP server, thread per core.** Each I/O thread runs its own
  single-threaded Tokio runtime with hyper and its own `SO_REUSEPORT`
  listener, so there is no work stealing between threads.
  - Requests reach the JS thread in batches, with one call into JS per batch.
  - Replies return through one queue per I/O thread, so a burst of responses
    costs a single wake-up.
  - Static `Response` routes are answered on the I/O threads without
    touching JS.
  - Request headers are built only when read. String bodies go straight
    from V8 to the socket buffer.
- **mimalloc.** Buffers allocated on one thread and freed on another made
  glibc's malloc arena locks a hotspot, so mimalloc is the global allocator.
- **HTTP client.** `fetch` uses [ureq](https://github.com/algesten/ureq) with
  rustls and gzip, and keeps up to 256 idle connections per host.
- **Release profile.** Fat LTO, `codegen-units = 1`, `panic = "abort"`.

## Benchmarks

Linux x86-64, 12 cores. Node is v24.19 and Bun is 1.3.14. Timings on this
machine vary by about ±10% between runs.

### Runtime

Run `python3 bench/run.py 15`. Each number is the median wall-clock time in
ms, including process startup. Lower is better. `sako-nc` means
`--no-cache`.

| bench | sako | sako-nc | node | bun |
|---|--:|--:|--:|--:|
| `empty.js` (startup) | **5.6** | 5.3 | 15.4 | 7.7 |
| `big.js` (2.5 MB module) | **20.0** | 53.1 | 65.0 | 79.9 |
| `compute.js` (JIT-bound) | 232 | 221 | 249 | 236 |
| `console.js` (200k lines → /dev/null) | **36.3** | 34.0 | 259.2 | 67.5 |
| `async.js` (300k awaits + 20k `setImmediate`) | **38.9** | 32.8 | 57.6 | 50.3 |
| `fs.js` (2000 async file writes + reads) | **20.1** | 20.1 | 157.1 | 33.5 |
| `fetch.js` (2500 local HTTP GETs) | 49.3 | 60.3 | 855.8 | **46.1** |

### HTTP server: Sako vs Bun.serve vs Elysia

Run `python3 bench/http/run.py 10 100`. You need `wrk` and
`cd bench/http && bun install` first.

The three servers are `bench/http/sako.js`, `bun.js` and `elysia.js`. They
implement the same four endpoints with each framework's router. The `/`
route is a static `Response` in Sako and Bun, and Elysia compiles its `/`
to a static response as well. The load generator is
`wrk -t4 -c100 -d10s` on the same machine, after a 2s warm-up per
endpoint.

**Throughput.** Requests/sec, with p99 latency and server CPU cores used.
Higher is better.

| endpoint | sako | sako, 1 I/O thread | Bun.serve | Elysia |
|---|--:|--:|--:|--:|
| `GET /` static text | **474k** (2.3ms, 2.9c) | 251k (1.7ms, 1.0c) | 273k (1.6ms, 1.0c) | 278k (1.7ms, 1.0c) |
| `GET /json` | **380k** (2.6ms, 3.8c) | 208k (1.8ms, 1.6c) | 214k (2.6ms, 1.0c) | 197k (3.2ms, 1.0c) |
| `GET /id/:id?name=` | **348k** (2.3ms, 3.6c) | 192k (2.4ms, 1.6c) | 157k (2.8ms, 1.0c) | 156k (2.9ms, 1.0c) |
| `POST /echo` (JSON) | **270k** (2.8ms, 3.1c) | 183k (2.3ms, 1.7c) | 132k (4.2ms, 0.9c) | 123k (3.3ms, 1.0c) |

**Efficiency.** Requests per CPU-second. Higher is better.

| endpoint | sako | sako, 1 I/O thread | Bun.serve | Elysia |
|---|--:|--:|--:|--:|
| `GET /` static text | 163k | 259k | 281k | **288k** |
| `GET /json` | 101k | 132k | **216k** | 202k |
| `GET /id/:id?name=` | 97k | 124k | 158k | **158k** |
| `POST /echo` (JSON) | 86k | 108k | **141k** | 125k |

How to read this:

- **Raw throughput:** Sako has the highest raw throughput on every endpoint,
  1.7×–2.2× Bun and Elysia.
- **Where that comes from:** part of it is parallelism. By default Sako
  parses HTTP on several I/O threads (cores/4, set with
  `SAKO_HTTP_THREADS`), while Bun serves from a single thread.
- **With one I/O thread:** Sako still beats both on the dynamic and POST
  routes, by 1.2×–1.5×. It ties Bun on JSON and loses about 10% on static
  text.
- **Per CPU-second:** Bun and Elysia are about 1.3–1.6× more efficient. Each
  dynamic request crosses threads twice (I/O thread → JS thread → I/O
  thread), while Bun does socket I/O on its JS thread. Running hyper on the
  JS thread's own event loop would close most of that gap.

## Usage

```
sako [options] <file.js> [args...]
sako -e <code>        evaluate code as an ES module
sako -p <expr>        evaluate and print the result
sako                  REPL (or run stdin when piped)

--no-cache            disable the V8 code cache (or SAKO_NO_CACHE=1)
--v8-flags=a,b        pass flags to V8 (or SAKO_V8_FLAGS="--a --b")
```

Entry files are always ES modules, so top-level `await` works.

| Environment variable | Effect |
|---|---|
| `SAKO_HTTP_THREADS` | number of HTTP I/O threads (default: cores/4, min 1) |
| `SAKO_CACHE_DIR` | code cache location (default `~/.cache/sako`) |
| `SAKO_NO_CACHE=1` | disable the code cache |

## APIs

- **`Sako.serve(options)`**, modelled on `Bun.serve`:
  - **Options:** `port` (default `$PORT` or 3000; 0 picks a free port),
    `hostname` (default `0.0.0.0`), `fetch(req, server)`, `routes`,
    `error(err)` and `reusePort`.
  - **`routes` values:** a `Response` (static, served from Rust), a handler,
    or `{ GET, POST, ... }`. Patterns support `:params` (as `req.params`)
    and a trailing `*` wildcard. Precedence is exact match, then params,
    then wildcard, then `fetch`.
  - **Handlers:** may be async. An uncaught error is passed to `error()` if
    given; otherwise it is logged and the client gets a 500.
  - **The returned server** has `port`, `hostname`, `url`, `stop()` and
    `reload({ fetch, routes, error })`.
  - **Port conflicts:** binding a port that's in use throws `EADDRINUSE`
    unless `reusePort: true`.
- **Modules:** relative and absolute paths, `file://` URLs, `.js`, `.mjs`
  and `.json` files, extension and `index.js` resolution, dynamic `import()`,
  `import.meta.{url,filename,dirname,main}`, and bare specifiers resolved
  from `node_modules` (`exports`, `module`, `main`).
- **Web:**
  - `fetch` with `Request`, `Response` and `Headers`. `redirect` can be
    `follow`, `manual` or `error`. Response methods are `text`, `json`,
    `arrayBuffer` and `bytes`, plus `Response.json`, `Response.redirect` and
    `clone`. A minimal `body` reader is included.
  - `URL` and `URLSearchParams`, which follow the WHATWG URL standard via
    the `url` crate and produce output identical to Node's.
  - `EventTarget`, `Event`, `AbortController` and `AbortSignal` (including
    `timeout` and `any`).
  - `TextEncoder`, `TextDecoder`, `atob`, `btoa`, `structuredClone`,
    `queueMicrotask`, `performance.now` and `DOMException`.
- **Timers:** `setTimeout`, `setInterval` and `setImmediate`, and their
  `clear*` functions. `process.nextTick` callbacks run before promise jobs,
  as in Node's documented rule.
- **`console`:** `log`, `error`, `table`, `time`, `count`, `group`, ... The
  formatting is a port of Node's `util.inspect`.
- **`process`:** `argv`, `env`, `exit`, `exitCode`, `cwd`, `chdir`,
  `hrtime`, `nextTick`, `memoryUsage`, `stdout.write`, `stderr.write`,
  `platform`, `versions`.
- **`Sako` file functions:** `readTextFile`, `readFile`, `writeTextFile`,
  `writeFile`, `exists`, `readDir`, `mkdir`, `remove`, `stat`. Each returns
  a promise and runs on the thread pool. Each also has a `*Sync` variant.
  `Sako` also has `serve`, `args`, `inspect`, `format` and `version`.

Errors behave like Node and Deno. An uncaught exception or an unhandled
rejection exits with code 1. A top-level `await` that never settles exits
with code 13. Unref'd timers, such as `AbortSignal.timeout()`, don't keep
the process alive.

### Known differences from Node and Bun

- `setTimeout(fn, 0)` really is 0 ms, so it can fire before a
  `setImmediate` queued at the same time.
- At module top level, `process.nextTick` runs before promise callbacks.
  Node ESM runs it after.
- `%o` in `console.log` does not show hidden properties such as `[length]`.
- An aborted `fetch` rejects immediately, but the HTTP request already in
  flight is not cancelled. Response bodies, for `fetch` and for the server,
  are buffered in full rather than streamed.
- `server.stop()` closes keep-alive connections that are still open.

## Not implemented (yet)

HTTPS and HTTP/2 serving, WebSocket, streaming bodies (`ReadableStream`),
`Blob`, `FormData`, CommonJS `require`, `node:` built-in modules,
TypeScript, and workers.

## Tests

`cargo test --release` runs each `tests/fixtures/*.js` with the built binary
and compares stdout with the matching `.out` file. It also checks the exit
code and stderr. Every fixture runs twice, first with a cold code cache and
then with a warm one. The fetch fixture uses a small HTTP/1.1 server started
inside the test process. The serve fixture exercises `Sako.serve` against
itself. To regenerate expectations, run
`SAKO_UPDATE_EXPECT=1 cargo test --release`, then review the diff.

## Layout

- `src/main.rs`: CLI, isolate setup, module loader, code cache, event loop
  (timers, I/O completions, HTTP batches, immediates, nextTick), and error
  reporting.
- `src/ops.rs`: native functions exposed to JS (stdout, timers, encoding,
  fs, fetch, URL, server).
- `src/pool.rs`: worker thread pool, event channel, op results and their
  conversion to JS.
- `src/http.rs`: `Sako.serve` I/O threads (Tokio + hyper, `SO_REUSEPORT`)
  and static routes.
- `src/bootstrap.js`: JS globals. It must stay ASCII because it is loaded
  as an external one-byte string, and a compile-time check enforces this.
- `bench/`: runtime benchmarks and `run.py`.
- `bench/http/`: HTTP server benchmark (Sako, Bun.serve, Elysia) and
  `run.py`.
- `tests/`: fixture-based integration tests.
