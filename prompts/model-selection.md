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
Use for well-specified tasks with clear acceptance tests and a narrow footprint:
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

## In task entries

Every task in the Phase 7 implementation plan carries `model` and `effort` fields chosen with these rules. The coordinator may override them with a one-line reason in the task state file, which is how the tiers get tuned over time.
