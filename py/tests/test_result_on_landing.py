"""A ticket result the work runner could not write on its branch (the branch's board lacked the ticket: 12 failures,
7 results lost on 2026-09-26) is written on main by the merge runner's flip_done, at landing."""

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
RUNNER = ROOT / "ops" / "merge-runner.sh"


def _flip(tmp_path, pending):
    repo, ops = tmp_path / "repo", tmp_path / "ops"
    g = lambda *a: subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t", *a],  # noqa: E731
                                  check=True, capture_output=True, text=True).stdout.strip()
    (repo / "docs").mkdir(parents=True)
    (repo / "py" / "hkpy").mkdir(parents=True)
    (repo / "py" / "hkpy" / "tasks.py").write_text("")
    (repo / "docs" / "tasks.yaml").write_text("tasks:\n  - id: T-1\n    title: a ticket\n    status: in-progress\n")
    g("init", "-q", "-b", "main")
    g("add", "-A")
    g("commit", "-q", "-m", "board")
    (ops / "work" / "T-1").mkdir(parents=True)
    (ops / "work" / "T-1" / "result.txt").write_text("DONE (work-runner, from handback.json). the summary\n")
    if pending:
        (ops / "work" / "T-1" / "result.pending").write_text("")
    fn = re.search(r"^flip_done\(\)\{.*?\n\}\n", RUNNER.read_text(), re.M | re.S).group(0)
    script = f"""S={ops}; REPO={repo}; LOG={ops}/log; log(){{ echo "LOG $*"; }}
git(){{ command git -c user.name=t -c user.email=t@t "$@"; }}
uv(){{ shift 5; PYTHONPATH={ROOT}/py {sys.executable} "$@"; }}
{fn}
flip_done T-1 abcdef1234
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True)
    return out.stdout + out.stderr, (repo / "docs" / "tasks.yaml").read_text(), g("log", "--format=%s"), ops


def test_a_pending_result_is_written_on_main_at_landing(tmp_path):
    out, board, log, ops = _flip(tmp_path, pending=True)
    assert "RESULT T-1 -> written on main" in out, out
    assert "status: done" in board and "the summary" in board
    assert log.splitlines()[0].startswith("Board: T-1 result from handback.json")
    assert not (ops / "work" / "T-1" / "result.pending").exists()


def test_without_a_pending_marker_the_landing_flips_status_only(tmp_path):
    out, board, log, _ = _flip(tmp_path, pending=False)
    assert "status: done" in board and "the summary" not in board and "RESULT" not in out, out
    assert log.splitlines()[0].startswith("Board: T-1 landed as abcdef12")
