"""ops/merge-runner.sh - every bisect/bulk-failure rewind is VERIFIED (reset_to_base). Incident 2026-09-26 03:07: in the lint
bisect of a 9-branch batch on 2a63ee04, bisect_red's `git reset -q --hard "$base"` after probe 3 did not take (no reflog
entry, stderr sent nowhere); the next probe gave up on "main moved", SUITE_BROKEN logged "rewound" without rewinding, and
the probe merge e88d6238 ('bisect probe (automated, never kept)', T-940) stayed on main UNGATED - the loop then skipped
T-940 as "already merged". The REAL functions run over a real git repo; `git reset` is made to fail on chosen calls
(an index.lock), everything else is real git.
"""

import pathlib
import re
import subprocess

RUNNER = pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh"
PROBE_MSG = "Merge {b} ({b}): bisect probe (automated, never kept)"


def _function(name: str) -> str:
    m = re.search(rf"^{name}\(\)\{{.*?^\}}\n", RUNNER.read_text(), re.M | re.S)
    assert m, f"{name} not found in {RUNNER}"
    return m.group(0)


def _fail_path() -> str:
    text = RUNNER.read_text()
    i = text.index("\ntry_bulk(){")
    start = text.index('  if [ "$(git -C "$REPO" rev-parse HEAD)" = "$after" ]; then', i)
    return text[start:text.index("\n}\n", start)]


def _repo(tmp_path, branches=("b0", "b1")):
    repo = tmp_path / "repo"

    def run(*a):
        return subprocess.run(["git", "-C", str(repo), *a], check=True, capture_output=True, text=True).stdout.strip()
    subprocess.run(["git", "init", "-q", "-b", "main", str(repo)], check=True)
    run("config", "user.email", "t@t")
    run("config", "user.name", "t")
    (repo / "f").write_text("0")
    run("add", "f")
    run("commit", "-qm", "base")
    for b in branches:
        run("checkout", "-qb", b, "main")
        (repo / b).write_text(b)
        run("add", b)
        run("commit", "-qm", b)
    run("checkout", "-q", "main")
    return repo, run


def _script(tmp_path, repo, body, fails="", always=False, culprit="b1"):
    """`git reset` fails (index.lock, HEAD unmoved) on the reset calls numbered in `fails`, or on every one."""
    s = tmp_path / "ops"
    s.mkdir(exist_ok=True)
    for f in ("queue", "needs"):
        (s / f).touch()
    return f"""
set -uo pipefail
REPO={repo}; S={s}; LOG={s}/log; QUEUE={s}/queue; NEEDS={s}/needs; BULKMARK={s}/bulk; RESET_RETRY_S=0
echo 0 > {s}/resets
log(){{ echo "LOG $*" >&2; }}
alert(){{ echo "ALERT $*" >> {s}/alerts; }}
ticket_of(){{ echo "$1"; }}
notify_coordinator(){{ echo "NOTIFY [$2] $1"; }}
record_attempt(){{ echo "$1 $2" >> {s}/attempts; }}
git(){{ if [ "$3" = reset ]; then
    local n=$(( $(cat {s}/resets) + 1 )); echo $n > {s}/resets
    if [ {int(always)} = 1 ] || case " {fails} " in *" $n "*) true;; *) false;; esac; then
      echo "fatal: Unable to create '$REPO/.git/index.lock': File exists." >&2; return 128; fi
  fi; command git "$@"; }}
just(){{ echo "just $* | $(ls {repo} | tr '\\n' ' ')" >> {s}/probes; [ -e {repo}/{culprit} ] && return 1; return 0; }}
uv(){{ return 0; }}
check_probe(){{ ( cd "$REPO" && HK_GATE_CRATES="" just "$TRIAGE_CHECK" ); }}
TRIAGE_KIND=suite; TRIAGE_CHECK=lint; TRIAGE_WHAT="just lint"; TRIAGE_FILTER=""; TRIAGE_SPECS=""; TRIAGE_TESTS=""
{_function("reset_to_base")}
{_function("bisect_red")}
{_function("bisect_fact")}
{_function("bisect_culprit")}
{_function("main_is_red")}
{_function("main_side_of")}
{_function("suite_broken_hold")}
{_function("bulk_dirty_stop")}
{body}
"""


def _go(tmp_path, repo, body, **kw):
    out = subprocess.run(["bash", "-c", _script(tmp_path, repo, body, **kw)], capture_output=True, text=True, timeout=60)
    s = tmp_path / "ops"

    def read(n):
        p = s / n
        return p.read_text() if p.exists() else ""
    return out.stdout + out.stderr, read, s


def test_a_reset_that_fails_once_is_retried_and_the_probe_goes_on(tmp_path):
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    out, read, s = _go(tmp_path, repo, f'bisect_red {base} b0={run("rev-parse", "b0")}; echo "RC=$?"', fails="1")
    assert "RC=1" in out                                          # a real verdict (green), not "gave up"
    assert run("rev-parse", "HEAD") == base                        # the probe merge is gone
    assert "index.lock': File exists" in out and "try 1 of 3" in out   # git's own words, logged
    assert not (s / "main-dirty").exists() and "MAIN_DIRTY" not in read("needs") and read("alerts") == ""


def test_a_reset_that_never_takes_is_main_dirty_and_nothing_gates_after_it(tmp_path):
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    out, read, s = _go(tmp_path, repo, f'bisect_red {base} b0={run("rev-parse", "b0")}; echo "RC=$?"', always=True)
    assert "RC=2" in out
    probe = run("rev-parse", "HEAD")
    assert probe != base and run("log", "-1", "--format=%s") == PROBE_MSG.format(b="b0")
    dirty = (s / "main-dirty").read_text()
    assert f"base={base}" in dirty and f"head={probe}" in dirty
    assert f"MAIN_DIRTY - main is at {probe[:8]}, not {base[:8]}" in read("needs")
    assert "ALERT red main left on an ungated commit" in read("alerts") and probe[:8] in read("alerts")
    assert "try 3 of 3" in out and "try 4" not in out


def test_the_incident_a_lint_bisect_whose_probe_reset_fails_stops_everything_and_never_says_rewound(tmp_path):
    """03:07: the reset after a green probe did not take. Now: MAIN_DIRTY, the batch re-queued, no further probe, no
    SUITE_BROKEN 'rewound' line, the bulk marker kept (main carries ungated commits)."""
    repo, run = _repo(tmp_path, ("b0", "b1", "b2"))
    base = run("rev-parse", "HEAD")
    for b in ("b0", "b1", "b2"):
        run("merge", "-q", "--no-ff", "-m", f"batch {b}", b)
    after = run("rev-parse", "HEAD")
    (tmp_path / "ops").mkdir()
    (tmp_path / "ops" / "bulk").write_text(f"base={base}\n")
    gated = " ".join(f"{b}={run('rev-parse', b)}" for b in ("b0", "b1", "b2"))
    # reset 1 = the bulk rewind; reset 2 = after probe {b0} (green) -> fails every try (calls 2, 3, 4).
    body = f"""fail(){{
local branches=(b0 b1 b2) gated=({gated}) base={base} after={after} tickets="b0 b1 b2" b wt rc tip
local gated_sig="SIG"
{_fail_path()}
}}
fail; echo "RC=$?" """
    out, read, s = _go(tmp_path, repo, body, fails="2 3 4")
    assert "RC=0" in out                                            # no isolation onto a dirty main
    assert (s / "main-dirty").exists() and "MAIN_DIRTY" in read("needs")
    assert read("queue").split() == ["b0", "b1", "b2"]
    # The pre-bisect "rewound to" line is true (that reset was verified); the SUITE_BROKEN one would not be.
    assert "FAILED without a test FAIL" not in out and "SUITE_BROKEN" not in read("needs")
    assert "main NOT rewound" in out
    assert len(read("probes").splitlines()) == 2                    # main alone, then {b0}; nothing after the failure
    assert (s / "bulk").exists()


def test_our_own_leftover_probe_on_main_is_reset_and_the_bisect_goes_on(tmp_path):
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    run("merge", "-q", "--no-ff", "-m", PROBE_MSG.format(b="b0"), "b0")      # what 03:07 left behind
    leftover = run("rev-parse", "HEAD")
    out, read, s = _go(tmp_path, repo, f'bisect_red {base} b1={run("rev-parse", "b1")}; echo "RC=$?"')
    assert "RC=0" in out                                            # the probe ran: b1 is red
    assert f"HEAD {leftover[:8]} is this runner's own leftover probe on {base[:8]}" in out
    assert "giving up" not in out
    assert run("rev-parse", "HEAD") == base
    assert "b0" not in read("probes")                                # the leftover was not part of the verdict
    assert not (s / "main-dirty").exists()


def test_a_foreign_commit_on_main_gives_up_as_before_and_nothing_is_reset(tmp_path):
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    run("merge", "-q", "--no-ff", "-m", "Merge b0 (b0): a person's merge", "b0")    # first parent IS base
    foreign = run("rev-parse", "HEAD")
    out, read, s = _go(tmp_path, repo, f'bisect_red {base} b1={run("rev-parse", "b1")}; echo "RC=$?"')
    assert "RC=2" in out and "main moved off" in out and "nothing reset" in out
    assert run("rev-parse", "HEAD") == foreign
    assert read("probes") == "" and not (s / "main-dirty").exists()


def test_suite_broken_says_rewound_only_when_main_is_on_base(tmp_path):
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    run("merge", "-q", "--no-ff", "-m", "x", "b0")
    body = f"""branches=(b0); gated_sig=SIG; tickets=b0; base={base}; suite_broken_hold"""
    out, _, _ = _go(tmp_path, repo, body)
    assert "rewound to" not in out and f"main is NOT on {base} (not rewound)" in out


def _loop_guard() -> str:
    text = RUNNER.read_text()
    start = text.index("  # MAIN_DIRTY (reset_to_base)")
    return text[start:text.index('  DIRTY_SAID=""', start)]


def test_the_loop_starts_no_gate_or_merge_while_main_dirty_stands(tmp_path):
    s = tmp_path / "ops"
    s.mkdir()
    (s / "main-dirty").write_text("base=aaaa\nhead=bbbb\n")
    script = f"""
set -uo pipefail
S={s}; sleep(){{ :; }}; log(){{ echo "LOG $*"; }}
for i in 1 2 3; do
{_loop_guard()}
  echo GATE
done
rm {s}/main-dirty
for i in 1; do
{_loop_guard()}
  echo GATE
done
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30).stdout
    assert out.count("GATE") == 1                                   # only after a person removed the marker
    assert out.count("HOLD: MAIN_DIRTY") == 1                       # said once, not every 8 s


def test_process_refuses_a_merge_while_main_dirty_stands(tmp_path):
    s = tmp_path / "ops"
    s.mkdir()
    (s / "main-dirty").write_text("x\n")
    script = f"""
set -uo pipefail
REPO={tmp_path}; S={s}; log(){{ echo "LOG $*"; }}; ticket_of(){{ echo T; }}
git(){{ case "$*" in *abbrev-ref*) echo main;; *merge*) echo MERGED;; esac; return 0; }}
{_function("process")}
process task-x; echo "RC=$?"
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30).stdout
    assert "WAIT task-x: MAIN_DIRTY" in out and "RC=1" in out and "MERGED" not in out
