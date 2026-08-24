# Sako.js

A fast, memory-efficient JavaScript runtime powered by V8 and Rust.

Sako.js is Windows-first and designed around native asynchronous I/O, low runtime overhead, npm compatibility, and predictable memory usage.

Status: Experimental  
License: BSD-3-Clause

The current Phase 1 target is the Windows x64 V8 bootstrap described in [the roadmap](docs/roadmap.md). The repository expects a compatible prebuilt V8 artifact in `.deps/v8` and does not download or rebuild it.

```powershell
cargo run -p sako-cli -- tests/hello.js
```
