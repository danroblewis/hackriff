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
**The worker's output contract is a file:** its last step writes `work/<ticket>/handback.json`
(`outcome: done|blocked|cancel`, `summary`, `commits`, `files`, `tests[{cmd,exit,summary}]`,
`precheck`, `blocked.needs`, `cancel.evidence`, `observed_but_not_chased`, `use_cases`). The runner
validates it, refuses `done` over a failing test, writes the ticket's `result:` (and a cancel's
status) on the worker's branch through `just task`, routes on `outcome` (cancel → an Opus review
confirms the evidence), and only then queues the branch. Workers never edit `docs/tasks.yaml`.
**The resource model is a fixed budget (user, 2026-09-22).** 28 cores: the merge gate is reserved
14 (`WORK_GATE_RESERVE`), each worker is bounded to ~3 (`WORK_WORKER_CORES`) by limits its whole
process tree inherits — `CARGO_BUILD_JOBS=2` and `NEXTEST_TEST_THREADS=2` in the environment, and a
permanent `taskpolicy -c background` QoS clamp (efficiency cores only on Apple Silicon) — so the count
is `WORK_CAP` = (28 − 14) / 3 = 4 and the gate always has its reserve. No load heuristics, no gate-time
throttling, no suspending workers. The hard ceiling is **`cpulimit -l 300 -i` from the HiGarfield fork**
(`$HACKRIFF_OPS/bin/cpulimit`; source kept at `$HACKRIFF_OPS/src/cpulimit`; rebuild with
`cd $HACKRIFF_OPS/src/cpulimit && make install DESTDIR=$HOME/.local/bin && cp src/cpulimit $HACKRIFF_OPS/bin/` —
no sudo needed; do NOT `brew install cpulimit`, that is the inert opsengine build) — Homebrew's `opsengine` build is inert on Apple Silicon (measured 0 %),
the fork measured 164 % aggregate over four busy loops under `-l 200 -i`. The same bound wraps the
reviewer and fix/resume runs, and `ops/launch.sh` wraps the coordinator/supervisor session itself at
`ROLE_CPU_PCT` (default 800) so its subagents' builds and `hk serve` runs are bounded too. Budget of
28 cores: gate 14 + workers 4×3 + role session 8 = 34 at peak, which the QoS tiers arbitrate; a
sustained load above ~28 means a bound is not holding. Disk floor 20 GB.
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

## Starting the orchestration environment (cold start, reboot, or "stop everything and restart")

Order matters: **runners before the coordinator**, and the coordinator **last**, because it reads
`main`'s board and role file at launch and starts acting on them. Each step says what to verify.

### 0. Stop what is running (skip on a fresh boot)
```bash
OPS=$(cat ~/.hackriff-ops/active-ops-dir)                 # where the running system keeps its state
tmux kill-session -t dev                                  # the coordinator + every subagent it spawned
pkill -f 'ops/merge-runner.sh'; pkill -f 'just gate'; pkill -f 'cargo-nextest nextest run'
pkill -f 'stage.sh'; pkill -f 'hk serve --bind 127.0.0.1:8899'
pkill -f 'monitor.py'; pkill -f 'work-runner.py'
ps -axo pid,command | grep -E 'Role: Coo|merge-runner|just gate|stage.sh|monitor.py|work-runner|hk serve' | grep -v grep   # must print nothing
```
**If a gate was killed mid-run, `main` is provisional.** A bulk batch commits each merge before
gating (`$OPS/bulk-in-progress` names the pre-batch `base=` sha and the branches); a killed gate
leaves those merges on `main` ungated. Rewind and re-queue them so one batch gates everything:
```bash
cd /Users/daniellewis/hackriff && cat $OPS/bulk-in-progress          # base=<sha> ... branches=...
git status --porcelain | grep -v '^??'                               # must be empty (else: git merge --abort)
git reset --hard <base sha>                                          # the same rewind the runner does itself
rm -f $OPS/bulk-in-progress
printf '%s\n' <the branches from the marker> >> $OPS/merge-queue.txt
```
A single staged (non-bulk) merge shows as `.git/MERGE_HEAD`: `git merge --abort` and re-queue the branch.

### 1. Put the state where a reboot cannot lose it
`$HACKRIFF_OPS` must be **`~/.hackriff-ops`**. If the last run kept it elsewhere (a `/private/tmp`
scratchpad, which a reboot wipes — this happened), copy it over once; `-n` never overwrites:
```bash
cp -Rn "$(cat ~/.hackriff-ops/active-ops-dir)/." ~/.hackriff-ops/ 2>/dev/null
export HACKRIFF_OPS=~/.hackriff-ops
```
Every script below rewrites `~/.hackriff-ops/active-ops-dir` to point at itself on start.

### 2. Start the four scripts, always from the repo (never a copy)
```bash
cd /Users/daniellewis/hackriff && export HACKRIFF_OPS=~/.hackriff-ops
nohup bash ops/stage.sh          >/dev/null 2>&1 & disown        # demo :8899, live HackRF or replay
nohup bash ops/merge-runner.sh   >/dev/null 2>&1 & disown        # the sole merger to main
python3 ops/work-runner.py --once --dry-run                       # READ what it would dispatch first
nohup python3 ops/work-runner.py >/dev/null 2>&1 & disown        # dispatch
MONITOR_PORT=8901 nohup python3 ops/monitor.py >$HACKRIFF_OPS/monitor.log 2>&1 &   # dashboard :8901
```
Verify, a minute later:
```bash
grep VERSION $HACKRIFF_OPS/merge-runner.log | tail -1   # "matches HEAD:ops/merge-runner.sh"
tail -3 $HACKRIFF_OPS/work-runner.log                    # "VERSION: matches" then "DISPATCH T-…"
tail -2 $HACKRIFF_OPS/stage.log                          # "started (live)" — or "(replay …)" if the HackRF is busy
curl -s http://127.0.0.1:8901/burndown.json | head -c 80 # dashboard answers
```
If a script's newest version is only on an unmerged branch, start it from that branch's worktree
(`.claude/worktrees/<name>/ops/<script>`) and restart it from `main` once the branch lands.

### 3. The coordinator, last
```bash
ops/launch.sh coordinator          # tmux session 'dev', in-role via .claude/roles/coordinator.md
```
Its first tick runs `just reconcile` and reads `$HACKRIFF_OPS/merge-needs-attention.txt` and
`work-needs-attention.txt`. It does **not** dispatch ordinary tickets — the work runner does — so if
it starts spawning workers for `todo` tickets, its role file is stale: stop it and check that
`.claude/roles/coordinator.md` on `main` carries the "Dispatch is … the work runner's job" paragraph.

Then, if needed: `nohup cloudflared tunnel --url http://127.0.0.1:8901 &` for the dashboard.
