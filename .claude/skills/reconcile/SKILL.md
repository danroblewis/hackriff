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

## First: is `main` even authoritative right now?

Reconcile now **leads with a warning** when the merge runner has a bulk staged, and that warning invalidates every finding below it at once. The runner commits each branch of a batch as it goes and rewinds only if the ~20-minute gate then fails, so for that whole window `main` carries commits that **have not passed a gate**.

**`ahead=0` in that window means "currently merged", not "durably merged".** On 2026-09-21 that one distinction cost three separate incidents: a branch deleted on an `ahead=0` reading that had to be recovered from a merge commit's second parent (`e097ce8a^2`), a batch announced as landed from `git log --merges` that the gate then rewound, and a worker branch cut from staged-but-ungated main that silently absorbed three other tickets' work and had to be rebuilt.

While the warning is showing: **do not delete a branch, do not flip a ticket to `done`, and do not cut a new worktree from `main`.** Wait for it to clear. The marker is `$HACKRIFF_OPS/bulk-in-progress` and the runner removes it on a pass or a rewind; if the runner died mid-bulk the warning **stays**, because main really is left ungated — that case needs a person, not a cleared file.

## What it prints, per in-progress ticket
- **MERGED** — `main` has a merge commit naming the branch tip (not mere ancestry, which a fresh branch also satisfies).
- **AHEAD n** — the branch has n commits not on main.
- **NO WORK** — a branch exists with nothing ahead.
- **NO BRANCH** — the ticket claims in-progress but no branch exists.
- Plus **staleness** (how long since the branch moved).

## Then act
- **MERGED** → mark the ticket `done` with its commit — *unless a bulk is staged*, in which case wait.
- **AHEAD n** (finished) → queue it: append the branch to `$HACKRIFF_OPS/merge-queue.txt` (gate + merge).
- **NO BRANCH / NO WORK** (lost agent) → relaunch the worker, or re-status the ticket honestly (`todo` if nothing was done).
- Anything ambiguous → re-status it honestly, don't leave it claiming in-progress.

**Staleness is a hint, not a verdict.** An agent reading rather than writing is legitimately quiet — check for a live agent before concluding one was lost.
