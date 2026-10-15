// Shell benchmark: the same script runs on Bun.$ and Sako.$.
// Usage: sako bench/shell/shell.js | bun bench/shell/shell.js
const $ = globalThis.Bun ? Bun.$ : Sako.$;
const dir = `${process.env.TMPDIR || "/tmp"}/sako-shell-bench`;
await $`rm -rf ${dir} && mkdir -p ${dir}`.quiet();
$.cwd(dir);
await $`seq 1 20000 > nums.txt`.quiet();
for (let i = 0; i < 30; i++) await $`touch f${i}.txt`.quiet();

const results = [];
async function bench(label, n, fn) {
  for (let i = 0; i < Math.min(30, n); i++) await fn();
  const t = performance.now();
  for (let i = 0; i < n; i++) await fn();
  const us = ((performance.now() - t) / n) * 1000;
  results.push([label, us]);
}

await bench("echo hello .text()", 3000, () => $`echo hello`.text());
await bench("echo with interpolation", 3000, () => $`echo ${"a b"} ${["x", "y"]}`.quiet());
await bench("cd dir && pwd", 3000, () => $`cd . && pwd`.quiet());
await bench("&& / || chain", 3000, () => $`true && echo a || echo b`.quiet());
await bench("ls (30 files)", 1000, () => $`ls`.quiet());
await bench("cat 108 KB file .text()", 1000, () => $`cat nums.txt`.text());
await bench("echo > file && cat file", 1000, () => $`echo data > out.txt && cat out.txt`.quiet());
await bench("cat file | wc -l", 500, () => $`cat nums.txt | wc -l`.text());
await bench("grep 7 file | head -5", 500, () => $`grep 7 nums.txt | head -5`.text());
await bench("sort -rn | head -3", 300, () => $`sort -rn nums.txt | head -3`.text());
await bench("external /bin/true", 300, () => $`/bin/true`.quiet());
await bench("external /bin/echo .text()", 300, () => $`/bin/echo hi`.text());
await bench("echo | external /bin/cat", 300, () => $`echo hi | /bin/cat`.text());

const name = globalThis.Bun ? "bun" : "sako";
console.log(JSON.stringify({ name, results }));
await $`rm -rf ${dir}`.quiet();
