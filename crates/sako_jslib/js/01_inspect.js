// util.inspect-style formatting (port of Node's rules) and format().
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

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

const tsStackRe = /\.[cm]?tsx?:\d+:\d+/;

function errorToString(e) {
  let stack;
  try { stack = e.stack; } catch {}
  if (typeof stack === "string" && stack !== "") {
    // Hide runtime-internal frames.
    if (stack.includes("sako:bootstrap:")) stack = stack.split("\n").filter((l) => !l.includes("sako:bootstrap:")).join("\n");
    // Report TypeScript files at their original positions.
    if (tsStackRe.test(stack)) stack = ops.mapStack(stack);
    return stack;
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
