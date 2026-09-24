# Workflow: run an experiment

**When:** a diagnosis names a knob. **Output:** a closed ledger entry in `docs/ops-experiments.md` with a decision (keep / rollback / inconclusive) and the numbers behind it. Invariants 2–8 and 12–15 bind every step.

## 1. Write the entry before touching anything

```bash
just experiment new --id E-007 \
  --hypothesis "overlap mode raises landings/h without raising the real red rate" \
  --knob WORK_GATE_ALONE=0 --knob WORKER_DRAIN_MAX=0 \
  --baseline "2026-09-23 00:00..13:00" \
  --metric "landings/h (rolling 6h)" \
  --guard "real red rate <= baseline*1.25" --guard "full-gate p50 <= baseline*1.25" --guard "blocked_minutes < 30" \
  --gates 8 --hours 8 \
  --rule "keep if metric >= +30% and no guard broken" \
  --rollback "just knobs set WORK_GATE_ALONE=1 WORKER_DRAIN_MAX=2700 && /dev-env restart work-runner merge-runner"
```

`new` refuses if an experiment is open (invariant 12), snapshots the baseline from `flow.jsonl` for the window you named, and appends the entry. Two knobs in one entry are allowed only when they are one *mechanism* (overlap mode is one mechanism with two switches); two mechanisms are two experiments.

## 2. Apply the knob between gates

```bash
just knobs set WORK_GATE_ALONE=0 WORKER_DRAIN_MAX=0     # persisted in $HACKRIFF_OPS/env
/dev-env restart work-runner                             # reads the env store on start
/dev-env restart merge-runner                            # ONLY between gates (invariant 21) - the skill refuses otherwise
```

Never a hold for this. If the knob only takes effect on restart and the runner is mid-gate, wait for the landing; `just experiment status` shows "applied: pending restart" until then.

## 3. Let it run; do nothing else to the pipeline

Each tick: `just experiment status` — gates counted so far (same class as the baseline only), the primary metric so far, each guard's current value against its bound, blocked minutes. Do not adjust another knob, do not queue a pipeline change that touches the same mechanism, do not restart anything the experiment measures. Reds that land in the window are triaged as usual (`triage-a-red-gate.md`) and charged to the experiment's guard.

## 4. Decide by the rule you wrote, then close

```bash
just experiment close --decision keep --note "landings/h 1.1 -> 2.7 over 9 gates; real reds 0/9; p50 full gate 46 -> 47 min"
# or
just experiment close --decision rollback --note "guard 'real red rate' broke: 3/8" && <the rollback command>
```

`close` computes the deltas from `flow.jsonl`, writes `blocked_minutes`, and refuses `keep` when a guard is broken (invariant 14). *Inconclusive* is a real decision: small effects with fewer than 6 gates are recorded as such and revisited, never kept because they "looked good".

## 5. Make a kept knob the default

A kept env knob is still an env knob until the script's default changes; open a `change-a-runner-rule.md` branch that moves the default (one line) so a plain restart keeps it. The ledger links the commit.

## Statistics, honestly

~30 full gates a day, each ~45 minutes, with per-suite noise of ±15 %: you can see a 30 % effect in 6–8 gates and cannot see a 10 % one in a day. Compare per suite, same class, paired against the baseline window; an aborted gate is not a sample (invariant 11). If the effect you need to see is small, the experiment is the wrong tool — instrument instead (`just gate-report`).

## Ledger entry shape

See `docs/ops-experiments.md` (E-001 is the template).
