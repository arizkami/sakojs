// 500 sequential + 2000 concurrent (in batches of 50) GETs to a local server.
const url = `http://127.0.0.1:${process.env.BENCH_PORT || 18734}/`;
let n = 0;
for (let i = 0; i < 500; i++) n += (await (await fetch(url)).json()).items.length;
for (let b = 0; b < 40; b++) {
  const rs = await Promise.all(Array.from({ length: 50 }, () => fetch(url).then((r) => r.json())));
  n += rs.length;
}
console.log(n);
