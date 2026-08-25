// SPDX-License-Identifier: BSD-3-Clause

// The HTTP client, pointed at a Sako server running in another process (see
// http-echo.mjs). Covers the two shapes callers use: a GET through http.get
// with the body arriving as events, and a POST built by hand carrying a
// request body and a header.

import assert from "node:assert";
import http from "node:http";

const port = Number(process.argv[2]);

const read = (response) => new Promise((resolve) => {
  let text = "";
  response.setEncoding("utf8");
  response.on("data", (chunk) => { text += chunk; });
  response.on("end", () => resolve(text));
});

const fetched = await new Promise((resolve, reject) => {
  http.get(`http://127.0.0.1:${port}/plain`, (response) => {
    assert.strictEqual(response.statusCode, 200);
    assert.strictEqual(response.headers["content-type"], "text/plain");
    read(response).then(resolve, reject);
  }).on("error", reject);
});
assert.strictEqual(fetched, "GET /plain ");

const posted = await new Promise((resolve, reject) => {
  const request = http.request(
    { hostname: "127.0.0.1", port, path: "/upload", method: "POST", headers: { "X-Send": "header" } },
    (response) => {
      assert.strictEqual(response.statusCode, 200);
      assert.strictEqual(response.headers["x-echo"], "header");
      read(response).then(resolve, reject);
    },
  );
  request.on("error", reject);
  request.write("payload");
  request.end();
});
assert.strictEqual(posted, "POST /upload payload");

console.log("node-client");
