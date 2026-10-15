// Sako.serve with routes (same shape as bun.js).
const json = { message: "Hello, World!", items: [1, 2, 3] };
Sako.serve({
  port: Number(process.env.PORT || 3000),
  routes: {
    "/": new Response("Hello World!"),
    "/json": () => Response.json(json),
    "/id/:id": (req) => {
      const url = req.url;
      const q = url.indexOf("?");
      const name = q === -1 ? null : new URLSearchParams(url.slice(q + 1)).get("name");
      return new Response(`${req.params.id} ${name}`, { headers: { "x-powered-by": "benchmark" } });
    },
    "/echo": { POST: async (req) => Response.json(await req.json()) },
  },
  fetch: () => new Response("Not Found", { status: 404 }),
});
