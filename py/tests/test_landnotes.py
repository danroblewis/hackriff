"""Landings as release notes (user, 2026-09-23): py/hkpy/landnotes.py, the runner's notify_ok, the digest."""

import json
import pathlib
import re
import subprocess
from datetime import datetime

from hkpy import flow, landnotes

RUNNER = pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh"


def _repo(tmp_path):
    """A repo whose main has a board and one merged non-ticket branch."""
    r = tmp_path / "repo"
    g = lambda *a: subprocess.run(["git", "-C", str(r), "-c", "user.name=t", "-c", "user.email=t@t", *a],  # noqa: E731
                                  check=True, capture_output=True, text=True).stdout.strip()
    r.mkdir()
    g("init", "-q", "-b", "main")
    (r / "docs").mkdir()
    (r / "docs" / "tasks.yaml").write_text("tasks:\n  - id: T-608\n    title: 'hk-blocks descramble: one generic LFSR (ADR-0011 s9.1)'\n")
    g("add", "-A")
    g("commit", "-q", "-m", "board")
    g("checkout", "-q", "-b", "task-pm-x")
    g("commit", "-q", "--allow-empty", "-m", "gate: the first thing this branch did")
    g("commit", "-q", "--allow-empty", "-m", "gate: a follow-up")
    g("checkout", "-q", "main")
    g("merge", "-q", "--no-ff", "-m", "Merge task-pm-x", "task-pm-x")
    return r, g("rev-parse", "HEAD")


def test_tickets_carry_their_board_title_and_branches_their_first_commit(tmp_path):
    repo, merge = _repo(tmp_path)
    ops = tmp_path / "ops"
    ops.mkdir()
    (ops / "landed.jsonl").write_text(json.dumps({"ticket": "task-pm-x", "branch": "task-pm-x", "merge": merge}) + "\n")
    body = landnotes.notes("3 landed · gate 25 min", ["task-t608", "task-pm-x", "task-t9999"], ops=str(ops), repo=str(repo))
    assert body.splitlines() == [
        "3 landed · gate 25 min",
        "- T-608 hk-blocks descramble: one generic LFSR (ADR-0011 s9.1)",
        "- task-pm-x — gate: the first thing this branch did",
        "- T-9999 (not on the board)",
    ]


def test_the_body_stays_under_discords_limit_with_a_count_of_the_rest():
    lines = [f"- T-{n} " + "x" * 80 for n in range(100)]
    body = landnotes.render("100 landed", lines, limit=1900)
    assert len(body) <= 1900 and body.splitlines()[-1].startswith("+") and body.endswith(" more")
    shown = len(body.splitlines()) - 2
    assert body.splitlines()[-1] == f"+{100 - shown} more"
    assert landnotes.render("1 landed", ["- T-1 a"]) == "1 landed\n- T-1 a"


def test_the_runner_posts_the_list_and_keeps_the_short_notice_for_the_coordinator(tmp_path):
    fn = re.search(r"^notify_ok\(\)\{.*?\n\}\n?|^notify_ok\(\)\{.*?; \}\n", RUNNER.read_text(), re.M | re.S).group(0)
    script = f"""REPO={tmp_path}; alert(){{ printf 'ALERT %s | %s\\n' "$2" "$3"; }}
tmux(){{ return 1; }}
uv(){{ echo "NOTES $*"; }}
{fn}
notify_ok "MERGED batch (T-1 T-2); queue now 0 waiting." "2 landed · gate 25 min" task-t1 task-t2
notify_ok "MERGED T-3 (task-t3); queue now 0 waiting."
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30).stdout
    first, second = out.strip().splitlines()
    assert first.startswith("ALERT landed | NOTES run --locked --project py python -m hkpy.landnotes --header 2 landed · gate 25 min task-t1 task-t2")
    assert second == "ALERT landed | MERGED T-3 (task-t3); queue now 0 waiting."     # no branches: old notice


def test_the_digest_lists_what_landed_since_the_last_one(tmp_path, monkeypatch):
    ops = tmp_path
    now = datetime(2026, 9, 23, 22)
    (ops / "landed.jsonl").write_text(
        json.dumps({"branch": "task-t608", "merge_ts": now.timestamp() - 600}) + "\n"
        + json.dumps({"branch": "task-t1", "merge_ts": now.timestamp() - 3 * 86400}) + "\n")      # too old
    monkeypatch.setattr(landnotes, "board_titles", lambda repo=None: {"T-608": "hk-blocks descramble"})
    s = {"ts": now.timestamp(), "at": "x", "landings_per_h_6h": 1.0, "landings_per_h_24h": 1.0, "reds_24h": 0,
         "gates_24h": 1, "real_reds_24h": 0, "flakes_24h": 0, "touchpoints_24h": 0}
    bodies = {}
    flow.digest(str(ops), s, now, lambda level, title, body, key: bodies.update({key: body}))
    assert "landed since last digest: 1\n- T-608 hk-blocks descramble" in bodies["flow:digest"]
