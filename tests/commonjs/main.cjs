// SPDX-License-Identifier: BSD-3-Clause

const value = require("./value");
const data = require("./data.json");
const packageMain = require("fixture-package");
const conditional = require("conditional-package");
const esm = require("./esm-value.mjs");
const assert = require("node:assert");
const processModule = require("node:process");
const consoleModule = require("node:console");
const timers = require("node:timers");
const packageImport = require("#condition");

assert.strictEqual(value.answer, 42);
assert.ok(require.resolve("./value").endsWith("value.js"));
assert.ok(require.resolve("conditional-package").endsWith("require.cjs"));
assert.strictEqual(processModule, process);
assert.strictEqual(consoleModule, console);
assert.strictEqual(timers.setTimeout, setTimeout);
assert.strictEqual(esm.esmAnswer, 84);
assert.strictEqual(esm.default, "esm-namespace");
assert.strictEqual(packageImport, "require-import-map");

console.log(
  value.answer,
  data.name,
  packageMain,
  conditional,
  __dirname.endsWith("commonjs"),
);
