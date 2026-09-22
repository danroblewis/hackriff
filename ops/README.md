# `ops/` — dev-orchestration scripts

Long-running helpers that run the autonomous dev setup. They are **not** product code and
nothing in the build depends on them; they are committed here only so a machine restart can
never lose them again (they used to live in a `/tmp` scratchpad that a reboot wiped).

Runtime state (logs, tokens, the demo build, the merge queue) lives in **`$HACKRIFF_OPS`**,
default **`~/.hackriff-ops`** — deliberately outside `/tmp` so it survives a reboot. Override
with `HACKRIFF_OPS=/some/dir` if you want it elsewhere. The four scripts share that one dir.

Environment-specific constants at the top of each file (edit for a different machine/checkout):
`REPO` (the repo path, `/Users/daniellewis/hackriff`); in `monitor.py` also `PROJ` (the Claude
projects dir), `COORD` (the coordinator's conversation id), and `SUPER`.

## The four scripts

### `stage.sh` — staging demo watcher (port 8899)
Rebuilds the `hk` binary and restarts the "bears" demo on every **code** commit to `main`
(ignores docs/tasks-only commits), smoke-tests it, and keeps a cloudflared tunnel up. Prefers
the live HackRF; falls back to a looping SigMF replay when the device is busy. Also self-heals:
if the live spectrum stream dies it restarts.
```bash
HACKRIFF_OPS=~/.hackriff-ops nohup bash ops/stage.sh >/dev/null 2>&1 & disown
# demo:      http://127.0.0.1:8899   (token in $HACKRIFF_OPS/hk-token-bears)
# tunnel URL: grep trycloudflare $HACKRIFF_OPS/cf-hk.log
# log:       $HACKRIFF_OPS/stage.log
```

### `monitor.py` — agent dashboard (port 8901)
Serves a live dashboard: worktrees, diffs, tasks, the coordinator tmux pane, agent sessions,
the merge queue + derived "up next" order, system load, a Claude token-budget tile
(auto-polled via a throwaway `/usage` session), and clickable ticket/transcript modals.
```bash
MONITOR_PORT=8901 HACKRIFF_OPS=~/.hackriff-ops nohup python3 ops/monitor.py >$HACKRIFF_OPS/monitor.log 2>&1 &
# open http://127.0.0.1:8901  (or tunnel it with cloudflared)
```

### `merge-runner.sh` — automated, no-AI merge runner
Owns all merges to `main` deterministically. The coordinator appends a **code-complete** branch
name (one per line, dependency order) to `$HACKRIFF_OPS/merge-queue.txt`; the runner then does
`git merge --no-ff --no-commit <branch>` → `just gate-merge` → on green, commit + remove the
worktree. A conflict or gate/test failure is written to `$HACKRIFF_OPS/merge-needs-attention.txt`
for a human/AI to handle — AI is only needed for the exceptions.
```bash
HACKRIFF_OPS=~/.hackriff-ops nohup bash ops/merge-runner.sh >/dev/null 2>&1 & disown
# queue:    echo task-t519 >> $HACKRIFF_OPS/merge-queue.txt
# failures: cat $HACKRIFF_OPS/merge-needs-attention.txt
# merged:   cat $HACKRIFF_OPS/merge-done.txt   ·   log: $HACKRIFF_OPS/merge-runner.log
```
**Always launch it from `ops/merge-runner.sh` in the repo, never from a copy in `$HACKRIFF_OPS`.**
A bash script is read by the live process from its own inode, so a running runner keeps executing
the code it started with: an edit to the script changes nothing until a restart, and an edit to a
*copy* changes nothing ever. On 2026-09-20 the runner had been started from a stale copy, and the
sequential-bulk fix sat committed and believed-live for hours while the process went on octopus-
merging — eight bulk attempts, zero bulk merges. The runner now logs `VERSION:` at startup saying
whether its own file matches `HEAD:ops/merge-runner.sh`; **read that line after every restart.**

**A conflicting branch is SKIPPED, not a reason to abandon the batch.** `try_bulk` merges the
queued branches one at a time (two heads each, ordinary recursive merge) and then runs ONE gate
over the accumulated result. If a branch conflicts it is aborted, flagged in
`merge-needs-attention.txt` as `CONFLICT(skipped from bulk)` and left out — the branches that
already merged stay merged, and the batch gates without it. It is never re-queued automatically:
a conflict needs a fix, not a retry. (Before 2026-09-21 the first conflict rewound the entire
batch and sent every branch through its own gate; one trivial justfile conflict in `task-t559`
cost fourteen branches their shared gate.)

**Restarting it safely:** only between gates. Killing it mid-gate leaves a staged merge with no
owner (`MERGE_HEAD` set, nothing to finish it) and the replacement waits on that merge for ever.
Wait until no `gate-merge` process is running, kill it *by the pid you captured at spawn* (never a
broad `pkill -f` — one of those killed a merge gate on 2026-09-20), `git merge --abort` if a merge
is still staged, then start the new one. Note also that a fallback batch is held **in memory**: if
the runner is stopped after a bulk attempt fell back to individual gates, the branches it had
already consumed from the queue are lost and must be re-queued by hand.

### `work-runner.py` — automated, no-AI dispatch (the fourth script, 2026-09-22)
Dispatch was the bottleneck: one serial coordinator loop deciding per tick whether to start a
ticket, while builder slots sat idle for the length of every gate. This runner mirrors the merge
runner's shape for the *start* of a ticket's life. Every 30 s it: **reaps** finished workers
(commits ahead of `main` → the reviewer stage for `core_interface`/cheap-model tickets, then the
merge queue; no commits, error, timeout or an uncommitted tree → `work-needs-attention.txt`);
**syncs the board** (started → `in-progress` + `branch:`, landed per `landed.jsonl` → `done` +
`commit:`) in one small commit on `main`, only when `main` is safe (no `MERGE_HEAD`, no
`bulk-in-progress`, clean tree); and **dispatches** `todo` tickets whose deps are done — not
`blocked_on`, not `needs: user|hardware`, not `dispatch: manual`, one per `parallel_group` at a
time, user-requested first, then priority, then number — into a fresh worktree + branch
(`task-t<nnn>`, target seeded by APFS clone) running `claude -p --agent worker` with the ticket's
`model`/`effort`, the brief on stdin, JSON result to `$HACKRIFF_OPS/work/<ticket>/out.json`.
Builder cap is CLAUDE.md's 4 *including* a running gate (`WORK_CAP`), disk floor 20 GB.
```bash
HACKRIFF_OPS=~/.hackriff-ops nohup python3 ops/work-runner.py >/dev/null 2>&1 & disown
# dry run:   python3 ops/work-runner.py --once --dry-run      (prints what it would dispatch)
# running:   cat $HACKRIFF_OPS/work-runner-status.json  ·  claims: work-claims.json
# exceptions: cat $HACKRIFF_OPS/work-needs-attention.txt   ·  per run: work-done.jsonl
# opt a ticket out of auto-dispatch: `dispatch: manual` on its board entry
```
Like the merge runner it logs `VERSION:` at startup — launch it from `ops/work-runner.py` in the
repo, never a copy. The coordinator no longer dispatches by hand (`.claude/roles/coordinator.md`):
it handles the two attention files, reviews, triage and ordering, and hand-launches only what the
runner will not touch (`needs: user|hardware`, `dispatch: manual`). **Append to the merge queue,
never rewrite it** — a rewrite on 2026-09-22 dropped a queued branch.

## Restart-all (after a reboot)
```bash
export HACKRIFF_OPS=~/.hackriff-ops
cd /Users/daniellewis/hackriff
nohup bash ops/stage.sh        >/dev/null 2>&1 & disown
nohup bash ops/merge-runner.sh >/dev/null 2>&1 & disown
nohup python3 ops/work-runner.py >/dev/null 2>&1 & disown
MONITOR_PORT=8901 nohup python3 ops/monitor.py >$HACKRIFF_OPS/monitor.log 2>&1 &
# then re-tunnel the dashboard if needed: nohup cloudflared tunnel --url http://127.0.0.1:8901 &
```
The coordinator tmux session is separate (see CLAUDE.md → Coordination).
