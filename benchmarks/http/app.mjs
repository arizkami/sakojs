// SPDX-License-Identifier: BSD-3-Clause

import http from "node:http";
http.createServer((_request, response) => response.end("Hello World")).listen(Number(process.argv[2]));
