"""Landings as release notes (user, 2026-09-23): one line per landed ticket, id AND title.

    python -m hkpy.landnotes --header "3 landed · gate 25 min" task-t608 task-t612 task-pm-x

prints a Discord-ready body:

    3 landed · gate 25 min
    - T-608 hk-blocks descramble: one generic LFSR (ADR-0011 s9.1)
    - T-612 <its title from the board>
    - task-pm-x — <its first commit subject>

A ticket's title comes from main's committed board (`git show main:docs/tasks.yaml`, the same
source the runners read); a branch that is not a ticket is named with the subject of its first
commit, found from the merge recorded in `$HACKRIFF_OPS/landed.jsonl` (the runner writes that
record before it alerts). The body stays under Discord's 2000-character message limit: lines that
do not fit become `+N more`. `ops/merge-runner.sh` posts it for every landing, and `hkpy.flow`'s
2-hourly digest carries the same list as "landed since last digest". Never raises: a missing board
or merge record degrades to the branch name.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys

OPS = os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops")
REPO = os.environ.get("HACKRIFF_REPO") or "/Users/daniellewis/hackriff"
LIMIT = 1900          # Discord allows 2000; alert.py adds a mention and a bold title


def _git(repo: str, *args: str) -> str:
    try:
        return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, timeout=30).stdout
    except Exception:
        return ""


def board_titles(repo: str = REPO) -> dict[str, str]:
    try:
        import yaml
        return {str(t.get("id")): str(t.get("title") or "").strip()
                for t in (yaml.safe_load(_git(repo, "show", "main:docs/tasks.yaml")) or {}).get("tasks", [])}
    except Exception:
        return {}


def ticket_of(branch: str) -> str:
    m = re.match(r"^task-t0*(\d+)$", branch, re.I)
    return f"T-{m.group(1)}" if m else branch


def merges(ops: str = OPS) -> dict[str, str]:
    """{branch: merge sha} from landed.jsonl, newest record winning."""
    out: dict[str, str] = {}
    try:
        for ln in open(os.path.join(ops, "landed.jsonl"), encoding="utf-8"):
            try:
                o = json.loads(ln)
            except ValueError:
                continue
            if o.get("branch") and o.get("merge"):
                out[o["branch"]] = o["merge"]
    except OSError:
        pass
    return out


def first_subject(repo: str, merge_sha: str) -> str:
    """The subject of the branch's first own commit: the oldest non-merge commit the merge brought."""
    log = _git(repo, "log", "--reverse", "--no-merges", "--format=%s", f"{merge_sha}^1..{merge_sha}^2")
    return log.splitlines()[0].strip() if log.strip() else ""


def line_for(branch: str, titles: dict[str, str], merge_of: dict[str, str], repo: str = REPO) -> str:
    t = ticket_of(branch)
    if t.startswith("T-"):
        return f"- {t} {titles.get(t) or '(not on the board)'}"
    subj = first_subject(repo, merge_of[branch]) if branch in merge_of else ""
    return f"- {branch} — {subj}" if subj else f"- {branch}"


def render(header: str, lines: list[str], limit: int = LIMIT) -> str:
    out, used = [header], len(header)
    for i, ln in enumerate(lines):
        rest = len(lines) - i
        tail = f"\n+{rest - 1} more" if rest > 1 else ""
        if used + 1 + len(ln) + len(tail) > limit:
            out.append(f"+{rest} more")
            break
        out.append(ln)
        used += 1 + len(ln)
    return "\n".join(out)


def notes(header: str, branches: list[str], ops: str = OPS, repo: str = REPO, limit: int = LIMIT) -> str:
    titles, merge_of = board_titles(repo), merges(ops)
    return render(header, [line_for(b, titles, merge_of, repo) for b in branches], limit)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="python -m hkpy.landnotes", description=__doc__.split("\n\n")[0])
    p.add_argument("--header", required=True)
    p.add_argument("branches", nargs="*")
    p.add_argument("--ops", default=OPS)
    p.add_argument("--repo", default=REPO)
    a = p.parse_args(argv)
    sys.stdout.write(notes(a.header, a.branches, a.ops, a.repo) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
