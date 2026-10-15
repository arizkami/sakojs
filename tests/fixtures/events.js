const t = new EventTarget();
const seen = [];
const h = (e) => seen.push(`h:${e.type}`);
t.addEventListener("ping", h);
t.addEventListener("ping", h); // duplicate ignored
t.addEventListener("ping", () => seen.push("once"), { once: true });
t.addEventListener("ping", { handleEvent: (e) => seen.push("object") });
t.dispatchEvent(new Event("ping"));
t.removeEventListener("ping", h);
t.dispatchEvent(new Event("ping"));
console.log(seen.join(","));
const ev = new Event("x", { cancelable: true });
t.addEventListener("x", (e) => e.preventDefault());
console.log("dispatch returned", t.dispatchEvent(ev), ev.defaultPrevented);
const ac = new AbortController();
ac.signal.addEventListener("abort", () => console.log("abort event, reason:", ac.signal.reason.name));
ac.abort();
console.log(ac.signal.aborted);
try { ac.signal.throwIfAborted(); } catch (e) { console.log("threw", e.name); }
console.log(AbortSignal.abort("why").reason, AbortSignal.any([new AbortController().signal, AbortSignal.abort(1)]).aborted);
// AbortSignal.timeout is unref'd (as in Node), so keep the loop alive meanwhile.
const keepAlive = setTimeout(() => {}, 1000);
const ts = AbortSignal.timeout(5);
await new Promise((r) => ts.addEventListener("abort", r));
clearTimeout(keepAlive);
console.log("timeout signal:", ts.reason.name);
