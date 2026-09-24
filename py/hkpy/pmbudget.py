"""The pipeline manager's scope check — enforced by the merge runner, not by prose.

WHY (user, 2026-09-23 17:30): "it's ok for the pipeline manager to make a lot of changes; I don't
want it to make a ton of weird changes that aren't warranted - it should stick to its directive:
improve the pipeline." Volume is not the problem; unwarranted scope is. So a `task-pm-*` branch
merges only if:

  1. it SAYS WHAT IT SERVES - a commit message on the branch carries one of
        Serves: E-<n>              an experiment in the ledger
        Serves: incident <what>    an incident the runner, watchdog or a person raised
        Serves: user <ask>         something the user asked for
        Serves: cost <measured>    a pipeline cost it measured - the line must carry a NUMBER
                                   ("cost: just test is 1800 s of every 45-min gate")
     A branch with no reason, or a "cost" with no number, is held for a person. "Improvement"
     is not a reason.
  2. it STAYS INSIDE THE PIPELINE - every file it touches is under a pipeline path (ops/, the
     hooks, the justfile, nextest config, the e2e harness, py/hkpy + tests, the role/rule/
     workflow/skill documents, ops docs). A path in crates/, ui/src, plugins/, tests/e2e or
     docs/tasks.yaml is the directive boundary (pipeline-invariants 18) and holds the branch.

A held branch is not refused for ever: `$HACKRIFF_OPS/pm-budget-ok/<branch>` (a person's word -
`just pm-budget release <branch>`) lets it through, and the runner re-queues it rather than
dropping it. Lines landed per day are REPORTED (`status`, and the manager's tick line), never
capped. The check is a pure function over git so it is testable; `ops/merge-runner.sh` calls
`check` in `ready_filter` for every `task-pm-*` name.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from datetime import datetime

OPS = os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops")
REPO = os.environ.get("HACKRIFF_REPO") or "/Users/daniellewis/hackriff"
PM_BRANCH = re.compile(r"^task-pm-")
_SERVES = re.compile(r"^\s*Serves:\s*(E-\d+|incident\b.*|user\b.*|cost\b.*)\s*$", re.I | re.M)

#: Where a pipeline branch may write. Anything else is product code or the board (invariant 18).
PIPELINE_PATHS = (
    "ops/", ".claude/hooks/", ".claude/skills/", ".claude/roles/", ".claude/rules/", ".claude/agents/",
    ".claude/pipeline/", ".claude/settings.json", "justfile", ".config/", "py/hkpy/", "py/tests/",
    "py/pyproject.toml", "py/uv.lock", "ui/e2e/", "ui/package.json", "ui/package-lock.json",
    "docs/ops-experiments.md", "docs/10-test-strategy.md", "ops/README.md", "CLAUDE.md",
)
#: Inside the allowed trees, these are still product assertions, not harness (invariant 10).
PRODUCT_INSIDE = re.compile(r"^ui/e2e/.*\.e2e\.mjs$")


def _git(repo: str, *args: str) -> str:
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, check=False).stdout


def serves(repo: str, base: str, branch: str) -> str | None:
    """The first `Serves:` line on the branch's own commits, or None."""
    body = _git(repo, "log", "--format=%B", f"{base}..{branch}")
    m = _SERVES.search(body)
    return m.group(1).strip() if m else None


def touched(repo: str, base: str, branch: str) -> list[str]:
    return [p for p in _git(repo, "diff", "--name-only", f"{base}...{branch}").splitlines() if p.strip()]


def out_of_scope(paths: list[str]) -> list[str]:
    bad = []
    for p in paths:
        if PRODUCT_INSIDE.match(p):
            bad.append(p)
        elif not any(p == a or p.startswith(a) for a in PIPELINE_PATHS):
            bad.append(p)
    return bad


def net_lines(repo: str, base: str, branch: str) -> int:
    out = _git(repo, "diff", "--shortstat", f"{base}...{branch}")
    ins = re.search(r"(\d+) insertion", out)
    dele = re.search(r"(\d+) deletion", out)
    return (int(ins.group(1)) if ins else 0) + (int(dele.group(1)) if dele else 0)


def landed_today(repo: str, now: datetime | None = None) -> tuple[int, int]:
    """(pipeline-manager merges today, their net lines) on main since midnight - reported, not capped."""
    now = now or datetime.now()
    since = now.strftime("%Y-%m-%d 00:00")
    merges = _git(repo, "log", "--merges", f"--since={since}", "--format=%H %s", "main").splitlines()
    count, lines = 0, 0
    for line in merges:
        sha, _, subject = line.partition(" ")
        if "task-pm-" not in subject:
            continue
        out = _git(repo, "diff", "--shortstat", f"{sha}^1", sha)
        ins = re.search(r"(\d+) insertion", out)
        dele = re.search(r"(\d+) deletion", out)
        count += 1
        lines += (int(ins.group(1)) if ins else 0) + (int(dele.group(1)) if dele else 0)
    return count, lines


def released(ops: str, branch: str) -> bool:
    return os.path.exists(os.path.join(ops, "pm-budget-ok", branch))


def check(repo: str, ops: str, base: str, branch: str) -> tuple[bool, str]:
    """(ok, one-line reason). Only `task-pm-*` branches are ever held; everything else is ok."""
    if not PM_BRANCH.match(branch):
        return True, "not a pipeline-manager branch"
    if released(ops, branch):
        return True, "released by a person (pm-budget-ok)"
    n = net_lines(repo, base, branch)
    bad = out_of_scope(touched(repo, base, branch))
    if bad:
        return False, (f"touches product code or the board, outside the pipeline directive: {', '.join(bad[:6])}"
                       f"{' …' if len(bad) > 6 else ''} ({n} lines); held for a person")
    why = serves(repo, base, branch)
    if why is None:
        return False, f"no `Serves:` line on its commits ({n} lines) - E-<n>, incident <what>, user <ask> or cost <measured>; held for a person"
    if why.lower().startswith("cost") and not re.search(r"\d", why):
        return False, f"`Serves: {why}` names a cost with no number in it ({n} lines) - a measured cost has a measurement; held for a person"
    return True, f"Serves: {why} ({n} lines, {len(touched(repo, base, branch))} files, all pipeline paths)"


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="just pm-budget", description=__doc__.split("\n\n")[0])
    p.add_argument("--repo", default=REPO)
    p.add_argument("--ops", default=OPS)
    sub = p.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("check")
    c.add_argument("branch")
    c.add_argument("--base", default="main")
    r = sub.add_parser("release")
    r.add_argument("branch")
    sub.add_parser("status")
    a = p.parse_args(argv)
    if a.cmd == "check":
        ok, why = check(a.repo, a.ops, a.base, a.branch)
        print(f"pm-budget {a.branch}: {'ok' if ok else 'HELD'} - {why}")
        return 0 if ok else 1
    if a.cmd == "release":
        os.makedirs(os.path.join(a.ops, "pm-budget-ok"), exist_ok=True)
        open(os.path.join(a.ops, "pm-budget-ok", a.branch), "w").write(datetime.now().isoformat() + "\n")
        print(f"pm-budget: {a.branch} released; the runner re-queues held branches on its own")
        return 0
    count, lines = landed_today(a.repo)
    print(f"pm-budget today: {count} pipeline-manager branch(es) landed, {lines} net lines (reported, not capped)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
