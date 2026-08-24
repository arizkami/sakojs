# TypeScript support

Sako transpiles TypeScript directly before V8 compilation. The same bounded native path handles entry files, static imports, dynamic imports, package targets, and CommonJS `require` resolution.

Supported runtime extensions are `.ts`, `.mts`, `.cts`, and `.tsx`. `.mts` is always ESM, `.cts` is always CommonJS, and `.ts`/`.tsx` follow the nearest `package.json` `type` field just like `.js`. Extensionless relative resolution checks JavaScript before TypeScript so adding TypeScript support does not change an existing JavaScript import.

The transpiler handles type annotations, interfaces, type-only imports/exports, enums, namespaces, `satisfies`, parameter properties, and classic React-style TSX. Input is limited to 16 MiB per source and emitted JavaScript to 32 MiB. Parsed output participates in the existing bounded module cache.

This is transpile-only support. Sako does not type-check, read `tsconfig.json`, resolve `paths` aliases, apply custom JSX factories, emit declaration files, or execute `.d.ts`/`.d.mts`/`.d.cts`. Legacy TypeScript decorator settings and source-mapped runtime stack traces are not implemented. Use `.mts` or `{"type":"module"}` for TypeScript containing ESM syntax; use `.cts` or a CommonJS package scope for `require` and `module.exports`.
