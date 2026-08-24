// SPDX-License-Identifier: BSD-3-Clause

import { answer } from "./value";
import message from "./nested/message.mjs";

queueMicrotask(() => console.log(message, answer, process.argv[2]));
