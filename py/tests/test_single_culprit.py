"""ops/merge-runner.sh - a batch that merged ONE branch and went red on a test that fails alone on base + it, with main
green on base, blames that branch at once instead of re-gating it alone. Incident 2026-09-26 00:05-00:50: a
`BULK attempt (5)` merged only task-t1009, went red on listen_identifies_ctcss_67p0, the triage ran it alone (red) and
on main (green) - and the runner still re-gated task-t1009 alone, a 30-40 min full gate, because the bisect needs >= 2
branches. The REAL try_bulk failure path runs over stubbed git/cargo/npm/uv.
"""

import pathlib
import re
import subprocess

RUNNER = pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh"


def _function(name: str) -> str:
    m = re.search(rf"^{name}\(\)\{{.*?^\}}\n", RUNNER.read_text(), re.M | re.S)
    assert m, f"{name} not found in {RUNNER}"
    return m.group(0)


def _fail_path() -> str:
    """try_bulk from the red-gate rewind to its end: the code that decides hold / main-red / bisect / isolate."""
    text = RUNNER.read_text()
    i = text.index("\ntry_bulk(){")
    start = text.index('  if [ "$(git -C "$REPO" rev-parse HEAD)" = "$after" ]; then', i)
    end = text.index("\n}\n", start)
    return text[start:end]


def _run(tmp_path, branches, culprits, main_rc=0, alone_first=1, specs="", side="", confirm_green=False):
    """The triaged test is red whenever a culprit is merged (green on every probe if confirm_green: the triage's
    red alone was a one-off); main_rc is its exit on the bare base."""
    state, probes, queue, needs = (tmp_path / n for n in ("merged", "probes", "queue", "needs"))
    state.write_text("")
    queue.write_text("task-later\n")
    (tmp_path / "bulk").write_text("base=base\n")
    (tmp_path / "ui").mkdir()
    culprit_re = "|".join(culprits) or "NONE"
    gated = " ".join(f"{b}=sha-{b}" for b in branches)
    filt = "" if specs else "test(listen_identifies_ctcss_67p0)"
    script = f"""
set -uo pipefail
LOG={tmp_path}/log; REPO={tmp_path}; S={tmp_path}; QUEUE={queue}; NEEDS={needs}; BULKMARK={tmp_path}/bulk
log(){{ echo "LOG $*" >&2; }}
ticket_of(){{ echo "$1"; }}
notify_coordinator(){{ echo "NOTIFY [$2] $1"; }}
record_attempt(){{ echo "$1 $2" >> {tmp_path}/attempts; }}
git(){{ shift 2
  case "$1" in
    rev-parse) [ -s {state} ] && echo moved || echo base ;;
    reset) : > {state} ;;
    merge) [ "$2" = --abort ] && return 0; b="${{@: -1}}"; b=${{b#sha-}}; echo "$b" >> {state} ;;
  esac; return 0; }}
probe(){{ echo "$1 | $(tr '\\n' ' ' < {state})" >> {probes}
  [ -s {state} ] || return {main_rc}
  [ {int(confirm_green)} = 1 ] && return 0
  grep -qxE '{culprit_re}' {state} && return 1; return 0; }}
cargo(){{ [ "$1" = build ] && return 0; probe cargo; }}
npm(){{ [ "$1" = run ] && [ "$2" = build ] && return 0; probe npm; }}
uv(){{ [ -n "{side}" ] && echo "main-side {side}"; return 0; }}
just(){{ echo "just $*" >> {probes}; return 0; }}
suite_split(){{ echo "SUITE_SPLIT"; }}
TRIAGE_KIND=test; TRIAGE_CHECK=""; TRIAGE_WHAT=""; TRIAGE_FILTER="{filt}"; TRIAGE_SPECS="{specs}"
TRIAGE_TESTS="hk-pipeline::listen listen_identifies_ctcss_67p0"; TRIAGE_ALONE_FIRST={alone_first}
{_function("main_is_red")}
{_function("main_side_of")}
{_function("bisect_red")}
{_function("bisect_fact")}
{_function("bisect_culprit")}
{_function("suite_broken_hold")}
fail(){{
local branches=({' '.join(branches)}) gated=({gated}) base=base after=base tickets="{' '.join(branches)}" b wt rc tip
local gated_sig="SIG {' '.join(branches)}"
{_fail_path()}
}}
fail; echo "RC=$?"
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)
    assert out.returncode == 0, out.stderr

    def read(p):
        return p.read_text() if p.exists() else ""
    return {
        "out": out.stdout + out.stderr,
        "probes": read(probes).splitlines(),
        "queue": read(queue).split(),
        "needs": read(needs),
        "attempts": read(tmp_path / "attempts").split(),
        "bulk": (tmp_path / "bulk").exists(),
    }


def test_a_batch_of_one_red_alone_with_main_green_is_failed_without_a_re_gate(tmp_path):
    r = _run(tmp_path, ["task-t1009"], ["task-t1009"])
    assert "RC=0" in r["out"]                                            # handled: the caller isolates nothing
    assert "falling back" not in r["out"] and "isolate by merging each individually" not in r["out"]
    assert r["probes"] == ["cargo | ", "cargo | task-t1009 "]            # main alone, then ONE confirming run; no gate
    assert r["attempts"] == ["task-t1009", "sha-task-t1009"]             # charged on the tip it was gated at
    assert "task-t1009  task-t1009  GATE_FAIL" in r["needs"]
    assert ("LOG GATE FAILED task-t1009 (the batch's only merged branch: red ALONE twice on base + it, green on base: "
            "test(listen_identifies_ctcss_67p0)) -> abort + flag for AI") in r["out"]
    assert "NOTIFY [gate failed - fix run] task-t1009 (task-t1009) FAILED the merge gate" in r["out"]
    assert r["queue"] == ["task-later"] and not r["bulk"]


def test_a_batch_of_one_browser_spec_red_is_failed_the_same_way(tmp_path):
    r = _run(tmp_path, ["task-t1"], ["task-t1"], specs="app-surface.e2e.mjs")
    assert "RC=0" in r["out"] and "GATE FAILED task-t1 (the batch's only merged branch" in r["out"]
    assert r["probes"] == ["npm | ", "npm | task-t1 "] and "task-t1  task-t1  GATE_FAIL" in r["needs"]


def test_a_batch_of_one_on_a_red_main_keeps_the_main_red_hold(tmp_path):
    r = _run(tmp_path, ["task-t1009"], ["task-t1009"], main_rc=1)
    assert "RC=0" in r["out"] and "MAIN_RED - test(listen_identifies_ctcss_67p0) fail(s) on main itself" in r["needs"]
    assert "GATE_FAIL" not in r["needs"] and r["attempts"] == []
    assert r["queue"] == ["task-later", "task-t1009"]


def test_a_red_that_passed_alone_first_still_isolates(tmp_path):
    """Flaky even alone (passed, then failed): not proof against the branch - isolation decides, as before."""
    r = _run(tmp_path, ["task-t1009"], ["task-t1009"], alone_first=0)
    assert "RC=1" in r["out"] and "isolate by merging each individually" in r["out"]
    assert "GATE_FAIL" not in r["needs"] and r["attempts"] == []


def test_a_main_side_spec_is_not_blamed_on_the_one_branch(tmp_path):
    r = _run(tmp_path, ["task-t1"], ["task-t1"], specs="app-surface.e2e.mjs", side="app-surface.e2e.mjs")
    assert "RC=1" in r["out"] and "is main-side -> no blame here" in r["out"]
    assert "GATE_FAIL" not in r["needs"] and r["attempts"] == []


def test_two_merged_branches_still_bisect(tmp_path):
    r = _run(tmp_path, ["b0", "b1"], ["b1"])
    assert "RC=0" in r["out"] and "bisecting before any isolate" in r["out"]
    assert "GATE FAILED b1 (bisected: red ALONE twice on base + it, green on base" in r["out"]
    assert "the batch's only merged branch" not in r["out"]
    assert r["queue"] == ["b0", "task-later"] and r["attempts"] == ["b1", "sha-b1"]


def test_a_batch_of_one_green_on_the_confirming_run_is_not_blamed_and_isolates(tmp_path):
    """Red alone once is not enough (the bisect's own rule): a green confirming probe blames nobody."""
    r = _run(tmp_path, ["task-t1009"], ["task-t1009"], confirm_green=True)
    assert r["probes"] == ["cargo | ", "cargo | task-t1009 "]
    assert "RC=1" in r["out"] and "isolate by merging each individually" in r["out"]
    assert "the confirming run alone on base + it was green -> no blame here" in r["out"]
    assert "GATE FAILED" not in r["out"] and "GATE_FAIL" not in r["needs"] and r["attempts"] == []
