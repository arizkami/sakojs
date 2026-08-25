// SPDX-License-Identifier: BSD-3-Clause

import assert, { strictEqual } from "node:assert";
import { Buffer } from "node:buffer";
import childProcess from "node:child_process";
import nodeCrypto from "node:crypto";
import dns from "node:dns";
import dnsPromises from "node:dns/promises";
import EventEmitter, { EventEmitter as NamedEventEmitter } from "node:events";
import fs, { readFileSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import path from "node:path";
import https from "node:https";
import net from "node:net";
import os from "node:os";
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
const readBytes = readFileSync(process.argv[2]);
assert.ok(Buffer.isBuffer(readBytes));
assert.ok(readBytes instanceof Uint8Array);
strictEqual(readBytes.byteLength, encoded.byteLength);
strictEqual(readBytes.subarray(0, 1).toString(), "h");
strictEqual(readFileSync(process.argv[2], "utf8"), "h\u00e9llo");
strictEqual(readFileSync(process.argv[2], { encoding: "utf8" }), "h\u00e9llo");
assert.ok(Buffer.isBuffer(readFileSync(process.argv[2], {})));
// Every read owns its own storage; writing one must not disturb the next.
readBytes[0] = 0x48;
strictEqual(readFileSync(process.argv[2]).subarray(0, 1).toString(), "h");
strictEqual((await readFile(process.argv[2], "utf8")), "h\u00e9llo");
assert.ok(fs.existsSync(process.argv[2]));
const descriptor = fs.openSync(process.argv[2], "r+");
const descriptorRead = Buffer.alloc(1);
strictEqual(fs.readSync(descriptor, descriptorRead, 0, 1, 0), 1);
strictEqual(descriptorRead.toString(), "h");
strictEqual(fs.writeSync(descriptor, Buffer.from("H"), 0, 1, 0), 1);
fs.closeSync(descriptor);
const promisedHandle = await fs.promises.open(process.argv[2], "r");
const promisedRead = Buffer.alloc(1);
strictEqual((await promisedHandle.read(promisedRead, 0, 1, 0)).bytesRead, 1);
strictEqual(promisedRead.toString(), "H");
await promisedHandle.close();
if (process.platform === "win32") {
  const extendedPath = `\\\\?\\${process.argv[2]}`;
  writeFileSync(extendedPath, "extended");
  strictEqual(readFileSync(extendedPath, "utf8"), "extended");
}
const watchEvent = await new Promise((resolve, reject) => {
  const watcher = fs.watch(process.argv[2], { interval: 25 }, (event) => {
    watcher.close();
    clearTimeout(timeout);
    resolve(event);
  });
  const timeout = setTimeout(() => { watcher.close(); reject(new Error("watch timed out")); }, 1000);
  setTimeout(() => writeFileSync(process.argv[2], "watched-content"), 30);
});
strictEqual(watchEvent, "change");
const directory = `${process.argv[2]}.dir`;
const nested = path.join(directory, "nested");
fs.mkdirSync(nested, { recursive: true });
const firstPath = path.join(nested, "first.txt");
const renamedPath = path.join(nested, "renamed.txt");
writeFileSync(firstPath, "data");
assert.ok(fs.statSync(firstPath).isFile());
assert.ok(fs.statSync(nested).isDirectory());
strictEqual(fs.statSync(firstPath).size, 4);
assert.ok(fs.readdirSync(nested).includes("first.txt"));
assert.ok(fs.readdirSync(nested, { withFileTypes: true })[0].isFile());
fs.renameSync(firstPath, renamedPath);
assert.ok(fs.realpathSync(renamedPath).endsWith("renamed.txt"));
const hardLinkPath = path.join(nested, "hard-link.txt");
const symbolicLinkPath = path.join(nested, "symbolic-link.txt");
fs.linkSync(renamedPath, hardLinkPath);
strictEqual(fs.readFileSync(hardLinkPath, "utf8"), "data");
fs.symlinkSync(renamedPath, symbolicLinkPath, "file");
assert.ok(fs.lstatSync(symbolicLinkPath).isSymbolicLink());
assert.ok(path.basename(fs.readlinkSync(symbolicLinkPath)).includes("renamed.txt"));
fs.unlinkSync(hardLinkPath);
fs.unlinkSync(symbolicLinkPath);
strictEqual((await fs.promises.stat(renamedPath)).size, 4);
const callbackEntries = await new Promise((resolve, reject) => {
  fs.readdir(nested, (error, entries) => error ? reject(error) : resolve(entries));
});
assert.ok(callbackEntries.includes("renamed.txt"));
fs.rmSync(directory, { recursive: true });
assert.ok(!fs.existsSync(directory));
strictEqual(path.win32.basename("C:\\work\\file.txt"), "file.txt");
strictEqual(path.extname("file.txt"), ".txt");
strictEqual(querystring.stringify({ value: "hello world" }), "value=hello+world");
strictEqual(querystring.parse("value=hello+world").value, "hello world");
const decoder = new StringDecoder("utf8");
const euro = Buffer.from("EUR: €");
strictEqual(decoder.write(euro.subarray(0, euro.length - 2)), "EUR: ");
strictEqual(decoder.end(euro.subarray(euro.length - 2)), "€");
strictEqual(os.platform(), process.platform);
strictEqual(os.arch(), "x64");
strictEqual(os.endianness(), "LE");
strictEqual(os.EOL, process.platform === "win32" ? "\r\n" : "\n");
const localhost = await dnsPromises.lookup("localhost", { family: 4 });
strictEqual(net.isIP(localhost.address), 4);
let unsupported = 0;
try { https.createServer(); } catch { unsupported += 1; }
strictEqual(unsupported, 1);
const [shellFile, shellFlags] = process.platform === "win32"
  ? ["cmd.exe", ["/d", "/c"]]
  : ["/bin/sh", ["-c"]];
const child = childProcess.spawnSync(shellFile, [...shellFlags, "echo child-output"], { encoding: "utf8" });
strictEqual(child.status, 0);
assert.ok(child.stdout.includes("child-output"));
const asyncChildOutput = await new Promise((resolve, reject) => {
  const spawned = childProcess.spawn(shellFile, [...shellFlags, "echo async-child"]);
  let output = "";
  spawned.stdout.on("data", (chunk) => { output += chunk.toString(); });
  spawned.once("error", reject);
  spawned.once("close", (code) => code === 0 ? resolve(output) : reject(new Error(`child status ${code}`)));
});
assert.ok(asyncChildOutput.includes("async-child"));
strictEqual(processModule, process);
assert.ok(path.isAbsolute(process.cwd()));
strictEqual(path.win32.normalize("\\\\server\\share\\folder\\..\\file.txt"), "\\\\server\\share\\file.txt");
strictEqual(path.win32.normalize("\\\\?\\UNC\\server\\share\\folder\\..\\file.txt"), "\\\\?\\UNC\\server\\share\\file.txt");
strictEqual(path.win32.normalize("\\\\?\\C:\\folder\\..\\file.txt"), "\\\\?\\C:\\file.txt");
strictEqual(path.posix.normalize("/a/b/folder/../file.txt"), "/a/b/file.txt");
assert.ok(typeof process.env === "object");
// process.env materializes on first read; it must then behave like the plain
// object it replaced.
assert.ok(Object.keys(process.env).length > 0);
process.env.SAKO_TEST_VARIABLE = "set";
strictEqual(process.env.SAKO_TEST_VARIABLE, "set");
assert.ok(Object.keys(process.env).includes("SAKO_TEST_VARIABLE"));
delete process.env.SAKO_TEST_VARIABLE;
strictEqual(process.env.SAKO_TEST_VARIABLE, undefined);
strictEqual(process.env, process.env);
assert.ok(process.execPath.endsWith(process.platform === "win32" ? "sako.exe" : "sako"));
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

// A relative reference against a file: base. `origin` is "null" for file:
// URLs, so resolving through it produced "null/..." and threw; import.meta.url
// is a file: URL, which is what tooling resolves its own assets against.
const moduleBase = "file:///W:/project/dist/bindings.js";
strictEqual(new NodeURL(".", moduleBase).pathname, "/W:/project/dist/");
strictEqual(new NodeURL("./binding.node", moduleBase).href, "file:///W:/project/dist/binding.node");
strictEqual(new NodeURL("../lib/main.js", moduleBase).href, "file:///W:/project/lib/main.js");
strictEqual(new NodeURL("file:///a/b").href, "file:///a/b");
strictEqual(new NodeURL("?q=1", "https://example.com/a/b").href, "https://example.com/a/b?q=1");

// SHA-256 is what TypeScript's build mode hashes every source file with, so
// tsc could not run at all while SHA-1 was the only algorithm on offer.
strictEqual(nodeCrypto.createHash("sha1").update("").digest("hex"),
  "da39a3ee5e6b4b0d3255bfef95601890afd80709");
strictEqual(nodeCrypto.createHash("sha256").update("").digest("hex"),
  "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
strictEqual(nodeCrypto.createHash("SHA-256").update("sako").digest("hex"),
  nodeCrypto.createHash("sha256").update("sako").digest("hex"));
strictEqual(nodeCrypto.createHash("sha512").update("").digest("hex").length, 128);
let unsupportedHash = "";
try { nodeCrypto.createHash("md5"); } catch (error) { unsupportedHash = error.message; }
assert.ok(unsupportedHash.includes("Unsupported hash algorithm"), unsupportedHash);

const controller = new AbortController();
let aborted = false;
controller.signal.addEventListener("abort", () => { aborted = true; });
controller.abort("reason");
assert.ok(aborted);

console.log("node-core", encoded.length, received);
