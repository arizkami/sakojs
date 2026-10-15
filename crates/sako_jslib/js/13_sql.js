// Sako.sql / Sako.SQL: PostgreSQL client modelled on Bun.sql and postgres.js.
// Queries are lazy tagged templates: values become $n parameters, nested
// sql`...` and sql(...) helpers are inlined, and the query runs when awaited
// (or on the next tick). Connection, protocol and pooling live in Rust
// (crates/sako_pg); this file builds SQL text and wraps results.

class PostgresError extends Error {
  get name() { return "PostgresError"; }
}

// A built query: text parts interleaved with parameters
// (parts.length === params.length + 1), so nesting never renumbers text.
const kBuild = Symbol("build");
const kConsume = Symbol("consume");
const kParam = Symbol("param");

class SQLParam {
  constructor(value) { this[kParam] = value; }
}

// sql(object | array | "identifier", ...columns): resolved against the SQL
// text before it (INSERT column list, UPDATE SET list, IN list, identifiers).
class SQLHelper {
  constructor(value, columns) {
    this.value = value;
    this.columns = columns;
  }
}

const quoteIdent = (name) => String(name).split(".").map((p) => '"' + p.replace(/"/g, '""') + '"').join(".");

function pgArrayLiteral(arr) {
  return "{" + arr.map((v) => {
    if (v === null || v === undefined) return "NULL";
    if (ArrayIsArray(v)) return pgArrayLiteral(v);
    const s = typeof v === "string" ? v : typeof v === "object" && !(v instanceof Date) ? JSON.stringify(v) : pgParam(v);
    return '"' + s.replace(/\\/g, "\\\\").replace(/"/g, '\\"') + '"';
  }).join(",") + "}";
}

// Parameter type OIDs declared for JS types the server can't infer from
// text alone (e.g. `select ${true}`); 0 lets the server decide.
function pgType(v) {
  switch (typeof v) {
    case "boolean": return 16;
    case "bigint": return 20;
    case "object":
      if (v === null || ArrayIsArray(v) || v instanceof SQLParam) return 0;
      if (v instanceof Date) return 1184;
      if (v instanceof ArrayBuffer || ArrayBuffer.isView(v)) return 17;
      return 3802; // plain objects as jsonb
  }
  return 0;
}

// JS value -> text parameter (null stays null, bytes stay bytes).
function pgParam(v) {
  if (v === null || v === undefined) return null;
  switch (typeof v) {
    case "string": return v;
    case "number": case "bigint": return String(v);
    case "boolean": return v ? "t" : "f";
  }
  if (v instanceof SQLParam) return v[kParam];
  if (v instanceof Date) return v.toISOString();
  if (v instanceof ArrayBuffer || ArrayBuffer.isView(v)) return v;
  if (ArrayIsArray(v)) return pgArrayLiteral(v);
  return JSON.stringify(v);
}

function appendParam(out, value) {
  out.params.push(pgParam(value));
  out.types.push(pgType(value));
  out.parts.push("");
}

function appendText(out, text) {
  out.parts[out.parts.length - 1] += text;
}

function appendBuilt(out, built) {
  if (built.rawParams) throw new TypeError("sql.unsafe() with parameters cannot be nested in another query");
  appendText(out, built.parts[0]);
  for (let i = 0; i < built.params.length; i++) {
    out.params.push(built.params[i]);
    out.types.push(built.types[i]);
    out.parts.push(built.parts[i + 1]);
  }
}

function appendHelper(out, h) {
  const v = h.value;
  const before = out.parts[out.parts.length - 1];
  if (typeof v === "string") return appendText(out, quoteIdent(v));
  if (ArrayIsArray(v) && v.every((x) => typeof x === "string") && !/\bin\s*$/i.test(before)) {
    return appendText(out, v.map(quoteIdent).join(", "));
  }
  if (ArrayIsArray(v) && v.every((x) => x === null || typeof x !== "object" || x instanceof Date)) {
    // `where id in ${sql([1, 2, 3])}`
    appendText(out, "(");
    v.forEach((x, i) => { if (i) appendText(out, ", "); appendParam(out, x); });
    return appendText(out, ")");
  }
  const rows = ArrayIsArray(v) ? v : [v];
  if (rows.length === 0) throw new TypeError("sql(): no rows to insert");
  const cols = h.columns.length ? h.columns.flat() : ObjectKeys(rows[0]);
  if (/\bset\s*$/i.test(before)) {
    // `update t set ${sql(obj)}`
    cols.forEach((c, i) => { appendText(out, (i ? ", " : "") + quoteIdent(c) + " = "); appendParam(out, rows[0][c]); });
    return;
  }
  // `insert into t ${sql(obj or rows)}`
  appendText(out, "(" + cols.map(quoteIdent).join(", ") + ") values ");
  rows.forEach((row, r) => {
    appendText(out, (r ? ", " : "") + "(");
    cols.forEach((c, i) => { if (i) appendText(out, ", "); appendParam(out, row[c]); });
    appendText(out, ")");
  });
}

function buildTemplate(strings, values) {
  const out = { parts: [strings[0]], params: [], types: [] };
  for (let i = 0; i < values.length; i++) {
    const v = values[i];
    if (v instanceof SQLQuery) appendBuilt(out, v[kBuild]());
    else if (v instanceof SQLHelper) appendHelper(out, v);
    else appendParam(out, v);
    appendText(out, strings[i + 1]);
  }
  return out;
}

function finishText(built) {
  let text = built.parts[0];
  for (let i = 0; i < built.params.length; i++) text += "$" + (i + 1) + built.parts[i + 1];
  return text;
}

// Date/timestamp cells arrive as epoch milliseconds (or as text for values
// like 'infinity').
const toDate = (x) => (typeof x === "number" ? new Date(x) : x);

// Row constructors by column list: an object literal per column set, so
// every row of a result shares one hidden class.
const rowFactories = new Map();
function rowFactory(names, dates) {
  const key = names.join("\0") + "\0" + dates.map(Number).join("");
  let f = rowFactories.get(key);
  if (f === undefined) {
    const cell = (k) => (dates[k] ? "d(v[i + " + k + "])" : "v[i + " + k + "]");
    const unique = new Set(names).size === names.length;
    f = unique
      ? new Function("v", "i", "d", "return {" + names.map((n, k) => JSON.stringify(n) + ": " + cell(k)).join(", ") + "};")
      : (v, i, d) => { const o = {}; names.forEach((n, k) => { o[n] = dates[k] ? d(v[i + k]) : v[i + k]; }); return o; };
    if (rowFactories.size < 1000) rowFactories.set(key, f);
  }
  return f;
}

// [columnNames, flatValues, command, count, dateColumns] -> rows (objects or arrays).
function makeRows(r, asArrays) {
  const names = r[0];
  const flat = r[1];
  const dates = r[4];
  const width = names.length;
  const n = width === 0 ? 0 : flat.length / width;
  const rows = new Array(n);
  if (asArrays) {
    const anyDate = dates.includes(true);
    for (let i = 0; i < n; i++) {
      const row = flat.slice(i * width, i * width + width);
      if (anyDate) for (let k = 0; k < width; k++) if (dates[k]) row[k] = toDate(row[k]);
      rows[i] = row;
    }
  } else if (n > 0) {
    const make = rowFactory(names, dates);
    for (let i = 0; i < n; i++) rows[i] = make(flat, i * width, toDate);
  }
  ObjectDefineProperty(rows, "command", { value: r[2], configurable: true });
  ObjectDefineProperty(rows, "count", { value: r[3], configurable: true });
  return rows;
}

function asPostgresError(e) {
  if (e !== null && typeof e === "object" && e.name === "PostgresError" && !(e instanceof PostgresError)) {
    Object.setPrototypeOf(e, PostgresError.prototype);
  }
  throw e;
}

class SQLQuery {
  #conn; #build; #values = false; #simple = false; #consumed = false; #promise = null;
  constructor(conn, build) {
    this.#conn = conn;
    this.#build = build;
    // Runs even if never awaited, unless inlined into another query first.
    resolved.then(() => { if (!this.#consumed && this.#promise === null) this.#run(); });
  }
  // Claimed synchronously by the query it is nested in (before its own
  // auto-run tick), then built as part of that query.
  [kConsume]() {
    if (this.#promise !== null) throw new TypeError("sql: a query that already ran cannot be nested");
    this.#consumed = true;
  }
  [kBuild]() {
    this.#consumed = true;
    return this.#build();
  }
  #run() {
    if (this.#promise === null) {
      try {
        const built = this.#build();
        const params = built.rawParams ?? built.params;
        const text = built.rawParams ? built.parts[0] : finishText(built);
        const simple = this.#simple && params.length === 0;
        const types = built.rawParams ? undefined : built.types;
        const asArrays = this.#values;
        this.#promise = this.#conn.run(text, params, simple, asArrays, types).then((r) => makeRows(r, asArrays), asPostgresError);
      } catch (e) {
        this.#promise = Promise.reject(e);
      }
    }
    return this.#promise;
  }
  /** Rows as arrays of column values instead of objects. */
  values() { this.#values = true; return this; }
  /** Use the simple protocol (several statements allowed, no parameters). */
  simple() { this.#simple = true; return this; }
  then(onFulfilled, onRejected) { return this.#run().then(onFulfilled, onRejected); }
  catch(onRejected) { return this.#run().catch(onRejected); }
  finally(onFinally) { return this.#run().finally(onFinally); }
  get [Symbol.toStringTag]() { return "SQLQuery"; }
}

// One client: a pool (txId 0) or a transaction bound to one connection.
function makeSql(pool, txId) {
  const conn = {
    run: (text, params, simple, values, types) => ops.pgQuery(pool.id, txId, text, params, simple, values, types),
  };
  function sql(strings, ...values) {
    if (ArrayIsArray(strings) && ArrayIsArray(strings.raw)) {
      for (const v of values) if (v instanceof SQLQuery) v[kConsume]();
      return new SQLQuery(conn, () => buildTemplate(strings, values));
    }
    return new SQLHelper(strings, values);
  }
  /**
   * Raw SQL text. Without parameters it may hold several statements and can
   * be nested in another query as a text fragment; with `$1`-style
   * parameters it runs as given.
   */
  sql.unsafe = (text, params = []) => {
    const raw = params.length ? params.map(pgParam) : null;
    const q = new SQLQuery(conn, () => ({ parts: [String(text)], params: [], types: [], rawParams: raw }));
    return raw ? q : q.simple();
  };
  sql.json = (value) => new SQLParam(JSON.stringify(value));
  sql.array = (value) => new SQLParam(pgArrayLiteral(value));
  if (txId === 0) {
    /** Runs `fn(tx)` in a transaction: COMMIT when it resolves, ROLLBACK when it throws. */
    sql.begin = async (options, fn) => {
      if (typeof options === "function") { fn = options; options = ""; }
      const txId = await ops.pgBegin(pool.id, "BEGIN " + (options || "")).catch(asPostgresError);
      const tx = makeSql(pool, txId);
      let result;
      try {
        result = await fn(tx);
      } catch (e) {
        await ops.pgFinish(txId, false).catch(() => {});
        throw e;
      }
      await ops.pgFinish(txId, true).catch(asPostgresError);
      return result;
    };
    sql.transaction = sql.begin;
    /** Closes the pool (idle connections now, busy ones when released). */
    sql.end = () => ops.pgClose(pool.id);
    sql.close = sql.end;
  }
  sql.PostgresError = PostgresError;
  return sql;
}

/** new Sako.SQL(url?, { max }) -> sql tagged-template client. */
function SQL(url, options) {
  if (url !== null && typeof url === "object") { options = url; url = options.url; }
  const pool = { id: ops.pgOpen(url === undefined ? undefined : String(url), options && options.max) };
  return makeSql(pool, 0);
}
SQL.PostgresError = PostgresError;

let defaultSql = null;
const getDefaultSql = () => (defaultSql ??= SQL());
