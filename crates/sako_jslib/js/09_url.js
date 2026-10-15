// URL and URLSearchParams (WHATWG parsing in Rust).
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

// ----------------------------------------------------- URL, URLSearchParams
const isHex = (c) => (c >= 48 && c <= 57) || (c >= 65 && c <= 70) || (c >= 97 && c <= 102);
function percentDecode(s) {
  if (s.indexOf("%") === -1) return s;
  const b = ops.encode(s);
  const out = new Uint8Array(b.length);
  let j = 0;
  for (let i = 0; i < b.length; i++) {
    if (b[i] === 37 && i + 2 < b.length && isHex(b[i + 1]) && isHex(b[i + 2])) {
      out[j++] = parseInt(String.fromCharCode(b[i + 1], b[i + 2]), 16);
      i += 2;
    } else out[j++] = b[i];
  }
  return ops.decode(out.subarray(0, j), false, true);
}
const formDecode = (s) => percentDecode(s.indexOf("+") === -1 ? s : s.replace(/\+/g, " "));
const formEncode = (s) => encodeURIComponent(String(s).toWellFormed())
  .replace(/%20/g, "+")
  .replace(/[!'()~]/g, (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase());

const kOnUpdate = Symbol("onUpdate");
const kReset = Symbol("reset");
class URLSearchParams {
  #list = [];
  [kOnUpdate] = null;
  constructor(init = "") {
    if (init instanceof URLSearchParams) this.#list = [...init];
    else if (typeof init === "object" && init !== null) {
      if (typeof init[Symbol.iterator] === "function") {
        for (const pair of init) {
          const p = [...pair];
          if (p.length !== 2) throw new TypeError("Each query pair must be an iterable [name, value] tuple");
          this.#list.push([String(p[0]), String(p[1])]);
        }
      } else {
        for (const k of ObjectKeys(init)) this.#list.push([k, String(init[k])]);
      }
    } else this[kReset](String(init));
  }
  [kReset](s) {
    this.#list = [];
    if (s.startsWith("?")) s = s.slice(1);
    for (const part of s.split("&")) {
      if (part === "") continue;
      const eq = part.indexOf("=");
      this.#list.push(eq === -1 ? [formDecode(part), ""] : [formDecode(part.slice(0, eq)), formDecode(part.slice(eq + 1))]);
    }
  }
  #update() { if (this[kOnUpdate]) this[kOnUpdate](this.toString()); }
  get size() { return this.#list.length; }
  append(name, value) { this.#list.push([String(name), String(value)]); this.#update(); }
  delete(name, value) {
    name = String(name);
    this.#list = this.#list.filter(([k, v]) => !(k === name && (value === undefined || v === String(value))));
    this.#update();
  }
  get(name) { name = String(name); const e = this.#list.find(([k]) => k === name); return e ? e[1] : null; }
  getAll(name) { name = String(name); return this.#list.filter(([k]) => k === name).map(([, v]) => v); }
  has(name, value) {
    name = String(name);
    return this.#list.some(([k, v]) => k === name && (value === undefined || v === String(value)));
  }
  set(name, value) {
    name = String(name);
    value = String(value);
    const i = this.#list.findIndex(([k]) => k === name);
    if (i === -1) this.#list.push([name, value]);
    else {
      this.#list[i][1] = value;
      this.#list = this.#list.filter(([k], j) => j <= i || k !== name);
    }
    this.#update();
  }
  sort() {
    this.#list = this.#list.map((e, i) => [e, i]).sort((a, b) => (a[0][0] < b[0][0] ? -1 : a[0][0] > b[0][0] ? 1 : a[1] - b[1])).map(([e]) => e);
    this.#update();
  }
  forEach(cb, thisArg) { for (const [k, v] of this.#list) cb.call(thisArg, v, k, this); }
  *keys() { for (const [k] of this.#list) yield k; }
  *values() { for (const [, v] of this.#list) yield v; }
  *entries() { for (const [k, v] of this.#list) yield [k, v]; }
  [Symbol.iterator]() { return this.entries(); }
  toString() { return this.#list.map(([k, v]) => formEncode(k) + "=" + formEncode(v)).join("&"); }
  [kCustomInspect]() {
    return "URLSearchParams {" + (this.#list.length ? " " + this.#list.map(([k, v]) => quote(k) + " => " + quote(v)).join(", ") + " " : "") + "}";
  }
}
define(URLSearchParams.prototype, Symbol.toStringTag, "URLSearchParams");

// parts: [href, origin, protocol, username, password, host, hostname, port, pathname, search, hash]
const urlFields = ["href", "origin", "protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"];
class URL {
  #p;
  #sp = null;
  constructor(url, base) {
    if (arguments.length === 0) throw new TypeError("The \"url\" argument must be specified");
    const input = String(url);
    const p = base === undefined ? ops.urlParse(input) : ops.urlParse(input, String(base));
    if (p === undefined) {
      const err = new TypeError("Invalid URL");
      err.code = "ERR_INVALID_URL";
      err.input = input;
      throw err;
    }
    this.#p = p;
  }
  static canParse(url, base) {
    return (base === undefined ? ops.urlParse(String(url)) : ops.urlParse(String(url), String(base))) !== undefined;
  }
  static parse(url, base) { try { return new URL(url, base); } catch { return null; } }
  #set(i, v) {
    this.#p = ops.urlSet(this.#p[0], i, String(v));
    if (this.#sp && (i === 0 || i === 9)) this.#sp[kReset](this.#p[9]);
  }
  get searchParams() {
    if (this.#sp === null) {
      this.#sp = new URLSearchParams(this.#p[9]);
      this.#sp[kOnUpdate] = (s) => { this.#p = ops.urlSet(this.#p[0], 9, s); };
    }
    return this.#sp;
  }
  toString() { return this.#p[0]; }
  toJSON() { return this.#p[0]; }
  [kCustomInspect](depth, ctx) {
    const o = {};
    for (let i = 0; i < urlFields.length; i++) o[urlFields[i]] = this.#p[i];
    o.searchParams = this.searchParams;
    return "URL " + inspect(o, ctx).replace(/^\{/, "{");
  }
  static {
    for (let i = 0; i < urlFields.length; i++) {
      const desc = { get() { return this.#p[i]; }, configurable: true, enumerable: true };
      if (i !== 1) desc.set = function (v) { this.#set(i, v); };
      ObjectDefineProperty(URL.prototype, urlFields[i], desc);
    }
  }
}
define(URL.prototype, Symbol.toStringTag, "URL");
define(g, "URL", URL);
define(g, "URLSearchParams", URLSearchParams);
