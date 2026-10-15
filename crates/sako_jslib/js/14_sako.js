// The Sako global and runtime hooks.
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

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
  $,
  SQL,
};
// Default client from DATABASE_URL / POSTGRES_URL / PG* variables, created
// on first use.
ObjectDefineProperty(Sako, "sql", { get: () => getDefaultSql(), enumerable: true });
define(g, "Sako", Object.freeze(Sako));

define(g, "console", console);
define(g, "global", g);
define(g, "self", g);

// Called by the module loader when the first TypeScript module loads: report
// `error.stack` positions in the TypeScript source. Frames are formatted by
// V8's own CallSite#toString, then .ts positions are mapped back.
function enableTsStacks() {
  if (Error.prepareStackTrace !== undefined) return; // user-installed
  Error.prepareStackTrace = (error, sites) => {
    let header;
    try { header = Error.prototype.toString.call(error); } catch { header = "Error"; }
    let stack = header;
    for (const site of sites) stack += "\n    at " + site;
    return tsStackRe.test(stack) ? ops.mapStack(stack) : stack;
  };
}

ops.setHooks(dispatchTimer, inspect, (p, ns) => p.then(() => ns), runTicks, startImmediates, runImmediate, httpDispatch, enableTsStacks);
