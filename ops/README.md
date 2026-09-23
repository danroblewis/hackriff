# `ops/` — dev-orchestration scripts

Long-running helpers that run the autonomous dev setup. They are **not** product code and
nothing in the build depends on them; they are committed here only so a machine restart can
never lose them again (they used to live in a `/tmp` scratchpad that a reboot wiped).

Runtime state (logs, tokens, the demo build, the merge queue) lives in **`$HACKRIFF_OPS`**,
default **`~/.hackriff-ops`** — deliberately outside `/tmp` so it survives a reboot. Override
with `HACKRIFF_OPS=/some/dir` if you want it elsewhere. The five scripts share that one dir.

Environment-specific constants at the top of each file (edit for a different machine/checkout):
`REPO` (the repo path, `/Users/daniellewis/hackriff`); in `monitor.py` also `PROJ` (the Claude
projects dir), `COORD` (the coordinator's conversation id), and `SUPER`.

## The five scripts

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
**Every run is accounted for (2026-09-23):** each tick samples the claim's whole process
TREE - not `ps -g <pgid>`, which on this box contains only the `cpulimit` wrapper - and keeps the
peak, so `work-claims.json`, `work-done.jsonl` and the ticket's `result:` all carry `cpu_s` and
`peak_rss_mb` ("Resources: 412 CPU-s, peak 1.9 GB"). CPU time cannot be read at reap (the kernel
discards it when the root exits), so these are a floor, not an exact total. At reap, anything of
the run still alive is a LEAK - it cannot be found by ancestry, because being reparented to
launchd *is* the leak, so it is matched by a pid+group the run was seen holding or by its
worktree path - and is killed (SIGTERM, 10 s, SIGKILL), noted in `work-needs-attention.txt` and
in the result.
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

**Discord alerts (user, 2026-09-23).** `ops/alert.py <red|amber|green|info> "<title>" "<body>" [--key K]`
posts to the `#hackriff` channel and mentions the user; both runners call it — amber for every
exception handed to a person (gate red, conflict, gave-up, MAIN_RED, suite-broken, stale merge,
BLOCKED/ERROR/REVIEW_FAIL), red for a `GATE_TIMEOUT` kill, green for each landing. Config is
`$HACKRIFF_OPS/discord.json` (mode 600, never in the repo): `{"token", "channel_id", "guild_id",
"mention_user_id"}`. Same `--key` within 30 min is deduped; every attempt is recorded in
`$HACKRIFF_OPS/alerts.jsonl`; a failed post never fails the caller. `HK_ALERT_OFF=1` silences it.
**Pipeline alarms also wake the pipeline manager:** a key starting `watchdog:`, `mr:`, `hold:`,
`timeout:`, `contended-gate` or `flake:` is typed into its tmux session `flow` (`HK_PM_SESSION`)
as a message, once per key per 30 min (its own dedupe, recorded as `woke` in `alerts.jsonl`, so a
failing Discord cannot turn a looping condition into a message every tick); no session, no wake.
**The flow digest:** `just flow --record --digest` (the pipeline manager's tick) posts the tick line
green under key `flow:digest` when 2 h have passed since the last one, and at once, amber, on a
trend break against the record ~2 h earlier — landings/h (6 h) halved from ≥ 0.5, real reds (24 h)
doubled by ≥ 2, a new touchpoint, or a hold in force — each under `flow:break:<kind>`.

**The gate shares the box (default since 2026-09-23 13:30; user).** Workers keep dispatching while a
gate runs, capped at the gate's reserve (`(WORK_CORES − WORK_GATE_RESERVE) / WORK_WORKER_CORES` = 4)
instead of `WORK_CAP`; the merge runner gates whatever is queued the moment the previous gate ends
(`WORKER_DRAIN_MAX=0`) and never waits for a claimed worker — only for a foreign spec run / `hk serve`
(they share its lane ports) or watchdog contention, at most `FOREIGN_DRAIN_MAX`. `WORK_QUEUE_PAUSE`
is inert in this mode. Why: the alone-mode cycle below alternated 45-min gates with 45-min drains;
on 2026-09-23 dispatch was zero in 10 of 13 hours and landings fell to ~1/hour once the crisis backlog
drained, while the three flake causes the rule was bought for had been fixed at the root (a hidden
tab's stopped rAF in `surface-contention`, a spec port shared by `fog-of-war`/`scan-everything`, a
self-matching `pgrep` wait loop). Every gate in this mode is honestly marked contended in its timing
record. `WORK_GATE_ALONE=1` on the work runner plus `WORKER_DRAIN_MAX=2700` on the merge runner
restore the cycle wholesale.

**The gate runs alone (user, 2026-09-22; now opt-in, above).** The bounded budget was not enough: the SDET review
measured untouched crates of small unit tests running 18–79× dearer during shared gates. So the two
runners cycled: the work runner **stops dispatching** once `WORK_QUEUE_PAUSE` (6) branches wait
in `merge-queue.txt`, and while a gate runs; running workers finish and join the queue (nothing is
suspended); the merge runner **starts a gate only when the claims file shows no running worker**
(`workers_drained`, capped at `WORKER_DRAIN_MAX` = 45 min so a stuck worker cannot hold every merge);
when the batch lands the queue drops below the threshold and dispatch resumes. `just gate` therefore
measures the code, not the neighbours. While the merge runner waits for that drain it holds
`$HACKRIFF_OPS/gate-wanted`, which the work runner reads as a running gate. **A full stop is a
file:** `touch $HACKRIFF_OPS/dispatch-paused` stops every dispatch until the file is removed
(reaping, results and queueing continue) — the conditional holds each have a window, this has none.
A batch that goes red **without a test FAIL** (lint, a build error, the UI unit step) is re-queued
in order and held until the queue changes, never isolated: main+batch is broken as a whole and every
isolated gate would reproduce it. Before isolating a red batch the runner re-runs the failing tests
(or browser specs) on the rewound `main`; if `main` itself is red it holds the batch (`MAIN_RED`)
instead of re-proving the defect once per branch. **Every gate has a hard time limit** —
`GATE_TIMEOUT` (default 3600 s): past it the gate's whole process group is killed, the batch is
re-queued once and flagged `GATE_TIMEOUT`. **The runner repairs its own leftovers at startup**: a
staged merge or a provisional bulk that a killed gate left on `main` is aborted / rewound and
re-queued automatically — nobody types `git merge --abort` any more.
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

### `watchdog.py` — resource-contention watchdog (the fifth script, 2026-09-23)
The budget above is only a budget if something checks it, and nothing did. On **2026-09-22** a
deflaker agent exited and left **sixteen** `/bin/zsh -c source …/shell-snapshots/snapshot-zsh-….sh`
busy loops reparented to launchd, each at 100 % CPU, from 14:29 to 16:47 — through every merge
gate in those two and a quarter hours. In the same window a killed merge runner left an orphan
`just gate` running beside its replacement's, and `ops/monitor.py` sat at 440 % CPU. None of it
appeared in a log, on the dashboard or in an alert: it was found because a person ran `ps`. Each
runner knows only its own children, so **no runner can see this**; the watchdog is the one process
whose subject is the whole box.

Every 20 s it builds a process table (`ps -axo pid,ppid,pgid,pcpu,rss,etime,command`) and
attributes every process to an **owner** — a worker (its claim's pid is its process group, or an
ancestor of it), the merge runner, the **gate** (its own owner even under the runner, because the
14-core reserve is the gate's), a role session (named from `HACKRIFF_ROLE`, else its
`--append-system-prompt-file` role file), the demo, the dashboard, the runners, the fuzz rig, plus
`sccache` / `system` / `apps` / `tunnel` / `claude-other` as **fallbacks applied only after
ancestry fails**. Anything left is **UNOWNED**. Ancestry always answers before a command-line
guess, so a worker's `rustc`, `git` and `/bin/zsh` stay the worker's, and only a process whose
ancestors are all gone can be unowned. Five rules:

| | condition | action |
|---|---|---|
| a | an UNOWNED process >90 % CPU for >120 s | amber, keyed per pid |
| b | an UNOWNED `shell-snapshots/snapshot-zsh` shell >50 % for >600 s | **SIGKILL** + red |
| c | more than one merge gate running | red |
| d | `ops/monitor.py` >200 % CPU or >1.5 GB for >120 s | amber |
| e | load1 over the plan (owners' budgets, capped at the core count, +4) for >5 min | amber + top 5 |

**Rule (b) is the only thing it kills**, and only on that signature, only when unowned, only
sustained: a live agent's shell has a live parent, so it is *owned* and can never match. Every
kill is logged to `watchdog.log` with its full command line. Everything else is an alert through
`ops/alert.py` (deduped 30 min per key). The last tick is `$HACKRIFF_OPS/watchdog.json`, which
`ops/monitor.py` renders as the **Box** line in the System card — owners with CPU, unowned in red,
and "no watchdog running" when the file is missing or stale.
```bash
HACKRIFF_OPS=~/.hackriff-ops nohup python3 ops/watchdog.py >/dev/null 2>&1 & disown
python3 ops/watchdog.py --once --print --dry-run   # one tick to stdout; never kills, never alerts
cat $HACKRIFF_OPS/watchdog.json   ·   cat $HACKRIFF_OPS/watchdog.log
```
Rules and attribution are pure functions over a list of rows, tested against a synthetic process
table in `py/tests/test_watchdog.py` — including the sixteen-loop incident, which cannot be
reproduced on demand.

**Two cheaper guards sit in front of it**, so most of this never has to be caught after the fact:

* **`.claude/hooks/reap-agent-processes.sh`**, wired to `Stop` and `SubagentStop`. The moment an
  agent finishes, it kills any `shell-snapshots/snapshot-zsh` shell that is **orphaned** (ppid 1,
  or a parent that is gone) **and** above 50 % CPU, and records the reap in the agent's transcript
  as a `systemMessage`. A live agent's shell has a live parent and is never touched. Fail-open on
  every error path.
* **`.claude/hooks/block-full-gate.sh`** now also refuses, in *every* session, to run a busy loop
  (`while :; do :; done` — polling with a `sleep` in the body is fine), `yes`, `stress`/`stress-ng`,
  more than two backgrounded loops in one command, and `npm run e2e` / `node e2e/run.mjs` /
  `hk serve` while a gate is running or `bulk-in-progress` exists. `HK_ALLOW_LOAD=1` overrides,
  for a genuine contention repro. `py/tests/test_hooks.py` runs the scripts as the harness does.

**`.claude/settings.json` sets `CARGO_BUILD_JOBS=3`, `NEXTEST_TEST_THREADS=2` and
`CARGO_INCREMENTAL=0` for every Claude Code session and subagent on this box.** That is where the
bound actually binds: `cpulimit` wraps a worker's *tree*, but an agent typing `cargo build -j 28`
inside that tree still oversubscribes the scheduler. It overrides the work runner's `CARGO_ENV`
(`CARGO_BUILD_JOBS=2`) inside a worker session — both are at or below the 3-core `cpulimit` bound,
so the effective ceiling is unchanged. The **merge runner is not a Claude session** and keeps its
own `CARGO_BUILD_JOBS=6`, which `py/hkpy/gate.py` sets explicitly for the suites it launches.

## The pipeline manager, the knob store and the bounded hold (2026-09-23)

A fourth role, **the pipeline manager** (`ops/launch.sh pipeline-manager`, tmux `flow`; role
`.claude/roles/pipeline-manager.md`, invariants `.claude/rules/pipeline-invariants.md`, workflows
`.claude/pipeline/workflows/`), owns *throughput*: landings per hour at constant gate honesty. It
ticks every 30 minutes, measures with `just flow`, runs one experiment at a time through `just
experiment` (ledger: `docs/ops-experiments.md`), and changes the pipeline through branches like
everyone else. The coordinator owns the backlog; the supervisor owns the user's intent; this role
owns the rate. Why: on 2026-09-23 the burndown went flat at ~1 ticket/hour with the box idle half
of every hour, and nobody's job was to see that in numbers.

- **`just flow [--hourly|--gates|--tickets|--touchpoints] [--since 24h] [--record]`** — where the
  hours went, from `merge-runner.log`, `work-runner.log`, `landed.jsonl`, `handbacks.jsonl`;
  `--record` appends to `$HACKRIFF_OPS/flow.jsonl`. Read-only.
- **`just knobs show|set K=V|unset K|reset`** — the persistent knob store `$HACKRIFF_OPS/env`
  (`KEY=VALUE`), which **both runners, `ops/launch.sh` and `/dev-env restart` read on start**
  (process environment wins). `show` reports the *effective* value from each runner's own `KNOBS:`
  start line. Refuses `WORK_CAP=0`. Every change is a line in `env.jsonl`, tagged with the open
  experiment or as an incident change.
- **`just hold --minutes N --why "…"` / `--release` / `--status`** — a merge-queue hold that is a
  marker with an expiry (`$HACKRIFF_OPS/hold`): at most 30 minutes, no second within 2 hours,
  refused while a branch is queued or a gate runs; the merge runner ignores it once expired and
  **ends it at the first queued branch**, alerting. Every hold is charged to the open experiment
  as blocked minutes. Dispatch is never held for measurement (the floor is `WORK_CAP=1`).
- **`just experiment new|status|close|list`** — one open at a time; `new` needs a rollback line
  and snapshots the baseline; `close` refuses `keep` when a guard is broken.

## Starting the orchestration environment (cold start, reboot, or "stop everything and restart")

Order matters: **runners before the coordinator**, and the coordinator **last**, because it reads
`main`'s board and role file at launch and starts acting on them. Each step says what to verify.

### 0. Stop what is running (skip on a fresh boot)
```bash
OPS=$(cat ~/.hackriff-ops/active-ops-dir)                 # where the running system keeps its state
tmux kill-session -t dev                                  # the coordinator + every subagent it spawned
pkill -f 'ops/merge-runner.sh'; pkill -f 'just gate'; pkill -f 'cargo-nextest nextest run'
pkill -f 'stage.sh'; pkill -f 'hk serve --bind 127.0.0.1:8899'
pkill -f 'monitor.py'; pkill -f 'work-runner.py'; pkill -f 'ops/watchdog.py'
ps -axo pid,command | grep -E 'Role: Coo|merge-runner|just gate|stage.sh|monitor.py|work-runner|watchdog.py|hk serve' | grep -v grep   # must print nothing
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

### 2. Start the five scripts, always from the repo (never a copy)
```bash
cd /Users/daniellewis/hackriff && export HACKRIFF_OPS=~/.hackriff-ops
nohup bash ops/stage.sh          >/dev/null 2>&1 & disown        # demo :8899, live HackRF or replay
nohup bash ops/merge-runner.sh   >/dev/null 2>&1 & disown        # the sole merger to main
python3 ops/work-runner.py --once --dry-run                       # READ what it would dispatch first
nohup python3 ops/work-runner.py >/dev/null 2>&1 & disown        # dispatch
MONITOR_PORT=8901 nohup python3 ops/monitor.py >$HACKRIFF_OPS/monitor.log 2>&1 &   # dashboard :8901
nohup python3 ops/watchdog.py    >/dev/null 2>&1 & disown        # contention watchdog
```
Verify, a minute later:
```bash
grep VERSION $HACKRIFF_OPS/merge-runner.log | tail -1   # "matches HEAD:ops/merge-runner.sh"
tail -3 $HACKRIFF_OPS/work-runner.log                    # "VERSION: matches" then "DISPATCH T-…"
tail -2 $HACKRIFF_OPS/stage.log                          # "started (live)" — or "(replay …)" if the HackRF is busy
curl -s http://127.0.0.1:8901/burndown.json | head -c 80 # dashboard answers
python3 -c 'import json;d=json.load(open("'"$HACKRIFF_OPS"'/watchdog.json"));print(d["load"],d["budget"],list(d["owners"])[:5])'
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
