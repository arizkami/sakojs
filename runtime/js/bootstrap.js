// SPDX-License-Identifier: BSD-3-Clause

(() => {
  "use strict";

  const immediateTasks = new Map();
  let nextImmediateId = 1;
  globalThis.setImmediate = (callback, ...args) => {
    const id = nextImmediateId++;
    if (immediateTasks.size >= 65536) throw new RangeError("immediate capacity exceeded");
    immediateTasks.set(id, callback);
    queueMicrotask(() => {
      const task = immediateTasks.get(id);
      if (!task) return;
      immediateTasks.delete(id);
      task(...args);
    });
    return id;
  };
  globalThis.clearImmediate = (id) => immediateTasks.delete(id);

  // Node hands back a Timeout object rather than a number, and `.unref()` on
  // it is how a program says "do not stay alive for this one". Everything that
  // takes a timer keeps working because the object coerces to its own id, and
  // node:timers reads these off the global, so its exports stay identical to
  // the globals.
  class Timeout {
    constructor(id) { this._id = id; this._referenced = true; }
    ref() { this._referenced = true; __sakoTimerRef(this._id, true); return this; }
    unref() { this._referenced = false; __sakoTimerRef(this._id, false); return this; }
    hasRef() { return this._referenced; }
    refresh() { return this; }
    close() { clearTimeout(this._id); return this; }
    valueOf() { return this._id; }
    [Symbol.toPrimitive]() { return this._id; }
  }
  const scheduleTimeout = globalThis.setTimeout;
  const scheduleInterval = globalThis.setInterval;
  globalThis.setTimeout = (...args) => new Timeout(scheduleTimeout(...args));
  globalThis.setInterval = (...args) => new Timeout(scheduleInterval(...args));

  // Buffer encodings. Only utf8 was ever decoded, which is enough for reading
  // a source file and nothing else: a bundler embedding an inline source map,
  // a loader reading a data: URL, and anything hashing to hex all arrive as
  // base64 or hex and used to fail at the first Buffer.from.
  const BASE64_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const BASE64_URL_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
  const BASE64_VALUES = (() => {
    const table = new Int16Array(256).fill(-1);
    for (let index = 0; index < BASE64_ALPHABET.length; index += 1) {
      table[BASE64_ALPHABET.charCodeAt(index)] = index;
    }
    // base64url spells the last two digits differently; accepting both on the
    // way in costs nothing and is what Node does.
    table[45] = 62;
    table[95] = 63;
    return table;
  })();

  const normalizeEncoding = (encoding) => {
    if (encoding === undefined || encoding === null) return "utf8";
    switch (String(encoding).toLowerCase()) {
      case "utf8": case "utf-8": return "utf8";
      case "hex": return "hex";
      case "base64": return "base64";
      case "base64url": return "base64url";
      case "latin1": case "binary": return "latin1";
      case "ascii": return "ascii";
      case "ucs2": case "ucs-2": case "utf16le": case "utf-16le": return "utf16le";
      default: return null;
    }
  };

  const encodeString = (value, encoding) => {
    if (encoding === "utf8") return __sakoEncodeUtf8(value);
    if (encoding === "hex") {
      const pairs = value.length >> 1;
      const bytes = new Uint8Array(pairs);
      for (let index = 0; index < pairs; index += 1) {
        const byte = Number.parseInt(value.substr(index * 2, 2), 16);
        // Node stops at the first pair that is not hex rather than throwing.
        if (Number.isNaN(byte)) return bytes.subarray(0, index);
        bytes[index] = byte;
      }
      return bytes;
    }
    if (encoding === "base64" || encoding === "base64url") {
      const output = [];
      let bits = 0;
      let count = 0;
      for (let index = 0; index < value.length; index += 1) {
        const code = value.charCodeAt(index);
        const digit = code < 256 ? BASE64_VALUES[code] : -1;
        // Padding and whitespace are skipped rather than rejected.
        if (digit < 0) continue;
        bits = (bits << 6) | digit;
        count += 6;
        if (count >= 8) {
          count -= 8;
          output.push((bits >> count) & 255);
        }
      }
      return new Uint8Array(output);
    }
    if (encoding === "utf16le") {
      const bytes = new Uint8Array(value.length * 2);
      for (let index = 0; index < value.length; index += 1) {
        const code = value.charCodeAt(index);
        bytes[index * 2] = code & 255;
        bytes[index * 2 + 1] = code >> 8;
      }
      return bytes;
    }
    // latin1 and ascii differ only in how much of each code unit survives.
    const mask = encoding === "ascii" ? 127 : 255;
    const bytes = new Uint8Array(value.length);
    for (let index = 0; index < value.length; index += 1) {
      bytes[index] = value.charCodeAt(index) & mask;
    }
    return bytes;
  };

  const decodeBytes = (bytes, encoding) => {
    if (encoding === "utf8") return __sakoDecodeUtf8(bytes);
    if (encoding === "hex") {
      return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
    }
    if (encoding === "base64" || encoding === "base64url") {
      const alphabet = encoding === "base64" ? BASE64_ALPHABET : BASE64_URL_ALPHABET;
      const padded = encoding === "base64";
      let output = "";
      for (let index = 0; index < bytes.length; index += 3) {
        const first = bytes[index];
        const second = bytes[index + 1];
        const third = bytes[index + 2];
        output += alphabet[first >> 2];
        output += alphabet[((first & 3) << 4) | ((second || 0) >> 4)];
        if (index + 1 < bytes.length) {
          output += alphabet[((second & 15) << 2) | ((third || 0) >> 6)];
        } else if (padded) {
          output += "=";
        }
        if (index + 2 < bytes.length) {
          output += alphabet[third & 63];
        } else if (padded) {
          output += "=";
        }
      }
      return output;
    }
    if (encoding === "utf16le") {
      let output = "";
      for (let index = 0; index + 1 < bytes.length; index += 2) {
        output += String.fromCharCode(bytes[index] | (bytes[index + 1] << 8));
      }
      return output;
    }
    const mask = encoding === "ascii" ? 127 : 255;
    let output = "";
    // Chunked, because one call per byte is slow and one call for the whole
    // buffer overflows the argument limit on anything large.
    for (let index = 0; index < bytes.length; index += 4096) {
      const chunk = bytes.subarray(index, index + 4096);
      output += String.fromCharCode.apply(null, Array.from(chunk, (byte) => byte & mask));
    }
    return output;
  };

  class Buffer extends Uint8Array {
    static from(value, encodingOrOffset, length) {
      let bytes;
      if (typeof value === "string") {
        const encoding = normalizeEncoding(encodingOrOffset);
        if (encoding === null) {
          throw new TypeError(`Unsupported encoding: ${encodingOrOffset}`);
        }
        bytes = encodeString(value, encoding);
      } else if (value instanceof ArrayBuffer) {
        // The second argument is a byte offset here, not an encoding: the
        // ArrayBuffer overload shares its position with the string form.
        const offset = encodingOrOffset === undefined ? 0 : Number(encodingOrOffset);
        const count = length === undefined ? value.byteLength - offset : Number(length);
        bytes = new Uint8Array(value, offset, count);
      } else if (ArrayBuffer.isView(value) || Array.isArray(value)) {
        bytes = new Uint8Array(value);
      } else if (value !== null && typeof value === "object" && typeof value.length === "number") {
        bytes = Uint8Array.from(value, (byte) => Number(byte) & 255);
      } else {
        throw new TypeError("Buffer.from needs a string, ArrayBuffer, or byte array");
      }
      Object.setPrototypeOf(bytes, Buffer.prototype);
      return bytes;
    }

    static alloc(size, fill = 0) {
      if (!Number.isSafeInteger(size) || size < 0) {
        throw new RangeError("Buffer size must be a non-negative safe integer");
      }
      const bytes = new Uint8Array(size);
      bytes.fill(fill);
      Object.setPrototypeOf(bytes, Buffer.prototype);
      return bytes;
    }

    static allocUnsafe(size) { return Buffer.alloc(size); }
    static allocUnsafeSlow(size) { return Buffer.alloc(size); }

    static isBuffer(value) {
      return value instanceof Buffer;
    }

    static byteLength(value, encoding = "utf8") {
      if (Buffer.isBuffer(value) || ArrayBuffer.isView(value)) return value.byteLength;
      if (value instanceof ArrayBuffer) return value.byteLength;
      return Buffer.from(String(value), encoding).length;
    }

    static concat(values, totalLength) {
      if (!Array.isArray(values)) throw new TypeError("Buffer.concat needs an array");
      const length = totalLength === undefined
        ? values.reduce((sum, value) => sum + value.length, 0)
        : totalLength;
      const output = Buffer.alloc(length);
      let offset = 0;
      for (const value of values) {
        const bytes = Buffer.from(value);
        output.set(bytes.subarray(0, Math.max(0, length - offset)), offset);
        offset += bytes.length;
        if (offset >= length) break;
      }
      return output;
    }

    toString(encoding = "utf8", start = 0, end = this.length) {
      const name = normalizeEncoding(encoding);
      if (name === null) throw new TypeError(`Unsupported encoding: ${encoding}`);
      return decodeBytes(this.subarray(start, end), name);
    }

    write(value, offset = 0, length, encoding) {
      // The trailing arguments slide the way Node's do: write(value, encoding)
      // is as common as the full form, and tooling uses both.
      if (typeof offset === "string") { encoding = offset; offset = 0; length = undefined; }
      else if (typeof length === "string") { encoding = length; length = undefined; }
      const name = normalizeEncoding(encoding);
      if (name === null) throw new TypeError(`Unsupported encoding: ${encoding}`);
      const bytes = encodeString(String(value), name);
      const room = Math.min(bytes.length, this.length - offset,
        length === undefined ? Number.MAX_SAFE_INTEGER : length);
      if (room <= 0) return 0;
      this.set(bytes.subarray(0, room), offset);
      return room;
    }

    equals(other) {
      if (!ArrayBuffer.isView(other)) throw new TypeError("Buffer.equals needs a byte array");
      if (other.byteLength !== this.length) return false;
      for (let index = 0; index < this.length; index += 1) {
        if (this[index] !== other[index]) return false;
      }
      return true;
    }

    toJSON() {
      return { type: "Buffer", data: Array.from(this) };
    }
  }

  // atob and btoa predate typed arrays: both sides of them are "binary
  // strings", one character per byte, which is latin1.
  globalThis.btoa = (value) => decodeBytes(encodeString(String(value), 'latin1'), 'base64');
  globalThis.atob = (value) => decodeBytes(encodeString(String(value), 'base64'), 'latin1');

  class TextEncoder {
    get encoding() { return "utf-8"; }
    encode(value = "") { return __sakoEncodeUtf8(String(value)); }
  }

  class TextDecoder {
    constructor(label = "utf-8") {
      if (!/^utf-?8$/i.test(label)) throw new RangeError(`Unsupported encoding: ${label}`);
      this.encoding = "utf-8";
    }
    decode(value = new Uint8Array()) { return __sakoDecodeUtf8(value); }
  }

  const formEncode = (value) => encodeURIComponent(String(value)).replace(/%20/g, "+");
  const formDecode = (value) => decodeURIComponent(String(value).replace(/\+/g, " "));

  class URLSearchParams {
    constructor(init = "") {
      this._entries = [];
      if (typeof init === "string") {
        const source = init.startsWith("?") ? init.slice(1) : init;
        if (source) for (const field of source.split("&")) {
          const index = field.indexOf("=");
          this.append(formDecode(index < 0 ? field : field.slice(0, index)), formDecode(index < 0 ? "" : field.slice(index + 1)));
        }
      } else if (init && typeof init[Symbol.iterator] === "function") {
        for (const [name, value] of init) this.append(name, value);
      } else if (init && typeof init === "object") {
        for (const name of Object.keys(init)) this.append(name, init[name]);
      }
    }
    append(name, value) { this._entries.push([String(name), String(value)]); }
    delete(name, value) {
      name = String(name);
      this._entries = this._entries.filter((item) => item[0] !== name || (arguments.length > 1 && item[1] !== String(value)));
    }
    get(name) { return this._entries.find((item) => item[0] === String(name))?.[1] ?? null; }
    getAll(name) { return this._entries.filter((item) => item[0] === String(name)).map((item) => item[1]); }
    has(name, value) { return this._entries.some((item) => item[0] === String(name) && (arguments.length < 2 || item[1] === String(value))); }
    set(name, value) {
      name = String(name); value = String(value);
      const index = this._entries.findIndex((item) => item[0] === name);
      this.delete(name);
      if (index < 0) this._entries.push([name, value]); else this._entries.splice(index, 0, [name, value]);
    }
    sort() { this._entries = this._entries.map((item, index) => ({ item, index })).sort((a, b) => a.item[0].localeCompare(b.item[0]) || a.index - b.index).map(({ item }) => item); }
    entries() { return this._entries[Symbol.iterator](); }
    keys() { return this._entries.map((item) => item[0])[Symbol.iterator](); }
    values() { return this._entries.map((item) => item[1])[Symbol.iterator](); }
    forEach(callback, thisArg) { for (const [name, value] of this._entries) callback.call(thisArg, value, name, this); }
    toString() { return this._entries.map(([name, value]) => `${formEncode(name)}=${formEncode(value)}`).join("&"); }
    [Symbol.iterator]() { return this.entries(); }
  }

  // Collapses "." and ".." segments, which relative resolution otherwise leaves
  // in the path: `new URL(".", base).pathname` has to end at the directory, not
  // at a literal trailing dot.
  const normalizePath = (pathname) => {
    if (!/(^|\/)\.{1,2}(\/|$)/.test(pathname)) return pathname;
    const segments = pathname.split("/");
    const output = [];
    for (let index = 0; index < segments.length; index += 1) {
      const segment = segments[index];
      const last = index === segments.length - 1;
      if (segment === ".." && output.length > 1) output.pop();
      if (segment === "." || segment === "..") {
        // A dot segment at the end still leaves the trailing separator behind.
        if (last) output.push("");
        continue;
      }
      output.push(segment);
    }
    return output.join("/");
  };

  class URL {
    constructor(input, base) {
      input = String(input);
      if (!/^[A-Za-z][A-Za-z\d+.-]*:/.test(input)) {
        if (base === undefined) throw new TypeError("Invalid URL");
        const baseUrl = base instanceof URL ? base : new URL(base);
        // Resolve against the base's scheme and authority rather than its
        // origin. `origin` is the string "null" for a file: URL by definition,
        // and import.meta.url is a file: URL -- so a tool resolving anything
        // relative to its own location, which bundlers emit constantly, used to
        // parse "null/..." and throw.
        const root = `${baseUrl.protocol}${baseUrl._slashes ? `//${baseUrl.host}` : ""}`;
        if (input.startsWith("//")) input = `${baseUrl.protocol}${input}`;
        else if (input.startsWith("/")) input = `${root}${input}`;
        else if (input.startsWith("?") || input.startsWith("#")) input = `${root}${baseUrl.pathname}${input}`;
        else input = `${root}${baseUrl.pathname.slice(0, baseUrl.pathname.lastIndexOf("/") + 1)}${input}`;
      }
      const match = /^([A-Za-z][A-Za-z\d+.-]*:)(?:\/\/([^/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(input);
      if (!match) throw new TypeError("Invalid URL");
      this.protocol = match[1].toLowerCase();
      // Whether the input carried an authority at all, empty or not. "file:"
      // and "file://" are different URLs, and only this tells them apart once
      // the empty host has been parsed away.
      this._slashes = match[2] !== undefined;
      let authority = match[2] || "";
      const at = authority.lastIndexOf("@");
      let credentials = "";
      if (at >= 0) { credentials = authority.slice(0, at); authority = authority.slice(at + 1); }
      const colon = credentials.indexOf(":");
      this.username = decodeURIComponent(colon < 0 ? credentials : credentials.slice(0, colon));
      this.password = decodeURIComponent(colon < 0 ? "" : credentials.slice(colon + 1));
      const port = authority.startsWith("[") ? authority.indexOf("]") + 1 : authority.lastIndexOf(":");
      if (port > 0 && authority[port] === ":") {
        this.hostname = authority.slice(0, port).toLowerCase();
        this.port = authority.slice(port + 1);
      } else {
        this.hostname = authority.toLowerCase();
        this.port = "";
      }
      this.pathname = normalizePath(match[3] || (authority ? "/" : ""));
      this._searchParams = new URLSearchParams(match[4] || "");
      this.hash = match[5] || "";
    }
    get host() { return this.hostname + (this.port ? `:${this.port}` : ""); }
    set host(value) {
      const parsed = new URL(`${this.protocol}//${value}${this.pathname}`);
      this.hostname = parsed.hostname; this.port = parsed.port;
    }
    get origin() { return this.hostname ? `${this.protocol}//${this.host}` : "null"; }
    get search() { const value = this._searchParams.toString(); return value ? `?${value}` : ""; }
    set search(value) { this._searchParams = new URLSearchParams(value); }
    get searchParams() { return this._searchParams; }
    get href() {
      const credentials = this.username || this.password ? `${encodeURIComponent(this.username)}${this.password ? `:${encodeURIComponent(this.password)}` : ""}@` : "";
      return `${this.protocol}${this._slashes ? `//${credentials}${this.host}` : ""}${this.pathname}${this.search}${this.hash}`;
    }
    set href(value) { Object.assign(this, new URL(value)); }
    toString() { return this.href; }
    toJSON() { return this.href; }
  }

  class Event {
    constructor(type, options = {}) {
      this.type = String(type);
      this.bubbles = Boolean(options.bubbles);
      this.cancelable = Boolean(options.cancelable);
      this.composed = Boolean(options.composed);
      this.defaultPrevented = false;
      this.target = null;
      this.currentTarget = null;
      this.eventPhase = 0;
      this.timeStamp = 0;
      this._stopped = false;
    }
    preventDefault() { if (this.cancelable) this.defaultPrevented = true; }
    stopPropagation() { this._stopped = true; }
    stopImmediatePropagation() { this._stopped = true; }
  }

  class CustomEvent extends Event {
    constructor(type, options = {}) {
      super(type, options);
      this.detail = options.detail === undefined ? null : options.detail;
    }
  }

  // EventTarget, because AbortSignal is one and a growing number of packages
  // check for it by name rather than by capability. Only the parts that mean
  // anything without a DOM tree are here: there is no propagation path, so
  // capture and bubbling have nothing to do.
  class EventTarget {
    constructor() {
      Object.defineProperty(this, "_listeners", {
        value: new Map(), writable: true, enumerable: false, configurable: true,
      });
    }
    addEventListener(type, listener, options = {}) {
      if (listener === null || listener === undefined) return;
      const callable = typeof listener === "function" ? listener : listener.handleEvent;
      if (typeof callable !== "function") return;
      const name = String(type);
      const entries = this._listeners.get(name) ?? [];
      if (entries.some((entry) => entry.listener === listener)) return;
      entries.push({ listener, once: Boolean(options && options.once) });
      this._listeners.set(name, entries);
    }
    removeEventListener(type, listener) {
      const name = String(type);
      const entries = this._listeners.get(name);
      if (entries === undefined) return;
      this._listeners.set(name, entries.filter((entry) => entry.listener !== listener));
    }
    dispatchEvent(event) {
      const entries = this._listeners.get(String(event.type));
      if (entries === undefined || entries.length === 0) return true;
      event.target = this;
      event.currentTarget = this;
      // Copied first: a listener is allowed to add or remove listeners, and
      // must not change who is called for the event already in flight.
      const pending = entries.slice();
      this._listeners.set(String(event.type), entries.filter((entry) => !entry.once));
      for (const entry of pending) {
        if (event._stopped) break;
        const callable = typeof entry.listener === "function"
          ? entry.listener
          : entry.listener.handleEvent;
        callable.call(entry.listener === callable ? this : entry.listener, event);
      }
      return !event.defaultPrevented;
    }
  }

  class AbortSignal extends EventTarget {
    constructor() {
      super();
      this.aborted = false;
      this.reason = undefined;
      this.onabort = null;
    }
    static abort(reason = undefined) {
      const controller = new AbortController();
      controller.abort(reason);
      return controller.signal;
    }
    static timeout(milliseconds) {
      const controller = new AbortController();
      const timer = setTimeout(() => {
        const reason = new Error("The operation was aborted due to timeout");
        reason.name = "TimeoutError";
        controller.abort(reason);
      }, milliseconds);
      if (typeof timer === "object" && typeof timer.unref === "function") timer.unref();
      return controller.signal;
    }
    throwIfAborted() { if (this.aborted) throw this.reason; }
  }

  class AbortController {
    constructor() { this.signal = new AbortSignal(); }
    abort(reason = undefined) {
      if (this.signal.aborted) return;
      if (reason === undefined) {
        reason = new Error("This operation was aborted");
        reason.name = "AbortError";
      }
      this.signal.aborted = true;
      this.signal.reason = reason;
      if (typeof this.signal.onabort === "function") {
        this.signal.onabort.call(this.signal, new Event("abort"));
      }
      this.signal.dispatchEvent(new Event("abort"));
    }
  }

  class Headers {
    constructor(init = undefined) {
      this._values = new Map();
      if (init instanceof Headers) {
        for (const [name, value] of init) this.append(name, value);
      } else if (init && typeof init[Symbol.iterator] === "function" && typeof init !== "string") {
        for (const pair of init) {
          if (!pair || pair.length !== 2) throw new TypeError("header pair must contain two values");
          this.append(pair[0], pair[1]);
        }
      } else if (init && typeof init === "object") {
        for (const [name, value] of Object.entries(init)) this.append(name, value);
      }
    }
    _name(name) {
      name = String(name).toLowerCase();
      if (!name || !/^[!#$%&'*+.^_`|~0-9a-z-]+$/.test(name)) throw new TypeError("invalid header name");
      return name;
    }
    append(name, value) {
      name = this._name(name);
      value = String(value);
      if (/\r|\n/.test(value)) throw new TypeError("invalid header value");
      const current = this._values.get(name);
      this._values.set(name, current === undefined ? value : `${current}, ${value}`);
    }
    set(name, value) {
      name = this._name(name);
      value = String(value);
      if (/\r|\n/.test(value)) throw new TypeError("invalid header value");
      this._values.set(name, value);
    }
    get(name) { return this._values.get(this._name(name)) ?? null; }
    has(name) { return this._values.has(this._name(name)); }
    delete(name) { this._values.delete(this._name(name)); }
    entries() { return this._values.entries(); }
    keys() { return this._values.keys(); }
    values() { return this._values.values(); }
    forEach(callback, thisArg) { for (const [name, value] of this) callback.call(thisArg, value, name, this); }
    [Symbol.iterator]() { return this.entries(); }
  }

  const bodyBytes = (body) => {
    if (body === undefined || body === null) return Buffer.alloc(0);
    if (typeof body === "string") return Buffer.from(body);
    if (body instanceof ArrayBuffer || ArrayBuffer.isView(body) || Array.isArray(body)) return Buffer.from(body);
    throw new TypeError("request body must be a string or byte array");
  };

  class Request {
    constructor(input, init = {}) {
      const source = input instanceof Request ? input : null;
      this.url = String(source ? source.url : input);
      this.method = String(init.method || source?.method || "GET").toUpperCase();
      this.headers = new Headers(init.headers === undefined ? source?.headers : init.headers);
      this.signal = init.signal || source?.signal || null;
      this._body = bodyBytes(init.body === undefined ? source?._body : init.body);
      if ((this.method === "GET" || this.method === "HEAD") && this._body.length !== 0) {
        throw new TypeError(`${this.method} request cannot have a body`);
      }
    }
    clone() { return new Request(this); }
  }

  class Response {
    constructor(body = null, init = {}) {
      this.status = Number(init.status === undefined ? 200 : init.status);
      this.statusText = String(init.statusText || "");
      this.headers = new Headers(init.headers);
      this.url = String(init.url || "");
      this.redirected = Boolean(init.redirected);
      this.type = "basic";
      this.bodyUsed = false;
      this._body = bodyBytes(body);
    }
    get ok() { return this.status >= 200 && this.status <= 299; }
    async arrayBuffer() {
      if (this.bodyUsed) throw new TypeError("response body is already used");
      this.bodyUsed = true;
      const bytes = Uint8Array.from(this._body);
      return bytes.buffer;
    }
    async text() {
      if (this.bodyUsed) throw new TypeError("response body is already used");
      this.bodyUsed = true;
      return this._body.toString();
    }
    async json() { return JSON.parse(await this.text()); }
    clone() {
      if (this.bodyUsed) throw new TypeError("response body is already used");
      return new Response(this._body, { status: this.status, statusText: this.statusText, headers: this.headers, url: this.url, redirected: this.redirected });
    }
  }

  const fetch = async (input, init = {}) => {
    const request = new Request(input, init);
    if (request.signal?.aborted) {
      throw request.signal.reason;
    }
    const headers = [];
    for (const [name, value] of request.headers) headers.push(name, value);
    const native = __sakoFetchSync(request.url, request.method, headers, request._body);
    return new Response(native.body, {
      status: native.status,
      statusText: native.statusText,
      headers: native.headers.reduce((pairs, value, index) => {
        if (index % 2 === 0) pairs.push([value, native.headers[index + 1]]);
        return pairs;
      }, []),
      url: native.url,
      redirected: native.url !== request.url,
    });
  };

  class EventEmitter {
    constructor() { this._events = null; }
    on(name, listener) {
      if (typeof listener !== "function") throw new TypeError("listener must be a function");
      if (!this._events) this._events = new Map();
      const listeners = this._events.get(name) || [];
      listeners.push(listener);
      this._events.set(name, listeners);
      return this;
    }
    addListener(name, listener) { return this.on(name, listener); }
    // Order matters to callers that install a handler ahead of one a library
    // already registered -- Vite puts its own "error" listener in front of the
    // HTTP server's to turn a port clash into a retry.
    prependListener(name, listener) {
      if (typeof listener !== "function") throw new TypeError("listener must be a function");
      if (!this._events) this._events = new Map();
      const listeners = this._events.get(name) || [];
      listeners.unshift(listener);
      this._events.set(name, listeners);
      return this;
    }
    prependOnceListener(name, listener) {
      const wrapper = (...args) => { this.off(name, wrapper); listener.apply(this, args); };
      wrapper.listener = listener;
      return this.prependListener(name, wrapper);
    }
    once(name, listener) {
      const wrapper = (...args) => { this.off(name, wrapper); listener.apply(this, args); };
      wrapper.listener = listener;
      return this.on(name, wrapper);
    }
    off(name, listener) {
      const listeners = this._events?.get(name);
      if (listeners) this._events.set(name, listeners.filter((item) => item !== listener && item.listener !== listener));
      return this;
    }
    removeListener(name, listener) { return this.off(name, listener); }
    removeAllListeners(name) {
      if (!this._events) return this;
      if (arguments.length === 0) this._events.clear(); else this._events.delete(name);
      return this;
    }
    emit(name, ...args) {
      const listeners = this._events === null ? undefined : this._events.get(name);
      if (listeners === undefined || listeners.length === 0) {
        if (name === "error") throw args[0] instanceof Error ? args[0] : new Error(String(args[0]));
        return false;
      }
      if (listeners.length === 1) listeners[0].apply(this, args);
      else for (const listener of listeners.slice()) listener.apply(this, args);
      return true;
    }
    listeners(name) { return (this._events?.get(name) || []).map((item) => item.listener || item); }
    listenerCount(name) {
      const listeners = this._events === null ? undefined : this._events.get(name);
      return listeners === undefined ? 0 : listeners.length;
    }
    rawListeners(name) { return (this._events?.get(name) || []).slice(); }
    eventNames() { return this._events === null ? [] : [...this._events.keys()]; }
    // No listener ceiling is enforced, so these only keep the accessors honest
    // for code that reads back what it set.
    setMaxListeners(count) { this._maxListeners = count; return this; }
    getMaxListeners() { return this._maxListeners ?? 10; }
  }

  console.error = console.error || console.log;
  console.warn = console.warn || console.log;
  console.info = console.info || console.log;

  const separators = /[\\/]+/g;
  const pathWin32 = {
    sep: "\\",
    delimiter: ";",
    normalize(value) {
      value = String(value).replace(/\//g, "\\");
      const trailing = value.endsWith("\\");
      let root = "";
      let body = value;
      let rooted = false;
      const extendedUnc = /^\\\\\?\\UNC\\([^\\]+)\\([^\\]+)/i.exec(value);
      const extendedDrive = /^\\\\\?\\[A-Za-z]:\\/.exec(value);
      const unc = /^\\\\([^\\]+)\\([^\\]+)/.exec(value);
      const drive = /^[A-Za-z]:/.exec(value)?.[0] || "";
      if (extendedUnc) {
        root = `${extendedUnc[0]}\\`;
        body = value.slice(extendedUnc[0].length);
        rooted = true;
      } else if (extendedDrive) {
        root = extendedDrive[0];
        body = value.slice(root.length);
        rooted = true;
      } else if (unc) {
        root = `${unc[0]}\\`;
        body = value.slice(unc[0].length);
        rooted = true;
      } else if (drive) {
        rooted = value[2] === "\\";
        root = drive + (rooted ? "\\" : "");
        body = value.slice(root.length);
      } else if (value.startsWith("\\")) {
        root = "\\";
        body = value.replace(/^\\+/, "");
        rooted = true;
      }
      const parts = [];
      for (const part of body.split(separators)) {
        if (!part || part === ".") continue;
        if (part === ".." && parts.length && parts[parts.length - 1] !== "..") parts.pop();
        else if (part !== ".." || !rooted) parts.push(part);
      }
      const result = root + parts.join("\\");
      return result ? result + (trailing && parts.length ? "\\" : "") : ".";
    },
    join(...parts) { return pathWin32.normalize(parts.filter(Boolean).join("\\")); },
    resolve(...parts) {
      let value = "";
      for (let index = parts.length - 1; index >= -1; --index) {
        value = `${index < 0 ? __sakoCwd : parts[index]}\\${value}`;
        if (pathWin32.isAbsolute(value)) break;
      }
      // The loop above puts a separator after the last segment, and normalize
      // preserves trailing separators by design. resolve must not: Node keeps
      // one only when the result is a root, and a stray trailing separator
      // makes stat reject the path as a directory name.
      const resolved = pathWin32.normalize(value);
      if (resolved === "\\" || /^[A-Za-z]:\\$/.test(resolved)) return resolved;
      const trimmed = resolved.replace(/[\\/]+$/, "");
      if (/^[A-Za-z]:$/.test(trimmed)) return `${trimmed}\\`;
      return trimmed || "\\";
    },
    isAbsolute(value) { return /^(?:[A-Za-z]:[\\/]|[\\/]{2})/.test(String(value)); },
    basename(value, suffix = "") {
      const name = String(value).replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "";
      return suffix && name.endsWith(suffix) ? name.slice(0, -suffix.length) : name;
    },
    dirname(value) {
      const normalized = String(value).replace(/\//g, "\\").replace(/\\+$/, "");
      const index = normalized.lastIndexOf("\\");
      if (index < 0) return ".";
      if (index === 2 && /^[A-Za-z]:/.test(normalized)) return normalized.slice(0, 3);
      return normalized.slice(0, index) || "\\";
    },
    extname(value) {
      const name = pathWin32.basename(value);
      const index = name.lastIndexOf(".");
      return index <= 0 ? "" : name.slice(index);
    },
  };

  const posixSeparators = /\/+/g;
  const pathPosix = {
    sep: "/",
    delimiter: ":",
    normalize(value) {
      value = String(value);
      const rooted = value.startsWith("/");
      const trailing = value.length > 1 && value.endsWith("/");
      const parts = [];
      for (const part of value.split(posixSeparators)) {
        if (!part || part === ".") continue;
        if (part === ".." && parts.length && parts[parts.length - 1] !== "..") parts.pop();
        else if (part !== ".." || !rooted) parts.push(part);
      }
      const result = (rooted ? "/" : "") + parts.join("/");
      return result ? result + (trailing && parts.length ? "/" : "") : ".";
    },
    join(...parts) { return pathPosix.normalize(parts.filter(Boolean).join("/")); },
    resolve(...parts) {
      let value = "";
      for (let index = parts.length - 1; index >= -1; --index) {
        value = `${index < 0 ? __sakoCwd : parts[index]}/${value}`;
        if (pathPosix.isAbsolute(value)) break;
      }
      // See the win32 twin: the loop leaves a trailing separator that
      // normalize keeps, and resolve must only keep one for the root.
      const resolved = pathPosix.normalize(value);
      return resolved === "/" ? resolved : resolved.replace(/\/+$/, "") || "/";
    },
    isAbsolute(value) { return String(value).startsWith("/"); },
    basename(value, suffix = "") {
      const name = String(value).replace(/\/+$/, "").split("/").pop() || "";
      return suffix && name.endsWith(suffix) && name !== suffix ? name.slice(0, -suffix.length) : name;
    },
    dirname(value) {
      const normalized = String(value).replace(/\/+$/, "");
      const index = normalized.lastIndexOf("/");
      if (index < 0) return ".";
      if (index === 0) return "/";
      return normalized.slice(0, index);
    },
    extname(value) {
      const name = pathPosix.basename(value);
      const index = name.lastIndexOf(".");
      return index <= 0 ? "" : name.slice(index);
    },
  };

  // `__sakoPlatform` is set before this script runs (see bridge.cc), so the
  // right flavor is already known at bootstrap-eval time, before `process`
  // itself exists.
  // relative/parse/format/toNamespacedPath, shared by both flavors. Written
  // once against a flavor's own primitives so the win32 and posix versions
  // cannot drift.
  const addPathExtras = (flavor, sep, delimiter, caseInsensitive) => {
    flavor.sep = sep;
    flavor.delimiter = delimiter;
    flavor.relative = (from, to) => {
      const start = flavor.resolve(String(from));
      const end = flavor.resolve(String(to));
      if (start === end) return "";
      const fold = (value) => (caseInsensitive ? value.toLowerCase() : value);
      const split = (value) => value.split(/[\\/]/).filter(Boolean);
      const fromParts = split(start);
      const toParts = split(end);
      // A different root has no relative expression; Node returns the target.
      if (fold(fromParts[0] ?? "") !== fold(toParts[0] ?? "")) return end;
      let shared = 0;
      while (shared < fromParts.length && shared < toParts.length &&
             fold(fromParts[shared]) === fold(toParts[shared])) {
        shared += 1;
      }
      const up = new Array(fromParts.length - shared).fill("..");
      return [...up, ...toParts.slice(shared)].join(sep);
    };
    flavor.parse = (value) => {
      const text = String(value);
      const dir = flavor.dirname(text);
      const base = flavor.basename(text);
      const ext = flavor.extname(text);
      const rootMatch = caseInsensitive
        ? /^(?:[A-Za-z]:[\\/]|[\\/]{2}[^\\/]+[\\/][^\\/]+[\\/]?|[\\/])/.exec(text)
        : /^\//.exec(text);
      return {
        root: rootMatch ? rootMatch[0] : "",
        dir: dir === "." && !text.includes(sep) ? "" : dir,
        base,
        ext,
        name: ext ? base.slice(0, -ext.length) : base,
      };
    };
    flavor.format = (parsed = {}) => {
      const base = parsed.base ?? `${parsed.name ?? ""}${parsed.ext ?? ""}`;
      const dir = parsed.dir ?? parsed.root ?? "";
      if (!dir) return base;
      return dir === parsed.root ? `${dir}${base}` : `${dir}${sep}${base}`;
    };
  };
  addPathExtras(pathWin32, "\\", ";", true);
  addPathExtras(pathPosix, "/", ":", false);
  // Only Windows has a namespaced form; on posix Node returns the input.
  pathWin32.toNamespacedPath = (value) => {
    const text = String(value);
    if (!pathWin32.isAbsolute(text) || text.startsWith("\\\\?\\")) return text;
    const resolved = pathWin32.resolve(text);
    return resolved.startsWith("\\\\")
      ? `\\\\?\\UNC\\${resolved.slice(2)}`
      : `\\\\?\\${resolved}`;
  };
  pathPosix.toNamespacedPath = (value) => value;
  pathWin32.win32 = pathWin32;
  pathWin32.posix = pathPosix;
  pathPosix.win32 = pathWin32;
  pathPosix.posix = pathPosix;

  const path = globalThis.__sakoPlatform === "win32" ? pathWin32 : pathPosix;
  path.win32 = pathWin32;
  path.posix = pathPosix;

  class Stats {
    constructor(value) {
      Object.assign(this, value);
      // Node exposes each timestamp twice, as milliseconds and as a Date, and
      // callers reach for either: a build tool compares `mtimeMs`, a static
      // file server stamps Last-Modified from `mtime`. Sako records one
      // modification time, so the rest mirror it rather than being absent --
      // reading a missing one is a crash, not a graceful fallback.
      const stamp = typeof this.mtimeMs === "number" ? this.mtimeMs : 0;
      this.mtimeMs = this.atimeMs = this.ctimeMs = this.birthtimeMs = stamp;
      this.mtime = new Date(stamp);
      this.atime = new Date(stamp);
      this.ctime = new Date(stamp);
      this.birthtime = new Date(stamp);
      // Windows has no POSIX inode or ownership to report; the mode is the
      // conventional default for a readable file or directory.
      this.mode = this.directory ? 0o040755 : 0o100644;
      this.nlink = 1;
      this.ino = 0;
      this.dev = 0;
      this.rdev = 0;
      this.uid = 0;
      this.gid = 0;
      this.blksize = 4096;
      this.blocks = Math.ceil((this.size || 0) / 512);
    }
    isFile() { return this.file; }
    isDirectory() { return this.directory; }
    isSymbolicLink() { return this.symbolicLink; }
    isBlockDevice() { return false; }
    isCharacterDevice() { return false; }
    isFIFO() { return false; }
    isSocket() { return false; }
  }

  const statSync = (value, followLinks = true) => new Stats(__sakoStatSync(value, followLinks));
  const readdirSync = (value, options) => {
    const names = __sakoReadDirectorySync(value);
    if (!options || typeof options !== "object" || !options.withFileTypes) return names;
    return names.map((name) => {
      const stats = statSync(path.join(value, name), false);
      return { name, isFile: () => stats.isFile(), isDirectory: () => stats.isDirectory(), isSymbolicLink: () => stats.isSymbolicLink() };
    });
  };
  const mkdirSync = (value, options = {}) => __sakoMakeDirectorySync(value, Boolean(options && options.recursive));
  const rmSync = (value, options = {}) => __sakoRemovePathSync(value, Boolean(options && options.recursive), Boolean(options && options.force));
  const callbackOperation = (operation, callback) => {
    if (typeof callback !== "function") throw new TypeError("callback must be a function");
    queueMicrotask(() => {
      let value;
      try { value = operation(); } catch (error) { callback(error); return; }
      callback(null, value);
    });
  };
  const fs = {
    readFileSync(target, options) {
      // The native read already owns a fresh backing store sized to the file,
      // so the bytes become a Buffer by retagging the view. Handing them to
      // Buffer.from would copy the whole file a second time.
      const encoding = typeof options === "string"
        ? options
        : options && typeof options === "object" ? options.encoding : undefined;
      const value = encoding === undefined || encoding === null
        ? __sakoReadFileSync(target)
        : __sakoReadFileSync(target, encoding);
      if (typeof value === "string") return value;
      Object.setPrototypeOf(value, Buffer.prototype);
      return value;
    },
    writeFileSync: __sakoWriteFileSync,
    existsSync: __sakoExistsSync,
    statSync: (value) => statSync(value, true),
    lstatSync: (value) => statSync(value, false),
    readdirSync,
    mkdirSync,
    rmSync,
    rmdirSync: (value, options = {}) => rmSync(value, options),
    unlinkSync: (value) => __sakoRemovePathSync(value, false, false, true),
    renameSync: __sakoRenamePathSync,
    linkSync: __sakoLinkPathSync,
    symlinkSync: (target, value, type = "file") => __sakoSymlinkPathSync(target, value, type === "dir" || type === "junction"),
    readlinkSync: __sakoReadLinkSync,
    realpathSync: __sakoRealPathSync,
    openSync: __sakoOpenSync,
    closeSync: __sakoCloseSync,
    readSync(fd, buffer, offset = 0, length = buffer.length - offset, position = null) {
      return __sakoReadSync(fd, buffer, offset, length, position);
    },
    writeSync(fd, buffer, offset = 0, length, position = null) {
      const bytes = typeof buffer === "string" ? Buffer.from(buffer) : buffer;
      return __sakoWriteSync(fd, bytes, offset, length === undefined ? bytes.length - offset : length, position);
    },
    // The copy family, expressed through readFileSync/writeFileSync rather
    // than native calls. Scaffolding tools lean on these heavily -- copying a
    // template tree is the whole job -- so their absence stopped project
    // generators immediately after they had already created the directory.
    copyFileSync(source, destination, mode = 0) {
      // COPYFILE_EXCL: fail rather than overwrite an existing destination.
      if ((mode & 1) !== 0 && __sakoExistsSync(String(destination))) {
        const error = new Error(`EEXIST: file already exists, copyfile '${source}' -> '${destination}'`);
        error.code = "EEXIST";
        throw error;
      }
      __sakoWriteFileSync(String(destination), __sakoReadFileSync(String(source), false));
    },
    cpSync(source, destination, options = {}) {
      const from = String(source);
      const to = String(destination);
      // statSync/readdirSync/path.join rather than the raw __sako* calls: the
      // wrappers normalize arguments and return a Stats object whose
      // isDirectory is a method, and path.join avoids the doubled separators
      // that hand-built paths produce.
      if (!statSync(from).isDirectory()) {
        if (options.force === false && __sakoExistsSync(to)) return;
        fs.copyFileSync(from, to);
        return;
      }
      if (options.recursive === false) {
        const error = new Error(`EISDIR: illegal operation on a directory, cp '${from}'`);
        error.code = "EISDIR";
        throw error;
      }
      mkdirSync(to, { recursive: true });
      for (const name of readdirSync(from)) {
        if (!name) continue;
        fs.cpSync(path.join(from, name), path.join(to, name), options);
      }
    },
    appendFileSync(target, data, options) {
      const existing = __sakoExistsSync(String(target)) ? __sakoReadFileSync(String(target), false) : Buffer.alloc(0);
      const addition = typeof data === "string" ? Buffer.from(data, typeof options === "string" ? options : (options && options.encoding) || "utf8") : data;
      __sakoWriteFileSync(String(target), Buffer.concat([Buffer.from(existing), Buffer.from(addition)]));
    },
    accessSync(target) {
      if (!__sakoExistsSync(String(target))) {
        const error = new Error(`ENOENT: no such file or directory, access '${target}'`);
        error.code = "ENOENT";
        throw error;
      }
    },
    mkdtempSync(prefix) {
      // Node guarantees six random characters; the loop guards the collision
      // that Math.random alone does not.
      for (let attempt = 0; attempt < 64; attempt += 1) {
        const suffix = Math.random().toString(36).slice(2, 8).padEnd(6, "0");
        const candidate = `${prefix}${suffix}`;
        if (__sakoExistsSync(candidate)) continue;
        __sakoMakeDirectorySync(candidate, true);
        return candidate;
      }
      throw new Error("EEXIST: cannot create a unique temporary directory");
    },
    // Windows has no POSIX mode bits and Sako exposes no chmod, so this is a
    // no-op rather than a failure: callers use it to set an execute bit that
    // does not exist here.
    chmodSync() {},
  };
  fs.constants = {
    F_OK: 0, X_OK: 1, W_OK: 2, R_OK: 4,
    COPYFILE_EXCL: 1, COPYFILE_FICLONE: 2, COPYFILE_FICLONE_FORCE: 4,
  };
  fs.readFile = (...args) => { const callback = args.pop(); callbackOperation(() => fs.readFileSync(...args), callback); };
  fs.writeFile = (...args) => { const callback = args.pop(); callbackOperation(() => fs.writeFileSync(...args), callback); };
  fs.stat = (value, callback) => callbackOperation(() => fs.statSync(value), callback);
  fs.lstat = (value, callback) => callbackOperation(() => fs.lstatSync(value), callback);
  fs.readdir = (value, options, callback) => {
    if (typeof options === "function") { callback = options; options = undefined; }
    callbackOperation(() => fs.readdirSync(value, options), callback);
  };
  fs.mkdir = (value, options, callback) => {
    if (typeof options === "function") { callback = options; options = undefined; }
    callbackOperation(() => fs.mkdirSync(value, options), callback);
  };
  fs.rm = (value, options, callback) => {
    if (typeof options === "function") { callback = options; options = undefined; }
    callbackOperation(() => fs.rmSync(value, options), callback);
  };
  fs.unlink = (value, callback) => callbackOperation(() => fs.unlinkSync(value), callback);
  fs.rename = (source, destination, callback) => callbackOperation(() => fs.renameSync(source, destination), callback);
  fs.link = (source, destination, callback) => callbackOperation(() => fs.linkSync(source, destination), callback);
  fs.symlink = (target, value, type, callback) => {
    if (typeof type === "function") { callback = type; type = undefined; }
    callbackOperation(() => fs.symlinkSync(target, value, type), callback);
  };
  fs.readlink = (value, callback) => callbackOperation(() => fs.readlinkSync(value), callback);
  fs.realpath = (value, callback) => callbackOperation(() => fs.realpathSync(value), callback);
  // Node exposes a second, OS-native resolver under .native. There is only one
  // here, but tooling picks the native form when it exists and crashes on the
  // property access rather than falling back.
  fs.realpathSync.native = (value) => fs.realpathSync(value);
  fs.realpath.native = (value, callback) => fs.realpath(value, callback);
  fs.open = (value, flags, mode, callback) => {
    if (typeof mode === "function") { callback = mode; mode = undefined; }
    callbackOperation(() => fs.openSync(value, flags, mode), callback);
  };
  fs.close = (fd, callback) => callbackOperation(() => fs.closeSync(fd), callback);
  fs.read = (fd, buffer, offset, length, position, callback) => {
    if (typeof callback !== "function") throw new TypeError("callback must be a function");
    queueMicrotask(() => {
      try { callback(null, fs.readSync(fd, buffer, offset, length, position), buffer); }
      catch (error) { callback(error, 0, buffer); }
    });
  };
  fs.write = (fd, buffer, offset, length, position, callback) => {
    if (typeof callback !== "function") throw new TypeError("callback must be a function");
    queueMicrotask(() => {
      try { callback(null, fs.writeSync(fd, buffer, offset, length, position), buffer); }
      catch (error) { callback(error, 0, buffer); }
    });
  };
  class FileHandle {
    constructor(fd) { this.fd = fd; }
    async close() { fs.closeSync(this.fd); }
    async read(buffer, offset = 0, length = buffer.length - offset, position = null) {
      return { bytesRead: fs.readSync(this.fd, buffer, offset, length, position), buffer };
    }
    async write(buffer, offset = 0, length, position = null) {
      return { bytesWritten: fs.writeSync(this.fd, buffer, offset, length, position), buffer };
    }
  }
  const fsPromises = {
    async readFile(...args) { return fs.readFileSync(...args); },
    async writeFile(...args) { fs.writeFileSync(...args); },
    async stat(value) { return fs.statSync(value); },
    async lstat(value) { return fs.lstatSync(value); },
    async readdir(value, options) { return fs.readdirSync(value, options); },
    async mkdir(value, options) { return fs.mkdirSync(value, options); },
    async rm(value, options) { return fs.rmSync(value, options); },
    async unlink(value) { return fs.unlinkSync(value); },
    async rename(source, destination) { return fs.renameSync(source, destination); },
    async link(source, destination) { return fs.linkSync(source, destination); },
    async symlink(target, value, type) { return fs.symlinkSync(target, value, type); },
    async readlink(value) { return fs.readlinkSync(value); },
    async realpath(value) { return fs.realpathSync(value); },
    async open(value, flags, mode) { return new FileHandle(fs.openSync(value, flags, mode)); },
  };
  fs.promises = fsPromises;

  // util.styleText, added in Node 20.12. CLI tooling has adopted it quickly as
  // a way to colour output without a dependency, so a missing export here
  // breaks such a tool at import time rather than at the call.
  const STYLE_CODES = {
    reset: [0, 0], bold: [1, 22], dim: [2, 22], italic: [3, 23],
    underline: [4, 24], blink: [5, 25], inverse: [7, 27], hidden: [8, 28],
    strikethrough: [9, 29],
    black: [30, 39], red: [31, 39], green: [32, 39], yellow: [33, 39],
    blue: [34, 39], magenta: [35, 39], cyan: [36, 39], white: [37, 39],
    gray: [90, 39], grey: [90, 39], blackBright: [90, 39], redBright: [91, 39],
    greenBright: [92, 39], yellowBright: [93, 39], blueBright: [94, 39],
    magentaBright: [95, 39], cyanBright: [96, 39], whiteBright: [97, 39],
    bgBlack: [40, 49], bgRed: [41, 49], bgGreen: [42, 49], bgYellow: [43, 49],
    bgBlue: [44, 49], bgMagenta: [45, 49], bgCyan: [46, 49], bgWhite: [47, 49],
  };
  const styleText = (format, text, options = {}) => {
    if (typeof text !== "string") throw new TypeError("styleText needs a string");
    const formats = Array.isArray(format) ? format : [format];
    // Node skips styling when the target stream is not a terminal, so piped
    // output stays free of escapes.
    const stream = options.stream;
    if (stream && stream.isTTY === false) return text;
    let result = text;
    for (let index = formats.length - 1; index >= 0; index -= 1) {
      const code = STYLE_CODES[formats[index]];
      if (code === undefined) throw new TypeError(`unknown style: ${formats[index]}`);
      result = `\x1b[${code[0]}m${result}\x1b[${code[1]}m`;
    }
    return result;
  };

  const isDeepStrictEqual = (left, right) => {
    if (Object.is(left, right)) return true;
    if (typeof left !== "object" || typeof right !== "object" || left === null || right === null) return false;
    if (Object.getPrototypeOf(left) !== Object.getPrototypeOf(right)) return false;
    if (Array.isArray(left) !== Array.isArray(right)) return false;
    const leftKeys = Reflect.ownKeys(left);
    const rightKeys = Reflect.ownKeys(right);
    if (leftKeys.length !== rightKeys.length) return false;
    return leftKeys.every((key) => Reflect.has(right, key) && isDeepStrictEqual(left[key], right[key]));
  };

  /// util.parseArgs. Supports the options CLIs actually use: long flags with
  /// = or a following value, short aliases, booleans, and multiples. Not
  /// supported: strict-mode error shapes beyond unknown-option detection.
  const parseArgs = (config = {}) => {
    const options = config.options ?? {};
    const args = config.args ?? process.argv.slice(2);
    const allowPositionals = config.allowPositionals ?? !config.strict;
    const values = {};
    const positionals = [];
    const byShort = new Map(
      Object.entries(options)
        .filter(([, option]) => option.short)
        .map(([name, option]) => [option.short, name]),
    );
    const assign = (name, value) => {
      const option = options[name] ?? {};
      if (option.multiple) (values[name] ??= []).push(value);
      else values[name] = value;
    };
    for (let index = 0; index < args.length; index += 1) {
      const argument = String(args[index]);
      if (argument === "--") { positionals.push(...args.slice(index + 1).map(String)); break; }
      let name = null;
      let inline;
      if (argument.startsWith("--")) {
        const equals = argument.indexOf("=");
        name = equals === -1 ? argument.slice(2) : argument.slice(2, equals);
        if (equals !== -1) inline = argument.slice(equals + 1);
      } else if (argument.startsWith("-") && argument.length > 1) {
        name = byShort.get(argument.slice(1, 2)) ?? argument.slice(1, 2);
        if (argument.length > 2) inline = argument.slice(2);
      }
      if (name === null) {
        if (!allowPositionals && config.strict) throw new TypeError(`unexpected argument '${argument}'`);
        positionals.push(argument);
        continue;
      }
      const option = options[name];
      if (option === undefined && config.strict) throw new TypeError(`unknown option '${argument}'`);
      if ((option?.type ?? "boolean") === "boolean") { assign(name, true); if (inline !== undefined) positionals.push(inline); continue; }
      if (inline !== undefined) { assign(name, inline); continue; }
      index += 1;
      if (index >= args.length) throw new TypeError(`option '${name}' needs a value`);
      assign(name, String(args[index]));
    }
    for (const [name, option] of Object.entries(options)) {
      if (values[name] === undefined && option.default !== undefined) values[name] = option.default;
    }
    return { values, positionals };
  };

  // Strips ANSI escapes. Tools use it to measure the printable width of
  // already-coloured text, so the pattern has to cover the cursor and erase
  // sequences a TUI emits, not just SGR colour.
  const stripVTControlCharacters = (value) =>
    String(value).replace(/(?:\[[0-?]*[ -/]*[@-~]|\][^]*(?:|\\)|[@-Z\\-_])/g, "");

  /// Parses .env content the way util.parseEnv does: KEY=value per line, with
  /// optional `export `, # comments, and single, double, or backtick quoting.
  /// Escape sequences are expanded only inside double quotes, matching dotenv.
  const parseEnv = (content) => {
    const result = {};
    for (let line of String(content).split(/\r?\n/)) {
      line = line.trim();
      if (!line || line.startsWith("#")) continue;
      if (line.startsWith("export ")) line = line.slice(7).trim();
      const equals = line.indexOf("=");
      if (equals <= 0) continue;
      const key = line.slice(0, equals).trim();
      if (!/^[\w.-]+$/.test(key)) continue;
      let value = line.slice(equals + 1).trim();
      const quote = value[0];
      if ((quote === '"' || quote === "'" || quote === "`") && value.endsWith(quote) && value.length > 1) {
        value = value.slice(1, -1);
        if (quote === '"') {
          value = value.replace(/\\n/g, "\n").replace(/\\r/g, "\r").replace(/\\t/g, "\t").replace(/\\"/g, '"').replace(/\\\\/g, "\\");
        }
      } else {
        // Unquoted values end at an inline comment.
        const comment = value.indexOf(" #");
        if (comment !== -1) value = value.slice(0, comment).trimEnd();
      }
      result[key] = value;
    }
    return result;
  };

  const util = {
    styleText,
    parseArgs,
    parseEnv,
    stripVTControlCharacters,
    isDeepStrictEqual,
    types: {
      isDate: (value) => value instanceof Date,
      isRegExp: (value) => value instanceof RegExp,
      isMap: (value) => value instanceof Map,
      isSet: (value) => value instanceof Set,
      isPromise: (value) => value instanceof Promise,
      isTypedArray: (value) => ArrayBuffer.isView(value) && !(value instanceof DataView),
      isArrayBuffer: (value) => value instanceof ArrayBuffer,
      isNativeError: (value) => value instanceof Error,
    },
    inherits(ctor, superCtor) {
      Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
      Object.setPrototypeOf(ctor, superCtor);
      ctor.super_ = superCtor;
    },
    promisify(fn) {
      return function (...args) {
        return new Promise((resolve, reject) => fn.call(this, ...args, (error, value) => error ? reject(error) : resolve(value)));
      };
    },
    deprecate(fn) { return function (...args) { return fn.apply(this, args); }; },
    inspect(value) {
      if (typeof value === "string") return `'${value}'`;
      try { return JSON.stringify(value) ?? String(value); } catch { return String(value); }
    },
    format(format, ...values) {
      if (typeof format !== "string") return [format, ...values].map((value) => util.inspect(value)).join(" ");
      let index = 0;
      const output = format.replace(/%[sdijoO%]/g, (token) => {
        if (token === "%%") return "%";
        if (index >= values.length) return token;
        const value = values[index++];
        if (token === "%s") return String(value);
        if (token === "%d" || token === "%i") return String(Number(value));
        if (token === "%j") { try { return JSON.stringify(value); } catch { return "[Circular]"; } }
        return util.inspect(value);
      });
      return index < values.length ? `${output} ${values.slice(index).map((value) => util.inspect(value)).join(" ")}` : output;
    },
  };
  util.formatWithOptions = (_options, ...args) => util.format(...args);

  const tty = {
    isatty: __sakoIsTty,
    ReadStream: class ReadStream {},
    WriteStream: class WriteStream {},
  };

  const unavailable = (name) => () => {
    throw new Error(`${name} is not implemented by Sako.js`);
  };
  const zlib = {
    createInflate: unavailable("zlib.createInflate"),
    createGunzip: unavailable("zlib.createGunzip"),
    createBrotliDecompress: unavailable("zlib.createBrotliDecompress"),
  };
  for (const name of ["Gzip", "Gunzip", "Deflate", "DeflateRaw", "Inflate", "InflateRaw", "Unzip"]) {
    zlib[name] = class UnsupportedZlibStream {};
  }

  // node:http2 exists so that bundles importing it up front can load. A dev
  // server's proxy pulls it in whether or not any route is configured to use
  // it, and a missing module stops the whole program at import time rather
  // than at the call that would actually need HTTP/2.
  const http2 = {
    createServer: unavailable("http2.createServer"),
    createSecureServer: unavailable("http2.createSecureServer"),
    connect: unavailable("http2.connect"),
    getDefaultSettings: () => ({}),
    constants: {
      HTTP2_HEADER_METHOD: ":method",
      HTTP2_HEADER_PATH: ":path",
      HTTP2_HEADER_STATUS: ":status",
      HTTP2_HEADER_AUTHORITY: ":authority",
      HTTP2_HEADER_SCHEME: ":scheme",
    },
  };

  // Math.random is not a cryptographic source. These exist so tooling that
  // wants an identifier or filler bytes works; nothing here should be used for
  // keys, tokens, or anything an attacker gets to see and predict.
  const randomFillBytes = (view) => {
    for (let index = 0; index < view.length; index += 1) {
      view[index] = Math.floor(Math.random() * 256);
    }
    return view;
  };
  const randomUUID = () => {
    const bytes = randomFillBytes(new Uint8Array(16));
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 1
    const hex = [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
    return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  };

  // Selectors for __sakoHash, mirroring SAKO_HASH_* in sako-v8's lib.rs. SHA-256
  // is not optional in practice: TypeScript's build mode hashes every source
  // file with it, so tsc could not run at all while SHA-1 was the only choice.
  const HASH_ALGORITHMS = { sha1: 1, sha256: 2, sha512: 3 };

  const crypto = {
    randomUUID,
    randomBytes(size, callback) {
      const bytes = Buffer.from(randomFillBytes(new Uint8Array(Number(size))));
      if (typeof callback === "function") { callback(null, bytes); return undefined; }
      return bytes;
    },
    getRandomValues: randomFillBytes,
    // The one-shot form Node added in 20.12 / 21.7. Tooling reached for it
    // immediately -- Vite hashes its lockfile with it on every start -- and it
    // is only createHash without the ceremony.
    hash(algorithm, data, outputEncoding = "hex") {
      const digest = crypto.createHash(algorithm).update(data).digest();
      return outputEncoding === "buffer" ? digest : digest.toString(outputEncoding);
    },

    createHmac(algorithm, key) {
      // RFC 2104 on top of the digests the runtime already computes. The block
      // size is the only thing that varies between them.
      const blockSize = String(algorithm).toLowerCase().includes("512") ? 128 : 64;
      let secret = Buffer.isBuffer(key) ? key : Buffer.from(key);
      if (secret.length > blockSize) {
        secret = crypto.createHash(algorithm).update(secret).digest();
      }
      const inner = Buffer.alloc(blockSize);
      const outer = Buffer.alloc(blockSize);
      for (let index = 0; index < blockSize; index += 1) {
        const byte = index < secret.length ? secret[index] : 0;
        inner[index] = byte ^ 0x36;
        outer[index] = byte ^ 0x5c;
      }
      const stream = crypto.createHash(algorithm).update(inner);
      return {
        update(value, encoding) { stream.update(value, encoding); return this; },
        digest(encoding) {
          const digest = crypto.createHash(algorithm)
            .update(outer)
            .update(stream.digest())
            .digest();
          return encoding === undefined ? digest : digest.toString(encoding);
        },
      };
    },

    createHash(algorithm) {
      const selector = HASH_ALGORITHMS[String(algorithm).toLowerCase().replace(/-/g, "")];
      if (selector === undefined) {
        throw new Error(`Unsupported hash algorithm: ${algorithm}`);
      }
      let chunks = [];
      let length = 0;
      let finalized = false;
      return {
        update(value, encoding) {
          if (finalized) throw new Error("Digest already called");
          const bytes = Buffer.isBuffer(value) ? value : Buffer.from(value, encoding);
          length += bytes.length;
          if (length > 256 * 1024 * 1024) throw new RangeError("hash input exceeds byte limit");
          chunks.push(bytes);
          return this;
        },
        digest(encoding) {
          if (finalized) throw new Error("Digest already called");
          finalized = true;
          const digest = Buffer.from(__sakoHash(selector, Buffer.concat(chunks, length)));
          chunks = [];
          return encoding === undefined ? digest : digest.toString(encoding);
        },
      };
    },
  };

  const STATUS_CODES = {
    200: "OK", 201: "Created", 202: "Accepted", 204: "No Content",
    301: "Moved Permanently", 302: "Found", 304: "Not Modified",
    400: "Bad Request", 401: "Unauthorized", 403: "Forbidden",
    404: "Not Found", 405: "Method Not Allowed", 406: "Not Acceptable",
    409: "Conflict", 413: "Payload Too Large", 415: "Unsupported Media Type",
    422: "Unprocessable Entity", 429: "Too Many Requests",
    500: "Internal Server Error", 501: "Not Implemented", 503: "Service Unavailable",
  };
  const METHODS = ["ACL", "BIND", "CHECKOUT", "CONNECT", "COPY", "DELETE", "GET", "HEAD", "LINK", "LOCK", "M-SEARCH", "MERGE", "MKACTIVITY", "MKCALENDAR", "MKCOL", "MOVE", "NOTIFY", "OPTIONS", "PATCH", "POST", "PROPFIND", "PROPPATCH", "PURGE", "PUT", "REBIND", "REPORT", "SEARCH", "SOURCE", "SUBSCRIBE", "TRACE", "UNBIND", "UNLINK", "UNLOCK", "UNSUBSCRIBE"];

  class Stream extends EventEmitter {
    constructor() { super(); this.destroyed = false; }
    pipe(destination) {
      this.on("data", (chunk) => destination.write(chunk));
      this.once("end", () => destination.end());
      return destination;
    }
    destroy(error) {
      if (this.destroyed) return this;
      this.destroyed = true;
      if (error) this.emit("error", error);
      this.emit("close");
      return this;
    }
  }
  class Readable extends Stream {
    constructor() { super(); this.readable = true; this.readableEnded = false; }
    resume() { return this; }
    pause() { return this; }
    unpipe() { return this; }
    push(chunk) {
      if (chunk === null) { this.readableEnded = true; this.emit("end"); return false; }
      this.emit("data", chunk);
      return true;
    }
  }
  class Writable extends Stream {
    constructor() { super(); this.writable = true; this.writableEnded = false; }
    write(chunk) { this.emit("data", chunk); return true; }
    end(chunk) { if (chunk !== undefined) this.write(chunk); this.writableEnded = true; this.emit("finish"); return this; }
  }
  class Duplex extends Readable {
    write(chunk) { this.emit("data", chunk); return true; }
    end(chunk) { if (chunk !== undefined) this.write(chunk); this.writableEnded = true; this.emit("finish"); return this; }
  }
  class Transform extends Duplex {
    _destroy(error, callback) { callback(error); }
  }
  class PassThrough extends Transform {}
  Object.assign(Stream, { Stream, Readable, Writable, Duplex, Transform, PassThrough });
  Stream.pipeline = (...args) => {
    const callback = typeof args[args.length - 1] === "function" ? args.pop() : undefined;
    for (let index = 0; index + 1 < args.length; ++index) args[index].pipe(args[index + 1]);
    if (callback) args[args.length - 1].once("finish", callback);
    return args[args.length - 1];
  };
  Stream.finished = (stream, callback) => { stream.once("finish", callback); stream.once("end", callback); return () => {}; };
  // Sako reads and writes files whole, so these are streams in shape rather
  // than in mechanism: a read stream delivers the file in one chunk a
  // microtask later, a write stream collects everything and commits on end.
  // What consumes them -- a static file server piping to a response, a
  // download being saved -- only needs the events in the right order.
  fs.ReadStream = class ReadStream extends Readable {
    constructor(target, options) {
      super();
      const settings = typeof options === "string" ? { encoding: options } : (options || {});
      this.path = target;
      this.bytesRead = 0;
      queueMicrotask(() => {
        let bytes;
        try {
          bytes = fs.readFileSync(target);
        } catch (error) {
          this.emit("error", error);
          return;
        }
        // start/end are inclusive in Node, which is what a Range request
        // header means by them.
        const start = Number.isInteger(settings.start) ? settings.start : 0;
        const end = Number.isInteger(settings.end) ? settings.end + 1 : bytes.length;
        const slice = bytes.subarray(start, Math.max(start, end));
        this.bytesRead = slice.length;
        this.emit("open", 0);
        this.emit("ready");
        this.push(settings.encoding ? slice.toString(settings.encoding) : slice);
        this.push(null);
        this.emit("close");
      });
    }
  };
  fs.WriteStream = class WriteStream extends Writable {
    constructor(target, options) {
      super();
      this._settings = typeof options === "string" ? { encoding: options } : (options || {});
      this.path = target;
      this.bytesWritten = 0;
      this._parts = [];
    }
    write(chunk, encoding, callback) {
      if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
      const bytes = Buffer.isBuffer(chunk)
        ? chunk
        : Buffer.from(String(chunk), encoding || this._settings.encoding);
      this._parts.push(bytes);
      this.bytesWritten += bytes.length;
      if (typeof callback === "function") queueMicrotask(callback);
      return true;
    }
    end(chunk, encoding, callback) {
      if (typeof chunk === "function") { callback = chunk; chunk = undefined; }
      else if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
      if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
      this.writableEnded = true;
      try {
        fs.writeFileSync(this.path, Buffer.concat(this._parts));
      } catch (error) {
        this.emit("error", error);
        return this;
      }
      if (typeof callback === "function") callback();
      this.emit("finish");
      this.emit("close");
      return this;
    }
  };
  fs.createReadStream = (target, options) => new fs.ReadStream(target, options);
  fs.createWriteStream = (target, options) => new fs.WriteStream(target, options);
  const activeWatchers = new Set();
  const watchedFiles = new Map();
  const watchSnapshot = (value) => {
    const stats = fs.statSync(value);
    if (!stats.isDirectory()) return `${stats.size}:${stats.mtimeMs}`;
    return fs.readdirSync(value).map((name) => {
      try {
        const entry = fs.lstatSync(path.join(value, name));
        return `${name}:${entry.size}:${entry.mtimeMs}`;
      } catch { return `${name}:missing`; }
    }).join("|");
  };
  class FSWatcher extends EventEmitter {
    constructor(value, options, listener) {
      super();
      if (activeWatchers.size >= 1024) throw new RangeError("filesystem watcher capacity exceeded");
      if (options?.recursive) throw new Error("recursive filesystem watching is not implemented");
      this._value = String(value);
      this._closed = false;
      this._snapshot = watchSnapshot(this._value);
      if (typeof listener === "function") this.on("change", listener);
      activeWatchers.add(this);
      const interval = Math.max(25, Number(options?.interval) || 100);
      this._timer = setInterval(() => {
        let current;
        try { current = watchSnapshot(this._value); } catch { current = "<missing>"; }
        if (current === this._snapshot) return;
        const event = current === "<missing>" || this._snapshot === "<missing>" ? "rename" : "change";
        this._snapshot = current;
        this.emit("change", event, path.basename(this._value));
      }, interval);
    }
    close() {
      if (this._closed) return;
      this._closed = true;
      clearInterval(this._timer);
      activeWatchers.delete(this);
      this.emit("close");
    }
    ref() { return this; }
    unref() { return this; }
  }
  fs.FSWatcher = FSWatcher;
  fs.watch = (value, options, listener) => {
    if (typeof options === "function") { listener = options; options = {}; }
    else if (typeof options === "string") options = { encoding: options };
    return new FSWatcher(value, options || {}, listener);
  };
  fs.watchFile = (value, options, listener) => {
    if (typeof options === "function") { listener = options; options = {}; }
    if (typeof listener !== "function") throw new TypeError("watchFile needs a listener");
    value = String(value);
    fs.unwatchFile(value, listener);
    let previous = fs.statSync(value);
    const timer = setInterval(() => {
      let current;
      try { current = fs.statSync(value); } catch { return; }
      if (current.size !== previous.size || current.mtimeMs !== previous.mtimeMs) {
        const old = previous;
        previous = current;
        listener(current, old);
      }
    }, Math.max(25, Number(options?.interval) || 5007));
    const entries = watchedFiles.get(value) || [];
    if (entries.length >= 1024) { clearInterval(timer); throw new RangeError("watchFile capacity exceeded"); }
    entries.push({ listener, timer });
    watchedFiles.set(value, entries);
  };
  fs.unwatchFile = (value, listener) => {
    value = String(value);
    const entries = watchedFiles.get(value) || [];
    const remaining = [];
    for (const entry of entries) {
      if (listener === undefined || entry.listener === listener) clearInterval(entry.timer);
      else remaining.push(entry);
    }
    if (remaining.length) watchedFiles.set(value, remaining);
    else watchedFiles.delete(value);
  };

  class IncomingMessage extends Readable {
    constructor() {
      super();
      this.complete = true;
      this.readable = true;
      this.readableEnded = false;
      this._headerBytes = undefined;
      this._headerRanges = undefined;
      this._rawHeaders = undefined;
      this._normalizedHeaders = undefined;
    }
    // Request headers stay as the bytes the parser produced until something
    // asks for them, so a handler that ignores headers never decodes any.
    get rawHeaders() {
      if (this._rawHeaders !== undefined) return this._rawHeaders;
      const bytes = this._headerBytes;
      const ranges = this._headerRanges;
      const values = [];
      if (ranges !== undefined) {
        for (let index = 0; index < ranges.length; index += 4) {
          values.push(
            __sakoDecodeUtf8(bytes.subarray(ranges[index], ranges[index] + ranges[index + 1])),
            __sakoDecodeUtf8(bytes.subarray(ranges[index + 2], ranges[index + 2] + ranges[index + 3])),
          );
        }
      }
      this._rawHeaders = values;
      return values;
    }
    set rawHeaders(value) { this._rawHeaders = value; }
    get headers() {
      if (this._normalizedHeaders !== undefined) return this._normalizedHeaders;
      const normalized = Object.create(null);
      const values = this.rawHeaders;
      for (let index = 0; index < values.length; index += 2) {
        const name = values[index].toLowerCase();
        const value = values[index + 1];
        normalized[name] = normalized[name] === undefined ? value : `${normalized[name]}, ${value}`;
      }
      this._normalizedHeaders = normalized;
      return normalized;
    }
    set headers(value) { this._normalizedHeaders = value; }
    resume() { return this; }
    pipe(destination) { return destination; }
    unpipe() { return this; }
    destroy(error) { if (error) this.emit("error", error); this.emit("close"); return this; }
  }

  class ServerResponse extends Writable {
    constructor(request) {
      super();
      this.req = request;
      this.statusCode = 200;
      this.statusMessage = undefined;
      this.headersSent = false;
      this.finished = false;
      this.writableEnded = false;
      this._headers = new Map();
      this._chunks = [];
    }
    setHeader(name, value) {
      name = String(name);
      this._headers.set(name.toLowerCase(), [name, value]);
      return this;
    }
    getHeader(name) { return this._headers.get(String(name).toLowerCase())?.[1]; }
    getHeaderNames() { return Array.from(this._headers.keys()); }
    getHeaders() { return Object.fromEntries(Array.from(this._headers, ([name, pair]) => [name, pair[1]])); }
    hasHeader(name) { return this._headers.has(String(name).toLowerCase()); }
    // Node 18.3 added this, and middleware that adds a Vary or a Set-Cookie on
    // top of one already set reaches for it rather than reading and rewriting.
    appendHeader(name, value) {
      const key = String(name).toLowerCase();
      const existing = this._headers.get(key);
      if (existing === undefined) return this.setHeader(name, value);
      const merged = Array.isArray(existing[1]) ? existing[1].slice() : [existing[1]];
      if (Array.isArray(value)) merged.push(...value); else merged.push(value);
      this._headers.set(key, [existing[0], merged]);
      return this;
    }
    setHeaders(headers) {
      if (headers && typeof headers.forEach === 'function' && !Array.isArray(headers)) {
        headers.forEach((value, name) => this.setHeader(name, value));
      } else if (headers) {
        for (const [name, value] of Object.entries(headers)) this.setHeader(name, value);
      }
      return this;
    }
    // Headers reach the socket with the body, so there is nothing to flush.
    flushHeaders() { this.headersSent = true; }
    removeHeader(name) { this._headers.delete(String(name).toLowerCase()); }
    writeHead(statusCode, statusMessage, headers) {
      this.statusCode = Number(statusCode);
      if (typeof statusMessage === "string") this.statusMessage = statusMessage;
      else { headers = statusMessage; }
      if (headers) for (const [name, value] of Object.entries(headers)) this.setHeader(name, value);
      this.headersSent = true;
      return this;
    }
    write(chunk, encoding) {
      if (this.writableEnded) throw new Error("write after end");
      this.headersSent = true;
      if (chunk === undefined) return true;
      // A plain UTF-8 string is handed to the native encoder as-is; converting
      // it to a Buffer here would copy the body an extra time per response.
      if (typeof chunk === "string" && (encoding === undefined || encoding === "utf8" || encoding === "utf-8")) {
        this._chunks.push(chunk);
      } else {
        this._chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(String(chunk), encoding));
      }
      return true;
    }
    end(chunk, encoding, callback) {
      if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
      if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
      this.headersSent = true;
      this.finished = true;
      this.writableEnded = true;
      // The dispatch already returned without an answer, so the connection is
      // waiting on this call rather than on the handler's return.
      if (this._deferred) {
        this._deferred = false;
        const [status, reason, headers, body] = describeResponse(this);
        __sakoHttpRespond(this._serverId, this._ticket, status, reason, headers, body);
      }
      if (typeof callback === "function") callback();
      this.emit("finish");
      return this;
    }
  }

  /// The wire form of a finished response: [status, reason, headers, body].
  const describeResponse = (response) => {
    const headers = [];
    for (const [, [name, value]] of response._headers) {
      headers.push(name, Array.isArray(value) ? value.join(", ") : String(value));
    }
    const chunks = response._chunks;
    let body;
    if (chunks.length === 0) body = "";
    else if (chunks.length === 1) body = chunks[0];
    else if (chunks.every((chunk) => typeof chunk === "string")) body = chunks.join("");
    else body = Buffer.concat(chunks.map((chunk) => typeof chunk === "string" ? Buffer.from(chunk) : chunk));
    return [
      response.statusCode,
      response.statusMessage || STATUS_CODES[response.statusCode] || "Unknown",
      headers,
      body,
    ];
  };

  class Server extends EventEmitter {
    constructor(handler, tlsOptions = null) {
      super();
      if (typeof handler === "function") this.on("request", handler);
      this._tlsOptions = tlsOptions;
      this.listening = false;
      this._id = 0;
      this._port = 0;
    }
    listen(port = 0, host, callback) {
      if (typeof host === "function") { callback = host; host = undefined; }
      if (typeof callback === "function") this.once("listening", callback);
      const handler = (request, response) => this.emit("request", request, response);
      const binding = this._tlsOptions
        ? __sakoHttpsListen(
            handler,
            Number(port),
            typeof this._tlsOptions.key === "string" ? Buffer.from(this._tlsOptions.key) : Buffer.from(this._tlsOptions.key),
            typeof this._tlsOptions.cert === "string" ? Buffer.from(this._tlsOptions.cert) : Buffer.from(this._tlsOptions.cert),
          )
        : __sakoHttpListen(handler, Number(port));
      this._id = binding[0];
      this._port = binding[1];
      this.listening = true;
      queueMicrotask(() => this.emit("listening"));
      return this;
    }
    address() { return this.listening ? __sakoHttpAddress(this._id, this._port) : null; }
    close(callback) {
      if (this.listening) __sakoHttpClose(this._id);
      this.listening = false;
      if (typeof callback === "function") queueMicrotask(callback);
      queueMicrotask(() => this.emit("close"));
      return this;
    }
  }

  const http = {
    METHODS,
    STATUS_CODES,
    IncomingMessage,
    ServerResponse,
    Server,
    createServer: (handler) => new Server(handler),
    Agent: class Agent {},
    globalAgent: {},
    // Sako serves HTTP but cannot yet act as a streaming client. These exist
    // because a named ESM import that is missing is a SyntaxError before any
    // code runs -- a module that merely imports `get` would fail even if it
    // never calls it. Calling one still fails loudly, matching https below.
    // Use fetch() for client requests.
    request: unavailable("http.request"),
    get: unavailable("http.get"),
  };

  // node:tls. Present so a namespace import resolves; there is no TLS client
  // here, so every entry point that would open a connection refuses.
  const tls = {
    TLSSocket: class TLSSocket {
      constructor() { throw new Error("tls.TLSSocket is not implemented by Sako.js"); }
    },
    connect: unavailable("tls.connect"),
    createServer: unavailable("tls.createServer"),
    createSecureContext: unavailable("tls.createSecureContext"),
    checkServerIdentity: unavailable("tls.checkServerIdentity"),
    getCiphers: () => [],
    rootCertificates: [],
    DEFAULT_ECDH_CURVE: "auto",
    DEFAULT_MIN_VERSION: "TLSv1.2",
    DEFAULT_MAX_VERSION: "TLSv1.3",
  };

  const net = {
    isIP(value) {
      value = String(value);
      if (/^(?:\d{1,3}\.){3}\d{1,3}$/.test(value) && value.split(".").every((part) => Number(part) <= 255)) return 4;
      if (value.includes(":") && /^[0-9a-f:]+$/i.test(value)) return 6;
      return 0;
    },
  };
  net.isIPv4 = (value) => net.isIP(value) === 4;
  net.isIPv6 = (value) => net.isIP(value) === 6;

  const dnsLookup = (hostname, options, callback) => {
    if (typeof options === "function") { callback = options; options = {}; }
    else if (typeof options === "number") options = { family: options };
    else options = options || {};
    if (typeof callback !== "function") throw new TypeError("dns.lookup needs a callback");
    const family = Number(options.family || 0);
    try {
      const addresses = __sakoResolveHost(String(hostname), family);
      queueMicrotask(() => callback(
        null,
        options.all
          ? addresses.map((address) => ({ address, family: net.isIP(address) }))
          : addresses[0],
        options.all ? undefined : net.isIP(addresses[0]),
      ));
    } catch (error) {
      queueMicrotask(() => callback(error));
    }
  };
  const dnsResolve = (family) => (hostname, callback) => {
    if (typeof callback !== "function") throw new TypeError("DNS resolve needs a callback");
    try {
      const addresses = __sakoResolveHost(String(hostname), family);
      queueMicrotask(() => callback(null, addresses));
    } catch (error) {
      queueMicrotask(() => callback(error));
    }
  };
  const dns = {
    lookup: dnsLookup,
    resolve4: dnsResolve(4),
    resolve6: dnsResolve(6),
    getDefaultResultOrder: () => "verbatim",
    setDefaultResultOrder(order) {
      if (order !== "verbatim" && order !== "ipv4first") throw new TypeError("invalid DNS result order");
    },
  };
  dns.promises = {
    lookup(hostname, options) {
      return new Promise((resolve, reject) => dnsLookup(hostname, options, (error, address, family) => {
        if (error) reject(error);
        else resolve(options?.all ? address : { address, family });
      }));
    },
    resolve4(hostname) { return new Promise((resolve, reject) => dns.resolve4(hostname, (error, value) => error ? reject(error) : resolve(value))); },
    resolve6(hostname) { return new Promise((resolve, reject) => dns.resolve6(hostname, (error, value) => error ? reject(error) : resolve(value))); },
  };

  const isWindows = globalThis.__sakoPlatform === "win32";
  const os = {
    EOL: isWindows ? "\r\n" : "\n",
    devNull: isWindows ? "\\\\.\\nul" : "/dev/null",
    arch: () => process.arch,
    platform: () => process.platform,
    type: () => isWindows ? "Windows_NT" : "Linux",
    endianness: () => "LE",
    homedir: () => (isWindows ? process.env.USERPROFILE : process.env.HOME) || "",
    tmpdir: () => (isWindows
      ? process.env.TEMP || process.env.TMP
      : process.env.TMPDIR) || (isWindows ? "" : "/tmp"),
    hostname: () => (isWindows ? process.env.COMPUTERNAME : process.env.HOSTNAME) || "",
    release: () => (isWindows ? process.env.OS || "Windows_NT" : "Linux"),
    availableParallelism: () => Math.max(1, Number(process.env.NUMBER_OF_PROCESSORS) || 1),
    cpus: () => Array.from({ length: Math.max(1, Number(process.env.NUMBER_OF_PROCESSORS) || 1) }, () => ({ model: "unknown", speed: 0, times: { user: 0, nice: 0, sys: 0, idle: 0, irq: 0 } })),
    freemem: () => 0,
    totalmem: () => 0,
    userInfo: () => ({
      username: (isWindows ? process.env.USERNAME : process.env.USER) || "",
      uid: -1,
      gid: -1,
      shell: isWindows ? null : process.env.SHELL || "/bin/sh",
      homedir: (isWindows ? process.env.USERPROFILE : process.env.HOME) || "",
    }),
  };

  class HttpsServer extends Server {
    constructor(options, handler) { super(handler, options); }
  }
  const https = {
    Agent: class Agent {},
    Server: HttpsServer,
    globalAgent: {},
    createServer(options, handler) {
      if (!options || options.key === undefined || options.cert === undefined) {
        throw new TypeError("https.createServer needs key and cert options");
      }
      return new HttpsServer(options, handler);
    },
    request: unavailable("https.request"),
    get: unavailable("https.get"),
  };
  class ChildProcess extends EventEmitter {
    constructor() {
      super();
      this.pid = 0;
      this.connected = false;
      this.killed = false;
      this.exitCode = null;
      this.signalCode = null;
      this.stdout = new Readable();
      this.stderr = new Readable();
      this.stdin = new Writable();
    }
    kill() {
      if (this.exitCode !== null || this.killed) return false;
      this.killed = true;
      return true;
    }
    ref() { return this; }
    unref() { return this; }
  }
  const childProcess = {
    ChildProcess,
    fork: unavailable("child_process.fork"),
    spawnSync(file, args = [], options = {}) {
      if (!Array.isArray(args)) { options = args || {}; args = []; }
      const native = __sakoSpawnSync(String(file), args.map(String), options.cwd === undefined ? undefined : String(options.cwd));
      const stdout = Buffer.from(native.stdout);
      const stderr = Buffer.from(native.stderr);
      const encoding = options.encoding && options.encoding !== "buffer" ? options.encoding : undefined;
      return {
        pid: 0,
        output: [null, encoding ? stdout.toString(encoding) : stdout, encoding ? stderr.toString(encoding) : stderr],
        stdout: encoding ? stdout.toString(encoding) : stdout,
        stderr: encoding ? stderr.toString(encoding) : stderr,
        status: native.status,
        signal: null,
        error: undefined,
      };
    },
  };
  childProcess.spawn = (file, args = [], options = {}) => {
    if (!Array.isArray(args)) { options = args || {}; args = []; }
    const child = new ChildProcess();
    queueMicrotask(() => {
      if (child.killed) {
        child.exitCode = 1;
        child.emit("exit", 1, null);
        child.emit("close", 1, null);
        return;
      }
      try {
        const result = childProcess.spawnSync(file, args, options);
        child.exitCode = result.status;
        if (result.stdout?.length) child.stdout.push(Buffer.isBuffer(result.stdout) ? result.stdout : Buffer.from(result.stdout));
        if (result.stderr?.length) child.stderr.push(Buffer.isBuffer(result.stderr) ? result.stderr : Buffer.from(result.stderr));
        child.stdout.push(null);
        child.stderr.push(null);
        child.emit("exit", result.status, null);
        child.emit("close", result.status, null);
      } catch (error) {
        child.emit("error", error);
        child.emit("close", null, null);
      }
    });
    return child;
  };
  childProcess.execFile = (file, args, options, callback) => {
    if (typeof args === "function") { callback = args; args = []; options = {}; }
    else if (!Array.isArray(args)) { callback = options; options = args || {}; args = []; }
    else if (typeof options === "function") { callback = options; options = {}; }
    const child = childProcess.spawn(file, args, { ...(options || {}), encoding: "utf8" });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk.toString(); });
    child.stderr.on("data", (chunk) => { stderr += chunk.toString(); });
    if (typeof callback === "function") child.once("close", (code) => callback(code === 0 ? null : new Error(`Command failed with status ${code}: ${file}`), stdout, stderr));
    return child;
  };
  // cmd.exe's /d /s /c flags have no POSIX sh equivalent (they disable
  // AutoRun scripts and quote-strip the command line); sh -c just takes the
  // command string directly.
  const shellInvocation = (command) => globalThis.__sakoPlatform === "win32"
    ? ["cmd.exe", ["/d", "/s", "/c", String(command)]]
    : ["/bin/sh", ["-c", String(command)]];
  childProcess.exec = (command, options, callback) => {
    if (typeof options === "function") { callback = options; options = {}; }
    const [file, args] = shellInvocation(command);
    return childProcess.execFile(file, args, options || {}, callback);
  };
  childProcess.execFileSync = (file, args, options) => {
    const result = childProcess.spawnSync(file, args, options);
    if (result.status !== 0) {
      const error = new Error(`Command failed with status ${result.status}: ${file}`);
      Object.assign(error, result);
      throw error;
    }
    return result.stdout;
  };
  childProcess.execSync = (command, options) => {
    const [file, args] = shellInvocation(command);
    return childProcess.execFileSync(file, args, options);
  };

  const plainSocket = { remoteAddress: "127.0.0.1", encrypted: false };
  const secureSocket = { remoteAddress: "127.0.0.1", encrypted: true };
  Object.defineProperty(globalThis, "__sakoDispatchHttpRequest", {
    value(handler, method, target, headerBytes, headerRanges, requestBody, secure = false, serverId = 0, ticket = 0) {
      const request = new IncomingMessage();
      request.method = method;
      request.url = target;
      request.httpVersion = "1.1";
      request.httpVersionMajor = 1;
      request.httpVersionMinor = 1;
      request._headerBytes = headerBytes;
      request._headerRanges = headerRanges;
      request.socket = request.connection = secure ? secureSocket : plainSocket;
      const response = new ServerResponse(request);
      // Kept so a handler that finishes later can name the request it is
      // answering; the native side has moved on by then.
      response._serverId = serverId;
      response._ticket = ticket;
      response.socket = response.connection = request.socket;
      request.res = response;
      handler(request, response);
      // Only schedule the readable events a listener is waiting for; a handler
      // that ignores the request body costs no microtask at all.
      if (request.listenerCount("data") !== 0 || request.listenerCount("end") !== 0) {
        queueMicrotask(() => {
          if (requestBody.length !== 0 && request.listenerCount("data") !== 0) {
            request.emit("data", Buffer.from(requestBody));
          }
          request.readable = false;
          request.readableEnded = true;
          request.emit("end");
        });
      } else {
        request.readable = false;
        request.readableEnded = true;
      }
      return response;
    },
  });
  Object.defineProperty(globalThis, "__sakoFinalizeHttpResponse", {
    value(response) {
      // A handler that has not called end() yet is still working -- awaiting a
      // file read, a transform, a proxied request. Null tells the native side
      // to leave the connection open and wait for __sakoHttpRespond instead of
      // demanding an answer the handler does not have.
      if (!response.writableEnded) {
        response._deferred = true;
        return null;
      }
      // Positional: the native side reads [status, reason, headers, body].
      return describeResponse(response);
    },
  });

  const querystring = {
    parse(value, separator = "&", equals = "=") {
      const result = Object.create(null);
      if (!value) return result;
      for (const field of String(value).split(separator)) {
        const index = field.indexOf(equals);
        const name = formDecode(index < 0 ? field : field.slice(0, index));
        const item = formDecode(index < 0 ? "" : field.slice(index + equals.length));
        if (result[name] === undefined) result[name] = item;
        else if (Array.isArray(result[name])) result[name].push(item);
        else result[name] = [result[name], item];
      }
      return result;
    },
    stringify(value, separator = "&", equals = "=") {
      const fields = [];
      for (const name of Object.keys(value || {})) {
        const values = Array.isArray(value[name]) ? value[name] : [value[name]];
        for (const item of values) fields.push(`${formEncode(name)}${equals}${formEncode(item ?? "")}`);
      }
      return fields.join(separator);
    },
  };

  class StringDecoder {
    constructor(encoding = "utf8") {
      if (encoding !== "utf8" && encoding !== "utf-8") throw new TypeError(`Unsupported encoding: ${encoding}`);
      this._pending = Buffer.alloc(0);
    }
    write(value) {
      const bytes = Buffer.concat([this._pending, Buffer.from(value)]);
      let sequenceStart = bytes.length - 1;
      while (sequenceStart >= 0 && (bytes[sequenceStart] & 0xc0) === 0x80) sequenceStart -= 1;
      let expected = 1;
      if (sequenceStart >= 0) {
        const lead = bytes[sequenceStart];
        if ((lead & 0xe0) === 0xc0) expected = 2;
        else if ((lead & 0xf0) === 0xe0) expected = 3;
        else if ((lead & 0xf8) === 0xf0) expected = 4;
      }
      const available = sequenceStart < 0 ? bytes.length : bytes.length - sequenceStart;
      const split = sequenceStart >= 0 && expected > available ? sequenceStart : bytes.length;
      this._pending = Buffer.from(bytes.subarray(split));
      return __sakoDecodeUtf8(bytes.subarray(0, split));
    }
    end(value) {
      let output = value === undefined ? "" : this.write(value);
      if (this._pending.length !== 0) {
        output += __sakoDecodeUtf8(this._pending);
        this._pending = Buffer.alloc(0);
      }
      return output;
    }
  }

  const pathToFileURL = (value) => new URL(`file:///${path.resolve(value).replace(/\\/g, "/").replace(/^([A-Za-z]):/, "$1:")}`);
  const fileURLToPath = (value) => {
    const url = value instanceof URL ? value : new URL(value);
    if (url.protocol !== "file:") throw new TypeError("URL must use the file: protocol");
    return decodeURIComponent(url.pathname).replace(/^\/([A-Za-z]:)/, "$1").replace(/\//g, "\\");
  };
  const url = { URL, URLSearchParams, pathToFileURL, fileURLToPath };

  // process.stdin. Absent entirely before, which meant anything that reads
  // input -- a prompt, a piped payload -- had nothing to attach to. Backed by
  // synchronous reads on descriptor 0, so it is a line source rather than a
  // full duplex stream.
  const STDIN_FD = 0;
  const stdinState = { buffer: Buffer.alloc(0), ended: false };

  const readStdinChunk = () => {
    if (stdinState.ended) return null;
    const chunk = Buffer.alloc(8192);
    let read = 0;
    try {
      read = fs.readSync(STDIN_FD, chunk, 0, chunk.length, null);
    } catch {
      stdinState.ended = true;
      return null;
    }
    if (read === 0) {
      stdinState.ended = true;
      return null;
    }
    return chunk.subarray(0, read);
  };

  /// Reads one line, or null once input is exhausted. The trailing newline is
  /// stripped, and a lone \r before it, so Windows input matches POSIX.
  const readStdinLine = () => {
    for (;;) {
      const newline = stdinState.buffer.indexOf(0x0a);
      if (newline !== -1) {
        const line = stdinState.buffer.subarray(0, newline).toString("utf8");
        stdinState.buffer = stdinState.buffer.subarray(newline + 1);
        return line.endsWith("\r") ? line.slice(0, -1) : line;
      }
      const chunk = readStdinChunk();
      if (chunk === null) {
        if (stdinState.buffer.length === 0) return null;
        const rest = stdinState.buffer.toString("utf8");
        stdinState.buffer = Buffer.alloc(0);
        return rest.endsWith("\r") ? rest.slice(0, -1) : rest;
      }
      stdinState.buffer = Buffer.concat([stdinState.buffer, chunk]);
    }
  };

  class Stdin extends Readable {
    constructor() {
      super();
      this.fd = STDIN_FD;
      this.readable = true;
    }
    // Asked of the descriptor on every read rather than cached at construction.
    // The bootstrap is evaluated once and its result is reused for every run
    // (and, once snapshotted, baked into the binary at build time), so a value
    // captured here would report the build machine's terminal state forever.
    get isTTY() { return Boolean(tty.isatty(this.fd)); }
    // Sako has no raw console mode, so character-at-a-time prompts cannot be
    // supported. Reporting the failure honestly is better than pretending:
    // a caller that checks can fall back to line input.
    setRawMode() { return this; }
    setEncoding() { return this; }
    resume() { return this; }
    pause() { return this; }
    read() {
      const line = readStdinLine();
      return line === null ? null : `${line}\n`;
    }
    async *[Symbol.asyncIterator]() {
      for (;;) {
        const line = readStdinLine();
        if (line === null) return;
        yield line;
      }
    }
  }
  const stdin = new Stdin();

  // process.stdout / process.stderr.
  //
  // The native side installs plain objects carrying only `write` and `fd`.
  // Terminal UIs treat these as streams -- attaching "resize" listeners,
  // calling end(), reading columns -- so a plain object fails at the first
  // `.on(...)`. These are real Writables that emit, and delegate the actual
  // write to the same descriptor.
  class StandardOutput extends Writable {
    constructor(fd) {
      super();
      this.fd = fd;
    }
    // See Stdin.isTTY: resolved per access, never captured at construction.
    get isTTY() { return Boolean(tty.isatty(this.fd)); }
    // Sized on each read: a terminal can be resized at any point, and Sako has
    // no SIGWINCH to invalidate a cached value.
    get columns() { return __sakoTerminalSize(this.fd)?.columns; }
    get rows() { return __sakoTerminalSize(this.fd)?.rows; }
    getWindowSize() {
      const size = __sakoTerminalSize(this.fd);
      return size ? [size.columns, size.rows] : undefined;
    }
    hasColors() { return this.isTTY; }
    write(chunk, encoding, callback) {
      if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
      const bytes = typeof chunk === "string"
        ? Buffer.from(chunk, typeof encoding === "string" ? encoding : "utf8")
        : Buffer.from(chunk);
      __sakoWriteStandard(this.fd, bytes);
      if (typeof callback === "function") callback();
      return true;
    }
    // Closing a standard stream would take the descriptor away from everything
    // else writing to it, so end() only marks and notifies.
    end(chunk, encoding, callback) {
      if (chunk !== undefined && typeof chunk !== "function") this.write(chunk, encoding);
      this.writableEnded = true;
      this.emit("finish");
      if (typeof callback === "function") callback();
      return this;
    }
    cork() {}
    uncork() {}
    setDefaultEncoding() { return this; }
  }
  const stdout = new StandardOutput(1);
  const stderr = new StandardOutput(2);

  // Process members contributed from JavaScript. Parked on the global rather
  // than assigned to `process`: the bootstrap runs once at context
  // initialization, while the native side rebuilds `process` for every
  // execution and would discard anything set here. InstallProcess copies these
  // across on each execution.
  const processEvents = new EventEmitter();
  // Supplied by the native side on every execution. Calling Date.now() here
  // would record when the bootstrap was evaluated -- which, once the context
  // is snapshotted, is when the binary was built.
  const processEpoch = () => globalThis.__sakoEpochMs ?? Date.now();
  const processExtras = {
    stdin,
    stdout,
    stderr,
    // Terminates the process; the native side flushes and exits.
    exit(code) { __sakoExit(code === undefined ? 0 : Number(code) | 0); },
    // Node returns [seconds, nanoseconds]; hrtime.bigint returns nanoseconds.
    // Date.now has millisecond resolution, so the low digits are zero rather
    // than fabricated.
    hrtime: Object.assign(
      (previous) => {
        const now = Date.now() * 1e6;
        const nanoseconds = previous ? now - (previous[0] * 1e9 + previous[1]) : now;
        return [Math.floor(nanoseconds / 1e9), Math.floor(nanoseconds % 1e9)];
      },
      { bigint: () => BigInt(Date.now()) * 1000000n },
    ),
    memoryUsage: () => ({ rss: 0, heapTotal: 0, heapUsed: 0, external: 0, arrayBuffers: 0 }),
    uptime: () => (Date.now() - processEpoch()) / 1000,
    chdir() { throw new Error("process.chdir is not supported"); },
    umask: () => 0,
    emitWarning(warning) {
      const text = warning instanceof Error ? warning.stack ?? warning.message : String(warning);
      process.stderr.write(`Warning: ${text}\n`);
    },
    // Lifecycle events are accepted so listeners can register, but nothing
    // emits them: Sako has no signal handling or exit hook to drive them.
    on: (...args) => { processEvents.on(...args); return process; },
    once: (...args) => { processEvents.once(...args); return process; },
    off: (...args) => { processEvents.off(...args); return process; },
    removeListener: (...args) => { processEvents.off(...args); return process; },
    emit: (...args) => processEvents.emit(...args),
  };
  Object.defineProperty(globalThis, "__sakoProcessExtras", { value: processExtras });

  // node:readline. Line-oriented only -- see Stdin.setRawMode above.
  const writeTo = (output, text) => {
    if (output && typeof output.write === "function") output.write(text);
  };
  class Interface extends EventEmitter {
    constructor(options = {}) {
      super();
      const settings = typeof options.write === "function" || options.read ? { input: options } : options;
      this.input = settings.input ?? stdin;
      this.output = settings.output ?? process.stdout;
      this.terminal = settings.terminal ?? Boolean(this.output && this.output.isTTY);
      this._prompt = settings.prompt ?? "> ";
      this.closed = false;
    }
    setPrompt(prompt) { this._prompt = prompt; }
    getPrompt() { return this._prompt; }
    prompt() { writeTo(this.output, this._prompt); }
    question(query, ...rest) {
      const callback = rest[rest.length - 1];
      writeTo(this.output, query);
      const answer = readStdinLine() ?? "";
      if (typeof callback === "function") {
        callback(answer);
        return undefined;
      }
      return Promise.resolve(answer);
    }
    write(text) { writeTo(this.output, text); }
    pause() { return this; }
    resume() { return this; }
    close() {
      if (this.closed) return;
      this.closed = true;
      this.emit("close");
    }
    async *[Symbol.asyncIterator]() {
      for (;;) {
        const line = readStdinLine();
        if (line === null) { this.close(); return; }
        yield line;
      }
    }
  }
  const readline = {
    Interface,
    createInterface: (options, output) =>
      new Interface(output === undefined ? options : { input: options, output }),
    // Cursor control is pure escape output, so it works whether or not the
    // terminal is interactive.
    clearLine(output, direction = 0, callback) {
      const code = direction < 0 ? "\x1b[1K" : direction > 0 ? "\x1b[0K" : "\x1b[2K";
      writeTo(output, code);
      if (typeof callback === "function") callback();
      return true;
    },
    clearScreenDown(output, callback) {
      writeTo(output, "\x1b[0J");
      if (typeof callback === "function") callback();
      return true;
    },
    cursorTo(output, x = 0, y, callback) {
      if (typeof y === "function") { callback = y; y = undefined; }
      writeTo(output, y === undefined ? `\x1b[${x + 1}G` : `\x1b[${y + 1};${x + 1}H`);
      if (typeof callback === "function") callback();
      return true;
    },
    moveCursor(output, dx = 0, dy = 0, callback) {
      let text = "";
      if (dx < 0) text += `\x1b[${-dx}D`; else if (dx > 0) text += `\x1b[${dx}C`;
      if (dy < 0) text += `\x1b[${-dy}A`; else if (dy > 0) text += `\x1b[${dy}B`;
      writeTo(output, text);
      if (typeof callback === "function") callback();
      return true;
    },
    // Keypress events need raw mode, which Sako lacks; the stream simply never
    // emits them rather than the import failing.
    emitKeypressEvents: () => undefined,
  };
  readline.promises = {
    Interface,
    createInterface: readline.createInterface,
  };

  // node:perf_hooks. Build tools time themselves with this as a matter of
  // course, so a missing module stops them before they do any work.
  const performance = globalThis.performance ?? {
    // Millisecond resolution: Date.now is the only clock available here, so
    // the sub-millisecond digits Node reports are simply not measurable.
    // See processEpoch: the origin is this run's start, not the build's.
    now: () => Date.now() - processEpoch(),
    get timeOrigin() { return processEpoch(); },
    mark: () => undefined,
    measure: () => undefined,
    clearMarks: () => undefined,
    clearMeasures: () => undefined,
    getEntries: () => [],
    getEntriesByName: () => [],
    getEntriesByType: () => [],
    toJSON: () => ({ timeOrigin: processEpoch() }),
  };
  if (globalThis.performance === undefined) {
    Object.defineProperty(globalThis, "performance", { value: performance, configurable: true, writable: true });
  }
  // Observers accept registration and never fire: Sako emits no performance
  // entries, so a callback would have nothing to receive.
  class PerformanceObserver {
    constructor(callback) { this._callback = callback; }
    observe() {}
    disconnect() {}
    takeRecords() { return []; }
  }
  PerformanceObserver.supportedEntryTypes = [];
  const perfHooks = {
    performance,
    PerformanceObserver,
    PerformanceEntry: class PerformanceEntry {},
    monitorEventLoopDelay: () => ({
      enable() {}, disable() {}, reset() {},
      min: 0, max: 0, mean: 0, stddev: 0, percentile: () => 0,
    }),
    createHistogram: () => ({ record() {}, reset() {}, min: 0, max: 0, mean: 0, percentile: () => 0 }),
  };

  // Minimal surfaces for modules tooling imports but rarely drives. Each is
  // shaped so the common read-only checks answer correctly rather than throw.
  const workerThreads = {
    isMainThread: true,
    threadId: 0,
    parentPort: null,
    workerData: null,
    // Sako has no worker threads; constructing one has to fail loudly rather
    // than return something that silently never runs.
    Worker: class Worker {
      constructor() { throw new Error("worker_threads is not supported"); }
    },
    // Named exports consumers destructure at import time. A missing name is a
    // SyntaxError before any code runs, so these have to exist even though
    // nothing can be posted across a thread that cannot be created.
    MessagePort: class MessagePort extends EventEmitter {
      postMessage() {}
      close() { this.emit("close"); }
      ref() { return this; }
      unref() { return this; }
      start() {}
    },
    MessageChannel: class MessageChannel {
      constructor() {
        this.port1 = new workerThreads.MessagePort();
        this.port2 = new workerThreads.MessagePort();
      }
    },
    BroadcastChannel: class BroadcastChannel extends EventEmitter {
      constructor(name) { super(); this.name = name; }
      postMessage() {}
      close() {}
      ref() { return this; }
      unref() { return this; }
    },
    receiveMessageOnPort: () => undefined,
    markAsUntransferable: () => undefined,
    moveMessagePortToContext: () => { throw new Error("worker_threads is not supported"); },
    setEnvironmentData: () => undefined,
    getEnvironmentData: () => undefined,
    SHARE_ENV: Symbol("SHARE_ENV"),
  };
  const asyncHooks = {
    createHook: () => ({ enable() { return this; }, disable() { return this; } }),
    executionAsyncId: () => 0,
    triggerAsyncId: () => 0,
    AsyncLocalStorage: class AsyncLocalStorage {
      constructor() { this._store = undefined; }
      run(store, callback, ...args) {
        const previous = this._store;
        this._store = store;
        try { return callback(...args); } finally { this._store = previous; }
      }
      getStore() { return this._store; }
      enterWith(store) { this._store = store; }
      exit(callback, ...args) {
        const previous = this._store;
        this._store = undefined;
        try { return callback(...args); } finally { this._store = previous; }
      }
    },
    AsyncResource: class AsyncResource {
      constructor(type) { this.type = type; }
      runInAsyncScope(fn, thisArg, ...args) { return fn.apply(thisArg, args); }
      emitDestroy() { return this; }
    },
  };

  // Small shims for modules tooling imports for capability checks or optional
  // features. Each answers the common query truthfully -- "no, that is not
  // available here" -- instead of failing the import outright.
  const v8Module = {
    // Vite calls this to size its cache; the numbers are inert but shaped
    // correctly so arithmetic on them works.
    getHeapStatistics: () => ({
      total_heap_size: 0, total_heap_size_executable: 0, total_physical_size: 0,
      total_available_size: 0, used_heap_size: 0, heap_size_limit: 0,
      malloced_memory: 0, peak_malloced_memory: 0, does_zap_garbage: 0,
      number_of_native_contexts: 0, number_of_detached_contexts: 0,
    }),
    getHeapSpaceStatistics: () => [],
    setFlagsFromString: () => undefined,
    serialize: () => { throw new Error("v8.serialize is not supported"); },
    deserialize: () => { throw new Error("v8.deserialize is not supported"); },
    takeCoverage: () => undefined,
    stopCoverage: () => undefined,
  };
  const inspectorModule = {
    // Reporting the session as unavailable lets callers skip profiling paths.
    Session: class Session {
      connect() { throw new Error("inspector is not supported"); }
      disconnect() {}
      post() { throw new Error("inspector is not supported"); }
    },
    open: () => undefined,
    close: () => undefined,
    url: () => undefined,
  };
  const diagnosticsChannel = {
    channel: (name) => ({
      name,
      hasSubscribers: false,
      publish: () => undefined,
      subscribe: () => undefined,
      unsubscribe: () => false,
    }),
    hasSubscribers: () => false,
    subscribe: () => undefined,
    unsubscribe: () => false,
    tracingChannel: (name) => ({
      name,
      subscribe: () => undefined,
      unsubscribe: () => false,
      traceSync: (fn, context, thisArg, ...args) => fn.apply(thisArg, args),
      tracePromise: (fn, context, thisArg, ...args) => fn.apply(thisArg, args),
      traceCallback: (fn, position, context, thisArg, ...args) => fn.apply(thisArg, args),
    }),
  };
  const timersPromises = {
    setTimeout: (delay, value) => new Promise((resolve) => setTimeout(() => resolve(value), delay)),
    setImmediate: (value) => new Promise((resolve) => queueMicrotask(() => resolve(value))),
    setInterval: async function* (delay, value) {
      for (;;) {
        await new Promise((resolve) => setTimeout(resolve, delay));
        yield value;
      }
    },
  };

  // node:module. Tools written for Node reach for createRequire constantly --
  // to read their own package.json, or to resolve a dependency from an ES
  // module -- so its absence stops most real CLIs at their first import.
  const builtinModules = [
    "assert", "buffer", "child_process", "console", "crypto", "dns",
    "dns/promises", "events", "fs", "fs/promises", "http", "https", "module",
    "net", "os", "path", "process", "querystring",
    "async_hooks", "diagnostics_channel", "http2", "inspector", "perf_hooks", "readline",
    "readline/promises", "stream", "stream/promises", "string_decoder",
    "timers", "timers/promises", "tls", "v8", "worker_threads",
    "tty", "url", "util", "zlib",
  ];
  const isBuiltin = (name) =>
    builtinModules.includes(typeof name === "string" && name.startsWith("node:") ? name.slice(5) : name);
  const createRequire = (referrer) => {
    if (referrer instanceof URL || (typeof referrer === "string" && referrer.startsWith("file:"))) {
      referrer = fileURLToPath(referrer);
    }
    if (typeof referrer !== "string" || referrer.length === 0) {
      throw new TypeError("createRequire needs a path or file URL");
    }
    return __sakoCreateRequire(referrer);
  };
  const moduleBuiltin = {
    createRequire,
    builtinModules,
    isBuiltin,
    // Present so `Module.createRequire(...)` works: Node exposes the same
    // functions both on the namespace and on the Module constructor.
    Module: Object.assign(function Module() {}, { createRequire, builtinModules, isBuiltin }),
    // A no-op keeps callers that register hooks working rather than crashing;
    // Sako has no loader-hook pipeline for them to attach to.
    register: () => undefined,
    syncBuiltinESMExports: () => undefined,
  };

  Object.assign(globalThis, { Buffer, TextEncoder, TextDecoder, URL, URLSearchParams, Event, CustomEvent, EventTarget, AbortController, AbortSignal, Headers, Request, Response, fetch });
  Object.defineProperty(globalThis, "__sakoBuiltins", {
    value: {
      "node:module": moduleBuiltin,
      "node:perf_hooks": perfHooks,
      "node:v8": v8Module,
      "node:inspector": inspectorModule,
      "node:inspector/promises": inspectorModule,
      "node:diagnostics_channel": diagnosticsChannel,
      "node:timers/promises": timersPromises,
      "node:stream/promises": { pipeline: async (...parts) => parts, finished: async () => undefined },
      "node:worker_threads": workerThreads,
      "node:async_hooks": asyncHooks,
      "node:readline": readline,
      "node:readline/promises": readline.promises,
      "node:buffer": { Buffer, SlowBuffer: Buffer },
      "node:crypto": crypto,
      "node:child_process": childProcess,
      "node:dns": dns,
      "node:dns/promises": dns.promises,
      "node:events": Object.assign(EventEmitter, {
        EventEmitter,
        defaultMaxListeners: 10,
        // Static helpers Node exposes on the module itself.
        once: (emitter, name) => new Promise((resolve, reject) => {
          emitter.once(name, (...args) => resolve(args));
          if (name !== "error" && typeof emitter.once === "function") {
            emitter.once("error", reject);
          }
        }),
        on: (emitter, name) => {
          const queue = [];
          let notify = null;
          emitter.on(name, (...args) => {
            if (notify) { const resume = notify; notify = null; resume(args); }
            else queue.push(args);
          });
          return {
            [Symbol.asyncIterator]() { return this; },
            next: async () => ({
              value: queue.length ? queue.shift() : await new Promise((resolve) => { notify = resolve; }),
              done: false,
            }),
          };
        },
      }),
      "node:fs": fs,
      "node:fs/promises": Object.assign({}, fsPromises, { constants: fs.constants }),
      "node:http": http,
      "node:https": https,
      "node:net": net,
      "node:tls": tls,
      "node:os": os,
      "node:path": path,
      "node:querystring": querystring,
      "node:string_decoder": { StringDecoder },
      "node:stream": Stream,
      "node:tty": tty,
      "node:url": url,
      "node:util": util,
      "node:zlib": zlib,
      "node:http2": http2,
    },
  });
})();
