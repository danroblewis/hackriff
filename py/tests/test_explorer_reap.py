"""hkpy.explorer_reap (T-983): the explorer window's IQ-ring reaper and rogue-server detector.

Two halves, both pure and both testable off a fake tree / fake `ps` table without tmux, a Mac or
a HackRF: (1) find and delete just the ring directories (`iqbuffer`, `iqbuffer-devices`) under a
window's data dirs, never history/recordings/db/logs; (2) parse a process table and pick out any
`hk serve` whose `--data-dir` is under the window but isn't the one server the window started.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest

from hkpy import explorer_reap as er

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "py" / "hkpy" / "explorer_reap.py"


def _make_ring(data_dir: Path, name="iqbuffer", nbytes=1000):
    d = data_dir / name
    d.mkdir(parents=True)
    (d / "ring.bin").write_bytes(b"\0" * nbytes)
    return d


# ---------------------------------------------------------------- ring_dirs / find_data_dirs


def test_ring_dirs_finds_only_the_known_ring_names(tmp_path):
    _make_ring(tmp_path, "iqbuffer")
    (tmp_path / "iqbuffer-devices" / "hackrf-0").mkdir(parents=True)
    (tmp_path / "history").mkdir()
    (tmp_path / "recordings").mkdir()
    (tmp_path / "hk.db").write_text("x")
    found = {p.name for p in er.ring_dirs(tmp_path)}
    assert found == {"iqbuffer", "iqbuffer-devices"}


def test_find_data_dirs_stops_descending_once_a_data_dir_is_found(tmp_path):
    dd = tmp_path / "data"
    _make_ring(dd)
    # A file inside the ring dir that itself looks like a directory named "iqbuffer" would be a
    # bug to find twice; here just assert we don't re-walk into dd's own ring subtree.
    (dd / "iqbuffer" / "nested").mkdir(parents=True, exist_ok=True)
    found = er.find_data_dirs(tmp_path)
    assert found == [dd]


def test_find_data_dirs_over_a_window_with_several_servers(tmp_path):
    """The T-983 shape: one hk serve per target, each with its own --data-dir under the window."""
    d1 = _make_ring(tmp_path / "data-fm", "iqbuffer", 4096)
    d2 = _make_ring(tmp_path / "data-70cm", "iqbuffer", 2048)
    (tmp_path / "captures" / "20260925").mkdir(parents=True)  # never mistaken for a data dir
    (tmp_path / "journal-20260925.md").write_text("# journal")
    found = sorted(er.find_data_dirs(tmp_path))
    assert found == sorted([d1.parent, d2.parent])


def test_find_data_dirs_on_a_missing_window_is_empty(tmp_path):
    assert er.find_data_dirs(tmp_path / "nope") == []


# ---------------------------------------------------------------------------------- plan / reap


def test_plan_reap_sums_bytes_without_touching_anything(tmp_path):
    _make_ring(tmp_path / "data", "iqbuffer", 4096)
    (tmp_path / "data" / "history").mkdir()
    (tmp_path / "data" / "history" / "keep.db").write_text("durable")
    plan = er.plan_reap(tmp_path)
    assert len(plan) == 1
    rd, nbytes = plan[0]
    assert rd == tmp_path / "data" / "iqbuffer"
    assert nbytes == 4096
    assert rd.is_dir()  # dry: nothing deleted
    assert (tmp_path / "data" / "history" / "keep.db").exists()


def test_reap_dry_run_deletes_nothing(tmp_path):
    _make_ring(tmp_path / "data", "iqbuffer", 100)
    plan, total = er.reap(tmp_path, dry_run=True)
    assert total == 100
    assert (tmp_path / "data" / "iqbuffer").is_dir()


def test_reap_deletes_only_ring_dirs_never_history_db_or_logs(tmp_path):
    """The exact selection the ticket asks for: the reaper picks the window's ring files and
    nothing else the coordinator reads."""
    d = tmp_path / "data"
    _make_ring(d, "iqbuffer", 5000)
    (d / "history").mkdir()
    (d / "history" / "detections.db").write_text("keep me")
    (tmp_path / "window.log").write_text("keep me too")
    (tmp_path / "journal-20260925.md").write_text("and me")
    (tmp_path / "captures" / "20260925").mkdir(parents=True)
    (tmp_path / "captures" / "20260925" / "fm-88.5.sigmf-data").write_bytes(b"x" * 10)

    plan, total = er.reap(tmp_path, dry_run=False)

    assert total == 5000
    assert not (d / "iqbuffer").exists()
    assert (d / "history" / "detections.db").read_text() == "keep me"
    assert (tmp_path / "window.log").read_text() == "keep me too"
    assert (tmp_path / "journal-20260925.md").read_text() == "and me"
    assert (tmp_path / "captures" / "20260925" / "fm-88.5.sigmf-data").exists()


def test_reap_over_several_servers_reclaims_each(tmp_path):
    _make_ring(tmp_path / "data-fm", "iqbuffer", 3000)
    _make_ring(tmp_path / "data-70cm", "iqbuffer", 1000)
    plan, total = er.reap(tmp_path, dry_run=False)
    assert total == 4000
    assert len(plan) == 2
    assert not (tmp_path / "data-fm" / "iqbuffer").exists()
    assert not (tmp_path / "data-70cm" / "iqbuffer").exists()


def test_reap_on_an_empty_window_reclaims_nothing(tmp_path):
    plan, total = er.reap(tmp_path, dry_run=False)
    assert plan == [] and total == 0


# -------------------------------------------------------------------------- process table parsing


def test_parse_data_dir_handles_space_and_equals_forms():
    assert er.parse_data_dir("hk serve --hackrf --data-dir /x/y --center-hz 1e8") == "/x/y"
    assert er.parse_data_dir("hk serve --data-dir=/x/y") == "/x/y"
    assert er.parse_data_dir("hk serve --no-data-dir-here") is None


def test_parse_process_table_keeps_only_hk_serve_with_a_data_dir():
    lines = [
        "111 hk serve --hackrf --data-dir /w/data --bind 127.0.0.1:8897",
        "222 python3 ops/stage.sh --hackrf",  # not hk serve
        "333 hk serve --hackrf --bind 127.0.0.1:9999",  # no --data-dir: not comparable
        "  ",
        "not-a-pid hk serve --data-dir /w/data",
        "444 hk build --data-dir /w/data",  # hk but not serve
    ]
    procs = er.parse_process_table(lines)
    assert [p.pid for p in procs] == [111]
    assert procs[0].data_dir == Path("/w/data")


# ------------------------------------------------------------------------------------ find_rogue


def test_find_rogue_over_a_fake_process_table_flags_a_second_server_new_data_dir(tmp_path):
    """The core T-983 amendment shape: window's own server is data-fm (kept), the agent started a
    second one on a fresh --data-dir for the next target. It is detected and the first untouched."""
    kept = tmp_path / "data-fm"
    rogue_dir = tmp_path / "data-70cm"
    table = [
        f"100 hk serve --hackrf --data-dir {kept} --bind 127.0.0.1:8897",
        f"200 hk serve --hackrf --data-dir {rogue_dir} --bind 127.0.0.1:8897",
    ]
    procs = er.parse_process_table(table)
    rogue = er.find_rogue(procs, tmp_path, keep_pid=100)
    assert [p.pid for p in rogue] == [200]
    assert rogue[0].data_dir == rogue_dir
    # the window's own server was not flagged
    assert 100 not in [p.pid for p in rogue]


def test_find_rogue_flags_a_second_listener_on_the_same_data_dir(tmp_path):
    """A second `hk serve` process pointed at the SAME --data-dir as the kept one is still rogue
    (a second listener) - it is the pid, not just the path, that decides "is this the one"."""
    dd = tmp_path / "data"
    table = [
        f"100 hk serve --hackrf --data-dir {dd} --bind 127.0.0.1:8897",
        f"101 hk serve --hackrf --data-dir {dd} --bind 127.0.0.1:8898",
    ]
    procs = er.parse_process_table(table)
    rogue = er.find_rogue(procs, tmp_path, keep_pid=100)
    assert [p.pid for p in rogue] == [101]


def test_find_rogue_ignores_servers_outside_the_window(tmp_path):
    other = tmp_path.parent / "somewhere-else" / "data"
    table = [f"300 hk serve --hackrf --data-dir {other} --bind 127.0.0.1:8899"]
    procs = er.parse_process_table(table)
    rogue = er.find_rogue(procs, tmp_path, keep_pid=None)
    assert rogue == []


def test_find_rogue_with_no_kept_pid_flags_every_server_under_the_window(tmp_path):
    dd = tmp_path / "data"
    table = [f"400 hk serve --hackrf --data-dir {dd} --bind 127.0.0.1:8897"]
    procs = er.parse_process_table(table)
    rogue = er.find_rogue(procs, tmp_path, keep_pid=None)
    assert [p.pid for p in rogue] == [400]


def test_find_rogue_none_when_only_the_kept_server_is_running(tmp_path):
    dd = tmp_path / "data"
    table = [f"100 hk serve --hackrf --data-dir {dd} --bind 127.0.0.1:8897"]
    procs = er.parse_process_table(table)
    assert er.find_rogue(procs, tmp_path, keep_pid=100) == []


# ----------------------------------------------------------------------------------------- CLI


def test_cli_reap_dry_run_prints_the_plan_and_total(tmp_path):
    _make_ring(tmp_path / "data", "iqbuffer", 2048)
    r = subprocess.run(
        [sys.executable, str(SCRIPT), "reap", "--window", str(tmp_path), "--dry-run"],
        capture_output=True, text=True, check=True,
    )
    lines = r.stdout.strip().splitlines()
    assert any("iqbuffer" in ln and "2048" in ln for ln in lines)
    assert lines[-1] == "TOTAL\t2048"
    assert (tmp_path / "data" / "iqbuffer").is_dir()  # dry-run: untouched


def test_cli_reap_for_real_deletes_and_reports(tmp_path):
    _make_ring(tmp_path / "data", "iqbuffer", 512)
    r = subprocess.run(
        [sys.executable, str(SCRIPT), "reap", "--window", str(tmp_path)],
        capture_output=True, text=True, check=True,
    )
    assert r.stdout.strip().splitlines()[-1] == "TOTAL\t512"
    assert not (tmp_path / "data" / "iqbuffer").exists()


def test_cli_rogue_reads_a_fake_ps_output_file(tmp_path):
    kept = tmp_path / "data-fm"
    rogue_dir = tmp_path / "data-70cm"
    ps_out = tmp_path / "ps.txt"
    ps_out.write_text(
        f"100 hk serve --hackrf --data-dir {kept} --bind 127.0.0.1:8897\n"
        f"200 hk serve --hackrf --data-dir {rogue_dir} --bind 127.0.0.1:8897\n"
    )
    r = subprocess.run(
        [sys.executable, str(SCRIPT), "rogue", "--window", str(tmp_path), "--keep-pid", "100",
         "--ps-output", str(ps_out)],
        capture_output=True, text=True, check=True,
    )
    out = r.stdout.strip().splitlines()
    assert out == [f"200\t{rogue_dir}"]


def test_cli_rogue_empty_when_only_the_kept_server_is_present(tmp_path):
    kept = tmp_path / "data-fm"
    ps_out = tmp_path / "ps.txt"
    ps_out.write_text(f"100 hk serve --hackrf --data-dir {kept} --bind 127.0.0.1:8897\n")
    r = subprocess.run(
        [sys.executable, str(SCRIPT), "rogue", "--window", str(tmp_path), "--keep-pid", "100",
         "--ps-output", str(ps_out)],
        capture_output=True, text=True, check=True,
    )
    assert r.stdout.strip() == ""


if __name__ == "__main__":
    sys.exit(pytest.main([__file__, "-v"]))
