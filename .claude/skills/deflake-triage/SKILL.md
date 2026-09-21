---
description: Triage a failing test before working around it — is it a real bug or a load-flake? Use the moment a gate fails, before any retry/quarantine/re-batch. Never quarantine a test that fails in isolation.
disable-model-invocation: false
allowed-tools: Bash, Read, Edit
---

## Triage a gate failure

A gate failed. **Before reshuffling, retrying, or re-batching anything**, find out what kind of failure it is.

### 1. Run it in isolation, on a quiet machine
```
just test-one <the_failing_test_name>
```
Run it a few times. If load-sensitivity is suspected, also run it under the gate's contention.

### 2. Decide
- **Fails alone (quiet machine) → REAL BUG.** Not a flake. Stop routing around it — root-cause-fix the code or the test's wrong assumption. Do **not** quarantine, do **not** retry, do **not** widen a timeout. (This is the readsb lesson: two tests that failed *in isolation* were treated as flaky red-main churn and sank batch after batch for hours because nobody ran the isolation check.)
- **Passes alone, fails under load → genuine load-flake.** Now (and only now) the mitigation machinery is appropriate — but the goal is still a deterministic fix.

### 3. Fix the cause (load-flake)
Find the nondeterminism:
- **Timing** — a budget or deadline measured in wall-clock that load blows through. Drive on **frames/events/sample-index**, not milliseconds (T-537 is the worked precedent). A cold-linked subprocess can take 30s to reach its first instruction — budget from when the subject *starts*, not from the parent (the readsb-child cold-start, T-501/task-coldlink).
- **Ordering / shared state** — tests sharing a temp dir, a port, a global.
- **A freed port a stranger's server took** — assert on identity, not just a reconnect.

### 4. Prove it
Deliberately **reintroduce the defect and confirm the test goes red.** A test that stays green with the bug present asserts nothing (the vacuous-guard failure: 6 of 7 "passing" runs judged 0 of 0). Report the actual work exercised (request count, cells folded), not just pass/fail.

### 5. If it truly can't be made deterministic now
Quarantine **with a measurement and an owning ticket** (never an open-ended retry), and **re-enable it when fixed** — the deflaker or the ticket owns re-enabling. Quarantine is for load-flakes only. **Never quarantine a test that fails in isolation.**
