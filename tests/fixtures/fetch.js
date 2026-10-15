const B = Sako.env.SAKO_TEST_SERVER;
const r = await fetch(B + "/json");
console.log(r.status, r.ok, r.headers.get("content-type"), await r.json());
const r2 = await fetch(B + "/redirect");
console.log(r2.status, r2.redirected, r2.url === B + "/json");
const r3 = await fetch(B + "/missing");
console.log(r3.status, r3.ok, r3.statusText, await r3.text());
const r4 = await fetch(new URL("/echo", B), { method: "POST", body: JSON.stringify({ a: 1 }), headers: { "x-test": "yes" } });
console.log(await r4.text());
const r5 = await fetch(B + "/echo", { method: "PUT", body: new URLSearchParams({ q: "a b" }) });
console.log(await r5.text());
const r6 = await fetch(B + "/json", { method: "HEAD" });
console.log("head:", r6.status, r6.body);
const all = await Promise.all(Array.from({ length: 20 }, () => fetch(B + "/json").then((r) => r.json())));
console.log("parallel:", all.length, all.every((j) => j.ok === true));
try { await fetch("http://127.0.0.1:1/"); } catch (e) { console.log("refused:", e.name); }
try { await fetch("data:text/plain,x"); } catch (e) { console.log(e.message); }
try { await fetch(B + "/json", { signal: AbortSignal.abort() }); } catch (e) { console.log("aborted:", e.name); }
try { new Request(B, { method: "GET", body: "x" }); } catch (e) { console.log(e.name); }
const res = new Response("body", { status: 201, headers: { "X-A": "1" } });
console.log(res.status, res.headers.get("x-a"), res.headers.get("content-type"), await res.clone().text(), await res.text(), res.bodyUsed);
console.log(Response.json({ k: 1 }).headers.get("content-type"), Response.redirect("http://x.y/", 301).headers.get("location"));
const h = new Headers([["B", "2"], ["a", "1"]]); h.append("a", "3");
console.log([...h], h.has("A"));
