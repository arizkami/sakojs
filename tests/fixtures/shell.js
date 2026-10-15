// Sako.$: parsing, expansion, builtins, external programs, pipes, redirects.
const $ = Sako.$;
const dir = Sako.env.SAKO_TEST_TMP + "/shell";
await $`mkdir -p ${dir}`;
$.cwd(dir);
const show = (label, v) => console.log(label.padEnd(18), JSON.stringify(v));
const run = async (p) => { const o = await p.nothrow().quiet(); return [o.exitCode, o.stdout.toString(), o.stderr.toString()]; };

// Builtins and syntax
show("echo", await $`echo hello   world`.text());
show("echo -n/-e", await $`echo -n a; echo -e "b\tc"`.text());
show("quotes", await $`echo 'single  $HOME' "double  ${"interp"}" esc\ aped`.text());
show("interp escape", await $`echo ${"a; rm -rf / && $(boom)"} ${["x y", "z"]} pre${"mid"}post`.text());
show("raw", await $`echo ${{ raw: "'raw  text'" }}`.text());
show("vars", await $`FOO=bar; echo $FOO "\${FOO}" ${"$FOO"}`.text());
show("$?", await $`false; echo $?; true; echo $?`.text());
show("&& ||", await $`false && echo no || echo yes; true || echo no && echo yes2`.text());
show("subst", await $`echo "[$(echo inner; echo two)]" $(echo a   b)`.text());
show("tilde", (await $`echo ~`.text()).trim() === Sako.env.HOME);
show("cd/pwd", (await $`mkdir -p sub/deep && cd sub/deep && pwd`.text()).trim().endsWith("/shell/sub/deep"));
show("cwd unchanged", (await $`pwd`.text()).trim().endsWith("/shell"));
show("exit", await run($`echo before; exit 3; echo after`));
show("seq/basename", await $`seq 2 4; basename /a/b/c.txt .txt; dirname /a/b/c.txt`.text());

// Files, redirects, globs
await $`echo one > f1.txt && echo two >> f1.txt && touch b.js A.txt`;
show("cat", await $`cat f1.txt`.text());
show("ls", await $`ls`.text());
show("glob", await $`echo *.txt`.text());
show("glob none", await $`echo *.nope`.text());
show("stdin redirect", await $`cat < f1.txt`.text());
show("stdin buffer", await $`cat < ${new TextEncoder().encode("from buffer\n")}`.text());
show("2> file", await run($`cat missing.txt 2> err.txt; cat err.txt`));
show("2>&1", await run($`cat missing.txt 2>&1`));
show("&> file", await run($`cat f1.txt missing.txt &> both.txt; cat both.txt`));
show("cp/mv/rm", await $`cp f1.txt c.txt && mv c.txt d.txt && rm f1.txt && ls *.txt`.text());

// Text tools (builtins), with GNU-compatible output
await $`seq 1 50 > nums.txt`;
show("wc", await $`wc -l nums.txt; cat nums.txt | wc -l; echo a b | wc`.text());
show("head/tail", await $`head -n 2 nums.txt; tail -3 nums.txt; tail -n +49 nums.txt`.text());
show("grep", await $`grep -c 1 nums.txt; grep -n '^4' nums.txt; grep -vE '[0-8]' nums.txt; grep -w 5 nums.txt`.text());
show("grep none", await run($`grep zzz nums.txt`));
show("sort/uniq", await $`echo -e "b\na\nb\nc\na" | sort | uniq -c`.text());
show("sort -rn", await $`sort -rn nums.txt | head -n 3`.text());

// External programs (real processes), pipes in both directions
show("external", await $`/bin/echo from process`.text());
show("ext pipe", await $`seq 1 5 | /usr/bin/tr 0-9 a-j | wc -l`.text());
show("ext stdin buf", await $`/bin/cat < ${new TextEncoder().encode("x".repeat(100000))}`.text().then((t) => t.length));
show("ext 2>&1", await run($`/bin/ls missing-file 2>&1`).then(([c, o]) => [c !== 0, o.includes("missing-file")]));
show("ext > file", await $`/bin/echo to file > ext.txt && cat ext.txt`.text());
show("prefix env", await $`SAKO_X=42 /usr/bin/env`.text().then((t) => t.split("\n").filter((l) => l.startsWith("SAKO_X="))));
show(".env()", await $`/usr/bin/env`.env({ ONLY: "1", PATH: Sako.env.PATH }).text().then((t) => t.split("\n").filter((l) => l && !l.startsWith("PATH=")).sort()));
show("not found", await run($`no-such-command arg`));
show("unsupported flag", (await $`grep -o 1 nums.txt`.text()).split("\n").length > 5);

// Promise API
try { await $`exit 7`.quiet(); } catch (e) { show("ShellError", [e.name, e.exitCode, e instanceof $.ShellError, e.message]); }
show("nothrow", (await $`exit 2`.nothrow().quiet()).exitCode);
show("json", await $`echo '{"a":[1,2]}'`.json());
const lines = [];
for await (const l of $`seq 3`.lines()) lines.push(l);
show("lines()", lines);
show("bytes", Array.from(await $`echo -n hi`.bytes()));
show("escape", [$.escape("plain"), $.escape("a b'c$")]);
try { await $`echo (oops`; } catch (e) { show("syntax error", e.message); }
console.log("--- printed by default:");
await $`echo printed; /bin/echo also printed`;
await $`rm -rf ${dir}`;
