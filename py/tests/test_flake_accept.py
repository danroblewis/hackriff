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


def run(tmp_path, gate_log: str, kind="rust", names="hk-cli::api_contract t", second_s=60):
    log = tmp_path / "merge-runner.log"
    log.write_text("[09-23 17:00:00] BULK gate (just gate --base abc ...)\n" + gate_log)
    flaky = tmp_path / "flaky.jsonl"
    script = f"""
set -u
LOG={log}; FLAKY={flaky}; FLAKE_SECOND_S={second_s}
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
    out, recs = run(tmp_path, "gate: just lint took 26s (exit 0)\ngate: just test took 1700s (exit 100)\n")
    assert "LIMITED just gate --base abc --resume-after test\n" in out
    assert recs[-1]["accepted"] is True and recs[-1]["suite"] == "test" and recs[-1]["saved_s"] == 1726 - 60
    assert "saves ~27 min" in out and "ALERT amber flake accepted" in out and "RC 0" in out


def test_a_red_in_acceptance_also_runs_the_harness_it_never_reached(tmp_path):
    one = ("gate: just lint took 20s (exit 0)\ngate: just test took 900s (exit 0)\n"
           "gate: running just acceptance-ci\n     Summary [ 120.0s] 60 tests run: 59 passed, 1 failed\n"
           "gate: just acceptance-ci took 150s (exit 100)\n")
    out, _ = run(tmp_path, one)
    assert "--resume-after acceptance-ci --resume-steps e2e-harness" in out
    both = one.replace("gate: just acceptance-ci took", "     Summary [ 90.0s] 22 tests run: 21 passed, 1 failed\ngate: just acceptance-ci took")
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
