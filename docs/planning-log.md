# hackriff architecture planning — running log

A short running record for autonomous planning. The coordinator/user reviews this after the fact. Every checkpoint decision made without the user is marked **PROVISIONAL** with the reasoning and the cheapest-to-reverse rationale. Ranked open questions for the user are at the bottom and kept current.

Convention: dates are absolute. "Reversible" = how hard it is to change later.

## Current phase

**Phase 3 (architecture + ADRs).** Phase 1 committed (8c2db03). Phase 1 follow-ups applied; capability-card sync delegated to a background agent (its commit lands separately). Phase 2 data model written to `docs/07-data-model.md`. Next: `docs/08-architecture.md` and `docs/adr/`, starting with the runtime and UI ADRs (the two the brief checkpoints first).

## Decisions from the user (not provisional)

- **2026-09-13 — Taxonomy frozen at 39 capabilities.** User approved; C39 `live-view-inspector` kept because the exploratory UI is wanted.
- **2026-09-13 — `needs-other-sdr` threshold** = phase-coherent multi-channel, >20 MHz gap-free, or a stated ≥12-bit dynamic-range requirement. Approved.
- **2026-09-13 — Two extra YAML fields** (`accessory`, `fit_note`) approved.
- **2026-09-13 — Public networks in scope, own networks out.** Using public receivers (KiwiSDR) and uploading to public networks (WSPRnet, SatNOGS, SondeHub, Blitzortung) is `native`; building the user's own sensor mesh is `out-of-scope`.
- **2026-09-13 — HF stays `native`**, with the caveat recorded as a structured `fit_flags: [marginal-hf]` entry rather than a downgrade.

## Provisional decisions

### Phase 1 follow-ups (2026-09-13)

- **P1.1 `fit_flags` schema added** to `use-cases.yaml`: `marginal-hf` (27), `marginal-8bit` (15), `exceeds-window` (32), `metadata-only` (13), `data-only` (41), `knowledge-item` (15). Orthogonal to `hardware_fit`. Reversible: yes, it's additive metadata. Documented in docs/06 §4.1.
- **P1.2 RESEARCH-027 → `out-of-scope`** (was needs-tx): jamming violates 47 USC 333 even against your own device. Capture-only variants stay in scope. Reversible: trivially.
- **P1.3 Taxonomy stays 39.** All card-feedback items resolved as *ownership/edge* decisions inside the frozen taxonomy, in docs/06 §5, not as new capabilities. Several marked provisional pending doc 07 / Phase 3 ADRs (own-key decryption owner, restricted-content gating, storage/retention policy, anomaly record shape, C24 data-vs-control split).
- **P1.4 Trunking use cases SIGNAL-080..086 proposed** (`status: proposed`), added to docs/05 and the YAML. Rationale: docs/04 §8 calls trunking the most-requested capability; catalogue had ~2. **Needs user accept/reject** (see questions).
- **P1.5 §4.2 build order** now names C04/C08/C11/C13 explicitly and stages the C05↔C09 bootstrap.
### Phase 2 — data model (2026-09-13)

- **P2.1 Measurement/interpretation split.** Frames, Detections and Recordings are immutable measurements with provenance; classifications and decodes are versioned, append-only interpretations re-runnable over stored measurements. This is the backbone; reversing it later is expensive, so flagged for user review.
- **P2.2 Three storage tiers** (provisional, Phase 3 ADR finalises): SQLite relational state, a tiled spectrum-history pyramid, SigMF files for IQ/audio/bits, all under one data dir.
- **P2.3 Anomaly object defined** (§2.18) as the shared record C08/C12/C27 emit and C30 consumes, resolving a §5 open item.
- **P2.4 SpectrumTile pyramid** is the region-over-time engine; fixed rolling byte budget bounds disk.

- **P1.6 Heartbeat caveat.** The `CronCreate` heartbeat (job cf8688aa, `7,27,47 * * * *`) is **session-only** — it does not survive this session exiting (CronCreate has no durable persistence in this build). Per-phase commits are therefore the real resume mechanism; a fresh session resumes from this log + git, not the cron.

## Open questions for the user (ranked)

1. **Accept the proposed trunking use cases SIGNAL-080..086?** They shape the C23 milestone and the roadmap. If any are unwanted, say which; IDs are permanent so rejected ones would be marked retired, not deleted.
2. **Restricted-content gating owner (P1.3).** I put enforcement on `stream-output` (C24) with a content-class flag set at classification, provisional pending a Phase 3 legal-guardrail ADR. Confirm that's the right seam, or name another.
3. **Own-key decryption (P1.3)** modelled as a `decoder-plugins` stage with user-supplied keys and key-source provenance. Confirm this belongs in C22 rather than its own capability.
4. Anything in docs/06 §5 ownership table you'd overrule before it hardens into the data model (doc 07)?
