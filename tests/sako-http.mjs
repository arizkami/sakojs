// SPDX-License-Identifier: BSD-3-Clause

// The sako:http library, driven from a second process. Sako's HTTP client
// blocks the loop while a request is in flight, so a server cannot call
// itself; the partner here is the Rust test that spawns this fixture.
//
// Closes once it has answered the number of requests it was asked for.

import { serve } from "sako:http";

let remaining = Number(process.argv[3] ?? 1);

const answered = () => {
  remaining -= 1;
  // The response is written once the handler's promise settles, so the close
  // waits a turn: closing from inside the handler would drop the answer it
  // was about to send.
  if (remaining <= 0) setTimeout(() => server.close(), 0);
};

const server = serve({
  port: Number(process.argv[2]),
  fetch: async (request, connection) => {
    const url = new URL(request.url);
    try {
      if (url.pathname === "/json") {
        const payload = await request.json();
        return new Response(JSON.stringify({ seen: payload.name }), {
          status: 201,
          headers: { "content-type": "application/json" },
        });
      }
      if (url.pathname === "/boom") throw new Error("the handler exploded");
      return new Response(`${request.method} ${url.pathname} ${await request.text()}`, {
        headers: { "content-type": "text/plain", "x-remote": connection.remoteAddress },
      });
    } finally {
      answered();
    }
  },
  onError: () => new Response("handled", { status: 503 }),
  onListen: ({ port, url }) => {
    if (url !== `http://127.0.0.1:${port}`) throw new Error(`unexpected address: ${url}`);
    console.log("http-ready");
  },
});
