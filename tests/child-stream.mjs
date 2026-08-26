// SPDX-License-Identifier: BSD-3-Clause

// A child that answers over a pipe while it is still running, which is what
// esbuild's service protocol and every language server needs. Under a spawn
// that waits for the child to exit, the first assertion here never arrives.

import { spawn } from "node:child_process";
import assert from "node:assert";

const windows = process.platform === "win32";
const shell = windows ? "cmd.exe" : "/bin/sh";
const script = (windows) =>
  windows
    ? ["/d", "/c", "echo before& ping -n 3 127.0.0.1 >nul& echo after"]
    : ["-c", "echo before; sleep 2; echo after"];

const collect = (child) =>
  new Promise((resolve, reject) => {
    const out = [];
    const err = [];
    let early = false;
    child.stdout.on("data", (chunk) => {
      out.push(chunk.toString());
      // The child has not exited yet, so anything read here proves the output
      // arrived while it ran rather than after it ended.
      if (!early && out.join("").includes("before") && child.exitCode === null) early = true;
    });
    child.stderr.on("data", (chunk) => err.push(chunk.toString()));
    child.on("error", reject);
    child.on("close", (code) => resolve({ code, out: out.join(""), err: err.join(""), early }));
  });

const streamed = await collect(spawn(shell, script(windows)));
assert.strictEqual(streamed.code, 0);
assert.ok(streamed.out.includes("before"), `stdout: ${streamed.out}`);
assert.ok(streamed.out.includes("after"), `stdout: ${streamed.out}`);
assert.ok(streamed.early, "output should arrive before the child exits");

// Writing to a live child and reading what it makes of the input.
const sorter = spawn(windows ? "sort" : "sort", []);
const sorted = collect(sorter);
sorter.stdin.write("beta\n");
sorter.stdin.end("alpha\n");
const answer = await sorted;
assert.strictEqual(answer.code, 0);
assert.ok(
  answer.out.indexOf("alpha") < answer.out.indexOf("beta"),
  `stdout: ${answer.out}`,
);

// A child that is killed reports the signal rather than an exit code.
const sleeper = spawn(shell, windows ? ["/d", "/c", "ping -n 60 127.0.0.1 >nul"] : ["-c", "sleep 60"]);
const stopped = collect(sleeper);
sleeper.kill("SIGKILL");
const ended = await stopped;
assert.strictEqual(ended.code, null);
assert.strictEqual(sleeper.signalCode, "SIGKILL");

// A command that does not exist reports an error rather than throwing.
const missing = spawn("sako-no-such-command-exists", []);
const failure = await new Promise((resolve) => missing.on("error", resolve));
assert.ok(failure instanceof Error);

console.log("child-stream-ok");
