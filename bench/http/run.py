#!/usr/bin/env python3
"""HTTP server benchmark: Sako.serve vs Bun.serve vs Elysia (on Bun).

Usage: python3 bench/http/run.py [seconds] [connections]

Needs `wrk` (WRK env var or on PATH) and `bun install` in bench/http for
Elysia. Each runtime serves the same four endpoints (see sako.js, bun.js,
elysia.js); every endpoint gets a 2s warm-up, then a measured run.
"""
import os, re, shutil, subprocess, sys, tempfile, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
SECS = int(sys.argv[1]) if len(sys.argv) > 1 else 10
CONNS = int(sys.argv[2]) if len(sys.argv) > 2 else 100
WRK = os.environ.get("WRK") or shutil.which("wrk") or os.path.expanduser("~/tools/wrk/wrk")
PORT = 3100

SAKO = [os.path.join(ROOT, "target", "release", "sako"), os.path.join(HERE, "sako.js")]
# (name, command, extra env)
RUNTIMES = [
    ("sako", SAKO, {}),
    ("sako-1io", SAKO, {"SAKO_HTTP_THREADS": "1"}),
    ("bun", ["bun", os.path.join(HERE, "bun.js")], {}),
    ("elysia", ["bun", os.path.join(HERE, "elysia.js")], {}),
]
TICK = os.sysconf("SC_CLK_TCK")


def cpu_seconds(pid):
    """utime + stime of a process (all threads), in seconds."""
    with open(f"/proc/{pid}/stat") as f:
        fields = f.read().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / TICK

POST_LUA = """
wrk.method = "POST"
wrk.body = '{"hello":"world","n":[1,2,3]}'
wrk.headers["Content-Type"] = "application/json"
"""

ENDPOINTS = [
    ("text  GET /", "/", None),
    ("json  GET /json", "/json", None),
    ("param GET /id/:id?name=", "/id/42?name=sako", None),
    ("echo  POST /echo", "/echo", POST_LUA),
]


def wait_ready(url, timeout=10):
    end = time.time() + timeout
    while time.time() < end:
        try:
            urllib.request.urlopen(url, timeout=1).read()
            return
        except Exception:
            time.sleep(0.05)
    raise RuntimeError(f"server at {url} did not start")


def wrk(url, secs, lua):
    cmd = [WRK, "-t4", f"-c{CONNS}", f"-d{secs}s", "--latency"]
    script = None
    if lua:
        script = tempfile.NamedTemporaryFile("w", suffix=".lua", delete=False)
        script.write(lua)
        script.close()
        cmd += ["-s", script.name]
    out = subprocess.run(cmd + [url], capture_output=True, text=True).stdout
    if script:
        os.unlink(script.name)
    rps = float(re.search(r"Requests/sec:\s+([\d.]+)", out).group(1))
    p99 = re.search(r"99%\s+([\d.]+)(us|ms|s)", out)
    scale = {"us": 0.001, "ms": 1, "s": 1000}[p99.group(2)]
    errors = re.search(r"Non-2xx or 3xx responses: (\d+)|Socket errors: .*", out)
    return rps, float(p99.group(1)) * scale, errors.group(0) if errors else ""


results = {}
for name, cmd, extra_env in RUNTIMES:
    env = dict(os.environ, PORT=str(PORT), **extra_env)
    proc = subprocess.Popen(cmd, env=env, cwd=HERE, stdout=subprocess.DEVNULL)
    try:
        wait_ready(f"http://127.0.0.1:{PORT}/")
        for label, path, lua in ENDPOINTS:
            url = f"http://127.0.0.1:{PORT}{path}"
            wrk(url, 2, lua)  # warm-up (JIT, connection setup)
            c0 = cpu_seconds(proc.pid)
            rps, p99, err = wrk(url, SECS, lua)
            cores = (cpu_seconds(proc.pid) - c0) / SECS
            results[(name, label)] = (rps, p99, err, cores)
            print(f"  {name:9} {label:26} {rps:>10,.0f} req/s  p99 {p99:6.2f}ms  {cores:.2f} cores {err}", file=sys.stderr)
    finally:
        proc.terminate()
        proc.wait()
        time.sleep(0.5)

names = [n for n, _, _ in RUNTIMES]


def table(title, value, fmt):
    print(f"\n{title}\n")
    print(f"{'endpoint':28}" + "".join(f"{n:>30}" for n in names))
    for label, _, _ in ENDPOINTS:
        row = [results[(n, label)] for n in names]
        best = max(value(r) for r in row)
        print(f"{label:28}" + "".join(f"{('*' if value(r) == best else '') + fmt(r):>30}" for r in row))


print(f"\nwrk -t4 -c{CONNS} -d{SECS}s against each server on the same machine")
table("requests/sec (p99 latency, server CPU cores used), higher is better",
      lambda r: r[0], lambda r: f"{r[0]:,.0f} ({r[1]:.2f}ms, {r[3]:.1f}c)")
table("requests per CPU-second (efficiency), higher is better",
      lambda r: r[0] / max(r[3], 0.01), lambda r: f"{r[0] / max(r[3], 0.01):,.0f}")
