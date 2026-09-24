---
description: Show or set the pipeline's runtime knobs (WORK_CAP, WORK_QUEUE_PAUSE, WORK_GATE_ALONE, WORKER_DRAIN_MAX, BULK_MAX, HK_E2E_CONCURRENCY, NEXTEST_TEST_THREADS, …) in the persistent env store every ops restart reads, and hold the merge queue for a bounded time. Effective values are read from the running scripts' own start lines, not assumed.
disable-model-invocation: false
allowed-tools: Bash(*), Read
---

## `just knobs` (`python -m hkpy.knobs`)

```bash
export HACKRIFF_OPS=~/.hackriff-ops; cd /Users/daniellewis/hackriff
just knobs show                 # every knob: default · stored ($HACKRIFF_OPS/env) · effective (from the running process's VERSION/start line) · who reads it
just knobs set WORK_CAP=6 WORK_QUEUE_PAUSE=12    # writes the store + one line in env.jsonl (who, when, why if --why); the script reads it on its next start
just knobs unset WORK_CAP       # back to the script's default on next start
just knobs reset                # empty the store
```

The store is `$HACKRIFF_OPS/env`, `KEY=VALUE` lines; `/dev-env restart <script>` and `ops/launch.sh` source it, so a restart never silently reverts an experiment (on 2026-09-23 the cap-6 experiment lived only in one process's environment). `set` refuses `WORK_CAP=0` (invariant 3: dispatch is never fully paused for measurement) and refuses an unknown key. Outside an open experiment a `set` is logged as an **incident change**, not an experiment.

## `just hold` — a bounded merge-queue hold

```bash
just hold --minutes 20 --why "incident: main red on api_contract, fix landing"   # writes $HACKRIFF_OPS/hold (until=, why=, owner=, since=), alerts Discord
just hold --release
just hold --status
```

Enforced (invariants 4–6): at most **30 minutes**; no second hold within **2 hours**; refused while any branch is queued or a gate is running; the merge runner ignores an expired marker and **ends the hold at the first queued branch**; it alerts on start and if the expiry is reached with a branch waiting. Longer needs the user (`hand-off.md`). Every hold is a line in `hold.jsonl`, which `just experiment close` charges as blocked minutes.
