const enc = new TextEncoder();
const bytes = enc.encode("héllo wörld ✓ 😀");
console.log(bytes.length, Array.from(bytes.slice(0, 6)));
console.log(new TextDecoder().decode(bytes));
console.log(new TextDecoder().decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x41])), new TextDecoder("utf-8", { ignoreBOM: true }).decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x41])).length);
console.log(JSON.stringify(new TextDecoder().decode(new Uint8Array([0xff, 0x41]))));
try { new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array([0xff])); } catch (e) { console.log(e.name); }
const into = new Uint8Array(4);
console.log(enc.encodeInto("a✓b", into), Array.from(into));
console.log(btoa("hello world"), atob("aGVsbG8gd29ybGQ="), btoa(""), atob("YQ"));
try { btoa("✓"); } catch (e) { console.log(e.name); }
const orig = { d: new Date(1000), m: new Map([[1, [2]]]), s: new Set([3]), u: new Uint8Array([4]), nested: { a: [1, { b: 2 }] } };
orig.self = orig;
const copy = structuredClone(orig);
console.log(copy !== orig, copy.self === copy, copy.d.getTime(), copy.m.get(1), copy.s.has(3), copy.u[0], copy.nested.a[1].b);
try { structuredClone(() => {}); } catch (e) { console.log(e.name); }
