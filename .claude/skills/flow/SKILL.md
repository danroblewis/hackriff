---
description: Measure the pipeline — landings/h, per-hour occupancy (dispatch / hand-back / landed / gate-min / wait-min / red), red rate by cause, conflict-skips, touchpoints, queue depth — from the ops logs, and record one flow.jsonl line. The pipeline manager runs it every tick; anyone can run it to see where the hours went.
disable-model-invocation: false
allowed-tools: Bash(*), Read
---

## `just flow` (`python -m hkpy.flow`)

Arguments: `--hourly` (default) | `--gates` (one row per gate: class, suites, duration, verdict, cause, batch size) | `--tickets` (one row per landed ticket: dispatched → first commit → hand-back → queued → gated → landed, with each wait); `--since 24h|6h|2026-09-23T00:00`; `--record` (append the summary record to `$HACKRIFF_OPS/flow.jsonl`); `--json`.

Sources, all read-only: `$HACKRIFF_OPS/merge-runner.log`, `work-runner.log`, `landed.jsonl`, `handbacks.jsonl`, `gate-timings.jsonl`, `flaky.jsonl`, `merge-attempts.txt`, `merge-needs-attention.txt`, `hold.jsonl`, `git log main`.

```bash
export HACKRIFF_OPS=~/.hackriff-ops; cd /Users/daniellewis/hackriff
just flow --hourly --since 24h
just flow --gates --since 24h
just flow --tickets --since 24h
just flow --record            # the tick: prints the one-line summary and appends it
```

The summary line (invariant 23): `flow: 2.6/h (6h) 1.9/h (24h) · reds 1/8 (flake) · conflicts 0 · touchpoints 0 · queue 3 (oldest 12m) · workers 4/6`.

Reading it: `landed` vs `dispatch` separates backlog from throughput (a steep slope with `dispatch ≈ 0` is backlog draining); `dispatch = 0` beside high `gate-min` is starvation; high `wait-min` is the runner draining or blocked; `red` and `conflicts` name the workflow to open (`triage-a-red-gate.md`, the board driver). Then `.claude/pipeline/workflows/diagnose-a-flat-burndown.md`.

Companions: `just touchpoints [--since]` (every human intervention, the number that should be zero), `just red-cause [last|run-id]`, `just gate-report [last|run-id]` (per-suite and per-test durations vs the previous same-class gates), `just worker-report [--running|T-nnn]`, `just cycle-time --suites`.
