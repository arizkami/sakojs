// SPDX-License-Identifier: BSD-3-Clause

interface Result {
  runtime: string;
  ready: boolean;
}

const result = { runtime: "typescript", ready: true } satisfies Result;
console.log(result.runtime, result.ready);
