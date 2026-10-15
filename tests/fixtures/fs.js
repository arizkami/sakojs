const dir = Sako.env.SAKO_TEST_TMP + "/fs-test";
Sako.mkdirSync(dir + "/sub/deep", { recursive: true });
Sako.writeTextFileSync(dir + "/a.txt", "hello");
Sako.writeTextFileSync(dir + "/a.txt", " world", { append: true });
console.log(Sako.readTextFileSync(dir + "/a.txt"), Sako.existsSync(dir + "/a.txt"), Sako.existsSync(dir + "/nope"));
await Sako.writeFile(dir + "/b.bin", new Uint8Array([1, 2, 3]));
console.log(Array.from(await Sako.readFile(dir + "/b.bin")), await Sako.readTextFile(dir + "/a.txt"));
const st = await Sako.stat(dir + "/a.txt");
console.log(st.size, st.isFile, st.isDirectory, st.mtime instanceof Date);
console.log((await Sako.readDir(dir)).map((e) => `${e.name}:${e.isDirectory ? "d" : "f"}`).sort().join(" "));
// Many concurrent async reads go through the pool.
const reads = await Promise.all(Array.from({ length: 50 }, () => Sako.readTextFile(dir + "/a.txt")));
console.log(reads.length, reads.every((r) => r === "hello world"));
try { await Sako.readTextFile(dir + "/missing.txt"); } catch (e) { console.log(e.code, e.syscall, e.message.startsWith("ENOENT")); }
try { Sako.readDirSync(dir + "/a.txt"); } catch (e) { console.log(e.code); }
try { Sako.removeSync(dir); } catch (e) { console.log("non-recursive remove:", e.code); }
await Sako.remove(dir, { recursive: true });
console.log("removed:", !(await Sako.exists(dir)));
