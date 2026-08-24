// SPDX-License-Identifier: BSD-3-Clause

import { typedValue } from "./value";
import type { Numeric } from "./value";
import legacy from "./legacy.cts";

interface User {
  name: string;
}

enum Mode {
  Fast = "fast",
}

namespace Config {
  export const answer: number = 42;
}

const user = { name: "Sako" } satisfies User;
const numeric: Numeric = typedValue;
const dynamic = await import("./dynamic.ts");
globalThis.React = {
  createElement(type: string, properties: unknown, ...children: unknown[]) {
    return { type, properties, children };
  },
};
const view = await import("./view.tsx");

console.log(
  "typescript",
  user.name,
  Mode.Fast,
  Config.answer,
  numeric,
  legacy.answer,
  dynamic.default,
  view.default.type,
);
