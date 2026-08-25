// SPDX-License-Identifier: BSD-3-Clause
//
// Exercises process.stdin and node:readline without a terminal, driven by
// bytes on a pipe. The escape sequences below are what a terminal sends for
// the keys they name, so the decoder meets the same input a real console
// delivers -- raw mode itself needs a console and is covered separately.

import assert, { strictEqual } from "node:assert";
import readline from "node:readline";
import { EventEmitter } from "node:events";

// A stand-in for a terminal, so the decoder can be fed one keystroke at a time
// without waiting on a real one.
class FakeInput extends EventEmitter {}
const input = new FakeInput();
input.isTTY = true;
input.setRawMode = () => input;

const rl = readline.createInterface({
  input,
  tabSize: 2,
  prompt: "",
  escapeCodeTimeout: 50,
  terminal: true,
});

const keys = [];
input.on("keypress", (character, key) =>
  keys.push(`${JSON.stringify(character ?? null)}:${key.name}:${key.ctrl}:${key.shift}`),
);

// --- the line editor a prompt library reads back --------------------------
for (const character of "vue") input.emit("data", character);
strictEqual(rl.line, "vue");
strictEqual(rl.cursor, 3);

input.emit("data", "\x7f");
strictEqual(rl.line, "vu", "backspace edits the line buffer");

input.emit("data", "\x1b[D");
input.emit("data", "e");
strictEqual(rl.line, "veu", "the left arrow moves the insertion point");

rl.write(null, { ctrl: true, name: "u" });
strictEqual(rl.line, "u", "ctrl-u kills to the left of the cursor");
strictEqual(rl.cursor, 0);

rl.write("my-app");
strictEqual(rl.line, "my-appu", "write() feeds the buffer rather than the terminal");

let submitted = null;
rl.on("line", (line) => { submitted = line; });
input.emit("data", "\r");
strictEqual(submitted, "my-appu");
strictEqual(rl.line, "", "submitting clears the buffer");

// --- key decoding ---------------------------------------------------------
const decode = (sequence) => {
  keys.length = 0;
  input.emit("data", sequence);
  return keys.join(",");
};
strictEqual(decode("\x1b[A"), "null:up:false:false");
strictEqual(decode("\x1b[B"), "null:down:false:false");
strictEqual(decode("\x1b[3~"), "null:delete:false:false");
strictEqual(decode("\x1b[1;5C"), "null:right:true:false");
strictEqual(decode("\x1bOP"), "null:f1:false:false");
strictEqual(decode("\x03"), '"\\u0003":c:true:false');
strictEqual(decode("A"), '"A":a:false:true');
strictEqual(decode("\t"), '"\\t":tab:false:false');

// A sequence split across two reads is still one key.
keys.length = 0;
input.emit("data", "\x1b[");
strictEqual(keys.length, 0, "an incomplete sequence waits for the rest");
input.emit("data", "A");
strictEqual(keys.join(","), "null:up:false:false");

// --- real stdin, fed from a pipe ------------------------------------------
const chunks = [];
process.stdin.on("data", (chunk) => chunks.push(chunk.toString()));
process.stdin.on("end", () => {
  const piped = chunks.join("");
  assert.ok(
    piped.startsWith("piped-payload"),
    `stdin delivered ${JSON.stringify(piped)}`,
  );
  strictEqual(process.stdin.isTTY, false);
  strictEqual(process.stdin.isRaw, false, "a pipe cannot be put into raw mode");
  console.log("stdin-keys ok");
});
