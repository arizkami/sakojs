// SPDX-License-Identifier: BSD-3-Clause

// The response only exists after a timer fires, so the request handler has
// long returned by the time there is anything to send. The connection has to
// be parked and answered later rather than replied to in place.

import http from "node:http";

const server = http.createServer((request, response) => {
  setTimeout(() => {
    response.setHeader("Content-Type", "text/plain");
    response.end(`late ${request.method} ${request.url}`);
    server.close();
  }, 25);
});

server.listen(Number(process.argv[2]), () => console.log("http-ready"));
