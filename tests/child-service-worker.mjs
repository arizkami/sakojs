// SPDX-License-Identifier: BSD-3-Clause

// The other half of child-service.mjs: answers each line it is given and
// keeps running until stdin closes.

let buffered = "";
process.stdin.on("data", (chunk) => {
  buffered += chunk.toString();
  let newline = buffered.indexOf("\n");
  while (newline !== -1) {
    const line = buffered.slice(0, newline).replace(/\r$/, "");
    buffered = buffered.slice(newline + 1);
    process.stdout.write(`${line.replace("ping", "pong")}\n`);
    newline = buffered.indexOf("\n");
  }
});
process.stdin.on("end", () => process.exit(0));
process.stdin.resume();
