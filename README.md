# Sako.js

A fast, memory-efficient JavaScript runtime powered by V8 and Rust.

Sako.js is Windows-first and designed around native asynchronous I/O, low runtime overhead, npm compatibility, and predictable memory usage.

Status: Experimental  
License: BSD-3-Clause

The Windows x64 V8 bootstrap and Phase 2 runtime are operational. Phase 3 module compatibility and the Phase 4 package manager remain incremental; [the roadmap](docs/roadmap.md) records their exact status. The repository expects a compatible prebuilt V8 artifact in `.deps/v8` and does not download or rebuild it.

```powershell
cargo run -p sako-cli -- tests/hello.js
```

Implemented experimental commands include `run`, `eval`, `repl`, `--version`, `install`, `add`, `remove`, and `update`. See [Node compatibility](docs/node-compatibility.md) and [package manager status](docs/package-manager.md) before relying on ecosystem behavior.
