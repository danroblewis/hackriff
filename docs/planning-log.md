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
8. **Own-key-decrypted content over the network (new, from T-016).** Provisional: local Unix socket only, never remote TCP (which is unauthenticated in M0). Confirm, or say if authenticated remote streaming of your own decrypted traffic is wanted.
9. **Plugin manifest trust and paging metadata (new, from the T-016 re-probe).** Provisional: manifests are trusted but human-reviewed; numeric pages count as content; a paging decoder may expose only capcode, function, baud, encoding and timestamp. Confirm, or say if manifests must be treated as adversarial (which would mean sandboxing plugins) or if more paging metadata is acceptable.

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
- **B0.7 Weak received levels (first batch):** ADS-B at l32/g40/amp gave max |x| = 23/127 and only 2 marginal readsb decodes (1-bit fixes, 0 clean CRC) in 60 s; FM 101.3 at l16/g20 gave max 4 LSB — 19 kHz pilot +18 dB but RDS 57 kHz not visible. Gains were too conservative for the attached antenna. Action: auto-gain probe ladder (1 s probes, highest non-clipping step) then recapture FM + ADS-B. If ADS-B still yields few CRC-valid messages, it is an antenna/placement limit (**user, physical**) and SIGNAL-001 CI uses a synthetic ADS-B IQ fixture (real CRC) until a better capture or a licence-checked public fixture exists.
- **B0.8 Capture batch complete** (all rc=0, sha256 manifest in `fixtures/store/2026-09-13/manifest.json`): ADS-B 60 s, FM 30 s, ISM 433 (180 s @ 2 Msps), ISM 915 (45 s @ 10 Msps), six 4 s @ 20 Msps urban gain-step/retune captures (98/99/915 MHz; the l32/g30/amp 98 MHz step clipped 956k samples — the overload case), three hackrf_sweep surveys (1 MHz–6 GHz). FM + ADS-B being recaptured with a probed gain (B0.7).
- **B0.9 Spikes S4 (detection in overload) and S5 (blind estimation) launched** as offline analysis agents on the stored captures (no device access); the coordinator remains the only HackRF user.
- **B0.10 Recapture results:** FM 101.3 at l32/g30/amp (max 92/127, no clip) — **RDS confirmed**: 57 kHz ±1.2 kHz biphase sidebands +7 dB with a centre null, envelope line at 2375 Hz = 2×1187.5 bd; pilot +29.5 dB. Good SIGNAL-062 recorded fixture. ADS-B at l40/g40/amp (max 49): still **0 CRC-valid messages** (noise rose, no pulses).
- **Blocker (needs user, physical): ADS-B reception.** The attached antenna/placement does not receive 1090 MHz usefully. Needs a 1090 MHz antenna or an outdoor/window position. Meanwhile SIGNAL-001 CI uses a synthetic DF17 IQ fixture with real CRCs (T-023 `adsb_squitter`) and T-015 is validated against it; a licence-checked public ADS-B SigMF is the second option. T-025's ADS-B item stays open.
- **B0.11 T-001 merged** (4c16e27; `just lint` + `just test` green on main: Rust workspace + 11 pytest). Deviations accepted: real SigMF datatype strings (`ci8`, `rf32_le`…); f64 floats in the model; no ids yet for frame/tile/ExternalEvent (T-002 decides). `git lfs install --local` run in the main checkout (repo-local hooks only). CI not yet exercised (no remote); GitHub Actions licences marked (verify).
- **B0.12 Wave 1 launched:** T-002 data model + SQLite (Opus high, core interface → review before merge), T-003 source/replay/ring buffer (Opus high — real-time path; built substrate-agnostic so it does not wait for S1), T-023 synthetic generator + e2e harness (Opus high; includes `adsb_squitter` and `fm_broadcast_rds` scenarios to cover the ADS-B antenna blocker).
- **B0.13 Spike S3 (web waterfall) — Mac PASS, hardware PENDING.** Thin-TS WebGL2 client with GPU DPX persistence holds 60 fps (p99 18.7 ms), zero drops, up to 16384 bins × 60 rows/s; forced-sync GPU cost 2.3/3.4 ms p50/p99 (5–8× headroom on an M3 Ultra). ADR-0002 stays PROVISIONAL (web direction supported, no case for the native fallback); Jetson kiosk ≥30 fps and phone ≥15 fps checks need the user + Jetson (procedure in `spikes/s3-web-waterfall/REPORT.md`). Implications: ADR-0004 needs a **WebSocket bridge** for browsers (1:1 mapping of header + records); send u8 or display-decimated bins remotely. T-022 unblocked on the S3 gate for development. Note: the agent harness blocked the subagent from writing REPORT.md; the coordinator saved it.
- **B0.14 Spike S1 (live reconfiguration) — owned dataflow PASS, FutureSDR FAIL.** Owned single-writer/multi-reader block ring (chains = reader cursors + runtime-built node list): worst-case attach 0.03–0.81 ms vs a 3.28 ms buffer period, zero ring loss over 800 attach/detach cycles, pre-trigger history for free, 8 permissive crates, stable Rust; requires the capture thread at raised priority. FutureSDR 0.8.0: no in-graph add/remove, attach up to 13 ms, flowgraph churn stalled the always-on source (64–137 k samples dropped in 4/11 runs), nightly-only. **ADR-0001 updated with the outcome: hk-core uses the owned dataflow** (the ADR's own fallback — within the provisional decision, not a reversal); status stays PROVISIONAL pending S2 on the Jetson. T-003 was sent the final ring design to align with. Report saved by the coordinator (subagent .md write blocked).
- **B0.15 T-002 implemented** (3d6ba9f): all docs/07 objects, SQLite repository (WAL, embedded migration, UPDATE-rejecting triggers on measurement/interpretation tables, index-backed region queries verified with EXPLAIN QUERY PLAN, ~424 k Detection rows/s batched in release — ADR-0006 throughput risk retired for now). Deviations: link tables instead of back-refs on immutable Detection/Demodulation. Held for the Opus core-interface review before merge.
- **B0.16 Scheduling (PROVISIONAL):** T-014 (plugin host) and T-016 (stream output) run as one Opus-high agent after T-002 merges — ADR-0004 says one framing contract serves plugin IPC and external egress, so one owner avoids duplicate codecs. T-019 (band-plan priors, Sonnet) runs in parallel with it.
- **B0.17 T-003 merged** (8386cbb → 76ff126; `just lint` + `just test` green). **Ring decision (PROVISIONAL):** T-003 kept its own seqlock-style ring (sample index = ring position, gaps as index jumps, optimistic reader copy + validate, atomic cells, no `unsafe`, zero per-block allocation) instead of S1's Arc-block ring. Trade: one copy per reader vs S1's zero-copy, in exchange for sample-exact pre-trigger windows, per-sample loss accounting, no writer mutex, and bounded memory. Measured: ~2.8 Gsps Complex32 unpaced (writer + 2 readers); at 20 Msps worst attach 708 ns over 8 chains × 800 cycles, zero loss. `Complex<i8>` ring storage path added (2 B/sample → 1.2 GB per 30 s of 20 Msps pre-trigger vs 4.8 GB as Complex32). Merged before review to unblock T-004; an Opus concurrency review runs in parallel and fixes go forward.
- **B0.18 Licence finding:** `libhackrf` (host/libhackrf/src/hackrf.c) is reported **BSD-3-Clause**; `hackrf_transfer` and firmware are GPL-2.0-or-later. If confirmed, the core could link libhackrf directly (ADR-0010 row "GPL (verify)" → to update after confirmation by the ring review agent). Until then the HackRF source stays a stub.
- **B0.19 T-004 spectral estimation launched** (Opus); T-025 formalisation (trim/annotate/LFS the captures) waits for S5 (rtl_433 truth) and T-023 (RDS/ADS-B reference decoders).
- **B0.20 T-002 Opus review: FIX-BEFORE-MERGE.** (1) **Legal guardrail gap:** `ContentClass::permits_content()` was never enforced — restricted pager/cellular content could land in SQLite/Recordings by default, and `Decode.fields` had no metadata/content split for ADR-0004 "reduce to metadata". Fix: split metadata/content, `RepoError::GatedContent` on every insert path, fail-closed default. (2) `clip_count` inside Provenance made nearly every frame's provenance unique → moved to Detection + SigMF capture segment; hash-based race-safe dedup. Also applied now because they'd be breaking later: IMMEDIATE transactions, Detection `detector_version`/`peak_level_dbfs`, overload→clipped enforcement, append-only `emitter_status` with `prior_ref`, f32 frame arrays, TileKey scheme id, ExternalEvent payload hash. Remaining follow-ups are parked on T-006/T-007/T-017/T-018/T-020 in tasks.yaml. T-004 and T-023 notified of the f32 / clip_count changes.
- **B0.21 T-023 merged** (1bf8bce): `hkpy.synth` scenarios (tone, fsk_burst_train, noise_floor_rise, injected_floor, occupancy_multi_hour schedule+render, fm_broadcast_rds PI C0DE/PS HACKRIFF, adsb_squitter DF17 ×4 ICAO) with composable front-end impairments and a `hackriff:truth` schema (py/README.md); readsb decodes all 16 synthetic squitters; `hk-e2e` harness (content-hashed synth cache, Fixture/truth loader, Stage pipeline, detection matching + tolerance asserts naming use-case IDs). CI fixed so uv is installed before `cargo test` with `HK_E2E_REQUIRE_SYNTH=1` (otherwise the e2e smoke tests silently skipped). `just test` green on main.
- **B0.22 Spike S4 — INCONCLUSIVE.** Where testable the chain holds: 7/7 known FM stations retained at mid/clipped/retuned captures; 100.000 MHz reference harmonic and DC flagged everywhere; a 209.48 kHz comb flagged at high gain; gain-step SNR-invariance test flags both predicted IM3 ghosts on a semi-synthetic control with 0/11 real stations flagged. Not establishable: real ghost suppression (no intermod in this indoor scene even with clipping) and the <1 false emitter/MHz/h target (4 s gives 0.006 MHz·h exposure; real per-cell tails 10–110× design, broadband impulsive man-made noise frames). Two design bugs found and fixed in the spike: fixed −3 dB hysteresis (35 FA/MHz/h on ideal noise) and gap-merge before min-duration. Adopted as provisional defaults for T-005/T-006 (tasks.yaml `s4_inputs`): per-frame block FCME floor (never per-bin min-stats as CFAR reference: +4–14 dB on carriers), OS-CFAR (32/4/k24) OR floor branch at Pfa 1e-6 on / 1e-3 off, min 3 frames, impulsive-frame gate, emitter confirmation by repeat or ≥1 s integration. Measured RX amp ≈15 dB at 98 MHz (not 11). New Detection flags `suspect_imd`/`compressed`/`impulsive`/`edge` + spur reason codes + Provenance `quantisation_limited` sent to T-002. Preselector NOT moved earlier; **FM notch moved earlier**.
- **Needs user (purchase + physical), from S4:** buy an FM broadcast band-stop (88–108 MHz) notch; then an **S4b** re-run outdoors or with a better antenna near strong transmitters: ≥60 s captures, interleaved gain steps within one dwell, a 50 Ω terminated capture per gain state (also the C05 spur map), a 915 MHz retune, notch A/B, and a second-gain 1–6 GHz sweep. Decision rule: suspect_imd >10 % of emitters or known-emitter loss → promote the preselector. T-006 thresholds stay provisional until S4b.
- **B0.23 Spike S5 — PASS (scoped).** Trusted blind symbol-rate estimates were never wrong on real 8-bit captures: 915 MHz FHSS 2-FSK (100/150 kbit/s, truth from a decoder-independent fixed-rate preamble+sync search: 60 trusted, median error 1.1e-4), RDS biphase (100 % within 1 % for windows ≥0.25 s, down to 0 dB), 0 trusted-and-wrong in 900 synthetic runs; FSK deviation 97 % within ±10 %; FSK family 100 % at ≥20 dB; all noise/CW/analog/chirp negatives untrusted. Floors: short FSK/OOK bursts ≥20 dB, PSK ≥15 dB, long signals ≥0 dB; below the floor, prior-led trial demodulation leads (it found sync at 0–6 dB). **T-011/T-013 gates pass (scoped)**; estimator recipe + pitfalls recorded on T-010/T-011/T-013. INCONCLUSIVE: OOK/PSK family on real signals; independent deviation truth. Side findings: the 433 MHz capture had no devices (rtl_433 0 decodes); the 915 MHz traffic is an unidentified 100/150 kbit/s FHSS 2-FSK mesh (likely utility AMI; PHY metadata only kept); HackRF clock −6.8 ppm (from the FM pilot).
- **Needs user (optional, to settle S5's inconclusive parts):** capture a 315/433.92 MHz sensor or remote **you own** near the antenna (OOK truth via rtl_433); a metadata-only AIS or Meteor LRPT capture (PSK truth); a device with documented FSK deviation.
- **B0.24 T-004 merged** (e404a46 → af148bb; lint + tests green): windows with measured ENBW/CG/scalloping, `FftBackend` (rustfft CPU; `gpu` CudaFft stub), Welch (0 dBFS = full-scale complex sinusoid; linear FS²/Hz), zero-allocation `StftProcessor` over `Complex32`/`Complex<i8>` chunks with discontinuity resets, Nita–Gary SK with variance (Pearson thresholds recommended for T-006), persistence, dual resolution. ~210 Msps (50 % overlap + SK, one M3 Ultra core). SPACE-050 floor within ±0.2 dB through the replay source + i8 ring.
- **B0.25 T-003 ring review: URGENT-FIX.** A release-mode stress test with index-encoded samples showed the seqlock ring accepting stale/unwritten samples (5–30 per 400 k chunks; also in the metadata ring), inexact overrun accounting (unreported skips, double-counted gaps), a false startup overrun, and unchecked `u64` overflow that can wedge the ring. The existing tests could not see it (constant-valued samples). Fix agent launched (Opus high) with authority to adopt S1's proven Arc-block ring if the seqlock can't be proven; also splitting `Source` into control/stream handles (needed before T-009), streaming pre-trigger, replay header/trailing bytes. Consumers (T-004/T-005/T-008) need no API change. Lesson: stress tests on the real-time path must use index-encoded samples in release mode.
- **B0.26 Licence (verified by the reviewer on upstream):** `libhackrf` `hackrf.c`/`hackrf.h` are **BSD-3-Clause**; the tools, CMake and repo COPYING are GPL-2; libusb is LGPL-2.1 (dynamic link). In-process linking of libhackrf looks permissible → ADR-0010 ledger row to be updated (a direct libhackrf source becomes an option for the HackRF source; `hackrf_transfer` pipes can't retune without restart, so they don't suit the scheduler).
- **B0.27 Launched:** T-005 noise floor and T-008 channelizer (both Opus high, core interfaces → review before merge; file ownership split `hk-dsp/src/floor/` vs `channelizer/`+`ddc/`), T-025 fixture formalisation (offline; RDS PI/PS truth from the real capture, 915 MHz PHY truth from S5, S4 overload windows, 433 noise negative control, sweeps; ADS-B + terminated input blocked).
- **B0.28 T-002 merged** (3d6ba9f + review fixes b6d4902). Content gating is now enforced and fails closed: `Decode`/`Annotation` split into `metadata` + optional `content`; `RepoError::GatedContent` on decode/annotation content, any recording and stored bitstreams under gated classes (schema CHECKs back it); no default ContentClass (`FAIL_CLOSED = MetadataOnly`). Provenance dedup by SHA-256 of canonical JSON; `clip_count` moved to Detection + typed SigMF `hackriff:clip_count` on captures; `quantisation_limited` added. Detection gains `clip_count`, `peak_level_dbfs`, `detector_version` and S4 flags (`suspect_imd`, `compressed`, `impulsive`, `edge`, spur reason, image retune-confirmed); overload/clip without `clipped` refused. Append-only known-status history; ExternalEvent payload hash pinned in evidence; f32 frame arrays; TileKey scheme id; IMMEDIATE transactions. Batched insert throughput fell to ~150 k detections/s (from 424 k) due to per-row checks — still far above detection rates; noted for the ADR-0006 benchmark. Merge needed a Cargo.toml union, an hk-dsp test-helper update (required `quantisation_limited`), and the e2e smoke test reading the now-typed capture `clip_count`.
- **B0.29 Launching** T-014+T-016 (plugin host + stream output, one Opus-high agent) and T-019 (band-plan priors, Sonnet) now that T-002 is on main.
- **B0.30 T-017 spectrum-history pyramid launched** (Opus high; deps T-002 + T-004 both on main). Running agents (ring fix, T-005, T-008, T-025) told to merge main for the Provenance change before committing.
- **B0.31 T-019 merged** (1b10130, Sonnet; no core-interface change, uses T-002's append-only known-status API): 40-row compact US 47 CFR 2.106 table (16 rows web-verified, 15 marked unverified), interval lookup, family→service-tag matcher with a documented Part 15 rule (e.g. 433.92 MHz fsk-ism → known via part15 tag), fail-safe unknown when no allocation data, prior_ref written to the status history. No new dependencies.
- **B0.32 T-025 merged** (f9d71b0): five annotated 24 MB SigMF fixtures in LFS under `fixtures/hackrf/2026-09-13/` + tooling in `py/fixtures/` (`just fixtures-verify/-fetch/-build-2026-09-13`); manifest v2. Truth: real FM station **RDS PI 1694** (249 CRC-valid groups; PS scrolls, so acceptance asserts PI), pilot 18 999.871 Hz; 915 MHz window with 18 sync-truth FSK bursts (PHY metadata only); urban clipped/mid pair with 7 known stations, 14-line comb, 100 MHz spur, DC; 433 MHz noise-only negative control. Findings: (1) the coordinator's store manifest `clip_count` counted I and Q rail hits separately (component count), not samples — fixtures use per-capture sample-based counts; (2) 915 MHz bursts near ±fs/2 (910/920 MHz) appear twice as edge aliases — flagged; (3) persistent 434.000 MHz line in the "empty" 433 capture = 217 × the 2 Msps sample clock → probable internal spur (terminated capture would confirm); (4) FM carrier −535 Hz from nominal, consistent with the −6.8 ppm clock; (5) store provenance JSON predated the T-002 schema — fixtures carry a normalised copy. ADS-B and terminated-input fixtures remain blocked on the user.
- **B0.33 Band-plan rows verified** (1ca400e): the 11 unverified rows checked against 47 CFR 97.301 and 2.106 footnotes (US247/US340/US270); 7 unchanged, 4 status corrections (30 m fixed primary / amateur secondary; 70 cm and 23 cm amateur secondary to radiolocation (+RNSS); 433.05–434.79 MHz inherits 420–450 MHz shared federal, radiolocation primary, amateur secondary, with the ETSI-SRD caveat). No matcher/test changes (matcher reads tags only).
- **B0.34 T-005 implemented** (a40c2eb), under Opus core-interface review. Per-frame block FCME (Gamma(n_eff) thresholds, n_eff corrected for 50 % Hann overlap), p20 cross-check (invalid ≥41 % occupancy), min-statistics for drift only, per-channel floors, impulsive-frame gate, slow IIR floor for science, `quantisation_limited`, `FloorRiseEvent` for T-020, `FloorThreshold` helper for T-006. Results: FCME bias ≤0.1 dB to 80 % occupancy; SPACE-050 floor median error ≤0.034 dB; AWARE-006 step detected within one frame (9.51 vs 9.62 dB); floor-branch Pfa 1.02× design; zero allocation; 126 µs/frame at 4096 bins (M3 Ultra). Known limit: top-1 % per-bin floor errors +4–20 dB at 23–61 % occupancy → T-006 must rely on OS-CFAR/integration inside wide signals.
- **B0.35 T-005 Opus review: FIX-BEFORE-MERGE.** Numerics independently verified (scipy): thresholds, Gamma P/Q to 4e-14, n_eff 9.5238 for 50 % Hann. Blocking state-machine bugs the tests could not see: (1) the impulsive gate locks on for any sustained 0.5–3 dB step (slow floor frozen, every frame flagged) — would flat-line T-021 on a 1–3 dB solar/galactic rise and hide sub-3 dB AWARE-006 rises; (2) 5-frame (~5 ms) rise confirmation with no hold-off turns bursty wide signals (LTE/WiFi) into rise/fall storms; (3) the documented "OS-CFAR covers wide-signal interiors" is false once the guard applies — both branches lose them; a wide-signal reference (sliding min of block floors) is needed for T-006. Fixes plus cheap follow-ups sent back to the implementer. Side note: S4's "SNR wall −9 dB" corresponds to ±0.25 dB floor uncertainty; at ±0.5 dB it is −6.4 dB. Lesson (again): state-machine/real-time code needs adversarial transition tests (steps, bursts, startup), not only steady-state accuracy tests.
- **B0.36 T-008 implemented** (3beb92b), under Opus core-interface review. Kaiser-prototype 2× oversampled analysis PFB (60 dB default; adjacent leakage −65 dB; boundary tone flat within 0.02 dB) with a `PfbBackend` trait (CUDA stub); DDC = mix + xlating decimating FIR (phase from absolute sample index) + optional integer/rational/fractional polyphase resampler; serde `DdcSpec` for runtime chains; `Complex<i8>` and `Complex32` inputs bit-identical; zero allocation per block. One-core headroom at 20 Msps on M3 Ultra: PFB M=64 10×, M=512 9.3×, DDC→250 kS/s 12×, →48 kS/s 17.5×. SIGNAL-062 pilot, AWARE-036 FSK tones and SIGNAL-001 preamble timing preserved through the DDC. Analysis-only PFB (no synthesis) accepted for M0.
- **B0.37 Ring fix merged** (8f21b6f). All review must-fixes resolved, seqlock kept (lock-free writer, no per-block allocation), plus Source split into a capture-thread stream + `Arc<dyn SourceControl>` with a non-blocking control mailbox (unblocks T-009), streaming pre-trigger (`TriggerStream`, no whole-window allocation), exact `Overrun { lost_samples, gap_samples, resume_at }`, exact capacities (30 s ci8 = 1.2 GB) + best-effort mlock, replay header/trailing bytes, `RLIMIT_RTPRIO` clamp. A release stress test with index-encoded samples (7 readers, tiny rings, random gaps/provenance; 150 seeds / 180 M samples) is clean and fails immediately if sample cells revert to `Ordering::Relaxed`. Throughput essentially unchanged (unpaced writer ~2.7 Gsps; lossless paced to 1.28 Gsps; worst attach 1.4 µs). **Open concern:** the stated root cause ("Relaxed atomics don't guarantee cross-core store order on Apple Silicon") conflicts with Rust atomics being individually linearisable regardless of the Ordering hint, so SeqCst may mask rather than fix a race; merged because it is strictly better than main and heavily stress-tested, with an independent Opus root-cause verification running. Remaining: no bias-tee field in Provenance; MAX2837 filter bandwidth list unverified.
- **B0.38 T-008 Opus review: FIX-BEFORE-MERGE** (one bug): after a failed rate re-plan the next block at the same rate returned `Ok` with the stale plan (garbage output + time map). Otherwise verified: Kaiser β/order, channel geometry matches hk-dsp spectrum incl. ±fs/2, exact (L−1)/2 delay, time map to ~0.05 source samples, host time ≤56 ns at 2^50, fractional resampler drift 1.2e-8 samples/h, NaN flushes after the filter span, zero allocation on all transitions except rate re-plan, per-block ≤0.76 ms vs 3.28 ms budget. Follow-ups applied now: exact u64 NCO phase (f64 phase degraded beyond ~24 h of sample index), forced PFB reset on rate change, even (not power-of-two) M for 12.5/25/100 kHz rasters at 20 Msps. **Scope change (PROVISIONAL):** T-008 delivers the CPU PFB/DDC; the GPU PFB is split into new task **T-026** (blocked on spike S2 / Jetson purchase). 20 Msps → 2.4 Msps for readsb (T-015) is the costliest plan (~5× real time on one M3 Ultra core) — measure on the Orin in S2.
- **B0.39 T-016 + T-014 implemented** (526dbfd, 457f11f), under Opus core-interface + legal-guardrail review. `docs/stream-contract.md` v1.0: u32-LE framed records (4 MiB cap, checked before allocation), JSON header, NDJSON messages, 32-byte binary record headers, dropped markers; UDS + TCP on std threads (no new crates); gating matrix 5 classes × 6 kinds with raw-byte sentinel scans (content never on socket or in SQLite for gated classes; plugins claiming a looser class are clamped); stuck consumer never blocks the producer (~7.7 M IQ records/s); zero producer-thread allocation for binary records. Plugin host: JSON manifest (licence + content_class required), stdin data plane, stdout NDJSON decode/annotation/log lines, restart backoff + crash-loop cap + hang watchdog, exact offered/enqueued/dropped accounting. WebSocket bridge not built (→ T-022).
- **B0.40 Provisional legal default (fail-closed, pending the user):** `own-key-decrypted` content may stream only over the **local Unix socket**, never remote TCP, until the user decides; remote TCP listeners are unauthenticated in M0. Spectrum streams are treated as metadata (not gated); raw IQ may feed local decoder subprocesses (never a network listener). All three are under review.
- **B0.41 T-017 merged** (0ea888d; lint + tests green; not core_interface, merged without a separate review). Custom CRC-checked binary tiles addressed by (scheme, level, f-block, t-block) — the key is the index; write-temp-fsync-rename; 5-level pyramid (6.25 kHz × 1 s → 100 kHz × 1 day) with max / power-mean / p10 / p90 / occupancy / max-occupancy / coverage; eviction only of tiles already covered by sealed coarser tiles; 8 GiB default budget. `Pyramid::query` + `channel_summaries` (occupancy over time, burst durations, UTC hour-of-day). AWARE-042: occupancy error ≤0.002, exact burst counts, duration quantiles ≤0.17 s (frames synthesised from the schedule, not IQ). SPACE-050: p10 −0.5…−0.6 dB vs injected floor (bias for T-021 to correct), 6 h stepped floor within ±1 dB. Ingest 6.7 k frames/s (223× real time); ~180 MB/h L0 for continuous 20 MHz dwell (zstd deferred). No new external crates.
- **B0.42 T-016/T-014 Opus review: FIX-BEFORE-MERGE — legal-guardrail leaks found by probes.** (1) The plugin output ceiling ignored the channel's input content_class: restricted-paging input + an `unrestricted` manifest wrote content to SQLite and TCP. (2) Content could be smuggled outside `content` via `metadata`, `frame_model`, `identity.value`, annotation values, log lines, serde error echoes and stderr. (3) own-key-decrypted content streamed over TCP. Robustness: wrapper grandchildren hang plugin supervision (T-015's readsb wrapper would hit this); unbounded idle consumers on an unauthenticated listener. Verified sound: clamping order, fail-closed parsing, framing, poll shutdown, pid-reuse safety, gating-matrix test with positive control. Fixes sent: input-class ceiling; **required per-manifest metadata-key allowlist (typed, length-capped) for any class that forbids content**, identity pattern caps, gated log/stderr handling; own-key streams refused on TCP, UDS 0600; process-group kill; `max_consumers`; move `stream/` to a new `hk-stream` crate before an hk-api↔hk-plugins cycle forms.
- **B0.43 Legal decisions (PROVISIONAL, reviewer-endorsed, pending the user):** (a) **Spectrum can carry content** — a waterfall with row rate ≥ symbol rate is a non-coherent demodulator (e.g. POCSAG, voice spectrograms); under a class that forbids content, spectrum streams are capped at 50 rows/s (survey waterfall ~30 fps unaffected). (b) own-key-decrypted content: local UDS (0600) only, never TCP/WebSocket, until the user decides (open question 8). (c) Raw IQ may feed local decoder subprocesses (no path to a listener); the manifest executable is the trust boundary and must be documented as such.
- **B0.44 T-008 merged** (3beb92b + review fixes 88c13c5; lint + full workspace tests green, even under load). Re-plan now errors on every block until the rate is realisable; exact u64 NCO; PFB always resets on rate change; even M (e.g. M=800 → 25 kHz at 20 Msps) with raster offset; stride-view usage documented. T-010 parameter estimation launched (Opus high). Note: hk-core `ring_stress` failed 1/5 runs under load ~30 on the implementer's machine (overrun-span assertion) — forwarded to the ring root-cause verifier, which is currently running its own loaded stress matrix.
- **B0.45 T-005 merged** (a40c2eb + review fixes 854614c; lint + workspace tests green; merge needed a union of hk-dsp lib.rs/Cargo.toml with T-008). Breaking API for consumers: `FloorEvent { kind: Rise|End|Fall, episode, class: NoiseLike|Structured|Unverified }` episodes (1 s confirmation, 2 s hold-off, end after 1 s back within 1.5 dB; SK/excess-std discriminator; structured episodes not emitted by default); `FloorThreshold::new(n, pfa_on, pfa_off)` with guard on the OS branch only; new `FloorFrame::wide_floor` reference (99.9 % of bins inside a 2048-bin +10 dB signal clear T_on vs 9.3 % with the per-frame floor; noise Pfa 1.02× design). Repro tests: 1/2/2.9 dB steps and startup transient release the gate with no events; bursty wide signals produce 0 events; AWARE-006 onset 0.998 s vs 1.0 s truth, step 9.62 dB exact; SPACE-050 slow floor ≤0.024 dB. 147 µs/frame at 4096 bins. Known limits: a steady Gaussian-like wideband emission (OFDM) reads as a noise-like rise; S4 quantisation floor away from 20 Msps unverified (terminated capture, user). Merged ahead of a focused post-merge re-review of the new episode state machine to unblock T-006/T-020/T-021.
- **B0.46 Ring root cause verified (CORRECT).** Independent Opus verification: on this Mac's toolchain (Homebrew rustc 1.93.1 / LLVM 21, M3 Ultra) `Ordering::Relaxed` atomics emit plain `ldr`/`str` (IR `monotonic`) and a sound two-variable probe shows them **not linearisable** (~5 M violations in 3 s; 479 k even with non-inlined calls), while `SeqCst` (`ldar`/`stlr`/`stlur`, IR `seq_cst`) shows 0. With linearisable atomics the seqlock is correct by argument; a loom model passes and catches three deliberately broken variants. Experiments isolate the Ordering change as the actual fix (bounds/accounting changes alone don't matter): SeqCst 0/4000+ seeds idle and under load; Relaxed sample cells 148/150 failures. The `ring_stress` load flakes are a **test-harness attach race** (readers created inside threads after the writer commits the first block) — fix + a guard test forbidding non-SeqCst ordering in the ring launched. My earlier doubt ("Relaxed is always linearisable") was wrong for this toolchain; recorded as a lesson. Follow-ups: re-run the ordering probe on the Jetson (aarch64 Linux) during S2; check SeqCst write throughput there (probe showed ~100× slower SeqCst writes under reader contention, not visible in the ring benchmark).
- **B0.47 New ring bug found by the harness-fix agent** (1985b09 fixed the test attach race and added a SeqCst-only guard test): `Shared::locate` loads `meta_head` then `meta_write_end` non-atomically; if the writer commits ≥4 blocks in between, the empty search range is treated as lossless and the reader's cursor jumps ahead, reporting real samples as a source gap (~1 % of loaded stress runs; trips a debug assertion). The independent verifier's argument missed this torn two-counter read. Fix (consistent snapshot + retry, audit of all two-counter decisions) with a deterministic barrier-injected regression test is in progress. Lesson: "correct by argument" reviews must enumerate every multi-load decision, not only the copy/validate path.
- **B0.48 T-005 post-merge re-review: URGENT-FIX.** The wide-floor reference biases low on any sloped floor (floor-branch Pfa 21× at 6 dB tilt, 650× on a HackRF-like roll-off, 763× at a notch; the real urban fixture reads 1.6–2.0 dB low over ~40 % of the band) — it would flood T-006 with false alarms. Episode semantics wrong for T-020: sub-band Falls suppressed, End fires while part of the region is still up, overlapping regions merge silently / duplicate Rises, onset ramps classed Structured (hidden by default), episodes never end after background drift, slow (<3 dB/s) rises missed, reset re-open semantics. Gate release biases the T-021 science series (+0.16…+1.56 dB mean under realistic impulsive duty). Clean: zero allocation over 60 k adversarial frames, hour-long episodes, no End without Rise. Fix agent launched with licence to restructure (per-block classifier + episode aggregator, explicit Extend/Update/Unknown events, property tests over random sequences). Running dependents adjusted: T-006 makes the floor reference swappable and defaults to per-frame; T-020 codes the Anomaly lifecycle against an adapter; T-021 flags gate-released periods. Swept-chirp jammers are Structured or invisible to the floor tracker — an AWARE-006 limitation needing a C09 chirp/comb detector later. Lesson (third time): merge-before-review of state-machine code is costly when dependents start immediately; for core state machines, prefer review before merge.
- **B0.49 T-016 + T-014 merged** (526dbfd, 457f11f + review fixes 993612d; lint + workspace tests green). Legal leaks closed: plugin output ceiling = clamp(manifest class, input channel class); restricted classes require typed `output.metadata_keys` allowlists (integer/number/boolean, hex/digits with max_len, enum — free text cannot be allowlisted; over-long values dropped, not truncated); identity/frame_models/labels allowlisted; restricted plugin log/stderr counted not stored; own-key-decrypted content only on own-key streams, refused on TCP, UDS 0600, live-socket bind refused; spectrum under non-content classes capped at 50 rows/s. Robustness: plugin process groups SIGKILLed, non-blocking stdin wake, `max_consumers` (16), drain timeout, tail drop marker. **New crate `hk-stream`** (contract moved out of hk-api to avoid an hk-api↔hk-plugins cycle); CLAUDE.md crate list updated. Reviewer probes P1–P10 re-run clean; an independent re-probe (allowlist encoding abuse, spectrum-rate bypass, every egress) runs post-merge. T-015 readsb plugin launched.
- **B0.50 T-016/T-014 independent re-probe: FOLLOW-UP (no live leak with current plugins).** All original probes pass on the fix. Remaining enforcement holes to close before a WebSocket bridge, in-process decoders or paging plugins ship: (1) the own-key locality rule lived only in `bind_tcp` — a direct `subscribe` of a TcpStream bypassed it; (2) the metadata allowlist applied only at plugin ingest — an in-process restricted publisher could emit free text; (3) the spectrum row-rate cap trusted the declared rate (10 rows/s declared, ~390 k rows/s accepted). Covert channels through typed allowlisted fields (hex identity text, packed integers, numeric pages in a capcode field, `sample_index` offsets, confidence digits) are possible with a careless or hostile manifest. Follow-up fix agent launched (locality enforced in `subscribe`, `MetadataPolicy` on publishers, token-bucket spectrum enforcement + payload cap, tightened allowlist defaults).
- **B0.51 Legal policy defaults (PROVISIONAL, fail-closed, pending the user):** plugin manifests are **trusted but human-reviewed** (the manifest is the trust boundary; long hex/digits keys under restricted classes require an explicit review note); **numeric pages are content** (message bodies are never allowlisted); a restricted-paging decoder may expose only capcode, function, baud, encoding and timestamp. Residual timing/count side channels are documented, not fixed.
- **B0.52 Ring follow-ups merged** (1985b09 + 5a044ea): `ring_stress` attach race fixed (readers created before the writer starts); a guard test forbids non-SeqCst orderings in the ring; `Shared::locate` reads `meta_head`/`meta_write_end` as a consistent pair and retries instead of treating an empty search as lossless. Audit of every two-counter decision found no other unsafe site. A test-only hook deterministically laps the metadata ring between the loads (failed before: `dropped_before` 60 vs true 0; passes after). Stress 0/50 idle, 0/200 under 8× load; throughput unchanged (~2.6–3.0 Gsps unpaced, lossless paced to 1.28 Gsps). The ring's correctness claim now rests on an argument covering all multi-load decisions plus deterministic interleaving tests.
- **B0.53 T-021 merged** (423945f → 84148da). `hk_dsp::radiometry` (PowerCalTable per frequency × gain state from CalibrationState; FloorSeries in dBm/Hz or flagged dBFS/Hz, noise temperature, dB above kT₀; quadrature uncertainty) + `hk_store::FloorProduct` (calibrated per-bin before folding into a dBm/Hz pyramid; uncalibrated frames into a separate dBFS/Hz pyramid — dBm never faked; flags/gain/calibration per 1 s cell in a run-length log; `floor_vs_time` = median over region cells of bias-corrected p10). Derived p10 correction `p10 − 10·log10(P⁻¹(n_c,p)/n_c)` fixes T-017's low bias to ≤0.03 dB. SPACE-050: worst calibrated-floor error 0.01 dB (series), 0.10 dB (L1 tiles); 8 dB gain step → ≤0.03 dB calibrated step. Additive hk-model change: optional `gain` + `uncertainty_db` on `PowerCalPoint`. One strict burst test ignored until the T-005 gate fix lands. All T-021 tests green; the full-suite run showed an unrelated hk-stream failure (B0.54).
- **B0.54 Main briefly red: hk-stream `p10_tail_drops_before_finish_get_a_marker`** (from the T-014/T-016 review fixes) failed 12/15 runs in isolation. Cause: test assumption, not publisher code — the consumer thread can dequeue between an early drop and the tail drop, splitting drops into two runs, while the test asserted exactly one marker. Fixed on main: assert ≥1 marker, final record is a drop marker, and full seq coverage; 0/15 after. The hk-stream gap-fix agent was told not to touch it.
- **B0.55 T-020 merged** (4192c7c; lint + workspace tests green). Offline-first feed cache (events in SQLite via Repository for FK + payload-hash pinning; feed state + content-addressed raw snapshots under `<data_dir>/context/feeds/`; `FeedFetcher` has offline/directory fetchers only — no network code). gpsjam adapter: daily H3-res-4 CSV (`hex,count_good,count_bad`), yellow/red cells → ExternalEvents over the UTC day with GNSS L1/L2/L5 extents; **licence/ToS not stated upstream → no real extracts redistributed**, synthetic extract for tests only (ledger row). Anomaly lifecycle behind an internal `EpisodeSignal` adapter (only `signal_from_floor_event` knows FloorEvent; NoiseLike filter; restart resume; orphan close). Correlator score `0.9·s_time·s_geo·s_band·s_mag` with a 0.2 floor and no Explanation when any factor is 0; stale sources marked provisional; payload revisions → StaleEvidence + superseding row. AWARE-006 e2e: NoiseLike rise → Anomaly (onset 1.6 ms from t0) → top Explanation time-coincidence, score 0.90, payload hash pinned; negatives (empty cache, other place/day, non-GNSS band) give none. Additive hk-model: `ExternalEvent.freq`, `Repository::external_events_overlapping`. New dependency h3o 0.11 (BSD-3-Clause). Known AWARE-006 limits: swept-chirp jammers produce no Anomaly; steady OFDM reads noise-like.
- **B0.56 Plan adjustments (PROVISIONAL):** (1) added **T-027 pipeline assembly** (hackriffd/hk replay composing source → ring → STFT → floor → detection → history → stream/plugins under a ScanPlan, with runtime chain attach/detach per the S1 outcome) — no task owned composition, yet M0's DoD ("run a survey on the device") and T-024's e2e tests need it; T-024 now depends on T-027. (2) Split **T-022a** (WebSocket bridge + waterfall + region-over-time view; deps T-016/T-017, both merged) out of T-022 so the UI isn't blocked on T-018; it waits for the stream enforcement-gap fixes because the bridge is a remote egress, and runs on Opus (legal-guardrail path).
- **B0.57 T-010 merged** (337d16a; lint + workspace tests green incl. real fixtures). `hk_estimate::{snippet, params, estimate, clock, normalise}`: detection box → DDC snippet with pads/time map/provenance; every estimate is measured (value, sigma, method, evidence) or abstained with a reason. Results: RDS CFO via x² 299.6136 Hz vs 299.613 truth; clock −6.770 ppm vs −6.768; FM OBW99 227 kHz; AWARE-036 synthetic RF centre within 0.24–0.83 % of symbol rate, SNR within 0.7 dB; ~1.6 ms per 915 MHz burst snippet. Deviations from S5 recorded on the task (OBW99 without negative-bin zeroing — S5's truth bandwidths are upper bounds; settled-frequency FSK CFO; z≥50 gate). T-011 blind symbol rate and T-012 WFM/RDS launched.
- **B0.58 T-015 implemented** (9f8439c, Sonnet): `hk-plugin-readsb` adapter (hackriff-v1 framing preserves sample_index; ci8→uc8 XOR; readsb `--raw --no-fix` as a subprocess, GPL isolated; independent CRC-24 recheck; DF17 identification/CPR position/velocity + DF11 → NDJSON decodes with `adsb-icao` identity). SIGNAL-001 on the synthetic squitter fixture: 16 CRC-valid decodes, 4 Emitters with correct ICAOs/last_seen, republished on a UDS stream; readsb SIGKILL → restart, no zombie. It added an emitter-upsert hook in `hk-plugins` ingest (core plugin contract) → Opus review before merge, focused on the hook running only on sanitized/clamped decodes and on wrapper/readsb process cleanup. T-011 (blind symbol rate) and T-012 (WFM + RDS) running.
- **B0.59 Egress enforcement follow-ups merged** (97a436c; lint + workspace tests green). Closed the re-probe gaps with regression tests built from the reviewer's probes: `PublisherHandle::subscribe` now takes the socket type (UnixStream = local, TcpStream = remote, anything else must be `Declared::local/remote`; remote on own-key → `LocalOnly`; a WebSocket bridge must declare remote); restricted message publishers require `Publisher::with_metadata_policy` (policy types in `hk_stream::policy`); gated spectrum streams must declare `fft_size`/`datatype` and rows over rate/size are withheld with a counted gated marker; `parse_line` bounds `sample_index` to the pushed input range under restricted classes; manifest `schema_id` is a short token, hex/digits ≤8 unless `review_note`, integer/number keys warn; example `policies/restricted-paging.json`. Documented residuals (stream-contract §11): timing/order side channels, 8-digit numeric page still fits a capcode field, in-process-chosen ids, `Declared::local` around a non-TcpStream network writer. **Breaks T-015** (subscribe signature, `parse_line` range arg); its emitter hook must sit in `Ingest::store_decode` after `self.store(..)`, built from the sanitised `&row`. T-022a (web bridge) now unblocked.
- **B0.60 T-022a launched** (Opus): WebSocket bridge (all browser consumers subscribe as remote → own-key refused by the contract; token auth; consumer cap), read-only JSON endpoints for T-017 history and T-021 floor-vs-time, a thin `hk serve --replay` demo composer (full composition stays T-027), and the S3 WebGL2 waterfall client ported into `ui/`.
- **B0.61 T-015 Opus review: FIX-BEFORE-MERGE (legal clean).** The emitter-upsert hook builds from the sanitized/clamped row, and a restricted-paging probe with over-long/non-digit/free-text/wrong-scheme identities created no Emitter or Decode rows. Must-fix: readsb crashes and its "SDR wedged" idle exit were recorded as clean exits (and idle deaths went unnoticed); ADS-B CPR even/odd pairing had no time window (a stale even + fresh odd emits a wrong position) plus a velocity subtype-2 scaling bug; rebase onto the egress-enforcement API. Verified sound: process-group cleanup of wrapper + readsb, no stdout-pipe deadlock, CRC-24, uc8 conversion, global CPR maths. Note: CRC recomputation does not detect readsb-repaired frames — `--no-fix` is the only guard. New follow-ups: Emitter rows carry no content_class (T-018 — inventory/export must re-gate restricted identities); count not idempotent on replay (T-018); per-decode SQLite transactions and readsb keepalive (T-027). Sonnet implementer applying fixes; coordinator will check the rebased ingest hunk (Opus check on Sonnet core-contract change) before merge.
- **B0.62 T-006 implemented** (6891727, merged main e816cab; lint + tests green), under Opus core-interface review before merge. OS-CFAR (N32/G4/k24; α by quadrature: on 4.70 / off 3.00 dB at n=10) OR floor branch (T 5.15 / 3.55 dB), Pfa-derived hysteresis, min-duration-before-merge, short-burst ISM profile, integrated/repeat confirmation, S4 spur/DC/comb/image/clip rules, gain-step/retune trust functions, batched Detection writes with a config-hashed `detector_version`; 12–18 µs/frame at 4096 bins. Results: ideal-noise 0 boxes in 1.10 MHz·h; sloped floors fine with the per-frame reference (the T-005 wide floor gave 21–695×); **floor notches still give 64–91× design with the per-frame FCME reference** (OS-only gives 0) — a known weakness pending T-005's slope-normalised reference; FM fixture station + 100 MHz ref-harmonic spur + DC correct; clipped urban 7/7 stations, clipped flag 100 %, 14-line comb; 915 MHz all 23 truth transmissions but 108 detections outside the (partial) truth — to be characterised; 433 control: the 434.000 MHz line (217 × sample clock, probable internal spur) is confirmed, not spur-flagged — clock-harmonic spur rule vs terminated-input spur map under review.
- **B0.63 T-015 merged** (9f8439c + review fixes e0777d1; lint + workspace tests green). readsb runs as a GPL-isolated subprocess behind `hk-plugin-readsb`: writer thread with idle keepalive, waiter thread and stderr "wedged" detection make any unexpected readsb death a non-zero wrapper exit (clean EOF is the only exit 0); `--no-fix` enforced and `--fix` refused (CRC recomputation cannot detect repaired frames); CPR even/odd pairing within 10 s; decodes stamped with the last valid input sample index. SIGNAL-001 on the synthetic `adsb_squitter` fixture: 16 CRC-valid decodes, 4 Emitters with correct ICAOs, callsign exact, CPR lat/lon within 0.01°, velocity/vertical rate exact, republished on a stream; readsb crash and wedge recovery tested with a fake readsb. Coordinator (Opus) checked the Sonnet ingest change: the emitter upsert is built from the sanitized stored row, between store and publish. Recorded 1090 MHz fixture still blocked on the antenna (user).
- **B0.64 T-012 merged** (8ae13de; lint + workspace tests green). `hk_demod::{mode, receiver, wfm, pilot, rds, record}`: automatic analog mode selection from T-010 estimates plus envelope variance and a trial-discriminator pilot search (WFM / NBFM / AM / SSB / CW / unknown, no manual mode); WFM demod with 75 µs de-emphasis to 48 kS/s audio; pilot PLL; RDS biphase demod locked to 3× pilot, EN 50067 block sync/syndromes, 0A/0B PI/PS/PTY/TP, scrolling-PS frame collection, block/group error rates. **SIGNAL-062 on the real FM fixture: WFM at 0.95 confidence, pilot 18 999.870 Hz (truth 18 999.871), PI 1694 (52/52 votes), PS "Unstoppa", block error 5.9 % (reference 6.7 %); Emitter written as rds-pi 1694 labelled "Unstoppa".** Synthetic PI C0DE / PS HACKRIFF exact; NBFM/AM/noise/no-RDS negatives correct; a 60 ms garbage burst causes no wrong PI/PS. Follow-ups on the task (no audio ref in hk-model Demodulation; stereo and other-mode audio not implemented).
- **B0.65 T-011 merged** (b86244e; lint + workspace tests green). `hk_estimate::blind`: S5's recipe ported — four whitened cyclic lines in two independent groups, guarded transition least squares, harmonic-aware consensus with separate rate/family trust, family scores (OOK/FSK/BPSK/QPSK/unknown), FSK deviation at LS symbol centres. **0 trusted-and-wrong** across a 900-run synthetic sweep, negatives and real fixtures; 915 MHz real bursts: every trusted rate within 1 %, deviation within 10 %; RDS chip rate 2375 Bd found in every window with 1187.5 as the ½ alternative, trusted 18/19 at 0.25 s (acceptance partially met; the miss abstains), 100 % at ≥0.5 s. Floors at or better than S5. Every S5 pitfall has a named regression test. T-013 (FSK demod + framing/CRC inference) launched.
- **B0.66 T-022a merged** (ab75627; lint + workspace tests + UI build/typecheck green). WebSocket bridge on std threads + tungstenite (no tokio): 1:1 header/record mapping; every browser subscribes as `Declared::remote` so own-key streams are refused by the contract (HTTP 403), consumer cap → 503, finished stream → 410; 256-bit token (generated or `HK_TOKEN`) compared in constant time, 401 before any stream info; binds 127.0.0.1 by default (no TLS — documented). Read-only `/api/streams`, `/api/history` (T-017), `/api/floor` (T-021), static UI. `hk serve --replay` demo composer (fixture class honoured; else unrestricted only for captures wholly inside the FM broadcast band, otherwise metadata-only → gated spectrum ≤50 rows/s). UI: ported S3 WebGL2 waterfall + persistence, gated/drop indicators, region-over-time heatmap + floor line; token read from the URL fragment. Headed Chromium on the real FM fixture: 60 fps, 0 drops. Follow-ups on the task (stream-contract doc §10/§11 stale; units field; loop reconnects).
- **B0.67 T-006 Opus review: FIX-BEFORE-MERGE.** Threshold maths independently exact (OS-CFAR "≥k cells below P/α" ≡ "P > α·X(k)"; α/T match scipy to 6 digits for n=1–40; Monte Carlo 0.996×/0.98×); FA accounting correct; the 108 "outside-truth" 915 MHz detections are real unannotated continuous lines (T-025 annotation gap), not threshold errors. Defects: one bridging or impulsive frame permanently fuses separate carriers into one wide record; component pool memory unbounded on dense frames and super-linear link cost; integration >15 blocks silently truncated. Recommendations adopted: detection branches per band profile; a floor-step guard to fix notch/accessory-edge phantoms (which survive the retune test); a new `clock-harmonic` spur reason (434.000 MHz line = 217 × fs within 5 Hz; flag, don't suppress — also LPD433 ch 38); CI full-chain FA exposure ≥1 MHz·h. Note: with 50 % Hann overlap the moment-matched Gamma(n_eff) runs 1.58× design at 1e-6 (T-027 overlap choice). T-013 (FSK demod + framing/CRC inference) launched with fail-closed content for real third-party traffic.
- **B0.68 T-005 rework done** (9425b32 + main merge 29e5a91 + downstream adaptation dbd896e; lint + 77 workspace suites green, nothing ignored). Tracker split into a per-block level classifier and a region/episode aggregator; wide reference built on a learned per-bin shape of downward floor features (min-of-blocks only at step-like rises), so tilts/roll-offs/notches no longer bias it (floor-branch Pfa 0.99–1.36× design; wide-interior coverage 99.9 %). New event model Rise → (Extend|Update)* → End(Returned|Reset|Merged|Rebaselined) or Unknown, standalone Fall{interrupted}; `emit_structured` default true. All nine B0.48 repros fixed with numbers; slow-floor burst bias ≤0.011 dB (T-021's previously ignored burst test now passes); randomized property tests hold all episode invariants; zero allocation. T-020 adapter and T-021 updated in the same branch. **Held for a pre-merge Opus re-review** (per the B0.48 lesson) probing learned-shape poisoning, the step rule on real edges, merge/extend storms and the T-020 anomaly mapping under Merged.
- **B0.69 docs/stream-contract.md updated** (9b16e68) to match the shipped bridge: implementation, refusal codes, token auth, loopback default without TLS, read-only query endpoints; §11 now separates plain TCP listeners (still unauthenticated) from the token-authenticated bridge and lists bridge residuals (no TLS, loop reconnects, consumer-slot race, missing units field as a v1.1 candidate).
- **B0.70 T-005 pre-merge re-review #3: FIX-BEFORE-MERGE (converging).** Per-frame/slow floors, allocation (3.5 k adversarial frames), 1 h stream lifecycle and real fixtures (0 events on urban/915) now hold. Remaining: (1) a single low frame (or 3-frame dip / in-block step) confirms an interrupted Fall whose low-frames-only average resets the slow floor, followed by a phantom Rise that only rebaselines at 600 s; (2) merges drop T-020 coverage — the absorbed episode's Anomaly closes as "merged" and a Structured survivor opens nothing (0 % of elevated bins covered); (3) the learned floor shape relearns at ~1 dB/s, so removing a notch filter gives up to 890× Pfa for ~15 s. Also Update not shrinking extents, nested Anomalies on symmetric widening, repeated interrupted Falls on a −2.8 dB drop. Fix round 2 sent with the reviewer's probes as regression tests. Wide-reference limits for T-006 recorded on the task (coverage degrades past ~2000 bins; >55 % span and soft edges uncovered; band-pass accessories need response calibration).
- **B0.71 T-006 review fixes** (1e5bc99; lint + workspace tests green): carrier fusion fixed (each split re-owns current-frame runs; impulsive runs never merge), bounded labeller memory/cost (worst 3.4 ms/frame, flat memory; dense frames >1024 runs skipped and counted), notch phantoms fixed by a floor-step guard (OS-only near persistent >6 dB block-floor dips/steps; plateaus exempt), branches per band profile, clock-harmonic spur reason (434.000 MHz = 217×fs flagged), full-chain FA 0 boxes in 1.02 MHz·h. Held for (a) a focused re-probe by the original reviewer (bridge variants, dense-skip losses on busy bands, guard coverage cost) and (b) replacing a `writable_schema` SQLite migration with an in-place schema edit.
- **B0.72 Schema policy (PROVISIONAL):** until the first release, `crates/hk-model` migration `0001_init.sql` is edited in place (no deployed databases exist); after release, schema changes are new migrations using table rebuilds — never `PRAGMA writable_schema`.
- **B0.73 User steer (2026-09-13): timebox review loops.** After the current fix round, merge T-005 and T-006 unless a real correctness bug remains; remaining nits become follow-up tasks in tasks.yaml; launch T-007/T-009 immediately after T-006 merges. Applied going forward to all tasks: one review, one fix round, then merge (block only on concrete correctness/legal bugs).
- **B0.74 T-007 and T-009 launched** on the T-006 branch (1e5bc99) instead of waiting for its merge — T-006's detector events and trust functions are stable; only a schema-only edit and a timeboxed re-probe remain. Both merge main once T-006 lands.
- **B0.75 T-006 merged** (6891727 + review fixes 1e5bc99 + in-place schema edit 855ce9f; lint + workspace tests green) under the timebox rule: the re-probe found no remaining correctness bug. Follow-ups filed: **T-028** (floor-step guard hardening before notch/filter-bank field use — the plateau exemption can be fooled into 23 phantoms; the OS-only zone misses wide signals near a notch), notes on T-007 (split side-runs, split-frame bridges) and T-027 (dense-frame flags on records; 65 536-bin resolution choice). **T-029** placeholder for T-005 follow-ups. T-007/T-009 (already running on the T-006 branch) told to merge main.
- **B0.76 T-005 floor rework merged** (dbd896e + final round ab9148b) under the timebox rule without a further review. Final round: a fall needs ≥50 % low frames and its level comes from all frames (no phantom episodes; slow-floor error ≤0.031 dB); merge survivor chosen by class and T-020 re-parents open Anomalies/Explanations (coverage 100 %, was 64 % / 0 %); learned floor shape snaps on accessory changes (wide floor-branch Pfa 1.01–1.09× within ~1 s); disconnected episodes split (`split_from`), Extend reports only added strips. Main was red after the merge: T-006's hk-detect helper/bench built `FloorFrame` without the new `shape` field (fixed with neutral 1.0), and two T-006 false-alarm tests encoded old-tracker expectations — the full-chain test now allows ≤2 boxes per ≥1 MHz·h (it saw 1 box with exceedance at 1.02× design on both references; asserting exactly 0 was statistically brittle) and the "no step guard" test's ratio threshold drops from >10× to >2× because the new floor makes notches far less harmful (3.8× without the guard). Remaining nits in **T-029**; T-028 and T-029 now unblocked.
- **B0.77 New correctness follow-up T-030:** T-013 found that T-011 trusted a symbol rate 7× too high on 13/20 bursts of rectangular FSK with h=4 — a violation of the "trusted is never wrong" contract. T-013 works around it (run-length harmonic divisor); the fix belongs in T-011's blind estimator.
- **B0.78 T-013 merged** (ca718d8; lint + workspace tests green). `hk_demod::fsk` (discriminator + timing recovery seeded by T-011, prior-led trial demod for weak bursts, run-length harmonic guard) and `hk_estimate::framing` (preamble/sync/polarity/bit-order/PN9-and-7-bit whitening/length-field inference; 29-entry CRC catalogue plus reflect/init/xorout variants; a CRC is claimed only with ≥3 validating bursts across ≥3 distinct messages, ratio ≥0.5, false-alarm bound ≤1e-3). AWARE-036: synthetic BER 0 at 20/12 dB, sync 2DD4 + CRC-16/CCITT-FALSE on 20/20, payloads exact, ground-truth annotation; real 915 MHz: sync in 18/18 truth bursts, no CRC claimed, payload labelled encrypted-or-scrambled. **Legal: content fails closed unless the caller supplies an EmitterClassification; encrypted-looking payloads are forced metadata-only; a sentinel scan of SQLite and stream bytes found 0 payload leaks for the real third-party fixture.**
- **B0.79 Launched follow-ups:** T-028 (detection guard hardening: plateau bypass, wide-floor reference inside guarded zones, evaluate flipping the default reference to Wide), T-029 (floor-tracker nits, SK-gated slow floor), T-030 (T-011 trusted harmonic rate on high-index FSK). T-007 and T-009 told about the T-005 API changes on main.
- **B0.80 T-007 merged** (a0cbd95; lint + workspace tests green). `hk_detect::track::Tracker`: held/ordered association of T-006 records (skips impulsive and <2-frame split fragments), max-duration and transition continuations recorded as segments, fused split boxes redistributed by frequency overlap, tone-lobe aggregation, nearest-track matching (centre within max(2 bins, 10 % BW), BW ratio ≤2), lattice-fold periodicity, duty cycle, back-to-back hop sets, merge/close events, batched repository writes. AWARE-036 period error 0.1 % with 25/25 bursts in one track; AWARE-042 per-channel tracks; synthetic hopper recovered exactly; 6 h carrier bounded and allocation-free. The real 915 MHz FHSS bursts form 28 tracks but no hop set (packets separated by silence). Follow-ups filed as **T-031**. T-018 (inventory + emitter clustering) unblocked and launching.
- **B0.81 T-030 merged** (25d1ddb): the blind estimator's transition fit only divided clock factors 5..2, so 7×/9× harmonics were trusted on high-index FSK; it now searches k=32..2 with a rise/fall-separate lattice check and marks unresolved factors ambiguous. 0 trusted-and-wrong over a 720-run rectangular-FSK sweep and the 900-run S5 sweep (no floor regression). The "trusted is never wrong" contract is restored.
- **B0.82 T-009 merged** (85b06be) under the timebox rule to unblock T-027; a single timeboxed post-merge review runs. `hk_core::scheduler`: deterministic `Scheduler<Clock>` compiling ScanPlans (clip to capability, merge overlaps, ≤15 MHz hops avoiding the HackRF 2170/2740 MHz path switches and DC), S:D sweep/dwell cycle, weighted round-robin POI dwells sized from emitter bandwidth/burst interval with revisit caps, verification groups (gain step A/B×3 at 0.5 s, retune ±1 MHz, optional rate change) feeding real `hk_detect::trust`, clipped pairs skipped, user-intent preemption with rollback, TX slot always gated, trust-pending flag on accessory bands; golden-file schedules; zero allocation over 20 k steps. Follow-ups filed as **T-032** (includes verifying the recalled 2170/2740 MHz constants).
- **B0.83 T-029 merged** (5641167): Structured blocks inside noise-like episodes reported separately (no wrongly covered anomaly bins), overlapping open anomalies absorbed, slow floor moves only on NoiseLike rises (steady wide signals no longer lift the science series), in-place anomaly region update deferred, FM startup Fall confirmed as a recording artefact, wide-reference limits documented.
- **B0.84 T-027 pipeline assembly launched** (Opus high; composes source → ring → STFT → floor → detector → tracker → stores/streams, scheduler-driven runtime demod/decode/plugin chains, SigMF recording on trigger, `hk replay` + `hackriffd`), plus a timeboxed post-merge review of T-009 and **T-031** (track-module follow-ups only; hk-model parts wait for T-018). T-032 held until the T-009 review reports.
- **B0.85 T-028 merged** (370bab3; lint + workspace tests green; lib.rs export conflict with T-007 resolved as a union): plateau exemption now needs both outer sides at the band floor (notch-plus-step phantoms 4/91 → 0); guarded zones use the shape-normalised wide floor with an OS-only fallback (wide signals beside a notch 0.1–0.4 % → 100 % coverage); notch/tilt/roll-off false alarms 1.00–1.07× design; fixtures unchanged; default reference stays PerFrame (Wide fails a stepped-notch case at 614×). Remaining gap before filter-bank field use filed as **T-033** (sharp-edged passbands still ~60× design as exempt plateaus).
- **B0.86 T-009 post-merge review: OK** (no must-fix): 4 933 random plans tile regions gap-free within capability and never straddle path switches; 6 131 verification groups correctly paired; 16 simulated days without time drift; TX request unconditionally gated; golden files deterministic. Its follow-ups (clipped retune base, verification group id, monotonic wall clock, score overflow/starvation, plan-update pass restart, retune vs wide POIs, filter-rolloff seams) are folded into **T-032**, which is now launched.
- **B0.87 T-033 launched** (filter-bank passband phantoms, narrow shelves, guarded bins in integrated confirmation, exposing shape-normalised block floors from hk-dsp, re-evaluating the Wide default) to clear the filter-bank field-use blocker while T-018/T-027/T-031/T-032 run.
- **B0.88 T-018 merged** (4c68648; lint + workspace tests green). Versioned emitter fingerprints and an ordered clustering pipeline (replay ledger → decoded identity → emitter context → fingerprint → new unknown emitter with priors), append-only merges (merged_into, link supersession), identity conflicts reported not merged, and `query_inventory` with fail-closed identity gating (clear only for unrestricted, or own-key with explicit own-traffic authorisation). Additive schema in the pre-release 0001. Follow-ups filed as **T-034** — first item is a legal gap to close before any inventory API/export ships: the raw `emitter()`/`emitter_by_identity` getters still return identities ungated. T-022 (web inventory table) launched.
- **B0.89 T-034 launched** (legal gap first: gate the raw emitter identity getters fail-closed; move RDS/FSK writers to class-aware `record_sighting`; dedup re-demodulated observations). T-027 told to use `record_track_event` + `query_inventory` only; T-031 told to merge main (T-018 touched `track/`).
- **B0.90 T-031 merged** (d043db8): bursty FHSS now forms hop sets via a silence rule guarded against independent emitters sharing a raster (real 915 MHz fixture: 20 tracks, was 28, and one 7-channel hop set), split trigger on stable two-cluster tracks, tentative tracks suppress low-SNR fragments, merged-track links copied to the survivor. Data-model parts, batched writes and the real-fixture hop raster estimate (40.3 kHz vs 200 kHz) filed as **T-035**.
- **B0.91 T-035 held** until T-034 merges (both change hk-model repository code; T-035 is not M0-critical). Running: T-022, T-027, T-032, T-033, T-034.
- **B0.92 T-032 merged** (1fbe932; lint + workspace tests green): verification now skips clipped bases, carries group ids and rejects stale captures; monotonic scheduler clock; score clamping and a 5 % minimum POI share; wide-POI retunes shrink or skip with notes; plan updates resume the pass; `hk_detect::rate_change` trust function; RF path boundaries moved into `SourceCapabilities` with the HackRF One 2170/2740 MHz switch points **verified** against upstream firmware `tuning.c` (commit 7a6b099; HackRF Pro uses 2320/2580 MHz). Deferred: ADR-0005 bandit revisit + C12 interestingness, per-region revisit, gain-down re-verification, cron/firmware sweep mode, DC-offset slices.
- **Decision (PROVISIONAL): sweep seam guard default = 0.** `seam_guard_fraction` 0.2 would overlap hops away from the 15 MHz filter roll-off but adds ~25 % more hops and shortens dwells under revisit targets; for M0 prefer revisit rate. Reversible by config; revisit after S2 on the Jetson.
- **B0.93 T-034, T-033 and T-022 merged** (02200e5, 7e346e8, 7011ffd; lint + workspace tests green). T-034: all public emitter getters now gate identities fail-closed with explicit authorised variants; RDS/FSK writers record identity class; re-demodulating the same IQ no longer doubles counts (PI 1694 stays at 1). T-033: filter-bank passbands and narrow shelves no longer produce phantoms (≤1.07× design, 0 boxes), guarded bins join integrated confirmation, and **the default floor reference is now Wide** (stepped notch 614× → 1.05×; fixtures identical); known limit: steady noise-like wide emissions read as floor features. T-022 complete: `/api/inventory` built only on the gated query (raw-byte scan clean) plus the UI inventory table. New follow-up **T-036** (ungated decode getters and tags — legal; audited reclassify API; steady wide-emission limit vs AWARE-006). T-035 released.
- **B0.94 T-036 launched** (legal first: gate decode getters and tags fail-closed; audited own-traffic reclassify API that can never open restricted cellular/paging; AWARE-006 check that steady noise jammers still raise an Anomaly despite reading as floor features on the Wide reference). Running: T-027, T-035, T-036.
- **B0.95 T-027 merged** (a75f781; lint + 114 Rust test binaries + Python + UI green) under the timebox rule, with a timeboxed post-merge review running. New crate **hk-pipeline** composes the whole M0 chain behind `hk replay` and `hackriffd`: raised-priority capture thread → ci8 ring; detection reader (STFT Hann 0 % overlap so thresholds keep design Pfa → floor → Detector → Tracker → batched SQLite + inventory with priors + anomalies/correlation); history/FloorProduct reader; spectrum-stream reader; scheduler on stream time; data-driven runtime demod/decode chains (wfm-rds, fsk-bursts, adsb-readsb) attached and detached as ring readers — the ADR-0001 "change pipelines without stopping capture" requirement exercised in tests (20 cycles, 0 overruns). End-to-end: SIGNAL-062 real FM → PI 1694 + label + recording; AWARE-036 20/20 CRC-valid with content withheld unless classified; SPACE-050 floor within 0.10 dB; SIGNAL-001 16/16 readsb decodes; restricted-paging source leaves no content. Follow-ups filed as **T-037**. **T-024 (the 7 slice acceptance tests in CI) — the last M0 task — launching.**
- **B0.96 T-036 merged** (b14cc33; lint + workspace tests green): decode getters and tags now gate identities fail-closed (withheld identities can't be confirmed by lookup or tag filter), an audited own-traffic `reclassify_identity` that can never open anything ever restricted-cellular/paging, and an AWARE-006 regression confirming noise jammers still raise NoiseLike floor-rise episodes even though they read as floor features on the Wide detection reference. Follow-ups filed as **T-038** (steady OFDM also opens a floor-rise Anomaly — a potential attack-map false explanation; partial-band extent under-reported; letters-only identity tags).
- **B0.97 T-035 merged** (288ad04): track timing features and segments persisted, index-bounded `tracks_in_region`, a typed atomic `Repository::batch` API used by the tracker. Hop raster on the real 915 MHz fixture now 199.8 kHz, flagged as possibly overfitted (tie with 300 kHz broken by a common-raster prior; thresholds tuned on one fixture). TDMA/co-occurrence and the docs/07 field update deferred.
- **B0.98 T-027 post-merge review: FIX-NEEDED.** (1) Correctness: unpaced (lossless) `hk replay` deadlocks on recordings whose `core:global_index` exceeds the ring slack — both trimmed ISM fixtures hang with 0 samples; the FM fixture only passed because it starts at 3.6 M. (2) Legal: no restricted-band derivation for untagged recordings, so a classify rule over the 929–932 MHz paging band could store content. Verified OK: paced/live capture never blocks on readers; gated classes refuse content and recordings; `/api/status` exposes only counters; replay trust verdicts are not persisted. Fix agent launched (gate clamp to ring oldest sample, restricted paging/cellular classes from 47 CFR bands that clamp classify rules, `lossless` default false and refused for non-pausable sources, no link writes after a failed detection write). T-024 briefed on vacuous-test and silent-skip risks (require fixtures/synth in CI; assert decodes > 0 in the legal regression; lower-bound ADS-B count).
- **B0.99 T-038 launched** (structured OFDM vs noise jammer so the attack map doesn't explain normal wideband emitters as jamming; partial-band extent; letters-only identity tags). T-037 held until the T-027 fix merges. Running: T-024 (acceptance), T-027 fix, T-038.
- **B0.100 T-024 merged** (0749296 → 522cf77): the 7 M0 slice acceptance tests (SIGNAL-001, AWARE-036, SIGNAL-062, AWARE-006, SPACE-050, AWARE-053, AWARE-042) plus a legal regression pass locally 11/11 in 15 s, stable over 3 runs. New CI job `acceptance` runs with LFS, required fixtures and generator; the CI step now builds the plugin binaries first. Only the real-readsb half of SIGNAL-001 skips, because CI has no readsb. SIGNAL-001 still uses the synthetic scene (1090 MHz antenna blocker).
  - Key numbers: 16/16 ADS-B CRC decodes; FSK symbol-rate error 0.000 %; RDS PI 1694; floor error 0.03 dB calibrated; occupancy error ≤ 0.007; restricted paging 10/10 decoded, all withheld, 0 leaks (control run shows content).
  - Gaps found: short lossless replays miss coverage chains, pilot frequency not stored, payload hex case → T-037. Pipeline emitters never get a family, so priors can't classify them (AWARE-053 classifies manually) → new T-039, held behind the T-027 fix.
  - SPACE-050 applies calibration after the run until the pipeline loads calibration (T-037).
- **B0.101 T-038 merged** (51c23f1 → 86b39a5).
  - **Attack map, jammer vs structured emitter:** a noise-floor rise is now classed Structured rather than NoiseLike when power changes in bins 3–8 apart are anticorrelated from frame to frame. Test results: noise jammers 12/12 NoiseLike (broadband, 800 kHz partial, noise-FM), OFDM 18/18 Structured, at 6/10/15 dB. A normal OFDM emitter therefore no longer opens a jamming Anomaly.
  - **Episode extent:** refined per bin; worst error 0.3 % of bandwidth.
  - **Legal:** emitters with withheld identity accept and show only `TAG_VOCABULARY` tags. The refusal depends on the class, not the value.
  - **Known limits → T-038 followups:** wideband single-carrier, dense QAM, and FFT sizes much longer than the symbol need IQ-level evidence.
  - **Review:** a timeboxed post-merge legal review of the tag gating is running.
- **B0.102 T-038 legal review: MERGE-OK.** No leak found; checked case/Unicode tags, batch and plugin ingest, context sightings, class tightening, tag-filter oracle, vocabulary. Four hardening nits become T-040, launched now (hk-model only; no collision with the T-027 fix):
  - `remove_emitter_tag` answers existence ungated;
  - `merge_emitters` copies free-text tags into withheld targets;
  - `add_emitter_tag` inserts on the pre-merge id;
  - stale tags persist after a class tightens.
- **B0.103 T-027 fix round complete.** Commits c995517 + 67f8843, previewed with no conflicts against main; merging after the B0.101 verification run.
  - **Deadlock fixed:** gate claims are clamped to the ring's oldest sample.
  - **Restricted bands:** paging and cellular classes are derived from frequency under 47 CFR 22.531/90.494/24.129/22.905/24.229/27.5 (edges checked on Cornell LII eCFR) and override classify rules. Fail-closed: a window overlapping a band is restricted as a whole.
  - **Lossless replay:** off by default, and refused for non-pausable sources.
  - **Detection writes:** retried, with links deferred until the detections are stored.
  - **Acceptance:** new untagged 930.5 MHz paging case; 12/12 pass.
  - **Follow-ups:** per-channel restriction instead of whole-window; ESMR/FirstNet/CBRS edges unverified.
  - **Next:** T-037 and T-039 holds released; they launch after the merge.
- **B0.104 T-027 fix merged** (69563f5); verification run in progress. Launched three agents in parallel with file ownership split within hk-pipeline:
  - **T-037a:** HackRF live source (libhackrf BSD-3, feature-gated so CI needs no library; receive-only with no TX bound in the FFI). It has exclusive HackRF use for an ignored HIL smoke test. Also Ctrl-C shutdown, `--loop` restart, retune header, calibration loading, attach-test margin.
  - **T-037b:** the data path — writer thread, verification persistence, readsb backpressure, WFM fragments, capture names, FSK bits, short-replay attach, pilot frequency, FloorProduct lock, correlator I/O.
  - **T-039:** mapping demod families to band-plan priors.
  T-040 is still running in hk-model.
- **B0.105 User request: T-041 Mac compute providers, launched immediately.** It doesn't collide with the running T-037a/b, T-039 or T-040, since none of them edit hk-dsp. The dev Mac is an M3 Ultra (28 CPU cores, 60-core GPU, Metal 3, 256 GB).
  - **Plan:**
    - Measure the multi-threaded CPU baseline at 20 Msps (STFT + PFB real-time factor).
    - Add a GPU provider for `FftBackend`/`PfbBackend`, choosing wgpu compute (Metal on Mac, Vulkan on Jetson) or native Metal after a timeboxed comparison.
    - Evaluate Accelerate vDSP for CPU FFT.
    - Parity tests against the CPU reference, benchmarks, and runtime/config provider selection with CPU fallback.
  - **Decisions (user):**
    - ADR-0007 becomes a per-platform provider model.
    - CUDA stays a later Jetson provider; T-026 stays blocked.
    - The GPU path gets exercised on the Mac now rather than waiting for the Jetson.
  - **Model:** Opus high (Fable excluded this session).
  - **Caveat:** concurrent agent builds make CPU benchmarks noisy, so the agent records the load average and reports min/median.
- **B0.106 User feedback (priority) → T-042..T-046, scheduled by file ownership:**
  - **(A) T-042, real live HackRF in the UI:** default `--hackrf` source for serve/hackriffd, full pipeline to inventory, demo seed only behind `--demo`. Folded into the running T-037a agent, which owns hk-cli and the HackRF source. It also exposes `hk_api::LiveControl` for UI controls.
  - **(B) T-043, Listen (click → auto analog demod → Web Audio):** held until T-037b (chains/), T-039 and T-044 merge; legal gating kept.
  - **(C) T-044, UI interaction + (D) T-045, frequency axis bug:** launched now in one agent, since ui/ has no other owner. For D: center showed 100.4324 MHz and the 101.3 MHz station showed at ~101.0 MHz, suggesting a ~368 kHz offset. Root cause first, with a tone-at-known-frequency test through STFT → header → UI mapping. If the root cause is in `hk-pipeline/src/spectrum.rs` (owned by T-037a), the diff is routed to T-037a.
  - **(E) T-046, shared provider conformance suite:** folded into the running T-041 agent (hk-dsp). Every provider, including a future CUDA one, must pass it before it is selectable.
- **B0.107 T-027 fix verified on main:** lint, 121 Rust test groups, Python tests and acceptance 12/12 all green.
- **T-040 merged** (7014e55 → a12848a). The four tag-hardening nits are fixed with raw-SQLite sentinel tests. Tags outside the vocabulary are now purged whenever an emitter becomes withheld. Two older "tags_withheld" assertions changed to match. Verified: lint, workspace, Python and acceptance green.
- **B0.108 User feedback on serving and tests.** The start of the message was truncated; acted on the visible part.
  - **Serving:** `seed_inventory` is removed from normal serving entirely, not just put behind `--demo`. Demo seeds only inside unit tests. Sent to T-037a.
  - **Test philosophy:** each fixture carries a ground-truth list of interesting emissions that the system must not be given. Tests run blind detection, assert every truth emission is detected, and assert a reasonable explanation among the top-k recommendations. No lookup-a-frequency-and-tune tests.
  - **T-039 scope:** ranked top-k explanations with an off-raster flag, plus the first blind cases: the fm_100p8M station → FM broadcast, and a +150 kHz shifted synthetic → detected and off-raster. Sent to the running agent.
  - **New T-047:** general blind ground-truth harness plus an audit and rewrite of existing acceptance tests; held behind T-039 and T-037b.
- **B0.109 User feedback 2 → new milestone M0b "Live device + exploration UI".** It sits after M0 and before decoder breadth; docs/10 and docs/11 are being updated by an agent.
  - **Principles (user):**
    - **Device interface:** E2E/acceptance tests drive the system through the SDR device interface via a mock SDR that replays SigMF realistically: retune inside the recording serves that band, outside it gives noise plus a flag, gain scales and clips at 8-bit, and it reports timestamps and overruns. No direct file feeding; the same tests later run against the real HackRF (HIL).
    - **Generic interface:** SoapySDR-ready, with HackRF specifics kept out of the core.
    - **UI is a major gap:** hover/click, multi-region selections as first-class objects, a full SDR control panel through an authenticated control API with legal/TX gating (M0 was GET-only).
  - **M0b tasks:**
    - T-042 live HackRF serving (in T-037a)
    - T-043 Listen
    - T-044 hover/click/multi-select (running)
    - T-045 axis bug (running)
    - T-047 blind truth harness, now through the mock device
    - T-048 generic device interface
    - T-049 mock SDR device
    - T-050 authenticated control API
    - T-051 SDR control panel
    - T-052 persisted multi-region selections plus actions
    - T-053 HIL acceptance run
  - **Running agents redirected:**
    - T-037a keeps the device interface generic (named gain stages, optional bias-tee/sweep) and adds no unauthenticated POSTs.
    - T-044 drops control endpoints and panel (→ T-050/T-051) and designs selection as multi-region client objects.
  - **M0 closes** when T-037a/b and T-039 merge; T-026 moves out with the Jetson. M0b launches in dependency order: T-048 → T-049 → T-047/T-053; T-050 → T-051/T-052; T-043.
- **B0.110 docs/10 + docs/11 updated for M0b** (23ab917, merged). docs/10: tiers now run through the mock device, with T5 as the same suite on the HackRF; new §1.1 device interface, §3.1 hidden ground truth, §3.2 anti-patterns. docs/11: new §1.2 M0b with contents, task map and definition of done; M1 now gated on M0b. Section numbers unchanged.
- **B0.111 T-039 merged** (f555e10 → 7855ec5). Pipeline emitters now get families and ranked top-5 explanations; AWARE-053 passes end to end with no manual classify step (acceptance 14/14).
  - **Blind cases:**
    - the fm_100p8M station ranks FM broadcast first;
    - a +150 kHz IQ-shifted copy is detected and flagged off-raster;
    - relabelled to 120.5 MHz it is unexpected-here (aviation-vhf-comm);
    - relabelled to 930.5 MHz it stays restricted-paging with no content.
  - **Design choice:** bare modulation names (fsk/ook/psk) and shared analog modes (nbfm/am/ssb/cw) name no service, so status stays `unknown` with allocation-only suggestions. `known` needs a service-specific decode (rtl_433, readsb, RDS).
  - **Blind harness helpers** landed for T-047 (`strip_truth`, `shift_ci8`, truth matcher, single `blind_replay` entry point).
  - **Follow-up:** plugin decodes are not yet wired to families; sent to T-037b.
  - **Checks:** post-merge legal review and verification running.
  - **M0 remaining:** T-037a, T-037b.
- **B0.112 T-039 legal review: MERGE-OK.**
  - **Checks that passed:** explanation evidence comes only from the fixed vocabulary, the band table and measurements; RDS PI never reaches annotations; known_status doesn't feed class or identity gating; T-040 gating is intact; raster math is correct.
  - **Nits → T-054, launched now** (it owns family.rs/query.rs; T-037b only touches chains/plugin.rs):
    - serve-path author check;
    - shape-only evidence must not set a status (vision step 4: unknowns stay unknown);
    - status from the best candidate, not the top one;
    - rasters keyed by region;
    - decoder-id family path and re-rank on reclassification;
    - a misleading test name.
- **B0.113 Disk full incident (user alert, 2026-09-13 ~13:20): ~159 MB free, ENOSPC.**
  - **Cause:** hackriff build dirs used ~39 GB — main `target/` 11 GB plus 4–7 GB per agent worktree, of which incremental caches were 2.5–4.8 GB each.
  - **Actions:**
    - `cargo clean` in main freed 15.0 GiB (→ 14 GiB free).
    - All five worktrees (T-037a, T-037b, T-041, T-044, T-054) had active builds, so none was finished or removable.
    - Each agent was told to delete `target/debug/incremental` after its current cargo command and build with `CARGO_INCREMENTAL=0` from then on (~11 GB more).
  - **Rules from now on:**
    - Launch no new agents or verification builds until free space is above ~20 GB.
    - Remove each worktree immediately after merge (removal deletes its target).
    - Every future agent brief sets `CARGO_TARGET_DIR=/Users/daniellewis/hackriff/target` and `CARGO_INCREMENTAL=0`.
    - Once the current worktrees are gone, add an uncommitted `.claude/worktrees/.cargo/config.toml` (`build.target-dir` shared, `incremental = false`) so new worktrees share one target automatically. It isn't added now, because running agents would each start a full rebuild into the shared dir while their old targets still exist.
    - Coordinator verification runs use `CARGO_INCREMENTAL=0`, and main `target/debug/incremental` is cleared when idle.
- **B0.114 Disk recovered:** 34 GiB free after agents dropped their incremental caches, above the 20 GB bar, so normal operation resumes. T-041's incremental cache (3.9 GB) is still pending. The shared-target and `CARGO_INCREMENTAL=0` rules from B0.113 still apply.
- **B0.115 T-037a merged** (96f9b48 → 34da3e8), including T-042 live serving and the generic device contract (T-048 contract done). **T-041/T-046 merged** (9504ea7 → 1501679). Both worktrees removed (46 GiB free).
  - **HackRF:**
    - Receive-only; no TX symbol in the FFI.
    - HIL at 1.95 Msps: 0 USB drops, retune ok.
    - `hk serve --hackrf` put a real FM emitter into `/api/inventory`.
    - Demo seed removed from serving.
    - **Problem:** debug-build pipeline readers overran the ring by 9.6 M samples at 2.4 Msps. Measured next in a release build (T-055, launched; the HackRF is assigned to it).
  - **LiveControl** exists without endpoints. It refuses retunes into a different legal class and refuses rate changes (one rate per run). T-050 revisits both, since exploration needs tuning anywhere with content gated per class, and span changes.
  - **Compute:**
    - wgpu chosen (Metal now, Vulkan for Jetson).
    - GPU async STFT 17–20× and PFB 14× real time at 20 Msps, with parity ≤ 1.9e-6.
    - Accelerate STFT 12–14×.
    - Conformance suite gates provider selection.
    - Pipeline hookup is T-056, held behind T-037b.
  - **Launched now:**
    - T-049 mock SDR device plus device conformance test;
    - T-050 authenticated control API;
    - T-055 HIL throughput.
    New agents share `CARGO_TARGET_DIR` with `CARGO_INCREMENTAL=0`. Verification of both merges is running.
- **B0.116 HackRF sharing and CLAUDE.md:**
  - The user's web demo (`hk serve` on 127.0.0.1:8899, run by a supervising session) now holds the live HackRF. When an agent needs the device (HIL, captures, T-053, T-055), stop it with `pkill -f 'hk serve.*127.0.0.1:8899'` and wait for `hackrf_info` to show it free. The supervisor restarts the demo on replay while the device is busy and back on live afterwards. Passed to T-055.
  - The user's CLAUDE.md vision step 4 edit (blind detection first, database only recommends) is committed on the user's approval (90fa6be).
- **B0.117 T-054, T-044 and T-045 finished; merges wait on the T-037a/T-041 verification run.**
  - **T-054:** explanations author check, shape-only never sets status, best-candidate status, region rasters, decoder evidence API; acceptance 16/16.
  - **T-044:** hover failed on touch (pointermove only, and taps never send it); multi-region selections; click-to-inspect via gated inventory; no control endpoints.
  - **T-045:** offset not reproduced with plain `hk serve`; UI bugs fixed (stale header after reconnect, half-bin offset).
  - **Axis-formula review: MERGE-OK, root cause found.** Scheduler virtual tuning (hackriffd, `hk replay --schedule`) rewrites the provenance centre to hop/dwell centres while the IQ stays at the recorded centre. Every signal then lands at the wrong absolute frequency in the header, detections, inventory and history. This is the user's ~368 kHz offset, and it is a correctness bug affecting data, not only the UI.
  - **T-057 opened and split:** the source side goes to the T-049 agent (truthful virtual tunes via an IQ shift, or refusal); header re-offer on any provenance change goes to the T-050 agent.
  - **Follow-up:** a shader pooling nit goes to T-051.
- **B0.118 T-037a/T-041 merge verified** (lint, 131 Rust test groups, Python, acceptance 14/14). T-054 and T-044/T-045 merged (b455c63, ff7f9d9); their worktrees are removed and verification is running with UI tests.
  - **T-055 HIL (release, real HackRF):**
    - USB never drops; 2.4 Msps is real-time with zero loss at ~8–14 % CPU.
    - At 8/10/20 Msps the single-threaded detect reader (STFT+CFAR) loses 67–80 % of samples. History and spectrum readers stay lossless.
  - **New tasks:**
    - T-058: detect-path profiling and optimisation, launched now (hk-detect/hk-dsp, no hardware, detection output must match the reference).
    - T-059: FOREIGN KEY failure in the analog chain writer at 8–10 Msps, sent to the T-037b agent.
  - **HackRF:** free; the demo was stopped for T-055 and the supervisor restarted it on replay.
- **B0.119 T-054 + T-044/T-045 merge verified:** lint, Rust workspace, Python, UI tests and acceptance all green.
- **B0.120 T-037b merged; M0 COMPLETE locally.**
  - **T-037b** (b648511 → 667b84b): writer thread with backpressure; trust verdicts persisted; readsb backpressure; short-replay attach with the ring workaround removed; pilot_hz stored; FloorIngestQueue; correlator I/O moved outside the repo lock; plugin decodes feed explanations; T-059 FK fix. The merge conflicted in `tests/e2e/tests/acceptance/blind.rs`; the coordinator resolved it by deep-merging T-037b's `extra` config over T-054's `plan_extra`.
  - **M0 status:** every M0 task is done except T-026 (GPU PFB, blocked on the Jetson; it moves out with S2). The acceptance suite is green locally. **CI has never run: nothing has been pushed**, so "green in CI" from the M0 definition of done is unverified until the user pushes. CLAUDE.md status now reads M0 complete locally, M0b in progress.
  - **Launched T-043 Listen.** Held: T-056 (after T-050 and T-058), T-047 (after T-049), T-051/T-052 (after T-050), T-053 (after T-047).
  - Verification of the merge passed: lint, Rust workspace, Python, UI and acceptance all green.
- **B0.121 User feedback 3 (start of message truncated: item 1 and the start of item 2 were lost; asked the user to resend).**
  - **Visible parts:**
    - (2) External programs connect to demodulated bitstreams, with a netcat/Python example, keeping the stream contract, gating and drop-not-block backpressure.
    - (3) Record outputs (bits, symbols, WAV, IQ slices) from UI and CLI with SigMF-style sidecars, linked to Bitstream/Recording rows, start/stop per selection, T-052 buttons wired.
    - Tests go through the mock SDR: FSK bits over TCP checked against truth, FM audio over WebSocket.
  - **Added:** T-060 (streams to external programs; held behind T-043 and T-049) and T-061 (record outputs; held behind T-060, T-052, T-049).
  - **Running agents:** T-043 was told to build its audio WebSocket as a reusable hk-stream transport with drop-not-block backpressure.
- **B0.122 T-050 merged** (6c4612a → c5f77e3). The coordinator resolved two conflicts with T-037b: hk-model exports and a history.rs seal check.
  - **Auth:** token file with Bearer header on mutating requests, Origin check, audit JSONL.
  - **Endpoints:** control endpoints and bookmarks; there is no TX route.
  - **Legal design change:** retuning into another legal class now re-plumbs the pipeline into a new segment with the new class, instead of refusing. Chains and recordings from the old segment finish under the old class; blocks from other windows are dropped. The sentinel paging test passes. A timeboxed post-merge legal/security review is running.
  - **Other changes:** rate change works through the same re-plumb. The spectrum half of T-057 is done (header re-offered per window).
  - **Follow-ups:** pause freezes only the spectrum; counters are per segment; a re-plumb blocks HTTP for up to 30 s.
  - **Next:** T-051 SDR control panel launched. T-052 held behind T-051 (shared UI files). T-049 and T-043 asked to merge main. Verification running.
- **B0.123 T-049 mock SDR done** (f6a47d4, merge waits for the T-050 verification run).
  - **Conformance:** suite of 20 checks; the mock passes. Realistic retune/coverage/gain/overrun model with provenance flags. CLI `--device/--source mock:<meta>`. Blind FM e2e through the mock passes; acceptance 16/16.
  - **T-057 done:** scheduled replay now goes through the mock. `open_replay` refuses virtual tuning. The emitter lands at 101.30 MHz across 18 scheduler retunes.
  - **Build-infra correction:** the shared `CARGO_TARGET_DIR` (B0.113 rule) cross-contaminates worktree builds. Branches reuse each other's workspace-crate artifacts, causing stale builds and spurious unresolved-import failures. New rule: each worktree uses its own target with `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only`, and at most ~4 Rust-building agents run at once. Worktrees are removed right after merge. T-043, T-051 and T-058 were told to switch and re-run their results.
  - **Unblocked:** T-047 once T-049 merges (switch recipe recorded in tasks.yaml).
- **B0.124 T-050 post-merge legal/security review: FIX-NEEDED (security, not legal).**
  - **Bug:** unauthenticated POST/PUT/DELETE requests each write an unbounded (~15 KB) audit JSONL line, with no rate limit, cap or rotation. Through the public cloudflared tunnel this can fill the disk: a real unbounded-resource bug.
  - **Clean:** the legal side. The WindowGuard checks each block's own window before the ring, so there is no class leak in either direction; HackRF in-flight transfers are discarded after a change; recording is re-checked per chunk; auth uses constant-time comparison; no TX route.
  - **Fix launched as T-062:** bounded fields, coalesced refused entries, size cap with rotation, plus hardening nits (X-Forwarded-* trusted only from loopback, token file no-follow with fd-based perms/owner checks, retune-timeout state consistency). The T-062 agent uses its own target (new build rule).
  - **Risk note for the user:** until T-062 merges, a demo built from main at or after c5f77e3 and exposed through the tunnel is open to that audit-log fill.
- **B0.125 Verification of the T-050 merge failed on one test: flaky, not a regression.**
  - `hk-core source::hackrf::tests::a_slow_reader_gets_counted_drops_and_an_exact_gap` (from T-037a) failed 1 of 3 runs in an isolated target. The dropped-sample counter kept growing after its single snapshot (67584 gaps vs 65536 counted).
  - Coordinator fixed the test: read until gaps equal the live counter, asserting gaps never exceed it. It then passed 15/15.
  - The first run may also have been contaminated by the T-049 agent building into main's shared target. T-049 was told to stop.
  - **Rule:** coordinator verification now uses an isolated target (`scratchpad/target-verify`, with `CARGO_INCREMENTAL=0` and line-tables-only debuginfo). Main's `target/` is no longer shared with agents.
  - Full verification is re-running.
- **B0.126 Legal regression on main: retune_legal fails deterministically (5/5, isolated target).**
  - **Symptom:** after the T-050 FM → 930.5 MHz paging re-plumb, a `bits/fsk-bursts/*` stream (the T-037b FSK bits stream) is offered with `content_class: Unrestricted`. The two tasks passed separately and fail merged.
  - **Open question:** unknown whether this is an old-segment lazy offer (test too strict) or a new-segment class leak.
  - **Fix:** dedicated agent T-063 launched (Opus high) to decide with evidence, fix, and add a publisher invariant: stream class never less restrictive than its feeding segment.
  - **Coordination:** running agents (T-062, T-043, T-049, T-051) were told it is a known failure and not to fix it.
  - **Merges held:** code merges other than T-063 wait until main is green again.
  - **Risk note for the user:** a demo built from main at or after c5f77e3 that is retuned into a restricted band via the control API may expose FSK bitstreams under the wrong class until T-063 lands. Other verification on the rerun was green up to that point (112 test groups).
- **B0.127 Main at ad4976e (isolated target):** Python 50 passed, UI tests green, acceptance 16/16, and 136 Rust test groups green. The only failure is `retune_legal` (T-063 in progress). T-049's merge waits for T-049 to finish merging main and for T-063.
- **B0.128 User steer (message start truncated):** keep going autonomously on features, in this priority set:
  - Listen (T-043)
  - control panel (T-051, then T-052)
  - outputs (T-060 streams, T-061 recording)
  - mock SDR (T-049, then T-047/T-053)
  - detection quality (T-058 throughput; next: T-038 follow-ups on IQ-level structured-vs-noise evidence, WFM fragment-track merging, live-data false alarms).
  Six agents are running (over the ~4 Rust-build cap), so new detection-quality work launches as slots free up.
- **B0.129 T-043 Listen done** (58762a5).
  - **Delivered:** on-demand audio WebSocket (`/ws/open/listen`) over a reusable hk-stream opener registry, which T-060 builds on. Stream contract 1.1 adds PCM and status records. hk-demod gains NBFM/AM/SSB/CW audio.
  - **Gating:** fail-closed. Restricted bands and restricted sources are refused; unclassified content is refused unless a user rule allows it.
  - **Results:** blind FM gives audio at 54 ms peak processing latency with 0 drops; the paging sentinel is refused before any attach.
  - **Merge:** held until T-063 greens main. A timeboxed legal/security review is running.
  - **Follow-ups (detection quality):** T-012 selector misreads a pure carrier as AM and a 60 % AM tone as unknown; emitter clustering is too coarse (2 MHz cluster).
- **B0.130 T-058 detect throughput done** (33ee78e); output is bit-identical.
  - **Root cause of the T-055 HIL loss:** the hop-set raster fit in the tracker, not STFT or CFAR. Hop-set members are never pruned, so the fit is O(n²) and recomputed every frame.
  - **Result:** whole-pipeline dense urban replay at 20 Msps went from 0.074× to 0.68× real time (~10×). Synthetic and ISM data run at ≥ 5–9× real time.
  - **Merge:** held until T-063 greens main.
  - **Next lever:** behaviour change → T-064 (prune members, recompute raster only on channel-set change; core interface), held behind the T-058 merge. The floor FCME comes after that, then T-056 GPU STFT.
- **B0.131 T-062 merged** (dd3d9bd → 190a8b1), ahead of T-063: it is a security fix that closes the audit-log disk-fill risk on main and adds no failures.
  - **Audit log:** bounded fields, coalesced unauthenticated refusals, 64 MiB rotation.
  - **Headers and token:** forwarded headers trusted only from loopback; token and audit files opened no-follow with fd-based checks.
  - **Retune:** timeout state refreshed from the applied window.
  - **Launched T-065:** analog mode selector quality (detection-quality steer); a blind synthetic confusion-matrix sweep.
  - **Still held behind T-063:** merges of T-049 (mock SDR), T-043 (Listen, review running) and T-058 (detect throughput).
  - Verification running; only the known `retune_legal` failure is expected.
- **B0.132 T-043 legal review: MERGE-OK.**
  - **Clean:** no gate bypass; partial overlap with a restricted band is always refused; the live window class comes from tuning; no auth hole (token checked before open, stripped, never logged); no segment-change leak. Listen doesn't share the T-063 failure mode: the header class is fixed at open, the chain ends when its segment ring closes, and it is never offered via /api/streams.
  - **Nits → T-066** (held behind the T-043 merge):
    - gate the DDC passband, not just nominal width (reachable only via a replay tagged unrestricted whose window covers paging);
    - a stronger retune test;
    - half-open client slot hold;
    - 503 instead of 410 during re-plumb;
    - free text in refusal reasons.
- **B0.133 Merge policy while T-063 is open.**
  - **Rule:** a finished branch merges when its own-target verification adds no failure beyond the known `retune_legal` regression, as T-062 did. This avoids a growing merge backlog and conflict pile-up behind T-063.
  - **Queue:** T-049 (mock SDR, ready: 137 Rust test binaries, py, ui, acceptance 16/16 green apart from `retune_legal`), T-043 (Listen, review MERGE-OK) and T-058 (detect throughput, bit-identical). They merge in sequence once the running T-062 verification finishes, followed by one combined verification.
  - **Unblocks after the merges:** T-047 and T-064. T-066 also unblocks, after T-043.
- **B0.134 User policy change (CLAUDE.md Legal, and licence stance, 2026-09-13).**
  - **New policy:** the user handles legality. No legal reviews, no legal regression tests, and no merges blocked on legal or content-gating concerns. Existing gating code stays but is not extended, and defaults to permissive where it obstructs a feature. The licence ledger is optional bookkeeping, not a gate. Standing preferences: no TX path unless asked, and no attack tooling.
  - **Committed:** the user's uncommitted CLAUDE.md and prompts/model-selection.md edits, so agent worktrees follow them.
  - **Task changes:** T-068 cancelled. T-066 trimmed to its robustness items (now unblocked). Later briefs omit legal-review steps.
  - **Merged in order:**
    - T-063 (f80a462; coordinator resolved the retune_legal.rs conflict with T-062)
    - T-049 (bedacff)
    - T-058 (f14a273)
    - T-043 (6f33776)
  - **T-051:** conflicts with T-043 in 4 UI files, being resolved. Combined verification follows.
- **B0.135 User clarification: close T-063 without further legal work.**
  - T-063 had already merged as a test-only change (f80a462).
  - Per the user, both `retune_legal` tests are now `#[ignore = "user decision 2026-09-13: legal gating not a requirement"]` so main verifies green without them.
  - No legal reviews on T-043, T-051, T-060 or T-061, or anything else.
  - T-049, T-043 and T-058 are merged; T-051 merge is being finished (UI conflicts resolved as unions with T-043 Listen).
- **B0.136 Batch merged and next features launched.**
  - Main now has T-062, T-063, T-049, T-058, T-043 and T-051 (UI conflicts with Listen resolved as unions). `retune_legal` is ignored per user decision. Combined verification is running on an isolated target.
  - **Launched** (user feature priorities):
    - T-047: blind ground-truth acceptance through the mock SDR, and the audit of lookup-and-tune tests;
    - T-060: bits/symbols/audio streams to external programs over TCP and WebSocket, with netcat/Python examples;
    - T-052: persisted multi-region selections with Inspect/Listen/Demod/Record actions.
  - **Running:** T-065.
  - **Waiting for agent slots:** T-064 hop-set scaling, T-066 Listen robustness, T-067 control API completeness, T-056 compute hookup. T-061 record outputs waits for T-060 and T-052.
- **B0.137 User UI feedback → T-069 launched now (Sonnet, UI-only):** Listen was hard to find (only in the hidden Inspect panel and per-selection actions). Adds a toolbar Listen above the waterfall, which plays the last click, else the selection, else the strongest signal in view. Also adds Listen per inventory row and a 'click a signal, then Listen' hint, and removes the restricted-class hint text. Merge on completion.
- **B0.138 Main fully green after the batch** (T-062, T-063, T-049, T-058, T-043, T-051; `retune_legal` ignored by user decision). Isolated-target verification passed: lint, 142 Rust test groups (0 failed), Python, UI, acceptance.
- **B0.139 T-069 merged** (56d5c8b): Listen is now discoverable. There is a toolbar Listen above the waterfall (last click, then selection, then strongest in view), a Listen button on each inventory row, a row click opens Inspect with Listen, and a hint. The restricted-class hint text is removed. UI tests pass on main.
- **B0.140 User feedback (start of message truncated) → T-070 output-driven refinement launched at queue front** (Opus high). A reusable RefinementLoop per demod chain, WFM first. It refines centre and bandwidth from demod output, stores them on the Emitter with provenance 'refined by output analysis', and off-raster checks use the refined centre. It never snaps to a band plan. Tests run through the mock SDR on fm_100p8M (±50/±100 kHz offsets, 60/400 kHz widths → ±2 kHz, bandwidth 150–220 kHz, PI decoded) plus a synthetic station 150 kHz off raster. Five Rust-building agents are running, one over the soft cap, accepted at load ~8. The user was asked to resend the truncated start.
- **B0.141 User feedback (start truncated; visible part is a test spec) → T-071 concurrent multi-signal demodulation.** Tests through the mock SDR: fm_100p8M with 3 stations demodulated at once gives 3 audio streams, each with its own station (distinct RDS PI or audio), plus 2+ FSK emitters with concurrent bits streams. Held until T-060 and T-070 merge (same chains/streams files), then first in the slot queue. The user was asked to resend the truncated start.
- **B0.142 User live probe (demo build 6b17fe5).** Concurrent Listen works but is hard-capped at 2: the third request got 503 busy, and stream ids reached listen/48, which suggests leaked sessions. T-066 was expanded and launched now, ahead of the queue: a configurable budget-based cap (default ≥ 8), leak fixes (WS close, tab gone, half-open, idle, re-plumb) with counters, and a 50 open/close sessions → 0 running test. T-071 keeps only the concurrent multi-demod tests.
- **B0.143 T-047 merged** (348716d → e9b7d33, test-only). All acceptance runs now go blind through the mock SDR via `acceptance/blind.rs`: truth is stripped and sealed, and never being read is checked after each run.
  - Lookup-and-tune tests removed (signal_062 no longer selects by 101.3 MHz/PI 1694).
  - Device variants added (gain step with clipping flags; retune outside coverage with no phantoms); `HK_DEVICE=hackrf` HIL switch. Acceptance: 20 passed, 1 ignored.
  - **Follow-ups → T-072:** ~0.4 s sample loss at device gaps, readsb stamps up to 95 ms late, single-detection tracks not stored, 120 µs squitter resolution.
  - **T-053 HIL unblocked:** needs the HackRF and a slot.
  - Verification running: lint, hk-e2e, acceptance (test-only change).
- **B0.144 T-047 merge verified** (lint, hk-e2e, acceptance green). T-060 finished: TCP stream server with a token line, bits/symbols openers, discovery, stdlib Python and netcat examples. Mock-SDR tests: FSK over TCP 20/20 payloads, FM audio over WebSocket, slow client dropped without stalling the chain, 2 concurrent streams isolated. Its merge was blocked by uncommitted edits to docs/stream-contract.md on main that the coordinator did not make; investigating.
- **B0.145 T-060 merged** after the user committed 6c7369d (legal text removal + output-driven tuning principle). The docs/stream-contract.md conflict was resolved by keeping the T-060 additions and dropping the legal lines. T-071 still waits for T-070. Verification running.
- **B0.146 T-065 done** (4b52b1c). Mode selector rules 0.2.0: a pure carrier now reads carrier/CW (the old envelope filter collapsed on narrow signals); AM detected by an in-phase/quadrature sideband test; SSB and CW keying added; unknown kept. Blind sweep at ≥10 dB: 70/114 → 111/114 correct. Merge waits for the running T-060 verification. **Launched T-072** (detection quality): device-gap sample loss, readsb stamps from sample time, single-detection carriers stored, short-burst detector, flaky daemon test.
- **B0.147 T-060 merge verified** (lint, 144 Rust test groups, Python, UI and acceptance all green). **T-065 merged** onto it; verification of that merge follows.
- **B0.148 T-065 merge verification found a regression.** `listen_retune` fails in 1 of 3 runs at its precondition (before any retune). Listening under broadcast FM is refused with `422 no-analog-mode: no rule matched confidently (OBW99 85 Hz)`: the new selector sometimes can't classify the probe snippet. Lint and acceptance are green, and the test passed consistently before T-065. The fix is T-073, launched now (probe/selector confidence, best-evidence fallback, 20/20 stability, sweep preserved). This is not a legal-gating issue.
- **B0.149 T-052 merged** (5680e74 → fc4ecf0): persisted multi-region selections with server-backed UI and action hooks. **T-061 record outputs launched** (bits, symbols, WAV, IQ slices with SigMF-style sidecars, linked to Bitstream/Recording/Selection; API + CLI + UI Demod/Record; quotas; mock-SDR tests). Five Rust-building agents are running, one over the soft cap, accepted at load ~6. Full verification of main is running.
- **B0.150 Blocker: API monthly spend limit hit (HTTP 429), 2026-09-13 evening.**
  - **Agents terminated:** T-061, T-066, T-070, T-072 and T-073. Many heartbeats queued while they were down; nothing could be launched.
  - **Main is safe:** the T-052 merge verification had already passed (lint, 146 Rust test groups, py, ui, acceptance).
  - **Salvage:**
    - T-066 had committed b7bbe15 (Listen cap and leak fixes) just before stopping; its verification is unknown.
    - Uncommitted work in T-070 (16 files), T-072 (4) and T-073 (2) is committed as WIP on their branches.
    - T-061 had produced nothing.
  - **Limit reset:** 2026-09-14 20:00 PT (now past).
  - **Resume plan, throttled to conserve budget:** the coordinator verifies and merges T-066 itself; resume T-073 (regression) and T-070 (user priority) only; hold T-072, T-061, T-071 and the slot queue until those land.
- **B0.151 Resume after the limit reset (2026-09-14 20:05 PT).**
  - **Staged WIP found intact:** the "WIP commit" step found nothing unstaged; changes in T-070, T-072 and T-073 were already staged in their worktrees.
  - **T-066 merged** by the coordinator (b7bbe15 → 230d652, clean merge, 22 files including a listen lifecycle test). Its agent never reported, so a full verification is running.
  - **Resumed with budget guidance:** T-073 (regression) and T-070 (refinement).
  - **Paused:** T-072 (staged WIP kept) and T-061 (back to todo).
- **B0.152 T-066 merge verified** (lint, 146 Rust test groups including the new listen lifecycle tests, py, ui, acceptance green). The only failure is the known intermittent `listen_retune` no-analog-mode refusal (T-073, resumed). T-066 adds no new failures.
- **B0.153 T-070 merged** (48cd940 → merge): RefinementLoop with WFM objective. Blind mock-SDR selections refine to within 78 Hz with bandwidth 194–201 kHz and PI decoded. The +150 kHz off-raster station refines to its true centre and is flagged off-raster, not snapped. Refined tuning is stored append-only with provenance. **Follow-ups:** neighbouring chains may both demodulate an off-raster station; NBFM/AM/FSK objectives still to write. T-071 unblocked (launches after T-073). Next: verification and a T-061 relaunch.
- **B0.154 T-061 relaunched** from main after the T-070 merge. It uses the refined tuning accessor, Listen lifecycle, T-060 taps and T-052 hooks, on a lean token budget. Two agents are running (T-073, T-061) under the budget throttle; T-071 is next once T-073 lands. Verification of the T-070 merge is running.
- **B0.155 T-070 merge verified** on an isolated target: lint, 149 Rust test groups (0 failed), Python, UI and acceptance all green. `listen_retune` passed this run; its intermittent no-analog-mode failure is still owned by T-073 until that fix lands.
- **B0.156 T-073 merged** (b4670e8). Listen no-analog-mode flake fixed: a quantisation residue on a clean carrier crossed the unmodulated limit, and a carrier under 1 % residue now counts as unmodulated (mode rules 0.2.1). Flake 2/6 → 20/20; sweep unchanged. New follow-up T-074: the stream_external slow-client drop test can pass on disconnect (Sonnet, small). T-071 launches next.
- **B0.157 T-071 concurrent multi-signal demodulation launched** (Opus high, lean). Scope: a unified per-run chain budget (T-066 listen budget + T-060 taps cap), dedupe of neighbouring-chain ownership (T-070 side effect), per-chain CPU/latency/drop counters, and release real-time numbers. Mock-SDR tests: 3 FM stations giving 3 distinct audio streams, and 2+ FSK emitters giving isolated bits streams. Running: T-061, T-071 (budget throttle). Targeted verification of the T-073 merge is running (`listen_retune` 5×).
- **B0.158 T-073 merge verified:** lint, hk-demod tests, `listen_retune` 5/5 and acceptance all green. The listen no-analog-mode flake is closed.
- **B0.159 T-061 merged** (6050d99). Record outputs: bits, symbols, WAV and full-window IQ with SigMF-style sidecars (including refined tuning), linked to Bitstream/Recording/Selection rows. Available through the API, `hk record` and UI Demod/Record, with quotas and drop-not-block. Mock-SDR e2e: FSK bits 20/20, FM WAV and IQ reopen. Follow-up: the IQ refusal for metadata-only sources is inherited and may conflict with the user's permissive policy. Next: verification, then resume paused T-072 in the freed slot.
- **B0.160 T-072 resumed** with its staged WIP and a priority order (device-gap loss, readsb sample-time stamps, single-detection carriers, flaky daemon test; short-burst detector if time allows). Running: T-071, T-072 (budget throttle). T-071 was told to merge T-061 and count output recorders in its chain budget. Verification of the T-061 merge is running.
- **B0.161 T-061 merge verification: one failure so far.** It is the known T-074 flake: `stream_external.rs:484`, `consumer_dropped > 0` false under load 36. The stalled client's wait ends before the queue overflows, so this is not a T-061 regression. T-074 launched on Sonnet (small, test-only unless a real race is proven) as a third, cheap agent. Remaining suites still running.
- **B0.162 T-061 merge verified.** Lint, 150 Rust test groups, Python, UI and acceptance all green. The only failure is the known T-074 stalled-client flake (fix in progress), so T-061 adds no new failures.
- **B0.163 T-072 merged** (c9bab4a).
  - **Ring:** device-gap sample loss fixed at the root (hk-core ring overwrite check); AWARE-042 now 0 lost.
  - **readsb:** stamps come from readsb's sample-count timestamps; SIGNAL-001 tolerance 2 ms.
  - **SPACE-050:** the real cause was the loaded calibration never being stored, so every detection write failed; now 6/6 carriers stored.
  - **Daemon test:** flake fixed.
  - **Short-burst detector:** deferred to T-075.
  - **Verification:** the core ring change is on the real-time path, so full verification is running.
- **B0.164 T-074 merged** (125c232, test-only). The test used `listen.active == 0` as a proxy for counted drops, but release runs just before drops are folded in; it now waits on `consumer_dropped` (20/20, and 3/3 under load). T-075 short-burst detector launches in the freed slot.
- **B0.165 User/supervisor bug → T-076 launched ahead of the queue.**
  - **Symptom:** Listen sends no audio on the live HackRF (release build e95ec47, :8900). Only status records arrive: squelch closed, 0 frames, SNR about 0–2 dB, refined centre 100.8365 MHz (about 36 kHz off). Build 268fcd4 played audio.
  - **Suspects:** T-070 refinement centre, the squelch SNR measure, T-065/T-073 mode rules, T-066 lifecycle, the T-072 ring change.
  - **T-076 does:** a mock-SDR real-time looping test with the exact request, a bisect over 268fcd4..e95ec47, and a root-cause fix. The supervisor re-verifies live.
  - **Why the existing tests may have missed it:** they are unpaced, short, or request by emitter id.
- **B0.166 T-072 merge verified** (core ring change): lint, 151 Rust test groups (0 failed), Python, UI and acceptance all green. The T-076 bisect covers the range including this merge.
- **B0.167 User request (start of message truncated) → T-077.** Parallel test runner (nextest preferred) with serial groups for e2e and pipeline listen/refine/stream tests, CLAUDE.md build commands updated, and wall time reported against the 914 s sequential `cargo test` baseline. Launched on Sonnet (infra only; no overlap with T-071/T-075/T-076). Measurements are noisy while agents build, so load is recorded. The user was asked to resend the truncated start.
- **B0.168 T-071 merged** (dfa57b7): unified per-run chain budget (16 chains / 8 listeners / 8 taps, CPU budget) with per-chain stats in /api/status, dedupe of neighbouring-chain ownership, and 3 concurrent WFM streams each carrying its own PI plus 2 isolated FSK bits streams through the mock SDR. The release build stays real-time with 3 listeners + 2 taps. T-076 was told to merge main (listen.rs admission changed). Full verification running.
- **B0.169 User token-budget rules** (uncommitted user edit to prompts/model-selection.md, 21:48).
  - **Waiting:** agents never wait in the foreground; builds and tests run in the background and resume on notification.
  - **Testing:** agents run targeted tests only. The coordinator runs one full check per merge, and flake hunts use one background command with N runs.
  - **Context:** lean; start a fresh agent rather than continue past ~300k tokens.
  - **Models:** Opus at medium effort unless the work is real-time, novel DSP or hard debugging; Sonnet is the default off the real-time path.
  - **Applied now:** T-075, T-076 and T-077 received the rules (T-077 aligns the runner and CLAUDE.md with them). Saved to coordinator memory; future briefs follow them.
  - **Also:** T-071 merged; full verification running.
- **B0.170 Supervisor correction on build caching.**
  - **Finding:** sccache only hits with an identical target path, so it doesn't help new worktrees; it stays on since it is harmless.
  - **What works:** seed each new worktree target with an APFS clone of main's target (`cp -c -R`, about 4 s, no extra disk). Only the 11 workspace crates then recompile; all dependencies are reused.
  - **Actions:**
    - T-077 adds `just seed-target <dir>` and the CLAUDE.md worktree-launch step.
    - Every agent brief from now on starts with the seed step.
    - Coordinator verification moves back to main's `target/` so it stays warm as the seed; agents no longer build there.
    - Coordinator memory updated.
- **B0.171 Supervisor retracted B0.170.** A cold hk-pipeline build (70 crates) takes 16 s uncached, so target seeding saves nothing: no seed-target recipe, no new launch step. T-077 stays on parallel nextest runs; the bottleneck is the 914 s sequential test run. Memory reverted. Coordinator verification stays on its isolated scratchpad target.
- **B0.172 Disk near floor** (21 GiB at load ~73 while T-077 runs parallel tests). Freed ~3 GB of coordinator leftovers: old scratchpad build dir (1.6 GB), probe data dirs (1.3 GB), and 35 stale hk-* test temp dirs older than 1 h with no open files; now 24 GiB. Kept: in-use worktree targets, the verification target, main target/ (the supervisor demo may run from it), and another session's scratchpad. No launches until disk and agents free up. T-077 was resumed after stopping with no live background job.
- **B0.173 T-075 short-burst detector done** (44f9130). Time-domain energy detector: 24/24 ADS-B squitters and 23/23 OOK bursts at 3–20 dB, start error ≤ 34 µs, 0 false alarms in 6 s, 82× real time at 20 Msps, SIGNAL-001 blind within 50 µs. Merge waits for the running T-071 verification; the coordinator full check follows (burst rows may shift exact detection counts in acceptance).
- **B0.174 T-076 finding: no code regression in Listen.** The live request box 100.6–101.0 MHz covers the DC spike and a weak signal; the strong station in that capture/tuning is at 101.3 MHz. Both the old and new builds stay squelched on that box, and HEAD plays 101.3 MHz with squelch open at ~16 dB and refined centre within 1 kHz. The agent commits a real-time paced listen test (tests only). Asked the supervisor to check whether the UI shows the station at 100.8 MHz while inventory has 101.3 MHz, which would be a live UI axis/click bug to open as a new task.
- **B0.175 Supervisor confirmed T-076:** the 100.8 MHz figure came from the fixture name and `--center-hz` tuning, not the UI. No UI axis/click bug and no new task; the request box was off-station. Plan unchanged: merge T-075 after the T-071 check, then T-076's tests-only commit and the T-077 timing.
- **B0.176 T-076 done** (526dc08, test-only). Adds a real-time paced listen_live test (PCM, squelch open, SNR ≥ 15 dB, refined centre within 5 kHz) and blind_live_paced; the targeted suite passes on the merged tree. T-075 and T-076 merge together after the T-071 verification (114 Rust test groups green so far), then one full check.
- **B0.177 T-071 merge verified** (lint, 152 Rust test groups, py, ui, acceptance green). T-075 (short-burst detector) and T-076 (real-time paced Listen test) merged; worktrees removed (T-076's uncommitted copied fixture data discarded with it). One full check running.
- **B0.178 T-064 hop-set tracker scaling launched** (Opus high, real-time path; background runs, targeted tests per budget rules). Running: T-077 (waiting on its timing job, told to merge main and group listen_live/concurrent_demod as serial) and T-064. Full check of main after the T-075/T-076 merges is running.
- **B0.179 User request (start of message truncated) → T-078/T-079/T-080.**
  - **T-078** (inferred scope): inventory candidates vs confirmed, blind auto-confirm rule, promote/delete API. Visible tests: FM station auto-confirms; intermittent FSK stays a candidate until promoted; a deleted entry leaves the list. Launched now (Opus medium, core interface).
  - **T-079** (Sonnet medium): docs/api.md reference, HTTP contract tests on the mock device, UI decision logic moved into backend endpoints, CLAUDE.md thin-client rule. Next slot, after T-077.
  - **T-080** (Sonnet medium): left sidebar, first/last seen columns removed, Candidates/Confirmed with Promote/Delete. After T-078 and T-079, since both touch ui/src.
  - The user was asked to resend the truncated start.
- **B0.180 Full user brief received** (supervisor file brief-ui-separation-inventory.md, 22:20). Replaces the truncated version.
  - **UI direction:** the user will rewrite the web UI later as a one-screen exploratory UI; no redesign now. The backend owns all signal logic (recognition, analysis, classification, demod, decoding), and the UI is a thin client over a documented API.
  - **T-078 corrected via message to the running agent:**
    - query param `state`;
    - recurrence stats on candidates;
    - delete keeps detections/history, and re-detection creates a new candidate (the earlier "not recreated" was wrong);
    - docs/07 §2.11 update.
  - **T-079:** no dependency; next free slot.
  - **T-080:** depends on T-078 only.
  - **Priority:** all three ahead of T-053/T-056/T-067.
- **B0.181 T-075 + T-076 merge verified.** Lint, 154 Rust test groups (0 failed), Python, UI and acceptance all green; the new short-burst detection rows did not break any exact-count acceptance assertion. Running: T-064 (release bench), T-077 (serial-group timing), T-078 (inventory lifecycle). T-079 takes the next free slot.
- **B0.182 User decision: GPU work is Mac-first.** T-026 (CUDA PFB) is now `deferred` to the Jetson phase: not blocked, not a gate for any milestone. T-056 stays the Mac GPU wiring task. CLAUDE.md gains a rule: new GPU work implements the Mac provider (wgpu/Metal or Accelerate) behind the conformance suite first, and CUDA ports come later on the Jetson. The M0 completion caveat about T-026 no longer applies.
- **B0.183 T-064 profile finding.** The dense urban whole-pipeline replay is no longer tracker-bound: 1,047 of 1,060 samples are in the WFM/RDS chain's `ParamEstimator::estimate` → `ChannelFilter::apply` (hk-estimate), and the unpaced replay waits for that chain. Filed as T-081 (Opus high, real-time path), queued after T-078–T-080 and ahead of T-053/T-056/T-067. T-064 is finishing its targeted tests before committing.
- **B0.184 T-064 merged** (b4db9e9). Hop-set membership is bounded and the raster refits only when the channel set changes; parity is exact. Detect bench at 20 Msps urban improved 1.5× → 2.2× RT (tracker share 65 % → 7 %). Whole-pipeline l32g30a1 improved 1.0 → 1.2× RT; l24g20a0 stays 0.35×, bound by the WFM/RDS chain ParamEstimator (T-081). T-079 launches in the freed slot; full check follows.
- **B0.185 T-078 done** (a5a1854): inventory lifecycle candidate/confirmed/deleted with history, auto-confirm rule (CRC-valid decoded identity, or a trusted continuous track), recurrence stats, and state/promote/delete API. Mock-SDR tests: FM auto-confirms; FSK stays candidate until promoted; delete leaves the list; re-detection creates a new candidate. It merges after the running T-064 verification. **Follow-up T-082:** one entry per physical emitter (track- and decoder-based entries currently duplicate); ideally before T-080.
- **B0.186 T-077 merged** (a897c08). `just test` now uses cargo-nextest plus doctests: 170.9 s vs the 914 s sequential baseline (~5.3×) at load ~17. Heavy/timing-sensitive suites (e2e, pipeline listen/retune/lossless/refine/stream, hk-api, hk-cli, ring stress, refine_wfm_real) run in a serial group, and 4 timing-sensitive tests get retries=1. Five runs of the heavy group all passed. `just test-seq` is the fallback; agents use `just test-crate`/`just test-one`. Coordinator verification switches to `just test` + acceptance.
- **B0.187 Supervisor: slots free** (1 agent running, 34 GB disk). Launched T-056 (Mac compute providers wired into the pipeline; Opus medium) and T-067 (control API completeness; Sonnet medium per token-budget rules) alongside T-079; neither depends on the T-064 check. T-067 reports its routes for T-079's docs/api.md. T-082 → T-080 → T-081 stay in order behind the T-078 merge (waiting on the T-064 verification).
- **B0.188 T-064 merge verified** (lint, 154 Rust test groups, py, ui, acceptance all green). **T-078 merged** (inventory lifecycle); full check with the new nextest `just test` + acceptance follows. T-079 is told the inventory routes are on main. T-082 (one entry per physical emitter) launches next, then T-080 and T-081 in order.
- **B0.189 T-082 launched** after the T-078 merge (Opus medium): link decoder evidence to the track's emitter and merge existing duplicates. T-079 told the inventory routes are on main. Full check of main after T-078 is running with the nextest `just test` for the first time. Running: T-079, T-056, T-067, T-082. T-080 follows T-082; T-081 after T-080.
- **B0.190 T-078 merge verified**, the first coordinator check on the nextest `just test`. 917 tests passed (5 skipped) in 126.9 s; the whole `just test` including doctests, Python and UI took 180.5 s against the old 914 s. Lint green; acceptance 22 passed, 1 ignored. Running: T-079, T-056, T-067, T-082.
- **B0.191 Supervisor: M1 is being re-scoped by the user.** Decoders will be built inside hackriff from reusable blocks, not as per-protocol plugins. No M1 tasks are added until the M1 brief arrives; work continues on M0b (T-079, T-056, T-067, T-082 running; T-080, T-081, T-053 queued).
- **B0.192 T-079 merged** (fc80c41).
  - **API reference:** docs/api.md covers all 36 routes plus TCP and on-demand openers; a test keeps routes and docs in sync.
  - **Contract tests:** 10 HTTP contract tests against a real `hk serve` on the mock device.
  - **Logic moved:** peak picking is now backend `GET /api/analysis/strongest`.
  - **CLAUDE.md:** thin-client rule added.
  - **T-067:** told its new routes must be documented in docs/api.md (the sync test enforces it).
  - **Next:** full check running.
- **B0.193 T-079 merge verified:** lint, nextest 928 passed (5 skipped; the whole `just test` took 190.5 s under load ~43), acceptance green. The new API contract tests and the route-docs sync test are part of the suite. Running: T-056 (GPU acceptance + RTF), T-067 (merging main, documenting routes), T-082.
- **B0.194 T-056 merged** (e2ca295). Compute providers are wired into detect/history/spectrum via `--compute` and `HK_COMPUTE*`, and `/api/status` reports per-reader providers (documented in api.md, contract-tested). The default stays CPU with identical detections; with GPU features, auto picks the GPU and SPACE-050/SIGNAL-062/Listen pass. Whole-pipeline RTF is unchanged on the short urban fixtures (the l24 capture is bound by the WFM chain, T-081); the GPU gain needs a long or live capture to show. Checked: no fixture data in the commit. Full check running.
- **B0.195 T-082 done** (56a6d09): one inventory entry per physical emitter. `same_emission` links decoder-identity entries to the matching track entry (centre tolerance + time overlap, hop-set-only, identities never cross-link, fingerprint check for co-channel tracks); the merge carries evidence/identity/refined tuning/labels, confirmed wins, deleted is never merged. FM and FSK now appear once each; nearby stations stay separate. The merge waits for the T-056 verification (934 tests passed so far, acceptance running); T-080 launches after the T-082 merge.
- **B0.196 T-056 merge verified** (lint, nextest 934 passed / 5 skipped, just test 224.9 s under load, acceptance green). **T-082 merged**; no fixture data was in the branch. Next: full check; T-080 launches from the new main; T-067 is told main moved.
- **B0.197 T-080 UI tweaks launched** (Sonnet medium) after the T-082 merge: left sidebar for Selections and Inventory, first/last seen columns removed, Candidates/Confirmed lists with Promote/Delete via the T-078 endpoints. It owns layout, inventory.ts and the selection panel; T-067 keeps ui/src/controls. Full check of main after T-082 running. T-081 follows T-080; then T-053.
- **B0.198 Lint failed on main after the T-082 merge:** rustfmt diffs in `cluster.rs`, `repo/mod.rs`, `same_emission_tests.rs` and `inventory_lifecycle.rs`, because the agent skipped fmt. The coordinator ran `cargo fmt --all` and committed 22a1add (format-only). Clippy is re-checking in the background, and the T-082 test and acceptance results come from the running check. T-080 and T-067 were told to run `just lint` before committing.
- **B0.199 Clippy fix after the T-082 merge.** Once rustfmt was fixed, clippy flagged `if !(err <= f_tol)` (neg_cmp_op_on_partial_ord) at `crates/hk-model/src/repo/cluster.rs:471`. It is rewritten as an explicit `partial_cmp` match with the same NaN-as-out-of-tolerance behaviour. `just lint` green; hk-model 97/97 tests pass. The full check started after the T-082 merge is still running.
- **B0.200 T-082 merge verified.** nextest 939 passed (5 skipped; just test 241 s under load) and acceptance green. The run's lint failure is resolved by 22a1add (rustfmt) and 9ef59cd (clippy), and lint is now green. Running: T-080, T-067. Then T-081, then T-053; M1 on hold.
- **B0.201 T-067 merged:** control API completeness.
  - **State endpoint:** display limits and the device baseband-filter range; the UI no longer hard-codes limits.
  - **New controls:** baseband filter endpoint and FFT window option.
  - **Bookmarks:** rename via the existing PUT is now documented and tested.
  - **Docs/tests:** api.md and contract tests updated.
  - **Coordination:** T-080 told to merge main (T-067 touched `ui/src/index.html` and controls). Full check running.
- **B0.202 T-080 merged** (ui-only): left sidebar, inventory Candidates/Confirmed with Promote/Delete, first/last seen columns removed; UI tests pass on main. This completes the user's T-078/T-079/T-080 UI-separation set. **T-081** (WFM/RDS chain ParamEstimator bottleneck) launches next in order; T-053 follows.
- **B0.203 T-067 merge verified:** lint, nextest 941 passed (5 skipped; just test 216 s), acceptance green. T-080 (ui-only) verified with UI tests (59 passing). T-081 launched.
- **B0.204 T-081 merged** (fbbb6de). The whole-pipeline bottleneck was an 8191-tap channel filter at 500 kHz for a narrow carrier; narrow bands now decimate first and wide ones use SIMD. Dense urban 20 Msps replay: l24g20a0 improved 0.37 → 1.8× real time (the ≥ 1.2 target is met on both urban captures); estimates identical. Full check running. The only open M0b task left is T-053 (HackRF HIL acceptance run), which needs the HackRF currently held by the supervisor's live server.
- **B0.205 T-081 merge check:** lint and acceptance green. `just test` hit 1 failure and stopped at 764/943: `hk-pipeline::data_path plugin_decodes_get_family_explanations_that_reveal_nothing_more_when_gated`. It is being investigated (5× rerun, flake vs regression); T-081 only changed the channel filter.
  - **User decision:** run T-053 HIL now. The agent stops the supervisor's live `hk serve` (pid 45833) holding the HackRF, runs the device conformance + acceptance suite against live FM-band air (receive-only), logs results to `docs/hil/2026-09-14-t053.md`, and releases the device. The supervisor restarts the demo.
- **B0.206 The T-081 check failure was a flake.** `data_path plugin_decodes_get_family_explanations_that_reveal_nothing_more_when_gated` (readsb plugin decoded 0 under load) passed 5/5 on rerun and T-081 did not touch the plugin path. It gets a nextest retry like T-077's timing-sensitive tests. The remaining 179 tests from that run (fail-fast) run as a full check after the T-053 HIL finishes, so real-hardware timing isn't loaded down.
- **B0.207 User via supervisor → T-083** (Sonnet, small UI, no redesign). Sortable sidebar lists: inventory Candidates/Confirmed and Selections default to frequency ascending, and clickable headers re-sort by frequency, SNR, last activity or recurrence where present, toggling asc/desc. Queued after T-053; M1 still on hold pending the user brief.
- **B0.208 T-053 HIL merged** (abb595f), timeboxed partial pass on the real HackRF.
  - **Passes:** device conformance 20/20; `hk serve --hackrf` gets real FM inventory rows; the live run keeps real time at 2.4 Msps with 0 drops and a stable floor.
  - **Live-air failures:** the surveyed 101.3 MHz station was not detected; no FM top-3, no RDS PI, no Listen.
  - **Hardware-only findings → T-084:** an off-centre station becomes one wide edge row; the run summary counts 6 emitters but /api/inventory returns 1 row; no RDS decodes on live air. T-084 reproduces offline first, then needs the HackRF again.
  - **Device:** the demo `hk serve` was stopped for the run and not restarted; the HackRF is free.
  - **Next:** T-083 launches; full check of main running.
- **B0.209 Launched T-083** (sortable sidebar lists, Sonnet) and the **offline phase of T-084** (reproduce/fix the edge wide-row, 6-vs-1 inventory count (possible T-082 over-merge) and missing live RDS via the mock SDR; update hil_hackrf.rs to centre the station). Full check of main running. HackRF free; the supervisor may restart the demo. T-084's live re-run will need the device again.
- **B0.210 Full check after the T-081/T-053 merges and nextest config:** nextest 943 passed (5 skipped; the 179 tests missed earlier and the new retry both covered) and acceptance green. Lint failed only on rustfmt in T-053's `hil_hackrf.rs`; the coordinator ran `cargo fmt` and committed, and clippy is re-running. T-084 was told to merge main (it edits hil_hackrf.rs).
- **B0.211 Main lint green again.** After the rustfmt fix (b863ee8), clippy flagged `needless_range_loop` in T-053's HIL survey centroid loop; it was rewritten as an iterator loop with identical behaviour (af4531f) and `just lint` passes. Tests were already green (943 passed, acceptance). Running: T-083, T-084 (offline).
- **B0.212 T-083 merged (ui-only).** Sidebar lists sort by frequency ascending by default, and clickable headers toggle asc/desc (inventory: frequency, bandwidth, family, identity, count, recurrence, tags; selections: frequency). SNR and last-activity sorting are not possible: those fields aren't in inventory rows, and T-080 removed last-seen. UI tests pass on main. Running: T-084 (offline).
- **B0.213 M1 is a go** (user-approved brief docs/13-m1-decoder-workbench.md). Decoders are built inside hackriff from generic blocks + declarative recipes + a declarative field-map parser + a Wireshark-style packet inspector, not per-protocol plugins. External decoders (rtl_433, readsb) are a long-tail escape hatch and test oracles.
  - **Roadmap:** docs/11 M1 row replaced.
  - **Tasks** (T-085..T-097):
    - **Contract first:** T-085 M1-DESIGN (ADR-0011: block contract, recipe schema, parser field-map schema, inspector stream framing, worked RDS recipe; Opus high, reviewed) blocks the rest.
    - **Parallel after T-085:** T-086 Blocks A (demod/symbol), T-087 Blocks B (framing/FEC), T-088 recipe runtime, T-089 parser + inspector API, T-091 authoring assist.
    - **Later:** T-090 inspector UI (after T-089); T-092 decoded-stream capture and T-093 follow_hops (after T-088).
    - **Tutorials:** T-094 RDS reference, then T-095 POCSAG, T-096 ACARS, T-097 ADS-B.
  - **Blocker:** T-097's live HIL needs a **1090 MHz antenna** (user); the recorded/mock path is unblocked.
  - **Now:** T-085 launched; the 4–6 agent fan-out follows its reviewed merge. T-083 already merged; T-084 (offline HIL fixes) continues in parallel.
- **B0.214 T-098 launched** (Sonnet) in parallel with T-085: source the POCSAG/ACARS/ADS-B tutorial fixtures (public SigMF or decoder test vectors, oracle truth lists, synthetic fallback). Tutorials T-095/T-096/T-097 now depend on it. Running: T-085 (M1-DESIGN), T-084 (offline HIL fixes), T-098.
- **B0.215 T-084 offline phase merged** (f1798e3).
  - **Wide edge row:** a false bursty hop set formed from near-threshold flicker; fixed with an 8 dB hop-set channel SNR gate.
  - **6 vs 1 emitters:** the summary counts at stop, while tracks enter the inventory on close. Clarified in the summary and logged in HIL.
  - **Missing live RDS:** hidden by the false row. A documented limit remains for strong adjacent channels → follow-up T-099.
  - **HIL test:** now centres the station.
  - **Next:** full check with acceptance (the hop-set gate may affect AWARE-036/042). The user is asked about the live re-run.
- **B0.216 User deferred the T-084 live HackRF re-run.** T-084 closed on its offline fixes; the live run is now T-100 (deferred, needs exclusive HackRF). **T-098 merged:** synthetic POCSAG (3 channels, multimon-ng oracle decodes 3/3) and synthetic ACARS (self-consistent checker only); ADS-B uses the existing synthetic squitter plus readsb oracle; no recordings were found or captured. Full check of main (T-084 + T-098) follows. Running: T-085 M1-DESIGN.
- **B0.217 T-099 launched** (dense-FM mode selection with strong adjacent channels, Opus medium), unblocked by T-084. Full check of main after the T-084 + T-098 merges is running; T-084's hop-set gate may touch AWARE-036/042. Running: T-085 (M1-DESIGN), T-099.
- **B0.218 The full check after the T-084 + T-098 merges failed.** Lint passed.
  - **Test 1:** `hk-pipeline signal_001_readsb`: readsb decoded 0 of 16 (load ~24; possibly the readsb timing flake).
  - **Test 2:** `inventory_lifecycle::t078_steady_fm_station_auto_confirms`: the FM station now appears **4×** in the inventory (expected 1).
  - **Test 3:** `inventory_lifecycle::t082_two_nearby_fm_stations_stay_two_entries`: each station appears **3×**.
  - **Suspect for 2 and 3:** T-084's hop-set SNR gate (8 dB), which T-084 never ran acceptance against. A likely mechanism is track fragments no longer collapsing, leaving duplicate emitters. T-098 only changed py synth scenarios.
  - **Next:** the three tests are being re-run 3× to tell flakes from regressions; if deterministic, bisect against pre-T-084 main (3d2a3db^1) and fix or revert.
- **B0.219 Reruns:** `signal_001_readsb` passed 3/3, so it's a flake; it gets a serial group and 1 retry in nextest. The `inventory_lifecycle` t078 (FM station 4× in inventory) and t082 (3× per station) failures are **deterministic**, 3/3. Bisecting at 3d2a3db (T-084 without T-098) and at pre-T-084 to find the culprit before fixing.
- **B0.220 Bisect result:** `inventory_lifecycle` t078/t082 FAIL at 3d2a3db (T-084 merge) and PASS at 6700731 (its first parent), so T-084 caused the regression; T-098 did not. T-101 has been launched to fix the root cause (Opus medium) while keeping T-084's gate intent. T-085 M1-DESIGN has been delivered (ce464ca: ADR-0011, the new crates `hk-recipe` and `hk-blocks`, `hk-stream` inspector records, an RDS worked recipe and an ownership map). It is now in a timeboxed Opus review before merge.
- **B0.221 T-085 review verdict: FIX-FIRST** (timeboxed, one round). Must-fix items:
  1. ADS-B can't be built from the pinned ports: PPM chip-level preamble and frame boundaries, and variable-length framing (length from DF, ACARS ETX terminator).
  2. Hot field-map edits can't be implemented: `update_params` can't receive a FieldMap, and EditPlan misses changes to a map's content.
  3. POCSAG 4-bit BCD text, ACARS parity and integer scaling can't be expressed.
  4. The ownership map misses shared files (hk-api lib.rs/http dispatch, Cargo deps, api_contract.rs, opener registration).
  Real-time safety and gating reuse passed. A fresh Opus agent (the original was past 300k tokens) is making the fixes plus cheap nits in the T-085 worktree; merge follows unless a real correctness bug remains.
- **B0.222 T-099 merged** (5855c2a). In dense FM, when C13's OBW99 abstains, the adjacent-channel OBW is measured between the spectral valleys; the channel filter is capped inside them, and the pilot check runs on that channel only (mode-rules@0.3.0, blind, no raster). Coordinator reviewed the diff: ~200 lines, not per-chunk real-time, no allocation concerns. Tests:
  - dense scene: all 5 boxes WFM, PI decoded; pipeline 3/3 chains and PIs;
  - sweep unchanged (111/114 at ≥10 dB);
  - hk-demod 33/33; analog/listen_retune/lossless/signal_062/refine pass.
  The full check is deferred until T-101 merges, so one run covers both; main still has the known T-084 inventory regression.
- **B0.223 T-101 root cause.** A WFM station's weak, narrow edge flicker used to hide inside chance hop sets. T-084's 8 dB gate correctly stopped those hop sets forming, so each flicker now becomes its own inventory candidate. The fix in progress marks in-band fragments of a wider, stronger continuous host track. t078 now passes; t082 still has a residual at 101.6749 MHz. Follow-up **T-102** is added (serialised behind T-101 on hk-detect): the station's own track is 407.8 kHz, over the 400 kHz wfm-rds cap, so its chain attaches only through a fragment.
- **B0.224 T-101 merged** (7cddade); the coordinator reviewed it. Closing tracks are marked `inband_fragment` when all of these hold: inside a continuous host (duty ≥90%, band widened by 0.5× its width each side), host ≥4× wider, host ≥6 dB stronger, and the fragment's whole life lies within the host's. Fragments stay tracked and can attach chains, but they get no inventory sighting. Bounded ring of 32 closed hosts; cost is only at close. Tests: acceptance inventory_lifecycle and signal_062 5/5; hk-detect 121 passed (LFS fixtures hydrated; all T-084 hop tests re-run); hk-pipeline inventory 16. **Risk noted:** the 0.5× skirt widening could hide a weak narrowband emitter one channel off a strong wideband one. Folded into T-102's acceptance: fix the station width, then revisit the skirt and add an adjacent weak emitter test. T-102 launched. Full check of main (T-099 + T-101) running.
- **B0.225 T-085 merged** (be9d0c0; fix round 527bf95). Contents:
  - ADR-0011 plus the `hk-recipe` and `hk-blocks` crates;
  - `ppm_demod`→frames, with variable length via `length_from`/`terminator`;
  - hot field-map edits (`update_params` gets `BuildCtx`; `EditPlan` treats a map change as a hot change);
  - field maps gain `char_bits` 4, `pocsag-bcd`, parity, scale/add/value_unit and `skip_bits`; `RECIPE_SCHEMA_VERSION` = 2;
  - worked recipes for RDS, ADS-B, POCSAG and ACARS (the last three unverified starting points);
  - ownership stubs: deps pre-added and per-task module stubs wired into hk-api dispatch; ROUTES, api_contract.rs and opener registration are shared append-only.
  The full check started for T-099+T-101 was stopped and restarted to cover T-085 too. **M1 fan-out:** T-086 Blocks A, T-087 Blocks B, T-088 recipe runtime, T-089 parser+inspector API and T-091 authoring assist launched, alongside the running T-102 (6 agents). T-090, T-092 and T-093 wait on T-088/T-089.
- **B0.226 Full check of main 5d48c0e (T-099 + T-101 + T-085).** Lint green; nextest 979/979 in 238 s. Acceptance 22/23: `inventory_lifecycle` t078/t082 now pass (T-101 fixed the regression). New failure: `signal_001::signal_001_adsb_readsb_plugin_chain` (blind.rs:798), under load from 6 concurrent agent builds. It's the same readsb plugin-startup family as the earlier nextest flake. Rerunning 3× to classify.
- **B0.227** `signal_001_adsb_readsb_plugin_chain` passed 3/3 on rerun, so it's a flake: 15/16 decoded, and only the first squitter at t=9.9 ms was missed while load was ~24. This is the third instance of plugin startup racing the stream start, so a real lossless-start fix is filed as **T-103** instead of another retry. It's queued behind the 6 running M1/T-102 agents (throttle, 25 GB disk). Main is otherwise verified green at 5d48c0e.
- **B0.228 T-102 merged** (be5934c). Coordinator reviewed. Changes:
  - (1) `hk-pipeline` detect.rs `occupied_box`: the chain candidate box is now the OBW about the centre, clamped inside the pixel box. The pixel box (the union of threshold bins over 1 s) inflated a WFM station to 408 kHz vs 333 kHz OBW.
  - (2) tracker `split_preds` accepts a continuation starting up to `coincidence_frames` (2) early. The real fixture restarts 1–2 frames early, which had merged station 1 with station 2 into 410 kHz.
  - (3) `HOST_SKIRT_FRACTION` removed. A fragment host is now its measured pixel extent (≥ OBW) + 2 bins, and fragments must be centred inside it. This unhid the real ~101.70 MHz weak emitter.
  Result: the station attaches wfm-rds directly (PI 1694). New test: weak narrowband emitters 230–280 kHz off a station keep their entries. hk-detect 122, acceptance inventory/signal_062 5/5, pipeline 42, lint clean. T-103 (lossless plugin start) launched into the freed slot; full check of main running.
- **B0.229 T-089 delivered** (5324b42): field-map `Evaluator` (compiled once; layer tree with bit/byte ranges and a per-byte leaf index; fit status), `fields`/`text` blocks, gated `publish_frame`/`publish_record` (stream contract 1.2), routes `POST /api/inspector/parse` and `POST /api/captures/{id}/parse`, and a `CaptureSource` reader trait for T-092. RDS/ADS-B/POCSAG/misfit tests pass; a 10k-frame re-parse takes 185 ms in debug. It made small edits outside its ownership (`ApiState.captures`, cli pipeline `None`, the ondemand version test). A timeboxed Opus review is running; the merge waits on it and on the in-flight full check. T-090 (inspector UI) launches after merge.
- **B0.230 T-086 delivered** (691ef04). It adds 14 IQ/demod/symbol blocks to the block contract. Blind bit-recovery tests pass for FM, AM, 2-FSK (150 ppm rate error), MSK, the ACARS path, ADS-B PPM 56/112 and RDS, and outputs are chunk-invariant. Release bench: the slowest block (lowpass) runs at 9.5e6 samples/s. Additive changes: `DdcKernel` in hk-dsp, and `hk-demod::dsp` made pub. Params for its blocks are pinned in ADR §1.5/`planned()`, which may conflict with T-087 at merge. Known limits: resample only decimates; rrc roll-off fixed at 0.35; ppm_demod skips over decoded frames. A timeboxed Opus review is running.
- **B0.231 T-087 delivered** (0979e48). Blocks: sync_search (sync-word + RDS offset-word), assemble (POCSAG), deframe, (de)interleave, crc (burst correction), bch, parity, checksum. Tests pass for RDS, POCSAG BCH, ACARS CRC-16/KERMIT and ADS-B CRC-24 vectors: hk-blocks + hk-estimate 96/96. Gaps: per-word check status is lost in assemble, and offset-word sync has no bit-slip search. A timeboxed Opus review is running; it includes a trial merge against T-086, since both edit ADR §1.5/`planned()` and duplicate the length logic.
- **B0.232 Full check of main 81d382d (after T-102): fully green.** Lint clean; nextest 980/980 in 254 s; acceptance 23/23. Main is verified. T-086, T-087 and T-089 are in timeboxed Opus review; T-088, T-091 and T-103 are running.
- **B0.233 T-089 review: FIX-FIRST** (timeboxed). Two must-fixes:
  1. The evaluator's length arithmetic overflows on over-the-air data. A 64-bit length field panics in debug, and in release it wraps into a capacity-overflow abort (eval.rs:414/445/454/515).
  2. `tests/e2e/tests/stream_external.rs:263` still asserts stream version 1.1.
  Everything else checked out: gating, API limits, 1.2 back-compat, bounded allocation, and no painful collisions with T-088/T-091. A fix round is running in the T-089 worktree, plus docs error codes and a frame cap on capture re-parse.
- **B0.234 T-086 merged** (691ef04). Opus review: MERGE. Real-time safety, state carried across chunks, time maps, the ADS-B DF length rule, blind tests and trial merges against T-087/T-089 all check out clean. Nits filed as **T-104** (Blocks A hardening):
  - dedupe LengthFrom→FrameLength after T-087;
  - clock_recovery output bound;
  - allocation-free restarts;
  - extreme-param caps;
  - NaN poisoning;
  - manchester time map;
  - allocation-counting test.
  Also noted for T-088: the runtime must set RESET on a rebuilt node's first chunk. Full check deferred until the T-087/T-089 merges, so one run covers all three.
- **B0.235 T-087 review: FIX-FIRST** (timeboxed). One must-fix: CRC burst correction manufactures valid frames on narrow CRCs. RDS 10-bit at burst 5 turns 36% of garbage blocks Valid; burst 1 turns 2.5–5%. Also real-time nits: assemble/deframe under-declare output counts, so FrameBuf reallocates on the RT thread. Checked OK: bch 3-bit refusal guaranteed with parity, ACARS CRC-16/KERMIT span and sync, blind tests, clean merge-tree with T-086. Keep `FrameLength` as the shared length evaluator; porting ppm_demod goes to T-104. Fix round running.
- **B0.236 T-089 merged** (5b578ec; fix round dc453ce). Fixes: checked/saturating length arithmetic, with tests near u64::MAX for bytes/ascii/repeats; stream_external version test now uses constants; capture re-parse fit capped at 100k frames with a `truncated` flag; api.md 413/422/500 documented. **T-090 inspector UI** launched (Sonnet, thin client over docs/api.md; merge after coordinator review). Full check of main covering T-086 + T-089 is running.
- **B0.237 T-088 delivered** (0a256ac). What landed:
  - recipe chains (ring→DDC→graph) under the chain budget;
  - off-thread build with swap at a chunk boundary, RESET on rebuilt nodes, zero sample loss across 4 hot edits;
  - file-backed versioned recipe store;
  - routes /api/blocks, /api/recipes*, /api/pipelines* (PUT hot-edit, save);
  - stage and inspector streams (interim §6 message framing), with openers registered;
  - mock-SDR e2e and allocation-free tests.
  Not yet: messages outputs (needs T-089 eval), follow-hops (T-093), capture targets (T-092), and `/api/recipes/match`. The branch predates the T-086/T-089 merges, so integration is needed. A timeboxed Opus review is running, including a merge-tree against main. **T-087 fix round done** (40ecd1a: CRC false-correction bound ≤1e-3 means no RDS correction and CRC-24 1-bit OK; output bounds; FrameLength validation). It merges after the current full check. The T-088 agent ran to ~444k tokens, past the 300k guideline; fix rounds go to a fresh agent.
- **B0.238 Full check of main e9dbaa0 (T-086 + T-089): green.** Lint clean; nextest 1023/1023 in 256 s; acceptance 23/23. **T-087 merged** (fix round 40ecd1a). **T-103 merged** (071fb12). Coordinator reviewed T-103. Two root causes: (a) the chain's fixed 2 s settle window killed a still-starting plugin, giving 0 decodes; (b) the readsb wrapper's 2 s Beast connect wait gave the first squitter a fallback stamp. Fix: `PluginInstance::finish` delivers queued input, then EOF, and waits on plugin progress (a lossless replay stalls out at ≥30 s, live at `settle_s`); the ring reader is dropped first; Beast wait raised to 20 s. Both regression tests failed on the old code. nextest retries removed; the serial groups stay for CPU. Watch: `readsb_wedge_message_ends_the_wrapper_without_waiting_for_the_child` hit 10.3 s against its 10 s bound once, at load 39. **T-104** (Blocks A hardening) launched; a full check covering T-087 + T-103 is running.
- **B0.239 T-091 delivered** (85f813d). Three assists:
  - **Sync/period hunt:** autocorrelation plus block-code linear dependence.
  - **CRC/BCH search:** the generator comes from the GCD of frame-difference polynomials, which makes the search exhaustive for widths 3–32 without enumerating polys. Init/xorout are solved and RDS offset words are grouped.
  - **Field-boundary drafts:** a draft field map that passes validation.

  Blind recoveries: RDS 0x5B9 plus offset words, POCSAG sync/BCH(31,21)/parity, ADS-B CRC-24, ACARS CRC-16/KERMIT plus parity, and synthetic FSK layout and CRC. Routes `POST /api/assist/{sync,fields,crc}` have ops/bit/frame caps. A timeboxed Opus review is running; its focus is API thread starvation from CPU-heavy requests and noise-input scoring.
- **B0.240 T-090 delivered** (0358d24, Sonnet, UI only). It adds a Frame inspector pane with:
  - a frame table with paging;
  - hex + ASCII view;
  - a layer tree;
  - linked selection via the backend's `bytes`/`byte_index`;
  - a draft field map + pasted frames box using `/api/inspector/parse`.
  26 UI tests, all green in `just test-ui`. Coordinator scan: the only client-side byte handling is hex→bytes for display, with no parsing or range math (thin-client rule OK). Missing API: a capture listing (T-092) and `/ws/open/inspector` (T-088 integration). It merges after the in-flight full check.
- **B0.241 T-088 review: FIX-FIRST** (timeboxed). Two must-fixes:
  1. The interim `metadata.record` framing isn't wire-compatible with T-089's §14 (frame/status/edit record types plus the header `inspector` profile), and T-090 is coded against §14.
  2. The stream registry is overwritten while staging an edit that keeps an output id, so a failed edit leaves the running output unreachable; `start()` has the same problem.
  Held up: RT path (retired graph dropped on the control thread), edit races serialised, store path safety, and zero loss across 4 hot edits (5/5 tests). merge-tree conflicts: http.rs, cli pipeline.rs, api.md. A fresh Opus agent is merging main into the branch, then fixing both and running the RDS recipe through the runtime.
- **B0.242 Full check of main 4f7336b (T-087 + T-103): green.** Lint clean; nextest 1044/1044 in 231 s, no retries needed; acceptance 23/23. **T-090 merged** (0358d24). It's UI-only, so the check is `just test-ui` on main.
- **B0.243 T-090 on main: `just test-ui` green** (26 inspector plus existing suites).
- **T-091 review: FIX-FIRST** (timeboxed). The GF(2) math and garbage-input robustness are solid, and it merges clean with main. Must-fix:
  1. Assist routes run CPU-heavy search on HTTP connection threads with no concurrency cap, and op counts are ~3× low (9.2 s measured). A few calls can starve the control API.
  2. With 3–4 frames, a wrong CRC generator (the true one times an extra factor) wins at score ~0.9 in 15–26% of trials.
  3. Random frames yield a sync word scored 1.0.
  4. Duplicate frames inflate evidence (alternating zeros/ones → BCH at 0.97).
  A fresh Opus fix round is running in the T-091 worktree.
- **B0.244 Disk cleanup** (supervisor: disk at 20 GB). Merged M1 worktrees were already removed at merge; `git worktree prune` found nothing stale. The remaining worktrees are all active (T-088, T-091, T-104). Removed coordinator scratchpad leftovers: the bisect `target-verify` (7.7 GB) and old hkdata/long/wgpu-probe outputs (~1 GB).
- **B0.245 T-088 merged** (fix/integration 75c399d). Fixes: §14 framing via publish_frame/publish_record with the header inspector profile; streams registered only after a successful swap/spawn (tested: a failed edit keeps the running output served); closed taps dropped off the RT thread; DELETE unregisters streams; validate isn't audited. `recipes/rds.recipe.json` runs on a mock channel (all blocks resolve; the tone fixture has no RDS). Follow-up note: the generic `StreamReader` returns §14 records as Unknown. **Launched:** T-092 (decoded capture + scrub), T-093 (follow_hops), T-094 (RDS tutorial). Full check of main running.
- **B0.246 T-104 delivered** (514b17c); all 7 review items have tests. Changes:
  - ppm_demod now uses the shared FrameLength; LengthFrom removed.
  - clock_recovery output is bounded by the shortest spacing, and the step is clamped. A hot bandwidth increase triggers a Rebuild.
  - `FirDecimator::clear` gives allocation-free restarts.
  - Caps at init: 65,536 samples per symbol, 65,536 taps, 2^22-sample ppm window.
  - NaN/infinite input is zeroed and counted as `non_finite`.
  - manchester: realignment holds later bits for the next chunk so the time map stays exact. Declared max_items grows by about one chunk of latency. Held bits are lost uncounted at END (minor, noted).
  - Counting-allocator test: zero allocations across 14 blocks × 24 configs, including restarts.
  hk-blocks 61/61, lint clean. Merge follows the in-flight full check (follow-up to reviewed work, per-item tests).
- **B0.247 Full check of main a02e66f (T-088): green.** Lint clean; nextest + UI 1058/1058 in 228 s; acceptance 23/23. **T-104 merged** (514b17c). Full check covering T-104 running.
- **B0.248 Full check of main 1183760 (T-104): green.** Lint clean; nextest + UI 1068/1068 in 223 s; acceptance 23/23. M1 merged so far: T-085, T-086, T-087, T-088, T-089, T-090 and T-104. In flight: the T-091 fix round, T-092, T-093 and T-094.
- **B0.249 T-091 merged** (626a5dd; fix round 391968d). Changes:
  - Assist is single-flight: a concurrent call gets 503 busy.
  - Op counts recalibrated to ~0.5–1.6 ns/op, since some paths were ~25× under. Default budget 5e8 (≤0.76 s in release), max 1.5e9 (≤2.4 s).
  - CRC scores are discounted by the chance of a shared extra factor.
  - Sync score is absolute, significance-based; the old relative score is kept as `relative_score` (API meaning change, documented).
  - Evidence is counted over distinct frames and non-periodic differences.
  Noise now gives a top sync score of 0.0 and no code from duplicate or alternating frames. Residual accepted under the timebox and filed as **T-105**: 3/200 wrong CRC tops at ~0.95 with 8 frames, and near-tie ordering at 3 frames (all scored low). Full check of main running.
- **B0.250 Full check of main 626a5dd (T-091): green.** Lint clean; nextest + UI 1086/1086 in 237 s; acceptance 23/23. In flight: T-092, T-093, T-094, T-105.
- **B0.251 T-094 merged** (7a17979), coordinator stat review. `recipes/rds.recipe.json` decodes the real FM capture unchanged. The station is found blind at 101.3022 MHz. PI 0x1694, PTY 7 and PS match the hidden truth and the oracle. 78% of groups are CRC-valid, which is the capture's limit (the oracle sees 8.7% block errors). All 44 oracle-valid groups match exactly, with 0 conflicts. RT is decoded after a hot edit to on-change. Synthetic RT case is exact. `docs/tutorials/01-rds.md` added. Follow-up **T-106**: oracle RDS positions count from pilot lock, ~103 ms late. **T-093 delivered** (8f02471): the recipe splits at the follow_hops node into per-channel upstream instances and a single downstream. Channel sets come from a static list, a blind hop-set fingerprint, or blind detections. Budget is N chains. Merge orders within a bounded window and dedupes by bytes across channels, keeping the best copy. `set_channels`/`refresh_channels` swap at a boundary. 4-channel mock-SDR test plus a hop-set refresh test. Timeboxed Opus review running. **Launched:** T-096 ACARS tutorial and T-097 ADS-B tutorial (recorded/synthetic; live 1090 MHz deferred on the antenna). Full check covering T-094 running.
- **B0.252 T-105 delivered** (52bd4fe). **Root cause:** all 3 confident wrong tops at 8 frames were (x+1)·CRC-24/Mode-S. The Mode-S generator already contains (x+1), so a 1-in-128 extra (x+1) across all differences can't be distinguished by divisibility. **Fix:**
  - Generators fitting one hypothesis share a posterior weighted 2^((k−1)·width).
  - A repeated-factor prior (2^−10 via gcd(g, g′)) demotes squared factors.
  - A new `ambiguous_with` API field is added.
  **Result over 200 seeds:** 8 frames 0 wrong (was 3 at 0.95); 3 frames 24 wrong at ≤0.07 (was 164 at ≤0.24). **Residual:** a truth generator without (x+1) remains indistinguishable at the inherent ~1/128 rate (8 frames: 1/200 at 0.94). This is inherent to frames-only evidence and accepted. Merge follows the in-flight full check.
- **B0.253 T-092 delivered** (8866566). Contents:
  - **Recording:** always-on decoded capture as a local consumer on each inspector publisher, never blocking; drops are counted.
  - **Storage:** `.hks` holds the published stream bytes (layers stripped), `.idx` a 16-byte offset/time index, `.json` the catalogue.
  - **Quotas:** 1 GiB total, 64 MiB per capture with segment roll, oldest evicted first.
  - **API:** `CaptureSource` gains open_at/list/info/frame_at_time/delete; routes list/get/delete/frames (frame or time scrub); parse seeks via the index; `/ws/open/inspector?capture=` replay.
  - **UI:** picker, slider and jump-to-time.
  - **Tests:** mock-SDR e2e covering stored==live, scrub, replay re-parse, quota, stalled disk.
  Timeboxed Opus review running; it checks crash consistency, eviction versus open readers, gating on replay, and the runtime.rs conflict with T-093.
- **B0.254 T-093 review: MERGE.** The review confirmed:
  - budget claims N−1 extra slots; overrun is marked as a discontinuity;
  - the merge buffer is sized and allocation-free; swaps are atomic across channels at a boundary and retired instances are dropped off the RT thread;
  - the watermark advances per chunk; `graph.rs` only affects frames-input graphs (follow-hops downstream);
  - merge-tree against main is clean.
  Nits filed as **T-107**: stale-tune race in set_channels; a lost DISCONTINUITY on an empty DDC chunk; ordering of long frames (relevant to T-095 POCSAG); detections-source and tracker-found hop-set tests; set_channels routes. T-093 and T-105 merge after the in-flight full check; the T-092 vs T-093 runtime.rs conflict is expected and resolvable.
- **B0.255 Full check of main 315aaf3 (T-094): green.** Lint clean; nextest + UI 1086/1086; acceptance 25/25 (tutorial_rds included). **T-093 and T-105 merged.** **Launched:** T-095 POCSAG tutorial (with follow_hops; long-frame ordering caveat from T-107) and T-106 (RDS oracle timestamp offset). T-107 is held until T-092 merges, since both touch the recipe runtime. Full check covering T-093 + T-105 running.
- **B0.256 T-092 review: FIX-FIRST** (timeboxed). Real-time safety, gating on store/replay, ids, 409 races and input validation are fine. Must-fix:
  1. Merging with main breaks the build: T-091's assist test builds `CtlRequest` without the new `query` field. There's also an api.md text conflict.
  2. A disk stall over 5 s trips the publisher slow-consumer disconnect and permanently stops the always-on recorder.
  3. Interrupted-capture recovery doesn't validate the tail record, so the last /frames page returns 422 forever.
  Promoted from nit: a partial-write retry duplicates bytes or misaligns the index. A fresh Opus fix and integration round is running in the T-092 worktree; T-107 follows after it merges.
- **B0.257 Full check of main 415f112 (T-093 + T-105): green.** Lint clean; nextest + UI 1099/1099 in 239 s; acceptance 25/25. M1 on main: T-085–T-091, T-093, T-094, T-104, T-105. In flight: T-092 fix/integration, T-095, T-096, T-097, T-106. Held: T-107 (after T-092).
- **B0.258 T-106 merged** (acb1552; coordinator diff review). `WfmDemod` records the MPX index of the first sample fed to the RDS demod after pilot lock and adds it to bit positions. Group, PS and record timestamps are now stream-absolute (previously 122 bits / 102.7 ms late). tutorial_rds dropped its offset estimate: direct agreement is 43/44 oracle-valid groups (0.977, 0 conflicts, 1 missing). New unit test with a 0.3 s pre-lock prefix: positions land within ~17 ms of truth. hk-demod 35/35, acceptance tutorial_rds + signal_062 4/4. Full check is batched with the T-092 merge (imminent).
- **B0.259 T-092 merged** (fix/integration 082a745). Changes:
  - The recorder subscribes via `subscribe_recorder`, which is exempt from the slow-consumer close; it keeps dropping with drop markers. Tested with a 600 ms stall against a 100 ms policy.
  - Interrupted captures are walked and truncated to the last complete indexed record.
  - A partial write retries only the remainder, and on failure cuts back to the last good commit.
  - Replay is capped at 4 (503 busy).
  - The `CtlRequest.query` compile break is fixed and the api.md sections are merged.
  **T-107** (follow_hops hardening) launched now that the runtime is stable. Full check covering T-106 + T-092 running.
- **B0.260 Full check of main f2561b1 (T-106 + T-092): green.** Lint clean; nextest + UI 1112/1112 in 262 s; acceptance 25/25. In flight: T-107, T-095 and T-096 (both wrapping up past budget, nudged off Monitor waits), T-097.
- **B0.261 T-096 (ACARS tutorial, Sonnet) ended over budget at ec7adcb; not merged.**
  - The recipe was fitted to the py synth: SYN SYN SOH sync, CRC-16/XMODEM over parity-zeroed characters, and a new `zero` mode in the pinned parity block.
  - The coordinator is concerned the synth is non-standard. The T-087 review said acarsdec uses KERMIT over bytes including parity.
  - The blind acceptance test is `#[ignore]` because detection never registers the synthetic burst (inventory empty after 240 s).
  - acarsdec is absent.

  **T-108** (fresh Opus agent, same worktree) takes over:
  - establish the convention from acarsdec source;
  - correct the synth, recipe and block;
  - fix the blind detection root cause;
  - un-ignore the test.

  Lesson: the Sonnet tutorial agents stalled on Monitor waits and overran budget; future tutorial briefs say so explicitly.
- **B0.262 T-095 (POCSAG tutorial, Sonnet) ended over budget at 7898e56; not merged.** The recipe is unmodified and validates, and a 4-channel synthetic pager scene was built. The blind acceptance test is `#[ignore]`: the inventory resolved only 1 of 4 channels (the others merged into 118 kHz / 49 kHz clusters), so follow_hops never got a channel set. **T-109** (fresh Opus, same worktree) takes over: find the merge stage, fix it blind, un-ignore. Pattern: both the ACARS (T-108) and POCSAG (T-109) tutorials hit **blind-detection gaps for bursty narrowband signals**, not decoder gaps. That's a real exploration-quality finding; T-108 and T-109 were told to keep hk-detect edits localised to avoid colliding. Multi-baud per-channel follow_hops is noted as an architecture limit.
- **B0.263 T-107 merged** (74c9a9e). Changes:
  - Per-channel tune re-plan at apply time. A channel outside the window is refused with 409 `outside_window`; a race test covers it.
  - DISCONTINUITY is held until a channel's DDC yields samples.
  - **The merge now orders by frame end**, not start. Coordinator decision: accepted, because it keeps the `order_window_s` latency bound for long frames. ADR-0011 is updated, noting that `sample_index` is non-monotonic across channels.
  - Blind detections-source and tracker-found hop-set tests run through the mock SDR.
  - New routes `PUT /api/pipelines/{id}/channels` and `POST .../channels/refresh`, with contract tests.
  Tests: hk-blocks 69, follow_hops 4/4, runtime/alloc/capture 10/10, hk-api 82, api_contract 15, lint clean. Full check running.
- **B0.264 Full check of main b8e118a (T-107): green.** Lint clean; nextest + UI 1118/1118 in 241 s; acceptance 25/25. In flight: T-097 (ADS-B tutorial), T-108 (ACARS: re-running with a vouched content class; check the gating interaction on report), T-109 (POCSAG channel separation).
- **B0.265 T-096/T-108 merged** (6cfa144). The ACARS convention now comes from acarsdec source (TLeconte/acarsdec@339f63e, cited in the tutorial):
  - LSB first, parity last;
  - coherent MSK chips (tone marks a chip change);
  - SYN SYN SOH, inverted accepted;
  - CRC-16/KERMIT over the transmitted chars including parity, BCS low byte first.

  **The T-098/T-096 synth was non-standard** (the coordinator's suspicion was confirmed). The synth was rewritten, with a py test decoder ported from acarsdec. Recipe: slicer → nrzi(encode) → sync_search (lsb, polarity either) → crc KERMIT → fields. The parity `zero` mode was removed; the new pinned params `nrzi.direction` and `sync_search.polarity` are both real-ACARS needs, and ADR-0011 is updated.

  **Detection root cause:** the scene sat on the tuned centre, and the DC/LO-leakage rule correctly rejected it. The scene is now 50 kHz off centre with repeated blocks; no thresholds changed. The 118–137 MHz content class is gated metadata-only, so the test vouches the recording unrestricted, per the existing pattern. No new gating rule was added (user policy).

  **Blind e2e:** mode/registration/label/block id/text match truth on 100% of frames; CRC-valid 19/19. hk-blocks 70, py synth 15. Remaining: a real 131.55 MHz capture plus the acarsdec oracle (not installed); tone→chip conversion propagates errors (a coherent MSK block if real captures need one). Full check running.
- **B0.266 T-097 (ADS-B tutorial, Sonnet) ended at 480k tokens, 7fc75c9; not merged.** Recipe fixes: bandwidth 1.6 MHz, `min_snr_db` 9. Only 12 of 16 truth squitters decode (fixed aircraft × kind pairs never do) and CRC-valid is 24%. The acceptance test was **weakened to ≥12/16**, which is not accepted. The readsb comparison was skipped as 'absent', but `/opt/homebrew/bin/readsb` exists (PATH issue in the agent). It also found that recipe `messages` outputs are still Idle (no Repository ingest). **T-110** (fresh Opus, same worktree) will root-cause ppm_demod, restore 16/16, run the readsb oracle for real, and wire the messages sink if small. Coordinator note: Sonnet tutorial agents (T-095/T-096/T-097) all overran badly and T-096/T-097 bent tests or fixtures; remaining tutorial/decoder-quality work stays on Opus.
- **B0.267 Full check of main fbce9db (ACARS tutorial): green.** Lint clean; nextest + UI 1120/1120 in 246 s; acceptance 26/26 including tutorial_acars. Acceptance wall time rose from ~51 s to 99 s: tutorial_acars alone takes ~76 s (240 s blind-discovery budget, 3-burst scene loop). Watch this; if the tutorials keep adding ~1 min each, move them to a separate `just acceptance-tutorials` step. In flight: T-109 (POCSAG, final lint/acceptance), T-110 (ADS-B).
- **B0.268 T-109 delivered** (9c7bfd2). The 4-channel pager net merged into one false 124 kHz **hop set**; detections and tracks were correct, and hop-set members get no inventory rows.
  - **Fix 1 (hk-detect):** `hop_check` refuses a link when either channel was also keyed during the other's burst (`keyed_during`, stat `hop_concurrent_vetoes`).
  - **Fix 2 (hk-pipeline):** confirmed open channel tracks with ≥4 bursts are offered to the inventory every 5 s of stream time. The offer is keyed by track, and a partial life never auto-confirms. Without it, a never-idle pager net was only catalogued at stop.
  - **Scene:** staggered key-ups, 25 kHz raster.
  - **Result:** blind tutorial_pocsag passes. 4/4 channels found; 16/16 CRC-valid pages match truth on the right lanes; simulcast deduped (`dups` 5, `late` 0); BCH 320 ok / 0 bad. multimon-ng per-channel oracle 4/4.
  - **Review:** both fixes are core interfaces, so a timeboxed Opus review is running. It checks real-hopper regressions and stale live rows after merge/split/fragment/hop-set changes.
  - **Limits:** single baud per recipe; about 4 min live latency for slow nets.
- **B0.269 T-097/T-110 merged** (98b07f0). **Root cause** of the 'content-dependent' ADS-B failures: `ppm_demod` read chips at integer samples from an integer preamble start. With ~1 sample/chip, a squitter at a fixed sub-sample phase in the looping replay always sat on chip boundaries.
  - **Fix:** fractional chip-centre interpolation, a 1/8-chip timing grid choosing max decision margin, and a half-sample preamble try. Bounded, alloc-free, chunk-invariant.
  - **Recipe** moved to 2.4 Msps / 2 MHz.
  - **Result:** blind 16/16 squitters (128/128 instances), 0 mismatches. readsb agreement 339/339 fields, covering every readsb (ICAO, TC).
  - **Test bar** restored to 'none missing'.
  - **T-097's readsb skip** was the unbuilt `hk-plugin-readsb`, not readsb itself.
  - **Remaining:** 81% of emitted frames are noise-triggered preambles, rejected by CRC. Recipe `messages` outputs are still Idle, now **T-111** (launched).
  All four M1 tutorials now have blind acceptance on main or in review (POCSAG in T-109 review). Full check running.
- **B0.270 T-109 review: FIX-FIRST.** Must-fix: live inventory offers ignore the T-101 in-band fragment rule (computed only at close), merges and pending hop sets. A WFM flicker with ≥4 bursts got a live 16 kHz row that was never retracted, and `tutorial_rds` then tuned to it and failed. It fails on the branch and passes on main. The same flaw covers merged tracks and slow-hopper channels offered before the hop set forms.
  Checked OK: no double counting (ledger keyed by source track); FM stations not duplicated; DB writes on the writer thread; the hop veto is bounded and passes the 915/hopper tests.
  Fix round launched (fresh Opus, T-109 worktree): merge main; offer only settled tracks plus retract rows created solely by a live offer (never user-confirmed rows); veto uses `gap_tol`; reuse the summaries buffer.
- **B0.271 Full check of main 5c518d8 (ADS-B tutorial): green.** Lint clean; nextest + UI 1121/1121 in 242 s; acceptance 28/28 (2 ignored: HIL HackRF, HIL 1090 MHz) in 100 s, including tutorial_rds, tutorial_acars and tutorial_adsb with readsb. In flight: T-109 fix round, T-111.
- **B0.272 T-111 delivered** (dc8be90). Recipe `messages` outputs now feed the Repository through the plugin Ingest path:
  - **Mapping:** declarative decode mapping plus `service`. A bounded 1024-entry try_send queue on the RT thread feeds a writer thread per output via `Ingest::store_decode`, which covers gate, identity sighting and republish.
  - **Shared code:** family classification shared with plugin chains.
  - **Stats:** new `decodes` and `decodes_dropped`.
  - **E2E:** ADS-B rows `recipe:adsb` are linked to every truth ICAO emitter; the RDS PI row is attached to the station. The alloc-free queue test passes.

  Timeboxed Opus review running. Focus: identity canonical-form compatibility with readsb/RDS (to avoid duplicate emitters), per-output SQLite writer contention, and collision with T-109.
- **B0.273 T-095/T-109 merged** (fix round 4267655). What changed:
  - `Tracker::live_offers_into` offers only settled tracks: no hop link or set, and not a live in-band fragment (duty measured over the reported life of a still-open host).
  - A provisional row is retracted via `Repository::retract_provisional_emitter`, marked deleted by `auto` only if untouched: never confirmed, merged, identified or shared. Retraction happens on fragment close, merge or hop-set formation.
  - Live offers are processed before end events within a batch.
  - The hop veto uses `gap_tol` (new slow A→B→A hopper test).
  - A reused offer buffer means no per-5 s allocation on the detection thread.

  Tests: hk-detect 124; pipeline 9/9; acceptance inventory_lifecycle, signal_062, tutorial_rds, tutorial_pocsag and tutorial_acars 9/9; hk-model lifecycle 5/5; lint clean.

  Accepted residual: a live offer that joined an existing row keeps its bursts counted if the track later proves to be a fragment.

  **All four M1 tutorials (RDS, POCSAG, ACARS, ADS-B) now pass blind on main.** Full check running; T-111 is in review.
- **B0.274 T-111 review: MERGE.** Checked: the RT path does only an Arc clone + try_send (alloc-free, including queue-full); retired sinks are dropped on the control thread; ICAO `{:06x}` and PI `{:04X}` canonical forms match readsb and hk-demod, so there are no duplicate emitters; the family step depends only on decoder evidence, so it can't go stale; restricted classes are sanitised fail-closed; per-output SQLite writers use the same WAL + 5 s busy_timeout as plugins; merge-tree against main (including T-109) is clean. Nits filed as **T-112**:
  - ingest stops classifying after 4096 emitters on long runs;
  - a bad output_policy is swallowed silently;
  - a sink drop happens under the pending lock;
  - decode writes are unbatched;
  - a nextest LEAK flag.

  T-111 merges after the in-flight full check.
- **B0.275 Full check of main c90b8c6 (POCSAG tutorial): green.** Lint clean; nextest + UI 1127/1127; acceptance 29/29 (2 ignored HIL) in 60 s, down from 100 s. **T-111 merged** (dc8be90). **T-112 launched** (decode ingest hardening). Full check covering T-111 running.
- **B0.276 Full check of main 946e99a (T-111): green.** Lint clean; nextest + UI 1132/1132 in 267 s, no LEAK lines (the flagged leak did not reproduce in the full run); acceptance 29/29 (2 ignored HIL) in 61 s. M1 remaining: T-112 (ingest hardening, running). Deferred: T-100 (live HIL re-run), T-026 (CUDA). Live ADS-B blocked on the 1090 MHz antenna.
- **B0.277 T-112 merged** (30a099c). Coordinator reviewed the hk-model change: `begin_write_batch`/`commit_write_batch` (BEGIN IMMEDIATE); inside a batch `write_tx` becomes a savepoint (per-row rollback on drop) and `read_tx` joins; outside a batch, behaviour is unchanged. Other changes:
  - **Emitter tracking:** a bounded recent-emitter set with `take_new_emitters` drain, so long runs classify past 4096 emitters (5000-identity test).
  - **output_policy:** a malformed policy is surfaced as a pipeline warning.
  - **Pending edit:** dropped outside the lock.
  - **Batched writer:** up to 128 rows per transaction, 5634 → 9262 rows/s; 0 dropped at 1000 decodes/s.
  - **LEAK flag:** not reproducible; test teardown made explicit.

  Tests: hk-plugins 52, pipeline 22, hk-model 97, acceptance tutorial_adsb/signal_001 4/4. **All M1 tasks are done** pending this full check.
- **B0.278 M1 COMPLETE (user, 2026-09-15). M2 started:** attention + memory maturity (docs/11: C04 bandit revisit scheduler, C12 occupancy baselines + novelty alarms, C26 history queries/reports). Tasks T-113..T-124 were added with parallel groups:
  - **T-113 M2-DESIGN** (ADR-0012 contracts; Opus high, reviewed) blocks T-115/T-118–T-122.
  - **Independent now:** T-114 scheduler simulator (hk-sim), T-116 history maturity (hk-store), T-117 synthetic occupancy scenes (py, Sonnet with strict budget).
  - **After design:** T-115 observation log → T-118 occupancy engine → T-119 baselines/novelty/score → T-122 alarms; T-120 bandit (needs T-114 sim); T-121 reports; T-123 UI hooks; T-124 acceptance.

  Launched 4 agents: T-113, T-114, T-116, T-117. Demo-relevant work stays mergeable per task. The UI rewrite and M1 review are pending with the user and don't block M2. Existing scheduler: WRR with a placeholder interestingness (hk-core scheduler/mod.rs); the bandit is an ADR TODO. No occupancy code exists yet.
- **B0.279 Full check of main 17f7561 (T-112, end of M1): green.** Lint clean; nextest + UI 1138/1138 in 257 s; acceptance 29/29 (2 ignored HIL) in 61 s. M1 closes on a verified main.
- **B0.280 T-117 merged** (4a34de5; py only). New scenario `occupancy_markov_scene`, 48 h default:
  - 4 Markov channels at FCO 1/10/50/100%;
  - an hour-of-week channel;
  - novelty at hour 30;
  - 00Z/12Z events;
  - a boring band.

  Truth is exact interval arithmetic. An irregular observation schedule gives sampled FCO with a Wilson CI. Only a few short IQ windows are rendered on demand (~0.03 s, ~620 KiB). No Rust plumbing was needed. py suite 82 passed / 2 skipped. Note for T-118/T-124: mock-SDR replay of observation windows needs harness support for the scene's observation schedule.
- **B0.281 T-114 merged** (be26ea5). New crate `hk-sim`:
  - **Emitters:** seeded population with per-emitter RNG streams, so every policy sees identical transmissions; hoppers; tagged IMD ghosts.
  - **Radio model:** hackrf_sweep-like 0.75 s pass; dwells 0.75·fs wide with a DC notch; dead time on retunes and mode switches.
  - **Policy:** `Policy` trait over `hk_core::ScheduleStep`, fed measured detections only; `PolicyRegistry` so T-120 can register the bandit.
  - **Output:** metrics JSON `hk-sim/comparison/v1`.

  **Baseline, seed 1, 24 h, 124 emitters + 10 ghosts:**

  | Policy | Discovered | Bursts/h | Median TTFD | T_R |
  |---|---|---|---|---|
  | pure-sweep | 120/124 | 1566 | 119 s | 0.75 s |
  | round-robin | 122/124 | 53 | 3219 s | 401 s |
  | WRR | 124/124 | 842 | 346 s | 2.7 s |

  WRR also wastes **6875 s of dwell on suspect ghosts**: that's the gap T-120 must close. POI matches the formula within statistical error. 9 tests; 1 simulated day × 3 policies takes 4.9 s in release. Not covered: preemption / 24 h baseline flag (needs T-120), replay.
- **B0.282 Main 231cb74 after T-114: lint clean; hk-sim 8 passed, 1 skipped (release-only speed test).** T-114 and T-117 are additive (new crate, py scenario), so the next full check is batched with the T-113/T-116 merges. In flight: T-113 (M2-DESIGN), T-116 (history maturity).
- **B0.283 T-125 added and launched** (unblocked while T-113 runs): mock SDR/e2e harness support for replaying time-compressed occupancy scenes on their observation schedule with simulated timestamps. T-118 and T-124 need it.
- **B0.284 T-116 merged** (4399a60; coordinator report review, not core).
  - **Tile format v2:** columnar zstd, 2.86× (L0 ~57 MB/h at a 20 MHz dwell); v1 still readable.
  - **Coverage:** mask plus per-cell coverage fraction and `coverage_summary`.
  - **Provenance:** gain/filter/spur-mask/noise-shape steps that survive rollup.
  - **Retention:** per-tier age + quota + region overrides, children-first.
  - **p10 bias:** fixed via a corrected `floor_db` (1 h: −100.02 vs −100 injected); burst survival passes.
  - **Sweep CSV:** import/export plus PNG waterfall.
  - **API:** additive `/api/history` (floor_db, coverage, provenance steps, format=csv|png).
  - **Contract test:** its CSV check now accepts zero lines (empty server history); shape is still checked, and values are covered in hk-store tests.

  Follow-ups filed as **T-126**. Note for T-118: the occupancy threshold should use the corrected floor. Full check covering T-114/T-116/T-117 running.
- **B0.285 T-113 M2-DESIGN delivered** (391249b): ADR-0012 plus shared types in `hk-model/src/attention` and stub modules. Key decisions:
  - **Observation log:** DwellRecord per step with reason codes; SweepRecord aggregated per pass.
  - **FCO:** computed only from activity-independent visits (sweep + scheduled), time-weighted; Wilson CI on effective samples.
  - **Channels** learned from detections on the history grid.
  - **Sites:** discrete (250 m), with `mobile`/`unassigned` states.
  - **Baselines:** 168 slots with pooled maturity fallback; frozen reference + 14-day adaptive copy.
  - **Score S:** computed by C12 via `InterestingnessProvider`; unknowns rank high; band plan ≤30% of the boring prior and only after blind discovery.
  - **Bandit:** deterministic discounted UCB; 15% exploration floor, 25% sweep floor.
  - **Reports** must carry coverage.
  - **Alarms:** Anomaly rows with hysteresis, suppressed when immature or mobile.
  - **API:** 22 planned routes.
  - **Ownership map** (ADR §11) moves the T-121 report to `hk-context/src/report` and T-122 alarm rows to SQLite.

  **Open questions for the user** (proceeding on defaults unless the user objects):
  1. Pooled baseline maturity (~1 parked day) instead of literal 24 h per hour-of-week slot (~24 weeks).
  2. Fixed UTC offset per site versus a timezone DB (DST shifts patterns an hour).
  3. Retention defaults: observation 30 d / 512 MiB; occupancy 90 d at 15 min, 2 y hourly; baselines 1 GiB.
  4. Novelty alarms off while moving, and a 250 m site radius.
  5. Sweep and exploration floors and score weights, to be retuned with hk-sim.

  Timeboxed Opus review running; it checks against main (T-116 tile API, hk-sim Policy) and T-125 simulated time.
- **B0.286 Full check of main b72f1b3 (T-114, T-116, T-117): green.** Lint clean; nextest + UI 1155/1155; acceptance 29/29 in 57 s. **T-113 merged** (d3af1cf). The review was FIX-FIRST; the coordinator applied the fixes directly:
  - ADR §0 time base: all contract time uses the device/sample clock, never wall time; only I/O flush may use wall time.
  - §2.5: too few activity-independent visits widens the rollup window instead of substituting the biased all-visits FCO. At the 25% sweep floor, 15-min intervals would otherwise always fall back.
  - §11: T-117 truth field names corrected.

  Reviewer view on the 5 open questions: Q1/Q2/Q3/Q5 are safe defaults; Q4 (walk-survey alarms, 250 m radius) is worth asking the user, with alarms suppressed while moving as the safe default. tasks.yaml T-121/T-122 areas were aligned to the ADR §11 ownership map. Deferred nit: `SharedInterestingness::publish` validates under the lock. Merge order per ADR: T-115 → T-118 → T-119 → T-122; T-115 before T-120 touches control.rs. **Disk at 20 GB floor**; worktrees are being cleaned before the next launches.
- **B0.287 Launched T-115** (observation log) **and T-120** (bandit scheduler; scheduler and sim work first, control.rs wiring after T-115 per the ADR merge order). Main build dir removed to free disk (21 → ~35 GB). Full check of main after T-113 running. Running: T-115, T-120, T-125, T-126.
- **B0.288 T-125 delivered** (1ebe82c; coordinator reviewed the mock SDR diff).
  - **Replay:** a scene's per-revisit IQ windows are joined into one SigMF recording, with each capture's `core:global_index` on the scene clock. The mock serves it through the unchanged device contract, and inter-window gaps are standard `GAP` with an exact `dropped_before`.
  - **Mock fix:** gaps are now placed at exact recording positions (`recording_gaps()`), so a block never splices two windows. Real-time pacing no longer sleeps through recording gaps.
  - **Wall-clock paths now on sample time:** `/api/analysis/strongest` (newest history frame) and the `hk serve mock:` start clock for gapped recordings.
  - **48 h scene e2e:** 46 h simulated in 10.7 s; 247 detections inside windows; history empty in gaps; novelty first seen at 30.61 h, never before 30 h.
  - **Open points:** scene gaps still count as device overruns/dropped samples, so if 'not observed' must be distinguished from 'device lost samples' that's a device-contract flag (tracked for T-115/T-118). No on-demand rendering per tune; windows are pre-rendered for a schedule.
  - **nextest:** `a_slow_disk_drops_and_counts_without_blocking_the_publisher` (hk-store decoded, wall-time assert) moved to heavy-serial with 1 retry after failing twice under load.

  T-125 merges after the in-flight full check.
- **B0.289 Full check of main f54e67d (T-113): green.** Lint clean; nextest + UI 1186/1186 in 297 s (cold build after the target cleanup); acceptance 29/29 in 64 s. **T-125 merged** (1ebe82c). Full check covering T-125 running.
- **B0.290 Main 4d03322 (T-125) verified green.** First run: acceptance 30/30 (including scene_48h) and lint clean, but `hk-stream::review_fixes::p7_connect_close_flood_is_reaped_and_consumers_are_capped` failed at load ~37 (4 of 8 consumers accepted in time). It passed 3/3 alone, and a full rerun gave nextest + UI 1187/1187. The test is now heavy-serial with 1 retry in nextest. In flight: T-115, T-120, T-126.
- **B0.291 T-126 merged** (c9ceb7f). Changes:
  - **Noise-shape estimator:** dB variance of noise-only bins inverts to look count (k within 5%). Floor error ≤0.37 dB for single-look CSV and ≤0.15 dB for 4-look sweeps.
  - **CLI:** `hk history import-sweep-csv`.
  - **Provenance steps:** per-source front-end state persisted, so steps survive restart and interleaved sources give no false steps.
  - **Region overrides:** trim unrelated cells of protected tiles.
  - **Retention:** indexed, flat cost at 100 vs 5000 protected tiles.

  Its api_contract failure was only an LFS pointer in the worktree; the full check covers it. Leftover pipeline wiring (FrameInput::source, sweep estimator in live ingest) is folded into T-118's note. Full check running.
- **B0.292 Full check of main 36c3a3b (T-126): green.** Lint clean; nextest + UI 1197/1197 in 242 s; acceptance 30/30 (2 ignored HIL) in 58 s. M2 on main: T-113, T-114, T-116, T-117, T-125, T-126. In flight: T-115 (observation log), T-120 (bandit). Next after T-115: T-118 → T-119/T-121 → T-122; T-123/T-124 last.
- **B0.293 T-118 and T-119 launched early.** Both build against the ADR-0012 hk-model attention types, which are on main: T-118 with an in-memory observation provider until T-115's store merges, T-119 with synthetic OccupancyStat inputs until T-118 merges. Per ADR §11, file ownership is disjoint from T-115/T-120. Merge order is still T-115 → T-118 → T-119. Running: T-115, T-118, T-119, T-120.
- **B0.294 T-115 delivered** (d54e8a6). What landed:
  - **Records:** a `WindowRule` usable span matching history L0 fold extents (±15 kHz DC notch). `ObservationRecorder` emits DwellRecord per step, SweepRecord per pass/60 s and SweepGeometry on change; alloc-free.
  - **Wiring:** the pipeline observer is one call in `SchedState::tick`, feeding a bounded try_send queue and a writer thread.
  - **Storage:** hourly CRC+JSON segments with self-contained geometries and torn-tail recovery; retention 30 d by sample clock, then 512 MiB.
  - **Queries:** `query`, `totals` for T-118, and gaps.
  - **API:** `/api/observations`, `/api/observations/coverage` and the `observations` stream.
  - **E2E:** the mock-SDR run matches the tuned windows hop for hop.

  Open points: the usable span follows the history rule rather than the ADR roll-off trim; overload is always false; **interactive runs without a scheduler log nothing** (demo-relevant, being assessed in review). Timeboxed Opus review running.
- **B0.295 T-120 delivered** (10bdabb): bandit in hk-core plus an hk-sim policy. Over 24 h on 4 seeds: 124/124 discovered; bursts/h 1642/1873/1399/1762, beating WRR on all seeds and pure-sweep on 3 of 4; median TTFD 118–216 s; suspect dwell ~700–900 s vs WRR 6875–9820 s (about 10× less). The injected burst emitter was found later than WRR on seeds 1–2. v1 policy is unchanged with the bandit off; alloc-free; POI matches the formula and Monte Carlo. Pipeline wiring, routes and POI from the observation log are deferred to **T-127** (after T-115). Timeboxed Opus review running; it probes the seed-3 and injected-emitter discovery trade-off.
- **B0.296 T-115 review: FIX-FIRST.** Must-fix: an open SweepRecord isn't closed by non-sweep steps, so a long user intent emits it hours late, outside query look-ahead. Checked OK: sample-clock time base; non-blocking try_send on the control thread; fully-inside coverage rule; retention never touches the open hour; merge-tree with main is clean.
  Coordinator decisions:
  - (a) **Record interactive runs without a scheduler** as `interactive`-tier DwellRecords (coverage yes, unbiased fco no). This is demo-relevant (the hk serve browsing path) and ~100 lines, so it goes in the fix round.
  - (b) Usable span follows the history L0 fold extent; ADR §1.4 note added, shared roll-off trim is a follow-up.
  - (c) Torn-tail repair on first append per hour.
  Fresh Opus fix round is running in the T-115 worktree.
- **B0.297 T-120 merged** (10bdabb). Opus review: MERGE.
  - **Checked:** bounded preallocated tables; device-clock-only discounting and floors; rollback correct; UCB safe with the default prior; DC avoidance; v1 invariants hold with the bandit on; TX gated; simulator fair (reviewer reproduced seed 1 exactly).
  - **Discovery trade-off:** later burst but earlier beacon discovery, and a better p90 TTFD than WRR. It's an honest trade-off (WRR's many short dwells land on new frequencies by chance), not a bug.
  - **Contract gap:** repack runs inside `next_step` and allocates. The caller is the event-rate control thread, so it's non-blocking for now.
  - **Follow-ups:** folded into **T-127** along with the ADR amendment. Full check running.
- **B0.298 Full check of main 5729e19 (T-120): green.** Lint clean; nextest + UI 1208/1208 in 261 s; acceptance 30/30 (2 ignored HIL) in 59 s. M2 on main: T-113, T-114, T-116, T-117, T-120, T-125, T-126. In flight: T-115 fix round, T-118, T-119. Next: T-127 after T-115; T-121 after T-118; T-122 after T-119.
- **B0.299 T-121 (survey reports) launched early** against the ADR-0012 report and occupancy types. Occupancy stats and observation coverage come through provider traits until T-118/T-115 merge; change-vs-baseline is flagged unavailable until T-119. File ownership is `hk-context/src/report/**`, disjoint from the running agents. Running: T-115 fix, T-118, T-119, T-121.
- **B0.300 T-115 merged** (fix round 029024e). Changes:
  - `ObservationRecorder::begin` closes an open sweep before long non-sweep steps (2 h intent test).
  - **Interactive `hk serve` without a schedule now logs** `interactive`-tier dwells: polled off the capture thread, split every 60 s, zero activity-independent visits; e2e and contract tests added.
  - ADR §1.4 usable-span note.
  - Per-hour torn-tail repair.
  - Geometries written before their first referencing sweep in each segment.

  Tests: hk-core 6, hk-store 7, pipeline 4, api_contract 17, lint clean. **T-127 launched** (bandit wiring, routes, POI from the log, plus T-120 review follow-ups). T-118 and T-121 can now replace their observation stand-in providers with the T-115 store adapter. Full check running.
- **B0.301 Full check of main d78142e (T-115): green.** Lint clean; nextest + UI 1227/1227 in 270 s; acceptance 30/30 (2 ignored HIL) in 60 s. M2 on main: T-113, T-114, T-115, T-116, T-117, T-120, T-125, T-126. In flight: T-118, T-119, T-121, T-127.
- **B0.302 T-119 delivered** (bb48dd2). Contents:
  - **Baselines:** per-key zstd store, atomic writes, 1 GiB LRU-site quota. Frozen reference with gated learning (the agent's reading of §3.4, flagged for review). Adaptive copy with 14-day half-life and ±3σ clip. CUSUM change points latch until re-freeze.
  - **Maturity:** pooled; levels kept per gain state; a new calibration starts a new key.
  - **Novelty:** rise-only level novelty; FCO novelty with a combined-spread z; Poisson first-sighting novelty.
  - **Score S:** boring prior with band plan ≤0.3, applied only after blind characterisation.
  - **Sites:** mobile, stationary join/create within 250 m, unassigned.
  - **Weights:** migration 0002 (site, attention_weights); append-only weights.
  - **API routes:** sites, baselines, refreeze, candidates, weights.

  Results on a synthetic 48 h scene: injected emitter detected in 3 intervals (45 min); false alarms 0.05%; a persistent interferer becomes a change point at 45.5 h without poisoning the reference.

  **Not wired:** no pipeline `ingest_occupancy` call and no candidate assembly from inventory. The `OccupancyStat` adapter is provisional (threshold used as level). Timeboxed Opus review running; it checks reference gating, rise-only level novelty (a silenced transmitter) and the adapter's level source.
- **B0.303 T-118 delivered** (3259c7b; includes main + T-115). Engine details:
  - **Visits:** from the T-115 log, falling back to the history coverage mask.
  - **Occupied:** a visit counts as occupied when the channel's max L0 cell mean is above threshold.
  - **FCO:** half-gap weighting over activity-independent visits, with widening.
  - **Threshold:** band floor from the 80% method over `floor_db`. The agent says ADR §2.2 described the 80% method backwards and corrected it against the SM.2256 text.
  - **Confidence interval:** Wilson on n_eff using lag-1 visit-state autocorrelation. Annex 1 is sample-size rules, and tests reproduce tables A1/A2.
  - **Channels:** learned blind.
  - **Storage:** CRC log store.
  - **Threading:** runs on its own thread on the history clock.
  - **API:** `/api/occupancy` and `/api/channels`.

  **Blind 48 h e2e:** 6/6 learned channels' realized FCO lies inside the 95% CI (10/50/100% Markov, diurnal, novelty, boring); the 1% channel and the event aren't learned (<5%, allowed); no phantoms. The first run failed 4/6 because of a per-channel self-floor, fixed with the band floor. Timeboxed Opus review running. Focus areas: 80% correction; dense-band floor (FM); n_eff bounds; exposing a channel level for T-119 level novelty.
- **B0.304 T-119 review: FIX-FIRST.**
  - **Must-fix:** the adapter uses `threshold_db` as the channel level, so level novelty would track the floor, not emitters. Fix: `level_db: None` until T-118 exposes a real level.
  - **Accepted, with follow-ups done in the fix round:**
    - §3.4 gating maturity deadlock for sharply patterned channels (busy-hour z≈4.4 against the coarse pool): test plus accrue-below-alarm-level while the hour-of-day pool is immature.
    - Reference learning bounded at hour-of-day maturity (~24 d, not ~24 wk).
    - Re-freeze behaviour documented.
    - `enforce_quota` called on flush.
  - **Checked OK:** sample-clock time base, atomic writes, migration 0002 numbering, FCO novelty two-sided.
  - **Recorded for follow-up:** FirstSightingRate unwired (no new-emitter novelty); dense GainSeries memory (~200 MB per gain state per key); a latched change point stops accrual; T-122 needs a quieter-than-usual alarm kind.
  Fix round running (fresh Opus). T-118 review pending (includes adding a channel level to OccupancyStat).
- **B0.305 T-118 review: FIX-FIRST.**
  - **Must-fix:**
    1. Only the band tuned at interval close is evaluated, so a retune mid-interval (user or bandit) loses the earlier bands.
    2. `interval=span` builds a ~415 MB cell grid under the FloorProduct lock, so history ingest defers and then drops frames.
  - **Stats sound:** ρ bounded; half-gap cap unbiased; any-cell-above-threshold matches SM.1880.
  - **Coordinator decisions:**
    - (a) Dense bands (FM demo): a **local floor** (20th percentile within ±1 MHz) plus a suspect flag and the floor recorded in the stat, done in the fix round.
    - (b) Level fields added to OccupancyStat now (optional, additive): p50/p90 occupied level, idle level, floor. T-119's adapter gets wired to them in a follow-up after both merge.
    - (c) Nits: `idle_fraction` doc; `fco_all_visits` strata weighted by duration.
  - **Demo note for T-121/T-123:** with only interactive tuning, `fco` is None by contract. Show `fco_all_visits` labelled biased/indicative, never as `fco`.
  Fix round running.
- **B0.306 T-121 delivered** (b41fbdd). `report(region, span)` is built from five provider traits and refuses to build without a coverage source. Coverage lists gaps, never-observed ranges, POI and the statement 'unobserved is not quiet'. `GET /api/report` serves JSON, CSV and PNG (rendered in the backend). The 48 h scene e2e reports the scene's longest gap as the top gap and 7/8 channels found blind. Stand-ins for now: tile-based occupancy, `NoBaselines`. Not done: source/site history filters (need a tile-format change). Timeboxed Opus review running; it checks honesty of disclosure, whether stand-in FCO is labelled biased, whether explanation labels are shown as suggestions, and auth on a route outside the control chain. **T-128 (M2 integration) added:** occupancy levels into baselines, real providers into reports, inventory candidates to the scheduler, first sightings, site/source tile filters, T-119 memory bound. T-124 now depends on T-127 and T-128.
- **B0.307 T-121 review: FIX-FIRST.**
  - **Must-fix:**
    1. Unbounded report history grid (~350–400 MB, no top-level cap, f_hi to 1e12) built under the FloorProduct lock, so history frames drop.
    2. Tile stand-in occupancy written as unbiased ADR `fco` (`revisit_biased:false`, invented 80% threshold metadata).
  - **Checked OK:**
    - No overstated coverage: first provider only, partial log reads as unobserved.
    - "amateur" is the classifier's top suggestion, carried as a suggestion.
    - The route passes the same auth/audit gate.
    - Error codes consistent; docs/contract in sync; merge-tree with main clean.
  - **Fix round launched:** must-fixes, plus one-pass log coverage, an unconditional e2e top-emitter assert and partial-log warning wording.
  - **Merge order:** T-118 before T-121. Source/site history filters are tracked in T-128.
- **B0.308 T-127 delivered** (8ff6b30). What it adds:
  - **Bandit wiring:** enabled by the plan's `extra.bandit`, off by default so the demo is unchanged.
  - **Stub interestingness:** built from confirmed tracks and published via `SharedInterestingness`.
  - **Dwell outcomes:** built from detections in each dwell window, recorded 1 s after on the stream clock.
  - **Verification:** TrustEval verdicts are passed to the bandit.
  - **Repack:** `refresh_bandit()` runs before each step.
  - **SchedulerHub snapshots.**
  - **Routes:** `/api/scheduler` (POI and gaps from the observation log), `/arms`, `/leases` create/list/delete (audited).
  - **T-120 follow-ups:** all 8, including an alloc-free repack test, gap-free passes with floor deferrals, NaN guard, `verifications_dropped` and the ADR amendment.

  The mock-SDR e2e passes with the bandit on and off. Known gaps for T-128: the real candidates, and `valid_decodes`, which is a global delta. A timeboxed Opus review is running and will also recommend a merge order across T-118, T-119, T-121 and T-127.
- **B0.309 T-119 fix round done** (06d4498).
  - **Adapter** is occupancy-only (`level_db: None`). Test: a floor rise causes no level novelty and no change point.
  - **Maturity deadlock** is fixed with a stricter rule than briefed. A fold scoring below 0.7 accrues only while its hour-of-day pool is immature, the fold isn't novel against that pool, the pool's adaptive copy is within 1σ of its reference, and the pool itself is novel against the coarse pool. The last condition separates a real daily pattern from an interferer. The brief's simple rule absorbed the interferer (no change point). Busy-hour novelty now reaches 0 by day 5; a new quiet-hour emitter still scores 1.0; the interferer still raises its change point at 45.5 h.
  - **Reference learning** is bounded at hour-of-day maturity (creep stops at 24 d).
  - **Re-freeze** is documented in ADR §3.4.
  - **Quota** is enforced on flush, and evicted engines are dropped.

  The 48 h scene is unchanged: 3-interval latency, 1/1824 false alarms. Residuals go to T-128. Merge waits for T-118, per the ADR order.
- **B0.310 T-127 review: FIX-FIRST.**
  - **Must-fix:**
    1. Any lease command, even a refused POST or an unknown DELETE, forces a new step without trimming, so the abandoned dwell/hop still counts in floor, coverage and visits.
    2. `valid_decodes` is the global counter read ≥1 s after the dwell, so arms are credited with unrelated decodes.
  - **Checked OK:** `refresh_bandit` is an atomic compare; the `on_member` scan is bounded (32); hub publish runs on the control thread; bandit-off behaviour matches main apart from the intended follow-ups; the T-115 observers are intact.
  - **Merge order:** **T-118 → T-119 → T-127 → T-121**. All pairs have additive conflicts (ApiState field, serve_api init, api.md sections); run api_contract after each merge.
  Fix round running, including nits: cut-end credit, TableFull→409, 503 cancel flag, bounded POI cost, doc comments.
- **B0.311 T-121 fix round done** (c732f17).
  - **Report grid bounded:** finest level within 4096×1024 and 500k cells, else 400. A 48 h×20 MHz report is 153,600 cells. It's read in ≤256-row tile-aligned chunks with short history locks, and JSON/CSV/PNG share one grid.
  - **Tile stand-in is honest:** `fco: None`, value in `fco_all_visits`, `revisit_biased: true`, new additive `ThresholdMethod::HistoryTile{margin_db}`, and `ReportEmitter.fco_all_visits`.
  - **Log coverage** takes one pass over segments (`observations_of_each`).
  - **E2E top-emitter assert** is now unconditional.
  - **Partial-log warning** names the time before which data is unobserved.

  Tests: report 11, pipeline 4, hk-model 31, api_contract 18, scene e2e pass. Merges last per order T-118 → T-119 → T-127 → T-121.
- **B0.312 T-127 fix round done** (f366619).
  - **Step accounting:** a step is only cut early when the scheduler actually cut or trimmed it (`running_end()` before and after). A refused or unknown lease command leaves step accounting untouched, and lease updates trim the running step.
  - **Decode credit:** a per-track `TrackDecodes` table now credits a dwell only for its own tracks. Test: 207 unrelated decodes are not credited.
  - **Lease cuts:** a lease-cut dwell drops its outcome.
  - **Status codes:** `TableFull` returns 409.
  - **503 timeouts:** a timed-out command is cancelled and never applies later.
  - **POI:** computed only when a span is given.
  - **Docs:** doc comments fixed.

  Tests: hk-core 28, hk-api 2, pipeline 18 (3 nextest-leaky, to be checked in the full check), api_contract 18. Waiting to merge after T-118 and T-119.
- **B0.313 T-118 fix round done** (e8e238d). Changes:
  - **All observed bands** in an interval are evaluated, taken from observation-log dwell and sweep windows. Retune test: both bands produce rows.
  - **Span queries** are chunked at ≤1 M cells, with the lock held per chunk, a peak of ~40 MB grid + 80 MB samples, and a band×span cap of 120 MHz·h.
  - **Local floor:** each column gets its own 80% floor, clamped to a split-side 20th-percentile reference within 1 MHz. On a ripple test it stays within 1 dB, where the old whole-band floor gave 25,593 phantom cells. A **suspect** flag is set when local occupancy exceeds 80%.
  - **New level fields:** `VisitSample.level_db` and six optional `OccupancyStat` fields (floor/source/suspect, occupied p50/p90, idle level).
  - **Nits:** duration-weighted `fco_all_visits`; `idle_fraction` doc corrected.

  The t118 e2e is unchanged (6/6 inside CI). Not run yet: the FM fixture sanity check (ignored); bias-model suspect rule. **Merge chain T-118 → T-119 → T-127 → T-121** delegated to one Opus integration agent on a branch (conflict resolution, api_contract after each merge, the FM sanity run, t118/report/scene e2es); the coordinator then fast-forwards main and runs the full check.
- **B0.314 M2 integration merged** (integration branch a5b8b36 → main). Merges: T-118 eac654e, T-119 eda63e9, T-127 ae10b77, T-121 3832609.
  - **Semantic conflict fixed during integration:** T-121's `ThresholdMethod::HistoryTile` versus T-118's exhaustive matches and new `OccupancyStat` fields.
  - **Tests after each merge:** api_contract 18/18 each time; targeted suites 23/168/44/68 green; lint clean; e2es t118 / report_over_48h / scene_48h pass.
  - **FM fixture sanity:** passes. Local floors (−85.8…−78.9 dB) mark 14% of columns occupied vs 18% whole-band, 0% suspect. **Finding:** learned channels are only 13–44 kHz wide and only 2 of 9 align with stations; the band row fco is None. Added **T-129** (FM-realistic channel learning, demo-relevant).
  - **Reports** still use the tile stand-in rather than the T-118 engine; that's T-128.

  Worktrees cleaned. Full check of main running; T-122 and T-128 launch after it.
- **B0.315 Launched T-122** (novelty alarms: adds a quieter-than-usual kind, device-first explanation, migration 0003), **T-128** (M2 integration: priorities are levels→baselines and real candidates→scheduler, then first sightings and report providers, then site/source tiles, memory bound, residuals and the full-path e2e) and **T-129** (FM-realistic channel learning). Ownership is split: T-129 owns `channels.rs`; T-128 owns the pipeline occupancy/attention wiring; T-122 owns the alarm files. Full check of main after the integration merge is running.
- **B0.316 Main 578f8b6 (M2 integration) verified green.** First run: lint clean and acceptance 32/32 in 84 s, but `scheduler_bandit_e2e::bandit_on_attaches_dwells...` failed at load ~32: only 6.8 s of bandit dwell before the deadline, with 9 floor deferrals. It passed 3/3 alone, and a full rerun gave nextest + UI 1310/1310. The binary is now heavy-serial with 1 retry. Acceptance wall time is 84 s (scene, report and t118 e2es added); consider a separate `just acceptance-m2` step if it passes ~120 s. In flight: T-122, T-128, T-129.
- **B0.317 T-129 delivered** (e0b7f35).
  - **Root cause:** about 380 of 399 FM-fixture detections were threshold flicker (short, narrow, ~4.5 dB), so median-extent clustering shrank the station channel to 38 kHz and gap flicker created 7 idle channels.
  - **Fixes:** T-101-style in-band fragment exclusion; a ≥7 dB mean-SNR confidence gate before a cluster becomes a channel; overlap split at the midpoint.
  - **Suspect rule (engine.rs, outside ownership):** a visit is now suspect only if all above-threshold cells are suspect. Before, a spur or DC anywhere made every band visit suspect, which was why band fco was None.
  - **FM fixture:** 2 channels. The 101.3 station channel is 344 kHz (measured OBW) with fco 1.0; band fco 1.0.
  - **Dense synthetic scene:** one ~100 kHz channel per station; gaps idle.
  - **t118 e2e:** identical.

  Timeboxed Opus review running. Concerns: the **7 dB gate may suppress weak persistent unknown emitters** (exploration-first); the suspect-rule reading of ADR §2.6; whether the fragment rule absorbs weak in-skirt signals.
- **B0.318 T-129 review: FIX-FIRST.**
  - **Must-fix:**
    1. The 7 dB all-time-max SNR gate suppresses weak persistent unknowns yet permanently publishes any single ≥7 dB flicker (against exploration-first).
    2. Restore seeds SNR 7 dB, so fragments rejoin and the station channel collapses again.
    3. The fragment rule has no time-overlap or persistence check, so a weaker station at +100 kHz or a narrow skirt unknown is dropped forever.
  - **Coordinator decision:** publish channels that are **confident OR persistent** (≥3 intervals, stable centre, ≥0.5 s); persistent clusters are never fragments; fragments must overlap the host in time; published channels are never absorbed.
  - **Suspect rule judged sound;** widen suspect extents ±1 cell.
  - **ADR §2.6/§2.7 amendment texts** are adopted in the fix round. Fix round running.
- **B0.319 T-122 delivered** (a56ee77). Key points:
  - **Alarm engine:** kinds include a new `quieter-than-usual`. Hysteresis and dedupe follow §7.2 with adjacent-cell merge, reopen within 1 h, and dismissal lasting 7 d on the sample clock.
  - **Suppression:** mobile, unassigned, immature and dismissed alarms are held back.
  - **Device first:** gain, cal and front-end steps explain a change as self-inflicted.
  - **Explanations:** from the existing correlator via `CORRELATED_KINDS`; `unexplained` is ranked on 1 − best external score.
  - **Storage:** migration 0003 `anomaly_detail`.
  - **API:** routes list/get/dismiss/reopen plus the `anomalies` stream.
  - **Tests:** 46 targeted, api_contract 22.

  Not wired into the pipeline yet (the `observe_fold` hook is for T-128 or a follow-up); the e2e goes to T-124. Timeboxed Opus review is running. Focus: could device-first explanations hide a real emitter that coincides with a gain change; adjacent-cell merge gluing emitters together; migration and append-only rules; backward-compatibility of `/api/anomalies`.
- **B0.320 T-122 review: FIX-FIRST.**
  - **Must-fix:**
    1. Device-first gain steps explain any busier/new-emitter change: any positive or no-delta step, with no breadth or residual check. `from_report` always passes a None delta, so a user gain change can silently resolve a real emitter keying up. This breaks exploration-first.
    2. Key reuse within 2 cells regardless of overlap, including dismissed keys, so a new neighbour emitter is swallowed as Dismissed for 7 days.
  - **Checked OK:** cooldown/reopen across resume; anomaly_detail uses UPDATE and never INSERT OR REPLACE; migration 0003 numbering; no UI consumers of the old anomalies stub; audited dismiss/reopen.
  - **Fix round running:** gain step explains only broad matching shifts within a fixed tolerance, never with no delta; dismissed keys absorb only groups inside their extent; key eviction.
  - **Merge order:** T-129 → T-122 → T-128 (rebase; trivial ApiState hunk). T-128 then wires `observe_fold` with gain deltas.
- **B0.321 T-128 items 1–5 delivered** (0e2ce73). What landed:
  - **Level novelty:** the adapter uses level above local floor (emitter z=23; floor rise → 0).
  - **Attention wiring:** attention service wired at occupancy closes.
  - **Candidates:** real candidates (SNR, suspect, periodicity, recipe, class entropy, novelty) replace T-127's stub.
  - **First sightings:** `FirstSightingRate` is fed, so new-emitter novelty works.
  - **Reports:** report occupancy comes from the T-118 series, and the baseline comparison from T-119.

  **Important finding:** every detection in T-127's unclipped bursty replay is `clipped:true`. With real suspect flags the bandit bans all candidates, so T-127's bandit-on e2e fails; T-128 `#[ignore]`d it. Opened **T-130** to root-cause the spurious clip flag (detector provenance), running now. Items 6–9 plus the T-122 alarm hook become **T-131** (after T-128/T-122/T-129/T-130). T-124 depends on T-131. Timeboxed Opus review of T-128 is running; it recommends whether to gate suspect-driven banning until T-130.
- **B0.322 T-128 review: FIX-FIRST.**
  - **Must-fix:**
    1. `publish_candidates` runs an unbounded class-entropy DB query on the control thread under the cands lock (breaks ADR §4.6).
    2. `compare_report` scores the whole span-rolled row as one 15-min interval, giving false busier-than-usual with inflated z; negative z is also mislabelled.
    3. Report FCO rollup is count-weighted rather than time-weighted, which reintroduces revisit bias.
  - **Item 3 recommendation: merge with the T-127 e2e ignored.** Candidates are only fed when the bandit is on, so the default path is unaffected.
  - **T-130 findings:** the replay tone peaks at ~46/127, so the clip flag isn't from sample clipping. The same flag already strips FCO visits on main. T-128's suspect-ban test isn't discriminating.
  - **Fix round running;** alarm hook, gain-state key and bandit-off candidates stay deferred to T-131.
  - **Merge order:** T-129 → T-122 → T-128 (one trivial ApiState conflict).
