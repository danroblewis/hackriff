# `ops/` — dev-orchestration scripts

Long-running helpers that run the autonomous dev setup. They are **not** product code and
nothing in the build depends on them; they are committed here only so a machine restart can
never lose them again (they used to live in a `/tmp` scratchpad that a reboot wiped).

Runtime state (logs, tokens, the demo build, the merge queue) lives in **`$HACKRIFF_OPS`**,
default **`~/.hackriff-ops`** — deliberately outside `/tmp` so it survives a reboot. Override
with `HACKRIFF_OPS=/some/dir` if you want it elsewhere. The three scripts share that one dir.

Environment-specific constants at the top of each file (edit for a different machine/checkout):
`REPO` (the repo path, `/Users/daniellewis/hackriff`); in `monitor.py` also `PROJ` (the Claude
projects dir), `COORD` (the coordinator's conversation id), and `SUPER`.

## The three scripts

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

**Restarting it safely:** only between gates. Killing it mid-gate leaves a staged merge with no
owner (`MERGE_HEAD` set, nothing to finish it) and the replacement waits on that merge for ever.
Wait until no `gate-merge` process is running, kill it *by the pid you captured at spawn* (never a
broad `pkill -f` — one of those killed a merge gate on 2026-09-20), `git merge --abort` if a merge
is still staged, then start the new one. Note also that a fallback batch is held **in memory**: if
the runner is stopped after a bulk attempt fell back to individual gates, the branches it had
already consumed from the queue are lost and must be re-queued by hand.

## Restart-all (after a reboot)
```bash
export HACKRIFF_OPS=~/.hackriff-ops
cd /Users/daniellewis/hackriff
nohup bash ops/stage.sh        >/dev/null 2>&1 & disown
nohup bash ops/merge-runner.sh >/dev/null 2>&1 & disown
MONITOR_PORT=8901 nohup python3 ops/monitor.py >$HACKRIFF_OPS/monitor.log 2>&1 &
# then re-tunnel the dashboard if needed: nohup cloudflared tunnel --url http://127.0.0.1:8901 &
```
The coordinator tmux session is separate (see CLAUDE.md → Coordination).
