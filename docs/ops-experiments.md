# Pipeline experiments — the ledger

Append-only. One block per experiment, written **before** the change (invariant 12), closed with a decision and the numbers. The machine copy is `$HACKRIFF_OPS/experiments.jsonl` (`just experiment` maintains both). Metrics come from `just flow` (`flow.jsonl`); compare same-class gates per suite, never pooled classes, and never count an aborted gate (invariant 11). Procedure: `.claude/pipeline/workflows/run-an-experiment.md`.

Baseline for the first week: 2026-09-23 00:00–13:00 (alone mode): landings 49 (48 of them the crisis backlog), dispatch ~30 tickets in 23 h, dispatch = 0 in 10 of 13 hours, gate occupancy 40–60 min/h, full gate p50 ≈ 45 min (lint 0.5 + test 30 + acceptance 7 + UI e2e 3.5), reds 3 (all harness flakes, since fixed at the root), conflict-skips 9 (7 the board driver, since fixed), human touchpoints 6.

---

## E-001 — overlap mode: workers dispatch during a gate at the reserve cap

- **Opened:** 2026-09-23 13:30 · **owner:** supervisor (pre-dates the pipeline manager; it inherits the measurement)
- **Hypothesis:** letting workers run during gates (capped at the gate's reserve, `(28−14)/3 = 4`) and letting the merge runner gate without waiting for drain raises landings/h without raising the *real* red rate; the flake causes the alone rule was bought for are fixed at the root (surface-contention's hidden-tab rAF, the shared 8807 spec port, the self-matching `pgrep` loop).
- **Knob:** `WORK_GATE_ALONE=0` (default after `task-pipeline-overlap`), `WORKER_DRAIN_MAX=0` — one mechanism, two switches.
- **Baseline window:** 2026-09-23 00:00..13:00 (above).
- **Primary metric:** landings/h, rolling 6 h, excluding backlog (count dispatched-and-landed tickets).
- **Guards:** real red rate ≤ baseline × 1.25 (baseline 0/12 full gates); full-gate p50 ≤ 45 min × 1.25; flake rate ≤ 3 per 12 gates; blocked minutes < 30.
- **Duration:** 8 full gates or 8 h.
- **Decision rule:** keep if landings/h ≥ +30 % over baseline throughput (≈1.3/h → ≥ 1.7/h) and no guard broken.
- **Rollback:** `just knobs set WORK_GATE_ALONE=1 WORKER_DRAIN_MAX=2700 && /dev-env restart work-runner merge-runner` (merge runner between gates only).
- **Status:** open — applied when `task-pipeline-overlap` lands and both runners restart.
- **Result:** —

---

## E-002 — (planned) UI e2e lanes 2 vs 3 under overlap mode

- **Hypothesis:** with the three harness causes fixed, 3 lanes is not flakier than 2 and is ~1.5 min faster per gate. Knob `HK_E2E_CONCURRENCY`. Guards: UI e2e red rate, per-spec durations (`just lane-plan`). Not before E-001 closes.

## Planned, not yet experiments

- Cut `just test` (30 min of every full gate): profile the four `hk-classify` sweep binaries and `hk-estimate::receiver_lines` that went ~3× dearer on 09-21 04:55 (`just gate-report`); fix or move to `timing`.
- A defined `ops`/`hooks` gate class with real tests (`bash -n`, `test_hooks.py`, `py` suites) instead of failing closed to `full` — six ops-only branches cost six full gates on 2026-09-23.
- Lane packing longest-first (`just lane-plan`).
- The Flow dashboard panel over `flow.jsonl`.
