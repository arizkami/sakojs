// SPDX-License-Identifier: BSD-3-Clause

import http from "node:http";
import process from "node:process";

const port = Number(process.argv[2]);
http.createServer((_request, response) => {
  response.setHeader("Content-Type", "text/plain");
  response.end("Hello World");
}).listen(port, "127.0.0.1");
