// SPDX-License-Identifier: BSD-3-Clause

const value = require("./value");
const data = require("./data.json");
const packageMain = require("fixture-package");
const assert = require("node:assert");
const processModule = require("node:process");
const consoleModule = require("node:console");
const timers = require("node:timers");

assert.strictEqual(value.answer, 42);
assert.ok(require.resolve("./value").endsWith("value.js"));
assert.strictEqual(processModule, process);
assert.strictEqual(consoleModule, console);
assert.strictEqual(timers.setTimeout, setTimeout);

console.log(value.answer, data.name, packageMain, __dirname.endsWith("commonjs"));
