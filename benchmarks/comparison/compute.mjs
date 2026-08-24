// SPDX-License-Identifier: BSD-3-Clause

import process from "node:process";

const iterations = Number(process.argv[2]);
let value = 0x12345678;
const started = Date.now();
for (let index = 0; index < iterations; index += 1) {
  value = (Math.imul(value ^ index, 1664525) + 1013904223) | 0;
}
const elapsedMilliseconds = Math.max(1, Date.now() - started);
console.log(JSON.stringify({
  benchmark: "integer_compute",
  iterations,
  checksum: value >>> 0,
  elapsedMilliseconds,
}));
