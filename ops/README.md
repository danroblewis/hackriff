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
