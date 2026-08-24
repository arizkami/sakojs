// SPDX-License-Identifier: BSD-3-Clause

import http from "node:http";

const server = http.createServer((request, response) => {
  const chunks = [];
  request.on("data", (chunk) => chunks.push(chunk));
  request.on("end", () => {
    response.setHeader("Content-Type", "text/plain");
    response.end(`${request.method} ${request.url} ${Buffer.concat(chunks).toString()}`);
    server.close();
  });
});

server.listen(Number(process.argv[2]), () => console.log("http-ready"));
