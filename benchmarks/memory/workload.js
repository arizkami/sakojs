// SPDX-License-Identifier: BSD-3-Clause

const values = Array.from({ length: 100_000 }, (_, index) => ({ index, text: "sako-memory" }));
queueMicrotask(() => values.length);
