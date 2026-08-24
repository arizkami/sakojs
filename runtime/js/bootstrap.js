// SPDX-License-Identifier: BSD-3-Clause

(() => {
  "use strict";

  class Buffer extends Uint8Array {
    static from(value, encoding = "utf8") {
      let bytes;
      if (typeof value === "string") {
        if (encoding !== "utf8" && encoding !== "utf-8") {
          throw new TypeError(`Unsupported encoding: ${encoding}`);
        }
        bytes = __sakoEncodeUtf8(value);
      } else if (value instanceof ArrayBuffer) {
        bytes = new Uint8Array(value);
      } else if (ArrayBuffer.isView(value) || Array.isArray(value)) {
        bytes = new Uint8Array(value);
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

    static isBuffer(value) {
      return value instanceof Buffer;
    }

    toString(encoding = "utf8", start = 0, end = this.length) {
      if (encoding !== "utf8" && encoding !== "utf-8") {
        throw new TypeError(`Unsupported encoding: ${encoding}`);
      }
      return __sakoDecodeUtf8(this.subarray(start, end));
    }
  }

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

  class URL {
    constructor(input, base) {
      input = String(input);
      if (!/^[A-Za-z][A-Za-z\d+.-]*:/.test(input)) {
        if (base === undefined) throw new TypeError("Invalid URL");
        const baseUrl = base instanceof URL ? base : new URL(base);
        if (input.startsWith("//")) input = `${baseUrl.protocol}${input}`;
        else if (input.startsWith("/")) input = `${baseUrl.origin}${input}`;
        else input = `${baseUrl.origin}${baseUrl.pathname.slice(0, baseUrl.pathname.lastIndexOf("/") + 1)}${input}`;
      }
      const match = /^([A-Za-z][A-Za-z\d+.-]*:)(?:\/\/([^/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(input);
      if (!match) throw new TypeError("Invalid URL");
      this.protocol = match[1].toLowerCase();
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
      this.pathname = match[3] || (authority ? "/" : "");
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
      return `${this.protocol}${this.hostname ? `//${credentials}${this.host}` : ""}${this.pathname}${this.search}${this.hash}`;
    }
    set href(value) { Object.assign(this, new URL(value)); }
    toString() { return this.href; }
    toJSON() { return this.href; }
  }

  class AbortSignal {
    constructor() {
      this.aborted = false;
      this.reason = undefined;
      this.listeners = [];
    }
    addEventListener(type, listener, options = {}) {
      if (type === "abort" && typeof listener === "function") {
        this.listeners.push({ listener, once: Boolean(options.once) });
      }
    }
    removeEventListener(type, listener) {
      if (type === "abort") this.listeners = this.listeners.filter((item) => item.listener !== listener);
    }
    throwIfAborted() { if (this.aborted) throw this.reason; }
  }

  class AbortController {
    constructor() { this.signal = new AbortSignal(); }
    abort(reason = new Error("This operation was aborted")) {
      if (this.signal.aborted) return;
      this.signal.aborted = true;
      this.signal.reason = reason;
      const listeners = this.signal.listeners.slice();
      this.signal.listeners = this.signal.listeners.filter((item) => !item.once);
      for (const { listener } of listeners) listener.call(this.signal, { type: "abort", target: this.signal });
    }
  }

  class EventEmitter {
    constructor() { this._events = new Map(); }
    on(name, listener) {
      if (typeof listener !== "function") throw new TypeError("listener must be a function");
      const listeners = this._events.get(name) || [];
      listeners.push(listener);
      this._events.set(name, listeners);
      return this;
    }
    addListener(name, listener) { return this.on(name, listener); }
    once(name, listener) {
      const wrapper = (...args) => { this.off(name, wrapper); listener.apply(this, args); };
      wrapper.listener = listener;
      return this.on(name, wrapper);
    }
    off(name, listener) {
      const listeners = this._events.get(name);
      if (listeners) this._events.set(name, listeners.filter((item) => item !== listener && item.listener !== listener));
      return this;
    }
    removeListener(name, listener) { return this.off(name, listener); }
    removeAllListeners(name) {
      if (arguments.length === 0) this._events.clear(); else this._events.delete(name);
      return this;
    }
    emit(name, ...args) {
      const listeners = this._events.get(name);
      if (!listeners || listeners.length === 0) {
        if (name === "error") throw args[0] instanceof Error ? args[0] : new Error(String(args[0]));
        return false;
      }
      for (const listener of listeners.slice()) listener.apply(this, args);
      return true;
    }
    listeners(name) { return (this._events.get(name) || []).map((item) => item.listener || item); }
    listenerCount(name) { return (this._events.get(name) || []).length; }
  }

  const separators = /[\\/]+/g;
  const path = {
    sep: "\\",
    delimiter: ";",
    normalize(value) {
      value = String(value).replace(separators, "\\");
      const drive = /^[A-Za-z]:/.exec(value)?.[0] || "";
      const rooted = value.startsWith("\\") || Boolean(drive && value[2] === "\\");
      const body = drive ? value.slice(2) : value;
      const parts = [];
      for (const part of body.split("\\")) {
        if (!part || part === ".") continue;
        if (part === ".." && parts.length && parts[parts.length - 1] !== "..") parts.pop();
        else if (part !== ".." || !rooted) parts.push(part);
      }
      const prefix = drive + (rooted ? "\\" : "");
      return prefix + parts.join("\\") || ".";
    },
    join(...parts) { return path.normalize(parts.filter(Boolean).join("\\")); },
    resolve(...parts) {
      let value = "";
      for (let index = parts.length - 1; index >= -1; --index) {
        value = `${index < 0 ? __sakoCwd : parts[index]}\\${value}`;
        if (path.isAbsolute(value)) break;
      }
      return path.normalize(value);
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
      const name = path.basename(value);
      const index = name.lastIndexOf(".");
      return index <= 0 ? "" : name.slice(index);
    },
  };
  path.win32 = path;

  const fs = {
    readFileSync(...args) {
      const value = __sakoReadFileSync(...args);
      return typeof value === "string" ? value : Buffer.from(value);
    },
    writeFileSync: __sakoWriteFileSync,
    existsSync: __sakoExistsSync,
  };
  const fsPromises = {
    async readFile(...args) { return fs.readFileSync(...args); },
    async writeFile(...args) { fs.writeFileSync(...args); },
  };
  fs.promises = fsPromises;

  const util = {
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
  };

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
    }
    write(value) { return __sakoDecodeUtf8(value); }
    end(value) { return value === undefined ? "" : this.write(value); }
  }

  const pathToFileURL = (value) => new URL(`file:///${path.resolve(value).replace(/\\/g, "/").replace(/^([A-Za-z]):/, "$1:")}`);
  const fileURLToPath = (value) => {
    const url = value instanceof URL ? value : new URL(value);
    if (url.protocol !== "file:") throw new TypeError("URL must use the file: protocol");
    return decodeURIComponent(url.pathname).replace(/^\/([A-Za-z]:)/, "$1").replace(/\//g, "\\");
  };
  const url = { URL, URLSearchParams, pathToFileURL, fileURLToPath };

  Object.assign(globalThis, { Buffer, TextEncoder, TextDecoder, URL, URLSearchParams, AbortController, AbortSignal });
  Object.defineProperty(globalThis, "__sakoBuiltins", {
    value: {
      "node:buffer": { Buffer, SlowBuffer: Buffer },
      "node:events": Object.assign(EventEmitter, { EventEmitter, defaultMaxListeners: 10 }),
      "node:fs": fs,
      "node:fs/promises": fsPromises,
      "node:path": path,
      "node:querystring": querystring,
      "node:string_decoder": { StringDecoder },
      "node:url": url,
      "node:util": util,
    },
  });
})();
