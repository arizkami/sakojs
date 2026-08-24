// SPDX-License-Identifier: BSD-3-Clause

export type Numeric = number;

export function double(value: Numeric): number {
  return value * 2;
}

export const typedValue: Numeric = double(21);
