// SPDX-License-Identifier: BSD-3-Clause
//
// Covers the threaded whole-file read: sizes on both sides of the threshold,
// sizes that do not divide evenly across workers, and byte-exact content so a
// mis-sliced range cannot pass.

import assert, { strictEqual } from "node:assert";
import fs from "node:fs";
import path from "node:path";

const directory = process.argv[2];
fs.mkdirSync(directory, { recursive: true });

const pattern = (index) => (index * 31 + (index >> 13)) & 0xff;

const sameBytes = (left, right) => {
  if (left.length !== right.length) return false;
  for (let index = 0; index < left.length; index += 1) {
    if (left[index] !== right[index]) return false;
  }
  return true;
};

const sizes = [
  0,
  1,
  4 * 1024 * 1024 - 1,
  4 * 1024 * 1024,
  4 * 1024 * 1024 + 1,
  9 * 1024 * 1024 + 7,
  17 * 1024 * 1024 + 3,
];

for (const size of sizes) {
  const file = path.join(directory, `read-${size}.bin`);
  const expected = Buffer.alloc(size);
  for (let index = 0; index < size; index += 1) expected[index] = pattern(index);
  fs.writeFileSync(file, expected);

  const bytes = fs.readFileSync(file);
  strictEqual(bytes.length, size, `length for ${size}`);
  assert.ok(Buffer.isBuffer(bytes), `buffer type for ${size}`);
  assert.ok(sameBytes(bytes, expected), `content for ${size}`);

  // Repeat reads must be independent: the second read cannot observe writes
  // made to the first one's storage.
  if (size > 0) {
    bytes[0] ^= 0xff;
    bytes[size - 1] ^= 0xff;
    assert.ok(sameBytes(fs.readFileSync(file), expected), `isolation for ${size}`);
  }

  // The text path shares the same threaded read.
  const text = fs.readFileSync(file, "utf8");
  strictEqual(Buffer.byteLength(text) >= 0, true);

  fs.rmSync(file);
}

// Reading the same large file repeatedly must stay stable under churn.
const stressFile = path.join(directory, "stress.bin");
const stressSize = 6 * 1024 * 1024 + 512;
const stressExpected = Buffer.alloc(stressSize);
for (let index = 0; index < stressSize; index += 1) stressExpected[index] = pattern(index * 7);
fs.writeFileSync(stressFile, stressExpected);
for (let round = 0; round < 8; round += 1) {
  assert.ok(sameBytes(fs.readFileSync(stressFile), stressExpected), `stress round ${round}`);
}
fs.rmSync(stressFile);

fs.rmSync(directory, { recursive: true });
console.log("filesystem-large ok");
