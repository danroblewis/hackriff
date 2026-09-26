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


def _script(tmp_path, repo, body, fails="", always=False, culprit="b1", silent=False):
    """`git reset` fails (index.lock, HEAD unmoved) on the reset calls numbered in `fails`, or on every one; `silent`
    makes the failure exit 0 with no message (a reset that simply did not take, as the 03:07 reflog shows)."""
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
      [ {int(silent)} = 1 ] && return 0
      echo "fatal: Unable to create '$REPO/.git/index.lock': File exists." >&2; return 128; fi
  fi; command git "$@"; }}
just(){{ echo "just $* | $(ls {repo} | tr '\\n' ' ')" >> {s}/probes; [ -e {repo}/{culprit} ] && return 1; return 0; }}
uv(){{ echo "uv $*" >> {s}/probes; return 0; }}
check_probe(){{ ( cd "$REPO" && HK_GATE_CRATES="" just "$TRIAGE_CHECK" ); }}
TRIAGE_KIND=suite; TRIAGE_CHECK=lint; TRIAGE_WHAT="just lint"; TRIAGE_FILTER=""; TRIAGE_SPECS=""; TRIAGE_TESTS=""
{_function("main_clean_on")}
{_function("try_reset")}
{_function("reset_to_base")}
{_function("own_commits_only")}
{_function("main_dirty_tick")}
{_function("suite_red_alone")}
{_function("suite_split")}
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
    needs = read("needs")
    assert f"MAIN_DIRTY - main is not clean on {base[:8]} (HEAD {probe[:8]})" in needs
    assert "clears itself once main is clean" in needs and "Do NOT just delete" in needs
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
main_dirty_tick(){{ [ -e {s}/clean ] && rm -f $S/main-dirty; [ -e {s}/clean ]; }}
for i in 1 2 3; do
{_loop_guard()}
  echo GATE
done
touch {s}/clean
for i in 1; do
{_loop_guard()}
  echo GATE
done
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30).stdout
    assert out.count("GATE") == 1                                   # only once main_dirty_tick lifted the hold
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


# --- review round (coordinator, 2026-09-26): clean = HEAD on base AND no merge staged AND no tracked edit; the hold
# clears itself; the retry resets only the runner's own commits; suite_split stops; the startup rewind is verified.

def _dirty(s, base):
    (s / "main-dirty").write_text(f"base={base}\nhead=x\n")


def test_a_staged_merge_on_base_is_not_clean(tmp_path):
    """A failed `merge --abort` leaves HEAD on base with MERGE_HEAD: that must not pass as rewound."""
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    run("merge", "-q", "--no-ff", "--no-commit", "-s", "ours", "b0")      # MERGE_HEAD, and no content diff at all
    out, read, s = _go(tmp_path, repo, f'reset_to_base {base} test; echo "RC=$?"', always=True)
    assert "RC=1" in out and "a merge staged" in out and (s / "main-dirty").exists()


def test_a_tracked_edit_on_base_is_not_clean(tmp_path):
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    (repo / "f").write_text("edited")
    out, read, s = _go(tmp_path, repo, f'reset_to_base {base} test; echo "RC=$?"', always=True)
    assert "RC=1" in out and (s / "main-dirty").exists()


def test_the_hold_clears_itself_once_main_is_clean_on_its_base(tmp_path):
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    s = tmp_path / "ops"
    s.mkdir()
    _dirty(s, base)
    (s / "bulk").write_text(f"base={base}\n")
    out, read, s = _go(tmp_path, repo, 'main_dirty_tick; echo "RC=$?"')
    assert "RC=0" in out and not (s / "main-dirty").exists() and not (s / "bulk").exists()
    assert f"ALERT amber main back on {base[:8]}, gates resume" in read("alerts")
    assert read("resets").strip() == "0"                            # nothing needed resetting


def test_the_retry_resets_the_runners_own_commits_and_lifts_the_hold(tmp_path):
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    run("merge", "-q", "--no-ff", "-m", "Merge b0 (b0): batch, gated together (automated, no AI)", "b0")
    run("merge", "-q", "--no-ff", "-m", PROBE_MSG.format(b="b1"), "b1")
    s = tmp_path / "ops"
    s.mkdir()
    _dirty(s, base)
    out, read, s = _go(tmp_path, repo, 'main_dirty_tick; echo "RC=$?"')
    assert "RC=0" in out and run("rev-parse", "HEAD") == base and not (s / "main-dirty").exists()


def test_the_retry_is_rate_limited(tmp_path):
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    run("merge", "-q", "--no-ff", "-m", PROBE_MSG.format(b="b0"), "b0")
    s = tmp_path / "ops"
    s.mkdir()
    _dirty(s, base)
    out, read, s = _go(tmp_path, repo, 'main_dirty_tick; main_dirty_tick; echo "RC=$?"', always=True)
    assert "RC=1" in out and read("resets").strip() == "1" and (s / "main-dirty").exists()


def test_a_persons_commit_on_main_is_never_reset_and_the_hold_stays(tmp_path):
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    run("merge", "-q", "--no-ff", "-m", PROBE_MSG.format(b="b0"), "b0")
    run("merge", "-q", "--no-ff", "-m", "Merge b1: a person's fix", "b1")
    head = run("rev-parse", "HEAD")
    s = tmp_path / "ops"
    s.mkdir()
    _dirty(s, base)
    out, read, s = _go(tmp_path, repo, 'main_dirty_tick; echo "RC=$?"')
    assert "RC=1" in out and run("rev-parse", "HEAD") == head and read("resets").strip() == "0"
    assert (s / "main-dirty").exists() and "not resetting; held for a person" in out


def test_suite_split_stops_probing_once_main_is_dirty(tmp_path):
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    (repo / "py").mkdir()                                           # suite_red_alone runs pytest from $REPO/py
    body = f'suite_split {base} "tests/x.py::a" b0={run("rev-parse", "b0")} b1={run("rev-parse", "b1")}; echo "RC=$?"'
    out, read, s = _go(tmp_path, repo, body, always=True)
    assert (s / "main-dirty").exists()                              # the reset after b0's probe did not take
    assert len(read("probes").splitlines()) == 2                    # base, then b0 - nothing probed on the dirty main
    assert "b1=" not in out


def _startup() -> str:
    text = RUNNER.read_text()
    start = text.index('if [ -f "$BULKMARK" ]; then\n  sbase=')
    return text[start:text.index("\nwhile true; do", start)]


def _boot(tmp_path, **kw):
    repo, run = _repo(tmp_path)
    base = run("rev-parse", "HEAD")
    run("merge", "-q", "--no-ff", "-m", "Merge b0 (b0): batch, gated together (automated, no AI)", "b0")
    s = tmp_path / "ops"
    s.mkdir()
    (s / "bulk").write_text(f"base={base}\nbranches=b0\n")
    _dirty(s, base)
    out, read, s = _go(tmp_path, repo, _startup(), **kw)
    return out, read, s, run, base


def test_the_startup_rewind_is_verified_and_clears_main_dirty(tmp_path):
    out, read, s, run, base = _boot(tmp_path)
    assert run("rev-parse", "HEAD") == base and "STARTUP: rewound a provisional bulk" in out
    assert not (s / "bulk").exists() and not (s / "main-dirty").exists() and read("queue").split() == ["b0"]


def test_a_startup_rewind_that_does_not_take_keeps_the_markers(tmp_path):
    out, read, s, run, base = _boot(tmp_path, always=True, silent=True)   # git says nothing and exits 0
    assert run("rev-parse", "HEAD") != base and "rewound a provisional" not in out
    assert "did NOT take" in out and (s / "bulk").exists() and (s / "main-dirty").exists()
    assert read("queue").split() == ["b0"]
