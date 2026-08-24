// SPDX-License-Identifier: BSD-3-Clause

import assert, { strictEqual } from "node:assert";
import { Buffer } from "node:buffer";
import EventEmitter, { EventEmitter as NamedEventEmitter } from "node:events";
import fs, { readFileSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import path from "node:path";
import processModule from "node:process";
import querystring from "node:querystring";
import { StringDecoder } from "node:string_decoder";
import timers from "node:timers";
import { URL as NodeURL, URLSearchParams as NodeURLSearchParams } from "node:url";
import util from "node:util";

const encoded = Buffer.from("h\u00e9llo");
strictEqual(encoded.toString(), "h\u00e9llo");
assert.ok(Buffer.isBuffer(encoded));
strictEqual(EventEmitter, NamedEventEmitter);

const events = new EventEmitter();
let received = 0;
events.once("value", (value) => { received = value; });
events.emit("value", 42);
events.emit("value", 0);
strictEqual(received, 42);

writeFileSync(process.argv[2], encoded);
strictEqual(readFileSync(process.argv[2]).toString(), "h\u00e9llo");
strictEqual((await readFile(process.argv[2], "utf8")), "h\u00e9llo");
assert.ok(fs.existsSync(process.argv[2]));
strictEqual(path.basename("C:\\work\\file.txt"), "file.txt");
strictEqual(path.extname("file.txt"), ".txt");
strictEqual(querystring.stringify({ value: "hello world" }), "value=hello+world");
strictEqual(querystring.parse("value=hello+world").value, "hello world");
strictEqual(processModule, process);
strictEqual(timers.setTimeout, setTimeout);
assert.ok(typeof util.promisify === "function");
strictEqual(new TextDecoder().decode(new TextEncoder().encode("text")), "text");
strictEqual(new StringDecoder().write(Buffer.from("decode")), "decode");

const url = new NodeURL("https://example.com:8443/a?value=one#hash");
strictEqual(url.hostname, "example.com");
strictEqual(url.port, "8443");
strictEqual(url.searchParams.get("value"), "one");
url.searchParams.set("value", "two words");
strictEqual(url.href, "https://example.com:8443/a?value=two+words#hash");
strictEqual(new NodeURLSearchParams("a=1&a=2").getAll("a").length, 2);
strictEqual(NodeURL, URL);

const controller = new AbortController();
let aborted = false;
controller.signal.addEventListener("abort", () => { aborted = true; });
controller.abort("reason");
assert.ok(aborted);

console.log("node-core", encoded.length, received);
