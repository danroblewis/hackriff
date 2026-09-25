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

**Role sessions (2026-09-25).** `ops/launch.sh <role>` starts claude with `--session-id <uuid>` (a
`--resume <id>` keeps that id) and writes it to `$HACKRIFF_OPS/role-session/<role>` (the coordinator
also to `coordinator-session`). The dashboard's agents panel names roles only from those files and
shows a session as live only while a claude process carries its id (`ps`, or Claude Code's
`~/.claude/sessions/<pid>.json`); otherwise `ended hh:mm`, hidden 5 min later. `COORD` is only the
fallback until `role-session/coordinator` exists.

## The five scripts

### `stage.sh` — staging demo watcher (port 8899)
Rebuilds the `hk` binary and restarts the "bears" demo on every **code** commit to `main`
(ignores docs/tasks-only commits), smoke-tests it, and keeps a cloudflared tunnel up. Prefers
the live HackRF; falls back to a looping SigMF replay when the device is busy. Also self-heals:
if the live spectrum stream dies it restarts.

**The radio lock (T-922).** One owner of the HackRF at a time, recorded in
**`$HACKRIFF_OPS/radio-lock`** — `key=value` lines `owner=`, `since=` and `until=` (epoch seconds),
`why=`. Past `until` a lock is **stale**. It is managed only through `just radio`
(`py/hkpy/radio.py`, tested in `py/tests/test_radio.py`):
```bash
just radio take <owner> <duration e.g. 3h|90m|2h30m> <why...>   # refuses while a live lock is held (even your own)
just radio release <owner>                                       # releases only <owner>'s lock
just radio status                                                # holder, until, and the staging mode
```
`stage.sh` respects it. While the lock is held by anyone other than `stage` it **never opens the
HackRF** and serves its looping SigMF replay; each 45 s tick it notices a lock taken while live
(stops the live server, restarts on replay — so a new owner waits up to ~1 min, until `just radio
status` shows `staging: replay (radio-lock: …)`) and a lock released while on a lock-driven replay
(back to **LIVE**). Both transitions are logged in `stage.log`, and the mode is in
`$HACKRIFF_OPS/hk-serve-source` (`live`, `replay (radio-lock: <owner> until HH:MM)`, `replay (hackrf
busy)`), which the dashboard and `just radio status` show. Its busy check reads **`hackrf_info`'s
output, not its exit status**: the tool exits 0 even when the open fails (`Found HackRF … hackrf_open()
failed: Access denied`, T-356's HIL), so the radio counts as free only with `Found HackRF` and no
`failed`/`Access denied`/`busy`/`No HackRF` line. The watchdog releases a stale lock (rule g, below);
the capture-agent and the explorer take and release it.
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
**Role work log — `/worklog`** (`ops/worklog.py`, linked "work log ↗" in the top bar): for each
role session (pipeline manager, coordinator, supervisor) the final assistant message of every
turn, newest first, as markdown, collapsed to its first line (click to expand), with a `flow:`
filter that lists just the pipeline manager's tick lines. It is how the user reads what the roles
did without running a command. Sessions are found from `~/.claude/sessions/<pid>.json` (role from
the process's `roles/<role>.md` argument or its tmux session `flow`/`dev`/`super`) and remembered
in `$HACKRIFF_OPS/role-sessions.json`; a role session started any other way — the supervisor in
the user's own terminal — is added there by hand: `{"<session id>": {"role": "supervisor",
"source": "manual"}}`. Transcripts are parsed once, then incrementally. Data: `/worklog.json`.

**Preview a dashboard branch — `bash ops/preview-dashboard.sh <branch> [PATH ...]`** (user,
2026-09-24: he does not wait behind a merge batch to see a dashboard change). Copies the branch's
committed `ops/` + `py/` out of git into `$HACKRIFF_OPS/preview` (`PREVIEW_DIR`) and runs that copy
on **:8902** (`PREVIEW_PORT`; :8901 is refused) with `MONITOR_PREVIEW=1`, which makes its child
builds run the copy's code, keeps its metrics cache in the preview directory and writes no daily
`metrics.jsonl` sample. It replaces whichever dashboard holds the port (one preview at a time; a
tunnel pointed at :8902 shows the newest), refuses a port held by anything else, verifies that ITS
pid is the listener, and prints `OK`/`FAIL` with the size for each PATH (a JSON route's own
`"error"` is a FAIL). The real :8901 restarts from main when the branch lands.

**`/metrics` — code metrics** (`ops/metricspage.py` + `py/hkpy/codemetrics.py`, linked "metrics ↗"
in the top bar): lines per language / crate / area (product vs test), churn per area over 24h/7d/30d
and the hottest files, tests and test-seconds per crate from the gate's kept JUnit (seconds per 1k
lines), the largest files and longest Rust functions (a brace-depth proxy), hygiene (unsafe,
TODO/FIXME/XXX, the last lint's clippy warnings, `#[ignore]` by reason, quarantine) and daily
trends. All of it at committed main (the bulk base while a batch gates), rebuilt in a child
process only when that sha moves, cached in `$HACKRIFF_OPS/metrics-cache.json`; the first build
each day appends a sample to `$HACKRIFF_OPS/metrics.jsonl`. Each section prints its method's caveat.
`python -m hkpy.codemetrics` prints the same summary. Data: `/metrics.json`.

**`/flow` — the Flow panel** (linked from the top bar) gives the pipeline manager throughput
visibility without waiting on `flow.jsonl` to accumulate: landings/h as a rolling 6h/24h line
chart backfilled hourly from `hkpy.flow.hourly()` (with any real `flow.jsonl` ticks overlaid as
dots, and the open experiment's opening time + baseline landings/h marked), per-hour dispatch
and gate occupancy for the last 24h, per-gate durations by class over 48h (red gates marked,
against the open experiment's — or the window's — full-gate p50), red rate by cause, the last
24h of touchpoints, and the open experiment's guards and rollback. `/flow.json`
(`build_flow_panel` in `monitor.py`) reads only through `hkpy.flow`/`hkpy.experiment` — it never
re-parses the ops logs itself — and is cached ~30 s (`flow_panel_cached`) because
`hourly()`/`gate_rows()`/`touchpoints()` each re-read `merge-runner.log` in full (~500k lines);
an uncached build took ~1.2 s in testing, a cached one ~13 ms. Tests: `py/tests/test_monitor_flow.py`.

### `merge-runner.sh` — automated, no-AI merge runner
**Restart between gates on request:** `echo "<why>" > $HACKRIFF_OPS/merge-runner-restart` - the runner
re-executes the repo's copy of itself at the top of its loop, the only point with no gate running and
no merge staged, and logs `RESTART: requested (<why>)`. Use it after a runner change lands; never
kill the runner mid-gate for that.

**CHEAP FIRST (user, 2026-09-24):** when the queue holds both kinds, the branches whose diff
classifies as anything but `full` (`hkpy.gatepri`, the gate's own `classify`) — a dashboard or
pipeline branch is `py+ops`, a UI one `ui` — are the next attempt, by themselves, and the rest go
back in queue order: a two-minute py+ops gate no longer waits for, or rides, a 30-minute full batch.
It narrows no gate (the attempt is `just gate`, classified as always); it logs `CHEAP FIRST: …` with
each branch's class, and forms the batch as before whenever classification fails.

**Red triage (the user's rule, 2026-09-23):** on a red, the failing tests or browser specs are re-run
ALONE. Pass alone **twice** → a load flake: that suite passes on the evidence, and the gate resumes
after the suite that stopped it (`just gate … --resume-after <suite>`, a strict suffix of the
classified suites — nothing that ran is re-run, nothing that did not is skipped; nextest runs with
`fail-fast = false` so a red suite's other tests all ran, and the workspace-test recipe runs
`test-rust` last). Fail alone on either run → unchanged: a real red (isolate, MAIN-IS-RED check). Every
acceptance is a `flaky.jsonl` record (`accepted`, `suite`, `saved_s` against the old retry), an amber
alert, a line in the 2-hourly digest; the 3rd in 7 days for one test writes
`deflake-requests.jsonl`, which the work runner dispatches as a `deflaker`.

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
**Deflakers are dispatched from the flake ledger (user, 2026-09-23: the 3rd flake of a test in 7 days
auto-spawns a deflaker).** `py/hkpy/flakes.py` appends one JSON line per due test to
`$HACKRIFF_OPS/deflake-requests.jsonl` (`ts`, `id`, `test`, `kind` rust|spec, `count_7d`, `incidents`,
`evidence`); each tick the runner reads it (garbage lines skipped), and for a request newer than the
one its claim last consumed it launches `claude -p --agent deflaker` (opus/high, the same `bounded()`
cpulimit + QoS wrapper, `CARGO_ENV` and budget as a worker) in `.claude/worktrees/<slug>` on
`task-<slug>` (`-r<n>` for a later run), cut from `merge_target()` — `main`, or the bulk marker's
`base=` while a batch gates. The brief carries the test, the incidents and evidence verbatim, and the
triage rules: alone first; fails alone → hand back BLOCKED; passes alone → a deterministic fix (never a
retry, skip, quarantine, timeout change or deleted assertion) proven red with the defect back. The claim
is keyed **`DEFLAKE:<slug>`**, never a T-id, and carries `deflake: <slug>`, so board sync, candidates
and stale-claim release (all looked up by board id) never see it and it never gets a `result:` block or a
fix resume. It is a worker: `busy_workers()` counts kind `deflake` against the same `dispatch_cap()`, at
most one deflake dispatch per tick (before ordinary dispatch), never while `dispatch-paused` exists or
`gate_holds_dispatch()`, never under the disk floor. One open deflaker per id: a new request waits
while the previous run is running or its branch is queued and unmerged (`DEFLAKE WAIT`, logged once);
after that, a request whose `ts` is at or before the claim's `ended` (the run's end, or when its branch
landed) is dropped as evidence from before the fix (`DEFLAKE DROP`, logged once). Reap:
commits ahead and a clean tree → an Opus review (told to FAIL any masking) → the merge queue;
otherwise one line in `work-needs-attention.txt` — `DEFLAKE_NO_WORK`, `DEFLAKE_BLOCKED` (with the
hand-back summary), `DEFLAKE_ERROR`, `DEFLAKE_UNCOMMITTED`, `DEFLAKE_REVIEW_FAIL`, or `DEFLAKE_GATE_FAIL`
for a queued deflake branch that goes red (escalated to a person, not resumed). A CONFLICT line takes
the ticket path's `conflict_skip` (a branch that merges cleanly now is re-queued); only where a ticket
would get a fix run is it escalated, as `DEFLAKE_CONFLICT`.
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
reviewer and fix/resume runs, and `ops/launch.sh` bounds each role session itself at
`ROLE_CPU_PCT` (default 800) so its subagents' builds and `hk serve` runs are bounded too. It
**attaches** the limiter (`cpulimit -i -p <pane pid>`, detached, named `limiter` by the watchdog) to a session
`exec`ed into the pane, and never wraps it: a wrapped child runs in its own process group, not the
pane's foreground one, and stops on SIGTTIN at its first terminal read (2026-09-23: state T, no prompt). It bounds
the session's descendants (subagent shells and their builds); claude's own node process is resumed by
tmux whenever the limiter stops it, so its CPU counts against the ceiling without being throttled. Budget of
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
A batch that goes red **without a test FAIL** in `just lint` or `just test-ui` is **bisected by that
check** (`just lint` = fmt + clippy, a compile check, on base + half the batch, log2(n) probes): main red
on it -> `MAIN_RED`; one branch red alone twice -> set aside as `GATE_FAIL`, the rest re-queued first;
no single culprit (a pair conflict) -> re-queued once with the last red subset named (`CHECK_PAIR`),
isolated the next time. Any other no-FAIL red, or a bisect that gives up, is re-queued in order and held
until the queue changes (`SUITE_BROKEN`), never isolated. Before isolating a red batch the runner re-runs the failing tests
(or browser specs) on the rewound `main`; if `main` itself is red it holds the batch (`MAIN_RED`)
instead of re-proving the defect once per branch. **Every gate has a hard time limit** —
`GATE_TIMEOUT` (default 3600 s): past it the gate's whole process group is killed, the batch is
re-queued once and flagged `GATE_TIMEOUT`. **The runner repairs its own leftovers at startup**: a
staged merge or a provisional bulk that a killed gate left on `main` is aborted / rewound and
re-queued automatically — nobody types `git merge --abort` any more.
**Remote hosts' repos are kept identical to this Mac's (invariant 29, `py/hkpy/reposync.py`):** every tick fetches each host mirror's `task-*` branches and fast-forwards them here (or pushes this Mac's commits there, fast-forward only), the host clone's hooks push every commit, and a hand-back whose branch differs between the host, its mirror and this Mac is held as `SYNC_ERROR`, never judged `NO_WORK`; `python -m hkpy.reposync --status` prints the drift read-only.
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
| g | `$HACKRIFF_OPS/radio-lock` past its `until` (T-922) | **release the lock** + red |
| h | an UNOWNED build/test process (cargo, nextest, non-sccache rustc, `hk serve`, ui/e2e node, a worktree `target/` binary, or a shell wrapping one) in this repo's `.claude/worktrees/<name>` (command line or one `lsof` cwd; either one protected is enough) with no running/fix-held/limited claim on any host, no owned process there and no git activity there (mtime of its admin dir's `index`/`HEAD`/`logs/HEAD`, found from the worktree's `.git` file) within the hold, >1800 s — the 2026-09-25 t901 15-h cargo wrapper and the 09:34 killed sessions' nextest runs | **SIGTERM, then SIGKILL** + red |

**Rules (b) and (h) are the only things it kills** ((h) re-reads ps and the claims before each signal) (rule g removes a file, never a process: an owner that overran
its window or died holding the radio would otherwise keep staging on replay indefinitely), and only on those shapes, only when unowned, only
sustained: a live agent's shell has a live parent, so it is *owned* and can never match. Every
kill is logged to `watchdog.log` with its full command line. Everything else is an alert through
`ops/alert.py` (deduped 30 min per key). The last tick is `$HACKRIFF_OPS/watchdog.json`, which
`ops/monitor.py` renders as the **Box** line in the System card — owners with CPU, unowned in red,
and "no watchdog running" when the file is missing or stale.
**Role-session liveness (incident 2026-09-24 04:07: a `pkill` took `dev` and `flow` down unnoticed
for 5.5 h).** At most once a minute it checks each `LIVE_ROLES` session in `ops/roles.py` (coordinator
`dev`, pipeline manager `flow`): alive = `tmux has-session -t =<s>` and a `claude` process at or below a
live pane pid; `#{pane_dead}`=1 is dead, a pane pid missing from ps is unknown (skipped). Each miss is red
(`watchdog:liveness:<role>`); the 2nd consecutive miss re-reads ps, logs the pane's last 40 lines, kills a
claude-less session and runs `ops/launch.sh <role>` (red `watchdog:relaunch:<role>`), at most once per role
per 10 min and never while `$HACKRIFF_OPS/roles-stopped` or `dispatch-paused` exists (a deliberate stop:
alert only). After 3 relaunches that came back dead it gives up and keeps alerting. `--dry-run` only logs.
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
**Never start an ops script from a worktree** - not even to try a change that has not landed.
The runner removes a worktree when its branch lands, and a script running from one loses its own
files: on 2026-09-24 the dashboard, started from `pm-dashmem`, answered /flow with
`FileNotFoundError: .../worktrees/pm-dashmem/ops/monitor.py`. Restart with `/dev-env restart
<script>`, which runs `REPO/ops/<script>`; a change reaches the running script by landing first.
Each script logs `PATH: <where it runs from>` at start, and refuses (exit 2, `REFUSED:`) under
`.claude/worktrees/` (`ops/launchpath.py`, `ops/launch-guard.sh`). That includes the one-shot diagnostics (`watchdog.py --once --print`,
`work-runner.py --once --dry-run`): run them from the repo too.

### 3. The coordinator, last
```bash
ops/launch.sh coordinator          # tmux session 'dev', in-role via .claude/roles/coordinator.md
```
Its first tick runs `just reconcile` and reads `$HACKRIFF_OPS/merge-needs-attention.txt` and
`work-needs-attention.txt`. It does **not** dispatch ordinary tickets — the work runner does — so if
it starts spawning workers for `todo` tickets, its role file is stale: stop it and check that
`.claude/roles/coordinator.md` on `main` carries the "Dispatch is … the work runner's job" paragraph.

Then, if needed: `nohup cloudflared tunnel --url http://127.0.0.1:8901 &` for the dashboard.

### The explorer window (T-923, on demand, Mac Studio only)
```bash
ops/launch.sh explorer --window 3h --dry-run   # validate: window, one instance, the commands it will run
ops/launch.sh explorer --window 3h             # tmux session 'explore'
```
The pane runs `ops/explorer-window.sh`, which takes the radio lock (`just radio take explorer 3h …`),
waits (≤ 150 s) for `just radio status` to show staging on replay, then runs
`claude --agent explorer` (`.claude/agents/explorer.md`). It warns the agent 15 min before the end
(`$HACKRIFF_OPS/explorer/wrap-up`), stops it at the deadline, stops any `hk serve` left on :8897,
and releases the lock from an EXIT/INT/TERM/HUP trap. A crash or `tmux kill-session -t explore`
releases it too; only a SIGKILL of the window script doesn't, and then the lock's `until` makes it stale.
A second window is refused. The watchdog never relaunches it (`explorer` is not in `LIVE_ROLES`).
Log: `$HACKRIFF_OPS/explorer/window.log`; journal: `$HACKRIFF_OPS/explorer/journal-YYYYMMDD.md`.
