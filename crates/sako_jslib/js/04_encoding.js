// TextEncoder/TextDecoder, atob/btoa, DOMException.
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

// -------------------------------------------------------------- encoding
class TextEncoder {
  get encoding() { return "utf-8"; }
  encode(input = "") { return ops.encode(String(input)); }
  encodeInto(input, dest) {
    const bytes = ops.encode(String(input));
    if (bytes.length <= dest.length) {
      dest.set(bytes);
      return { read: input.length, written: bytes.length };
    }
    // Slow path: only copy whole code points that fit.
    let read = 0, written = 0;
    for (const ch of String(input)) {
      const b = ops.encode(ch);
      if (written + b.length > dest.length) break;
      dest.set(b, written);
      written += b.length;
      read += ch.length;
    }
    return { read, written };
  }
}

class TextDecoder {
  #fatal; #ignoreBOM;
  constructor(label = "utf-8", options = {}) {
    label = String(label).toLowerCase().trim();
    if (label !== "utf-8" && label !== "utf8" && label !== "unicode-1-1-utf-8") {
      throw new RangeError(`The "${label}" encoding is not supported`);
    }
    this.#fatal = !!options.fatal;
    this.#ignoreBOM = !!options.ignoreBOM;
  }
  get encoding() { return "utf-8"; }
  get fatal() { return this.#fatal; }
  get ignoreBOM() { return this.#ignoreBOM; }
  decode(input, _opts) {
    if (input === undefined) return "";
    let u8;
    if (input instanceof Uint8Array) u8 = input;
    else if (input instanceof ArrayBuffer || input instanceof SharedArrayBuffer) u8 = new Uint8Array(input);
    else if (ArrayBuffer.isView(input)) u8 = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
    else throw new TypeError("The \"input\" argument must be an instance of ArrayBuffer or ArrayBufferView");
    return ops.decode(u8, this.#fatal, this.#ignoreBOM);
  }
}
define(g, "TextEncoder", TextEncoder);
define(g, "TextDecoder", TextDecoder);

const b64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const b64rev = new Int16Array(256).fill(-1);
for (let i = 0; i < 64; i++) b64rev[b64.charCodeAt(i)] = i;

function btoa(s) {
  s = String(s);
  let out = "";
  let i = 0;
  for (; i < s.length; i++) {
    if (s.charCodeAt(i) > 255) throw new DOMException("Invalid character", "InvalidCharacterError");
  }
  for (i = 0; i + 2 < s.length; i += 3) {
    const n = (s.charCodeAt(i) << 16) | (s.charCodeAt(i + 1) << 8) | s.charCodeAt(i + 2);
    out += b64[n >> 18] + b64[(n >> 12) & 63] + b64[(n >> 6) & 63] + b64[n & 63];
  }
  const rem = s.length - i;
  if (rem === 1) {
    const n = s.charCodeAt(i) << 16;
    out += b64[n >> 18] + b64[(n >> 12) & 63] + "==";
  } else if (rem === 2) {
    const n = (s.charCodeAt(i) << 16) | (s.charCodeAt(i + 1) << 8);
    out += b64[n >> 18] + b64[(n >> 12) & 63] + b64[(n >> 6) & 63] + "=";
  }
  return out;
}

function atob(s) {
  s = String(s).replace(/[\t\n\f\r ]/g, "");
  if (s.length % 4 === 0) s = s.replace(/==?$/, "");
  if (s.length % 4 === 1 || /[^A-Za-z0-9+/]/.test(s)) {
    throw new DOMException("The string to be decoded is not correctly encoded.", "InvalidCharacterError");
  }
  let out = "";
  let buf = 0, bits = 0;
  for (let i = 0; i < s.length; i++) {
    buf = (buf << 6) | b64rev[s.charCodeAt(i)];
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out += String.fromCharCode((buf >> bits) & 255);
    }
  }
  return out;
}

class DOMException extends Error {
  #name;
  constructor(message = "", name = "Error") {
    super(message);
    this.#name = String(name);
  }
  get name() { return this.#name; }
}
define(g, "DOMException", DOMException);
define(g, "btoa", btoa);
define(g, "atob", atob);
