"""ops/merge-runner.sh restart-on-request: the runner re-executes itself only between gates."""

import pathlib
import re
import subprocess

RUNNER = pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh"


def _block() -> str:
    text = RUNNER.read_text()
    m = re.search(r'^  if \[ -e "\$S/merge-runner-restart" \].*?^  fi\n', text, re.M | re.S)
    assert m, "the restart block is gone from the main loop"
    # exec would replace the test's shell; the decision is what is under test
    return m.group(0).replace('exec bash "$REPO/ops/merge-runner.sh"', 'echo "EXEC $REPO/ops/merge-runner.sh"')


def run(tmp_path, marker=True, bulk=False, merge_head=False):
    ops, repo = tmp_path / "ops", tmp_path / "repo"
    (repo / ".git").mkdir(parents=True)
    ops.mkdir()
    if marker:
        (ops / "merge-runner-restart").write_text("flake-accept landed\n")
    if bulk:
        (ops / "bulk-in-progress").write_text("base=abc\n")
    if merge_head:
        (repo / ".git" / "MERGE_HEAD").write_text("deadbeef\n")
    script = f'S={ops}; REPO={repo}; log(){{ echo "LOG $*"; }}\n{_block()}echo "LOOP GOES ON"\n'
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)
    assert out.returncode == 0, out.stderr
    return out.stdout, (ops / "merge-runner-restart").exists()


def test_a_request_between_gates_re_executes_and_is_consumed(tmp_path):
    out, left = run(tmp_path)
    assert "RESTART: requested (flake-accept landed )" in out and "EXEC " in out and not left


def test_never_during_a_batch_or_a_staged_merge(tmp_path):
    for kw in ({"bulk": True}, {"merge_head": True}):
        out, left = run(tmp_path / str(kw), **kw)
        assert "EXEC" not in out and left and "LOOP GOES ON" in out, kw


def test_no_request_no_restart(tmp_path):
    out, _ = run(tmp_path, marker=False)
    assert "EXEC" not in out and "LOOP GOES ON" in out
