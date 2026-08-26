# Sako libraries

Libraries Sako ships inside the executable, written in TypeScript and reached
through the `sako:` scheme:

```ts
import { serve } from "sako:http";
```

They are not npm packages and there is nothing to install. `libs/<name>/src/index.ts`
is transpiled at build time and embedded in the binary, so `sako:<name>` is
resolvable in every program the runtime runs, with no `node_modules` involved.

| Library | Specifier | What it is |
| --- | --- | --- |
| [http](http) | `sako:http` | A web-standard HTTP server on the native Rust transport |
| [psql](psql) | `sako:psql` | PostgreSQL, over a Rust client built into the runtime |

## Where this sits next to `node:`

`node:http` exists so the ecosystem written against Node runs on Sako, and it
will keep existing. `sako:http` is the interface for code being written now:
`Request` in, `Response` out, no `(req, res)` pair to finish by hand. Both go
through the same overlapped-IOCP server in `crates/sako-http`, so choosing one
over the other costs nothing at the transport.

## Adding a library

1. Create `libs/<name>/src/index.ts`. Its exports are what `sako:<name>`
   exports, and its default export is `sako:<name>`'s default.
2. Add a `libs/<name>/README.md` describing it, and a row to the table above.
3. Rebuild. `crates/sako-v8/build.rs` finds the directory, transpiles the entry
   with the same parser that runs `.ts` entry files, and embeds the result --
   so a library that does not parse fails the build rather than the program.

Two rules the embedding imposes:

- **One file per library.** `src/index.ts` is the whole library; there is no
  module resolution inside the `sako:` scheme for a relative import to use.
- **No dependencies.** A library may use the runtime's globals and other
  `sako:` libraries, and nothing else. These load before any program does,
  from a binary with no package manager attached to it.

Libraries are compiled the first time a program imports them and cached for
the rest of that run, so one that nobody imports costs nothing but the bytes
it occupies in the executable.
