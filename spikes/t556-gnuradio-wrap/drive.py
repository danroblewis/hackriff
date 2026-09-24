#!/usr/bin/env python3
"""T-556 spike: host-side driver for the wrapped decoder (stands in for hk_plugins::PluginInstance).

Frames a cf32 fixture as hackriff-v1 (docs/stream-contract.md §3-§5.2) onto the plugin's stdin,
optionally paced at real time and with whole records dropped (§5.3 markers, like a full
queue), reads §9.3 NDJSON from its stdout, samples RSS/CPU with ps, and scores decodes BLIND
against the fixture's hidden truth list (payload + sample_index within a tolerance).

Usage: drive.py <fixture-stem> [--realtime] [--drop-every N] [--record N] [--repeat K]
                [--kill-after S] -- <plugin argv...>
Prints one JSON result object on stdout.
"""
import json
import os
import struct
import subprocess
import sys
import threading
import time

import numpy as np

REC_HDR = struct.Struct("<BBHIQqQ")


def frame(payload: bytes) -> bytes:
    return struct.pack("<I", len(payload)) + payload


def ps_sample(pid: int) -> tuple[float, float] | None:
    """(rss MiB, cumulative cpu seconds) for pid and its descendants."""
    try:
        out = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,rss=,time="], capture_output=True,
                             text=True, check=True).stdout
    except subprocess.CalledProcessError:
        return None
    rows = {}
    for ln in out.splitlines():
        p, pp, rss, t = ln.split(None, 3)
        rows[int(p)] = (int(pp), int(rss), t.strip())
    family, frontier = {pid}, [pid]
    while frontier:
        cur = frontier.pop()
        for p, (pp, _, _) in rows.items():
            if pp == cur and p not in family:
                family.add(p)
                frontier.append(p)
    if pid not in rows:
        return None

    def secs(t: str) -> float:
        parts = [float(x) for x in t.replace("-", ":").split(":")]
        s = 0.0
        for x in parts:
            s = s * 60 + x
        return s

    return (sum(rows[p][1] for p in family if p in rows) / 1024,
            sum(secs(rows[p][2]) for p in family if p in rows))


def main() -> int:
    argv = sys.argv[1:]
    sep = argv.index("--")
    opts, plugin = argv[:sep], argv[sep + 1:]
    stem = opts[0]
    realtime = "--realtime" in opts
    get = lambda k, d: type(d)(opts[opts.index(k) + 1]) if k in opts else d  # noqa: E731
    drop_every, rec_len, repeat = get("--drop-every", 0), get("--record", 8192), get("--repeat", 1)
    kill_after = get("--kill-after", 0.0)

    truth = json.load(open(stem + ".truth.json"))
    fs = truth["sample_rate_hz"]
    x = np.fromfile(stem + ".cf32", dtype="<c8")
    n = len(x)

    t_spawn = time.monotonic()
    proc = subprocess.Popen(plugin, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE)
    lines, stderr_lines, events = [], [], {}
    ready = threading.Event()

    def pump_out() -> None:
        for raw in proc.stdout:
            try:
                obj = json.loads(raw)
            except ValueError:
                events.setdefault("malformed", 0)
                events["malformed"] = events["malformed"] + 1
                continue
            now = time.monotonic() - t_spawn
            if obj.get("type") == "ready":
                events["ready_s"] = now
                ready.set()
            elif obj.get("type") == "decode":
                obj["_t"] = now
                lines.append(obj)
            elif obj.get("type") == "log":
                events.setdefault("logs", []).append(obj["msg"])

    def pump_err() -> None:
        for raw in proc.stderr:
            stderr_lines.append(raw.decode(errors="replace").rstrip())

    threading.Thread(target=pump_out, daemon=True).start()
    threading.Thread(target=pump_err, daemon=True).start()

    samples = []
    stop_sampling = threading.Event()

    def sampler() -> None:
        while not stop_sampling.is_set():
            s = ps_sample(proc.pid)
            if s:
                samples.append((time.monotonic() - t_spawn, *s))
            stop_sampling.wait(0.25)

    threading.Thread(target=sampler, daemon=True).start()

    if not ready.wait(60):
        events["ready_timeout"] = True
    header = {"schema": "hackriff.stream", "version": "1.2", "stream_id": "t556/channel",
              "kind": "iq", "content_class": "unrestricted", "source": "t556-drive",
              "datatype": "cf32_le", "sample_rate_hz": fs, "center_hz": 868.1e6,
              "max_frame_len": REC_HDR.size + rec_len * 8, "record_header_len": 32,
              "t_start": time.time_ns(), "hackriff_version": "spike"}
    parity = "frames" not in truth  # payload-multiset truth (sat_fixture.py), no positions
    truth.setdefault("frames", [])
    host_truth, dropped_ranges = [], []
    feed_start = time.monotonic()
    try:
        proc.stdin.write(frame(json.dumps(header).encode()))
        seq, host_base = 0, 0
        for rep in range(repeat):
            for f in truth["frames"]:
                host_truth.append({**f, "host_start": host_base + f["start_sample"]})
            for i, off in enumerate(range(0, n, rec_len)):
                chunk = x[off:off + rec_len]
                sidx = host_base + off
                if drop_every and i % drop_every == drop_every - 1:
                    dropped_ranges.append((sidx, sidx + len(chunk)))
                    proc.stdin.write(frame(REC_HDR.pack(2, 2, 0, 8, seq, 0, sidx)
                                           + struct.pack("<Q", 1)))
                else:
                    proc.stdin.write(frame(REC_HDR.pack(1, 0, 0, len(chunk) * 8, seq, 0, sidx)
                                           + chunk.tobytes()))
                seq += 1
                if realtime:
                    due = feed_start + (sidx + len(chunk)) / fs
                    delay = due - time.monotonic()
                    if delay > 0:
                        time.sleep(delay)
                if kill_after and time.monotonic() - t_spawn > kill_after:
                    raise BrokenPipeError
            host_base += n
        proc.stdin.close()
    except BrokenPipeError:
        events["stdin_broken"] = True
    feed_s = time.monotonic() - feed_start
    if kill_after:
        proc.kill()
    # wait4 (not Popen.wait) so the child's own rusage gives exact CPU and peak RSS.
    waited = {}
    w = threading.Thread(target=lambda: waited.update(r=os.wait4(proc.pid, 0)), daemon=True)
    w.start()
    w.join(60)
    if "r" not in waited:
        proc.kill()
        w.join(5)
    _, status, ru = waited.get("r", (0, -1, None))
    proc.returncode = rc = os.waitstatus_to_exitcode(status) if status != -1 else "timeout"
    exit_s = time.monotonic() - t_spawn
    time.sleep(0.2)
    stop_sampling.set()

    # Blind scoring: every truth frame not overlapping a dropped range must be found, with the
    # right payload, at a host sample_index within tolerance of the true start.
    tol = 0 if parity else int(0.5 * (1 << truth["sf"]) * fs / truth["bw_hz"])  # half a symbol
    expected = [t for t in host_truth
                if not any(a < t["host_start"] + (t["end_sample"] - t["start_sample"]) and
                           t["host_start"] < b for a, b in dropped_ranges)]
    found, errs = 0, []
    for t in expected:
        m = [d for d in lines if d.get("content", {}).get("payload_text") == t["payload"]
             and d.get("crc_status") == "valid" and "sample_index" in d
             and abs(d["sample_index"] - t["host_start"]) <= tol]
        if m:
            found += 1
            errs.append(m[0]["sample_index"] - t["host_start"])
    false = [d for d in lines if not any(d.get("content", {}).get("payload_text") == t["payload"]
                                         for t in host_truth)]
    if parity:
        from collections import Counter
        want = Counter(truth["payloads_hex"] * repeat)
        got = Counter(d.get("content", {}).get("payload_hex") for d in lines)
        found = sum((want & got).values())
        expected = list(range(sum(want.values())))
        false = list((got - want).elements())
        host_truth = expected
    rss = [s[1] for s in samples]
    cpu_total = round(ru.ru_utime + ru.ru_stime, 3) if ru else None
    result = {
        "plugin": plugin, "realtime": realtime, "record_samples": rec_len, "repeat": repeat,
        "drop_every": drop_every, "fixture_s": truth["duration_s"] * repeat,
        "ready_s": events.get("ready_s"), "first_decode_s": lines[0]["_t"] if lines else None,
        "feed_s": round(feed_s, 3), "exit_s": round(exit_s, 3), "rc": rc,
        "truth_frames": len(host_truth), "expected_frames": len(expected), "found": found,
        "decodes": len(lines), "false_or_garbled": len(false),
        "crc_invalid": sum(d.get("crc_status") == "invalid" for d in lines),
        "sample_index_err": {"min": min(errs), "max": max(errs), "tol": tol} if errs else None,
        "rss_mib_peak": round(max(rss), 1) if rss else None,
        "rss_mib_last": round(rss[-1], 1) if rss else None,
        "maxrss_mib_rusage": round(ru.ru_maxrss / 2**20, 1) if ru else None,
        "cpu_s": cpu_total,
        "cpu_pct_of_core_over_feed": round(100 * cpu_total / feed_s, 1) if cpu_total and feed_s else None,
        "malformed_stdout": events.get("malformed", 0), "stdin_broken": events.get("stdin_broken", False),
        "stderr_lines": len(stderr_lines), "stderr_tail": stderr_lines[-4:],
        "plugin_log": events.get("logs", [])[-1:] if events.get("logs") else [],
    }
    print(json.dumps(result, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
