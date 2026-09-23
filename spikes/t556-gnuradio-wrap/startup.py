#!/usr/bin/env python3
"""T-556 spike: spawn -> first byte / ready-line latency of the wrapped decoder, N samples."""
import json, os, subprocess, sys, time
n = int(sys.argv[1]); cmd = sys.argv[2:]
out = []
for _ in range(n):
    t0 = time.monotonic()
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    line = p.stdout.readline()
    t = time.monotonic() - t0
    assert json.loads(line)["type"] == "ready", line
    p.stdin.close(); p.wait()
    out.append(round(t, 3))
out_s = sorted(out)
print(json.dumps({"n": n, "load": os.getloadavg()[0], "ready_s": out,
                  "median": out_s[n // 2], "max": out_s[-1], "min": out_s[0]}))
