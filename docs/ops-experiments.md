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
- **Status:** open — overlap live since 2026-09-23 14:44; registered in the machine ledger (`$HACKRIFF_OPS/experiments.jsonl`) 2026-09-23 15:32 by the pipeline manager, so `just experiment status` counts gates from 15:32 (the 14:44–15:32 overlap stretch is outside the count). **Measured baseline** over 00:00..13:00: landings/h **0.92**, real reds 2/15, full-gate p50 47 min, dispatch-hours 5, blocked 0 min — not the ~1.3/h assumed above, so the +30 % bar is **≥ 1.20 landings/h**. Registered guards: `real_reds_24h <= baseline*1.25`, `full_gate_p50_min <= 56`, `blocked_minutes < 30`; 8 gates or 8 h.
- **Result:** —

- **Incident 2026-09-23 (confounds E-001):** from the 04:14 gate every full gate ran its tests at a measured concurrency of exactly **2.0** (JUnit: ~3300 test-seconds in ~1650 s wall; 4.5-7.2 and ~620-870 s before), because the merge runner, restarted from a Claude session at 04:05, inherited that session's `NEXTEST_TEST_THREADS=2` (`.claude/settings.json`, f2fc783b) and an environment variable beats the nextest profile's `test-threads = 8`. Cost ≈ **+870 s per full gate**. Fixed by `task-pm-gate-threads` (the gate drops an inherited thread count; only the knob store may set one). From the gate that carries it, E-001's `full_gate_p50_min` and landings/h move for a reason that is not overlap mode — compare E-001 on gates before that point, or read both halves separately.

---

## E-002 — (planned) UI e2e lanes 2 vs 3 under overlap mode

- **Hypothesis:** with the three harness causes fixed, 3 lanes is not flakier than 2 and is ~1.5 min faster per gate. Knob `HK_E2E_CONCURRENCY`. Guards: UI e2e red rate, per-spec durations (`just lane-plan`). Not before E-001 closes.

## Planned, not yet experiments

- Cut `just test` (30 min of every full gate): profile the four `hk-classify` sweep binaries and `hk-estimate::receiver_lines` that went ~3× dearer on 09-21 04:55 (`just gate-report`); fix or move to `timing`.
- A defined `ops`/`hooks` gate class with real tests (`bash -n`, `test_hooks.py`, `py` suites) instead of failing closed to `full` — six ops-only branches cost six full gates on 2026-09-23.
- Lane packing longest-first (`just lane-plan`).
- The Flow dashboard panel over `flow.jsonl`.

**E-001 closed 2026-09-23 18:34 — KEEP.** landings_per_h_24h: 0.92 → 3.63 over 8 gates / 3.0 h; real reds 1/8; full-gate p50 41 min; blocked 0.0 min; guards: real_reds_24h 1 <= 2.5 ok; full_gate_p50_min 41 <= 56.0 ok; blocked_minutes 0.0 < 30.0 ok. Confounds, stated: the 17:38 and 18:03 gates (2 of the 8) ran after task-pm-gate-threads restored nextest's 8 test threads (full gate 25-26 min vs 41), which lifts the tail of the window; the overlap gates before it (16:15 and 16:57 full, 41 min each, plus the 16:56 py gate) already carry landings/h well past the +30% bar (correction to the jsonl note: 17:38 ran at 8 threads, it carried the fix). The 3 lint rewinds at 16:11-16:14 were a ruff E702 in a pipeline-manager test, not overlap. The one real red (18:03) is fog-of-war beside T-845, isolated by the runner. Overlap mode (WORK_GATE_ALONE=0, WORKER_DRAIN_MAX=0) stays.

---

## E-002 — User decision 14:20: accepting a red after ONE solo pass for tests the flake led

- **Opened:** 2026-09-24 14:26 · **owner:** pipeline-manager
- **Hypothesis:** User decision 14:20: accepting a red after ONE solo pass for tests the flake ledger already shows passing alone (>=2, zero fail-alone, same window) saves the second isolated run (~1-4 min per accept) with no rise in real reds
- **Knob:** FLAKE_SOLO_ONE=1
- **Baseline window:** 2026-09-23 18:00..2026-09-24 14:00 — landings/h 1.6, real reds 14/63, full-gate p50 27 min, dispatch-hours 13, blocked 0.0 min
- **Primary metric:** flake_solo_saved_min_24h (just flow: 'K after one solo pass, S min of it'); landings_per_h_6h as context
- **Guards:** real_reds_24h <= baseline*1.25; full_gate_p50_min <= baseline*1.25; blocked_minutes < 30
- **Duration:** 6 gates or 24.0 h
- **Decision rule:** keep if >= 6 solo accepts, no guard broken, and no solo-accepted test fails alone within 7 d of its accept (hkpy.flakes); one such fail-alone = rollback
- **Rollback:** `just knobs set FLAKE_SOLO_ONE=0 && restart ops/merge-runner.sh between gates`
- **Status:** open
- **Result:** —


**E-002 closed 2026-09-24 19:56 — INCONCLUSIVE.** flake_solo_saved_min_24h: None → None over 25 gates / 5.5 h; real reds 6/25; full-gate p50 47 min; blocked 64.2 min; guards: real_reds_24h 6 <= 17.5 ok; full_gate_p50_min 47 <= 33.75 BROKEN; blocked_minutes 64.2 < 30.0 BROKEN. Closed early for E-004 (user: GATE_TIERS=check tonight). Both broken guards are the disk incident, not the knob: blocked_minutes 64 = the WORK_CAP=1 incident cap 18:23-19:27; full-gate p50 47 = contended gates during the clone-divergence rebuilds. flake_solo_saved not yet measurable (no qualifying solo accept counted). FLAKE_SOLO_ONE stays 1 as the user's 14:20 decision, part of E-004's baseline, not a result.


---

## E-004 — Gating merges with the check phase only (GATE_TIERS=check - CI's split: check on

- **Opened:** 2026-09-24 19:57 · **owner:** pipeline-manager
- **Hypothesis:** Gating merges with the check phase only (GATE_TIERS=check - CI's split: check on push, acceptance nightly) and draining the whole queue in one batch (BULK_MAX=30; one mechanism: the drain) takes the browser tier's serial reds off the merge path and raises landings/h. The acceptance phase still runs on main (idle, or between gates once 24 h overdue); HAND-CHECKED guards (not yet machine metrics): real-red rate of main's acceptance runs (ACCEPTANCE ... RED lines) and count of ACCEPTANCE_RED filed in merge-needs-attention (<= 2 per 24 h)
- **Knob:** GATE_TIERS=check, BULK_MAX=30
- **Baseline window:** 2026-09-24 00:00..2026-09-24 19:55 — landings/h 1.51, real reds 20/74, full-gate p50 28 min, dispatch-hours 15, blocked 64.2 min
- **Primary metric:** landings/h (rolling 6h)
- **Guards:** real_reds_24h <= baseline*1.25; full_gate_p50_min <= baseline*1.25; blocked_minutes < 30
- **Duration:** 6 gates or 12.0 h
- **Decision rule:** keep if landings/h >= +30%, no machine guard broken, and every acceptance red on main is attributed to one merge and filed; rollback at the first unattributed acceptance red or a 3rd ACCEPTANCE_RED in 24 h
- **Rollback:** `just knobs set GATE_TIERS=full BULK_MAX=15 && echo E-004-rollback > $HACKRIFF_OPS/merge-runner-restart`
- **Status:** open
- **Result:** —

**E-004 closed 2026-09-25 01:18 — KEEP.** landings/h: None → None over 28 gates / 5.4 h; real reds 1/28; full-gate p50 29 min; blocked 0.0 min; guards: real_reds_24h 1 <= 25.0 ok; full_gate_p50_min 29 <= 35.0 ok; blocked_minutes 0.0 < 30.0 ok. GATE_TIERS=check (+ BULK_MAX=30 for the drain): landings/h 1.51 (baseline 09-24 00:00-19:55) -> 5.5 (6h at 01:18 09-25); guards ok (real reds 1, full-gate p50 29 min <= 35, blocked 0). The one acceptance red on main since (RC 00:21 app-dismiss) was attributed to one merge and fixed by task-rcfix-app-dismiss (landed 01:16) - the rule's condition. Confounds, stated: node2 remote workers from 00:31 (0 remote landings by 01:18, so none of this gain), WORK_CAP 3->4->3 00:31-00:53 (incident). GATE_TIERS=check is also the user's rule (2026-09-24 20:00), so this keeps what the user decided; the acceptance phase lives in the daily RC.
