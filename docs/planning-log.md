# hackriff architecture planning — running log

A short running record for autonomous planning. The coordinator/user reviews this after the fact. Every checkpoint decision made without the user is marked **PROVISIONAL** with the reasoning and the cheapest-to-reverse rationale. Ranked open questions for the user are at the bottom and kept current.

Convention: dates are absolute. "Reversible" = how hard it is to change later.

## Current phase

**Building M0 (started 2026-09-13).** Autonomous coordinator session authorised by the user: provisional defaults adopted, trunking use cases SIGNAL-080..086 accepted, parallel worktree agents, commit per task (no push). See "Build log" at the bottom.

**Planning complete (Phases 0–7).** All phases committed. test_tier filled for all 398 use cases (field 143, offline-recorded 92, offline-synth 80, data-only 53, hil 30). Heartbeat deleted. Awaiting user review of docs/planning-log.md open questions; next real work is spikes S4/S5/S1/S3 on the Mac + HackRF and task T-001.

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

### Phase 3 — architecture + ADRs (2026-09-13). All ADRs PROVISIONAL.

- **P3.1 (ADR-0001, runtime).** Rust core with a **dynamic dataflow**; always-on capture/survey/detect/channelize separated from demod/decode chains that are added at runtime as data-driven nodes + plugins — so "change pipelines without stopping capture or rebuilding" is an architecture property, not a framework feature. **Prefer FutureSDR** as the substrate if spike S1 passes, else an owned Rust dataflow on liquid-dsp/cuFFT. GR3 rejected as core (static flowgraph + GPLv3 in sample path); GR4 tracked but pre-ecosystem. Verified on web: GR4 RC1 runtime graph reconfiguration is real; GR3 needs lock/unlock or restart.
- **P3.2 (ADR-0002, UI).** Headless core + **web UI (WASM/WebGL2) served by the device**, one app for on-device kiosk and remote phone/laptop (Maia SDR pattern). Fallback: a thin native shell for the on-device waterfall only if spike S3 fails. Flagged as the user's most important decision — most reversible option chosen.
- **P3.3 (ADR-0003).** Existing decoders and all GPLv3 code = **subprocess plugins** (crash + licence isolation); own demods in-process. Small manifest + one IPC contract; not a plugin marketplace.
- **P3.4 (ADR-0004).** Stream contract = length-prefixed framed messages over UDS/TCP (+ optional ZeroMQ), SigMF-style headers, **drop-not-block** backpressure, `content_class` gating enforced here.
- **P3.5 (ADR-0005).** Scheduler = interleaved discovery sweep + POI-sized dwells, bandit revisit on interestingness (computed by C12), user intent preempts, TX gets exclusive slots. First version a simple alternation.
- **P3.6 (ADR-0006).** Storage = SQLite state + tiled spectrum pyramid + SigMF files + RAM pre-trigger ring, one data dir. Engine reversible behind a repository layer.
- **P3.7 (ADR-0007).** CPU/NEON for control+detection+decode, **GPU for FFT/channelizer/persistence/ML** (unified memory, no copy tax), **no usable FPGA** on HackRF. Low-power survey mode vs full mode.
- **P3.8 (ADR-0008).** Offline-first cache of feeds+reference data; passes computed locally from cached TLEs; seed cache shipped; sync opportunistically.
- **P3.9 (ADR-0009).** Jetson Orin Nano Super 8 GB (verified $249/JetPack 6.2/67 TOPS/7-25W+MAXN) + HackRF One + NVMe; phone-as-display first, on-device screen later; preselector/notch bank is the highest-value accessory.
- **P3.10 (ADR-0010).** Rust core / Python orchestration-only / TS+WASM UI. Licence ledger: **liquid-dsp (MIT) in-core, VOLK/GNU Radio (GPLv3) plugin-only** → the project licence stays open. Several rows marked (verify) before adoption; vocoder IP and feed ToS tracked separately.

## Open questions for the user (ranked)

1. ~~**Accept the proposed trunking use cases SIGNAL-080..086?**~~ **Resolved 2026-09-13: accepted by the user** (build log B0.1). They shape the C23 milestone and the roadmap. If any are unwanted, say which; IDs are permanent so rejected ones would be marked retired, not deleted.
2. **Restricted-content gating owner (P1.3).** I put enforcement on `stream-output` (C24) with a content-class flag set at classification, provisional pending a Phase 3 legal-guardrail ADR. Confirm that's the right seam, or name another.
3. **Own-key decryption (P1.3)** modelled as a `decoder-plugins` stage with user-supplied keys and key-source provenance. Confirm this belongs in C22 rather than its own capability.
4. Anything in docs/06 §5 ownership table you'd overrule before it hardens into the data model (doc 07)?
5. **UI direction (ADR-0002)** — you called this one of the most important decisions. I chose headless-core + web thin clients (reversible). Confirm, or say if you want a native on-device UI as the primary target.
6. **Runtime substrate (ADR-0001)** — FutureSDR-if-it-holds vs an owned Rust dataflow. Both meet the hard requirement; the spike S1 result decides. Any preference to force one now?
7. **Project licence (ADR-0010)** — still deliberately undecided; the architecture keeps it open. No action needed unless you want to pick early (it would let GPL code into the core and simplify some choices).

### Phase 4 — risks & spikes (2026-09-13)
- **P4.1** Seven spikes S1–S7 defined, each tied to the ADR/capability it unblocks. Recommended order **provisional**: S4 (detection in overload) + S5 (blind estimation) first — they run on the user's Mac+HackRF now and de-risk the two highest-impact unknowns and produce the first fixtures; then S1 (reconfig), S3 (web waterfall) on Mac; then S2, S6 on the Jetson; S7 optional.
- **Needs user:** pick which spikes to run (brief checkpoint). Default = the S4/S5/S1/S3 Mac-runnable set now, Jetson spikes when hardware arrives.

### Phases 5–7 (2026-09-13)
- **P5.1** Test tiers T1–T6; `test_tier` rubric = {offline-synth, offline-recorded, hil, field, data-only}; SigMF fixtures, licence checks, CI-without-hardware. Bulk test_tier fill delegated to a Sonnet agent (merge pending).
- **P6.1** Slice M0 = SIGNAL-001 (ADS-B, known decoder), AWARE-036 (unknown signal), SIGNAL-062 (FM/RDS auto-demod), AWARE-006 (attack map), SPACE-050 (science), AWARE-053 (priors), AWARE-042 (occupancy/region-over-time). Six of seven CI-provable offline. **Provisional** — depends on user accept of trunking (for M4) and spike results (S1/S3/S4/S5).
- **P6.2** Milestones M1–M7 ordered by docs/06 §4.2 shared-core: decoder breadth → attention+memory → classification/ML → trunking → accessory clusters → localization → device+TX.
- **P7.1** 25 M0 tasks in docs/tasks.yaml with deps/use-cases/acceptance/files/needs/model/effort/parallel-group/DoD; core-interface tasks flagged Fable/Opus + review. Repo scaffold, macOS-dev→Jetson-deploy, CI (no hardware), fixture capture plan, hardware shopping list with timing all in docs/12.
- **P7.2** CLAUDE.md gained Engineering + Coordination sections and a refreshed Status (kept concise).
- **Needs user:** confirm the M0 slice IDs, pick spikes to run, and the open questions above before build starts.

### Phase 5 follow-up — test_tier (2026-09-13)
- **P5.2** test_tier merged into use-cases.yaml (bulk-filled by a Sonnet agent against the docs/10 §2 rubric, reconciled). 57% (225/398) are CI-testable offline. SPACE/PROP skew to `field` because the science claim is a real-world physical one; their pipeline is still tested offline. Summary table + regen command in docs/10 §6.

## Build log (M0)

### 2026-09-13 — build start
- **B0.1 User decisions this session:** proceed autonomously on provisional defaults; **SIGNAL-080..086 accepted** (status flip in docs/05 + YAML pending a doc-sync task); no Fable — fable-tier tasks run on Opus high.
- **B0.2 Wave 0 launched:** T-001 scaffold (Opus high; also seeds hk-model ids/Provenance/SigMF types — override recorded in tasks.yaml), spike S1 (FutureSDR vs owned dataflow), spike S3 (web waterfall fps), all in worktrees.
- **B0.3 Tooling installed on the dev Mac:** just 1.58, git-lfs 3.8, rtl_433 25.12 (GPLv2+, truth labelling / plugin), readsb 3.16.16 (GPL, T-015 plugin) via Homebrew.
- **B0.4 Fixture store (PROVISIONAL):** raw captures go to `fixtures/store/<date>/` (gitignored) with a sha256 manifest; only small (≤25 MB) trimmed SigMF snippets are committed via LFS. Reversible.
- **B0.5 Coordinator-run HackRF capture batch** (receive-only, antenna as attached/unknown): ADS-B 1090, FM 101.3 (+RDS), ISM 433/915, 20 Msps urban gain-step + retune sets at 98/99/915 MHz, hackrf_sweep 1 MHz–6 GHz. Feeds S4, S5, T-025.
- **B0.6 Observation:** FM-band sweep shows the strongest "station" at 100.002 MHz, not a US FM channel — almost certainly a 10 MHz reference harmonic (internal spur). Useful ground truth for the S4 spur mask.
- **Blocker (needs user, physical):** terminated-input spur-map capture (docs/12 §4) needs the antenna swapped for a 50 Ω terminator. Deferred; spur-mask work uses the 100 MHz harmonic and gain-step/retune tests meanwhile.
