// Sako.serve: routes (static, per-method, params, wildcard), fetch fallback,
// async handlers, errors, bodies and headers, reload, stop.
const server = Sako.serve({
  port: 0,
  hostname: "127.0.0.1",
  routes: {
    "/": new Response("static root", { headers: { "x-static": "1" } }),
    "/item": { GET: new Response("static get"), POST: async (req) => new Response("created " + (await req.text()), { status: 201 }) },
    "/users/:id": (req) => Response.json({ id: req.params.id }),
    "/users/:id/posts/:post": (req) => Response.json(req.params),
    "/files/*": (req) => new Response("file " + new URL(req.url).pathname),
    "/headers": (req) => Response.json({ custom: req.headers.get("x-custom"), method: req.method }),
    "/async": async () => { await new Promise((r) => setTimeout(r, 5)); return new Response("later"); },
    "/throw": () => { throw new Error("handler failed"); },
    "/bytes": () => new Response(new Uint8Array([111, 107]), { headers: { "content-type": "application/octet-stream" } }),
  },
  fetch: (req) => new Response("fallback " + new URL(req.url).pathname, { status: 404 }),
  error: (err) => new Response("handled: " + err.message, { status: 500 }),
});
console.log(server.port > 0, server.hostname, server.url.href === `http://127.0.0.1:${server.port}/`);
const B = `http://127.0.0.1:${server.port}`;
async function show(path, init) {
  const r = await fetch(B + path, init);
  return `${init?.method ?? "GET"} ${path} -> ${r.status} [${r.headers.get("content-type")}] ${await r.text()}`;
}
console.log(await show("/"));
console.log((await fetch(B + "/")).headers.get("x-static"));
console.log(await show("/", { method: "HEAD" }));
console.log(await show("/item"));
console.log(await show("/item", { method: "POST", body: "thing" }));
console.log(await show("/item", { method: "DELETE" }));
console.log(await show("/users/42"));
console.log(await show("/users/a%20b/posts/7"));
console.log(await show("/users/42/"));
console.log(await show("/files/a/b.txt"));
console.log(await show("/headers", { headers: { "X-Custom": "yes" }, method: "PUT", body: "" }));
console.log(await show("/async"));
console.log(await show("/throw"));
console.log(await show("/bytes"));
console.log(await show("/missing?x=1"));
const all = await Promise.all(Array.from({ length: 300 }, (_, i) => fetch(`${B}/users/${i}`).then((r) => r.json())));
console.log("300 concurrent:", all.every((j, i) => j.id === String(i)));
server.reload({ routes: { "/": () => new Response("reloaded") } });
console.log(await show("/"), "|", await show("/users/1"));
try { Sako.serve({ port: server.port, hostname: "127.0.0.1", fetch() {} }); } catch (e) { console.log("second bind:", e.code); }
try { Sako.serve({}); } catch (e) { console.log(e.name); }
server.stop();
console.log("stopped");
