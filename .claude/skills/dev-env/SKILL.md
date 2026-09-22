---
description: Start, stop, restart or check the orchestration environment (staging demo, merge runner, work runner, dashboard, coordinator). The supervisor runs `/dev-env start` at the start of a session; `/dev-env stop` before a reboot or when things must halt; `/dev-env status` any time.
disable-model-invocation: false
allowed-tools: Bash(*), Read
---

## The dev environment (`ops/README.md` is the reference; this skill is the procedure)

Argument: `start` (default), `stop`, `restart`, `status`. Every step below prints what it verified.
Runners before the coordinator, and the coordinator **last** — it reads `main`'s board and role
file at launch and starts acting on them.

Constants: `REPO=/Users/daniellewis/hackriff`, `HACKRIFF_OPS=~/.hackriff-ops` (the durable home;
`~/.hackriff-ops/active-ops-dir` is the pointer every script rewrites on start).

### status
```bash
export HACKRIFF_OPS=~/.hackriff-ops; cd /Users/daniellewis/hackriff
ps -axo pid,etime,command | grep -E 'Role: Coo|merge-runner.sh|work-runner.py|stage.sh|monitor.py|just gate|claude -p' | grep -v grep | cut -c1-110
git branch --show-current; git log -1 --format='%h %s'; ls .git/MERGE_HEAD $HACKRIFF_OPS/bulk-in-progress 2>/dev/null
cat $HACKRIFF_OPS/merge-queue.txt; tail -3 $HACKRIFF_OPS/merge-needs-attention.txt $HACKRIFF_OPS/work-needs-attention.txt
cat $HACKRIFF_OPS/work-runner-status.json; df -h / | tail -1
```
Report: which of the five are up, whether a gate is running, what is queued, what needs attention.

### stop
1. `tmux kill-session -t dev` — the coordinator and every subagent it spawned.
2. `pkill -f work-runner.py` — dispatch. Its running `claude -p` workers keep going in their
   worktrees and finish on their own; their branches are picked up by the next runner start
   (a branch with commits is left alone; one with none is reused).
3. The merge runner: **between gates only** if you can wait (`pgrep -f 'just gate'` empty and no
   `bulk-in-progress`). If you cannot wait, kill it (`pkill -f merge-runner.sh; pkill -f 'just gate';
   pkill -f 'cargo-nextest nextest run'`) and then repair `main` — see "a killed gate" below.
4. `pkill -f stage.sh; pkill -f 'hk serve --bind 127.0.0.1:8899'; pkill -f monitor.py`
5. Verify nothing is left: the `ps` line from **status** must print nothing.

**A killed gate leaves `main` provisional.** A bulk batch commits each merge before gating; the
marker `$HACKRIFF_OPS/bulk-in-progress` names `base=<sha>` and `branches=…`. Rewind and re-queue:
```bash
cat $HACKRIFF_OPS/bulk-in-progress
git -C /Users/daniellewis/hackriff status --porcelain | grep -v '^??'   # must be empty (else: git merge --abort)
git -C /Users/daniellewis/hackriff reset --hard <base>                  # use -C: never rely on cwd
rm -f $HACKRIFF_OPS/bulk-in-progress
printf '%s\n' <the branches> >> $HACKRIFF_OPS/merge-queue.txt
```
A staged single merge is `.git/MERGE_HEAD`: `git merge --abort`, re-queue the branch.

### start
0. **Pre-flight.** `git -C $REPO branch --show-current` is `main`; no `MERGE_HEAD`, no
   `bulk-in-progress` (else repair as above); `git status --porcelain | grep -v '^??'` empty;
   `df -h /` ≥ 20 GB (else remove clean merged worktrees: `git worktree list`, then
   `git worktree remove <path>` for any with no commits ahead of main and a clean tree).
1. **State home.** `cp -Rn "$(cat ~/.hackriff-ops/active-ops-dir)/." ~/.hackriff-ops/ 2>/dev/null;
   export HACKRIFF_OPS=~/.hackriff-ops`.
2. **The four scripts, from the repo, never copies:**
   ```bash
   cd /Users/daniellewis/hackriff && export HACKRIFF_OPS=~/.hackriff-ops
   nohup bash ops/stage.sh          >/dev/null 2>&1 & disown
   nohup bash ops/merge-runner.sh   >/dev/null 2>&1 & disown
   python3 ops/work-runner.py --once --dry-run        # read what it would dispatch; stop here if it looks wrong
   nohup python3 ops/work-runner.py >/dev/null 2>&1 & disown
   MONITOR_PORT=8901 nohup python3 ops/monitor.py >$HACKRIFF_OPS/monitor.log 2>&1 &
   ```
   If a script's newest version is only on an unmerged branch, start it from that worktree
   (`.claude/worktrees/<name>/ops/<script>`) and restart from `main` once it lands.
3. **Verify** (about a minute later), and report each line:
   ```bash
   grep VERSION $HACKRIFF_OPS/merge-runner.log | tail -1     # "matches"
   tail -3 $HACKRIFF_OPS/work-runner.log                      # "VERSION: matches", then DISPATCH/tick lines
   tail -2 $HACKRIFF_OPS/stage.log                            # "started (live)" or "(replay …)"
   curl -s http://127.0.0.1:8901/burndown.json | head -c 60   # dashboard answers
   ```
   The work runner's first tick can take minutes: each dispatch clones `target/` (large tree).
4. **The coordinator, last:** `ops/launch.sh coordinator`. Its role file must be the one on
   `main`; if it starts spawning workers for ordinary `todo` tickets, its role file is stale —
   stop it, check `.claude/roles/coordinator.md` carries the "Dispatch … is the work runner's job"
   paragraph.
5. Tell the user what is running, what is queued, what the first dispatches were, and anything
   in either attention file.

### restart
`stop` (waiting for the gate if at all possible), then `start`.

## Rules that bind this skill's session too
- Never edit, `stash`, `reset` or commit in the main checkout except the documented rewind, and
  never while a gate runs (root `CLAUDE.md`, Coordination). Your own changes go on a branch in
  `.claude/worktrees/<name>` and into `merge-queue.txt` — **append, never rewrite**.
- Use `git -C /Users/daniellewis/hackriff …` for anything aimed at `main`; a harness can pin the
  shell's cwd inside a worktree and a bare `cd` may not take (a `reset --hard` meant for `main`
  once landed on a feature branch this way).
