# Model and effort selection

How hackriff sessions and subagents pick a model. The goal is to spend the strongest reasoning where mistakes are expensive or hard to see (architecture, real-time DSP correctness, core interfaces), and cheaper models where a task is well specified and the tests will catch errors.

These tiers are qualitative starting points. Revisit them after the first milestone, based on which tasks needed rework.

## Models

| Tier | Model ID | Agent tool `model` value |
|---|---|---|
| Deepest reasoning | `claude-fable-5-1` | `fable` |
| Strong generalist, long context | `claude-opus-5` | `opus` |
| Fast, capable implementer | `claude-sonnet-5` | `sonnet` |
| Cheap, mechanical | `claude-haiku-4-5-20251001` | `haiku` |

How to set the model:
- **Session:** `claude --model <id>` or `/model`.
- **Subagent:** the Agent tool's `model` parameter, or `model:` in a `.claude/agents/*.md` definition.
- **Forks** always inherit the parent's model.

## The complexity rubric (T-563) — set BOTH model and effort at spawn

Measured 2026-09-20 from `/perf`: **model/thinking time was 184 h against 144 h for all tool and
build time combined**, with 18.4 B cache-read tokens. Almost every subagent that day was launched on
Opus at high effort regardless of the work — the coordinator's habit, not this policy. Reasoning time
is now the largest single cost, and unlike compile time nothing had been done about it.

So estimate complexity **before** spawning, and set the tier from it:

| Complexity | Examples | Model | Effort |
|---|---|---|---|
| Trivial / mechanical | rename, move, doc edit, board hygiene, re-queue, cherry-pick | `haiku`, or `opus` | **low** |
| Well specified, single crate, tests exist or are obvious | a route field, a contract assertion, a bounded fix whose mechanism is already measured | `sonnet` | **medium** |
| Core interface, real-time path, novel DSP, hard debugging | schema/plugin/stream contracts, detection thresholds, scheduler, ring/timing races, anything CLAUDE.md marks `core_interface` | `opus` (or `fable`) | **high / xhigh** |

Two rules that override the table:
- **`core_interface` and the real-time path never go to Sonnet or Haiku alone** (CLAUDE.md), and a
  cheaper model's output touching them is reviewed by Opus before merge.
- **A measured mechanism lowers the tier.** A ticket whose cause is already established by a prior
  ticket's measurement is *well specified*, however alarming its symptom — hand the agent the
  measurement and drop to Sonnet. Re-deriving what another ticket already measured is the single
  most common way reasoning time is wasted here.

**The coordinator's own effort:** medium for routine orchestration (queueing, reconciling, briefing,
merges). Reserve high for hard planning, reviews, and diagnosing a failure nobody has characterised.

**Tool limitation, stated honestly:** the Agent tool exposes `model` but **no `effort` parameter**, so
at spawn only the model tier can be set programmatically. Effort is set by the session/agent
definition; `tasks.yaml`'s `effort:` field records the intended tier so a brief, an agent definition
or a human can honour it, and so the choice is reviewable rather than implicit.

**This is an experiment with a number attached:** re-check `/perf`'s Model time after a day of
tiering. It should fall materially without throughput dropping. If it does not, the rubric is wrong
and should be changed rather than quietly ignored.

## What runs where

### Fable — high effort
Use for decisions that are costly to reverse, and problems where subtle errors pass the tests:
- Architecture, ADRs, the data model, and changes to any settled ADR.
- Designing core contracts: the stream/bitstream output contract, the plugin/decoder interface, provenance, scheduler policy.
- Novel signal-analysis design: blind symbol-rate/modulation estimation, open-set "unknown" classification, CFAR tuning strategy, the attention scheduler, event correlation for the attack map.
- Hard debugging: dropped samples, timing/ring-buffer races, GPU/USB throughput, numerical bugs that only show on real captures.
- Reviewing the Phase 7 implementation plan, and milestone-end reviews of the core pipeline.

### Opus 5 — high or medium effort
Use for judgment across many files, or work that needs long context:
- **The development coordinator session**: planning task batches, briefing subagents, reviewing and merging their output, and keeping the task state file current.
- Implementing core real-time components once the design is settled: source abstraction, dwell capture and ring buffer, channelizer, spectral estimation, detection, param estimation, digital demod.
- Code review of Sonnet/Haiku output that touches core interfaces or the real-time path.
- Research synthesis and doc upkeep: capability cards, research docs, checkpoint critiques of Fable's output.
- Writing the test harness itself: IQ replay, synthetic scenario generator, SigMF fixture tooling.

### Sonnet 5 — medium effort
The default implementer for anything off the real-time path and outside core interfaces. Use for well-specified tasks with clear acceptance tests and a narrow footprint:
- Web UI work, the HTTP/control API surface, CLI and config, stream openers and discovery.
- Test additions, flaky-test fixes, and follow-up tasks from reviews.
- Wrapping existing decoders as plugins (rtl_433, readsb, multimon-ng, AIS-catcher…) against an approved plugin contract.
- UI views and components after the UI ADR and data model exist.
- CLI tools, config handling, storage and queries against the approved schema, external feed adapters (C29).
- Writing unit and e2e tests from a task's acceptance criteria. Converting captures to SigMF with annotations.
- Bulk mapping and classification work with a fixed rubric, e.g. assigning use cases to an approved capability taxonomy, one batch per agent.

### Haiku 4.5 — low effort
Use for mechanical, easily checked work:
- Link checks, YAML/SigMF metadata validation, formatting, renames, lint fixes.
- Codebase and doc search (or the Explore agent), log and test-output summaries.
- Generating index tables, changelogs, and inventories of files or fixtures.

## Escalation rules

- **Start at the cheapest tier that fits the task description above, never below it.**
- **Escalate one tier** when:
  - two attempts fail the acceptance tests, or
  - the task turns out to be ambiguous or under-specified, or
  - the fix needs changes outside the task's declared file footprint.
- **Always use Opus or Fable** (never Sonnet or Haiku alone) for changes to:
  - the data model schema, stream/plugin contracts, or provenance fields
  - the real-time sample path (ring buffer, USB ingest, channelizer, timing)
  - the attention scheduler
  - calibration/detection thresholds that tests can't fully verify
- **Changing a decision recorded in an ADR** goes to Fable plus the user, never a silent change in code.
- **A cheaper model's output that touches core interfaces gets reviewed** by Opus before merge.

## Effort

- **High:** architecture, novel DSP, hard debugging.
- **Medium:** default for implementation and review.
- **Low:** mechanical tasks.

When unsure between two efforts, choose the higher one for work on the real-time path and the lower one for everything else.

## Token budget

The budget ran out early on 2026-09-13. Most of the spend was long agent contexts re-read every turn, and prompt caches that expired while an agent waited more than 5 minutes on a build or test run. Rules:
- **Never wait in the foreground.** Run builds, test suites and repeated test runs as background tasks and act on the completion notification. No sleep or poll loops.
- **Targeted tests while developing:** `cargo nextest run -p <crate>` or a single test. Agents don't run the full suite.
- **One full check per merge.** The coordinator runs `just test` plus acceptance on main after merging. Flake hunts run a test N times in one background command, not N turns.
- **Lean context.** Read logs with `grep`/`tail`, never whole. Keep briefs to the task entry plus at most 5 pointers. Start a fresh agent rather than continuing one past about 300k tokens.
- **Opus at medium effort** unless the task is on the real-time path, novel DSP or hard debugging.

## In task entries

Every task in the Phase 7 implementation plan carries `model` and `effort` fields chosen with these rules. The coordinator may override them with a one-line reason in the task state file, which is how the tiers get tuned over time.
