// Headers, Request, Response, fetch.
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

// ---------------------------------------------------- Headers, Request, Response, fetch
const tokenRe = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
const validNames = new Map(); // raw name -> lowercase, for names seen before
function normName(name) {
  const known = validNames.get(name);
  if (known !== undefined) return known;
  const raw = name;
  name = String(name);
  if (!tokenRe.test(name)) throw new TypeError(`Header name must be a valid HTTP token ["${name}"]`);
  const lower = name.toLowerCase();
  if (typeof raw === "string" && validNames.size < 1024) validNames.set(raw, lower);
  return lower;
}
const isWs = (c) => c === 32 || c === 9 || c === 10 || c === 13;
function normValue(v) {
  v = String(v);
  if (v.length === 0 || (!isWs(v.charCodeAt(0)) && !isWs(v.charCodeAt(v.length - 1)))) return v;
  return v.replace(/^[\t\n\r ]+|[\t\n\r ]+$/g, "");
}

const kFromFlat = Symbol("fromFlat");
class Headers {
  #map = new Map();
  #cookies = [];
  // Trusted fast path for response headers: names are already lowercase
  // and valid (they come from the HTTP parser).
  static [kFromFlat](flat) {
    const h = new Headers();
    const map = h.#map;
    for (let i = 0; i < flat.length; i += 2) {
      const k = flat[i];
      const v = flat[i + 1];
      if (k === "set-cookie") h.#cookies.push(v);
      const cur = map.get(k);
      map.set(k, cur === undefined ? v : cur + ", " + v);
    }
    return h;
  }
  constructor(init) {
    if (init == null) return;
    if (init instanceof Headers) {
      for (const [k, v] of init) this.append(k, v);
    } else if (typeof init[Symbol.iterator] === "function") {
      for (const pair of init) {
        const p = [...pair];
        if (p.length !== 2) throw new TypeError("Header pair must have exactly two items");
        this.append(p[0], p[1]);
      }
    } else if (typeof init === "object") {
      for (const k of ObjectKeys(init)) this.append(k, init[k]);
    } else throw new TypeError("Invalid headers init");
  }
  append(name, value) {
    name = normName(name);
    value = normValue(value);
    if (name === "set-cookie") this.#cookies.push(value);
    const cur = this.#map.get(name);
    this.#map.set(name, cur === undefined ? value : cur + ", " + value);
  }
  delete(name) {
    name = normName(name);
    this.#map.delete(name);
    if (name === "set-cookie") this.#cookies = [];
  }
  get(name) { const v = this.#map.get(normName(name)); return v === undefined ? null : v; }
  getSetCookie() { return this.#cookies.slice(); }
  has(name) { return this.#map.has(normName(name)); }
  set(name, value) {
    name = normName(name);
    value = normValue(value);
    this.#map.set(name, value);
    if (name === "set-cookie") this.#cookies = [value];
  }
  forEach(cb, thisArg) { for (const [k, v] of this) cb.call(thisArg, v, k, this); }
  *entries() {
    for (const k of [...this.#map.keys()].sort()) {
      if (k === "set-cookie") for (const c of this.#cookies) yield [k, c];
      else yield [k, this.#map.get(k)];
    }
  }
  *keys() { for (const [k] of this.entries()) yield k; }
  *values() { for (const [, v] of this.entries()) yield v; }
  [Symbol.iterator]() { return this.entries(); }
  [kCustomInspect]() {
    const items = [...this.entries()];
    return "Headers {" + (items.length ? " " + items.map(([k, v]) => quote(k) + ": " + quote(v)).join(", ") + " " : "") + "}";
  }
}
define(Headers.prototype, Symbol.toStringTag, "Headers");

// Returns [source, defaultContentType] for a BodyInit. Strings stay strings
// so text()/json() and server responses never round-trip through bytes.
function extractBody(body) {
  if (body == null) return [null, null];
  if (typeof body === "string") return [body, "text/plain;charset=UTF-8"];
  if (body instanceof URLSearchParams) return [body.toString(), "application/x-www-form-urlencoded;charset=UTF-8"];
  if (body instanceof ArrayBuffer) return [new Uint8Array(body.slice(0)), null];
  if (ArrayBuffer.isView(body)) return [new Uint8Array(body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength)), null];
  return [String(body), "text/plain;charset=UTF-8"];
}

const kRaw = Symbol("raw");
class Body {
  #src; // string | Uint8Array | null
  #used = false;
  constructor(src) { this.#src = src; }
  get [kRaw]() { return this.#src; }
  get bodyUsed() { return this.#used; }
  get body() {
    if (this.#src === null) return null;
    const self = this;
    let done = false;
    const read = async () => {
      if (done) return { done: true, value: undefined };
      done = true;
      return { done: false, value: self.#consumeBytes() };
    };
    return {
      locked: false,
      getReader() { return { read, releaseLock() {}, cancel: async () => { done = true; }, closed: Promise.resolve() }; },
      cancel: async () => { done = true; },
      [Symbol.asyncIterator]() { return { next: read, return: async () => ({ done: true }) }; },
    };
  }
  #consume() {
    if (this.#used) throw new TypeError("Body is unusable: Body has already been read");
    this.#used = true;
    return this.#src;
  }
  #consumeBytes() {
    const src = this.#consume();
    return src === null ? new Uint8Array(0) : typeof src === "string" ? ops.encode(src) : src;
  }
  async arrayBuffer() {
    const b = this.#consumeBytes();
    return b.byteOffset === 0 && b.byteLength === b.buffer.byteLength ? b.buffer : b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
  }
  async bytes() { return this.#consumeBytes(); }
  async text() {
    const src = this.#consume();
    return src === null ? "" : typeof src === "string" ? src : ops.decode(src, false, false);
  }
  async json() { return JSON.parse(await this.text()); }
}

const kInit = Symbol("init");
const kFromServer = Symbol("fromServer");
const kFlat = Symbol("flatHeaders");
class Request extends Body {
  #url; #method; #headers = null; #flat = null; #signal = null; #redirect = "follow";
  constructor(input, init = {}) {
    if (init === kInit) { super(input); return; }
    const src = input instanceof Request ? input : null;
    const [body, ctype] = init.body !== undefined ? extractBody(init.body) : [src ? src[kRaw] : null, null];
    super(body);
    if (src) this.#url = src.url;
    else {
      const href = typeof input === "string" ? ops.urlHref(input) : new URL(String(input)).href;
      if (href === undefined) new URL(input); // throws the standard "Invalid URL" TypeError
      this.#url = href;
    }
    const method = init.method !== undefined ? String(init.method) : src ? src.method : "GET";
    this.#method = /^(get|head|post|put|delete|options|patch)$/i.test(method) ? method.toUpperCase() : method;
    if (body !== null && (this.#method === "GET" || this.#method === "HEAD")) {
      throw new TypeError("Request with GET/HEAD method cannot have body.");
    }
    this.#headers = new Headers(init.headers !== undefined ? init.headers : src ? src.headers : undefined);
    if (ctype && !this.#headers.has("content-type")) this.#headers.set("content-type", ctype);
    this.#signal = init.signal !== undefined ? init.signal : src ? src.signal : null;
    this.#redirect = init.redirect || (src ? src.redirect : "follow");
  }
  // Trusted fast path for incoming server requests; headers are built lazily.
  static [kFromServer](method, url, flat, body) {
    const r = new Request(body, kInit);
    r.#method = method;
    r.#url = url;
    r.#flat = flat;
    return r;
  }
  get url() { return this.#url; }
  get method() { return this.#method; }
  get headers() { return (this.#headers ??= Headers[kFromFlat](this.#flat ?? [])); }
  get signal() { return this.#signal; }
  get redirect() { return this.#redirect; }
  clone() {
    if (this.bodyUsed) throw new TypeError("Request body is already used");
    return new Request(this);
  }
}
define(Request.prototype, Symbol.toStringTag, "Request");

class Response extends Body {
  #status = 200; #statusText = ""; #url = ""; #redirected = false; #type = "default";
  // Headers are materialized lazily: most server responses only need the
  // default content-type, which kFlat can produce without a Headers object.
  #headers = null; #headersInit = undefined; #ctype = null;
  constructor(body = null, init = undefined) {
    if (init === kInit) { super(body); return; }
    const [src, ctype] = extractBody(body);
    super(src);
    this.#ctype = ctype;
    if (init === undefined) return;
    if (init.status !== undefined) {
      const status = init.status | 0;
      if (status < 200 || status > 599) throw new RangeError(`init["status"] must be in the range of 200 to 599, inclusive.`);
      this.#status = status;
    }
    if (init.statusText !== undefined) this.#statusText = String(init.statusText);
    if (init.headers !== undefined) this.#headersInit = init.headers;
  }
  static [kInit](body, status, statusText, headers, url, redirected, type) {
    const r = new Response(body, kInit);
    r.#status = status;
    r.#statusText = statusText;
    r.#headers = headers;
    r.#url = url;
    r.#redirected = redirected;
    r.#type = type;
    return r;
  }
  static json(data, init = undefined) {
    const r = new Response(JSON.stringify(data), init);
    r.#ctype = "application/json";
    return r;
  }
  static error() { return Response[kInit](null, 0, "", new Headers(), "", false, "error"); }
  static redirect(url, status = 302) {
    if (![301, 302, 303, 307, 308].includes(status)) throw new RangeError("Invalid status code");
    return Response[kInit](null, status, "", new Headers({ location: new URL(url).href }), "", false, "default");
  }
  get status() { return this.#status; }
  get ok() { return this.#status >= 200 && this.#status < 300; }
  get statusText() { return this.#statusText; }
  get headers() {
    if (this.#headers === null) {
      this.#headers = new Headers(this.#headersInit);
      if (this.#ctype !== null && !this.#headers.has("content-type")) this.#headers.set("content-type", this.#ctype);
    }
    return this.#headers;
  }
  // Flat [k, v, ...] header list for the server.
  [kFlat]() {
    if (this.#headers === null) {
      const init = this.#headersInit;
      if (init === undefined) return this.#ctype === null ? undefined : ["content-type", this.#ctype];
      // Plain object init: build the list directly, no Headers object.
      if (ObjectGetPrototypeOf(init) === Object.prototype) {
        const flat = [];
        let hasType = false;
        for (const k of ObjectKeys(init)) {
          const name = normName(k);
          if (name === "content-type") hasType = true;
          flat.push(name, normValue(init[k]));
        }
        if (!hasType && this.#ctype !== null) flat.push("content-type", this.#ctype);
        return flat;
      }
    }
    const flat = [];
    for (const [k, v] of this.headers) flat.push(k, v);
    return flat;
  }
  get url() { return this.#url; }
  get redirected() { return this.#redirected; }
  get type() { return this.#type; }
  clone() {
    if (this.bodyUsed) throw new TypeError("Response body is already used");
    return Response[kInit](this[kRaw], this.#status, this.#statusText, new Headers(this.headers), this.#url, this.#redirected, this.#type);
  }
  [kCustomInspect](depth, ctx) {
    return "Response " + inspect({
      status: this.#status, statusText: this.#statusText, ok: this.ok, url: this.#url,
      redirected: this.#redirected, headers: this.headers, bodyUsed: this.bodyUsed,
    }, ctx);
  }
}
define(Response.prototype, Symbol.toStringTag, "Response");

async function fetch(input, init) {
  const req = new Request(input, init);
  const signal = req.signal;
  if (signal) signal.throwIfAborted();
  const protocol = req.url.slice(0, req.url.indexOf(":") + 1);
  if (protocol !== "http:" && protocol !== "https:") {
    throw new TypeError(`fetch failed: unsupported URL scheme "${protocol}"`);
  }
  const flat = [];
  for (const [k, v] of req.headers) flat.push(k, v);
  const redirect = req.redirect === "manual" ? 1 : req.redirect === "error" ? 2 : 0;
  let pending = ops.fetch(req.url, req.method, flat, req[kRaw], redirect);
  if (signal) {
    // The request itself cannot be cancelled mid-flight; abort rejects early.
    pending = Promise.race([pending, new Promise((_, reject) => {
      signal.addEventListener("abort", () => reject(signal.reason), { once: true });
    })]);
  }
  const [status, statusText, url, redirected, hdrs, body] = await pending;
  const headers = Headers[kFromFlat](hdrs);
  const nullBody = req.method === "HEAD" || status === 204 || status === 304;
  return Response[kInit](nullBody ? null : body, status, statusText, headers, url, redirected, "basic");
}

define(g, "Headers", Headers);
define(g, "Request", Request);
define(g, "Response", Response);
define(g, "fetch", fetch);
