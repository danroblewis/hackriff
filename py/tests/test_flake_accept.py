"""ops/merge-runner.sh `flake_accept` - the user's rule (2026-09-23): a red test that passes alone
twice is a load flake, its suite passes on that evidence, and the gate resumes after the suite
that stopped it. The REAL function text is extracted from the script and run in bash against a
synthetic gate log, with its side effects (log, limited, alert) stubbed to print what they got.
"""

import json
import pathlib
import re
import subprocess

RUNNER = pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh"


def _function(name: str) -> str:
    text = RUNNER.read_text()
    m = re.search(rf"^{name}\(\)\{{.*?^\}}\n", text, re.M | re.S)
    assert m, f"{name} not found in {RUNNER}"
    return m.group(0)


#: What really follows a red in the log: the runner's TRIAGE lines and the two isolated re-runs,
#: each printing its OWN nextest summary (review, 2026-09-23 - a fixture without them hid a skip).
ISOLATED = ("[09-23 17:40:00] TRIAGE: re-running the failing tests alone: t\n"
            "     Summary [   1.2s] 1 test run: 1 passed\n"
            "[09-23 17:41:00] TRIAGE: first isolated run passed - running them alone once more (the rule is twice)\n"
            "     Summary [   1.1s] 1 test run: 1 passed\n")


def run(tmp_path, gate_log: str, kind="rust", names="t", second_s=60, t0=""):
    log = tmp_path / "merge-runner.log"
    log.write_text("[09-23 17:00:00] BULK gate (just gate --base abc ...)\n" + gate_log)
    flaky = tmp_path / "flaky.jsonl"
    script = f"""
set -u
LOG={log}; FLAKY={flaky}; FLAKE_SECOND_S={second_s}; TRIAGE_T0={t0!r}
log(){{ echo "LOG $*"; }}
limited(){{ echo "LIMITED $*"; return 0; }}
alert(){{ echo "ALERT $1 $2"; }}
{_function("flake_accept")}
flake_accept {kind} "{names}" 1 "T-1 T-2" "just gate --base abc"
echo "RC $?"
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)
    assert out.returncode == 0, out.stderr
    recs = [json.loads(ln) for ln in flaky.read_text().splitlines()] if flaky.exists() else []
    return out.stdout, recs


def test_a_rust_red_resumes_after_the_workspace_suite_and_counts_what_the_old_retry_cost(tmp_path):
    out, recs = run(tmp_path, "gate: just lint took 26s (exit 0)\n     Summary [1600.0s] 2802 tests run: 2801 passed, 1 failed\n"
                              "gate: just test took 1700s (exit 100)\n")
    assert "LIMITED just gate --base abc --resume-after test\n" in out
    assert recs[-1]["accepted"] is True and recs[-1]["suite"] == "test" and recs[-1]["saved_s"] == 1726 - 60
    assert "saves ~27 min" in out and "ALERT amber flake accepted" in out and "RC 0" in out


def test_a_red_in_acceptance_also_runs_the_harness_it_never_reached(tmp_path):
    one = ("gate: just lint took 20s (exit 0)\ngate: just test took 900s (exit 0)\n"
           "gate: running just acceptance-ci\n     Summary [ 120.0s] 60 tests run: 59 passed, 1 failed\n"
           "gate: just acceptance-ci took 150s (exit 100)\n") + ISOLATED
    out, _ = run(tmp_path, one)
    assert "--resume-after acceptance-ci --resume-steps e2e-harness" in out
    # the red was in the harness this time: acceptance passed, then e2e-harness ran and failed
    both = one.replace("60 tests run: 59 passed, 1 failed", "60 tests run: 60 passed").replace(
        "gate: just acceptance-ci took", "     Summary [ 90.0s] 22 tests run: 21 passed, 1 failed\ngate: just acceptance-ci took")
    out, _ = run(tmp_path, both)
    assert "--resume-after acceptance-ci\n" in out and "--resume-steps" not in out


def test_a_browser_red_counts_the_acceptance_phase_the_old_rule_re_ran(tmp_path):
    out, recs = run(tmp_path, "gate: just test took 900s (exit 0)\ngate: just acceptance-ci took 300s (exit 0)\n"
                              "gate: just test-ui-e2e took 450s (exit 1)\n", kind="spec", names="fog-of-war.e2e.mjs")
    assert "--resume-after test-ui-e2e" in out
    assert recs[-1]["kind"] == "spec" and recs[-1]["saved_s"] == 750 - 60


def test_without_a_stopped_suite_in_the_log_it_falls_back_to_the_full_retry(tmp_path):
    out, recs = run(tmp_path, "some output with no gate lines\n")
    assert "LIMITED just gate --base abc\n" in out and "--resume-after" not in out
    assert recs[-1]["accepted"] is False



def test_a_red_the_isolated_runs_did_not_cover_is_never_accepted(tmp_path):
    """A crash beside a flaky test: the stopped run counted 2 failures, one was re-run alone."""
    log = ("gate: just lint took 20s (exit 0)\n"
           "     Summary [1500.0s] 2802 tests run: 2800 passed, 2 failed\n"
           "gate: just test took 1600s (exit 100)\n") + ISOLATED
    out, recs = run(tmp_path, log, names="flaky_one")
    assert "not accepted" in out and "LIMITED" not in out and "RC 1" in out and recs == []
    out, _ = run(tmp_path, log.replace("2 failed", "1 failed, 1 timed out"), names="flaky_one")
    assert "RC 1" in out
    out, _ = run(tmp_path, log, names="flaky_one crashing_two")
    assert "RC 0" in out and "--resume-after test" in out


def test_the_record_carries_the_reds_own_time(tmp_path):
    _, recs = run(tmp_path, "     Summary [1.0s] 9 tests run: 8 passed, 1 failed\ngate: just test took 900s (exit 100)\n",
                  t0="2026-09-23T17:00:05")
    assert recs[-1]["ts"] == "2026-09-23T17:00:05"


def test_a_red_in_the_resumed_gate_is_triaged_too_and_main_red_uses_its_own_reds(tmp_path):
    """2026-09-24 10:36-10:47: a Rust red passed alone twice and was accepted; the resumed gate then
    went red on fog-of-war.e2e.mjs, which got no re-run - straight to isolating 11 branches - and the
    MAIN-IS-RED check re-ran the accepted Rust test's filter instead of the spec."""
    log = tmp_path / "merge-runner.log"
    log.write_text("[09-24 10:15:34] BULK gate (just gate --base abc ...)\n"
                   "gate: just lint took 40s (exit 0)\n"
                   "        FAIL [   4.159s] (2681/3003) hk-cli::api_contract t_band\n"
                   "gate: just test took 1231s (exit 100)\n")
    (tmp_path / "ui").mkdir()
    calls = tmp_path / "calls"
    script = f"""
set -u
LOG={log}; FLAKY={tmp_path}/flaky.jsonl; REPO={tmp_path}
log(){{ echo "[09-24 10:40:00] $*" >> $LOG; echo "LOG $*"; }}
alert(){{ :; }}
cargo(){{ echo "cargo $*" >> {calls}; return 0; }}          # the Rust flake passes alone
npm(){{ echo "npm $*" >> {calls}; return 0; }}              # ... and so does the browser spec
N=0
limited(){{
  N=$((N+1)); echo "limited $*" >> {calls}
  if [ $N -eq 1 ]; then                                     # the resumed gate: red in the browser tier
    printf 'gate: just acceptance-ci took 213s (exit 0)\\ne2e: 14/15 files passed in 237.3 s (backend 4.2 s); failed: fog-of-war.e2e.mjs\\ngate: just test-ui-e2e took 300s (exit 1)\\n' >> $LOG
    return 1
  fi
  return 0
}}
{_function("_flake_retry")}
{_function("flake_accept")}
_flake_retry abc 1 "T-1 T-2" "just gate --base abc"
echo "RC $? FILTER=[$TRIAGE_FILTER] SPECS=[$TRIAGE_SPECS]"
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)
    assert out.returncode == 0, out.stderr
    ran = calls.read_text().splitlines()
    assert [c for c in ran if c.startswith("npm")] == ["npm run e2e -- fog-of-war.e2e.mjs"] * 2   # re-run alone, twice
    assert ran[-1] == "limited just gate --base abc --resume-after test-ui-e2e"
    assert "RC 0 FILTER=[] SPECS=[fog-of-war.e2e.mjs]" in out.stdout


def test_the_main_is_red_rerun_treats_a_test_main_lacks_as_green():
    """09-24 10:15: T-870's own new test was re-run on the rewound main, where it does not exist;
    nextest's "no tests to run" exit (4) read as red -> MAIN IS RED, and T-870 was never isolated."""
    text = RUNNER.read_text()
    i = text.index("TRIAGE: is main itself red? re-running the failing tests alone on main")
    rerun = next(ln for ln in text[i:].splitlines() if "cargo nextest run" in ln)
    assert "--no-tests=pass" in rerun


def _main_is_red(tmp_path, filt="", specs="", kind="test", cargo_rc=0, npm_rc=0):
    calls = tmp_path / "calls"
    script = f"""
set -u
LOG={tmp_path}/log; REPO={tmp_path}; mkdir -p {tmp_path}/ui
log(){{ echo "LOG $*"; }}
cargo(){{ echo "cargo $*" >> {calls}; [ "$2" = build ] && return 0; return {cargo_rc}; }}
npm(){{ echo "npm $*" >> {calls}; [ "$2" = build ] && return 0; return {npm_rc}; }}
TRIAGE_KIND={kind}; TRIAGE_FILTER={filt!r}; TRIAGE_SPECS={specs!r}
{_function("main_is_red")}
main_is_red; echo "RC $? WHAT=[$MAIN_RED_WHAT]"
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)
    assert out.returncode == 0, out.stderr
    return out.stdout, (calls.read_text().splitlines() if calls.exists() else [])


def test_main_is_red_answers_for_this_triages_own_reds(tmp_path):
    """2026-09-24 11:19-11:30: T-858, T-802, T-803 each gated alone and were blamed for app-trace's
    T-475 check, which main itself failed - the single-branch path never asked main."""
    out, calls = _main_is_red(tmp_path, specs="app-trace.e2e.mjs", npm_rc=1)
    assert "RC 0 WHAT=[browser spec(s) app-trace.e2e.mjs]" in out
    assert calls[-1] == "npm run e2e -- app-trace.e2e.mjs" and any("build" in c for c in calls)   # rebuilt first
    out, _ = _main_is_red(tmp_path, specs="app-trace.e2e.mjs", npm_rc=0)
    assert "RC 1 WHAT=[]" in out
    out, calls = _main_is_red(tmp_path, filt="test(t_band)", cargo_rc=100)
    assert "RC 0 WHAT=[test(t_band)]" in out and "--no-tests=pass" in calls[-1]
    out, calls = _main_is_red(tmp_path / "s", filt="test(x)", kind="suite")
    assert "RC 1 WHAT=[]" in out and calls == []                          # lint/build: nothing to ask


def test_a_single_branch_red_that_main_shares_is_not_charged_and_stops_an_isolation():
    text = RUNNER.read_text()
    single = text[text.index("    git merge --abort 2>/dev/null || true\n    if main_is_red; then"):]
    single = single[:single.index("record_attempt")]
    assert "return 1" in single and "MAIN_RED_STOP=1" in single           # re-queued by the caller, no attempt
    assert '>> "$S/main-red-parked"' in single                               # ... and parked until main moves
    loop = text[text.index('        MAIN_RED_STOP=""'):]
    assert 'if [ -n "$MAIN_RED_STOP" ]; then echo "$b" >> "$QUEUE"; continue; fi' in loop[:loop.index("\n        done")]


def _park_block() -> str:
    text = RUNNER.read_text()
    i = text.index("    # A branch whose red main shares")
    j = text.index("\n    fi\n", text.index('log "PARK: $(echo $parked)', i)) + len("\n    fi\n")
    return text[i:j]


def _park(tmp_path, parked_lines, head, ready):
    (tmp_path / "main-red-parked").write_text(parked_lines)
    q = tmp_path / "queue"
    q.write_text("")
    script = f"""
set -u
S={tmp_path}; QUEUE={q}; REPO={tmp_path}
log(){{ echo "LOG $*"; }}
sleep(){{ :; }}
git(){{ case "$*" in *"rev-parse HEAD"*) echo {head};; *) echo tipA;; esac; }}
ready="{ready}"; GATED=none
for _ in 1; do
{_park_block()}
GATED="$ready"
done
echo "GATED=[$GATED]"
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)
    assert out.returncode == 0, out.stderr
    return out.stdout, q.read_text().split(), (tmp_path / "main-red-parked").exists()


def test_a_branch_main_is_red_on_waits_until_main_moves(tmp_path):
    """Review 2026-09-24: re-queued on MAIN IS RED, a lone branch re-gated every tick until main was
    fixed. Parked, it waits; the rest of the queue (a fix for main among it) still gates."""
    out, queued, still = _park(tmp_path, "task-t802 aaa tipA\n", "aaa", "task-t802 task-fix")
    assert "GATED=[task-fix]" in out and queued == ["task-t802"] and still
    out, queued, still = _park(tmp_path, "task-t802 aaa tipA\n", "aaa", "task-t802")
    assert "GATED=[none]" in out and queued == ["task-t802"]                 # nothing else: no gate at all
    out, queued, still = _park(tmp_path, "task-t802 aaa tipA\n", "bbb", "task-t802")
    assert "GATED=[task-t802]" in out and queued == [] and not still          # main moved: it gates again
    out, queued, still = _park(tmp_path, "task-t802 aaa tipOLD\n", "aaa", "task-t802")
    assert "GATED=[task-t802]" in out and queued == []                         # its own tip moved: a fix, it gates


def _solo_run(tmp_path, solo_answer, knob="1", alone_rcs=(0, 0), code_changed=False):
    """A browser red in test-ui-e2e, re-run alone by the real _flake_retry/flake_accept/solo_ok."""
    log = tmp_path / "merge-runner.log"
    log.write_text("[09-24 14:30:00] BULK gate (just gate --base abc ...)\n"
                   "gate: just test-ui took 8s (exit 0)\n"
                   "e2e: 15/16 files passed in 258.5 s (backend 2.5 s); failed: app-trace.e2e.mjs\n"
                   "gate: just test-ui-e2e took 259s (exit 1)\n")
    (tmp_path / "ui").mkdir(exist_ok=True)
    calls = tmp_path / "calls"
    rcs = " ".join(str(r) for r in alone_rcs)
    script = f"""
set -u
LOG={log}; FLAKY={tmp_path}/flaky.jsonl; REPO={tmp_path}; FLAKE_SOLO_ONE={knob}; BULKMARK={tmp_path}/no-bulk
git(){{ echo "git $*" >> {calls}; return {1 if code_changed else 0}; }}      # `git diff --quiet` of the acceptance code
log(){{ echo "[09-24 14:40:00] $*" >> $LOG; echo "LOG $*"; }}
alert(){{ :; }}
RCS=({rcs}); N=0
npm(){{ echo "npm $*" >> {calls}; local r=${{RCS[$N]:-0}}; N=$((N+1)); return $r; }}
uv(){{ echo "uv $*" >> {calls}; echo "{solo_answer}"; case "{solo_answer}" in solo-ok*) return 0;; *) return 1;; esac; }}
limited(){{ echo "limited $*" >> {calls}; return 0; }}
{_function("judge_changed")}
{_function("solo_ok")}
{_function("_flake_retry")}
{_function("flake_accept")}
_flake_retry abc 1 "T-1" "just gate --base abc"
echo "RC $?"
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)
    assert out.returncode == 0, out.stderr
    ran = calls.read_text().splitlines() if calls.exists() else []
    recs = [json.loads(ln) for ln in (tmp_path / "flaky.jsonl").read_text().splitlines()] if (tmp_path / "flaky.jsonl").exists() else []
    return out.stdout, ran, recs


def test_a_ledger_known_flaker_is_accepted_after_one_solo_pass(tmp_path):
    """User decision 2026-09-24 14:20 (knob FLAKE_SOLO_ONE)."""
    out, ran, recs = _solo_run(tmp_path, "solo-ok 11")
    assert [c for c in ran if c.startswith("npm")] == ["npm run e2e -- app-trace.e2e.mjs"]          # ONE solo run
    assert "accepted after one solo pass (ledger: 11 alone-passes)" in out and "PASS alone once" in out
    assert ran[-1] == "limited just gate --base abc --resume-after test-ui-e2e" and "RC 0" in out
    assert recs[-1]["passes_alone"] == 1 and recs[-1]["accepted"] is True


def test_a_first_time_flaker_or_the_knob_off_keeps_the_twice_rule(tmp_path):
    for answer, knob in (("solo-no 1", "1"), ("solo-ok 11", "0")):
        d = tmp_path / f"{answer[:7]}{knob}"
        d.mkdir()
        out, ran, recs = _solo_run(d, answer, knob=knob)
        assert [c for c in ran if c.startswith("npm")] == ["npm run e2e -- app-trace.e2e.mjs"] * 2
        assert "PASS alone twice" in out and recs[-1]["passes_alone"] == 2
        assert knob == "1" or not any(c.startswith("uv") for c in ran)                    # off: the ledger is not even asked


def test_a_fail_alone_is_a_real_red_whatever_the_ledger_says(tmp_path):
    out, ran, recs = _solo_run(tmp_path, "solo-ok 11", alone_rcs=(1,))
    assert "FAILS alone -> a real defect" in out and "RC 1" in out and recs == []
    assert not any(c.startswith("uv") for c in ran)                                          # asked only after a pass


def test_a_merge_that_changes_the_acceptance_code_keeps_the_twice_rule(tmp_path):
    """Review 2026-09-24: a batch editing hkpy/flakes.py or the runner would decide its own flake accept."""
    out, ran, recs = _solo_run(tmp_path, "solo-ok 11", code_changed=True)
    assert [c for c in ran if c.startswith("npm")] == ["npm run e2e -- app-trace.e2e.mjs"] * 2
    assert any("diff --quiet HEAD -- py/hkpy py/pyproject.toml py/uv.lock ops/merge-runner.sh" in c for c in ran)
    assert not any(c.startswith("uv") for c in ran) and recs[-1]["passes_alone"] == 2


def test_main_side_of_asks_the_ledger_with_the_triaged_names(tmp_path):
    calls = tmp_path / "calls"

    def run(specs, tests):
        script = f"""
set -u
REPO={tmp_path}
uv(){{ echo "uv $*" >> {calls}; echo "main-side x: task-t1"; }}
TRIAGE_SPECS={specs!r}; TRIAGE_TESTS={tests!r}
{_function("main_side_of")}
main_side_of task-t9
"""
        return subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30).stdout

    assert run("canvas-journey.e2e.mjs", "") == "main-side x: task-t1\n"
    assert calls.read_text().splitlines()[-1].endswith("--main-side canvas-journey.e2e.mjs --branch task-t9")
    run("", "hk-pipeline::sweep_residency a_sustained hk-api::x b_c")       # the ledger's full nextest names
    assert calls.read_text().splitlines()[-1].endswith("--main-side hk-pipeline::sweep_residency a_sustained hk-api::x b_c --branch task-t9")
    n = len(calls.read_text().splitlines())
    assert run("", "") == "" and len(calls.read_text().splitlines()) == n   # nothing triaged: not asked


def test_a_single_branch_main_side_red_is_held_once_per_tip_not_charged():
    text = RUNNER.read_text()
    block = text[text.index('    local side=""; grep -qx "$branch $tip" "$S/main-side-seen"'):]
    block = block[:block.index("record_attempt")]
    assert block.splitlines()[1].strip() == 'if [ -n "$side" ]; then'                   # the hold is what the verdict gates
    assert 'echo "$branch $tip" >> "$S/main-side-seen"' in block                     # the second time on this tip is charged
    assert '>> "$S/main-red-parked"' in block and "return 1" in block and "BLOCKED_ON_SPEC" in block
    assert '"main-side defect - deflaker/ticket needed"' in block
