let n = 0;
for (let i = 0; i < 300000; i++) await Promise.resolve(i).then((v) => { n += v; });
let t = 0;
await new Promise((resolve) => {
  const tick = () => (++t < 20000 ? setImmediate(tick) : resolve());
  tick();
});
console.log(n, t);
