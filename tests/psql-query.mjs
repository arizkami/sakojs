// SPDX-License-Identifier: BSD-3-Clause

// sako:psql end to end: the library, the bridge, the FFI, and the Rust driver,
// against the fake server the Rust test alongside this one runs.

import { connect } from "sako:psql";
import assert from "node:assert";

const db = await connect(`postgres://sako@127.0.0.1:${process.argv[2]}/app`);

assert.strictEqual(db.parameter("server_version"), "16.2");
assert.strictEqual(db.parameter("nothing_like_this"), null);

const rows = await db.query`select * from things where id = ${7} and live = ${true}`;

assert.strictEqual(rows.length, 2);
assert.strictEqual(rows.command, "SELECT 2");
assert.strictEqual(rows.count, 2);
assert.strictEqual(
  rows.columns.map((column) => column.name).join(","),
  "id,name,live,score,payload,absent",
);
assert.strictEqual(rows.columns[0].type, 23);

// Each value comes back as what it is, not as the text the server printed.
const [first, second] = rows;
assert.strictEqual(first.id, 7);
assert.strictEqual(typeof first.id, "number");
assert.strictEqual(first.name, "a thing");
assert.strictEqual(first.live, true);
assert.strictEqual(first.score, 1.5);
assert.strictEqual(JSON.stringify(first.payload), '{"ok":true}');
// A NULL is null, and not the empty string.
assert.strictEqual(first.absent, null);

assert.strictEqual(second.live, false);
assert.strictEqual(second.name, "");

const one = await db.first`select 1`;
assert.strictEqual(one.id, 7);

await db.close();
assert.strictEqual(db.closed, true);
let refused = false;
try {
  await db.query`select 1`;
} catch (error) {
  refused = String(error.message).includes("closed");
}
assert.ok(refused, "a query after close should be refused");

console.log("psql-ok");
