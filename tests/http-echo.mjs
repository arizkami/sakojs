// SPDX-License-Identifier: BSD-3-Clause

// Echoes the request back, and closes once it has answered the number of
// requests it was asked for. A partner for node-client.mjs, which runs in a
// second process: Sako's HTTP client blocks the loop while a request is in
// flight, so it cannot call a server sharing that loop.

import http from "node:http";

let remaining = Number(process.argv[3] ?? 1);

const server = http.createServer((request, response) => {
  const chunks = [];
  request.on("data", (chunk) => chunks.push(chunk));
  request.on("end", () => {
    response.setHeader("Content-Type", "text/plain");
    response.setHeader("X-Echo", request.headers["x-send"] ?? "none");
    response.end(`${request.method} ${request.url} ${Buffer.concat(chunks).toString()}`);
    remaining -= 1;
    if (remaining <= 0) server.close();
  });
});

server.listen(Number(process.argv[2]), () => console.log("http-ready"));
