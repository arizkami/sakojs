// SPDX-License-Identifier: BSD-3-Clause

let value = 0;
for (let index = 0; index < 20_000_000; index += 1) value = (value + index) >>> 0;
if (value === -1) console.log(value);
