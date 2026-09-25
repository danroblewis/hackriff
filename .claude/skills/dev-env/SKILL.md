---
description: Start, stop, restart or check the orchestration environment (staging demo, merge runner, work runner, dashboard, watchdog, coordinator). The supervisor runs `/dev-env start` at the start of a session; `/dev-env stop` before a reboot or when things must halt; `/dev-env status` any time.
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
ps -axo pid,etime,command | grep -E 'Role: Coo|merge-runner.sh|work-runner.py|stage.sh|monitor.py|watchdog.py|just gate|claude -p' | grep -v grep | cut -c1-110
git branch --show-current; git log -1 --format='%h %s'; ls .git/MERGE_HEAD $HACKRIFF_OPS/bulk-in-progress 2>/dev/null
cat $HACKRIFF_OPS/merge-queue.txt; tail -3 $HACKRIFF_OPS/merge-needs-attention.txt $HACKRIFF_OPS/work-needs-attention.txt
cat $HACKRIFF_OPS/work-runner-status.json; df -h / | tail -1
cat $HACKRIFF_OPS/watchdog.json | head -c 400; echo; tail -5 $HACKRIFF_OPS/watchdog.log
```
Report: which of the six are up, whether a gate is running, what is queued, what needs attention,
and **the watchdog's last tick** — its `load` against `budget`, any `unowned` entry, any `alarms`.
A `watchdog.json` older than two minutes means the watchdog is dead, and nothing is watching the
box: that is how sixteen orphaned busy loops ran through every gate for 2 h 18 m on 2026-09-22.

### stop
0. `touch $HACKRIFF_OPS/roles-stopped` — FIRST: `ops/watchdog.py` relaunches a dead `dev`/`flow`
   session within ~2 min (incident 2026-09-24 04:07) unless this marker (or `dispatch-paused`) exists.
1. `tmux kill-session -t dev` — the coordinator and every subagent it spawned.
2. `pkill -f work-runner.py` — dispatch. Its running `claude -p` workers keep going in their
   worktrees and finish on their own; their branches are picked up by the next runner start
   (a branch with commits is left alone; one with none is reused).
3. The merge runner: **between gates only** if you can wait (`pgrep -f 'just gate'` empty and no
   `bulk-in-progress`). If you cannot wait, kill it (`pkill -f merge-runner.sh; pkill -f 'just gate';
   pkill -f 'cargo-nextest nextest run'`) and then repair `main` — see "a killed gate" below.
4. `pkill -f stage.sh; pkill -f 'hk serve --bind 127.0.0.1:8899'; pkill -f monitor.py; pkill -f 'ops/watchdog.py'`
5. Verify nothing is left: the `ps` line from **status** must print nothing.

**A killed gate leaves `main` provisional — and the runner repairs it itself at its next start.**
A bulk batch commits each merge before gating (marker `$HACKRIFF_OPS/bulk-in-progress`, with
`base=<sha>` and `branches=…`); a single merge stages `.git/MERGE_HEAD`. On startup
`ops/merge-runner.sh` aborts a staged merge, rewinds a provisional bulk to its base and re-queues
its branches, and logs `STARTUP: …` for each. So the repair is: **start the runner.** Only if it
logs "a person must look" (the tree has edits that are not the merge's own) do the manual steps
apply — `git -C /Users/daniellewis/hackriff merge --abort` / `reset --hard <base>` — and then
find whose edits those were before touching them.

### start
0. **Pre-flight.** `git -C $REPO branch --show-current` is `main`; no `MERGE_HEAD`, no
   `bulk-in-progress` (else repair as above); `git status --porcelain | grep -v '^??'` empty;
   `df -h /` ≥ 20 GB (else remove clean merged worktrees: `git worktree list`, then
   `git worktree remove <path>` for any with no commits ahead of main and a clean tree).
1. **State home.** `cp -Rn "$(cat ~/.hackriff-ops/active-ops-dir)/." ~/.hackriff-ops/ 2>/dev/null;
   export HACKRIFF_OPS=~/.hackriff-ops`.
2. **The five scripts, from the repo, never copies:**
   ```bash
   cd /Users/daniellewis/hackriff && export HACKRIFF_OPS=~/.hackriff-ops
   nohup bash ops/stage.sh          >/dev/null 2>&1 & disown
   nohup bash ops/merge-runner.sh   >/dev/null 2>&1 & disown
   python3 ops/work-runner.py --once --dry-run        # read what it would dispatch; stop here if it looks wrong
   nohup python3 ops/work-runner.py >/dev/null 2>&1 & disown
   MONITOR_PORT=8901 nohup python3 ops/monitor.py >$HACKRIFF_OPS/monitor.log 2>&1 &
   nohup python3 ops/watchdog.py    >/dev/null 2>&1 & disown   # contention watchdog; see ops/README.md
   ```
   **Never from a worktree**, not even for a change that has not landed: the runner removes the
   worktree when its branch lands and the running script loses its own files (2026-09-24: /flow
   answered `FileNotFoundError: .../worktrees/pm-dashmem/ops/monitor.py`). Each script logs
   `PATH:` at start and refuses (`REFUSED:`, exit 2) under `.claude/worktrees/`.
3. **Verify** (about a minute later), and report each line:
   ```bash
   grep VERSION $HACKRIFF_OPS/merge-runner.log | tail -1     # "matches"
   tail -3 $HACKRIFF_OPS/work-runner.log                      # "VERSION: matches", then DISPATCH/tick lines
   tail -2 $HACKRIFF_OPS/stage.log                            # "started (live)" or "(replay …)"
   curl -s http://127.0.0.1:8901/burndown.json | head -c 60   # dashboard answers
   head -c 200 $HACKRIFF_OPS/watchdog.json                    # a tick with owners + load/budget
   ```
   The work runner's first tick can take minutes: each dispatch clones `target/` (large tree).
4. **The coordinator, last:** `ops/launch.sh coordinator`. Its role file must be the one on
   `main`; if it starts spawning workers for ordinary `todo` tickets, its role file is stale —
   stop it, check `.claude/roles/coordinator.md` carries the "Dispatch … is the work runner's job"
   paragraph.
   Then (and the pipeline manager, `ops/launch.sh pipeline-manager`, if it is not running)
   `rm -f $HACKRIFF_OPS/roles-stopped` — only now may the watchdog relaunch a session that dies.
5. Tell the user what is running, what is queued, what the first dispatches were, and anything
   in either attention file.

### restart
`stop` (waiting for the gate if at all possible), then `start`.

### restart <script>   (one script: `merge-runner` | `work-runner` | `watchdog` | `monitor` | `stage`)
The pipeline manager's per-script restart (`.claude/pipeline/workflows/restart-an-ops-script.md`
says what each one interrupts). Every start sources the knob store `$HACKRIFF_OPS/env` (`just knobs
show`) so an experiment's setting survives the restart; the process environment still wins.
```bash
export HACKRIFF_OPS=~/.hackriff-ops; cd /Users/daniellewis/hackriff
set -a; . $HACKRIFF_OPS/env 2>/dev/null; set +a
# merge-runner: REFUSE while a gate runs or a merge is staged - its startup rewinds a provisional batch
if [ "$1" = merge-runner ]; then ls $HACKRIFF_OPS/bulk-in-progress .git/MERGE_HEAD 2>/dev/null && { echo "gate running - wait for MERGED"; exit 1; }; fi
pkill -f "ops/$1" 2>/dev/null; sleep 2
case "$1" in
  merge-runner) nohup bash ops/merge-runner.sh >/dev/null 2>&1 & disown ;;
  work-runner)  nohup python3 ops/work-runner.py >/dev/null 2>&1 & disown ;;
  watchdog)     nohup python3 ops/watchdog.py >/dev/null 2>&1 & disown ;;
  monitor)      MONITOR_PORT=8901 nohup python3 ops/monitor.py >$HACKRIFF_OPS/monitor.log 2>&1 & disown ;;
  stage)        nohup bash ops/stage.sh >/dev/null 2>&1 & disown ;;   # restarts the :8899 demo - only when the user is not on it
esac
sleep 4; pgrep -fl "ops/$1"; grep -E 'PATH:|REFUSED|VERSION|KNOBS' $HACKRIFF_OPS/${1}.log 2>/dev/null | tail -3
```
Report the `PATH: /Users/daniellewis/hackriff/ops/…`, `VERSION: matches …` and `KNOBS: …` lines; a `STALE`/`DIFFERS` version means the script on disk is not what `main` has.

## Rules that bind this skill's session too
- Never edit, `stash`, `reset` or commit in the main checkout except the documented rewind, and
  never while a gate runs (root `CLAUDE.md`, Coordination). Your own changes go on a branch in
  `.claude/worktrees/<name>` and into `merge-queue.txt` — **append, never rewrite**.
- Use `git -C /Users/daniellewis/hackriff …` for anything aimed at `main`; a harness can pin the
  shell's cwd inside a worktree and a bare `cd` may not take (a `reset --hard` meant for `main`
  once landed on a feature branch this way).
