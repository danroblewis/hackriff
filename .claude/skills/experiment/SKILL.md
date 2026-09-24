---
description: Open, watch and close a pipeline experiment in the ledger (docs/ops-experiments.md) — one at a time, with a baseline snapshot, guard metrics, blocked-minutes accounting and a prepared rollback. The pipeline manager's only way to change a knob for measurement.
disable-model-invocation: false
allowed-tools: Bash(*), Read
---

## `just experiment` (`python -m hkpy.experiment`)

```bash
export HACKRIFF_OPS=~/.hackriff-ops; cd /Users/daniellewis/hackriff
just experiment status                      # the open one: gates so far (same class), metric, each guard vs bound, blocked minutes
just experiment new --id E-007 --hypothesis "…" --knob K=V [--knob K2=V2] \
    --baseline "2026-09-23 00:00..13:00" --metric "landings/h (rolling 6h)" \
    --guard "real red rate <= baseline*1.25" --guard "full-gate p50 <= baseline*1.25" --guard "blocked_minutes < 30" \
    --gates 8 --hours 8 --rule "keep if metric >= +30% and no guard broken" \
    --rollback "just knobs set K=old && /dev-env restart work-runner"
just experiment close --decision keep|rollback|inconclusive --note "…"
just experiment list
```

What it enforces (`.claude/rules/pipeline-invariants.md` 7, 12–15): `new` refuses while one is open, requires every field including `--rollback`, and snapshots the baseline window from `$HACKRIFF_OPS/flow.jsonl` (run `just flow --record` first if the window is empty). `close` computes the deltas per metric from `flow.jsonl` over the window, charges `blocked_minutes` from `hold.jsonl` and the dispatch-cap history in `env.jsonl`, and **refuses `keep` when a guard is broken**. Two knobs in one entry are allowed only as one mechanism.

The ledger is `docs/ops-experiments.md` (append-only, one block per experiment, E-001 is the template); the machine copy is `$HACKRIFF_OPS/experiments.jsonl`. Procedure and the statistics that make a decision honest: `.claude/pipeline/workflows/run-an-experiment.md`.
