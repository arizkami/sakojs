#!/usr/bin/env python3
"""Compare wall-clock time of sako vs node vs bun on the bench/*.js files.

Usage: python3 bench/run.py [runs]
"""
import os, shutil, statistics, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RUNS = int(sys.argv[1]) if len(sys.argv) > 1 else 10
SAKO = os.path.join(ROOT, "target", "release", "sako")

runtimes = [("sako", [SAKO]), ("sako-nc", [SAKO, "--no-cache"])]
for name, cmd in (("node", ["node"]), ("bun", ["bun"]), ("deno", ["deno", "run", "-A"])):
    if shutil.which(cmd[0]):
        runtimes.append((name, cmd))

# big.js imports a generated ~2.5 MB module to exercise parsing / code cache.
bigmod = os.path.join(ROOT, "bench", "bigmod.js")
if not os.path.exists(bigmod):
    with open(bigmod, "w") as f:
        for i in range(20000):
            f.write(f"export function f{i}(a, b) {{ const x = a * {i} + b; if (x % 7 === 0) return [x, 'str{i}', {{ k: x }}]; return x - {i}; }}\n")
        f.write("export const total = f1(1,2) + f19999(3,4);\n")

benches = ["empty.js", "big.js", "compute.js", "console.js", "async.js", "fs.js", "fetch.js"]


def measure(cmd):
    times = []
    for i in range(RUNS + 1):
        t = time.perf_counter()
        subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
        if i:  # first run is a warm-up (fills OS page cache / code caches)
            times.append((time.perf_counter() - t) * 1000)
    return statistics.median(times)


server = None
if shutil.which("node"):
    server = subprocess.Popen(["node", os.path.join(ROOT, "bench", "server.mjs"), "18734"])
    time.sleep(0.5)
else:
    benches.remove("fetch.js")

print(f"median of {RUNS} runs (ms), lower is better\n")
print(f"{'bench':<12}" + "".join(f"{n:>10}" for n, _ in runtimes))
for b in benches:
    path = os.path.join(ROOT, "bench", b)
    row = [measure(cmd + [path]) for _, cmd in runtimes]
    best = min(row)
    print(f"{b:<12}" + "".join(f"{('*' if r == best else '') + f'{r:.1f}':>10}" for r in row))

if server:
    server.terminate()
