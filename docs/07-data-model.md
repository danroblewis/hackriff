# 07 — Core data model

*Architecture planning, Phase 2. Drafted 2026-09-13. Status: **PROVISIONAL** — the object shapes are settled enough to build the first slice on, but the storage-engine choices (§3) and a few ownership items inherited from [docs/06 §5](06-capability-map.md) are finalised by Phase 3 ADRs. Field lists are illustrative, not a frozen schema; the schema ADR pins types and indexes.*

This is the domain model everything hangs off. It follows two rules taken straight from the research:

- **Separate measurement from interpretation** ([docs/04 §11.2](04-radio-engineering-and-signals-analysis.md)). Store raw measurements (frames, detections, IQ) with provenance, and re-run classifiers and decoders over them as those improve. Measurements are immutable; interpretations are versioned and reversible.
- **Everything carries confidence and provenance.** A detection without its gain state, overload flags, calibration and spur mask is untrustworthy on an 8-bit front end (CLAUDE.md key findings; [docs/04 §10](04-radio-engineering-and-signals-analysis.md)).

The objects map onto the capabilities in [docs/06](06-capability-map.md); each object below names the capabilities that write and read it.

## 1. Object map

```
                         ScanPlan ──(drives)──► Survey (a run)
                                                   │ contains
                 ┌─────────────────────────────────┼───────────────────────────────┐
                 ▼                                  ▼                                ▼
           SweepFrame                        SpectrumFrame                     Provenance ◄─ CalibrationState
             │  (survey rows)                  │ (dwell PSD/persistence/SK)      ▲            SpurMask
             └──────────┬─────────────────────┘                                 │ (every frame/detection/recording
                        ▼                                                        │  references one)
                  SpectrumTile  (compressed multi-resolution history)            │
                        │                                                        │
                        ▼                                                        │
                   Detection ──────► Track ──────► Emitter (inventory entry) ◄───┘
                        │              │             │  ▲          ▲
                        │              │             │  │          │
                   Recording (SigMF)   │        Demodulation   Anomaly ──► Explanation ◄── ExternalEvent
                        │              │             │             ▲            ▲
                   Annotation ◄────────┴─────────────┤          (C08/C12/C27)  (C29 feeds)
                                                     ▼
                                              Decode / Message ──► Bitstream ──► (external consumers, C24)
```

Two loops the arrows flatten: Emitter ↔ Explanation ↔ ExternalEvent (an emitter can be a cause or a subject), and Emitter ↔ Decode (a valid decode names the emitter, and the emitter routes the decoder).

## 2. The objects

Each entry: what it is, **identity & lifecycle**, key relationships, **retention & size** on a disk-limited device, and **how tests assert on it**. Capabilities that own it are in brackets.

### 2.1 ScanPlan  [C04]
Declarative statement of what to watch: a list of regions `(f_lo, f_hi, priority)`, sweep-vs-dwell policy, per-band gain tables and filter/antenna-port map, revisit targets, and a schedule (cron-like or continuous). The user's "peruse/automate" intent in machine form.
- **Identity & lifecycle:** `plan_id` (UUID) + human name; **versioned** — editing creates a new version, and each Survey records the version it ran, so history is reproducible. Never deleted, superseded.
- **Relationships:** referenced by Survey; read by the attention scheduler (C04).
- **Retention & size:** tiny (KB); keep all versions.
- **Tests:** load a plan, assert the scheduler emits the expected tune sequence and dwell budget for a synthetic clock; golden-file the schedule.

### 2.2 Survey  [C02, C03, C04]
One execution of scanning over a time span under a ScanPlan version. The container that scopes frames, detections and recordings for provenance and for "what did this run see".
- **Identity & lifecycle:** `survey_id` (UUIDv7, time-sortable); states `open → closed` (or `aborted`); a continuous background survey is one long-lived Survey rolled over daily.
- **Relationships:** references ScanPlan version + device; parents SweepFrame, SpectrumFrame, Detection, Recording.
- **Retention & size:** metadata only (KB per run); the run's frames age out under the history policy.
- **Tests:** replay a fixture through a Survey; assert the run summary (counts, spans, dropped-sample tally) matches expected.

### 2.3 SweepFrame  [C02]
One wideband survey observation: `t`, `f_lo`, `f_hi`, `bin_width`, `power[]` (dBFS, or dBm if calibrated), `provenance_ref`. The output of the firmware sweep, 5 MHz slices stitched.
- **Identity & lifecycle:** `(survey_id, seq)`; **ephemeral individually** — consumed by detection and folded into SpectrumTiles, then dropped at raw resolution within hours.
- **Relationships:** aggregated into SpectrumTile; feeds Detection and NoiseFloor.
- **Retention & size:** raw is ~0.5 MB/s at 4096 bins × 30 rows/s; kept full-res for a short recent window only (§3).
- **Tests:** feed a synthetic sweep with known tones; assert bins/power within tolerance and provenance attached.

### 2.4 SpectrumFrame  [C07]
A dwell-window frame at higher resolution: `t`, `f_center`, `span`, `bins`, `psd[]`, `persistence[]` (DPX histogram), `sk[]` (spectral kurtosis), `provenance_ref`. May carry two resolutions in parallel (burst vs narrow-carrier, [docs/04 §3.5](04-radio-engineering-and-signals-analysis.md)).
- **Identity & lifecycle:** `(survey_id, seq)`; ephemeral like SweepFrame; the decimated "zoom" stream is produced by the channelizer (C11) and FFT'd here (docs/06 §5).
- **Relationships:** feeds Detection, NoiseFloor, radiometry; folds into SpectrumTile.
- **Retention & size:** larger per second than sweep; only kept raw while dwelling and briefly after.
- **Tests:** synthetic burst in-window; assert persistence reveals it and SK flags it as non-noise.

### 2.5 SpectrumTile  [C26]
The persisted, compressed, multi-resolution history derived from Sweep/SpectrumFrames: `(f_lo, f_hi, t_start, t_end, level)` with per-bin `max`, `mean`, and a percentile (e.g. p10 for floor). This is what "region over time" reads — not the raw frames.
- **Identity & lifecycle:** `tile_id` keyed by `(level, f-block, t-block)`; **downsampled as it ages** — full-res tiles for recent hours, coarser levels for days/weeks/months, oldest expired by quota.
- **Relationships:** derived from frames; read by occupancy-baseline (C12), history queries, and the region-over-time view (C39).
- **Retention & size:** the dominant history budget; a pyramid keeps it bounded (§3.2). Target a fixed rolling budget (e.g. 5–20 GB, set in config).
- **Tests:** write frames, roll the pyramid, assert a region/time query returns the expected max-hold and that downsampling preserves peak occupancy within tolerance.

### 2.6 Provenance  [C01, referenced everywhere]
The trust record attached to every SweepFrame, SpectrumFrame, Detection and Recording: source `device_id`, tune (`f`, `fs`, LNA/VGA/amp gains), sticky `overload` flag, `quantisation_limited` (noise floor within 3 dB of the ADC quantisation floor; added from spike S4, 2026-09-13), `noise_sigma_lsb` (**ADC fill**, *added T-625*), temperature, active filter/antenna port (Opera Cake), `bias_tee` (*added T-325*), clock source + lock, `calibration_state_ref`, `spur_mask_ref`, and the timestamp method + error budget.
- **`bias_tee` is three-valued** (`unknown` / `off` / `on`), never a bool: a device that cannot report its bias tee, a replayed recording whose file carries no such field, and a device reporting off are three different facts. "Nothing said" is never "off" — a bias tee left on into a passive or DC-shorted port is a hardware hazard, and an active antenna's LNA shifts the noise floor, so it is also a measurement confound. It is device-local context like gain state, so it is read from the device at the source layer (T-259). Omitted from the JSON form when unknown, so rows written before T-325 read as unknown and keep their dedup hash.
- **`noise_sigma_lsb` is the ADC fill, and an absent one is `under_filled`** (*T-625*, ADR-0015 §13.3). Per-component noise σ in ADC LSB, recorded beside the gain state rather than inferred later, because it is the key the evidence-metric calibration tables are conditioned on and the key any confirm-rate breakdown must stratify by. It is emphatically **not** the gain setting: T-547 applied 51 dB of gain with the ADC *skipped* and reproduced the float calibration table to 0.02 bits on every metric with a bit-identical demod success rate (docs/21 §4), because every metric in the set is a ratio or a power-normalised statistic — so a corpus, dashboard or breakdown keyed on gain is keyed on a no-op. Within the ADC the mover is **under-fill** (σ = 0.21 LSB costs 1.0–1.6 bits and swings demod success 19 % → 53 %), not clipping (28 % clipped costs ≤ 0.34 bits). `FillBucket` has **two buckets with a table between them** — `nominal` (σ ≥ 0.5 LSB and clip ≤ 30 %) against `under_filled` / `over_clipped`, which get none and score 0 bits — because σ = 0.5 LSB is the only boundary docs/21 §3's sweep supports; more buckets would manufacture distinctions the measurement cannot see. **Missing σ is `under_filled`, never `nominal`**, the same rule as `bias_tee` above: an unknown fill is not a good fill, and the error direction is *less* evidence, never more. `quantisation_limited` stays the coarse flag and the two must agree; a disagreement (`Provenance::fill_flags_disagree`) is a front-end bug to surface. Omitted from the JSON form when unmeasured, so rows written before T-625 keep their dedup hash.
- **`device_id` also attributes every *command* to the front end** (*T-343*), not only every measurement. The five control routes that reach the radio — centre, rate, gains, bias tee, baseband filter — answer with, and are audited with, `device: {action, id}`, where `id` is this same `device_id` taken from the source itself (`SourceControl::device_info`). A retune that cannot say which device retuned is the gap T-302/T-303/T-304/T-305 closed for artifacts, baselines, history and the source layer, reopened at the control plane. `id` is `null` when the source reports no identity — "nothing said", never a placeholder, the same rule as `bias_tee` above.
- **Identity & lifecycle:** `provenance_id`; immutable; deduplicated (many frames share one provenance row when nothing changed).
- **Relationships:** references CalibrationState and SpurMask; referenced by frames, detections, recordings.
- **Retention & size:** small, deduplicated; kept as long as anything referencing it.
- **Tests:** assert every Detection has a resolvable provenance chain; assert overload/clip flags propagate to `suspect-IMD` on detections.
- **Note (from docs/06 §5):** timestamp method is host-arrival-time + running sample count, optionally GNSS-tagged; there is **no hardware 1PPS** on HackRF One, so sub-µs timing needs a GPSDO into CLKIN. Error budget is a spike (Phase 4).

### 2.7 CalibrationState  [C05]
Versioned calibration: frequency `ppm` (positive = oscillator fast) + method (LTE PSS / FM pilot / GNSS / LMR raster — the trunk hunt's blind raster fit with its alias settled by granted-channel energy, *added T-560*) + time, power-cal table ref (dBFS→dBm over frequency×gain), validity window / temperature.
- **Identity & lifecycle:** `cal_id`, versioned; a new measurement supersedes; provenance rows pin the version used.
- **Relationships:** referenced by Provenance; produced by C05.
- **Retention & size:** tiny; keep all versions (audit).
- **Tests:** feed a known reference tone; assert recovered ppm within tolerance; assert dBFS→dBm mapping monotonic across gain steps.

### 2.8 SpurMask  [C05, C09]
Versioned set of internal spur/image frequencies measured with a terminated input, per gain/config, plus IQ-image rules.
- **Identity & lifecycle:** `spur_id`, versioned; seeded from a factory/terminated-input scan and refined at runtime (the C05↔C09 bootstrap loop, docs/06 §5).
- **Relationships:** referenced by Provenance; used by C09 to mask/flag detections.
- **Retention & size:** small; keep versions.
- **Tests:** inject a known internal spur; assert detections at masked frequencies are flagged, real signals are not.

### 2.9 Detection  [C09]
The atomic measurement: `detection_id`, `survey_id`, `t_start`, `t_end`, `f_center`, `bandwidth` (OBW + x-dB), `snr_peak`, `snr_mean`, `peak_level_dbfs` (+ optional dBm), `sk`, `clip_count` (per span; not in Provenance, which is deduplicated), `detector_version`, `provenance_ref`, flags: `clipped` (required when `clip_count > 0` or the provenance is overloaded), `spur_candidate` with optional `spur_reason` ∈ {`ref-harmonic`, `dc`, `lo-relative`, `comb`, `spur-map` + SpurMask ref}, `image_candidate` with `image_retune_confirmed`, `marginal`, `suspect_imd`, `compressed`, `impulsive`, `edge` (*`spur_reason`, `image_retune_confirmed`, `suspect_imd`, `compressed`, `impulsive` and `edge` added from spike S4, 2026-09-13*), `dense_skipped` (the box spans a frame the detector could not label completely: dense, or runs dropped at the component cap; *added T-037b*). Cross-capture trust-test verdicts (gain step, retune, rate change) never update a detection: they are append-only `trust_verdict` rows per Survey (test, label, emitter span, soft track reference, numeric detail; *T-037b*). The IQ snippet and track membership link to the detection from Recording (`trigger`) and the track↔detection link table, so the row never changes.
- **It is a time–frequency region** (*[ADR-0017](adr/0017-time-extent-signal-model.md) §1, the user's settled model, 2026-09-16*). `t_start`/`t_end` are its **time extent** and `f_lo`/`f_hi` its width — a detection is a box in time and frequency, not a point on a frequency axis. Nothing requires a carrier or a stable centre, so a millisecond burst and a 10-second chirp are each one ordinary detection. This is stated because everything above it (§2.27, §2.11) depends on it and because the model was previously implicit.
- **Identity & lifecycle:** `detection_id` (UUIDv7); **immutable** once written. Interpretation lives elsewhere.
- **Relationships:** child of Survey; links to Track, Recording; the raw material for everything downstream.
- **Retention & size:** ~530 bytes/row with its indexes and track link (measured on the staging device 2026-09-24); at ~65–90 rows/s from one 2.4 Msps HackRF that is ~125 MB/h, ~3 GB/day unpruned. Indexed by `(f_center, t_start)`, `(f_lo, f_hi)`, `t_end` and `(survey_id, t_end)`. **Aged out, rolled up (T-904, `repo/retention.rs`):** a row whose `t_end` is older than its survey's watermark minus the retention age (an open survey's own newest `t_end`, so a replay into an existing store or a host clock behind it never ages rows the running tracker still holds; a closed survey's is the store's newest; the daemon floors the age at 10 min; default 1 hour: no reader needs older per-frame rows, and an hour is ~190 MB at the staging rate where a day would be ~4.6 GB) is folded into a **DetectionRollup** of its track — one per contiguous run (same survey and provenance, no gap over 10 s, at most 60 s; a row no track links rolls up only with rows overlapping it in frequency, so a one-off burst keeps its own time–frequency box and bursts at different frequencies never share one): time hull, time on air (the members' summed duration — the hull is never the time on air), frequency envelope, mean/max OBW and SNR, peak level, count, clip count, OR/AND of the flags — and then deleted with its track link. Never pruned: each emitter's newest 256 linked rows (every per-emitter "newest detections" query reads at most that many, so those answers are exact), and any row named by id (decode provenance, recording trigger, retune verdict, direct emitter link, annotation, anomaly subject, classification input, ledger source). Tracks, emitters, presence and links are untouched, so a pruned window keeps its inventory, events and presence; time-windowed reads past the age (occupancy) see the rollups. Batched, off the real-time path, reported in `/api/status` `storage`.
- **Tests:** replay a fixture; assert detection count, center/bandwidth/SNR within tolerance, and false-alarm rate under threshold on a noise-only capture (the SNR-wall check).

### 2.10 Track  [C10]
A linked series of Detections (same `f±ε`, similar BW) with timing features: periodicity, duty cycle, inter-arrival stats, hop set + rate, TDMA frame period, inter-channel co-occurrence.
- **Identity & lifecycle:** `track_id`; grows as detections arrive; closed after an idle timeout; may merge/split (recorded, not overwritten).
- **Relationships:** groups Detections; belongs to an Emitter (or is a candidate).
- **Retention & size:** compact; kept with its Emitter — durable, whatever happens to its per-frame Detection rows (§2.9), whose aged-out runs it keeps as DetectionRollups.
- **Tests:** synthesise a periodic/hopping emitter; assert recovered period, hop set and duty cycle within tolerance.

### 2.11 Emitter  [C27] (the inventory entry)
The persistent "thing seen on the air": `emitter_id`, current `f`/`BW`, `fingerprint` (C18), `first_seen`, `last_seen`, `count`, `classification` (family + confidence + open-set score + model version; from M3 the full §2.21 Classification) as an **append-only history** (not overwritten), with the current family picked by arbitration rank (§2.21), `identity` (decoded id such as ADS-B hex, RDS PI, MMSI, talkgroup) or `unknown`, `known_status` vs priors (`known` / `unexpected-here` / `unknown`), `tags`, and links to Tracks, Detections, Recordings, Demodulations, Explanations, Annotations.
- **Identity & lifecycle:** `emitter_id` stable for the life of the cluster; created when detections cluster to a new fingerprint (C18); `last_seen`/`count` update continuously; classification is re-run and appended as models improve — the measurement it ran on is unchanged.
- **Time extent lives on the presence interval, not here** (*[ADR-0017](adr/0017-time-extent-signal-model.md) §1, §8*). An Emitter is an **identity that owns an ordered set of disjoint presence intervals** (§2.27). Consequently:
  - **`first_seen`/`last_seen` are the *hull* of that set, never its extent.** An emitter that fired at 09:00 and at 17:00 has an eight-hour hull that is 99.99 % silence. **Never display the hull as a duration**, and never treat it as "how long this was on air". The inventory time filter must test *interval overlap*, not hull overlap — today's `last_seen >= t0 AND first_seen <= t1` (`repo/inventory.rs`) is a hull test and matches every window between two distant sightings.
  - **`count` is a lifetime total, valid only in History.** It is **excluded from every liveness decision and from live-list ranking**. It was previously the only place "this is still here" could be written down, which is why it grew without bound (38 → 582,500/h, observed 2026-09-16); the open interval's advancing `t_end` is where that information belongs. Its one consumer is `recurrence.occurrences` (→ `/api/inventory` → the UI's recurrence sort and "N× in M min") plus the API's own `count` field; `ConfirmPolicy` does **not** read it (`ConfirmEvidence` is decode evidence plus `TrackTrust`), corrected in T-329.
  - **`count` totals *occurrences*, not writes** (T-209, T-329, T-336). One emission over one stretch of air is **one** occurrence, however many producers saw it and however many rows they wrote: overlapping observations are not counted twice, **whoever observed them**. The ledger records *who measured what* and `count` records *how often the emitter was there*, so they are different numbers and a ledger row's `count` is what its producer measured, never what it added. Entity resolution enforces this on both routes an observation can take to an entry — arriving directly (`repo/cluster.rs::unclaimed_count`: a sighting adds only its excess over the largest count already recorded over the same air, by any producer) and arriving by a later merge of two entries of one emission (below). "The same air" is one test throughout: spans intersecting by at least half the shorter one (an instant inside a span counts; touching windows and a few ms of edge jitter do not). There is no proration between those: `count` counts occurrences, not seconds.
  - **Derived, window-scoped fields** for a request window `[t0, t1]`: `intervals_in_window`, `on_air_s_in_window`, and `liveness` ∈ `live` (an interval is open at the live edge) / `ended` (its latest in-window interval is closed, with `ended_t_s`) / `absent` (no interval intersects; Confirmed rows only). All derived, none stored.
  - **An emitter that is a set of disjoint events is still one emitter.** Disjointness never argues for splitting an identity — a doorbell sensor is one device whether it fires once or a thousand times. What is wrong in that case is only the *rendering*: show "17 events over 6 h, 4.2 s on air", never "first seen 6 h ago, count 582,500".
- **Inventory lifecycle (T-078):** `lifecycle_state` is `candidate` / `confirmed` / `deleted`, with an append-only history (`emitter_lifecycle`: new and previous state, author `auto` or `user`, actor = rule id such as `hk-pipeline/confirm@1` or the API token fingerprint, reason, `t`).
  - Every emitter starts as a **candidate**. Candidates carry **recurrence statistics** from the observation ledger: occurrences (`count`), appearances (track observations, or decoder sightings when there are no tracks), span, on-air time (Σ appearance span × measured duty cycle; unknown duty adds nothing), duty cycle, and the latest appearances.
  - **Auto-confirm** only on strong, unambiguous, blind evidence (`hk_pipeline::inventory::ConfirmPolicy`, configurable; defaults): a decoded transmitter identity carried by ≥ 1 CRC-valid decode (structural identities such as the blind framer's `other:hk-framing` signature do not count), **or** one continuous, trusted track: ≥ 2 s on air, duty cycle ≥ 0.8, ≤ 50 % suspect member detections and ≥ 1 trust-confirmed detection. Recurring intermittent, weak or suspect signals stay candidates until a user **promotes** them (`POST /api/inventory/{id}/promote`). No rule demotes or deletes.
  - **`ConfirmPolicy.synthesized`** (*T-860, MAUTO M-9; [ADR-0015](adr/0015-decoder-synthesis-contracts.md) §5.5 as derived by [ADR-0022](adr/0022-false-confirm-budget.md) §6*; actor `hk-pipeline/confirm-synth@2`) is a third route, for a region-analyze job whose rank-1 pipeline **solved on hold-out**: fields `enabled`, `min_analytic_holdout_bits` (24), `hard_check_floor_bits` (16), `min_check_width` (8 as shipped by T-860; T-577 measured 16 for the shipped count, 8 only with ADR-0022 §4.3.1's count change, which T-575 applies), `assumed_decisions_per_week` (20 000, the budget's denominator; the rolling counter against it is T-575), `require_null_control_when_searched` (true), `max_suspect_detection_fraction` (0.5), `forbid_overload_in_window` (true). It confirms a candidate only when the profile is not `quick`, the check is ≥ 8 bits wide, `check_bits = width × differences − L_check ≥ 16` over frames valid without FEC correction, analytic hold-out bits (each stage net of its own look-elsewhere) ≥ 24, a **searched** check's null control ran and passed, and the analysed window has ≥ 1 detection of the emitter with ≤ 50 % suspect and the IQ known not overloaded. `min_evidence_bits` and `min_distinct_valid` do not exist (ADR-0022 §6: wrong currency; the frame count is the per-job formula `max(1, ⌈(24 + L_check)/width⌉)`). Synthesized decodes (`decoder_id` `synth:…`) never count toward the identity route above. The lifecycle reason carries the arithmetic.
  - **User delete** (`DELETE /api/inventory/{id}`) removes the entry from the inventory (`/api/inventory` lists it only with `state=deleted`) but keeps the row, its detections, tracks, links, decodes and classification/status/lifecycle history. A deleted row takes no further sightings and is never merged: entity resolution skips it (fingerprint, context, ledger, re-measurement), so **a later detection of the same signal creates a new candidate**; a decoded identity it held moves to the emitter that sighting reaches. There is no undelete. The region-over-time history query (§4) still sees deleted emitters.
- **One entry per physical emitter (T-082):** a decoder or chain output of an emission the tracker also followed (RDS PI on a WFM track, the blind framer's signature on an FSK sensor track) is linked to that track's entry instead of staying a second one.
  - **Same emission** (`Repository::same_emission`): detected or refined centres within the centre tolerance, observations overlapping in time, both hop sets or neither, not two identities; two track-based entries also need fingerprints within tolerance. The pipeline links only entries of the same run (capture) and never a channel-sharing transmitter identity (ADS-B ICAO, …) to a channel entry.
  - **Merge:** the confirmed entry survives, else the first seen. Links, observations (recurrence, overlapping observations not counted twice), tags, identity, classification history, refined tuning and a decoder/classifier/user status are kept. Confirmed wins over candidate, and a lifecycle history row records a confirmation carried by a merge. A deleted entry is never merged, so re-detection after delete still creates one new candidate.
- **User band (T-191):** an optional user-adjusted `f_lo`/`f_hi` with `set_at`, actor (token fingerprint) and reason (`emitter_user_band`, one current row per emitter), stored beside the measured `f`/`BW`, which it never overwrites and which detection, tracking and entity resolution keep using. Set/clear through `PUT`/`DELETE /api/inventory/{id}/band` (audited). Validated: finite, `0 < f_lo < f_hi`, width ≤ 40 MHz, overlapping the measured band or within 1 MHz of it. On merge the survivor keeps an override if either entry had one, the latest `set_at` winning.
- **Synthesis (`emitter_synthesis`, *T-546; job rows T-860*, ADR-0015 §5.4):** an append-only history of analyses — `emitter_id`, provenance `synthesized by output analysis`, `engine`, `t`, `verdict`, `stage_reached`, the chosen `pipeline`, per-stage `evidence`, the `trace`, a `resolution` whenever the verdict is below `solved` (never `not-searched`: a row *is* a finished search), the receiver fit, and — on a row a region-analyze job attached — `job`: `job_id`, `profile`, `evidence_bits` (rank key), `prior_bits`, `analytic_holdout_bits` (the confirm key), `template`, the rank-1 `recipe` inline (≤ 64 KiB) with its `recipe_hash` (`sha256:…`), `check`, the hold-out evidence, `trace_summary`, `replay_key`, `null_control`, the `sealed_resolution`, `decodes_stored`/`decodes_valid` and the `confirm` decision. **An analysis never overwrites the emitter's measured values.** `/api/inventory` rows carry the latest row summarised (`synthesis`, `null` = not searched) and whether the identity rests only on synthesized decodes (`identity_synthesized`).
- **Relationships:** the hub. Loops with Explanation (subject or cause) and with Decode (identity) and priors (C17).
- **Retention & size:** thousands of rows; never auto-deleted by the system (it's the memory; only a user deletes an inventory entry, and the row stays as history); links may outlive the raw detections they summarise.
- **Tests:** replay two sessions of the same emitter; assert one Emitter with `count` summed and `first/last_seen` spanning both; assert an unknown signal yields `known_status: unknown` with a non-zero open-set score.

### 2.12 Recording  [C25]
A SigMF dataset reference: `recording_id`, `uri` (`.sigmf-data` + `.sigmf-meta` paths), `type` (`iq-snippet` / `channel-decimated` / `audio`), `t_span`, `f_center`, `fs`, `trigger` (`detection_ref` / `demodulation_ref` / `scheduler` / `manual` / `analyze` — *`analyze` added T-857: the ring windows a region analysis acquired, pinned before its search, ADR-0015 §6*), pre/post-trigger seconds, `size_bytes`, `retention_class`, `content_class` (for gating), `provenance_ref`, embedded `annotations[]`.
- **Identity & lifecycle:** `recording_id`; files live on disk, **not** as blobs in the DB; triggered by detection with pre-trigger from the ring buffer (C03).
- **Relationships:** referenced by Detection/Emitter/Demodulation; carries Annotations (written into the SigMF meta).
- **Retention & size:** the IQ budget — 20 Msps is 144 GB/h, so recordings are short snippets, quota-managed and ranked by interestingness; `content_class` can forbid retention outright (restricted content, docs/06 §5).
- **Tests:** trigger on a synthetic burst; assert the SigMF file validates, includes pre-trigger samples, and its annotations match the detection.

### 2.13 Annotation  [C28]
A label on a Detection / Emitter / Recording / time-freq box: `author` (`user` / `decoder` / `classifier`), `kind` (`label` / `correction` / `ground-truth`), `value`, `confidence`, `t`, `exported?`.
- **Identity & lifecycle:** `annotation_id`; append-only; a `ground-truth` from a valid decode is the label used for fine-tuning (C38) and for tests.
- **Relationships:** attaches to any of the above; written into SigMF annotations when on a Recording.
- **Retention & size:** small; kept with its target; exportable as labelled SigMF datasets.
- **Tests:** assert a valid CRC decode writes a `ground-truth` annotation on the Emitter; assert export produces a valid labelled SigMF set.

### 2.14 Demodulation  [C19, C20]
A demod session on a channel derived from an Emitter/Detection: `demod_id`, `emitter_ref`, `mode`/family, estimated params (symbol rate, deviation, CFO, mod order, roll-off, bandwidth, and for WFM the measured stereo pilot frequency `pilot_hz`, *T-037b*), `lock_quality`/EVM, outputs (`audio_ref` / `symbol_stream_ref` / `bitstream_ref`), `demod_version`.
- **Identity & lifecycle:** `demod_id`; re-runnable over a Recording (offline) or live; params come from C13/C14; version recorded.
- **Relationships:** child of Emitter; produces Decode/Bitstream; the estimated params also refine the Emitter fingerprint.
- **Retention & size:** small metadata; audio/symbol/bit outputs are Recording-like files under quota.
- **Tests:** replay a known-modulation fixture; assert estimated symbol rate/deviation/CFO within tolerance and EVM below threshold.

### 2.15 Decode / Message  [C21, C22]
Structured output from a decoder plugin or bit-framing inference: `decode_id`, `demodulation_ref` (or `recording_ref` for replay), `decoder_id` + version, frame model / fields, `crc_status`, decoded `identity`, `content_class`, `t`, and an optional `provenance`.
- **`provenance`** (*T-860, ADR-0015 §5.5 + ADR-0022 §11.3*): absent on every decoder-, plugin- and recipe-produced row. A decode made by a **synthesized** pipeline over a region-analyze job's hold-out window carries `{kind: "synthesized", job_id, holdout: true, evidence_bits, hypotheses, analytic_holdout_bits, check_bits?, l_check?, check_searched, template_provenance?}` (`template_provenance` is `template-fixed` · `discovered` · `searched` — how ADR-0022 §5.1 priced the template's check; T-884), with `decoder_id` `synth:<template id | open>` and `decoder_version` `<engine>+sha256:<recipe hash>`, so a confirmation's arithmetic is reconstructible from the stored row. An open search's decodes carry the structural identity `other:hk-framing`; a template-bound identity is kept and still marked synthesized. Such rows never count toward the identity confirm route (§2.11).
- **Identity & lifecycle:** `decode_id`; a valid CRC makes it ground truth that names the Emitter and writes an Annotation; re-runnable as decoders improve (the SatDump lesson).
- **Relationships:** produced by C21 (inferred framing) or C22 (known decoder); feeds Emitter identity and Bitstream; `content_class` gates what may be stored/streamed (docs/06 §5, restricted content).
- **Retention & size:** small rows; high volume for chatty protocols (ADS-B) — summarise/roll old messages, keep identities.
- **Tests:** replay an ADS-B/AIS/rtl_433 fixture; assert decoded fields and CRC-valid message count match golden output.

### 2.16 Bitstream  [C20, C21 → C24]
The bit/soft-symbol artifact bridging demod/inference and external consumers: either a stored file (with framing metadata) or a live stream descriptor for C24. Carries `emitter_ref`, framing, timestamps, source provenance.
- **Identity & lifecycle:** `bitstream_id`; live streams are transient with a descriptor row; stored ones are Recording-like.
- **Relationships:** output of Demodulation/Decode; consumed by external programs via stream-output (C24), subject to `content_class` gating.
- **Retention & size:** live = none; stored = quota.
- **Tests:** assert the stream contract framing round-trips and backpressure is honoured (contract defined in a Phase 3 ADR).

### 2.17 ExternalEvent  [C29]
A cached fact from a context feed: `event_id`, `source` (SWPC scale / GOES X-ray / Kp / lightning / TLE-pass / SondeHub launch / gpsjam / PSKReporter / FMLIST …), `type`, `t` or `t_span`, `geo` (point / region / orbit-pass window), `payload`, `fetch_time`, `cache_age`/validity.
- **Identity & lifecycle:** `event_id` (source + native id); **offline-first** — everything works from the cache; refreshed opportunistically when online; pass windows are computed locally from cached TLEs (docs/06 §5).
- **Relationships:** consumed by Explanation (C30) and scheduler (C04); FMLIST/SatNOGS also serve as priors via C17 over the same cache.
- **Retention & size:** small; time-bounded cache with per-source validity.
- **Tests:** with a frozen cache, assert correlation results are deterministic and a stale cache degrades gracefully (no network in CI).

### 2.18 Anomaly  [C08, C12, C27 → C30]
The shared record §5 said doc 07 must define: `anomaly_id`, `kind` (`new-emitter` / `busier-than-baseline` / `noise-floor-rise` / `novelty` / `level-above-baseline` / `change-point`; the C12 novelty-alarm kinds and their `baseline_ref` format are in [ADR-0012 §7](adr/0012-attention-memory-contracts.md)), `subject_ref` (Detection/Emitter/region), `region` (`f_lo,f_hi,t`), `score`, `baseline_ref`, `t`.
- **Identity & lifecycle:** `anomaly_id`; emitted by C08/C12/C27; consumed by C30; resolved/dismissed state tracked.
- **Relationships:** the input to Explanation; references the baseline it deviated from.
- **Retention & size:** small; kept while relevant, then summarised.
- **Tests:** feed a baseline then a change; assert the right anomaly kind/score fires and no anomaly on stationary input.

### 2.19 Explanation / Correlation  [C30] (the attack map)
Links a local Anomaly to a likely cause: `explanation_id`, `anomaly_ref`, `candidate_cause` (ExternalEvent ref or own-history pattern), `correlation_type` (`time-coincidence` / `geometry` / `signature`), `score`, `evidence[]` (links), `t`.
- **Identity & lifecycle:** `explanation_id`; recomputed as feeds arrive; ranked; user can confirm/reject (writes an Annotation).
- **Relationships:** joins Anomaly ↔ ExternalEvent / Emitter; surfaced in the attack-map view (C39).
- **Retention & size:** small; kept with the anomaly.
- **Tests:** with a synthetic GNSS noise-floor rise + a cached gpsjam event at the same time/region, assert an Explanation of type `time-coincidence`/`geometry` with the event as top cause.

### 2.20 Selection  [C39] (user region, *T-052*)
A region the user marked and keeps acting on: `selection_id`, `name`, `f_lo`/`f_hi` (Hz, `0 <= f_lo < f_hi`), optional `t_lo`/`t_hi` (both or neither, `t_lo <= t_hi`), `notes`, `tags` (distinct), `links[]` to the actions taken on it (`kind` `demodulation` / `recording` / `bitstream` / `inspection`, `target` id or short reference, `t`, `note`), `created_at`, `updated_at`.
- **Identity & lifecycle:** `selection_id` (UUID; a client may choose it so an offline-created selection keeps its identity when it syncs). **User metadata, mutable** like bookmarks: renamed, re-bounded, deleted; `created_at` never moves. Links are an append-only ring (oldest dropped past 256). Several selections exist at once.
- **Relationships:** the entry point of the region-over-time query (§4) and of actions: Listen/Demodulation (T-043), Recording (T-050 manual IQ; per-selection outputs T-061), inspection (history + ranked explanations of the Emitters inside). It references those objects by id and never holds signal content itself.
- **Already a time–frequency region.** `f_lo`/`f_hi` plus the optional `t_lo`/`t_hi` make a Selection the user-authored counterpart of §2.27's interval, so a drag on the capture timeline (docs/14) needs no new object — it is this one, with its time bounds set ([ADR-0017](adr/0017-time-extent-signal-model.md) §8.1).
- **Retention & size:** small; never auto-deleted (the user's memory of what mattered).
- **Storage:** `selection` table (id, name, `f_lo`, `f_hi`, `t_lo`, `t_hi`, times, JSON body) in the run database, served by `/api/selections` (`crates/hk-api/src/selections.rs`), token + audit like the control API.
- **Tests:** repository CRUD, validation, link ring and reopen (`hk-model` `repo/selections.rs`); HTTP CRUD, restart persistence, validation and auth (`crates/hk-api/tests/selections_api.rs`); UI store sync, offline fallback and action dispatch (`ui/test/selections.test.ts`).

### 2.21 Classification  [C15] (*T-211*, [ADR-0016](adr/0016-classification-contracts.md) §1–§2)
One C15 output about an emitter (`hk_model::classify::Classification`, schema 1):
- `t`, `taxonomy` (e.g. `hk-mod@1`), `input` (the observation it ran on) and `coarse` (`analog` / `digital` / `noise-like` / `unknown`).
- `posterior` **and** `likelihood` (evidence only): distributions over the taxonomy's families plus `unknown`, each summing to 1. No posterior entry is exactly 1.
- `prior` (C17 `{prior_ref, lambda [λ₀..λ₃] with λ₀ ≥ 0.1, dist over known families}`, or none).
- `family` (the top posterior label, possibly `unknown`), `confidence` (≤ 0.999) and `class` (`{label, p, dist, stage}` within the family, or none below its gate).
- `open_set_score` (0–1) and `entropy_norm` (H/ln K, K = families + 1).
- `stage` (`feature-tree` / `verifier` / `dl` / `decoder` / `user` / `chain` / `track-shape`) and `provenance` (rules@version, features version/ref, model ref when a DL stage decided, SNR vs gate, thresholds@version, suspect flags, power mode).
- `flags` (`prior-tiebreak`, `prior-mismatch`, `below-gate`, `suspect-input`, `dl-shadow-disagrees`) and machine `reasons`.
- **Taxonomy `hk-mod@1`** (data: `hk_model::classify::taxonomy`):
  - analog → `analog` {am, nbfm, wfm, ssb, cw};
  - digital → `ook-ask` {ook, ask4}, `fsk` {2fsk, gfsk, msk, 4fsk}, `psk-qam` {bpsk, qpsk, 8psk, qam16, qam64}, `ofdm`, `css` {chirp}, `dsss`, `pulsed` {ppm, pulse};
  - noise-like → `noise-like`.

  `unknown` is the open-set outcome at every level. A new class or family is a new version, and rows keep their version. `taxonomy::family_of(label, version)` maps pre-M3 labels (`fsk`, `2fsk`, `ook`, `wfm` …). Service labels (`adsb`, `fm-broadcast`, decoder ids) map to nothing.
- **Storage (legacy fit):** additive nullable columns on `emitter_classification` (migration 0007): `taxonomy`, `stage`, `arb_rank`, and `detail` (the full Classification as JSON). The legacy columns keep their meaning (`model_version` = rules@version, the DL model ref, or `decoder:<id>`), so existing readers and writers are unchanged. Rows written before M3 have NULLs, and readers derive their stage and rank.
- **Arbitration rank (current family):** the emitter's current family is the row with the lowest rank, latest among equals.

  | Rank | Evidence |
  |---|---|
  | 0 | user |
  | 1 | decoder (CRC-valid) |
  | 2 | lock-verified: verifier, or a chain label with demod lock |
  | 3 | classifier: feature tree / DL, or a chain label without a recorded lock |
  | 4 | track shape |

  Derivation for pre-M3 rows: a `decoder:` model version → 1; `input_kind = track` → 4; anything else → `chain` at 3. The inventory `family`, its filter and `classification` all use this rank. A newer lower-ranked row is still history (`latest_classification` in the API).
- **`family_in_window` (additive projection, [ADR-0017](adr/0017-time-extent-signal-model.md) §7.1).** **The rank ladder is unchanged**; only its input set gains an optional time predicate. `family` stays arbitration over *all* rows — identity evidence is time-invariant, and a CRC-valid decode from yesterday still says what the thing is. `family_in_window` is the same arbitration restricted to rows whose `t` falls in the view window, and is `null` when the window contains none; a view-scoped UI then shows `family` marked *(from earlier)* rather than silently asserting a stale classification. No ADR-0016 contract changes.
- **Identity & lifecycle:** append-only (the table's no-update trigger), re-run and appended as classifiers improve; carried to the survivor on a merge with every column.
- **Tests:** `hk-model` `classify::{taxonomy,rank}` unit tests and serde round trips; `repo/classify_rank_tests.rs` covers every writer pair in both write orders and both merge directions, the SQL-versus-Rust legacy rule, and migration 0007 over pre-M3 rows; `crates/hk-api/tests/inventory_classification_api.rs` checks the row fields.

### 2.23 Signature  [C18] (*T-218*, [ADR-0016](adr/0016-classification-contracts.md) §5)
An immutable catalogue entry (`hk_model::signature::Signature`, schema 1): "an emission with *these* parameters, within *these* tolerances, is consistent with this protocol or device type".
- `id` + `version` (immutable once written; a new version is a new row), `name`, `kind` (`protocol` / `device-type` / `rfi` / `radar` / `learned`).
- `taxonomy` + optional `family`/`class` it expects. **Rank-only:** an `unknown` classification gates nothing.
- `fields`: per field name, `{expect, tolerance, required, weight}`, where `expect` is a value, a range, a set, a bit pattern (`0`/`1`/`x`, matched in both polarities and every PSK rotation within `max_errors`) or text. Default symbol-rate tolerance ±1 %.
- `min_discriminating` (default 3): required fields that must be present before a match can be `full`.
- `recipe` (the decoder recipe it hands the search, ADR-0016 §8), `provenance` (`builtin` / `user` / `recipe-confirmed` / `rtl433-import` (untrusted) / `cluster-promoted`), `author`, `created_at`, `supersedes`, `bands_hz` (**rank-only: a band never gates a match** — an emission in the "wrong" band is the interesting case).
- **Storage:** table `signature` (migration 0009), PK `(signature_id, version)`, content immutable by trigger; `retired_at` is the only mutable column, and rows are never deleted.
- Minting (`recipe-confirmed` from a CRC-valid decode), matching and import are T-201/T-214.

### 2.24 SignatureMatch  [C18] (*T-218*, [ADR-0016](adr/0016-classification-contracts.md) §5)
The append-only record of comparing one emitter's measured features against the catalogue (`hk_model::signature::SignatureMatch`): `emitter_id`, `t`, `outcome` (`full` / `partial` / `none`), `features_ref` (the EmissionFeatures snapshot, §2.22, T-201), `signatures_rev` (so a match can be re-derived exactly), `candidates` (≤ 5, ranked best first: `{signature, name, score, agreement: [{field, measured, expected, z, ok}], missing, conflicting, recipe}`) and machine `reasons`.
- **A match never sets identity, `known_status` or lifecycle.** It adds ranked explanation evidence, feeds `decoder_available` when the top candidate has a recipe, and seeds MAUTO. Only a CRC-valid decode confirms a signal.
- A near miss is kept as `partial` with the conflicting fields named (`z > 3`), never snapped to the nearest entry — mismatches are interesting.
- `none` means the catalogue has nothing to say, **not** that the emission is unknown.
- **Storage:** table `signature_match` (migration 0009), append-only by trigger, with the outcome/top-candidate columns kept consistent by CHECK constraints.

### 2.26 ModelManifest / Prediction  [C38] (*T-218*, [ADR-0016](adr/0016-classification-contracts.md) §6)
Provenance for the ML runtime (`hk_ml`, a contract stub until T-203): a `ModelManifest` (`id@version#sha8`, sha256, task, consumer, taxonomy, the family a within-family class model is scoped to, labels, energy-based open-set calibration, precision, metrics and enable-evidence references) and a `Prediction` (model, provider, precision, labels, logits, calibrated `probs`, `energy`, `unknown_score`, optional embedding, `mode`, latency, batch, `t`).
- **Open set is energy-based and calibrated**; the softmax maximum is never an unknown detector.
- **A model never chooses the family** — within-family class only.
- **Shadow mode decides nothing**: it writes no Classification row and changes no decision.
- Models are data (a manifest + an ONNX file loaded at runtime): swapping or rolling one back never rebuilds the pipeline. `active` needs the §4.6 enable evidence. Shadow records live in hk-store, not the database.

### 2.27 Presence interval  [C27, C10] (*[ADR-0017](adr/0017-time-extent-signal-model.md) §1, the user's settled model 2026-09-16*)
**A maximal span during which one emitter was continuously on the air**, within the detector's ability to tell continuity from gaps: `{emitter_id, t_start, t_end, f_center, count, open}`. This is where a signal's **time extent** lives — invariant 1 of the settled model ("a signal is a time–frequency region, not a persistent carrier"), and the object the waterfall **box** is drawn from.

- **It is already materialised, not new.** `emitter_observation` (migration 0001) carries exactly `(source_kind, source_id, emitter_id, count, t_start, t_end, measurement, f_center)`, one row per sighting source (a track, a decode sighting). It has existed since the first schema; it was never named, never indexed for time, and never surfaced beyond `recurrence.recent[]` in the API. **No new table is needed** — see §3.1.
- **Disjoint after normalisation.** Overlapping source rows for the same minutes (a track and a decode of the same emission) merge into one interval. This is the existing "overlapping observations not counted twice" rule of §2.11, given a name.
- **`open` is derived, never stored.** An interval is open while `now − t_end ≤ idle_gap`, closed otherwise. The idle gap is a tuning parameter, and storing a decision made under one parameter value would break this document's first rule (measurement versus interpretation): change the gap and closure re-derives correctly.
- **`idle_gap` is derived from the revisit period, not set per band** (*T-262 answers [ADR-0017](adr/0017-time-extent-signal-model.md) §11 question 4*). `idle_gap = clamp(2 × revisit_period, 1 s, 60 s)` (`hk_model::presence::IdleGap`). The reason is a measurement one: **a gap shorter than the revisit period is not evidence of absence** — a receiver that returns every 2 s and sees bursts at 0 s and 1.9 s never observed the emitter being off. Every constant comes from a rule that already exists: `2 ×` because absence needs two consecutive missed revisits (the bandit's own point-of-interest gap rule), the 1 s floor from the tracker's `max_transition_gap_s`, the 60 s ceiling from its `idle_timeout_s` (past it the tracker has *already* closed the track, and a parameter must not overrule a measurement). An unknown revisit takes the 60 s end, claiming no absence it cannot show. **Consequence, accepted deliberately:** a chatty 915 MHz ISM sensor firing every 30 s is **fifty intervals, not one** — "50 events, 1.0 s on air" is honest and each burst's box is 20 ms tall. Fifty intervals are cheap because they are fifty intervals on *one* emitter. Periodicity is said by `EmissionFeatures` (period, duty cycle, burst length), not by smearing fifty transmissions into one span of mostly silence — which is the hull pathology this section exists to remove, and exactly what a per-band gap wide enough to "make ISM one interval" would reintroduce one level further down.
- **Closing is not decay.** It is a measurement fact — evidence stopped arriving — and it is permanent History. Decay applies to a *candidate's confidence in its hypothesis*, never to the record (ADR-0017 §5).
- **Decay is confidence as a function of observed absence** (*T-251 implements [ADR-0017](adr/0017-time-extent-signal-model.md) TM-6*). `confidence = 1` while the latest in-window interval is open, then `exp(−(silence − idle_gap) / idle_gap)`, and `0` when no interval intersects the window (`hk_model::presence::confidence_after_silence`). **The time constant is the `idle_gap` above**, and it is not a new number: the gap is one unit of *observed* absence, so below it the receiver has seen no absence at all (confidence is flat at 1, and the interval reads open), and past it the receiver can only learn "still nothing" once per gap — `(silence − idle_gap) / idle_gap` independent observations, one `1/e` each. No free parameter; changing the revisit period moves decay exactly as far as it moves closure. **What it owns is one case:** window-scoping already drops a signal that stopped *hours* ago (it is not in the window), but `on_air_s` cannot rank a signal that stopped *inside* the window, being blind to when — five seconds at the window's start and five seconds ending now are the same number. Confidence separates them, so the stopped row reads `ended` and **ranks lower while staying listed**; its box is on screen and its interval stands. **It is a rank, not a lifetime:** never a per-tick decrement (ADR-0017 §5 — "the same pathology inverted"), it deletes nothing, expires nothing and never touches History, and being a pure function of interval boundaries it is reversible with no revival path — a returning signal's new interval restores it to 1 on the same emitter. `hk_model::presence::recheck_horizon_s` turns the same law into the scheduler's re-verification horizon, `gap × (1 − ln 0.05) ≈ 4 × gap`, where 0.05 is the scheduler's own `MIN_DWELL_SHARE` — past it a stopped candidate is no longer re-checked, because a dwell spent there is one taken from a candidate with live evidence.
- **A persisting signal's box grows** because its open interval's `t_end` advances with the live edge. That growth **is** the accumulation the unbounded `count` was standing in for, now bounded, time-scoped and directly meaningful.
- **An OPEN interval's box runs to the live edge; only a detected end caps it** (*T-410, [ADR-0019](adr/0019-presence-as-an-interval-with-endpoints.md), the user's model 2026-09-16*). Presence is an **interval with endpoints**, so the measurement is the interval opening plus the **absence of a close** — a track is open until it closes. `open` therefore stops being decoration on the top edge and becomes the claim the box is drawn from, and the stream carries only endpoints (`presence-start` / `presence-reopen` / `presence-end`), never a per-poll bump. Two things keep it honest, and both are stated rather than assumed:
  - **The box distinguishes measured air from assumed air.** The span from `t_end` to the live edge is the **open cap**, drawn as assumption with a rule where measurement stops, and it grows visibly as the silence grows — so a suspected end is legible without being acted on, and there is no third "suspected" state to flicker on a missed frame. The same device as the coverage map's grey, on the time axis.
  - **A close carries the *measured* `t_end`, not the instant it was decided**, so the box **retracts** to the truth when the interval closes rather than keeping whatever the assumption had reached. The over-claim is transient and bounded by the end detector's latency.
- **The idle gap is the end detector's latency, so it must be *measured*, not defaulted** (*T-410*). Because a box now runs to the live edge until the interval closes, `idle_gap` is exactly how long a box over-claims silent air after an emission stops. hk-api used to pass `IdleGap::conservative()` — the 60 s end — on the grounds that it "does not know the scheduler's revisit period", which made every live box over-claim a minute. But a receiver's revisit period is a **measurement**, recorded in the IQ ring's tune history: one segment per retune, with its window, centre and rate. `IdleGap::from_coverage` reads it per band — contiguous coverage ⇒ the 1 s floor (`IdleGap::continuous`, the receiver never looked away), combed coverage ⇒ `2 ×` the largest silence between visits, **no coverage record at all** ⇒ the conservative 60 s, which is now reserved for its real meaning: *nobody recorded whether the receiver looked*. Latency under a live dwell: **≤ 1.25 s** on the stream, **≤ 5 s** if a close is lost and the poll is the backstop.
- **A detected end is provisional, and one further idle gap revokes it** (*T-413, the user 2026-09-17; [ADR-0019](adr/0019-presence-as-an-interval-with-endpoints.md) §6.2*). A signal that resumes within tolerance **nulls the end and keeps the ONE interval open** on the same emitter, rather than splitting it. The tolerance is the **existing** `idle_gap` — no new parameter — and its window is anchored on the end *event*, because an end only fires once a full gap of silence has been observed past the measurement, so a window measured from the measurement could never be reached. That makes the join tolerance `2 × idle_gap` (`IdleGap::revocable_nanos`), which is not that constant doubled as a dial: the first gap is the observed absence that **justifies** the end, the second the observed absence that **confirms** it — equivalently, the end stands revocable for exactly as long as `confidence` is above `1/e`, its first e-fold. **Clamped by the 60 s ceiling**, which revocation may not lift: past the tracker's `idle_timeout_s` the discontinuity was judged by a measurement upstream. **The rejoined silence is never air:** the interval records it (`PresenceInterval::revoked`, served as `revoked_s`) and both `duration_s` and `on_air_s` subtract it, so revoking changes how many *events* were seen and never how much air was claimed — without which a sensor at a 1.5 s cadence would read as one interval of 75 s "on air" holding 1 s of emission, the hull pathology one level down. **The ISM reading is unaffected and asserted:** under contiguous coverage the window closes 2 s after the measured end, fifteen times inside a 30 s cadence.
- **Revival appends, never duplicates — and the threshold is the idle gap.** Whether a return is a *new interval* is an absence question, answered by `idle_gap` (a silence shorter than it is not evidence the emitter stopped); whether it is the *same emitter* is an identity question, answered by entity resolution. A returning signal that entity resolution places on the same emitter gets a **new interval on the same `emitter_id`** (a `presence-reopen` on the stream, drawn as a second box with the silence between them drawn as a gap, never one box stretched across it): `emitter_observation` is keyed by `(source_kind, source_id)`, so a new track is a new row on the existing emitter.
  - **The storage half is free; the *resolution* half was not** (*T-262, measured*). Getting a second interval costs nothing — but only once entity resolution puts the returning track on the same emitter, and it did not: the user's 99.8148/99.8151 pair failed the fingerprint match on **burst length** alone (0.68 s vs 0.37 s, normalised error 1.86), because that is a statistic of the 305 s and 59 s windows each row was watched over, not of the emission. Two observations whose presence intervals are **disjoint** were watched over different windows, so `period`, `duty cycle` and `burst length` carry no information about identity and are excluded (`Fingerprint::compare_across_silence`); centre, bandwidth, family, symbol rate, deviation and hop behaviour still apply, so distinct emissions stay distinct. This is the same narrow exclusion T-250 made in `relate::distinguishing_evidence`, applied one layer earlier. **Cost:** two emissions sharing a channel, a bandwidth and a family that never transmit at the same time can no longer be separated by duty cycle alone (the ISM case, measured at TM-9).
- **Bursts and chirps are first-class.** A one-off burst is one interval a few milliseconds long; a 10-second chirp is one interval with a 10-second extent. **Limitation:** one `(f_lo, f_hi)` per interval means a chirp's box is the **bounding box** of its sweep, with the per-detection ladder underneath. A swept polyline `f(t)` is a later refinement and is deliberately not in ADR-0017's plan.
- **Overlapping intervals across *rows* are an error signal** (*T-369, [ADR-0017](adr/0017-time-extent-signal-model.md) §4.1*). Within one emitter, overlapping source rows normalise into one interval (above). **Between** two emitters, a shared region in time *and* frequency is not something to normalise — it is proof the analysis is wrong, because two real emissions do not occupy one region and two that did would not demodulate. It triggers **re-analysis of the region** (`resolve_overlaps` stage 4), which merges the measured `f_lo`/`f_hi` of every detection behind every overlapping row into contiguous modes and either collapses the rows into the one emission those modes show (`emitter_relation`, `detail.verdict = "one-emission"`) or records the region **contested** (an `active = 0` row carrying `detail.blocked_by`) and leaves both rows listed. It never merges past `relate::distinguishing_evidence`, and it is bounded by `relate::REGION_MAX_ROUNDS` because it runs on a live serving path.
- **An appearance is exactly one presence interval.** This gives ADR-0016 §5's cluster-visibility gate ("≥ 3 appearances of one emitter across ≥ 2 sessions") a precise meaning, and fixes the rule that **cluster membership counts emitters, never intervals** — otherwise one intermittent sensor would trip a cluster's ≥ 3-member gate on its own.
- **Retention & size:** rows are small and kept with their Emitter; they outlive the raw detections they summarise, which is what lets History answer for one-offs after detection rows have aged out.
- **Storage:** `emitter_observation`; ADR-0017's migration 0012 adds `idx_emitter_observation_time (emitter_id, t_start, t_end)` and nothing else — **no `closed_at` column, no persisted decay score** (a mutable score on `emitter` would be a second `count` waiting to happen; if one must persist, it goes in an append-only table like `emitter_lifecycle`).
- **Tests:** replay a signal that stops and returns; assert **two** intervals on **one** emitter, correct durations, and `liveness: ended` then `live`. Replay a one-off burst; assert one closed interval of the burst's duration, present in History and **absent** from a live window that has moved past it. Assert a window query returns a row only when an interval intersects the window, never on hull overlap.

### 2.28 TrunkSystem  [C23] (*T-266*, [C23 card](capabilities/C23-trunking-follow.md) §Interface)
A trunked land-mobile system as **measured**, not as a catalogue describes it (`hk_model::trunking::TrunkSystem`): `trunk_system_id`, `protocol` (`p25-phase1` / `p25-phase2` / `dmr-tier3` / `smart-net` / `edacs` / `nxdn-type-c` / `mpt1327` / **`unknown`**), `system_id` and `site_id` as decoded, `cc_freq_hz`, the **channel table** (per `iden`: `base_hz`, `spacing_hz`, `tx_offset_hz`, `bandwidth_hz`, `slots`, decode time), **neighbour sites**, **talkgroups**, `first_seen`/`last_seen`, `created_at`/`updated_at`.
- **Found before it is identified.** A control channel is confirmed by frame sync plus CRC (T-267) before any protocol decoder names it, so `protocol` may be `unknown` and the identifiers `NULL`. Two systems whose ids have not been decoded stay two rows: nothing is merged on the strength of not knowing. `cc_freq_hz` is `NULL` for a system with **no dedicated control channel** (Capacity Plus rest channel, NXDN Type-D, LTR) — never `0`.
- **The channel table is append-only, with the time each entry was decoded**, so a **stale** `IDEN_UP` table is detectable instead of silently mapping a grant to the wrong frequency (a C23 pitfall). The current table is the newest entry per `iden`.
- **An entry carries its TDMA `slots`, and a channel number is read through it** (*T-272*, migration 0016): `f = base_hz + spacing_hz × (channel / slots)` and `slot = channel % slots`, with `slots = 1` (FDMA) the identity and no slot at all. A P25 Phase 2 system announces `IDEN_UP_TDMA`, whose channel type names 2 or 4 slots per carrier, so **consecutive channel numbers are one frequency on different slots** — two talkgroups, not two channels. An entry stored without its slot count is ambiguous between the two readings and a reader re-deriving a frequency from it lands half a channel out, which is C23's TDMA slot mix-up pitfall preserved in the database. `protocol` is `p25-phase2` when a corroborated TDMA entry exists: a Phase 2 system's control channel is a Phase 1 channel, so the band plan is the only thing that can say.
- **Talkgroup labels are suggestions** from a prior (C17) or the user, carried with their source, and never what a call is matched on.
- **Storage:** `trunk_system` (+ `trunk_channel_plan`, `trunk_neighbour`, `trunk_talkgroup`), migration 0013. Fully columnar — the three child tables are the single copy of each fact.
- **Tests:** `hk-model` `repo/trunking.rs` — round trip with children, newest-entry-per-`iden`, append-only channel table, natural-key lookup.

### 2.29 CallRecord  [C23] (*T-266*)
One followed call, **metadata only** (`hk_model::trunking::CallRecord`): `call_id`, `trunk_system_id`, `t_start`, `t_end` (`NULL` while open, truncated, or never observed), `observed_until` (*T-308*), `talkgroup`, `unit_id`, `channel`, `slot`, `f_hz`, `encryption`, `late_entry`, `emitter_id` (the emission it rode on), `reasons[]`.
- **`t_end` and `observed_until` together say which of three things a row means** (*T-308*), because `t_end IS NULL` alone had to carry two opposite claims — "it is still running to the live edge" and "we stopped looking". `observed_until` is the last instant the receiver was **actually observing that channel**: a fact about the receiver's schedule, the call layer's version of the coverage map's grey.

  | `t_end` | `observed_until` | meaning | duration |
  |---|---|---|---|
  | set | set | the end was **observed** (T-269's 90 ms silence timeout) | exact |
  | `NULL` | set | **truncated**: present at `observed_until`, nothing claimed after it | a **lower bound** |
  | `NULL` | `NULL` | never observed (grant outside the window, or unmapped channel) | unknown |

  `CallRecord::ending()` returns that as a `CallEnding`, and `duration_is_lower_bound()` / `end_is_observed()` are how a consumer is kept from reading a truncated row as an end. **A call is never closed at the buffered window's edge** — that would manufacture a boundary the radio never produced, and would systematically under-state every call longer than the dwell (the observation-measuring-itself defect, T-281, in the call layer).
- **A call may be continued across passes, over a gap no end could hide in** (*T-308*): the bound is the silence timeout **itself** (90 ms), so continuity is asserted only where the same rule the in-pass splitter uses could not have missed an end, and the same row grows instead of a second one appearing. The built-in hunt observes 0.5 s of every 10 s, so its ordinary gap is 9.5 s — 105× the bound — and continuation correctly never fires there; those calls stay truncated. Error direction: continuing over ≤90 ms can over-state by at most that gap (and only merges what the in-pass rule already merges); refusing to continue never over-states, it leaves a lower bound.
- **There is no CallAudio object and no column that could hold one.** M4 records *that* a call happened and never its content, which is why the roadmap's vocoder-IP question gates none of this work. Adding audio is a separate decision.
- **Encryption is three-state — `clear` / `encrypted` / `unknown` — with no default-to-clear path.** The C23 pitfall is late entry without a header, where the status must read `unknown`. The model makes the mistake unconstructible rather than merely discouraged: `Encryption` has **no `Default`**; its `Clear` and `Encrypted` variants each *require* an evidence field (`algid` / `service-options` / `dmr-pi` / `lc-header` / `user`) plus optional ALGID and Key ID, while `Unknown` carries none; and `is_clear()` matches `Clear` alone, so any gate written against it fails closed. The schema repeats it: `encryption` is `NOT NULL` **with no DEFAULT**, `CHECK ((encryption = 'unknown') = (encryption_evidence IS NULL))`, and a clear row may not carry an encrypting ALGID. The repository's read path errors on a contradictory row instead of coercing it. `unknown` means *not measured*, never a value (the same rule as T-164's `duty_cycle` and T-207's "Not yet classified").
- **A call never walks back towards clear.** Later evidence may sharpen the state, but a call once seen encrypted stays encrypted (repository check plus a schema trigger): a mid-call key change must not read as "listenable after all". **Nothing decrypts anything** — this is a correctness and safety requirement, not a legal one.
- **A TDMA call's `slot` comes from the grant; its boundaries do not** (*T-272*). Both slots of a P25 Phase 2 carrier key one transmitter, and M4 demodulates no TDMA burst timing, so a followed call's start and end are the **shared carrier's** envelope and the row says so with the `tdma-shared-envelope` reason. Two talkgroups on alternating slots of one frequency are therefore two distinct `CallRecord`s with the same boundaries, correct slots and their own talkgroups — never one merged call.
- **Identity & lifecycle:** aggregate — opened from a grant, its end and its sharpened encryption state filling in.
- **Storage:** `call_record`, migration 0013, indexed by `(system, t_start)`, `(system, talkgroup, t_start)` and `emitter_id`; `observed_until` added by migration 0018 with a trigger pair refusing a row whose observation stopped before the call started or before the end it claims to have measured, plus a partial index for the truncated set (`Repository::truncated_calls`).
- **Tests:** `hk-model` `trunking.rs` — the three endings and the contradictions validate refuses (T-308); `repo/trunking.rs` — a truncated call round-trips as truncated and closing it later moves the observation boundary with the end; `hk-pipeline` `chains/trunk.rs` — the continuation bound *is* the silence timeout and the duty cycle's 9.5 s gap is refused. `repo/trunking.rs` — a record built from a late-entry grant reads back `unknown`; the column cannot be omitted; `clear` without evidence, `unknown` with evidence and a clear row with an encrypting ALGID are all refused; no downgrade from encrypted; `no_audio_column_exists`.

### 2.30 GrantEvent  [C23] (*T-266*, AWARE-067)
The append-only stream of what a control channel said (`hk_model::trunking::GrantEvent`): `trunk_system_id`, optional `call_id`, `kind` (`grant` / `grant-update` / `call-start` / `call-end` / `denied` / `outside-window` / `unmapped-channel`), `t`, `talkgroup`, `unit_id`, `channel`, `slot`, `f_hz`, `encryption` **as that message stated it** (usually `unknown` for a bare grant update), and machine `detail`.
- **A grant that could not be followed is a row, not a silence.** `outside-window` records a grant beyond the ≤20 MHz dwell span and `unmapped-channel` one with no (or a stale) `IDEN` — logged, never silently dropped.
- **Relationships:** drives the metadata-only trunking load index (AWARE-067, T-273); links to the CallRecord it opened.
- **Storage:** `grant_event`, migration 0013, append-only by trigger, indexed by `(system, t)` and `(call_id, event_id)`.
- **Tests:** `hk-model` `repo/trunking.rs` — append-only triggers, time-ordered reads, each event's own encryption state.

### 2.31 GnssObservableEpoch / SvAcquisition  [C36] (*T-274*, [ADR-0018](adr/0018-gnss-known-code-exception.md))
What a software GNSS receiver reports, and the one object in this model produced by a **known-signal-led** path rather than by blind detection.

- `GnssObservableEpoch` (`hk_gnss::GnssObservableEpoch`): `t`, `svs: Vec<SvObservable>` (`prn`, `cn0_dbhz`, `doppler_hz`, `elevation_deg`, `locked`), optional `position` (ECEF) and `clock_bias_s`. Pseudorange and carrier phase are **deliberately absent**: they need tracking loops and a decoded nav message, and T-274 built neither — empty fields would imply a capability that does not exist.
- `SvAcquisition` (`hk_gnss::SvAcquisition`): `prn`, `doppler_hz`, `code_phase_chips`, `peak_ratio`, estimated `cn0_dbhz`. Every result carries `AcquisitionEvidence::KnownCodeCorrelation { codebook }`.
- `AcquisitionResult` also records **the bar the search actually applied** and the geometry that set it: `threshold_ratio` and `search_cells` (code phases × Doppler bins). T-414: `peak_ratio` is a maximum over that many cells, so a bare ratio means nothing without the count — the acquire/reject bar is stated as `AcquisitionThreshold::FalseAlarm(p)` (default `1e-4` per satellite) and derived per search, and a fixed `PeakToMean(r)` that noise would clear is **refused** (`AcquireError::ThresholdUnsound`) rather than run.
- **These types live in `hk-gnss`, not in `hk-model`, and that is the point.** An acquisition is not a `Detection` (§2.9). Putting it in the shared model crate would make "found by despreading a known code" part of the vocabulary the blind inventory speaks, which is exactly the leak ADR-0018 exists to prevent. `crates/hk-gnss/tests/blind_path_boundary.rs` asserts no `hk-gnss` source names a `Detection`.
- **Relationships:** feeds C12/C30 as evidence for `Anomaly`/`Explanation` on the attack map; a *jamming* verdict reaches that path **without** any of these objects (§2.18 Anomaly from the ordinary C08 floor tracker), which is what keeps AWARE-002/AWARE-003 blind.
- **Storage:** none yet. Nothing persists these; the crate is not wired into `hk-pipeline`.
- **Tests:** `hk-gnss` `below_noise_acquisition.rs` (blind energy detection cannot see L1 — 0.02 dB spectral difference — while known-code correlation recovers the satellite from the same IQ, **at the crate default bar**), `noise_only_threshold.rs` (T-414's pair of controls: zero acquisitions from noise at 2.046 / 4 / 20 Msps where the old fixed 2.5 would have taken 32 of 32 off the same profiles, plus the refusal), `blind_path_boundary.rs` (the dependency-graph guard), `integrity.rs` (jamming flags with observables absent).

### 2.32 HarmonicFamily  [C40] (*T-374*, from T-317's method)

**What it is.** Several measured emitters declared to be harmonics of **one fundamental that was never itself detected** — `f = n·f₀`, fitted over their measured centres, with the corroborating physics checked. It is the one relationship in this model whose *cause has no row*: T-317 identified the FM capture's unexplained 100.465339 MHz emission as harmonic 43 of a free-running ~2.3364 MHz oscillator that sits outside every band ever tuned. §2.11's `emitter_relation` (migration 0008) cannot express that — every row there names a `source_id`, one emitter deferring to another, and it binds pairs rather than sets. A nullable source plus a synthetic emitter for the fundamental would put a never-measured frequency into the inventory, which is the exploration-first rule inverted.

- **It is device-local, like every other artifact claim** (T-302/T-259/T-305). A harmonic family is manufactured by one oscillator and one mixer, so the whole family carries one `ReceiveChain` (device + antenna port, the port compared only when both sides recorded one) and a family never spans two front ends however well the arithmetic fits. `hk_model::repo::harmonic` partitions the candidate rows by chain *before* the search, so the chain gate in the verdict is a second guard rather than the only one.
- **Four tests, and the model records what each is a property of.** (1) The **residual** about the fitted `f = n·f₀ + b`, against a tolerance from the measurement — the tighter of 5 ppm of the highest member frequency and 10 % of the narrowest member's width — never from the prediction, so the window cannot widen with the order. (2) **The intercept pins the indices**: shifting every index by one leaves the slope and *every residual bit-identical* and moves `b` by exactly one `f₀`, so the residual can never choose a labelling — only "a harmonic family passes through the origin" can, recorded as `origin_sigmas` (`|b|/se(b)`) *and* `origin_fraction` (`|b|/f₀`, because the first is self-scaling and a sloppy fit would otherwise buy its own acquittal). `index_pin` (`se(b)/f₀`) is the **non-vacuity** number: past ½ the labelling is a coin flip. (3) **Width ∝ n**, the one genuinely independent corroboration, since the widths are a column the fit never touched: a fundamental's frequency noise multiplies with `n`, so `wᵢ/nᵢ` is constant. `width_index_leverage` and `width_separates` record honestly whether `n` spans enough for that to be distinguishable from `w = const` — at T-317's 45/43 it is not, and the evidence confirms the scale without separating the models. (4) **Line shape**, optional, and able only to reject.
- **Identity & lifecycle:** `family_id` (monotonic). **Append-only**, like `emitter_relation`: never mutated, never deleted, enforced by triggers. A revocation appends a new row with `active = 0` and `supersedes` set to the row it retires, so the reasoning of both survives; members keep their emitter rows, detections, tracks and history throughout, because a relationship is ranked evidence and never an automatic delete.
- **Relationships:** `harmonic_family_member` joins it to three or more `Emitter` rows, each with its index `n ≥ 2`, measured centre, residual and width. `n = 1` is the fundamental itself, which is `emitter_relation`'s `artifact-of / harmonic` case and not this one. Nothing here reads a catalogue: `f₀` comes out of the measured centres, so a band plan may later *suggest* a name for the oscillator and may never supply one.
- **Retention & size:** two small tables, one row per claim plus one per member; kept with the emitters they name.
- **Tests:** `crate::harmonic_tests` runs T-317's own measurement (indices 43/44/45, `f₀` = 2.336398 MHz, residual 190.7 Hz against a 525.7 Hz tolerance, `index_pin` 0.0044, `origin_fraction` 3.07e-5, width/n = 138/120/137 Hz) **blind, mixed into real unrelated stations**, and asserts the search finds exactly that family. The negative control is the point of the ticket — real unrelated emitters and 4400 randomly drawn populations must *not* be declared families — and it is what set the thresholds: an earlier width-only tolerance declared **55.5 %** of unrelated FM-band draws to be families. Every `FamilyRejection` variant has a case that only it rejects, and `within_tolerance_the_indices_are_pinned_by_construction` enumerates the bound that makes the mechanism non-vacuous. `crate::repo::harmonic_tests` covers storage, idempotence, revocation and the two-front-end refusal.

## 3. Storage (provisional — Phase 3 storage ADR finalises)

Three stores under one per-device data directory, so the whole state is one thing to back up, export, or wipe:

### 3.1 Relational state — SQLite (candidate; DuckDB considered)
Holds ScanPlan, Survey, Provenance, CalibrationState, SpurMask, Detection, Track, Emitter, Recording (rows, not bytes), Annotation, Demodulation, Decode, Bitstream descriptors, ExternalEvent, Anomaly, Explanation, trunking metadata (TrunkSystem §2.28, CallRecord §2.29, GrantEvent §2.30 — never call audio), and user metadata (Bookmark, Selection §2.20). SQLite for one-writer simplicity and ubiquity on the Jetson; DuckDB if analytic region/time scans dominate. **Provisional**; the ADR decides, and the pick is isolated behind a repository layer so it's reversible.

### 3.2 Spectrum history — tiled pyramid
SpectrumTiles in a columnar store (Parquet) or a purpose-built ring of downsampled tiles, partitioned by time and frequency block, with a resolution pyramid (recent = fine, old = coarse). Fixed rolling byte budget from config. This is the object that makes "region over time" cheap and bounds disk.

### 3.3 IQ / audio / bits — SigMF files on disk
Recordings and stored Bitstreams are SigMF datasets (data + meta) referenced by URI. Annotations live in the SigMF meta so a recording is self-describing and portable. Quota-managed, ranked by interestingness, `content_class`-gated.

**Disk budget sketch** (device with, say, a 256 GB–1 TB NVMe): relational state MB–low GB; spectrum history a fixed 5–20 GB pyramid; the rest is the IQ snippet pool, quota-capped, oldest/least-interesting evicted. A storage/retention policy object (owner TBD in the ADR, docs/06 §5) enforces the split.

## 4. The central query: "what has this region looked like over time?"

The product's core question (workflow step 3). It resolves against three stores and unions the results into a region-over-time view:

1. **Occupancy & shape** from SpectrumTiles: `WHERE f overlaps [f_lo,f_hi] AND t in [t0,t1]` at the coarsest level that meets the requested resolution → max-hold, mean, percentile bands over time.
2. **Events** from presence intervals (§2.27, indexed by `(emitter_id, t_start, t_end)`), Detection (indexed by `(f_lo,f_hi)`,`t_start`) and Track → the bursts/emitters active in that box, with SNR and timing. The interval query is what the **History surface** and the Explore time window both read, and what makes scrubbing one indexed range query rather than a detector replay ([ADR-0017](adr/0017-time-extent-signal-model.md) §2.4).
3. **Identities & status** from Emitter overlapping the region → known/unknown, classification, decoded ids, tags.
4. **Explanations** from Anomaly/Explanation whose region intersects → "this band got busier on the 3rd; likely cause: …".

Because measurements are immutable and interpretations are versioned, the same query re-run after a classifier upgrade yields better identities over the *same* history. Region and time are the two indexed axes throughout.

### 4.1 A span request and its response (T-334, span-matched resolution)

**The rule, settled by the user** (CLAUDE.md, "Time, the waterfall, and the live view", invariant 4): *data, timestamps and span-matched resolution are the backend's responsibility; time↔pixel mapping and view state are thin-client presentation.* The visible span is user-selectable from seconds to the full retention, and **zooming re-scales rather than truncates**. This is the thin-client rule applied to the **time** axis — the axis mapping is presentation; *choosing which value represents an interval* is a measurement, and it is made here, where the noise floor, the occupancy threshold and the cell shape are known.

**A span request carries**, beyond the region and window (`f_lo`/`f_hi`, `t0`/`t1`):

| | |
|---|---|
| **a product budget** | the largest response the caller can hold (`max_cells`). It bounds size; it cannot shape a grid, because 16 × 6000 and 600 × 160 satisfy it equally. |
| **per-axis budgets** | the rows and columns the view will actually draw (`max_t`, `max_f`). These are the request stated in the view's own terms, and they are what makes the served grid match the span. |
| **an origin filter** | optional `source`/`site` (§2.6, T-133), unchanged. |

**The response carries**, beyond the cells:

| | |
|---|---|
| **the grid geometry** | `t0_s` (the start of time row 0), `t_cell_s`, `nt`; `f_lo_hz`, `f_cell_hz`, `nf`. Row *k* starts at `t0_s + k·t_cell_s`; this is **contract, not inference** — a client reading a row's time from it is reading the grid the server described, not guessing one. `t0_s` may precede `t0`: the window is snapped *outward*, never clipped. |
| **the resolution actually served** | which pyramid level answered, what was asked for, and — when they differ — which budget was missed. It is not always the one requested, and a caller must never have to deduce that from the cell counts. |
| **which tier answered** | the tiered spectrum-history pyramid, or (later) a live-IQ-backed tier. The two have **different horizons** — the pyramid's retention is tiered, lossy and byte-budgeted; the IQ ring's window is short and lossless — and conflating them misreports what the device still holds. |
| **coverage** | unchanged (§2.5, C26): unobserved is never quiet, and a gap is never filled to make a grid look complete. |

**Error direction.** The pyramid's ladder is discrete (§3.2; scheme 1 steps 6.25 kHz × 1 s → 100 kHz × 1 day), so an exact match is not generally reachable. The rule is **the finest level that fits every budget**, which errs *coarser* than the view, never finer. That asymmetry is the point: a coarse cell drawn across several pixels repeats one measured value, while a finer grid reduced in the client invents the value a pixel stands for. Where no level is coarse enough, the response **says so** rather than leaving the caller to reduce silently.

The wire form is `GET /api/history` (`docs/api.md`, "Span-matched resolution").

#### 4.1.1 The capture window, and the overview drawn on it (T-338; the user's time/waterfall invariant 2)

**The rule, settled by the user:** *the timeline is the capture window, and it is a visualization.* The scrubbable capture-history timeline spans **exactly the configured recording/retention duration** — no more, no less — grows and shrinks when that duration is reconfigured, and is itself a **compressed "sideways" overview waterfall** of the retained capture, never an empty box.

**The horizon is a first-class field, because there are two of them.** §4.1's table already says the pyramid's retention and the IQ ring's window (§3.2, §3.3, [ADR-0014](adr/0014-iq-capture-ring.md)) are different lengths. Invariant 2 says which one the scrubber is: the **ring's**. A scrubber sized from the longer, lossy one offers times the ring has already overwritten — it promises capture that no longer exists, and it looks right while it does. So the capture window is served as its own object (`GET /api/timeline`), and it names its horizon rather than leaving it to be assumed:

| | |
|---|---|
| **the span** | the **configured** retention, not what the ring currently holds. A ring part-way through filling is a mostly-empty capture window of the full length; a band sized to its contents would grow under the user as it filled. |
| **the live edge** | the ring's newest sample, falling back to the history's newest frame while the ring is empty. Never wall clock — a replay runs on its own clock (§4.2), and a band anchored to `now` places its capture in the future. |
| **what is held** | reported *beside* the span, inside it, so the difference between "the window" and "the IQ that is still there" is drawn rather than inferred. |
| **absence** | no ring, no retention or no live edge is `null` — **unknown**, never a default span. |

**The picture on it is a measurement, and the pyramid alone cannot make it.** §4.1's error direction assumes one level can serve the view. A sideways band is fine in time and coarse in frequency at once, and the ladder **couples its axes**: the tier whose cells are coarse enough in frequency for a thin strip's few rows (100 kHz) has one-day time cells. The tier is therefore chosen from the *time* axis and its grid folded onto exactly the cells the band draws, in the backend — the same rule as §4.1, applied where a single level cannot reach. Only statistics that **fold exactly** survive it: max of max-holds, max of peak occupancy, summed frames, mean coverage over equal-duration cells. A percentile (`p_low`, `floor`) cannot be folded from cell values, so it is not offered rather than approximated. The grid's own observed range is served too, because choosing a dynamic range is a measurement as well.

The wire form is `GET /api/timeline` (`docs/api.md`, "The capture window, and the overview drawn on it").

### 4.2 What every time-varying record carries (T-337, one shared time axis)

**The rule, settled by the user** (CLAUDE.md, "Time, the waterfall, and the live view", invariant 1): *for the current view there is a single canonical mapping between absolute capture time and screen position, and everything time-varying is laid out through it and moves together.* §4.1 settled the **resolution** half of the thin-client line; this settles the **timestamp** half, and it is a constraint on the data model, not only on the wire:

> **Every record that varies in time carries the absolute capture time it happened at.** A record whose time a reader has to *infer* — from arrival order, from a sequence number, from an index into a buffer, from a declared rate, from when a response arrived — is a modelling defect, because inference is how an overlay drifts away from the data it describes.

This is why the model shapes below are what they are, and none of them is optional:

| Object (§2) | The time it carries |
|---|---|
| **SpectrumFrame / Sweep** | `t` — and, on the wire, the sample index beside it. A frame's `t` is its **first** sample (`SpectrumFrame.t.sample_index` is the STFT's `frame_start`), with `sample_count` giving its span: a frame is an interval, not an instant. |
| **Detection** | `time: TimeRange` — a detection is a time–frequency *region* (§1 invariant 1), so a start and a stop, never a bare "seen at". |
| **Track / presence interval** | `t_start`, `t_end`, `open`, and a **backend-computed** `duration_s` on the wire: a client never derives a timespan from two fields it was handed. |
| **Emitter** | `first_seen`/`last_seen` as a **hull**, explicitly not an extent — the extent lives on the intervals. |
| **SpectrumTile** | `t_start`, `t_end`, `level`; a cell's time is `t0_s + k·t_cell_s` (§4.1), stated, not counted. |
| **Selection** | `t_lo`/`t_hi`, or null — a *deliberately* timeless region is representable, and distinguishable from a missing timestamp. |
| **Provenance** | the time the settings it records were in force, so a detection's gain state and overload flags are pinned to when it was measured. |

**A declared rate is never a substitute.** A rows-per-second figure — a stream header's `sample_rate_hz`, a configured `spectrum_rows_per_s` — describes production, not the records in hand. It is wrong in two directions that both grow linearly with age: a gated stream declares a rate deliberately above the actual one (`RowPlan::declared_hz`, ×1.1), and gated, dropped or skipped records advance capture time without advancing any buffer. Sample index and timestamp are the clock; the rate is a hint about throughput.

The wire form and the client's obligations are `docs/api.md`, "One shared time axis"; the UI consequences are docs/14; the signal-model consequences are ADR-0017 §2.4.2.

### 4.3 The achievable capture state, and the detail claim (T-341)

**The rule, settled by the user** (CLAUDE.md, "Time, the waterfall, and the live view", invariant 6): *navigation is discretized to achievable capture states, and the UI never implies detail the front end can't deliver.* §4.1 settled the resolution the backend serves and §4.2 the timestamps it carries; this settles the **third** thing a client must not decide for itself — **which states exist at all, and what the picture of one is evidence of.**

It is the same rule as absent-means-not-measured (T-297: no field is written for a region that was never swept, so nothing ever writes a zero rate), expressed on the navigation surface. There, a fabricated field is a number nobody measured; here, **an interpolated pixel that looks like a measurement is a lie with a picture attached**.

#### The achievable `(centre, span)` grid

A capture state is a point on a three-axis grid, and the model carried only two of the axes until T-341:

| Axis | Model field | Bounded by |
|---|---|---|
| centre bounds | `SourceCapabilities.frequency_ranges` | what the front end can tune to |
| span | `SourceCapabilities.sample_rates` — **a live window's span *is* its sample rate** | the instantaneous bandwidth |
| centre granularity | `SourceCapabilities.tuning_step` (**new**) | the synthesiser's own grid |

`tuning_step` is **three-valued, not a number**: `Unknown`, or `Uniform { step_hz }`. This is `BiasTee`'s shape (§2.6) for `BiasTee`'s reason — a source that *cannot report* and a source that *reports a value* are different facts, and collapsing the first into a benign default is the defect this project guards against. Reading "nothing said" as 1 Hz would offer the user centres the radio cannot reach. A device driver states its own step; a SigMF replay reports `Unknown`, because a recording holds the centre it was made at and never the synthesiser grid of the device that made it.

**A step is a granularity, and it promises only that.** The device lands within half a step of a requested grid point; the residual is bounded by the step by construction, and nothing downstream may read a grid point as exact beyond it.

**The active capture windows are a list, not a singleton (T-340).** A run's *currently-active capture windows* — one per live front end, each `(device_id, centre, span)` with the window edges that follow from the span — are reported as an array (`GET /api/navigation`'s `windows`). The count is a fact about the run, measured per request, not a constant of the model: the source layer is already N-shaped (T-259's audit; T-302/T-303/T-304/T-305 key artifacts, baselines, history and the source-layer rule on the front end that produced each frame), and the user's multi-SDR direction is explicit. A consumer that reads one device's tuned state as "the window" would have to be re-shaped the day a second receive chain exists; one that reads the list shows exactly as many windows as were reported, which today is one. Multi-device *capture* is not built, and the list never claims it is: on a replay it is empty.

#### The detail claim

Every picture of a region says which tier it came from, as a claim about **how much detail it is evidence of**, ordered `live-iq` > `spectrum-history` > `survey-overview`:

| Claim | The picture is |
|---|---|
| **live IQ** | one capture window, at the resolution drawn. |
| **spectrum history** | measured, reduced to a pyramid tier's cells (§4.1). Never interpolated. |
| **survey overview** | wider than any single capture window, so stitched from separate dwells. |

The test is `span ≤ the widest instantaneous bandwidth`, boundary inside: a view exactly as wide as the sample rate is one window's worth, and a hertz past it had to come from a different dwell. **A surface that cannot establish the stronger claim makes the weaker one** — not knowing the window is not evidence that a span fits inside it.

#### Error direction on every axis

Each axis errs in the direction where the user loses *choice* rather than *truth*:

| Axis | Errs | Because the other way |
|---|---|---|
| centre | to a **coarser** grid than the hardware's | offers centres that do not exist while the axis claims otherwise |
| span | **down** to an achievable rate | promises a window the device cannot open |
| time cell | **coarser** (§4.1's rule) | invents the value a pixel stands for |
| the claim | to the **weaker** one | over-claims detail nobody captured |

The wire form is `GET /api/navigation` and `resolution.source` on `GET /api/history` (`docs/api.md`); the UI consequences are docs/14; the navigation surfaces that consume the grid are ADR-0017's Explore/History split and the edge navigators (T-340), the capture timeline (T-338).

### 4.4 The coverage map: grey means genuinely unobserved (T-368)

§4.3 stops a view **claiming** detail the front end never captured. This is the other half of the same honesty: a view may **show** what the front end did capture, and must grey only what it did not.

> **The waterfall shows the data that exists for the selected (time, frequency); grey means genuinely unobserved.** … This requires the backend to keep a **coverage map derived from the SDR configuration/tune history** — for each interval, which centre/span/rate (and which device) was active — so observed-vs-unobserved is computed from what was actually sampled, and the frequency navigator's survey view is built from that same coverage. (User invariant, 2026-09-16.)

#### The three states, and why the third is a type and not a null

| State | Meaning | Model |
|---|---|---|
| 1 | observed, and there was energy | `Coverage::Observed(Sampled)`, level high |
| 2 | observed, and it was **quiet** — a finding | `Coverage::Observed(Sampled)`, level low |
| 3 | **never observed** — no claim either way | `Coverage::Unobserved` |

States 2 and 3 are the pair that gets collapsed, and collapsing them is how a view comes to report "nothing here" about spectrum nothing ever looked at — an absence-of-signal finding invented out of an absence of measurement. So the model makes state 3 **unrepresentable as state 2**: `Sampled` has no value meaning "nothing was sampled" (`hk_store::coverage::Sampled::new` refuses a zero span count or a zero sampled duration and yields `Unobserved`), and on the wire an unobserved cell carries **no measurement keys at all** rather than null ones. Same rule as `BiasTee::Unknown` ≠ `Off` and `Encryption::Unknown` ≠ clear: *nothing said is never permissive.*

A fourth thing exists and is deliberately distinct: **observed, level not retained** (`shade: null` on an observed cell) — the radio sampled here but the spectrum-history pyramid keeps no value for it. Drawn as neither grey nor the bottom of the ramp.

#### Coverage is device-local

Coverage is a fact about **one front end** (§2's provenance rule; T-259/T-305). Two radios covering disjoint ranges are two coverage grids, each unobserved exactly where the other looked — never merged into a claim that either saw both. `Device::Unknown` is its own device, not a wildcard: a span whose record did not name the radio is evidence that *something* looked, never that a *particular* front end did. A union across devices exists only as an explicitly-requested, explicitly-labelled `Device::Any`.

#### Derived from provenance already written, not a new ledger

No new record is journalled. Two existing ones already say "for each interval, which centre/span/rate was active"; one of them also says which device.

| Source | Interval | Centre/span/rate | Device | Horizon |
|---|---|---|---|---|
| **IQ ring journal** (§2's `Recording`/ADR-0014 ring segments) — a new segment on **every** provenance change, so retunes are segment boundaries by construction | yes | yes | **yes** (`Provenance::device_id`) | the ring's configured retention |
| **Observation log** (`DwellRecord`/`SweepRecord`, ADR-0012 §1; `ObservedWindow::covered()` already removes the DC notch) | yes | yes | **yes** — `device_id`, T-378 | 30 days |

**One spelling of "which device" (T-378).** `DwellRecord::device_id` and `SweepRecord::device_id` carry the source's own `DeviceInfo::device_id` — the same string `ChainKey::of_device` (T-303/T-314) and `hk_store::history::source_key` (T-304/T-377) are hashed from. Four links of one chain of custody, one value: the baseline key, the history origin, the ingest floor key and the observation record all name the front end with the same string, so there is no second way to say it and nothing to drift. Both writers take it from the source's identity, never from `PipelineConfig::device_id`, which is a config default that names no device that produced anything.

The field is `#[serde(default, skip_serializing_if = "Option::is_none")]`: a record written before T-378 reads back with **no** device, and coverage reads that as `Device::Unknown` — its own device, which never answers for a named front end and is never defaulted to whichever radio is running now. Reading it as the running device would invent provenance for data that has none, the same class of error as `BiasTee::Unknown` reading as `off`. Because the field is skipped when absent, a record with no device serialises to exactly the bytes it did before, so the CRC-checked line log's existing content is untouched.

With both histories device-local, coverage over the **whole** retention answers "did *this* front end look here", not merely "did anything" — which, with two SDRs, is the question. `GET /api/coverage`'s `sources[]` reports `named_spans` beside `spans` per record kind, so a log still holding pre-T-378 lines discloses them rather than claiming a device-local horizon it has not got.

The wire form is `GET /api/coverage` (`docs/api.md`); the surface it fills is docs/14's frequency navigator; the rule it serves is ADR-0017 §2.4.5.

## 5. Worked examples

### 5.1 Science — natural radio noise-floor survey (SPACE-050)
A scheduled wide Survey produces SweepFrames across 10 kHz–1 GHz (the sub-1 MHz part via a VLF front end, `needs-accessory`). C08 computes per-bin NoiseFloor; C05 supplies CalibrationState so power is dBm. Frames fold into SpectrumTiles. Nightly, the region-over-time query on the tiles yields a calibrated noise-floor-vs-time-vs-frequency surface; C33 tags it as the radiometry product and compares to ITU-R P.372. No Emitter needed. **Objects:** ScanPlan → Survey → SweepFrame(+Provenance→CalibrationState) → SpectrumTile → (query) → radiometry product. **Test:** replay a synthetic capture with a known injected floor; assert recovered floor within ±1 dB after calibration.

### 5.2 Attack map — GNSS jamming near 1575 MHz (AWARE-006 / AWARE-044)
A dwell on L1 shows a broadband noise-floor rise with no new structured Emitter. C08→C12 emit an Anomaly (`noise-floor-rise`, region 1575 MHz ± , now). C29 has a cached gpsjam ExternalEvent (region+day) and TLE-derived facts. C30 correlates the Anomaly with the event by time-coincidence and geography and writes an Explanation ("GNSS L1 jamming reported in your area today") with evidence links; the user confirms, writing an Annotation. **Objects:** SpectrumFrame(+Provenance) → Anomaly → Explanation ← ExternalEvent; Annotation on confirm. **Test:** frozen cache + synthetic floor rise → deterministic Explanation with gpsjam as top cause; no explanation when the cache has no matching event.

### 5.3 Unknown signal — mystery ISM burst (AWARE-036)
A dwell on 902–928 MHz yields repeated Detections. C10 links them into a Track (period ~30 s, constant 40 kHz BW). C18 clusters to a new fingerprint → a new Emitter with `known_status: unknown`. A pre-trigger Recording (SigMF) is kept. C13/C14 estimate FSK, symbol rate, deviation → Demodulation → soft bits → Bitstream. C21 infers preamble/sync/CRC across many bursts → a Decode with a draft framing and, once a CRC validates, a `ground-truth` Annotation and a candidate signature. The Emitter now carries a fingerprint, an estimated protocol, and links to its Recording and Track. **Objects:** Detection → Track → Emitter + Recording → Demodulation → Bitstream → Decode → Annotation. **Test:** replay a synthetic FSK sensor; assert one unknown Emitter, correct estimated symbol rate/deviation, recovered framing, and a CRC-valid decode that flips `known_status` toward identified.

## 6. Open questions (also in planning-log.md)

- **Storage engine** (SQLite vs DuckDB; Parquet vs custom tiles) — Phase 3 storage ADR.
- **`content_class` gating point** — provisionally C24 enforces; confirm in a Phase 3 ADR.
- **Own-key decryption** modelled as a C22 stage with key-source provenance — confirm.
- **Retention/quota policy object** ownership across Recording/SpectrumTile/Detection — Phase 3.
- **Timestamp error budget** for Provenance — Phase 4 spike (no hardware 1PPS on HackRF One).

## 7. Time-bounded signals and a view-scoped inventory (2026-09-16)

The model is **settled by the user** (CLAUDE.md, "Signal & inventory model") and recorded in **[ADR-0017](adr/0017-time-extent-signal-model.md)**. Its data-model consequences are folded in above: §2.9 (a Detection *is* a time–frequency region), **§2.27 (the presence interval — where the time extent lives)**, §2.11 (`first_seen`/`last_seen` demoted to a hull; `count` demoted to a History-only total; derived liveness), §2.20 (a Selection is the user-authored counterpart), §2.21 (`family_in_window` as an additive projection, arbitration unchanged) and §4 (intervals as an event source).

**Stages TM-1 … TM-4 of ADR-0017 §9 need no migration**; TM-5 needs one index-only migration 0012. **The staged plan awaits the user's sign-off — do not implement ahead of it.**
