# `sako:http`

A web-standard HTTP server on Sako's native Rust transport. A handler takes a
`Request` and returns a `Response`; the runtime owns the connection, the
parsing, and the writing.

```ts
import { serve } from "sako:http";

const server = serve((request) => {
  const url = new URL(request.url);
  if (url.pathname === "/health") return new Response("ok");
  return new Response("not found", { status: 404 });
}, { port: 8080 });

console.log(`listening on ${server.url}`);
```

Nothing is installed to use it: the library is embedded in the executable, so
`import { serve } from "sako:http"` resolves in any program Sako runs.

## `serve(handler, options?)` / `serve(options)`

Binds a port and returns once it is listening. The handler may be async, and
is given the request whose body has already been read, plus who is asking.

```ts
serve({
  port: 3000,
  fetch: async (request, connection) => {
    const body = await request.json();
    return Response.json?.(body) ?? new Response(JSON.stringify(body), {
      headers: { "content-type": "application/json" },
    });
  },
  onError: (error) => new Response(String(error), { status: 500 }),
  onListen: ({ url }) => console.log(`up on ${url}`),
});
```

| Option | Meaning |
| --- | --- |
| `port` | The port to bind. Zero, the default, takes whatever is free. |
| `fetch` | The handler, when it is not the first argument. |
| `onError` | Answers when a handler throws. Defaults to logging and a 500. |
| `onListen` | Called once with the bound address. |
| `tls` | `{ key, cert }`, both PEM, to serve HTTPS instead. |

The returned server carries `address`, `port`, `url`, a `close()`, and a
`finished` promise that resolves once it has been closed.

## How it is dispatched

A handler that returns a `Response` rather than a promise is answered inline:
the native dispatcher calls it with a `Request` and takes the `Response` back
as its own return value, so there is no `IncomingMessage`/`ServerResponse` pair
to build, no second call into JavaScript to ask what the answer was, and no
connection parked waiting for one. A handler that returns a promise still gets
the parked path, because the answer genuinely is not ready yet.

The `Request` it is handed defers what it can: reading `request.url` is what
builds the absolute URL, and reading a header is what decodes the header map,
so a handler that only looks at the method pays for neither.

## What differs from other runtimes

- **Loopback only.** The native server binds `127.0.0.1`, so there is no
  `hostname` option to pass: a server here is reachable from this machine.
- **Whole bodies.** A request body is read before the handler runs, and a
  response body is written when the handler returns. `Request.body` and
  `Response.body` are not streams yet, so this is not the place to serve a
  file larger than memory or to hold a long-lived event stream open.
- **No upgrades.** WebSockets and other protocol upgrades are unimplemented.

`node:http` remains what it was and runs on the same transport; see
[Node compatibility](../../docs/node-compatibility.md) for its status.

## Types in an editor

The library has no `.d.ts` to install -- the source is the types. Point
TypeScript at it:

```json
{
  "compilerOptions": {
    "paths": { "sako:http": ["./libs/http/src/index.ts"] }
  }
}
```
