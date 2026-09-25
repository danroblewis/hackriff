"""Keep every remote worker host's repo and this Mac's repo identical (pipeline invariant 29).

WHY. On 2026-09-25 nine resumed node2 runs (T-957..T-972) committed on node2 and were handed back
NO_WORK: the work runner judged "no commits" from this Mac's ref, which nothing had moved. The user:
"This is not a bug, this is a design flaw ... keep the node2 repo and the Mac repo in sync at all
times." So the two repos are treated as one, programmatically, and nothing is judged from a ref that
has not been synced.

The host side is two repos: its clone (where the worker's worktree lives - `hosts.json` `repo`) and
the bare mirror that clone calls `origin`, which is also this Mac's git remote named after the host.
A commit in a host worktree is in the clone's refs at once but reaches the mirror only when pushed,
so the clone's hooks push it (HOOK, installed by the work runner's remote_prepare).

The rules, per task branch on the mirror (`reconcile`):
  * local == mirror                    in sync
  * local behind (fast-forward)        moved: `update-ref` when no worktree has it checked out, a
                                       `reset --hard` of that worktree when it is clean, never when dirty
  * local ahead                        pushed to the mirror, fast-forward only - never --force
  * local is a tip this Mac FETCHED    the host rewrote its own history (an amend): moved as above. Only
    (the tracking ref's reflog)        a fetch counts - a tip this Mac PUSHED is its own work, never followed
  * anything else                      diverged, or no local branch: reported, never moved - a person decides

main -> mirror after every landing stays in ops/merge-runner.sh (push_mirrors), and the work runner's
remote_prepare pushes the gated base fast-forward-only before every remote dispatch.

Every call that reaches the host (fetch, push) is bounded: BatchMode/ConnectTimeout ssh, NET_TIMEOUT_S.

`python -m hkpy.reposync --status` prints each host's drift after a fetch into refs/remotes only:
no pushes, no local branch moved.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys

OPS = os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops")
REPO = os.environ.get("HACKRIFF_REPO") or "/Users/daniellewis/hackriff"
PREFIX = "task-"
NET_TIMEOUT_S = 60
#: The ssh a fetch/push to a host uses; the work runner passes its own (the same options as every other ssh it runs).
SSH = "ssh -o BatchMode=yes -o ConnectTimeout=10"
#: States that leave local == mirror.
SYNCED = ("in-sync", "fast-forwarded", "pushed", "rewritten")

#: The host clone's hooks (core.hooksPath -> a dir of links to this one script). Each hook first runs the repo's
#: own tracked hook (.githooks/<name>: Git LFS); after a commit, merge or rewrite on a task branch it pushes the
#: branch to the mirror. Invoked as `reposync-push` it only pushes (the remote wrapper's last step).
#: Fast-forward; or, after this branch's own amend, a lease over exactly the tip THIS clone last pushed (recorded in
#: refs/reposync/pushed/<b>) - never over a tip anyone else put there. A push that moves nothing records nothing.
HOOK = r"""#!/bin/sh
# hackriff reposync (hkpy.reposync; installed by ops/work-runner.py remote_prepare)
n=$(basename "$0")
t="$(git rev-parse --show-toplevel)/.githooks/$n"
case "$n" in
  post-commit|post-merge|post-rewrite) [ -x "$t" ] && "$t" "$@" ;;
  reposync-push) ;;
  *) [ -x "$t" ] && exec "$t" "$@"; exit 0 ;;
esac
b=$(git symbolic-ref -q --short HEAD) || exit 0
case "$b" in task-*) ;; *) exit 0 ;; esac
h=$(git rev-parse HEAD)
m=$(git ls-remote origin "refs/heads/$b" | cut -f1)
[ "$m" = "$h" ] && exit 0
mine=$(git rev-parse -q --verify "refs/reposync/pushed/$b")
if [ -z "$m" ] || git merge-base --is-ancestor "$m" HEAD 2>/dev/null; then
  git push -q --no-verify origin "HEAD:refs/heads/$b" >&2 && git update-ref "refs/reposync/pushed/$b" "$h"
elif [ -n "$mine" ] && [ "$m" = "$mine" ]; then
  git push -q --no-verify --force-with-lease="refs/heads/$b:$mine" origin "HEAD:refs/heads/$b" >&2 &&
    git update-ref "refs/reposync/pushed/$b" "$h"
else
  echo "reposync: $b on the mirror has commits this clone never pushed - not pushed (the Mac reports SYNC_ERROR)" >&2
fi
exit 0
"""
#: The hooks that push; the rest of the links are the clone's own tracked hooks (`ls .githooks`), chained.
PUSH_HOOKS = ("post-commit", "post-merge", "post-rewrite", "reposync-push")


def install_hook_cmd(hook_dir: str, clone: str) -> str:
    """Shell (run on the host, after HOOK is written to <hook_dir>/reposync) that links the clone's tracked hook
    names and the push triggers to it and points the clone's core.hooksPath there."""
    return (f"chmod +x {hook_dir}/reposync; for n in $(ls {clone}/.githooks 2>/dev/null) {' '.join(PUSH_HOOKS)}; "
            f"do ln -sf reposync {hook_dir}/$n; done; git -C {clone} config core.hooksPath {hook_dir}")


def ff_worktree_cmd(wt: str, branch: str) -> str:
    """Shell (on the host): a clean worktree takes the mirror's commits on its branch, fast-forward only -
    the Mac's commits (a runner board result, a coordinator's merge) reach the worker."""
    return (f"[ -z \"$(git -C {wt} status --porcelain --untracked-files=no)\" ] && "
            f"git -C {wt} merge-base --is-ancestor HEAD origin/{branch} 2>/dev/null && git -C {wt} merge -q --ff-only origin/{branch}")


def _git(repo: str, *args: str, ssh: str | None = None) -> subprocess.CompletedProcess:
    """git in `repo`; with `ssh`, a call that reaches a host - bounded by that ssh's options and NET_TIMEOUT_S."""
    env = dict(os.environ, GIT_SSH_COMMAND=ssh) if ssh else None
    try:
        return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, env=env,
                              timeout=NET_TIMEOUT_S if ssh else 300)
    except subprocess.TimeoutExpired:
        return subprocess.CompletedProcess(args, 124, "", f"timed out after {NET_TIMEOUT_S if ssh else 300} s")


def _out(repo: str, *args: str) -> str:
    r = _git(repo, *args)
    return r.stdout.strip() if r.returncode == 0 else ""


def _refs(repo: str, pattern: str) -> dict[str, str]:
    out = _out(repo, "for-each-ref", "--format=%(refname) %(objectname)", pattern)
    return {ln.split()[0]: ln.split()[1] for ln in out.splitlines() if ln.strip()}


def _checked_out(repo: str) -> dict[str, str]:
    """{branch: worktree path} for every branch checked out in a worktree of `repo`."""
    wts, path = {}, None
    for ln in _out(repo, "worktree", "list", "--porcelain").splitlines():
        if ln.startswith("worktree "):
            path = ln[len("worktree "):]
        elif ln.startswith("branch refs/heads/") and path:
            wts[ln[len("branch refs/heads/"):]] = path
    return wts


def _count(repo: str, a: str, b: str) -> int | None:
    """Commits in b not in a; None when git cannot say (the caller holds - it never moves on an unknown)."""
    r = _git(repo, "rev-list", "--count", f"{a}..{b}")
    return int(r.stdout.strip()) if r.returncode == 0 and r.stdout.strip().isdigit() else None


def _fetched(repo: str, tracking: str, sha: str) -> bool:
    """`sha` was a value of the tracking ref that a FETCH wrote - the mirror's, not one this Mac pushed there."""
    for ln in _out(repo, "reflog", "show", "--format=%H %gs", tracking).splitlines():
        h, _, subject = ln.partition(" ")
        if h == sha and subject.startswith("fetch"):
            return True
    return False


def fetch(repo: str, host: str, branch: str | None = None, ssh: str = SSH) -> str | None:
    """Fetch the host mirror's task branches (or one) into refs/remotes/<host>/; None, or the error."""
    src = f"refs/heads/{branch}" if branch else f"refs/heads/{PREFIX}*"
    dst = f"refs/remotes/{host}/{branch}" if branch else f"refs/remotes/{host}/{PREFIX}*"
    r = _git(repo, "fetch", "-q", "--no-write-fetch-head", host, f"+{src}:{dst}", ssh=ssh)
    return None if r.returncode == 0 else (r.stderr.strip()[-300:] or f"git fetch exit {r.returncode}")


def _move(repo: str, branch: str, old: str, new: str, wt: str | None) -> str | None:
    """Move local `branch` from `old` to `new`; None or why not. A worktree that has it checked out is reset only
    when clean (tracked files); a dirty one is never touched."""
    if wt:
        if not os.path.isdir(wt):
            return f"checked out in {wt}, which is missing"
        if _out(wt, "status", "--porcelain", "--untracked-files=no"):
            return f"checked out dirty in {wt}"
        r = _git(wt, "reset", "-q", "--hard", new)
    else:
        r = _git(repo, "update-ref", f"refs/heads/{branch}", new, old)
    return None if r.returncode == 0 else r.stderr.strip()[:200]


def reconcile(repo: str, host: str, branch: str, wts: dict[str, str] | None = None, dry: bool = False) -> dict:
    """Apply the rules to one branch whose mirror tip is already fetched - except a push, which is returned as
    state `ahead` for push_ahead to batch. {mirror, local, behind, ahead, state[, why]}; `local` after any move."""
    wts = _checked_out(repo) if wts is None else wts
    tracking = f"refs/remotes/{host}/{branch}"
    mirror = _out(repo, "rev-parse", "-q", "--verify", tracking)
    local = _out(repo, "rev-parse", "-q", "--verify", f"refs/heads/{branch}")
    e = {"mirror": mirror or None, "local": local or None, "behind": 0, "ahead": 0}
    if not mirror:
        return dict(e, state="no-mirror")
    if not local:
        return dict(e, state="no-local")
    if local == mirror:
        return dict(e, state="in-sync")
    behind, ahead = _count(repo, local, mirror), _count(repo, mirror, local)
    if behind is None or ahead is None:
        return dict(e, behind=behind, ahead=ahead, state="held", why="git rev-list failed")
    e.update(behind=behind, ahead=ahead)
    if ahead == 0 or _fetched(repo, tracking, local):
        state = "fast-forwarded" if ahead == 0 else "rewritten"
        if dry:
            return dict(e, state=f"would-{state}")
        why = _move(repo, branch, local, mirror, wts.get(branch))
        return dict(e, state=state, local=mirror) if why is None else dict(e, state="held", why=why)
    if behind == 0:
        return dict(e, state="would-push" if dry else "ahead")
    return dict(e, state="diverged")


def push_ahead(repo: str, host: str, entries: dict[str, dict], ssh: str = SSH) -> None:
    """ONE push of every `ahead` branch to the mirror, fast-forward only (never --force); each entry's state
    becomes `pushed` or `push-failed` by what the mirror's tracking ref says afterwards."""
    ahead = sorted(b for b, e in entries.items() if e["state"] == "ahead")
    if not ahead:
        return
    r = _git(repo, "push", "-q", "--no-verify", host, *(f"refs/heads/{b}:refs/heads/{b}" for b in ahead), ssh=ssh)
    for b in ahead:
        e = entries[b]
        if _out(repo, "rev-parse", "-q", "--verify", f"refs/remotes/{host}/{b}") == e["local"]:
            e.update(state="pushed", mirror=e["local"])
        else:
            e.update(state="push-failed", why=r.stderr.strip()[-200:] or f"git push exit {r.returncode}")


def sync_branch(repo: str, host: str, branch: str, ssh: str = SSH) -> dict:
    """Fetch one branch from the host mirror and reconcile it: the sync a hand-back judgment requires."""
    err = fetch(repo, host, branch, ssh)
    if err:
        return {"mirror": None, "local": None, "behind": 0, "ahead": 0, "state": "fetch-failed", "why": err}
    entries = {branch: reconcile(repo, host, branch)}
    push_ahead(repo, host, entries, ssh)
    return entries[branch]


def sync_host(repo: str, host: str, dry: bool = False, ssh: str = SSH) -> dict:
    """One fetch of every task branch on the host's mirror, each reconciled, and one push of those this Mac is
    ahead on (dry: reported only). {refs_in_sync, drifting, error, branches: {branch: {...}}}."""
    err = fetch(repo, host, ssh=ssh)
    if err:
        return {"refs_in_sync": False, "drifting": None, "error": err, "branches": {}}
    wts = _checked_out(repo)
    pre = f"refs/remotes/{host}/"
    branches = {ref[len(pre):]: reconcile(repo, host, ref[len(pre):], wts, dry)
                for ref in sorted(_refs(repo, f"{pre}{PREFIX}*"))}
    push_ahead(repo, host, branches, ssh)
    drifting = sum(1 for e in branches.values() if e["state"] not in SYNCED)
    return {"refs_in_sync": drifting == 0, "drifting": drifting, "error": None, "branches": branches}


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="python -m hkpy.reposync", description=__doc__.split("\n\n")[0])
    ap.add_argument("--status", action="store_true", help="read-only: fetch into refs/remotes, print each host's drift")
    ap.add_argument("--ops", default=OPS)
    ap.add_argument("--repo", default=REPO)
    a = ap.parse_args(argv)
    if not a.status:
        ap.error("only --status is offered; the work runner syncs every tick")
    try:
        hosts = json.load(open(os.path.join(a.ops, "hosts.json")))
    except (OSError, ValueError):
        hosts = {}
    if not hosts:
        print("no remote host configured")
    for h in hosts:
        r = sync_host(a.repo, h, dry=True)
        if r["error"]:
            print(f"{h}: fetch failed - {r['error']}")
            continue
        print(f"{h}: refs in sync: {'yes' if r['refs_in_sync'] else 'no'} ({r['drifting']} drifting of {len(r['branches'])})")
        for b, e in r["branches"].items():
            if e["state"] not in SYNCED:
                print(f"  {b:<22} {e['state']:<16} mirror {(e['mirror'] or '-')[:8]} local {(e['local'] or '-')[:8]} "
                      f"behind {e['behind']} ahead {e['ahead']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
