// structuredClone.
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

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
