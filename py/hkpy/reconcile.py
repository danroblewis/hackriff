"""Reconcile `docs/tasks.yaml`'s in-progress tickets against what git actually shows.

An `in-progress` ticket is a CLAIM ABOUT THE WORLD: someone is working on it right now. The claim
goes stale in ways nobody notices, because nothing in the normal flow re-checks it:

  * a branch is merged and the board is never flipped to `done` — the ticket is finished and still
    reads as running (T-300, T-364, T-426, T-458 all did this);
  * an agent is lost — killed, timed out, or its session ended — and its branch sits with real work
    on it, or with none at all, while the board says it is in hand.

Both look identical from the board and completely different from git, so this is a pure function of
git plus the board and belongs in a runner rather than in a coordinator's memory. Same principle as
T-396 moving the which-suites decision into `just gate`: the rule gets applied the same way whoever
is at the keyboard, and it is checkable.

What it reports per in-progress ticket, all read from git, never guessed:

  MERGED       main carries a merge commit naming the branch's tip as a parent -> the work is ON
               MAIN and the ticket should be `done`. This is the loudest case and the one that has
               recurred. Note it is NOT "ancestor of main": a branch freshly cut from main is an
               ancestor too, and the first draft of this tool reported three live worktrees as
               merged for exactly that reason.
  AHEAD n      the branch has n commits main does not -> finished-or-abandoned work waiting for a
               gate. Needs a human decision: gate and merge it, or relaunch.
  WORKING n    the branch has no commits, but its worktree has n uncommitted files -> an agent is
               mid-task. Reported separately from NO WORK because an agent that has not committed
               YET and an agent that was lost BEFORE committing look identical at the branch and
               completely different in the worktree. Calling a busy agent "no work" would be the
               same false claim this tool exists to catch.
  NO WORK      the branch exists, with no commits and a clean worktree -> nothing has been done.
  LANDED       no branch, but main carries a commit naming this ticket -> it was merged and its
               branch was cleaned up. THIS IS THE ONE THAT KEPT PRODUCING PHANTOMS: removing a
               worktree after a merge is the documented reclaim step, so a finished ticket ends up
               looking exactly like one that never started. Reported loudly for that reason.
  NO BRANCH    no branch and nothing on main names it -> nothing was ever started.

Staleness is the age of the newest commit on the branch (or of the worktree's newest modified file
when nothing is committed yet), because "in-progress and untouched for hours" is the signal that
separates a live agent from a lost one. It is a HINT, not a verdict: a long-running agent that is
reading rather than writing is legitimately quiet, which is why this prints evidence and refuses to
draw the conclusion itself.

EVERY READING HERE ASSUMES `main` IS AUTHORITATIVE, AND DURING A BULK MERGE IT IS NOT. The merge
runner commits each branch of a batch as it goes and only rewinds if the ~20-minute gate then fails,
so for that whole window main carries commits that have not passed a gate. `ahead=0` in that window
means "currently merged", NOT "durably merged". On 2026-09-21 that distinction cost three incidents:
a branch deleted on an ahead=0 read that had to be recovered from a merge commit's second parent, a
batch announced as landed from `git log --merges` that the gate then rewound, and a worker branch cut
from staged-but-ungated main that silently absorbed three other tickets' work. The runner therefore
writes `$HACKRIFF_OPS/bulk-in-progress` for exactly that window, and this tool leads with it — a
warning ABOVE the findings, because it invalidates all of them at once rather than qualifying any one.

Exit status is 0 unless `--strict`, which exits 1 when anything needs attention, so a caller can
gate on it. Stdlib only, like `hkpy.gate`.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BOARD = REPO / "docs" / "tasks.yaml"

#: `- id: T-123` followed, somewhere in the same block, by `status: <x>`.
_ID = re.compile(r"^  - id: (T-\d+)$", re.M)
_STATUS = re.compile(r"^    status: (\S+)$", re.M)
_TITLE = re.compile(r"^    title: (.*)$", re.M)


def _git(*args: str) -> str:
    """Run a git command in the repo and return stripped stdout ('' on failure)."""
    try:
        out = subprocess.run(
            ["git", "-C", str(REPO), *args],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return ""
    return out.stdout.strip() if out.returncode == 0 else ""


@dataclass(frozen=True)
class Ticket:
    id: str
    status: str
    title: str


def tickets(text: str) -> list[Ticket]:
    """Every ticket on the board, parsed textually.

    Deliberately NOT via PyYAML: this must run when the board has a merge conflict in it, which is
    exactly when a coordinator most wants to know what is in flight. A conflicted file does not
    parse, and refusing to answer then would make the tool useless at its most useful moment.
    """
    out: list[Ticket] = []
    marks = [(m.start(), m.group(1)) for m in _ID.finditer(text)]
    for n, (start, tid) in enumerate(marks):
        end = marks[n + 1][0] if n + 1 < len(marks) else len(text)
        block = text[start:end]
        st = _STATUS.search(block)
        ti = _TITLE.search(block)
        out.append(Ticket(tid, st.group(1) if st else "?", (ti.group(1) if ti else "").strip()))
    return out


def branch_name(tid: str) -> str:
    """The branch name this repo's convention gives `T-123`: `task-t123`.

    Pure, and separate from whether it exists, so the naming rule can be tested without a repo.
    """
    return "task-t" + tid.split("-")[1].lstrip("0")


def branch_for(tid: str) -> str | None:
    """`branch_name(tid)` if that branch exists in this repo, else None."""
    name = branch_name(tid)
    return name if _git("rev-parse", "--verify", "--quiet", name) else None


def _worktree_dirty(branch: str) -> int:
    """How many files are uncommitted in the worktree checked out at `branch` (0 if none/absent)."""
    for line in _git("worktree", "list", "--porcelain").split("\n\n"):
        if f"branch refs/heads/{branch}" in line:
            path = next(
                (ln.split(" ", 1)[1] for ln in line.splitlines() if ln.startswith("worktree ")), None
            )
            if not path:
                return 0
            out = subprocess.run(
                ["git", "-C", path, "status", "--porcelain"],
                capture_output=True,
                text=True,
                check=False,
            )
            if out.returncode != 0:
                return 0
            return len([ln for ln in out.stdout.splitlines() if ln and not ln.startswith("??")])
    return 0


def _merged_via_commit(branch: str) -> bool:
    """True when main carries a merge commit naming this branch's tip as a parent.

    `--is-ancestor` answers reachability, which a branch freshly cut from main also satisfies. This
    answers whether the branch was actually MERGED, by looking for its tip among the parents of
    main's merge commits — which is what `git merge --no-ff` writes and what a fresh branch has
    never had written for it.
    """
    tip = _git("rev-parse", branch)
    if not tip:
        return False
    # Only merges need checking, and only their parent lists — MINUS THE FIRST PARENT.
    #
    # A merge commit's first parent is main's own previous tip; the second and later parents are the
    # branches that were merged IN. Checking every parent makes any branch cut from a commit that
    # later became a merge's first parent look merged — which is every fresh worktree the moment the
    # next merge lands. Observed: task-t480, cut from 44b6d4c, read as MERGED as soon as T-457's
    # merge named 44b6d4c as its mainline parent.
    #
    # This is the THIRD false claim this tool has made about its own subject, and they share a shape:
    # each time, a cheaper question (reachability, branch-has-commits, any-parent) stood in for the
    # real one (was this branch merged in). The real question is only ever about the SECOND parent.
    for line in _git("log", "main", "--merges", "--format=%P").splitlines():
        if tip in line.split()[1:]:
            return True
    return False


@dataclass(frozen=True)
class Finding:
    ticket: Ticket
    branch: str | None
    state: str  # MERGED | AHEAD | NO WORK | NO BRANCH
    ahead: int
    age_s: float | None

    @property
    def needs_attention(self) -> bool:
        return self.state in ("MERGED", "AHEAD", "LANDED")


def inspect(t: Ticket, now: float) -> Finding:
    br = branch_for(t.id)
    if br is None:
        # A deleted branch is ambiguous: merged-and-cleaned-up, or never started. Ask main whether
        # it carries a commit that names this ticket - the merge subjects here are "Merge T-nnn: ..."
        # and result commits "T-nnn: ...", so the id in a subject is a reliable signal and a far
        # better answer than shrugging.
        # Plain id, not a \b word boundary: git's ERE does not support \b and SILENTLY MATCHES
        # NOTHING, which made every landed ticket read as never-started - the bug this branch of the
        # function exists to fix, reintroduced inside the fix. Ids are T-nnn and the board is in the
        # 400s, so a bare id cannot collide until there is a four-digit ticket.
        named = _git("log", "main", "--oneline", "--grep", t.id, "-n", "1")
        return Finding(t, None, "LANDED" if named else "NO BRANCH", 0, None)
    ahead_txt = _git("rev-list", "--count", f"main..{br}")
    ahead = int(ahead_txt) if ahead_txt.isdigit() else 0
    ts = _git("log", "-1", "--format=%ct", br)
    age = now - float(ts) if ts else None
    if ahead:
        return Finding(t, br, "AHEAD", ahead, age)
    # No commits. Before calling that "no work", look in the WORKTREE: an agent mid-task has
    # uncommitted files and is the opposite of idle.
    dirty = _worktree_dirty(br)
    if dirty:
        return Finding(t, br, "WORKING", dirty, age)
    # Nothing unmerged. Two VERY different worlds look identical here, and `--is-ancestor` cannot
    # tell them apart: a branch whose work landed, and a branch freshly cut from main that has
    # never been committed to. Both are ancestors of main with zero commits ahead.
    #
    # The discriminator is the MERGE COMMIT: `git merge --no-ff` records the branch tip as a
    # SECOND PARENT on main. A landed branch is named there; a fresh one never is. That is the
    # "has-merge-commit?" question, asked directly instead of inferred from reachability.
    return Finding(t, br, "MERGED" if _merged_via_commit(br) else "NO WORK", 0, age)


def _age(sec: float | None) -> str:
    if sec is None:
        return "?"
    if sec < 90:
        return f"{int(sec)}s"
    if sec < 5400:
        return f"{int(sec / 60)}m"
    return f"{sec / 3600:.1f}h"




def ops_dir() -> Path:
    """Where the merge runner's state actually lives — asked, not assumed.

    `HACKRIFF_OPS` is per-process, so the runner and whoever reads its state can disagree about
    the directory. On 2026-09-22 they did, and the worst instance was this module: a bulk was
    staged, `bulk-in-progress` existed, and `just reconcile` in a session with no `HACKRIFF_OPS`
    printed "no in-progress tickets" with no warning at all. The marker's whole purpose is to stop
    a reader trusting a provisional `main`, so a marker the reader cannot see is the same
    false-quiet one level up — the failure this project keeps rediscovering in new clothes
    (`Coverage::Unobserved` is not quiet; an override naming a package no run can see).

    Resolution order, most specific first: an explicit `HACKRIFF_OPS`; then the pointer the runner
    publishes at the fixed default (`~/.hackriff-ops/active-ops-dir`), which is how a session that
    sets nothing still finds it; then the fixed default itself.

    A pointer to a directory that no longer exists is ignored rather than trusted, so a stale
    pointer degrades to the default instead of silently hiding the marker again.
    """
    env = os.environ.get("HACKRIFF_OPS")
    if env:
        return Path(env)
    default = Path.home() / ".hackriff-ops"
    try:
        pointed = Path((default / "active-ops-dir").read_text(encoding="utf-8").strip())
        if pointed.is_dir():
            return pointed
    except OSError:
        pass
    return default


def bulk_warning(now: float | None = None) -> str:
    """Report the merge runner's staged-bulk window, or "" when main is authoritative.

    Read straight off the runner's marker file rather than inferred from git, because the
    distinction this draws — ungated commits currently on main vs. commits that passed a gate —
    is not visible in the commit graph at all. No marker means no warning; a marker whose writing
    process is gone is reported as such and NOT cleared, since a runner that died mid-bulk leaves
    main provisional for real.
    """
    mark = ops_dir() / "bulk-in-progress"
    try:
        fields = dict(
            line.split("=", 1) for line in mark.read_text(encoding="utf-8").splitlines() if "=" in line
        )
    except OSError:
        return ""
    age = _age((now or time.time()) - mark.stat().st_mtime)
    pid = fields.get("pid", "")
    alive = True
    if pid.isdigit():
        try:
            os.kill(int(pid), 0)
        except ProcessLookupError:
            alive = False
        except PermissionError:
            pass
    branches = fields.get("branches", "(unrecorded)")
    lines = [
        "  !! A BULK MERGE IS STAGED ON MAIN — EVERY READING BELOW IS PROVISIONAL.",
        f"     branches: {branches}",
        f"     gated-from: {fields.get('base', '?')}   started {age} ago"
        + (
            f"   (runner pid {pid} alive)"
            if alive and pid
            else f"   (runner pid {pid} IS GONE — main is left ungated)"
            if pid
            else "   (no runner pid recorded)"
        ),
        "     `ahead=0` right now means CURRENTLY merged, not DURABLY merged: the gate can still",
        "     fail and reset main back to gated-from. Do NOT delete a branch, do NOT flip a ticket",
        "     to done, and do NOT cut a new worktree from main until this clears.",
        "",
    ]
    return "\n".join(lines)


def render(findings: list[Finding]) -> str:
    if not findings:
        return "reconcile: no in-progress tickets — the board claims nothing is running.\n"
    w = max(len(f.ticket.id) for f in findings)
    lines = [f"reconcile: {len(findings)} in-progress ticket(s) on the board\n"]
    for f in sorted(findings, key=lambda f: (not f.needs_attention, f.ticket.id)):
        mark = "!!" if f.state in ("MERGED", "LANDED") else ("->" if f.state == "AHEAD" else "  ")
        ahead = (f"~{f.ahead}" if f.state == "WORKING" else f"+{f.ahead}") if f.ahead else "  "
        lines.append(
            f"  {mark} {f.ticket.id:<{w}}  {f.state:<9} {ahead:>3}  last {_age(f.age_s):>5}"
            f"  {f.branch or '(no branch)'}"
        )
        lines.append(f"       {f.ticket.title[:96]}")
    merged = [f for f in findings if f.state in ("MERGED", "LANDED")]
    ahead = [f for f in findings if f.state == "AHEAD"]
    lines.append("")
    if merged:
        lines.append(
            "  !! MERGED: already on main. Mark done with its merge commit — the work is finished\n"
            "     and the board is lying about it. " + ", ".join(f.ticket.id for f in merged)
        )
    if ahead:
        lines.append(
            "  -> AHEAD: commits main has not got. Gate and merge if the agent handed back; relaunch\n"
            "     if it was lost. " + ", ".join(f"{f.ticket.id}(+{f.ahead})" for f in ahead)
        )
    working = [f for f in findings if f.state == "WORKING"]
    if working:
        lines.append(
            "  ~  WORKING: uncommitted files in the worktree — an agent is mid-task. Leave it alone.\n"
            "     " + ", ".join(f"{f.ticket.id}(~{f.ahead})" for f in working)
        )
    if not merged and not ahead:
        lines.append("  nothing needs attention: no in-progress ticket has unmerged committed work.")
    lines.append(
        "\n  Staleness is a HINT, not a verdict — an agent that is reading rather than writing is\n"
        "  legitimately quiet. Check for a live agent before concluding one was lost.\n"
    )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Reconcile in-progress tickets against git.")
    ap.add_argument("--strict", action="store_true", help="exit 1 if anything needs attention")
    ap.add_argument("--board", type=Path, default=BOARD)
    args = ap.parse_args(argv)

    text = args.board.read_text(encoding="utf-8")
    now = time.time()
    findings = [inspect(t, now) for t in tickets(text) if t.status == "in-progress"]
    sys.stdout.write(bulk_warning(now) + render(findings))
    if args.strict and any(f.needs_attention for f in findings):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
