---
description: Reconcile the board against git at the start of every coordinator tick, before launching anything. Catches stale in-progress tickets, landed-but-unmarked branches, and lost agents.
disable-model-invocation: false
allowed-tools: Bash(just reconcile), Bash(git *), Read
---

## Reconcile the board (T-477)

Run **at the start of every tick, before launching anything**:
```
just reconcile
```
`in-progress` is a claim about the world and it goes stale silently — a branch lands and the board is never flipped (T-300/T-364/T-426/T-458 all did this), or an agent is lost and its branch sits with work on it, or with none. Both read identically from the board and completely differently from git, so the check lives in the runner, not in memory.

## What it prints, per in-progress ticket
- **MERGED** — `main` has a merge commit naming the branch tip (not mere ancestry, which a fresh branch also satisfies).
- **AHEAD n** — the branch has n commits not on main.
- **NO WORK** — a branch exists with nothing ahead.
- **NO BRANCH** — the ticket claims in-progress but no branch exists.
- Plus **staleness** (how long since the branch moved).

## Then act
- **MERGED** → mark the ticket `done` with its commit.
- **AHEAD n** (finished) → queue it: append the branch to `ops/merge-queue.txt` (gate + merge).
- **NO BRANCH / NO WORK** (lost agent) → relaunch the worker, or re-status the ticket honestly (`todo` if nothing was done).
- Anything ambiguous → re-status it honestly, don't leave it claiming in-progress.

**Staleness is a hint, not a verdict.** An agent reading rather than writing is legitimately quiet — check for a live agent before concluding one was lost.
