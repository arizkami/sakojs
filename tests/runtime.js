// SPDX-License-Identifier: BSD-3-Clause

console.log(process.platform, process.arch, process.argv[2]);
queueMicrotask(() => console.log("microtask"));

const cancelled = setTimeout(() => console.log("cancelled"), 0);
clearTimeout(cancelled);

let ticks = 0;
const interval = setInterval(() => {
  ticks += 1;
  console.log(`interval-${ticks}`);
  if (ticks === 2) clearInterval(interval);
}, 1);

setTimeout((value) => console.log(value), 0, "timeout");
