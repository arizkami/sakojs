# Sako.js

A fast, memory-efficient JavaScript runtime powered by V8 and Rust.

Sako.js is Windows-first and designed around native asynchronous I/O, low runtime overhead, npm compatibility, and predictable memory usage.

Status: Experimental  
License: BSD-3-Clause

The Windows x64 V8 bootstrap and Phase 2 runtime are operational. Phase 3 module compatibility and the Phase 4 package manager remain incremental; [the roadmap](docs/roadmap.md) records their exact status. The repository expects a compatible prebuilt V8 artifact in `.deps/v8` and does not download or rebuild it.

```powershell
cargo run -p sako-cli -- tests/hello.js
```

Implemented experimental commands include `run`, `x`, `create`, `eval`, `repl`, `--version`, `--memory-stats`, `--detect-leaks`, `--workers=N|auto`, `install`, `add`, `remove`, and `update`. The short spellings the npm ecosystem shares work too -- `i`, `a`, `rm`, `up`, `r`, `exec` -- and `sako install <package>` adds a dependency the way `npm install <package>` does. Run `sako --help` for the table, or `sako help <command>` for one command's page and its aliases. JavaScript and parser-backed TypeScript entry files run directly; see [TypeScript support](docs/typescript.md), the [architecture](docs/architecture.md), [Node compatibility](docs/node-compatibility.md), and [package manager status](docs/package-manager.md) before relying on ecosystem behavior.

Sako ships its own libraries under the `sako:` scheme, written in TypeScript in [`libs/`](libs) and embedded in the executable, so there is nothing to install:

```ts
import { serve } from "sako:http";

serve((request) => new Response(`hello ${new URL(request.url).pathname}`), { port: 8080 });
```

[`sako:http`](libs/http) is the first: a handler takes a `Request` and returns a `Response`, on the same native Rust server `node:http` runs on. [`sako:psql`](libs/psql) is the second -- PostgreSQL over a Rust client built into the runtime, where a tagged template binds every interpolation as a parameter:

```ts
import { connect } from "sako:psql";

const db = await connect("postgres://sako@localhost/app");
const users = await db.query`select id, email from users where team = ${team}`;
```

Children are spoken to rather than waited for. `spawn` returns while the child runs, its output arrives as it is written, and writes to its stdin reach it at once -- which is what a build service needs, and what esbuild's JavaScript API uses to run through Sako.

Interactive command-line tools work: `process.stdin` is a real stream, raw mode is backed by `SetConsoleMode` and `tcsetattr`, and `node:readline` decodes terminal key sequences into keypresses, so a prompt library such as `@clack/prompts` behaves as it does under Node. The terminal is restored on every exit path.

Compiled Node-API addons load and run: the runtime implements the Node-API C surface itself and exports it from the executable, which is where a napi-rs binding resolves its symbols. Vite 8 serves its dev server and produces a production build on Sako through Rolldown's native binding, and `sako create next-app` scaffolds a Next.js project that then serves pages through Turbopack. See [native addons](docs/native-addons.md) for what differs from Node.

The repository also contains bounded overlapped IOCP, native `CreateProcessW`/Job Object process ownership, connection slabs, native HTTP/1 and TLS server bridges, bounded Web Fetch, diagnostics, and measured scalar/SSE4.2/AVX2 foundations. Express 4.21.2 serves through the partial Node HTTP layer, and `http`/`https` clients run over the same bounded transport as `fetch`; public raw TCP sockets remain unimplemented. The [roadmap](docs/roadmap.md) is authoritative.
