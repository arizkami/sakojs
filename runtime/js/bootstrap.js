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

    static allocUnsafe(size) { return Buffer.alloc(size); }
    static allocUnsafeSlow(size) { return Buffer.alloc(size); }

    static isBuffer(value) {
      return value instanceof Buffer;
    }

    static byteLength(value, encoding = "utf8") {
      return Buffer.isBuffer(value) || ArrayBuffer.isView(value)
        ? value.byteLength
        : Buffer.from(String(value), encoding).length;
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
      const bytes = this.subarray(start, end);
      if (encoding === "utf8" || encoding === "utf-8") return __sakoDecodeUtf8(bytes);
      if (encoding === "hex") return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
      if (encoding === "base64") {
        const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let output = "";
        for (let index = 0; index < bytes.length; index += 3) {
          const first = bytes[index];
          const second = bytes[index + 1];
          const third = bytes[index + 2];
          output += alphabet[first >> 2];
          output += alphabet[((first & 3) << 4) | ((second || 0) >> 4)];
          output += index + 1 < bytes.length ? alphabet[((second & 15) << 2) | ((third || 0) >> 6)] : "=";
          output += index + 2 < bytes.length ? alphabet[third & 63] : "=";
        }
        return output;
      }
      throw new TypeError(`Unsupported encoding: ${encoding}`);
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
    abort(reason = undefined) {
      if (this.signal.aborted) return;
      if (reason === undefined) {
        reason = new Error("This operation was aborted");
        reason.name = "AbortError";
      }
      this.signal.aborted = true;
      this.signal.reason = reason;
      const listeners = this.signal.listeners.slice();
      this.signal.listeners = this.signal.listeners.filter((item) => !item.once);
      for (const { listener } of listeners) listener.call(this.signal, { type: "abort", target: this.signal });
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
  }

  console.error = console.error || console.log;
  console.warn = console.warn || console.log;
  console.info = console.info || console.log;

  const separators = /[\\/]+/g;
  const path = {
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

  class Stats {
    constructor(value) { Object.assign(this, value); }
    isFile() { return this.file; }
    isDirectory() { return this.directory; }
    isSymbolicLink() { return this.symbolicLink; }
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

  const crypto = {
    createHash(algorithm) {
      if (String(algorithm).toLowerCase().replace("-", "") !== "sha1") {
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
          const digest = Buffer.from(__sakoSha1(Buffer.concat(chunks, length)));
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
  fs.ReadStream = class ReadStream extends Readable {};
  fs.WriteStream = class WriteStream extends Writable {};
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
      if (typeof callback === "function") callback();
      this.emit("finish");
      return this;
    }
  }

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

  const os = {
    EOL: "\r\n",
    devNull: "\\\\.\\nul",
    arch: () => process.arch,
    platform: () => process.platform,
    type: () => "Windows_NT",
    endianness: () => "LE",
    homedir: () => process.env.USERPROFILE || "",
    tmpdir: () => process.env.TEMP || process.env.TMP || "",
    hostname: () => process.env.COMPUTERNAME || "",
    release: () => process.env.OS || "Windows_NT",
    availableParallelism: () => Math.max(1, Number(process.env.NUMBER_OF_PROCESSORS) || 1),
    cpus: () => Array.from({ length: Math.max(1, Number(process.env.NUMBER_OF_PROCESSORS) || 1) }, () => ({ model: "unknown", speed: 0, times: { user: 0, nice: 0, sys: 0, idle: 0, irq: 0 } })),
    freemem: () => 0,
    totalmem: () => 0,
    userInfo: () => ({ username: process.env.USERNAME || "", uid: -1, gid: -1, shell: null, homedir: process.env.USERPROFILE || "" }),
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
  childProcess.exec = (command, options, callback) => {
    if (typeof options === "function") { callback = options; options = {}; }
    return childProcess.execFile("cmd.exe", ["/d", "/s", "/c", String(command)], options || {}, callback);
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
  childProcess.execSync = (command, options) => childProcess.execFileSync(
    "cmd.exe",
    ["/d", "/s", "/c", String(command)],
    options,
  );

  const plainSocket = { remoteAddress: "127.0.0.1", encrypted: false };
  const secureSocket = { remoteAddress: "127.0.0.1", encrypted: true };
  Object.defineProperty(globalThis, "__sakoDispatchHttpRequest", {
    value(handler, method, target, headerBytes, headerRanges, requestBody, secure = false) {
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
      if (!response.writableEnded) throw new Error("asynchronous HTTP responses are not implemented");
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
      // Positional: the native side reads [status, reason, headers, body].
      return [
        response.statusCode,
        response.statusMessage || STATUS_CODES[response.statusCode] || "Unknown",
        headers,
        body,
      ];
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

  Object.assign(globalThis, { Buffer, TextEncoder, TextDecoder, URL, URLSearchParams, AbortController, AbortSignal, Headers, Request, Response, fetch });
  Object.defineProperty(globalThis, "__sakoBuiltins", {
    value: {
      "node:buffer": { Buffer, SlowBuffer: Buffer },
      "node:crypto": crypto,
      "node:child_process": childProcess,
      "node:dns": dns,
      "node:dns/promises": dns.promises,
      "node:events": Object.assign(EventEmitter, { EventEmitter, defaultMaxListeners: 10 }),
      "node:fs": fs,
      "node:fs/promises": fsPromises,
      "node:http": http,
      "node:https": https,
      "node:net": net,
      "node:os": os,
      "node:path": path,
      "node:querystring": querystring,
      "node:string_decoder": { StringDecoder },
      "node:stream": Stream,
      "node:tty": tty,
      "node:url": url,
      "node:util": util,
      "node:zlib": zlib,
    },
  });
})();
