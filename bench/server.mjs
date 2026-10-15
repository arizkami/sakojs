// Local HTTP server for bench/fetch.js (run with node).
import http from "node:http";
const body = JSON.stringify({ hello: "world", items: Array.from({ length: 20 }, (_, i) => i) });
http.createServer((req, res) => {
  res.writeHead(200, { "content-type": "application/json", "content-length": Buffer.byteLength(body) });
  res.end(body);
}).listen(Number(process.argv[2] || 18734), "127.0.0.1");
