// SPDX-License-Identifier: BSD-3-Clause

import { answer } from "./value";
import message from "#message";
import conditional from "conditional-package";
import feature from "conditional-package/feature/value";
import legacyDefault, { legacy } from "./legacy.cjs";

queueMicrotask(() =>
  console.log(
    message,
    answer,
    conditional,
    feature,
    legacyDefault.legacy,
    legacy,
    process.argv[2],
  ),
);
