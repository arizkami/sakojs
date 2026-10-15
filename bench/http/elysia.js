// Elysia on Bun.
import { Elysia } from "elysia";
const json = { message: "Hello, World!", items: [1, 2, 3] };
new Elysia()
  .get("/", "Hello World!")
  .get("/json", () => json)
  .get("/id/:id", ({ params, query, set }) => {
    set.headers["x-powered-by"] = "benchmark";
    return `${params.id} ${query.name}`;
  })
  .post("/echo", ({ body }) => body)
  .listen(Number(process.env.PORT || 3000));
