// SPDX-License-Identifier: BSD-3-Clause

import express from "express";

const app = express();
app.get("/", (_request, response) => {
  response.send("Hello World");
});
app.listen(3000);
