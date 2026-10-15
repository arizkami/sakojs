// process.
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

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
