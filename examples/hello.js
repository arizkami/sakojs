import { add, VERSION } from "./lib/math.js";
import data from "./lib/data.json" with { type: "json" };

console.log("hello from sako", Sako.version, "on V8", process.versions.v8);
console.log("add(2, 3) =", add(2, 3), VERSION);
console.log({ a: 1, b: "two", c: [1, 2, 3], d: { e: { f: { g: 1 } } }, fn() {}, m: new Map([[1, { x: 1 }]]), s: new Set([1, 2]) });
console.log("%s is %d years old, %o", "Bob", 42, { x: [1, 2] });
console.log(data, import.meta.url, import.meta.main);
console.log([1, 2, 3].map((x) => x * 2), new Uint8Array([1, 2, 3]), 10n, -0, null, undefined, Symbol("s"));
console.log(new Error("boom").message, class Foo extends Map {}, async function bar() {});
console.log(Promise.resolve(42), new Promise(() => {}), Promise.reject(new Error("x")).catch(() => {}) && "caught");

const t0 = performance.now();
await new Promise((r) => setTimeout(r, 20));
console.log("slept ~20ms:", (performance.now() - t0).toFixed(1));

let n = 0;
const iv = setInterval(() => { if (++n === 3) { clearInterval(iv); console.log("interval done", n); } }, 5);
const cancelled = setTimeout(() => console.log("SHOULD NOT PRINT"), 10_000);
clearTimeout(cancelled);
queueMicrotask(() => console.log("microtask"));
process.nextTick(() => console.log("nextTick"));

const { default: dyn } = await import("./lib/dyn.js");
console.log("dynamic import:", dyn());

const enc = new TextEncoder().encode("héllo ✓");
console.log(enc, new TextDecoder().decode(enc), btoa("hello"), atob("aGVsbG8="));
Sako.writeTextFileSync("/tmp/sako-test.txt", "file contents\n");
console.log(JSON.stringify(Sako.readTextFileSync("/tmp/sako-test.txt")), Sako.statSync("/tmp/sako-test.txt").size);
try { Sako.readTextFileSync("/nope"); } catch (e) { console.log(e.code, e.message); }
console.log(structuredClone({ a: [1, { b: 2 }], d: new Date(0) }));
console.table([{ a: 1, b: "x" }, { a: 2, b: "y" }]);
