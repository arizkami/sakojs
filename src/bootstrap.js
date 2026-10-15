// Sako runtime bootstrap. Evaluated once as a classic script; its completion
// value is a function that receives the native `ops` object and installs the
// global APIs. Nothing here should do work eagerly that a script may not need.
(function bootstrap(ops) {
  "use strict";

  const {
    ObjectDefineProperty,
    ObjectGetOwnPropertyNames,
    ObjectGetOwnPropertySymbols,
    ObjectGetPrototypeOf,
    ObjectPrototypeToString,
    ObjectKeys,
    ArrayIsArray,
    ErrorCaptureStackTrace,
  } = {
    ObjectDefineProperty: Object.defineProperty,
    ObjectGetOwnPropertyNames: Object.getOwnPropertyNames,
    ObjectGetOwnPropertySymbols: Object.getOwnPropertySymbols,
    ObjectGetPrototypeOf: Object.getPrototypeOf,
    ObjectPrototypeToString: Function.prototype.call.bind(Object.prototype.toString),
    ObjectKeys: Object.keys,
    ArrayIsArray: Array.isArray,
    ErrorCaptureStackTrace: Error.captureStackTrace,
  };

  const g = globalThis;
  const define = (obj, name, value, enumerable = false) =>
    ObjectDefineProperty(obj, name, { value, writable: true, configurable: true, enumerable });

  // ---------------------------------------------------------------- inspect
  const TypedArrayProto = ObjectGetPrototypeOf(Uint8Array.prototype);
  const typedArrayTag = Object.getOwnPropertyDescriptor(TypedArrayProto, Symbol.toStringTag).get;
  const kCustomInspect = Symbol.for("nodejs.util.inspect.custom");
  const identRe = /^[A-Za-z_$][\w$]*$/;
  const strEscapeRe = /[\x00-\x1f\x27\x5c\x7f-\x9f]/;
  const strEscapeNoQuoteRe = /[\x00-\x1f\x5c\x7f-\x9f]/;
  const meta = { "\b": "\\b", "\t": "\\t", "\n": "\\n", "\f": "\\f", "\r": "\\r", "\\": "\\\\" };

  // Mirrors Node's strEscape: use "..." or `...` to avoid escaping single quotes.
  function quote(s) {
    let q = "'";
    let test = strEscapeRe;
    if (s.includes("'")) {
      if (!s.includes('"')) q = '"';
      else if (!s.includes("`") && !s.includes("${")) q = "`";
      if (q !== "'") test = strEscapeNoQuoteRe;
    }
    if (!test.test(s)) return q + s + q;
    let out = q;
    for (let i = 0; i < s.length; i++) {
      const c = s[i];
      const code = s.charCodeAt(i);
      if (c === q && q === "'") out += "\\'";
      else if (meta[c] !== undefined) out += meta[c];
      else if (code < 0x20 || (code >= 0x7f && code <= 0x9f)) out += "\\x" + code.toString(16).toUpperCase().padStart(2, "0");
      else out += c;
    }
    return out + q;
  }

  function fmtKey(k) {
    if (typeof k === "symbol") return k.toString();
    return identRe.test(k) ? k : quote(k);
  }

  function fmtNumber(n) {
    return Object.is(n, -0) ? "-0" : "" + n;
  }

  function fnBase(fn) {
    let src;
    try { src = Function.prototype.toString.call(fn); } catch { src = ""; }
    const name = fn.name;
    if (src.startsWith("class") && /^class\b/.test(src)) {
      const ext = ObjectGetPrototypeOf(fn);
      return "[class " + (name || "(anonymous)") + (ext && ext.name ? " extends " + ext.name : "") + "]";
    }
    const kind = fn.constructor && fn.constructor.name;
    const prefix = kind === "AsyncFunction" ? "AsyncFunction" : kind === "GeneratorFunction"
      ? "GeneratorFunction" : kind === "AsyncGeneratorFunction" ? "AsyncGeneratorFunction" : "Function";
    return "[" + prefix + (name ? ": " + name : " (anonymous)") + "]";
  }

  function ctorName(obj) {
    let proto = obj;
    while (proto !== null) {
      const d = Object.getOwnPropertyDescriptor(proto, "constructor");
      if (d && typeof d.value === "function" && d.value.name !== "") return d.value.name;
      proto = ObjectGetPrototypeOf(proto);
    }
    return null;
  }

  // Port of Node's groupArrayElements (lib/internal/util/inspect.js).
  function groupArrayElements(ctx, output, value, indent) {
    let totalLength = 0;
    let maxLength = 0;
    let outputLength = output.length;
    if (ctx.maxArrayLength < output.length) outputLength--; // "... n more items"
    const separatorSpace = 2;
    const dataLen = new Array(outputLength);
    for (let i = 0; i < outputLength; i++) {
      const len = output[i].length;
      dataLen[i] = len;
      totalLength += len + separatorSpace;
      if (maxLength < len) maxLength = len;
    }
    const actualMax = maxLength + separatorSpace;
    if (actualMax * 3 + indent < ctx.breakLength && (totalLength / actualMax > 5 || maxLength <= 6)) {
      const averageBias = Math.sqrt(actualMax - totalLength / output.length);
      const biasedMax = Math.max(actualMax - 3 - averageBias, 1);
      const columns = Math.min(
        Math.round(Math.sqrt(2.5 * biasedMax * outputLength) / biasedMax),
        Math.floor((ctx.breakLength - indent) / actualMax),
        ctx.compact * 4,
        15,
      );
      if (columns <= 1) return output;
      const tmp = [];
      const maxLineLength = [];
      for (let i = 0; i < columns; i++) {
        let lineLength = 0;
        for (let j = i; j < output.length; j += columns) if (dataLen[j] > lineLength) lineLength = dataLen[j];
        maxLineLength.push(lineLength + separatorSpace);
      }
      let padStart = true;
      if (value !== undefined) {
        for (let i = 0; i < output.length; i++) {
          if (typeof value[i] !== "number" && typeof value[i] !== "bigint") { padStart = false; break; }
        }
      }
      for (let i = 0; i < outputLength; i += columns) {
        const max = Math.min(i + columns, outputLength);
        let str = "";
        let j = i;
        for (; j < max - 1; j++) {
          const cell = output[j] + ", ";
          str += padStart ? cell.padStart(maxLineLength[j - i]) : cell.padEnd(maxLineLength[j - i]);
        }
        str += padStart ? output[j].padStart(maxLineLength[j - i] - separatorSpace) : output[j];
        tmp.push(str);
      }
      if (ctx.maxArrayLength < output.length) tmp.push(output[outputLength]);
      return tmp;
    }
    return output;
  }

  // Port of Node's reduceToSingleString with the default `compact: 3`.
  function reduceToSingle(output, base, braces, indent, ctx, depth, isArrayLike, value) {
    if (output.length === 0 && !base.includes("\n")) return base ? base + " " + braces[0] + braces[1] : braces[0] + braces[1];
    const entries = output.length;
    if (isArrayLike && entries > 6) output = groupArrayElements(ctx, output, value, indent);
    if (ctx.currentDepth - depth < ctx.compact && entries === output.length) {
      const start = output.length + indent + braces[0].length + base.length + 10;
      let total = output.length + start;
      let fits = total + output.length <= ctx.breakLength;
      for (let i = 0; fits && i < output.length; i++) {
        total += output[i].length;
        if (total > ctx.breakLength) fits = false;
      }
      if (fits && !base.includes("\n")) {
        const joined = output.join(", ");
        if (!joined.includes("\n")) return (base ? base + " " : "") + braces[0] + " " + joined + " " + braces[1];
      }
    }
    const ind = "\n" + " ".repeat(indent);
    return (base ? base + " " : "") + braces[0] + ind + "  " + output.join("," + ind + "  ") + ind + braces[1];
  }

  function fmtValue(v, ctx, depth, indent, nested) {
    switch (typeof v) {
      case "string": return nested ? quote(v) : v;
      case "number": return fmtNumber(v);
      case "bigint": return v + "n";
      case "boolean": return "" + v;
      case "undefined": return "undefined";
      case "symbol": return v.toString();
    }
    if (v === null) return "null";

    if (typeof v[kCustomInspect] === "function") {
      const r = v[kCustomInspect](ctx.depth - depth, ctx, inspect);
      if (r !== v) return typeof r === "string" ? r : fmtValue(r, ctx, depth, indent, nested);
    }

    if (ctx.seen.includes(v)) {
      let index = ctx.circular.get(v);
      if (index === undefined) ctx.circular.set(v, (index = ctx.circular.size + 1));
      return "[Circular *" + index + "]";
    }

    let base = "";
    let braces = ["{", "}"];
    let parts = [];
    let keys = null;
    const isFn = typeof v === "function";

    if (isFn) {
      base = fnBase(v);
      keys = ObjectKeys(v);
      if (keys.length === 0) return base;
    } else if (v instanceof Error) {
      base = errorToString(v);
      keys = ObjectKeys(v).filter((k) => k !== "stack" && k !== "message");
      if (keys.length === 0) return nested && indent ? base.split("\n").join("\n" + " ".repeat(indent)) : base;
    } else if (ArrayIsArray(v)) {
      braces = ["[", "]"];
      const name = ctorName(v);
      if (name !== "Array") base = (name || "[Array: null prototype]") + "(" + v.length + ")";
    } else if (v instanceof Date) {
      return isNaN(v) ? "Invalid Date" : v.toISOString();
    } else if (v instanceof RegExp) {
      return RegExp.prototype.toString.call(v);
    } else if (ArrayBuffer.isView(v) && !(v instanceof DataView)) {
      const tag = typedArrayTag.call(v);
      base = tag + "(" + v.length + ")";
      braces = ["[", "]"];
    } else if (v instanceof Map) {
      base = (ctorName(v) || "Map") + "(" + v.size + ")";
    } else if (v instanceof Set) {
      base = (ctorName(v) || "Set") + "(" + v.size + ")";
    } else if (v instanceof Promise) {
      const [state, result] = ops.promiseState(v);
      base = "Promise";
      if (state === 0) parts.push("<pending>");
      else {
        if (depth > ctx.depth) parts.push(state === 2 ? "<rejected> [Object]" : "[Object]");
        else {
          ctx.seen.push(v);
          const r = fmtValue(result, ctx, depth + 1, indent + 2, true);
          ctx.seen.pop();
          parts.push(state === 2 ? "<rejected> " + r : r);
        }
      }
    } else if (v instanceof ArrayBuffer) {
      base = "ArrayBuffer";
      const bytes = new Uint8Array(v, 0, Math.min(v.byteLength, ctx.maxArrayLength));
      let hex = "";
      for (let i = 0; i < bytes.length; i++) hex += (i ? " " : "") + (bytes[i] < 16 ? "0" : "") + bytes[i].toString(16);
      const more = v.byteLength - bytes.length;
      parts.push("[Uint8Contents]: <" + hex + (more > 0 ? ` ... ${more} more byte${more > 1 ? "s" : ""}` : "") + ">", "[byteLength]: " + v.byteLength);
    } else if (v instanceof WeakMap || v instanceof WeakSet) {
      return (v instanceof WeakMap ? "WeakMap" : "WeakSet") + " { <items unknown> }";
    } else {
      const name = ctorName(v);
      const tag = v[Symbol.toStringTag];
      if (name === null) base = "[Object: null prototype]";
      else if (name !== "Object") base = name;
      if (typeof tag === "string" && tag !== "" && tag !== name) base = (base || "Object") + " [" + tag + "]";
      if (name === "Number" || name === "String" || name === "Boolean") {
        base = "[" + name + ": " + fmtValue(v.valueOf(), ctx, depth, indent, true) + "]";
        keys = ObjectKeys(v).filter((k) => !(name === "String" && /^\d+$/.test(k)));
        if (keys.length === 0) return base;
      }
    }

    if (depth > ctx.depth) {
      if (ArrayIsArray(v)) return "[Array]";
      return "[" + (ctorName(v) || "Object") + "]";
    }

    ctx.seen.push(v);
    ctx.currentDepth = depth;
    const nextIndent = indent + 2;
    if (ArrayIsArray(v) || (ArrayBuffer.isView(v) && !(v instanceof DataView))) {
      const len = v.length;
      const max = Math.min(len, ctx.maxArrayLength);
      let holes = 0;
      for (let i = 0; i < max; i++) {
        if (!(i in v)) { holes++; continue; }
        if (holes) { parts.push("<" + holes + " empty item" + (holes > 1 ? "s" : "") + ">"); holes = 0; }
        parts.push(fmtValue(v[i], ctx, depth + 1, nextIndent, true));
      }
      if (holes) parts.push("<" + holes + " empty item" + (holes > 1 ? "s" : "") + ">");
      if (len > max) parts.push("... " + (len - max) + " more item" + (len - max > 1 ? "s" : ""));
      if (keys === null) keys = ObjectKeys(v).filter((k) => !/^\d+$/.test(k));
    } else if (v instanceof Map) {
      for (const [k, val] of v) {
        parts.push(fmtValue(k, ctx, depth + 1, nextIndent, true) + " => " + fmtValue(val, ctx, depth + 1, nextIndent, true));
      }
    } else if (v instanceof Set) {
      for (const val of v) parts.push(fmtValue(val, ctx, depth + 1, nextIndent, true));
    }
    if (keys === null) keys = ObjectKeys(v);
    for (const k of keys) {
      let val;
      const desc = Object.getOwnPropertyDescriptor(v, k);
      if (desc && !("value" in desc) && (desc.get || desc.set)) {
        parts.push(fmtKey(k) + ": " + (desc.get ? (desc.set ? "[Getter/Setter]" : "[Getter]") : "[Setter]"));
        continue;
      }
      try { val = v[k]; } catch (e) { val = e; }
      parts.push(fmtKey(k) + ": " + fmtValue(val, ctx, depth + 1, nextIndent, true));
    }
    for (const s of ObjectGetOwnPropertySymbols(v)) {
      const d = Object.getOwnPropertyDescriptor(v, s);
      if (d && d.enumerable) parts.push(fmtKey(s) + ": " + fmtValue(v[s], ctx, depth + 1, nextIndent, true));
    }
    ctx.seen.pop();
    if (isFn || v instanceof Error) braces = ["{", "}"];
    const ref = ctx.circular.get(v);
    if (ref !== undefined) base = base === "" ? "<ref *" + ref + ">" : "<ref *" + ref + "> " + base;
    const isArrayLike = ArrayIsArray(v) || (ArrayBuffer.isView(v) && !(v instanceof DataView));
    return reduceToSingle(parts, base, braces, indent, ctx, depth, isArrayLike, isArrayLike ? v : undefined);
  }

  function errorToString(e) {
    let stack;
    try { stack = e.stack; } catch {}
    if (typeof stack === "string" && stack !== "") {
      // Hide runtime-internal frames.
      return stack.includes("sako:bootstrap:") ? stack.split("\n").filter((l) => !l.includes("sako:bootstrap:")).join("\n") : stack;
    }
    try { return Error.prototype.toString.call(e); } catch { return "[Error]"; }
  }

  function inspect(v, opts) {
    const ctx = {
      depth: opts && opts.depth !== undefined ? (opts.depth === null ? Infinity : opts.depth) : 2,
      breakLength: (opts && opts.breakLength) || 80,
      maxArrayLength: (opts && opts.maxArrayLength) ?? 100,
      compact: 3,
      currentDepth: 0,
      seen: [],
      circular: new Map(),
    };
    return fmtValue(v, ctx, 0, 0, true);
  }

  function format(...args) {
    const first = args[0];
    let out = "";
    let i = 0;
    if (typeof first === "string") {
      i = 1;
      if (first.indexOf("%") === -1) out = first;
      else {
        let last = 0;
        for (let j = 0; j < first.length - 1; j++) {
          if (first.charCodeAt(j) !== 37) continue; // '%'
          const c = first[j + 1];
          let rep;
          switch (c) {
            case "s": {
              const a = args[i];
              rep = typeof a === "string" ? a : typeof a === "bigint" ? a + "n" :
                typeof a === "object" && a !== null ? inspect(a, { depth: 1 }) : String(a);
              break;
            }
            case "d": case "i": {
              const a = args[i];
              rep = typeof a === "bigint" ? a + "n" : typeof a === "symbol" ? "NaN" : fmtNumber(c === "i" ? parseInt(a) : Number(a));
              break;
            }
            case "f": rep = typeof args[i] === "symbol" ? "NaN" : "" + parseFloat(args[i]); break;
            case "j": try { rep = JSON.stringify(args[i]); } catch { rep = "[Circular]"; } break;
            case "o": rep = inspect(args[i], { depth: 4 }); break;
            case "O": rep = inspect(args[i]); break;
            case "c": rep = ""; break;
            case "%": out += first.slice(last, j) + "%"; last = j + 2; j++; continue;
            default: continue;
          }
          if (i >= args.length) continue;
          i++;
          out += first.slice(last, j) + rep;
          last = j + 2;
          j++;
        }
        out += first.slice(last);
      }
    }
    for (; i < args.length; i++) {
      const a = args[i];
      if (out.length || i > 0) out += " ";
      out += typeof a === "string" ? a : inspect(a);
    }
    return out;
  }

  // ---------------------------------------------------------------- console
  const print = ops.print;
  let groupIndent = "";
  const counts = new Map();
  const timers = new Map();

  function emit(isErr, args) {
    let s = args.length === 1 && typeof args[0] === "string" ? args[0] : format(...args);
    if (groupIndent) s = groupIndent + s.split("\n").join("\n" + groupIndent);
    print(s + "\n", isErr);
  }

  const console = {
    log(...args) { emit(false, args); },
    info(...args) { emit(false, args); },
    debug(...args) { emit(false, args); },
    error(...args) { emit(true, args); },
    warn(...args) { emit(true, args); },
    trace(...args) {
      const err = { name: "Trace", message: format(...args) };
      ErrorCaptureStackTrace(err, console.trace);
      emit(true, [err.stack]);
    },
    dir(v, opts) { emit(false, [inspect(v, opts)]); },
    dirxml(...args) { emit(false, args); },
    assert(cond, ...args) {
      if (!cond) emit(true, args.length ? ["Assertion failed:", ...args] : ["Assertion failed"]);
    },
    count(label = "default") {
      const n = (counts.get(label) || 0) + 1;
      counts.set(label, n);
      emit(false, [label + ": " + n]);
    },
    countReset(label = "default") { counts.delete(label); },
    group(...args) { if (args.length) emit(false, args); groupIndent += "  "; },
    groupCollapsed(...args) { console.group(...args); },
    groupEnd() { groupIndent = groupIndent.slice(2); },
    time(label = "default") { timers.set(label, ops.now()); },
    timeLog(label = "default", ...args) {
      const t = timers.get(label);
      if (t === undefined) return emit(true, [`Warning: No such label '${label}' for console.timeLog()`]);
      emit(false, [label + ": " + fmtMs(ops.now() - t), ...args]);
    },
    timeEnd(label = "default") {
      const t = timers.get(label);
      if (t === undefined) return emit(true, [`Warning: No such label '${label}' for console.timeEnd()`]);
      timers.delete(label);
      emit(false, [label + ": " + fmtMs(ops.now() - t)]);
    },
    table(data) {
      if (data === null || typeof data !== "object") return emit(false, [data]);
      const rows = [];
      const cols = [];
      const colSet = new Set();
      let hasValues = false;
      const entries = data instanceof Map ? [...data] : Object.entries(data);
      for (const [k, row] of entries) {
        const r = { key: String(k), cells: {} };
        if (row !== null && typeof row === "object") {
          for (const c of ObjectKeys(row)) {
            if (!colSet.has(c)) { colSet.add(c); cols.push(c); }
            r.cells[c] = fmtValue(row[c], tableCtx(), 1, 0, true);
          }
        } else { hasValues = true; r.value = fmtValue(row, tableCtx(), 1, 0, true); }
        rows.push(r);
      }
      const header = ["(index)", ...cols, ...(hasValues ? ["Values"] : [])];
      const table = rows.map((r) => [r.key, ...cols.map((c) => r.cells[c] ?? ""), ...(hasValues ? [r.value ?? ""] : [])]);
      const widths = header.map((h, i) => Math.max(h.length, ...table.map((row) => row[i].length)) + 2);
      const center = (s, w) => " " + s.padEnd(w - 2) + " ";
      const line = (l, m, r) => l + widths.map((w) => "\u2500".repeat(w)).join(m) + r;
      const out = [line("\u250c", "\u252c", "\u2510"), "\u2502" + header.map((h, i) => center(h, widths[i])).join("\u2502") + "\u2502", line("\u251c", "\u253c", "\u2524")];
      for (const row of table) out.push("\u2502" + row.map((c, i) => center(c, widths[i])).join("\u2502") + "\u2502");
      out.push(line("\u2514", "\u2534", "\u2518"));
      emit(false, [out.join("\n")]);
    },
    clear() {},
  };

  const tableCtx = () => ({ depth: 0, breakLength: Infinity, maxArrayLength: 3, compact: 3, currentDepth: 0, seen: [], circular: new Map() });

  function fmtMs(ms) {
    if (ms >= 1000) return (ms / 1000).toFixed(3) + "s";
    return ms.toFixed(3) + "ms";
  }

  // ----------------------------------------------------------------- timers
  // Callbacks live here; Rust only tracks (deadline, id) in a binary heap.
  const timerMap = new Map();
  let nextTimerId = 1;

  function dispatchTimer(id) {
    const t = timerMap.get(id);
    if (t === undefined) return;
    if (t.repeat) ops.timerStart(id, t.ms, t.unref);
    else timerMap.delete(id);
    if (t.args === undefined) t.cb();
    else t.cb(...t.args);
  }

  function addTimer(cb, ms, args, repeat, unref = false) {
    if (typeof cb !== "function") throw new TypeError("The \"callback\" argument must be of type function");
    ms = +ms;
    if (!(ms >= 1)) ms = repeat ? 1 : 0;
    if (ms > 2147483647) ms = 1;
    const id = nextTimerId++;
    timerMap.set(id, { cb, ms, args: args.length ? args : undefined, repeat, unref });
    ops.timerStart(id, ms, unref);
    return id;
  }

  function clearTimer(id) {
    if (typeof id === "object" && id !== null) id = id[Symbol.toPrimitive]?.() ?? id.id;
    if (timerMap.delete(id)) ops.timerClear(id);
  }

  define(g, "setTimeout", function setTimeout(cb, ms, ...args) { return addTimer(cb, ms, args, false); });
  define(g, "setInterval", function setInterval(cb, ms, ...args) { return addTimer(cb, ms, args, true); });
  // Immediates run in their own phase after timers and I/O, like Node's
  // check phase. Rust snapshots the queue once per loop turn and runs each
  // entry with a microtask checkpoint in between.
  let immediateQueue = [];
  let runningImmediates = [];
  const immediateMap = new Map();
  function setImmediate(cb, ...args) {
    if (typeof cb !== "function") throw new TypeError("The \"callback\" argument must be of type function");
    const id = nextTimerId++;
    const im = { id, cb, args: args.length ? args : undefined };
    immediateMap.set(id, im);
    if (immediateQueue.length === 0) ops.immediatesPending();
    immediateQueue.push(im);
    return id;
  }
  function startImmediates() {
    runningImmediates = immediateQueue;
    immediateQueue = [];
    return runningImmediates.length;
  }
  function runImmediate(i) {
    const im = runningImmediates[i];
    if (!immediateMap.delete(im.id)) return; // cleared
    if (im.args === undefined) im.cb();
    else im.cb(...im.args);
  }
  define(g, "setImmediate", setImmediate);
  define(g, "clearTimeout", function clearTimeout(id) { clearTimer(id); });
  define(g, "clearInterval", function clearInterval(id) { clearTimer(id); });
  define(g, "clearImmediate", function clearImmediate(id) { immediateMap.delete(id); });

  // process.nextTick queue. Rust drains it before each microtask checkpoint,
  // so ticks run before promise jobs, as in Node.
  const tickQueue = [];
  function runTicks() {
    try {
      for (let i = 0; i < tickQueue.length; i++) tickQueue[i]();
    } finally {
      tickQueue.length = 0;
    }
  }

  const resolved = Promise.resolve();
  function queueMicrotask(cb) {
    if (typeof cb !== "function") throw new TypeError("The \"callback\" argument must be of type function");
    resolved.then(() => {
      try { cb(); } catch (e) { ops.reportError(e); }
    });
  }
  define(g, "queueMicrotask", queueMicrotask);

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

  // --------------------------------------------------------- structuredClone
  function structuredClone(value, seen = new Map()) {
    if (typeof value !== "object" || value === null) {
      if (typeof value === "function" || typeof value === "symbol") {
        throw new DOMException(`${String(value)} could not be cloned.`, "DataCloneError");
      }
      return value;
    }
    if (seen.has(value)) return seen.get(value);
    let out;
    if (value instanceof Date) out = new Date(value.getTime());
    else if (value instanceof RegExp) out = new RegExp(value.source, value.flags);
    else if (value instanceof ArrayBuffer) out = value.slice(0);
    else if (ArrayBuffer.isView(value)) {
      out = new value.constructor(value.buffer.slice(value.byteOffset, value.byteOffset + value.byteLength));
    } else if (value instanceof Map) {
      out = new Map();
      seen.set(value, out);
      for (const [k, v] of value) out.set(structuredClone(k, seen), structuredClone(v, seen));
      return out;
    } else if (value instanceof Set) {
      out = new Set();
      seen.set(value, out);
      for (const v of value) out.add(structuredClone(v, seen));
      return out;
    } else if (value instanceof Error) {
      out = new (g[value.name] || Error)(value.message);
      out.stack = value.stack;
    } else if (ArrayIsArray(value)) {
      out = new Array(value.length);
      seen.set(value, out);
      for (let i = 0; i < value.length; i++) if (i in value) out[i] = structuredClone(value[i], seen);
      return out;
    } else {
      out = {};
      seen.set(value, out);
      for (const k of ObjectKeys(value)) out[k] = structuredClone(value[k], seen);
      return out;
    }
    seen.set(value, out);
    return out;
  }
  define(g, "structuredClone", function (v) { return structuredClone(v); });

  // ------------------------------------------------------------ performance
  const timeOrigin = ops.timeOrigin;
  const performance = {
    now: ops.now,
    timeOrigin,
    mark() {},
    measure() {},
    toJSON() { return { timeOrigin }; },
  };
  define(g, "performance", performance);

  // ---------------------------------------------------------------- process
  const nsPerMs = 1000000n;
  function hrtime(prev) {
    const t = ops.now();
    let sec = Math.floor(t / 1000);
    let ns = Math.round((t - sec * 1000) * 1e6);
    if (prev) {
      sec -= prev[0];
      ns -= prev[1];
      if (ns < 0) { sec--; ns += 1e9; }
    }
    return [sec, ns];
  }
  hrtime.bigint = () => BigInt(Math.round(ops.now() * 1e6)) + timeOriginNs;
  const timeOriginNs = BigInt(Math.round(timeOrigin)) * nsPerMs;

  const process = {
    argv: ops.argv,
    env: ops.env,
    pid: ops.pid,
    platform: ops.platform,
    arch: ops.arch,
    version: "v" + ops.version,
    versions: { sako: ops.version, v8: ops.v8Version },
    execPath: ops.execPath,
    exitCode: undefined,
    exit(code) { ops.exit(code === undefined ? (process.exitCode ?? 0) : code | 0); },
    cwd: ops.cwd,
    chdir: ops.chdir,
    hrtime,
    uptime() { return ops.now() / 1000; },
    memoryUsage: ops.memoryUsage,
    nextTick(cb, ...args) {
      if (typeof cb !== "function") throw new TypeError("The \"callback\" argument must be of type function");
      if (tickQueue.length === 0) ops.ticksPending();
      tickQueue.push(args.length ? () => cb(...args) : cb);
    },
    stdout: { write(s) { print(typeof s === "string" ? s : new TextDecoder().decode(s), false); return true; }, isTTY: ops.stdoutIsTTY },
    stderr: { write(s) { print(typeof s === "string" ? s : new TextDecoder().decode(s), true); return true; }, isTTY: ops.stderrIsTTY },
    on() { return process; },
    once() { return process; },
    off() { return process; },
    emitWarning(w) { print("Warning: " + w + "\n", true); },
  };
  define(g, "process", process);

  // ------------------------------------------------------- Event, Abort
  const kStop = Symbol("stop");
  class Event {
    constructor(type, init = {}) {
      if (arguments.length === 0) throw new TypeError("The \"type\" argument must be specified");
      this.type = String(type);
      this.bubbles = !!init.bubbles;
      this.cancelable = !!init.cancelable;
      this.composed = !!init.composed;
      this.defaultPrevented = false;
      this.timeStamp = ops.now();
      this.target = null;
      this.currentTarget = null;
      this[kStop] = false;
    }
    preventDefault() { if (this.cancelable) this.defaultPrevented = true; }
    stopPropagation() {}
    stopImmediatePropagation() { this[kStop] = true; }
  }

  class EventTarget {
    #listeners = new Map();
    addEventListener(type, callback, options) {
      if (callback == null) return;
      const once = typeof options === "object" && options !== null && !!options.once;
      const capture = typeof options === "boolean" ? options : !!(options && options.capture);
      const signal = typeof options === "object" && options !== null ? options.signal : undefined;
      if (signal && signal.aborted) return;
      let list = this.#listeners.get(type);
      if (!list) this.#listeners.set(type, (list = []));
      if (list.some((l) => l.callback === callback && l.capture === capture)) return;
      const entry = { callback, once, capture };
      list.push(entry);
      if (signal) signal.addEventListener("abort", () => this.removeEventListener(type, callback, { capture }));
    }
    removeEventListener(type, callback, options) {
      const list = this.#listeners.get(type);
      if (!list) return;
      const capture = typeof options === "boolean" ? options : !!(options && options.capture);
      const i = list.findIndex((l) => l.callback === callback && l.capture === capture);
      if (i !== -1) list.splice(i, 1);
    }
    dispatchEvent(event) {
      if (!(event instanceof Event)) throw new TypeError("The \"event\" argument must be an instance of Event");
      event.target = this;
      event.currentTarget = this;
      const handler = this["on" + event.type];
      const list = (this.#listeners.get(event.type) || []).slice();
      if (typeof handler === "function") list.unshift({ callback: handler, once: false });
      for (const l of list) {
        if (l.once) this.removeEventListener(event.type, l.callback, { capture: l.capture });
        try {
          if (typeof l.callback === "function") l.callback.call(this, event);
          else l.callback.handleEvent(event);
        } catch (e) {
          queueMicrotask(() => { throw e; });
        }
        if (event[kStop]) break;
      }
      event.currentTarget = null;
      return !event.defaultPrevented;
    }
  }

  const kAbort = Symbol("abort");
  class AbortSignal extends EventTarget {
    #aborted = false;
    #reason = undefined;
    onabort = null;
    get aborted() { return this.#aborted; }
    get reason() { return this.#reason; }
    throwIfAborted() { if (this.#aborted) throw this.#reason; }
    [kAbort](reason) {
      if (this.#aborted) return;
      this.#aborted = true;
      this.#reason = reason === undefined ? new DOMException("This operation was aborted", "AbortError") : reason;
      this.dispatchEvent(new Event("abort"));
    }
    static abort(reason) {
      const s = new AbortSignal();
      s[kAbort](reason);
      return s;
    }
    static timeout(ms) {
      const s = new AbortSignal();
      addTimer(() => s[kAbort](new DOMException("The operation was aborted due to timeout", "TimeoutError")), ms, [], false, true);
      return s;
    }
    static any(signals) {
      const s = new AbortSignal();
      for (const sig of signals) {
        if (sig.aborted) { s[kAbort](sig.reason); return s; }
      }
      for (const sig of signals) sig.addEventListener("abort", () => s[kAbort](sig.reason), { once: true });
      return s;
    }
  }

  class AbortController {
    #signal = new AbortSignal();
    get signal() { return this.#signal; }
    abort(reason) { this.#signal[kAbort](reason); }
  }

  define(g, "Event", Event);
  define(g, "EventTarget", EventTarget);
  define(g, "AbortSignal", AbortSignal);
  define(g, "AbortController", AbortController);

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

  // ------------------------------------------------------------------- Sako
  const toDirents = (list) => list.map(([name, isFile, isDirectory, isSymlink]) => ({ name, isFile, isDirectory, isSymlink }));
  const toStat = ([size, isFile, isDirectory, isSymlink, mtimeMs]) =>
    ({ size, isFile, isDirectory, isSymlink, mtimeMs, mtime: new Date(mtimeMs) });
  const Sako = {
    version: ops.version,
    args: ops.args,
    inspect,
    format,
    exit: process.exit,
    env: ops.env,
    cwd: ops.cwd,
    // Sync variants run inline; async variants run on the I/O thread pool.
    readTextFileSync: ops.readTextFile,
    readFileSync: ops.readFile,
    writeTextFileSync: (p, s, o) => ops.writeFile(p, String(s), !!(o && o.append)),
    writeFileSync: (p, d, o) => ops.writeFile(p, d, !!(o && o.append)),
    existsSync: ops.exists,
    readDirSync: (p) => toDirents(ops.readDir(p)),
    mkdirSync: (p, o) => ops.mkdir(p, !!(o && o.recursive)),
    removeSync: (p, o) => ops.remove(p, !!(o && o.recursive)),
    statSync: (p) => toStat(ops.stat(p)),
    readTextFile: ops.readTextFileAsync,
    readFile: ops.readFileAsync,
    writeTextFile: (p, s, o) => ops.writeFileAsync(p, String(s), !!(o && o.append)),
    writeFile: (p, d, o) => ops.writeFileAsync(p, d, !!(o && o.append)),
    exists: ops.existsAsync,
    readDir: async (p) => toDirents(await ops.readDirAsync(p)),
    mkdir: (p, o) => ops.mkdirAsync(p, !!(o && o.recursive)),
    remove: (p, o) => ops.removeAsync(p, !!(o && o.recursive)),
    stat: async (p) => toStat(await ops.statAsync(p)),
    serve,
  };
  define(g, "Sako", Object.freeze(Sako));

  define(g, "console", console);
  define(g, "global", g);
  define(g, "self", g);

  ops.setHooks(dispatchTimer, inspect, (p, ns) => p.then(() => ns), runTicks, startImmediates, runImmediate, httpDispatch);
})
