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
