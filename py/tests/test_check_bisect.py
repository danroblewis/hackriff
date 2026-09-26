"""ops/merge-runner.sh - a batch red on a CHECK (`just lint` / `just test-ui`, no test FAIL) is bisected by that check,
never re-queued whole. Incident 2026-09-25 12:48-13:07: seven batches in a row went red on clippy (t844 x t989 in
classify.rs, t940's presence_intervals()) and were re-bulked within ~10 s each time; supervisor 13:22: "it should
bisect by cargo check or hold, never retry unchanged". The REAL try_bulk failure path runs over stubbed git/just.
"""

import pathlib
import re
import subprocess

RUNNER = pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh"


def _function(name: str) -> str:
    m = re.search(rf"^{name}\(\)\{{.*?^\}}\n", RUNNER.read_text(), re.M | re.S)
    assert m, f"{name} not found in {RUNNER}"
    return m.group(0)


def _check_fn() -> str:
    m = re.search(r"^check_probe\(\)\{.*$", RUNNER.read_text(), re.M)
    assert m
    return m.group(0)


def _fail_path() -> str:
    """try_bulk from the red-gate rewind to its end: the code that decides hold / main-red / bisect / isolate."""
    text = RUNNER.read_text()
    i = text.index("\ntry_bulk(){")
    start = text.index('  if [ "$(git -C "$REPO" rev-parse HEAD)" = "$after" ]; then', i)
    end = text.index("\n}\n", start)
    return text[start:end]


def _run(tmp_path, branches, culprits, main_rc=0, check="lint", merge_fail="", recorded=None):
    """culprits break the check only together (all of them merged); main_rc is the check's exit on the bare base."""
    state, probes, queue, needs = (tmp_path / n for n in ("merged", "probes", "queue", "needs"))
    state.write_text("")
    queue.write_text("task-later\n")
    if recorded is not None:
        (tmp_path / "bisect-no-culprit").write_text("".join(f"{r}\n" for r in recorded))
    (tmp_path / "bulk").write_text("base=base\n")
    culprit_re = "|".join(culprits) or "NONE"
    gated = " ".join(f"{b}=sha-{b}" for b in branches)
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
    merge) [ "$2" = --abort ] && return 0; b="${{@: -1}}"; b=${{b#sha-}}; [ "$b" = "{merge_fail}" ] && return 1; echo "$b" >> {state} ;;
  esac; return 0; }}
just(){{ echo "just $* crates=[${{HK_GATE_CRATES-unset}}] | $(tr '\\n' ' ' < {state})" >> {probes}
  [ -s {state} ] || return {main_rc}
  [ "$(grep -cxE '{culprit_re}' {state})" -ge {len(culprits) or 1} ] && return 1; return 0; }}
cargo(){{ echo "cargo $*" >> {probes}; return 0; }}
npm(){{ echo "npm $*" >> {probes}; return 0; }}
uv(){{ echo "uv $*" >> {probes}; return 0; }}
suite_split(){{ echo "SUITE_SPLIT"; }}
TRIAGE_KIND=suite; TRIAGE_CHECK={check!r}; TRIAGE_WHAT="just {check}"; TRIAGE_FILTER=""; TRIAGE_SPECS=""; TRIAGE_TESTS=""
{_check_fn()}
{_function("main_is_red")}
{_function("main_side_of")}
{_function("reset_to_base")}
{_function("bisect_red")}
{_function("bisect_fact")}
{_function("bisect_culprit")}
{_function("suite_broken_hold")}
{_function("bulk_dirty_stop")}
fail(){{
local branches=({' '.join(branches)}) gated=({gated}) base=base after=base tickets="{' '.join(branches)}" b wt rc
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
        "held": read(tmp_path / "suite-broken"),
        "bulk": (tmp_path / "bulk").exists(),
        "left": read(state).strip(),
    }


def test_a_build_red_batch_sets_the_one_culprit_aside_and_re_queues_the_rest_first(tmp_path):
    """t940 alone breaks clippy (E0061 against a caller on main): named in log2 probes, never a SUITE_BROKEN re-bulk."""
    r = _run(tmp_path, ["b0", "b1", "b2", "b3"], ["b2"])
    assert "RC=0" in r["out"]
    assert r["queue"] == ["b0", "b1", "b3", "task-later"]            # the rest FIRST, as one batch
    assert r["attempts"] == ["b2", "sha-b2"]                         # charged on the tip it was probed at
    assert "b2  b2  GATE_FAIL" in r["needs"] and "SUITE_BROKEN" not in r["needs"]
    assert "NOTIFY [gate failed - fix run] b2 (b2) FAILED the merge gate (bisected from the batch): just lint" in r["out"]
    assert "GATE FAILED b2 (bisected: red ALONE twice on base + it, green on base: just lint)" in r["out"]
    assert r["held"] == ""                                           # no suite-broken hold on the rest
    assert not r["bulk"] and r["left"] == ""                         # main back on base, window closed


def test_the_probe_is_the_failing_check_over_the_workspace_never_a_test_run(tmp_path):
    r = _run(tmp_path, ["b0", "b1", "b2", "b3"], ["b2"])
    assert r["probes"][0] == "just lint crates=[] | "                # main alone first
    assert all(p.startswith("just lint crates=[] | ") for p in r["probes"])   # --workspace, as the merge gate
    assert [p.split("| ")[1].strip() for p in r["probes"][1:]] == ["b0 b1", "b2", "b2", "b2"]
    (tmp_path / "ui").mkdir()
    r = _run(tmp_path / "ui", ["b0", "b1"], ["b1"], check="test-ui")
    assert r["probes"] and all(p.startswith("just test-ui ") for p in r["probes"])


def test_a_pair_conflict_is_re_queued_once_with_the_pair_named_then_isolated(tmp_path):
    """t844 x t989: each alone compiles, together E0308. First red: re-queued as one batch, the pair in the attention
    file; the same tips red again: isolate (the no-culprit rule that landed with task-pm-bisect-retry)."""
    r = _run(tmp_path, ["b0", "b1"], ["b0", "b1"])
    assert "RC=0" in r["out"] and "isolate by merging each individually" not in r["out"]
    assert r["queue"] == ["b0", "b1", "task-later"] and r["attempts"] == []
    line = next(ln for ln in r["needs"].splitlines() if "CHECK_PAIR" in ln)
    assert "just lint red on base + b0 b1 together" in line and "re-queued once" in line
    assert "SUITE_BROKEN" not in r["needs"] and r["held"] == ""
    (tmp_path / "2").mkdir()
    r = _run(tmp_path / "2", ["b0", "b1"], ["b0", "b1"], recorded=["b0=sha-b0", "b1=sha-b1"])
    assert "RC=1" in r["out"] and "isolate by merging each individually" in r["out"]
    assert r["queue"] == ["task-later"]
    assert "CHECK_PAIR - just lint red on base + b0 b1 together" in r["needs"] and "isolating now" in r["needs"]


def test_main_red_on_the_check_is_the_main_red_hold_and_no_bisect(tmp_path):
    r = _run(tmp_path, ["b0", "b1", "b2"], ["b1"], main_rc=1)
    assert r["probes"] == ["just lint crates=[] | "]                 # one run on main, nothing merged
    assert "MAIN_RED - just lint fail(s) on main itself" in r["needs"]
    assert r["queue"] == ["task-later", "b0", "b1", "b2"] and r["held"] == "SIG b0 b1 b2"
    assert r["attempts"] == []


def test_a_bisect_that_gives_up_keeps_the_suite_broken_hold(tmp_path):
    r = _run(tmp_path, ["b0", "b1", "b2", "b3"], ["b3"], merge_fail="b1")
    assert "SUITE_BROKEN - no test FAIL; just lint red on main+batch" in r["needs"]
    assert r["held"] == "SIG b0 b1 b2 b3" and r["queue"] == ["task-later", "b0", "b1", "b2", "b3"]
    assert "CHECK_PAIR" not in r["needs"] and r["attempts"] == [] and not r["bulk"]


def test_a_red_with_no_named_check_is_still_held_whole(tmp_path):
    """`just test` red with no FAIL (a doctest, a harness crash): no probe for it - SUITE_BROKEN as before."""
    r = _run(tmp_path, ["b0", "b1"], ["b1"], check="")
    assert r["probes"] == [] and "SUITE_BROKEN" in r["needs"] and r["held"] == "SIG b0 b1"


def _triage(tmp_path, gate_log):
    log = tmp_path / "merge-runner.log"
    log.write_text("[09-25 12:48:00] BULK gate (just gate --base abc ...)\n" + gate_log)
    script = f"""
set -u
LOG={log}; REPO={tmp_path}; GATE_PHASE=""; base=abc
log(){{ :; }}
{_function("_flake_retry")}
_flake_retry abc 1 "T-1 T-2"
echo "KIND=$TRIAGE_KIND CHECK=[$TRIAGE_CHECK] WHAT=[$TRIAGE_WHAT]"
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)
    assert "KIND=" in out.stdout, out.stderr
    return out.stdout.strip().splitlines()[-1]


def test_the_triage_names_the_check_from_the_gates_own_suite_line(tmp_path):
    assert _triage(tmp_path, "error[E0308]: mismatched types\ngate: just lint took 212s (exit 101)\n") \
        == "KIND=suite CHECK=[lint] WHAT=[just lint]"
    assert _triage(tmp_path, "gate: just lint took 20s (exit 0)\ngate: just test-ui took 40s (exit 1)\n") \
        == "KIND=suite CHECK=[test-ui] WHAT=[just test-ui]"
    assert _triage(tmp_path, "gate: just test took 600s (exit 101)\n") == "KIND=suite CHECK=[] WHAT=[]"
    assert _triage(tmp_path, "FAILED tests/test_board.py::test_a - x\ngate: just test took 60s (exit 1)\n") \
        == "KIND=suite CHECK=[] WHAT=[pytest red: tests/test_board.py::test_a]"
