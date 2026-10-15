function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
let s = 0;
for (let i = 0; i < 30; i++) s += fib(27);
const arr = Array.from({ length: 1e6 }, (_, i) => i);
const m = new Map();
for (const x of arr) m.set("k" + (x % 50000), x);
const json = JSON.stringify(arr.slice(0, 200000).map((i) => ({ i, s: "v" + i })));
console.log(s, m.size, JSON.parse(json).length);
