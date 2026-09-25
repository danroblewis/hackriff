"""ops/monitor.py's radio_status()/explorer_status() (T-925): the dashboard's EXPLORER row
(current target, phase, journal tail) and RADIO-OWNER tile (holder, until, staging mode),
read from $HACKRIFF_OPS/radio-lock (hkpy.radio, T-922) and the explorer journal
($HACKRIFF_OPS/explorer/journal-YYYYMMDD.md, per .claude/agents/explorer.md). Same
importlib-by-path style as test_monitor_flow.py — importing monitor.py must have no side
effects (no server bound).
"""

from __future__ import annotations

import importlib.util
import os
import sys
import time

import pytest

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
MONITOR_PATH = os.path.join(REPO, "ops", "monitor.py")


def _load_monitor():
    spec = importlib.util.spec_from_file_location("hk_ops_monitor_radio_explorer", MONITOR_PATH)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


@pytest.fixture(scope="module")
def monitor():
    before = set(sys.modules)
    m = _load_monitor()
    yield m
    for k in set(sys.modules) - before:
        if k == "hk_ops_monitor_radio_explorer":
            sys.modules.pop(k, None)


@pytest.fixture()
def ops(tmp_path, monitor, monkeypatch):
    monkeypatch.setattr(monitor, "SCRATCH", str(tmp_path))
    return tmp_path


def _write_lock(ops, owner, since, until, why="test"):
    (ops / "radio-lock").write_text(f"owner={owner}\nsince={since}\nuntil={until}\nwhy={why}\n")


# --- radio_status() -----------------------------------------------------------------------

def test_radio_status_free_when_no_lock(ops, monitor):
    st = monitor.radio_status()
    assert st["held"] is False
    assert "staging" in st


def test_radio_status_held_reports_owner_and_until(ops, monitor):
    now = time.time()
    _write_lock(ops, "explorer", int(now), int(now + 3600), "explorer window: FM/RDS, NOAA WX")
    st = monitor.radio_status()
    assert st["held"] is True
    assert st["owner"] == "explorer"
    assert st["until"] == int(now + 3600)
    assert st["why"].startswith("explorer window")


def test_radio_status_stale_lock_reads_as_free(ops, monitor):
    now = time.time()
    _write_lock(ops, "capture-agent", int(now - 7200), int(now - 3600))
    st = monitor.radio_status()
    assert st["held"] is False
    assert st["stale"] is True


def test_radio_status_reflects_staging_mode(ops, monitor):
    (ops / "hk-serve-source").write_text("source: replay (radio-lock: explorer since 12:00)\n")
    st = monitor.radio_status()
    assert "replay" in st["staging"]


# --- explorer_status() --------------------------------------------------------------------

def test_explorer_status_no_window_when_nothing_ever_ran(ops, monitor):
    st = monitor.explorer_status()
    assert st["phase"] == "no window"
    assert st["target"] == ""
    assert st["tail"] == []


def test_explorer_status_running_reports_current_target_and_phase(ops, monitor):
    edir = ops / "explorer"
    edir.mkdir()
    (edir / "journal-20260925.md").write_text(
        "## 22:05 FM/RDS 88-108 MHz (88-108 MHz)\n"
        "- **Looked for / why:** FM broadcast band\n"
        "- **Decode:** partial\n"
        "## 23:10 NOAA WX 162.400 (162.400 MHz)\n"
        "- **Looked for / why:** NWR\n"
    )
    now = time.time()
    _write_lock(ops, "explorer", int(now), int(now + 1800), "explorer window 1")
    st = monitor.explorer_status()
    assert st["phase"] == "running"
    assert st["target"].startswith("NOAA WX 162.400")
    assert any("NOAA WX" in line for line in st["tail"])


def test_explorer_status_wrap_up_when_marker_present(ops, monitor):
    edir = ops / "explorer"
    edir.mkdir()
    (edir / "journal-20260925.md").write_text("## 22:05 FM/RDS (88-108 MHz)\n")
    (edir / "wrap-up").write_text("")
    now = time.time()
    _write_lock(ops, "explorer", int(now), int(now + 60), "explorer window 1")
    st = monitor.explorer_status()
    assert st["phase"] == "wrap-up"


def test_explorer_status_ended_after_lock_released(ops, monitor):
    edir = ops / "explorer"
    edir.mkdir()
    (edir / "journal-20260925.md").write_text("## 22:05 FM/RDS (88-108 MHz)\n## Window summary\n- targets tried: 1\n")
    st = monitor.explorer_status()
    # No live lock held by explorer -> not "running"; the trailing "## Window summary" section
    # (with content after it) reads as summarized rather than pointing at a stale target.
    assert st["phase"] == "summarized"


def test_explorer_status_someone_elses_lock_does_not_read_as_running(ops, monitor):
    edir = ops / "explorer"
    edir.mkdir()
    (edir / "journal-20260925.md").write_text("## 22:05 FM/RDS (88-108 MHz)\n")
    now = time.time()
    _write_lock(ops, "capture-agent", int(now), int(now + 1800), "fixture capture")
    st = monitor.explorer_status()
    assert st["phase"] == "ended"
