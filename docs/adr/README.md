# Architecture Decision Records

One file per major decision, `NNNN-title.md`. Format: **Context → Options → Trade-offs → Decision → Consequences → Status**, with the capabilities/data-model objects it touches and, where relevant, the Phase 4 spike that would confirm or overturn it.

**Status values:** `PROVISIONAL` (made autonomously during planning, cheapest-to-reverse option chosen, awaiting user review or a spike), `ACCEPTED` (confirmed by the user or a passing spike), `SUPERSEDED by NNNN`.

Changing an ACCEPTED ADR goes to Fable plus the user, never a silent code change (`prompts/model-selection.md`).

| ADR | Title |
|---|---|
| [0001](0001-pipeline-runtime.md) | Pipeline runtime and live reconfiguration |
| [0002](0002-ui-web-vs-native.md) | UI: web versus native |
| [0003](0003-process-plugin-model.md) | Process and plugin model |
| [0004](0004-stream-output-contract.md) | Bitstream and stream-output contract |
| [0005](0005-survey-dwell-scheduler.md) | Survey/dwell scheduler |
| [0006](0006-storage.md) | Storage |
| [0007](0007-compute-placement.md) | Compute placement |
| [0008](0008-offline-first-context.md) | Offline-first external context |
| [0009](0009-hardware-platform.md) | Hardware platform sketch |
| [0010](0010-language-and-licence-ledger.md) | Language, toolchain, and dependency licence ledger |
| [0011](0011-decoder-workbench-contracts.md) | Decoder workbench contracts: blocks, recipes, field maps, inspector stream; §8 amendment: audio output — an `audio_out` sink block, an `audio` output kind over the existing audio stream profile, a live-edge liveness policy, and `refine.objective.builtin` (schema 3) |
| [0012](0012-attention-memory-contracts.md) | Attention + memory contracts: observation log, occupancy, baselines, interestingness, bandit, reports, alarms |
| [0013](0013-ui-architecture.md) | MUI web UI architecture: vanilla TS + small store, component tree, state slices, frontend↔API map with API gaps, migration, per-panel briefs |
| [0014](0014-iq-capture-ring.md) | IQ capture ring: pre-allocated persistent on-disk ring for the rolling IQ buffer (slots, CRC-framed journal, recovery, quota change, free space, runs) |
| [0015](0015-decoder-synthesis-contracts.md) | Decoder synthesis contracts (MAUTO): recipe-prefix candidates, stage evidence in significance bits, beam search + budget, templates, `POST /api/analyze` jobs, confirm-by-decode, burst path, blind evaluation; §11 amendment: a candidate **is** a decode pipeline — an Emitter owns competing pipeline hypotheses ranked by evidence, overlap/artifact resolution as hypothesis competition, promotion drives the inventory lifecycle; §12 amendment: Listen as an audio pipeline — chooser + pipeline + opener, the opener kept permanently, per-mode cutover, and a 0–7 staged migration that never breaks live listening |
| [0016](0016-classification-contracts.md) | Classification contracts (M3): Classification with open-set unknown and taxonomy `hk-mod@1`, prior fusion that never vetoes evidence, classical cascade + gated DL, EmissionFeatures/Signature/SignatureMatch and unknown clustering, ml-runtime (Mac-first ONNX), blind evaluation and exit gate, MAUTO seed, M3 task graph |
| [0017](0017-time-extent-signal-model.md) | Signals as time–frequency regions (**records** the user's settled model): the presence interval as the time extent, `first_seen`/`last_seen` demoted to a hull and `count` to a History-only total, a window-scoped Explore Candidate list with always-listed Confirmed rows, boxes that grow along the time axis, a separate History surface for every event including one-offs, the live-edge-versus-incremental reader ruling for Listen versus decode, and a TM-1…TM-10 staged plan that never breaks live Explore |
| [0018](0018-gnss-known-code-exception.md) | GNSS acquisition is known-signal-led: the project's one documented exception to blind-first, forced by L1 sitting 20–30 dB below the noise floor, and the crate-boundary + guard-test mechanism that confines it so the general detector can never reach it; the jamming/spoofing half stays blind |
| [0019](0019-presence-as-an-interval-with-endpoints.md) | Presence is an interval with endpoints: a box runs from its start to the live edge and caps only on a detected END, so the measurement is the START plus the absence of an END; the honesty burden moves onto the end detector and onto drawing the assumed span as assumption (the open cap); the idle gap becomes the end detector's latency and is measured off the IQ ring's tune history instead of defaulting to the 60 s unknown; REOPEN is derived from that same gap plus entity resolution; the stream carries `presence-start`/`presence-reopen`/`presence-end` (`hackriff.presence/2`) and nothing at all while an interval continues |
| [0020](0020-last-known-shadow-tier.md) | A fourth honesty tier, **last-known / stale**: a band swept then departed carries its most-recent-known value dimly (the canvas's shadow, with its `last_t_s` and source resolution), computed at query time from the pyramid newest-first/fine-to-coarse with nothing maintained and nothing on the capture thread; never carried backward, unsearched windows stated; **grey's meaning unchanged** — still decided by coverage alone, still only where nothing retained reaches |
| [0021](0021-search-trace-and-negative-result.md) | The **search trace** and the **negative result** (MAUTO): a bounded `TraceNode` record of the engine's *decisions* with a closed `outcome` enum split into **tried** (measured bits against a floor) and **not tried** (deferred for budget or prior, no block exists) — the decode-side statement of "grey = genuinely unobserved"; retention that is complete in counts and lossy in detail; a sealed **`Resolution`** carrying what was searched, how far, and which of five reasons means nothing won, with `not-searched` ≠ `unknown` and `structured-unidentified` as a first-class result; and the guard against a search that always finds something — an in-job **shuffled-null control** that can only cap a verdict, under a stated **1-false-label-per-1 000-jobs** budget measured over four negative populations |
