# Sako.js

A fast, memory-efficient JavaScript runtime powered by V8 and Rust.

Sako.js is Windows-first and designed around native asynchronous I/O, low runtime overhead, npm compatibility, and predictable memory usage.

Status: Experimental  
License: BSD-3-Clause

The Windows x64 V8 bootstrap and Phase 2 runtime are operational. Phase 3 module compatibility and the Phase 4 package manager remain incremental; [the roadmap](docs/roadmap.md) records their exact status. The repository expects a compatible prebuilt V8 artifact in `.deps/v8` and does not download or rebuild it.

```powershell
cargo run -p sako-cli -- tests/hello.js
```

Implemented experimental commands include `run`, `eval`, `repl`, `--version`, `--memory-stats`, `--detect-leaks`, `--workers=N|auto`, `install`, `add`, `remove`, and `update`. JavaScript and parser-backed TypeScript entry files run directly; see [TypeScript support](docs/typescript.md), the [architecture](docs/architecture.md), [Node compatibility](docs/node-compatibility.md), and [package manager status](docs/package-manager.md) before relying on ecosystem behavior.

Compiled Node-API addons load and run: the runtime implements the Node-API C surface itself and exports it from the executable, which is where a napi-rs binding resolves its symbols. Vite 8 serves its dev server and produces a production build on Sako through Rolldown's native binding. See [native addons](docs/native-addons.md) for what differs from Node.

The repository also contains bounded overlapped IOCP, native `CreateProcessW`/Job Object process ownership, connection slabs, native HTTP/1 and TLS server bridges, bounded Web Fetch, diagnostics, and measured scalar/SSE4.2/AVX2 foundations. Express 4.21.2 serves through the partial Node HTTP layer; public raw TCP sockets and Node HTTP/HTTPS clients remain unimplemented. The [roadmap](docs/roadmap.md) is authoritative.
