# Workflow: diagnose a flat burndown

**When:** landings/h has fallen, or the user says the burndown flattened. **Output:** one paragraph naming where the hours went, with the table that proves it, and the single cheapest lever.

## 1. Get the hourly table

```bash
just flow --hourly --since 24h
```

Columns: `dispatch` (workers started), `handback` (workers finished), `landed` (tickets merged), `gate-min` (minutes a gate held the box), `wait-min` (minutes the runner spent waiting for drain/foreign runs/contention), `red` (gates that went red). Read it as a story, hour by hour.

## 2. Separate backlog from throughput

A steep slope that ends abruptly is usually **backlog draining**, not throughput: count `landed` vs `dispatch` over the window. On 2026-09-23, 48 tickets landed 00:00–09:00 while only ~30 had been dispatched in 23 hours — the slope was the crisis backlog, and it went flat when the backlog was gone. The true rate is `dispatch` (or `handback`) per hour, not `landed`.

## 3. Attribute each idle hour to one of five causes

| Cause | Signature in the table | Lever |
|---|---|---|
| **Starved** — workers not running | `dispatch = 0` while `gate-min` high or `HOLD:` lines in `work-runner.log` | the alone/overlap policy, cap, queue pause (`just knobs`) |
| **Gated** — box busy with a gate, no landing yet | `gate-min ≥ 40` with `landed = 0` | gate cost (`just gate-report`), batch size |
| **Waiting** — runner draining / held | `wait-min` high; `WAIT:` / `WAIT over:` lines | drain cap, declared waiters (`just wait-for-gate`), foreign spec runs |
| **Red** — gate failed | `red > 0`; `TRIAGE:` lines | `triage-a-red-gate.md` |
| **Conflicting** — branches skipped at merge | `BULK conflict` lines; `CONFLICT(skipped from bulk)` in the attention file | the board driver, rebase policy, which file (`git diff` the two sides) |

```bash
grep -E 'HOLD:|DISPATCH' $HACKRIFF_OPS/work-runner.log | tail -40          # why dispatch stopped
grep -E 'WAIT|OVERLAP|BULK attempt|BULK conflict|MERGED|FAILED' $HACKRIFF_OPS/merge-runner.log | tail -40
just touchpoints --since 24h                                                  # what a person had to do
```

## 4. Check the workers themselves before blaming the pipeline

```bash
just worker-report --running        # tool-call timeline per running worker; idle vs working
```
A worker at ~0 % CPU in a `sleep` loop is not working (2026-09-23: three self-matching `pgrep -f "just gate"` loops, 30/19/9 minutes wedged). A worker 3 hours into a ticket with no commit is a brief problem, not a pipeline problem — hand it to the coordinator.

## 5. Name the lever, then the cost of the lever

One sentence: *"10 of 13 hours had zero dispatch because the gate ran alone; overlap mode restores dispatch during gates at the reserve cap; cost: gate timings marked contended."* If the lever is a knob, it is an experiment (`run-an-experiment.md`). If it is code, `change-a-runner-rule.md`. If it is a user rule, `hand-off.md`.

## Worked example (2026-09-23)

Table showed `dispatch = 0` in 10 of 13 hours, `gate-min` 40–60 in most, `landed` 48 before 09:00 then 1. Causes: backlog drained (not throughput); starved (gate-runs-alone + drain); plus that day's specifics — 5 of 7 branches conflict-skipped on `docs/tasks.yaml` (the work runner's own two writes to one block → field-level board merge), three ops-only branches consuming full gates, two flake retries (~1 h). Lever chosen: overlap mode (E-001).
