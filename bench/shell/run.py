#!/usr/bin/env python3
"""Compare Sako.$ with Bun.$ on bench/shell/shell.js (microseconds per op).

Each runtime runs the script several times (alternating); the best time per
operation is reported to reduce noise from other load on the machine.

Usage: python3 bench/shell/run.py [rounds]
"""
import json, os, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
ROUNDS = int(sys.argv[1]) if len(sys.argv) > 1 else 3
script = os.path.join(HERE, "shell.js")
cmds = {"sako": [os.path.join(ROOT, "target", "release", "sako"), script], "bun": ["bun", script]}
best = {name: {} for name in cmds}
for _ in range(ROUNDS):
    for name, cmd in cmds.items():
        out = subprocess.run(cmd, capture_output=True, text=True, check=True).stdout.strip().splitlines()[-1]
        for label, us in json.loads(out)["results"]:
            best[name][label] = min(us, best[name].get(label, float("inf")))

print(f"best of {ROUNDS} runs, microseconds per operation (lower is better)\n")
print(f"{'operation':32}{'sako':>12}{'bun':>12}{'speedup':>10}")
for label, s in best["sako"].items():
    b = best["bun"][label]
    print(f"{label:32}{s:>10.1f}us{b:>10.1f}us{b / s:>9.1f}x")
