// Sako.$ shell API.
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

// -------------------------------------------------------------------- shell
// Sako.$: a tagged-template shell like Bun.$. Rust parses and runs the
// script without /bin/sh; scripts made only of builtins finish
// synchronously on this thread, anything that spawns runs on the pool.
const shellDefaults = { cwd: undefined, env: undefined, throws: true };

function splitLines(text) {
  if (text === "") return [];
  const parts = text.split("\n");
  if (parts[parts.length - 1] === "") parts.pop();
  return parts;
}

// Like Bun's Buffer output: bytes whose toString() decodes UTF-8.
class ShellBytes extends Uint8Array {
  toString() { return ops.decode(this, false, false); }
  static from(u8) { return u8 === undefined ? EMPTY_BYTES : new ShellBytes(u8.buffer, u8.byteOffset, u8.byteLength); }
}
const EMPTY_BYTES = new ShellBytes(0);

// Output arrives as a string (UTF-8 text), a Uint8Array (other bytes) or
// undefined (empty); `stdout`/`stderr` bytes are built only when read.
const asText = (v) => (v === undefined ? "" : typeof v === "string" ? v : ops.decode(v, false, false));
const asBytes = (v) => (v === undefined ? EMPTY_BYTES : ShellBytes.from(typeof v === "string" ? ops.encode(v) : v));

class ShellOutput {
  #out; #err; #outBytes = null; #errBytes = null;
  constructor(exitCode, out, err) {
    this.exitCode = exitCode;
    this.#out = out;
    this.#err = err;
  }
  get stdout() { return (this.#outBytes ??= asBytes(this.#out)); }
  get stderr() { return (this.#errBytes ??= asBytes(this.#err)); }
  text() { return asText(this.#out); }
  json() { return JSON.parse(asText(this.#out)); }
  lines() { return splitLines(asText(this.#out)); }
  bytes() { return this.stdout; }
  arrayBuffer() {
    const b = this.stdout;
    return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
  }
  [kCustomInspect](depth, ctx) {
    return "ShellOutput " + inspect({ exitCode: this.exitCode, stdout: this.stdout, stderr: this.stderr }, ctx);
  }
}

class ShellError extends Error {
  #output;
  constructor(out) {
    super(`Failed with exit code ${out.exitCode}`);
    this.name = "ShellError";
    this.exitCode = out.exitCode;
    this.#output = out;
    // Own enumerable properties, like Bun's ShellError (built lazily).
    ObjectDefineProperty(this, "stdout", { get: () => out.stdout, enumerable: true, configurable: true });
    ObjectDefineProperty(this, "stderr", { get: () => out.stderr, enumerable: true, configurable: true });
  }
  text() { return this.#output.text(); }
  json() { return this.#output.json(); }
  bytes() { return this.#output.stdout; }
}

function settleShell(r, throws) {
  const out = new ShellOutput(r[0], r[1], r[2]);
  if (out.exitCode !== 0 && throws) throw new ShellError(out);
  return out;
}


class ShellPromise {
  #texts; #values; #cwd; #env; #throws;
  #quiet = false;
  #promise = null;
  constructor(texts, values) {
    this.#texts = texts;
    this.#values = values;
    this.#cwd = shellDefaults.cwd;
    this.#env = shellDefaults.env;
    this.#throws = shellDefaults.throws;
    // Runs even if never awaited (after modifiers chained in this tick).
    resolved.then(() => { if (this.#promise === null) this.#run(); });
  }
  #run() {
    if (this.#promise !== null) return this.#promise;
    let r;
    try {
      r = ops.shell(this.#texts, this.#values, this.#cwd, this.#env, this.#quiet);
    } catch (e) {
      return (this.#promise = Promise.reject(e));
    }
    const throws = this.#throws;
    if (ArrayIsArray(r)) {
      // Ran inline: settle without another round trip.
      try {
        this.#promise = Promise.resolve(settleShell(r, throws));
      } catch (e) {
        this.#promise = Promise.reject(e);
      }
    } else {
      this.#promise = r.then((res) => settleShell(res, throws));
    }
    return this.#promise;
  }
  then(onFulfilled, onRejected) { return this.#run().then(onFulfilled, onRejected); }
  catch(onRejected) { return this.#run().catch(onRejected); }
  finally(onFinally) { return this.#run().finally(onFinally); }
  quiet() { this.#quiet = true; return this; }
  nothrow() { this.#throws = false; return this; }
  throws(v = true) { this.#throws = !!v; return this; }
  cwd(dir) { this.#cwd = dir === undefined ? undefined : String(dir); return this; }
  env(vars) { this.#env = flattenEnv(vars); return this; }
  text() { this.#quiet = true; return this.#run().then((o) => o.text()); }
  json() { this.#quiet = true; return this.#run().then((o) => o.json()); }
  bytes() { this.#quiet = true; return this.#run().then((o) => o.bytes()); }
  arrayBuffer() { this.#quiet = true; return this.#run().then((o) => o.arrayBuffer()); }
  lines() {
    this.#quiet = true;
    const p = this.#run();
    return (async function* () { yield* (await p).lines(); })();
  }
  get [Symbol.toStringTag]() { return "ShellPromise"; }
}

function flattenEnv(vars) {
  if (vars === undefined) return undefined;
  const flat = [];
  for (const k of ObjectKeys(vars)) if (vars[k] !== undefined) flat.push(k, String(vars[k]));
  return flat;
}

function shellValue(v) {
  if (typeof v === "string") return v;
  if (ArrayIsArray(v)) return v.flat(Infinity).map((x) => (typeof x === "string" ? x : String(x)));
  if (v instanceof ArrayBuffer || ArrayBuffer.isView(v)) return v;
  if (v instanceof ShellOutput) return v.text();
  if (v instanceof Response || v instanceof Request) {
    throw new TypeError("Sako.$: interpolate `await response.bytes()` instead of a Response");
  }
  return String(v);
}

function $(strings, ...values) {
  if (!ArrayIsArray(strings) || !ArrayIsArray(strings.raw)) {
    throw new TypeError("Sako.$ must be used as a tagged template: $`command`");
  }
  // Raw strings: backslashes are passed to the shell as written.
  const raw = strings.raw;
  const texts = [raw[0]];
  const vals = [];
  let cacheable = true;
  for (let i = 0; i < values.length; i++) {
    const v = values[i];
    if (v !== null && typeof v === "object" && !ArrayIsArray(v) && typeof v.raw === "string" &&
        !(v instanceof ArrayBuffer) && !ArrayBuffer.isView(v)) {
      // { raw: "..." } is inserted as shell syntax, unescaped.
      texts[texts.length - 1] += v.raw + raw[i + 1];
      cacheable = false;
      continue;
    }
    vals.push(shellValue(v));
    texts.push(raw[i + 1]);
  }
  // A call site always passes the same `strings` array: parse it once.
  let script = texts;
  if (cacheable) {
    script = scriptCache.get(strings);
    if (script === undefined) {
      try {
        script = ops.shellParse(texts, vals.length);
        scriptCache.set(strings, script);
      } catch {
        script = texts; // syntax error: reported when the command runs
      }
    }
  }
  return new ShellPromise(script, vals);
}
const scriptCache = new WeakMap();
$.cwd = (dir) => { shellDefaults.cwd = dir === undefined ? undefined : String(dir); return $; };
$.env = (vars) => { shellDefaults.env = flattenEnv(vars); return $; };
$.nothrow = () => { shellDefaults.throws = false; return $; };
$.throws = (v = true) => { shellDefaults.throws = !!v; return $; };
$.escape = (s) => ops.shellEscape(String(s));
$.ShellError = ShellError;
$.ShellPromise = ShellPromise;
