// Sako.serve: router and request dispatch.
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

// ------------------------------------------------------------------ serve
// Requests arrive from Rust in batches (see dispatch_http in main.rs) as a
// flat array of [serverId, requestId, method, url, headers, body] records.
const servers = new Map();
let nextServerId = 1;

function serve(options) {
  if (typeof options === "function") options = { fetch: options };
  if (options === null || typeof options !== "object" ||
      (typeof options.fetch !== "function" && (options.routes === null || typeof options.routes !== "object"))) {
    throw new TypeError("Sako.serve() expects { fetch(request, server) } and/or { routes }");
  }
  const hostname = String(options.hostname ?? "0.0.0.0");
  const port = Number(options.port ?? ops.env.PORT ?? 3000);
  const id = nextServerId++;
  const bound = ops.serve(id, hostname, port, !!options.reusePort);
  const shownHost = hostname === "0.0.0.0" || hostname === "::" ? "localhost" : hostname;
  const entry = { fetch: options.fetch, error: options.error, router: null };
  const server = {
    port: bound,
    hostname: shownHost,
    url: new URL(`http://${shownHost}:${bound}/`),
    stop() { if (servers.delete(id)) ops.serverStop(id); },
    reload(o) {
      if (o && typeof o.fetch === "function") entry.fetch = o.fetch;
      if (o && typeof o.error === "function") entry.error = o.error;
      if (o && o.routes) setRoutes(id, entry, o.routes);
    },
    [Symbol.dispose]() { this.stop(); },
  };
  entry.server = server;
  servers.set(id, entry);
  if (options.routes) setRoutes(id, entry, options.routes);
  return server;
}

// Routes, like Bun.serve: { "/path": Response | handler | { GET, POST, ... } }.
// Static Responses are answered in Rust without calling into JS; handlers are
// matched here: exact paths first, then ":param" patterns, then "*" wildcards.
function setRoutes(id, entry, routes) {
  ops.serveStaticClear(id);
  const exact = new Map();
  const patterns = [];
  for (const path of ObjectKeys(routes)) {
    if (path[0] !== "/") throw new TypeError(`Route path must start with "/": ${path}`);
    let value = routes[path];
    const dynamic = path.includes(":") || path.includes("*");
    const register = (method, v) => {
      if (v instanceof Response && !dynamic) {
        ops.serveStatic(id, path, method, v.status, v[kFlat](), v[kRaw]);
        return undefined;
      }
      if (v instanceof Response) { const r = v; return () => r.clone(); }
      if (typeof v !== "function") throw new TypeError(`Invalid handler for route ${path}`);
      return v;
    };
    let handler;
    if (value instanceof Response || typeof value === "function") {
      handler = register("", value);
    } else if (value !== null && typeof value === "object") {
      const byMethod = { __proto__: null };
      let any = false;
      for (const m of ObjectKeys(value)) {
        const h = register(m.toUpperCase(), value[m]);
        if (h) { byMethod[m.toUpperCase()] = h; any = true; }
      }
      handler = any ? byMethod : undefined;
    } else throw new TypeError(`Invalid handler for route ${path}`);
    if (handler === undefined) continue;
    if (!dynamic) exact.set(path, handler);
    else patterns.push(compileRoute(path, handler));
  }
  // Wildcards last; longer patterns first.
  patterns.sort((a, b) => (a.wildcard - b.wildcard) || (b.segs.length - a.segs.length));
  entry.router = { exact, patterns };
}

// "/users/:id/*" -> { segs: ["users", ":id"], names: [null, "id"], wildcard: true }
function compileRoute(path, handler) {
  const parts = path.slice(1).split("/");
  const wildcard = parts[parts.length - 1] === "*";
  if (wildcard) parts.pop();
  const names = parts.map((p) => (p.charCodeAt(0) === 58 ? p.slice(1) : null));
  // Builds the params object with a fixed shape (fast property stores).
  const keys = names.filter((n) => n !== null);
  const make = keys.length === 0 ? () => ({}) :
    new Function("v", "return {" + keys.map((k, i) => JSON.stringify(k) + ": v[" + i + "]").join(", ") + "};");
  return { segs: parts, names, wildcard, handler, make, nparams: keys.length };
}

const paramValues = [];
function matchRoute(router, path) {
  const h = router.exact.get(path);
  if (h !== undefined) return [h, undefined];
  const patterns = router.patterns;
  outer: for (let p = 0; p < patterns.length; p++) {
    const route = patterns[p];
    const segs = route.segs;
    const names = route.names;
    let pos = 1; // after the leading "/"
    let n = 0;
    for (let i = 0; i < segs.length; i++) {
      if (pos > path.length) continue outer;
      let end = path.indexOf("/", pos);
      if (end === -1) end = path.length;
      if (names[i] !== null) {
        if (end === pos) continue outer; // empty param
        paramValues[n++] = percentDecode(path.slice(pos, end));
      } else {
        const lit = segs[i];
        if (end - pos !== lit.length || !path.startsWith(lit, pos)) continue outer;
      }
      pos = end + 1;
    }
    if (!route.wildcard && pos <= path.length) continue; // extra segments
    return [route.handler, route.nparams === 0 ? undefined : route.make(paramValues)];
  }
  return undefined;
}

function serverError(entry, err) {
  if (typeof entry.error === "function") {
    try {
      const r = entry.error.call(entry.server, err);
      if (r instanceof Response || (r !== null && typeof r === "object" && typeof r.then === "function")) return r;
    } catch (e) { err = e; }
  }
  print("error: Uncaught (in request handler) " + inspect(err) + "\n", true);
  return new Response("Internal Server Error", { status: 500 });
}

function respond(entry, id, res) {
  if (res instanceof Response) {
    ops.httpRespond(id, res.status, res[kFlat](), res[kRaw]);
  } else if (res !== null && typeof res === "object" && typeof res.then === "function") {
    res.then((r) => respond(entry, id, r), (e) => respond(entry, id, serverError(entry, e)));
  } else {
    respond(entry, id, serverError(entry, new TypeError(`Expected a Response object, got ${inspect(res)}`)));
  }
}

function httpDispatch(batch) {
  for (let i = 0; i < batch.length; i += 6) {
    const entry = servers.get(batch[i]);
    const id = batch[i + 1];
    if (entry === undefined) {
      ops.httpRespond(id, 503, undefined, "Service Unavailable");
      continue;
    }
    const method = batch[i + 2];
    const url = batch[i + 3];
    const req = Request[kFromServer](method, url, batch[i + 4], batch[i + 5]);
    let res;
    try {
      let handler = entry.fetch;
      const router = entry.router;
      if (router !== null) {
        const start = url.indexOf("/", 7); // after "http://"
        const q = url.indexOf("?", start);
        const m = matchRoute(router, q === -1 ? url.slice(start) : url.slice(start, q));
        if (m !== undefined) {
          let h = m[0];
          if (typeof h !== "function") h = h[method] ?? (method === "HEAD" ? h.GET : undefined);
          if (h !== undefined) {
            handler = h;
            req.params = m[1] ?? {};
          }
        }
      }
      res = handler === undefined
        ? new Response("Not Found", { status: 404 })
        : handler.call(entry.server, req, entry.server);
    } catch (e) {
      res = serverError(entry, e);
    }
    respond(entry, id, res);
  }
}
