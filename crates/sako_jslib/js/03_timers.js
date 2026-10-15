// Timers, immediates, nextTick and queueMicrotask.
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

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
