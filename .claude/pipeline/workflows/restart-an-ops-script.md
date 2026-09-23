# Workflow: restart an ops script safely

**When:** a landed change to `ops/`, or a knob that only takes effect on start. **Output:** the script running the code on `main` (its `VERSION:` line says `matches`) with the env store applied, and nothing interrupted that mattered. Use `/dev-env restart <script>`; this page is what the skill does and why.

| Script | What a restart interrupts | Safe when | Env it reads |
|---|---|---|---|
| `merge-runner.sh` | **a running gate** — startup repair rewinds a provisional batch and re-queues it (~45 min lost); a staged `MERGE_HEAD` is aborted | no `bulk-in-progress`, no `MERGE_HEAD`, no `hkpy.gate` process | `WORKER_DRAIN_MAX`, `BULK_MAX`, `FOREIGN_DRAIN_MAX`, `GATE_TIMEOUT`, `MAX_ATTEMPTS` |
| `work-runner.py` | nothing — workers are detached (`start_new_session`), claims are in a file; a tick in flight is retried | any time; a dispatch may be delayed one tick | `WORK_CAP`, `WORK_QUEUE_PAUSE`, `WORK_GATE_ALONE`, `WORK_PER_TICK`, `WORK_GATE_RESERVE`, … |
| `watchdog.py` | one 20 s tick of attribution; no kills are pending across ticks | any time | `WATCHDOG_*` |
| `monitor.py` | the dashboard for ~3 s; open browser tabs reconnect | any time | `MONITOR_PORT` |
| `stage.sh` | **the staging demo on :8899** (and the user's live HackRF if attached) — a rebuild + restart cycle | only when the user is not using the demo; it changes rarely, prefer not to | — |

```bash
export HACKRIFF_OPS=~/.hackriff-ops; cd /Users/daniellewis/hackriff
set -a; . $HACKRIFF_OPS/env 2>/dev/null; set +a          # the knob store (just knobs show)
pkill -f 'ops/work-runner.py'; sleep 2
nohup python3 ops/work-runner.py >/dev/null 2>&1 & disown
sleep 4; tail -2 $HACKRIFF_OPS/work-runner.log             # VERSION: matches …  cap=N
```

Rules:
- **Never the merge runner mid-gate** (invariant 21). If a gate is running, wait for `BULK MERGED` / `MERGED` and the `bulk-in-progress` marker to clear, then restart within the seconds before it takes the next batch — or accept that the old code gates one more batch. The skill checks this and refuses.
- **Say what it interrupts** in the tick line, before doing it.
- **Two backgrounded commands per shell invocation at most** — the hook blocks more.
- After a restart, read the first three log lines: `merge-runner up … VERSION: matches <sha>` and, since 2026-09-23, the `STARTUP:` lines listing what it left alone (worktree processes) or killed (orphan gate processes on the main checkout only).
- A restart is not a fix. If you restarted because something wedged, the wedge is a rule to write (`change-a-runner-rule.md`).
