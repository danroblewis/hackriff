"""The merge runner's review hold (supervisor 2026-09-25 12:24, incident T-955).

At 12:20 T-955's tip moved (a board-conflict fix) while the branch sat in merge-queue.txt awaiting an Opus review
verdict outside the runner; the moved tip re-gated and landed review-FAILED code. A marker in
$HACKRIFF_OPS/review-hold/<branch> now keeps a queued branch out of every batch until it is removed.

The real `ready_filter` is cut out of ops/merge-runner.sh and run in bash against a real repo, as the other
merge-runner tests do; nothing touches the live queue.
"""
import pathlib
import re
import subprocess

RUNNER = pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh"


def _ready_filter() -> str:
    text = RUNNER.read_text()
    m = re.search(r"^ready_filter\(\)\{\n.*?^\}\n", text, re.S | re.M)
    assert m, "ready_filter not found"
    return m.group(0)


def _run(tmp_path, *branches):
    script = f"""set -u
REPO={tmp_path}/repo; S={tmp_path}/ops; NEEDS=$S/needs
log(){{ echo "LOG $*" >&2; }}
ticket_of(){{ echo "$1"; }}
alert(){{ :; }}
{_ready_filter()}
ready_filter {' '.join(branches)}
"""
    r = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=60)
    assert r.returncode == 0, r.stderr
    held = (tmp_path / "ops" / "pm-held").read_text().split() if (tmp_path / "ops" / "pm-held").exists() else []
    return r.stdout.split(), held, [ln for ln in r.stderr.splitlines() if "REVIEW HOLD" in ln]


def _repo(tmp_path):
    repo, ops = tmp_path / "repo", tmp_path / "ops"
    ops.mkdir()
    g = lambda *a: subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t", *a],  # noqa: E731
                                  check=True, capture_output=True, text=True)
    repo.mkdir()
    g("init", "-q", "-b", "main")
    g("commit", "-q", "--allow-empty", "-m", "base")
    for b in ("task-t955", "task-t956"):
        g("checkout", "-q", "-b", b, "main")
        g("commit", "-q", "--allow-empty", "-m", f"{b} work")
    g("checkout", "-q", "main")
    return ops


def test_a_branch_held_for_review_stays_queued_and_is_never_gated(tmp_path):
    ops = _repo(tmp_path)
    (ops / "review-hold").mkdir()
    (ops / "review-hold" / "task-t955").write_text("Opus review of the Sonnet round (coordinator 12:05)\n")
    ready, held, said = _run(tmp_path, "task-t955", "task-t956")
    assert ready == ["task-t956"] and held == ["task-t955"]            # not in the batch, back in the queue
    assert len(said) == 1 and "task-t955" in said[0]
    ready, held, said = _run(tmp_path, "task-t955", "task-t956")
    assert ready == ["task-t956"] and held == ["task-t955"] and said == []   # held again, said once
    (ops / "review-hold" / "task-t955").unlink()                          # the verdict is in
    ready, held, said = _run(tmp_path, "task-t955", "task-t956")
    assert ready == ["task-t955", "task-t956"] and held == []
