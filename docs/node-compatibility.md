# Node.js compatibility

Sako.js compatibility is experimental and intentionally reported per API. A module not listed as supported should be treated as unavailable.

| Area | Status | Current behavior |
| --- | --- | --- |
| CommonJS wrapper | Partial | `exports`, `module`, `require`, `require.resolve`, `__filename`, and `__dirname` work for local and installed JavaScript packages. |
| CommonJS resolution | Partial | Relative, absolute, JSON, directory `main`, directory `index`, scoped package, and bare `node_modules` lookup work. Package `exports` and `imports` do not. |
| ESM | Partial | V8-native compilation/linking/evaluation and relative or absolute imports work. Dynamic import, package imports, and CJS interoperability do not. |
| `node:assert` | Partial | CommonJS `require` supports callable `assert`, `assert.ok`, and `assert.strictEqual`. ESM import is unsupported. |
| `node:buffer` | Unsupported | Import/require fails explicitly. |
| `node:console` | Partial | Global `console.log` and CommonJS `require("node:console")` work. Other console methods and ESM import are unsupported. |
| `node:events` | Unsupported | Import/require fails explicitly. |
| `node:fs` | Unsupported | Import/require fails explicitly. |
| `node:fs/promises` | Unsupported | Import/require fails explicitly. |
| `node:path` | Unsupported | Import/require fails explicitly. |
| `node:process` | Partial | Global and CommonJS module `process.argv`, `platform`, `arch`, and `versions` work. Other APIs and ESM import are unsupported. |
| `node:timers` | Partial | Global and CommonJS module bounded timeout/interval scheduling and cancellation work. ESM import is unsupported. |
| `node:url` | Unsupported | Import/require fails explicitly. |
| `node:util` | Unsupported | Import/require fails explicitly. |

All other Node modules, native addons, and N-API are unsupported at this stage.
