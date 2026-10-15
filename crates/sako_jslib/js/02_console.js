// The console object.
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

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
