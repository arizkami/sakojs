// SPDX-License-Identifier: BSD-3-Clause

import fs from "node:fs";

const file = process.argv[2];
const iterations = Number(process.argv[3]);
let bytes = 0;
const started = Date.now();
for (let index = 0; index < iterations; index += 1) {
  bytes += fs.readFileSync(file).length;
}
const elapsedMilliseconds = Math.max(1, Date.now() - started);
console.log(JSON.stringify({ benchmark: "filesystem_hot_read", iterations, bytes, elapsedMilliseconds }));
