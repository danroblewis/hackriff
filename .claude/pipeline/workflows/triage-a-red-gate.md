# Workflow: triage a red gate

**When:** `gate: just <suite> took Ns (exit ≠ 0)` in `merge-runner.log`, or `just flow` shows `red > 0`. **Output:** the red's cause class, what it cost, and either a fix queued or a hand-off written. The runner has already done the first step for you; do not repeat it.

## 1. Read what the runner decided

```bash
just red-cause last            # or: just red-cause <run-id>
grep -E 'TRIAGE|BULK gate FAILED|GATE FAILED|rewound|retry' $HACKRIFF_OPS/merge-runner.log | tail -12
```

The runner's own triage: failing tests re-run **alone**; if they pass it retries the phase once (`retry PASSED` lands the batch); if they fail alone it re-runs them on the **rewound main** (`MAIN IS RED` → hold and wait for a fix; green on main → the batch is the culprit → isolate). A red without any `FAIL` line (lint, build, UI unit) is `SUITE_BROKEN`: re-queued in order and held until the queue changes.

## 2. Classify — the four causes, and what each costs

| Class | Evidence | Typical cost | Whose |
|---|---|---|---|
| **Flake (harness)** | passes alone; the failing assertion depends on timing, a port, a hidden tab, a shared resource | 11 min (acceptance retry) or 45 min (full retry) | **yours** — fix the harness, or spawn a deflaker |
| **Flake (product test)** | passes alone; the assertion reads the wall clock to choose *what* to assert (docs/10 §3.6 kind 1) | same | deflaker (you spawn it, you verify it) |
| **Real** | fails alone on the batch tree, green on rewound main | isolation + a coordinator round trip | coordinator (prose + evidence in the attention file) |
| **Pipeline** | wrong port, wrong tree, a killed process, an orphan gate, a merge that lost a file | whatever it blocked | **yours**, today |

Three "load flakes" of 2026-09-23 were all harness bugs: a second tab opened in the *same* window stopped the first page's `requestAnimationFrame`; two specs on one literal port at three lanes; a `pgrep -f "just gate"` loop matching its own argv. **"Load flake" is a hypothesis, not a diagnosis.**

## 3. For a flake: reproduce the condition, not the failure

Read the failing assertion's surroundings and the diagnostics the spec printed. Ask: what did the machine choose that the test assumed? (Which lane ran beside it — `e2e: lane N ▶`; which window; which port; what was the load.) If the answer is "the run was inconclusive and asserted anyway", that is the fix: a bounded wait for the state, gated on a wire fact, never a single snapshot. Prove it: green alone, green in the 13-file run at 3 lanes, and **red when the defect returns** (`HK_TILE_FAIR_SHARE=off` for the contention spec, a forced fault for the others).

Spawn a `deflaker` when the fix is in a product test or needs more than an hour; brief it with the assertion, the diagnostics, the runner log line numbers, the load, and the runs. Verify its handback before queueing.

## 4. For a real red: hand off with everything the coordinator needs

Assertion text, `panicked at file:line`, the batch composition, which branch the isolation named, the retry history. One paragraph in `$HACKRIFF_OPS/merge-needs-attention.txt`. Do not fix product code.

## 5. Record the cost

`just experiment status` shows the open experiment's guard metrics; a red that lands in its window counts against it. `just flow --record` picks the red up; the ledger line says *class* and *minutes lost*.

## Never

Retry a test that fails alone · quarantine · `#[ignore]` · widen a timeout to make the red go away · blame load without the lane/port/window evidence.
