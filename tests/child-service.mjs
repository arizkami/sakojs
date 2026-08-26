// SPDX-License-Identifier: BSD-3-Clause

// The shape esbuild's JavaScript API uses: one child started once, spoken to
// over stdin, answering on stdout, and still running between requests. The
// child here is Sako itself running child-service-worker.mjs, so the test
// needs nothing installed.

import { spawn } from "node:child_process";
import assert from "node:assert";
import path from "node:path";

const worker = path.join(import.meta.dirname, "child-service-worker.mjs");
const service = spawn(process.execPath, [worker], { stdio: ["pipe", "pipe", "inherit"] });

let buffered = "";
const waiting = [];
service.stdout.on("data", (chunk) => {
  buffered += chunk.toString();
  let newline = buffered.indexOf("\n");
  while (newline !== -1) {
    const line = buffered.slice(0, newline);
    buffered = buffered.slice(newline + 1);
    const pending = waiting.shift();
    if (pending !== undefined) pending(line);
    newline = buffered.indexOf("\n");
  }
});

const ask = (request) =>
  new Promise((resolve) => {
    waiting.push(resolve);
    service.stdin.write(`${request}\n`);
  });

// Three round trips against one child. A spawn that only delivers output
// once the child exits answers none of them.
assert.strictEqual(await ask("ping 1"), "pong 1");
assert.strictEqual(await ask("ping 2"), "pong 2");
assert.strictEqual(await ask("ping 3"), "pong 3");
assert.strictEqual(service.exitCode, null, "the service should still be running");

const closed = new Promise((resolve) => service.on("close", resolve));
service.stdin.end();
assert.strictEqual(await closed, 0);

console.log("child-service-ok");
