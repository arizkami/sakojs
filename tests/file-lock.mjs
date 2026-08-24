// SPDX-License-Identifier: BSD-3-Clause

import fs from "node:fs";

try {
  fs.readFileSync(process.argv[2]);
  console.log("unlocked");
} catch {
  console.log("locked");
}
