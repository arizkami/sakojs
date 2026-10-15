#!/usr/bin/env sako
import mul, { add } from "./lib/math.js";
import data from "./lib/data.json" with { type: "json" };
import { greet } from "demo-pkg";
import { sub } from "demo-pkg/dist/sub.js";
console.log(add(2, 3), mul(2, 3), data.items.length, greet("tests"), sub);
console.log(import.meta.main, import.meta.url.startsWith("file://"), import.meta.url.endsWith("/modules.js"));
console.log(import.meta.filename.endsWith("modules.js"), import.meta.dirname === import.meta.filename.slice(0, -"/modules.js".length));
const lazy = await import("./lib/lazy.js");
console.log(lazy.loaded, lazy.meta);
const again = await import("./lib/lazy.js");
console.log("same namespace:", again === lazy);
try { await import("./lib/missing.js"); } catch (e) { console.log("missing:", e.message.includes("Cannot find module")); }
try { await import("no-such-pkg"); } catch (e) { console.log("bare missing:", e.message.includes("Cannot find package 'no-such-pkg'")); }
