"""System stats for every connected remote node on the dashboard (user via supervisor, 2026-09-25 14:02).

ops/work-runner.py's per-tick host probe (one ssh: loadavg, nproc, df, meminfo, /proc/stat) -> hosts/<h>.json and the
status file's hosts[h].stats; ops/monitor.py's host_tiles() -> one System tile per host, 'stale <age>' past two probe
intervals, 'unreachable' when the probe failed, never blank. Hermetic: fake ssh output, tmp $HACKRIFF_OPS.
"""
from __future__ import annotations

import importlib.util
import json
import pathlib
import re
import shutil
import subprocess

import pytest

_OPS = pathlib.Path(__file__).resolve().parents[2] / "ops"


def _load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


R = _load("hk_work_runner_hoststats", _OPS / "work-runner.py")
M = _load("hk_monitor_hoststats", _OPS / "monitor.py")

# node2's real output shape (2026-09-25), counters chosen so the delta is easy to check by hand
PROBE_1 = ("9.63 8.54 13.63 22/4064 1202593\n24\n  76G     1877G\n"
           "MemTotal:       65761928 kB\nMemAvailable:   43711812 kB\n"
           "cpu  1000 0 500 8000 500 0 0 0 0 0\n")
# +1000 ticks: +600 user, +150 system, +200 idle, +50 iowait -> busy 750/1000 = 75 %
PROBE_2 = PROBE_1.replace("cpu  1000 0 500 8000 500", "cpu  1600 0 650 8200 550")


@pytest.fixture()
def ops(tmp_path, monkeypatch):
    for name in ("CLAIMS", "NEEDS", "DONE", "LOG", "WORKDIR", "HOSTS_FILE"):
        monkeypatch.setattr(R, name, str(tmp_path / pathlib.Path(getattr(R, name)).name))
    monkeypatch.setattr(R, "S", str(tmp_path))
    monkeypatch.setattr(R, "PROJECTS", str(tmp_path / "projects"))
    monkeypatch.setattr(M, "SCRATCH", str(tmp_path))
    (tmp_path / "hosts.json").write_text(json.dumps({"node2": {"ssh": "u@h", "repo": "/r", "ops": "/o"}}))
    return tmp_path


def test_the_first_probe_has_every_stat_but_cpu_percent():
    rec = R.parse_probe(PROBE_1, {}, 1000)
    assert rec["reachable"] and rec["at"] == 1000
    assert (rec["load1"], rec["load5"], rec["load15"], rec["cores"]) == (9.63, 8.54, 13.63, 24)
    assert (rec["disk_free_gb"], rec["disk_total_gb"]) == (76, 1877)
    assert (rec["mem_total_gb"], rec["mem_used_gb"], rec["mem_pct"]) == (67, 22.6, 34)
    assert rec["cpu_ticks"] == [10000, 8500]
    assert "cpu_pct" not in rec and "interval_s" not in rec          # no previous sample: no delta, shown as '—'


def test_cpu_percent_is_the_delta_between_two_probes():
    first = R.parse_probe(PROBE_1, {}, 1000)
    second = R.parse_probe(PROBE_2, first, 1040)
    assert second["cpu_pct"] == 75 and second["interval_s"] == 40
    assert R.parse_probe(PROBE_1, first, 1080).get("cpu_pct") is None     # no ticks elapsed: no percentage, not 0


def test_the_runner_keeps_the_previous_sample_in_the_host_file_and_the_status_carries_the_stats(ops, monkeypatch):
    outs = iter([(0, PROBE_1), (0, PROBE_2), (255, "ssh: connect timed out")])
    monkeypatch.setattr(R, "remote_sh", lambda h, cmd, timeout=120, input=None: next(outs))
    clock = iter([1000, 1040, 1080])
    monkeypatch.setattr(R.time, "time", lambda: next(clock))
    R.sync_remote_view({}, dry=False)
    assert "cpu_pct" not in json.load(open(ops / "hosts" / "node2.json"))
    R.sync_remote_view({}, dry=False)
    rec = json.load(open(ops / "hosts" / "node2.json"))
    assert rec["cpu_pct"] == 75 and rec["mem_pct"] == 34 and rec["interval_s"] == 40
    stats = R._probe_stats("node2")
    assert stats == {k: rec[k] for k in R.PROBE_STATS} and "cpu_ticks" not in stats
    R.sync_remote_view({}, dry=False)
    assert R._probe_stats("node2") == {"at": 1080, "reachable": False}


def _status(ops, stats, running=4, cap=6, refs=True):
    (ops / "work-runner-status.json").write_text(json.dumps({"hosts": {
        "mac": {"running": 3, "cap": 3}, "node2": {"running": running, "cap": cap, "refs_in_sync": refs,
                                                   "drifting": 0 if refs else 2, "stats": stats}}}))


LIVE = {"at": 1000, "reachable": True, "interval_s": 40, "cores": 24, "load1": 9.6, "load5": 8.5, "load15": 13.6,
        "cpu_pct": 75, "mem_used_gb": 22.6, "mem_total_gb": 67, "mem_pct": 34, "disk_free_gb": 76, "disk_total_gb": 1877}


def test_a_tile_per_remote_host_with_the_macs_fields(ops):
    _status(ops, LIVE)
    [t] = M.host_tiles(now=1012)
    assert t["name"] == "node2" and t["state"] == "live" and t["age_s"] == 12
    assert t["cores"] == 24 and t["load"] == [9.6, 8.5, 13.6] and t["cpu_pct"] == 75
    assert t["mem"] == {"used_gb": 22.6, "total_gb": 67, "pct": 34}
    assert t["disk"] == {"free_gb": 76, "total_gb": 1877, "pct": 96}
    assert (t["running"], t["cap"], t["refs_in_sync"]) == (4, 6, True)


def test_a_probe_older_than_two_intervals_is_stale_and_a_failed_one_unreachable(ops):
    _status(ops, LIVE)
    assert M.host_tiles(now=1079)[0]["state"] == "live"            # 79 s < 2 x 40 s
    assert M.host_tiles(now=1081)[0]["state"] == "stale"           # 81 s > 2 x 40 s
    _status(ops, {"at": 1000, "reachable": False})
    assert M.host_tiles(now=1010)[0]["state"] == "unreachable"
    assert M.host_tiles(now=1000 + 2 * M.PROBE_DEFAULT_S + 1)[0]["state"] == "stale"


def test_a_host_with_no_probe_still_gets_a_tile_and_an_older_runner_falls_back_to_the_probe_file(ops):
    [t] = M.host_tiles(now=1000)
    assert t["state"] == "no probe" and t["name"] == "node2"
    (ops / "hosts").mkdir()
    (ops / "hosts" / "node2.json").write_text(json.dumps({"at": 990, "reachable": True, "load1": 2.4, "cores": 24,
                                                          "disk_free_gb": 77}))
    [t] = M.host_tiles(now=1000)
    assert t["state"] == "live" and t["load"] == [2.4, None, None] and t["disk"] == {"free_gb": 77} and t["mem"] == {}


def _js(name):
    m = re.search(rf"^(?:const {name}=.*?;|function {name}\(.*?\n\}})$", M.PAGE, re.S | re.M)
    assert m, name
    return m.group(0)


@pytest.mark.skipif(not shutil.which("node"), reason="node not installed")
def test_the_page_renders_one_tile_per_host_with_stale_and_unreachable_said(ops):
    _status(ops, LIVE)
    tiles = M.host_tiles(now=1012)
    tiles += [dict(tiles[0], name="node3", state="stale", age_s=420),
              dict(tiles[0], name="node4", state="unreachable", age_s=5, cpu_pct=None, refs_in_sync=False, drifting=2)]
    esc = 'const esc=s=>(s||"").replace(/[&<>]/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;"}[c]));'
    src = "\n".join([esc, _js("dur"), _js("hostLine"), _js("gauge"), _js("renderHosts"),
                     f"console.log(renderHosts({json.dumps(tiles)}))"])
    html = subprocess.run(["node", "-e", src], capture_output=True, text=True, check=True).stdout
    assert html.count('class="card hostcard"') == 3
    assert "node2 · 24 cores · probe 12s ago" in html and "load 9.6 / 8.5 / 13.6 · cpu 75% · workers 4/6 · refs in sync" in html
    assert "22.6 / 67 GB · 34%" in html and "76 GB · 96% used" in html
    assert "stale 7m 0s" in html
    assert "unreachable</span> · probe 5s ago" in html and "cpu —" in html and "refs 2 drifting" in html
