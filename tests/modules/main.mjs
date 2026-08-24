// SPDX-License-Identifier: BSD-3-Clause

import { answer } from "./value";
import message from "#message";
import conditional from "conditional-package";
import feature from "conditional-package/feature/value";
import legacyDefault, { legacy } from "./legacy.cjs";
import packageImport from "#condition";
import namedDefault, { counter, increment } from "./named.cjs";

increment();
if (counter !== 1 || namedDefault.counter !== 2) {
  throw new Error("CommonJS named export snapshot semantics are incorrect");
}

const dynamicModule = await import("./dynamic.mjs");
const dynamicPath = await import("node:path");
const dynamicCommonJs = await import("./legacy.cjs");
let missingRejected = false;
try {
  await import("./missing.mjs");
} catch (error) {
  missingRejected = error.message.includes("module not found");
}

queueMicrotask(() =>
  console.log(
    message,
    answer,
    conditional,
    feature,
    legacyDefault.legacy,
    legacy,
    process.argv[2],
    dynamicModule.dynamicValue,
    dynamicPath.default.basename("C:\\dynamic\\file.js"),
    dynamicCommonJs.legacy,
    missingRejected,
    packageImport,
  ),
);
