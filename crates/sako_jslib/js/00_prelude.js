// Shared primordials and helpers used by every other file.
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

"use strict";

const {
  ObjectDefineProperty,
  ObjectGetOwnPropertyNames,
  ObjectGetOwnPropertySymbols,
  ObjectGetPrototypeOf,
  ObjectPrototypeToString,
  ObjectKeys,
  ArrayIsArray,
  ErrorCaptureStackTrace,
} = {
  ObjectDefineProperty: Object.defineProperty,
  ObjectGetOwnPropertyNames: Object.getOwnPropertyNames,
  ObjectGetOwnPropertySymbols: Object.getOwnPropertySymbols,
  ObjectGetPrototypeOf: Object.getPrototypeOf,
  ObjectPrototypeToString: Function.prototype.call.bind(Object.prototype.toString),
  ObjectKeys: Object.keys,
  ArrayIsArray: Array.isArray,
  ErrorCaptureStackTrace: Error.captureStackTrace,
};

const g = globalThis;
const define = (obj, name, value, enumerable = false) =>
  ObjectDefineProperty(obj, name, { value, writable: true, configurable: true, enumerable });
