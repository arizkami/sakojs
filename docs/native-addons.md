# Native addons (Node-API)

Sako loads compiled Node-API addons. `require` of a `.node` file opens it as a
shared library, calls its `napi_register_module_v1` entry point, and uses
whatever that returns as `module.exports`.

The implementation is Sako's own, in `crates/sako-v8/src/napi.cc`. Node-API is
a C ABI rather than a V8 one — an addon never sees a V8 header — which is what
makes it implementable outside Node at all. The headers under
`crates/sako-v8/include/node` are copied unmodified from Node so the ABI is the
real one rather than a reconstruction of it.

## How an addon finds the runtime

On Windows the `napi_*` functions are exported from `sako.exe` itself. Addons
built with napi-rs resolve them with `GetProcAddress` against the process that
loaded them, so a host that is not named `node.exe` works without any
cooperation from the addon. An addon built by node-gyp instead *imports* from
`node.exe` and will not load here; rebuilding it against Sako's import library
(`sako.lib`, emitted beside the executable) is what makes it work.

Building an addon against Sako directly looks like the test fixture:

```powershell
clang-cl /LD /MT /I crates\sako-v8\include\node -DBUILDING_NODE_EXTENSION `
  addon.c /link target\release\deps\sako.lib /OUT:addon.node
```

## What is implemented

The full Node-API 9 surface, minus nothing an addon can reasonably ask for:
values and conversions, objects and properties, functions, classes,
`napi_wrap`/`napi_unwrap` and type tags, references and handle scopes,
errors and exception state, ArrayBuffers, typed arrays, DataViews, buffers,
promises, bigints, dates, instance data, environment cleanup hooks,
`napi_create_async_work` on a bounded worker pool, and threadsafe functions
callable from any thread.

Finalizers do not run inside garbage collection. A weak callback may not touch
the heap and addon finalizers routinely do, so collection only queues them and
the event loop runs them on the next turn. Async work completions and
threadsafe calls arrive the same way, which is why an addon holding either one
keeps the process alive: `sako_napi::HasPendingWork` is part of the loop's exit
condition.

## What differs from Node

**External buffers are copied.** `napi_create_external_buffer` and
`napi_create_external_arraybuffer` copy the caller's bytes instead of adopting
them. V8's sandbox requires every ArrayBuffer's storage to live inside the
sandbox address space and aborts the process when handed a pointer from
anywhere else, so a genuinely external backing store does not exist in this
build. The finalizer still runs when JavaScript is done with the value, so an
addon's memory has exactly the lifetime it was promised; what is lost is the
zero copy.

**`napi_get_uv_event_loop` returns a placeholder.** Sako has no libuv, and an
addon cannot reach one either — nothing exports `uv_*`, so there is no symbol
to resolve. The pointer is stable and non-null because addons store it and
hand it back to APIs they cannot call, and a null one reads as an assertion
failure to some of them.

**`process.versions.node` names an API level, not an identity.**
`process.versions.sako` and `process.release.name` are where the truth lives.

**Async hooks are absent.** `napi_async_init`, `napi_async_destroy`, and the
callback scopes exist and are honoured as bookkeeping, but nothing observes
them.

## Tested against

Vite 8 runs its dev server and its production build on Sako, which means
Rolldown's native binding — a large napi-rs addon using async work, threadsafe
functions, buffers, and external memory — loads and does real work. oxlint,
another napi-rs binding, runs. `crates/sako-cli/tests/cli.rs` builds the addon
in `tests/addon` against the executable and exercises the same paths directly.
