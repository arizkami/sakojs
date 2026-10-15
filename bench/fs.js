// Write, then read back 2000 small files with async fs APIs, in parallel.
const dir = `${process.env.TMPDIR || "/tmp"}/sako-bench-fs-${process.pid}`;
const isSako = typeof Sako !== "undefined";
const fs = isSako ? null : await import("node:fs/promises");
const write = isSako ? Sako.writeTextFile : fs.writeFile;
const read = isSako ? Sako.readTextFile : (p) => fs.readFile(p, "utf8");
await (isSako ? Sako.mkdir(dir, { recursive: true }) : fs.mkdir(dir, { recursive: true }));
const N = 2000;
await Promise.all(Array.from({ length: N }, (_, i) => write(`${dir}/f${i}.txt`, "x".repeat(1000) + i)));
const texts = await Promise.all(Array.from({ length: N }, (_, i) => read(`${dir}/f${i}.txt`)));
await (isSako ? Sako.remove(dir, { recursive: true }) : fs.rm(dir, { recursive: true }));
console.log(texts.length, texts[5].length);
