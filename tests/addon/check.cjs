// SPDX-License-Identifier: BSD-3-Clause

const assert = require('node:assert');
const addon = require('./addon.node');

assert.strictEqual(addon.add(19, 23), 42);
assert.strictEqual(addon.shout('sako runs addons'), 'SAKO RUNS ADDONS');

const object = addon.makeObject();
assert.strictEqual(object.name, 'sako');
assert.strictEqual(JSON.stringify(object.squares), '[0,1,4]');

const buffer = addon.makeBuffer();
assert.ok(Buffer.isBuffer(buffer), 'created buffer should be a Buffer');
assert.strictEqual(buffer.toString(), 'sako');

const external = addon.makeExternalBuffer();
assert.ok(Buffer.isBuffer(external), 'external buffer should be a Buffer');
assert.strictEqual(external.toString(), 'bytes');

let message = '';
let code = '';
try {
  addon.throws();
} catch (error) {
  message = error.message;
  code = error.code;
}
assert.strictEqual(message, 'addon said no');
assert.strictEqual(code, 'ERR_SAKO');

const counter = new addon.Counter(10);
assert.strictEqual(counter.bump(), 11);
assert.strictEqual(counter.bump(), 12);

const version = addon.version();
assert.strictEqual(typeof version.napi, 'number');
assert.strictEqual(version.release, 'sako');

const tickets = [];
addon.countFromThread((value) => { tickets.push(value); });

addon.doubleLater(21).then((value) => {
  assert.strictEqual(value, 42);
  console.log('async work ok');
});

setTimeout(() => {
  assert.strictEqual(JSON.stringify(tickets), '[1,2,3]');
  console.log('threadsafe ok', tickets.join(','));
  console.log('napi ok');
}, 400);
