# Workflow: hand off

**When:** a finding belongs to someone else, or a decision is not yours. **Output:** the other party has everything they need in the channel they already read, and the hand-off is in your tick line.

## To the coordinator — tickets, product bugs, worker branches

Channel: append to `$HACKRIFF_OPS/merge-needs-attention.txt` (it reads it every tick). You never allocate a `T-` id (T-841); you write the ticket so it can file it without reading anything else:

```
[MM-DD HH:MM] pipeline: <one-line title> — evidence: <file:line / assertion / log lines / run id>;
  cause: <what you established, or "unknown after: <what you tried>">; suggested: <model/effort, group>;
  cost so far: <minutes, gates, tickets held>.
```

Product bugs a deflaker exposes, a worker branch that needs a rebase you must not do (its board block, its product code), a test that fails alone: all this way. A ticket that lives only in a file is not on the board until the coordinator reads it — say in your tick line that it is waiting there.

## To the supervisor / the user — rules, holds, budgets, trends

Channel: the supervisor's tmux session, then the tick line.

```bash
tmux send-keys -t super -l "PIPELINE: <one sentence with the numbers>. Needs: <the decision>." ; tmux send-keys -t super Enter
tmux capture-pane -t super -p | tail -5        # verify it landed; relays truncate - keep it under ~600 chars
```

What must go this way, always: reversing or permanently changing a **user rule** (invariant 20); a hold longer than 30 minutes or a second within 2 hours (invariant 4); any new long-running process (invariant 16); an experiment whose rollback would itself cost more than 30 minutes of pipeline; a trend break you cannot explain after one tick's diagnosis. Give the numbers and the cost of each option; recommend one; do not wait silently — say in the tick line what is pending on whom.

## From the user or supervisor to you — incidents

"The queue is stuck", "a worker is wedged", "main is red": you may hold the queue under the same 30-minute rule with `why=incident:<what>`, kill a wedged process you can attribute (`ops/watchdog.py --once --print` names owners), and restart a runner (`restart-an-ops-script.md`). Then the incident becomes a rule (`change-a-runner-rule.md`), and the tick line says what it cost.

## Discord

`python3 ops/alert.py <green|amber|red> "<title>" "<body>" --key <dedupe-key>` — for trend breaks and every hold (invariant 6), never for routine ticks. The dedupe key keeps a repeating condition to one message per 30 minutes.
