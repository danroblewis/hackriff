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
The trust record attached to every SweepFrame, SpectrumFrame, Detection and Recording: source `device_id`, tune (`f`, `fs`, LNA/VGA/amp gains), sticky `overload` flag, `quantisation_limited` (noise floor within 3 dB of the ADC quantisation floor; added from spike S4, 2026-09-13), temperature, active filter/antenna port (Opera Cake), clock source + lock, `calibration_state_ref`, `spur_mask_ref`, and the timestamp method + error budget.
- **Identity & lifecycle:** `provenance_id`; immutable; deduplicated (many frames share one provenance row when nothing changed).
- **Relationships:** references CalibrationState and SpurMask; referenced by frames, detections, recordings.
- **Retention & size:** small, deduplicated; kept as long as anything referencing it.
- **Tests:** assert every Detection has a resolvable provenance chain; assert overload/clip flags propagate to `suspect-IMD` on detections.
- **Note (from docs/06 §5):** timestamp method is host-arrival-time + running sample count, optionally GNSS-tagged; there is **no hardware 1PPS** on HackRF One, so sub-µs timing needs a GPSDO into CLKIN. Error budget is a spike (Phase 4).

### 2.7 CalibrationState  [C05]
Versioned calibration: frequency `ppm` + method (LTE PSS / FM pilot / GNSS) + time, power-cal table ref (dBFS→dBm over frequency×gain), validity window / temperature.
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
- **Retention & size:** ~a few hundred bytes/row; millions over months = hundreds of MB; indexed by `(f_center, t_start)` and `(f_lo, f_hi)` for region queries. Aged by quota, oldest/least-interesting first, but summarised into Emitter/Track before deletion.
- **Tests:** replay a fixture; assert detection count, center/bandwidth/SNR within tolerance, and false-alarm rate under threshold on a noise-only capture (the SNR-wall check).

### 2.10 Track  [C10]
A linked series of Detections (same `f±ε`, similar BW) with timing features: periodicity, duty cycle, inter-arrival stats, hop set + rate, TDMA frame period, inter-channel co-occurrence.
- **Identity & lifecycle:** `track_id`; grows as detections arrive; closed after an idle timeout; may merge/split (recorded, not overwritten).
- **Relationships:** groups Detections; belongs to an Emitter (or is a candidate).
- **Retention & size:** compact; kept with its Emitter.
- **Tests:** synthesise a periodic/hopping emitter; assert recovered period, hop set and duty cycle within tolerance.

### 2.11 Emitter  [C27] (the inventory entry)
The persistent "thing seen on the air": `emitter_id`, current `f`/`BW`, `fingerprint` (C18), `first_seen`, `last_seen`, `count`, `classification` (family + confidence + open-set score + model version; from M3 the full §2.21 Classification) as an **append-only history** (not overwritten), with the current family picked by arbitration rank (§2.21), `identity` (decoded id such as ADS-B hex, RDS PI, MMSI, talkgroup) or `unknown`, `known_status` vs priors (`known` / `unexpected-here` / `unknown`), `tags`, and links to Tracks, Detections, Recordings, Demodulations, Explanations, Annotations.
- **Identity & lifecycle:** `emitter_id` stable for the life of the cluster; created when detections cluster to a new fingerprint (C18); `last_seen`/`count` update continuously; classification is re-run and appended as models improve — the measurement it ran on is unchanged.
- **Time extent lives on the presence interval, not here** (*[ADR-0017](adr/0017-time-extent-signal-model.md) §1, §8*). An Emitter is an **identity that owns an ordered set of disjoint presence intervals** (§2.27). Consequently:
  - **`first_seen`/`last_seen` are the *hull* of that set, never its extent.** An emitter that fired at 09:00 and at 17:00 has an eight-hour hull that is 99.99 % silence. **Never display the hull as a duration**, and never treat it as "how long this was on air". The inventory time filter must test *interval overlap*, not hull overlap — today's `last_seen >= t0 AND first_seen <= t1` (`repo/inventory.rs`) is a hull test and matches every window between two distant sightings.
  - **`count` is a lifetime total, valid only in History.** It is **excluded from every liveness decision and from live-list ranking**. It was previously the only place "this is still here" could be written down, which is why it grew without bound (38 → 582,500/h, observed 2026-09-16); the open interval's advancing `t_end` is where that information belongs. Its existing uses in `recurrence.occurrences` and `ConfirmPolicy` are unchanged.
  - **Derived, window-scoped fields** for a request window `[t0, t1]`: `intervals_in_window`, `on_air_s_in_window`, and `liveness` ∈ `live` (an interval is open at the live edge) / `ended` (its latest in-window interval is closed, with `ended_t_s`) / `absent` (no interval intersects; Confirmed rows only). All derived, none stored.
  - **An emitter that is a set of disjoint events is still one emitter.** Disjointness never argues for splitting an identity — a doorbell sensor is one device whether it fires once or a thousand times. What is wrong in that case is only the *rendering*: show "17 events over 6 h, 4.2 s on air", never "first seen 6 h ago, count 582,500".
- **Inventory lifecycle (T-078):** `lifecycle_state` is `candidate` / `confirmed` / `deleted`, with an append-only history (`emitter_lifecycle`: new and previous state, author `auto` or `user`, actor = rule id such as `hk-pipeline/confirm@1` or the API token fingerprint, reason, `t`).
  - Every emitter starts as a **candidate**. Candidates carry **recurrence statistics** from the observation ledger: occurrences (`count`), appearances (track observations, or decoder sightings when there are no tracks), span, on-air time (Σ appearance span × measured duty cycle; unknown duty adds nothing), duty cycle, and the latest appearances.
  - **Auto-confirm** only on strong, unambiguous, blind evidence (`hk_pipeline::inventory::ConfirmPolicy`, configurable; defaults): a decoded transmitter identity carried by ≥ 1 CRC-valid decode (structural identities such as the blind framer's `other:hk-framing` signature do not count), **or** one continuous, trusted track: ≥ 2 s on air, duty cycle ≥ 0.8, ≤ 50 % suspect member detections and ≥ 1 trust-confirmed detection. Recurring intermittent, weak or suspect signals stay candidates until a user **promotes** them (`POST /api/inventory/{id}/promote`). No rule demotes or deletes.
  - **User delete** (`DELETE /api/inventory/{id}`) removes the entry from the inventory (`/api/inventory` lists it only with `state=deleted`) but keeps the row, its detections, tracks, links, decodes and classification/status/lifecycle history. A deleted row takes no further sightings and is never merged: entity resolution skips it (fingerprint, context, ledger, re-measurement), so **a later detection of the same signal creates a new candidate**; a decoded identity it held moves to the emitter that sighting reaches. There is no undelete. The region-over-time history query (§4) still sees deleted emitters.
- **One entry per physical emitter (T-082):** a decoder or chain output of an emission the tracker also followed (RDS PI on a WFM track, the blind framer's signature on an FSK sensor track) is linked to that track's entry instead of staying a second one.
  - **Same emission** (`Repository::same_emission`): detected or refined centres within the centre tolerance, observations overlapping in time, both hop sets or neither, not two identities; two track-based entries also need fingerprints within tolerance. The pipeline links only entries of the same run (capture) and never a channel-sharing transmitter identity (ADS-B ICAO, …) to a channel entry.
  - **Merge:** the confirmed entry survives, else the first seen. Links, observations (recurrence, overlapping observations not counted twice), tags, identity, classification history, refined tuning and a decoder/classifier/user status are kept. Confirmed wins over candidate, and a lifecycle history row records a confirmation carried by a merge. A deleted entry is never merged, so re-detection after delete still creates one new candidate.
- **User band (T-191):** an optional user-adjusted `f_lo`/`f_hi` with `set_at`, actor (token fingerprint) and reason (`emitter_user_band`, one current row per emitter), stored beside the measured `f`/`BW`, which it never overwrites and which detection, tracking and entity resolution keep using. Set/clear through `PUT`/`DELETE /api/inventory/{id}/band` (audited). Validated: finite, `0 < f_lo < f_hi`, width ≤ 40 MHz, overlapping the measured band or within 1 MHz of it. On merge the survivor keeps an override if either entry had one, the latest `set_at` winning.
- **Relationships:** the hub. Loops with Explanation (subject or cause) and with Decode (identity) and priors (C17).
- **Retention & size:** thousands of rows; never auto-deleted by the system (it's the memory; only a user deletes an inventory entry, and the row stays as history); links may outlive the raw detections they summarise.
- **Tests:** replay two sessions of the same emitter; assert one Emitter with `count` summed and `first/last_seen` spanning both; assert an unknown signal yields `known_status: unknown` with a non-zero open-set score.

### 2.12 Recording  [C25]
A SigMF dataset reference: `recording_id`, `uri` (`.sigmf-data` + `.sigmf-meta` paths), `type` (`iq-snippet` / `channel-decimated` / `audio`), `t_span`, `f_center`, `fs`, `trigger` (`detection_ref` / `manual`), pre/post-trigger seconds, `size_bytes`, `retention_class`, `content_class` (for gating), `provenance_ref`, embedded `annotations[]`.
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
Structured output from a decoder plugin or bit-framing inference: `decode_id`, `demodulation_ref` (or `recording_ref` for replay), `decoder_id` + version, frame model / fields, `crc_status`, decoded `identity`, `content_class`, `t`.
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
- **Revival appends, never duplicates.** A returning signal that entity resolution places on the same emitter gets a **new interval on the same `emitter_id`**: `emitter_observation` is keyed by `(source_kind, source_id)`, so a new track is a new row on the existing emitter.
  - **The storage half is free; the *resolution* half was not** (*T-262, measured*). Getting a second interval costs nothing — but only once entity resolution puts the returning track on the same emitter, and it did not: the user's 99.8148/99.8151 pair failed the fingerprint match on **burst length** alone (0.68 s vs 0.37 s, normalised error 1.86), because that is a statistic of the 305 s and 59 s windows each row was watched over, not of the emission. Two observations whose presence intervals are **disjoint** were watched over different windows, so `period`, `duty cycle` and `burst length` carry no information about identity and are excluded (`Fingerprint::compare_across_silence`); centre, bandwidth, family, symbol rate, deviation and hop behaviour still apply, so distinct emissions stay distinct. This is the same narrow exclusion T-250 made in `relate::distinguishing_evidence`, applied one layer earlier. **Cost:** two emissions sharing a channel, a bandwidth and a family that never transmit at the same time can no longer be separated by duty cycle alone (the ISM case, measured at TM-9).
- **Bursts and chirps are first-class.** A one-off burst is one interval a few milliseconds long; a 10-second chirp is one interval with a 10-second extent. **Limitation:** one `(f_lo, f_hi)` per interval means a chirp's box is the **bounding box** of its sweep, with the per-detection ladder underneath. A swept polyline `f(t)` is a later refinement and is deliberately not in ADR-0017's plan.
- **An appearance is exactly one presence interval.** This gives ADR-0016 §5's cluster-visibility gate ("≥ 3 appearances of one emitter across ≥ 2 sessions") a precise meaning, and fixes the rule that **cluster membership counts emitters, never intervals** — otherwise one intermittent sensor would trip a cluster's ≥ 3-member gate on its own.
- **Retention & size:** rows are small and kept with their Emitter; they outlive the raw detections they summarise, which is what lets History answer for one-offs after detection rows have aged out.
- **Storage:** `emitter_observation`; ADR-0017's migration 0012 adds `idx_emitter_observation_time (emitter_id, t_start, t_end)` and nothing else — **no `closed_at` column, no persisted decay score** (a mutable score on `emitter` would be a second `count` waiting to happen; if one must persist, it goes in an append-only table like `emitter_lifecycle`).
- **Tests:** replay a signal that stops and returns; assert **two** intervals on **one** emitter, correct durations, and `liveness: ended` then `live`. Replay a one-off burst; assert one closed interval of the burst's duration, present in History and **absent** from a live window that has moved past it. Assert a window query returns a row only when an interval intersects the window, never on hull overlap.

### 2.28 TrunkSystem  [C23] (*T-266*, [C23 card](capabilities/C23-trunking-follow.md) §Interface)
A trunked land-mobile system as **measured**, not as a catalogue describes it (`hk_model::trunking::TrunkSystem`): `trunk_system_id`, `protocol` (`p25-phase1` / `p25-phase2` / `dmr-tier3` / `smart-net` / `edacs` / `nxdn-type-c` / `mpt1327` / **`unknown`**), `system_id` and `site_id` as decoded, `cc_freq_hz`, the **channel table** (per `iden`: `base_hz`, `spacing_hz`, `tx_offset_hz`, `bandwidth_hz`, decode time), **neighbour sites**, **talkgroups**, `first_seen`/`last_seen`, `created_at`/`updated_at`.
- **Found before it is identified.** A control channel is confirmed by frame sync plus CRC (T-267) before any protocol decoder names it, so `protocol` may be `unknown` and the identifiers `NULL`. Two systems whose ids have not been decoded stay two rows: nothing is merged on the strength of not knowing. `cc_freq_hz` is `NULL` for a system with **no dedicated control channel** (Capacity Plus rest channel, NXDN Type-D, LTR) — never `0`.
- **The channel table is append-only, with the time each entry was decoded**, so a **stale** `IDEN_UP` table is detectable instead of silently mapping a grant to the wrong frequency (a C23 pitfall). The current table is the newest entry per `iden`.
- **Talkgroup labels are suggestions** from a prior (C17) or the user, carried with their source, and never what a call is matched on.
- **Storage:** `trunk_system` (+ `trunk_channel_plan`, `trunk_neighbour`, `trunk_talkgroup`), migration 0013. Fully columnar — the three child tables are the single copy of each fact.
- **Tests:** `hk-model` `repo/trunking.rs` — round trip with children, newest-entry-per-`iden`, append-only channel table, natural-key lookup.

### 2.29 CallRecord  [C23] (*T-266*)
One followed call, **metadata only** (`hk_model::trunking::CallRecord`): `call_id`, `trunk_system_id`, `t_start`, `t_end` (`NULL` while open or never observed), `talkgroup`, `unit_id`, `channel`, `slot`, `f_hz`, `encryption`, `late_entry`, `emitter_id` (the emission it rode on), `reasons[]`.
- **There is no CallAudio object and no column that could hold one.** M4 records *that* a call happened and never its content, which is why the roadmap's vocoder-IP question gates none of this work. Adding audio is a separate decision.
- **Encryption is three-state — `clear` / `encrypted` / `unknown` — with no default-to-clear path.** The C23 pitfall is late entry without a header, where the status must read `unknown`. The model makes the mistake unconstructible rather than merely discouraged: `Encryption` has **no `Default`**; its `Clear` and `Encrypted` variants each *require* an evidence field (`algid` / `service-options` / `dmr-pi` / `lc-header` / `user`) plus optional ALGID and Key ID, while `Unknown` carries none; and `is_clear()` matches `Clear` alone, so any gate written against it fails closed. The schema repeats it: `encryption` is `NOT NULL` **with no DEFAULT**, `CHECK ((encryption = 'unknown') = (encryption_evidence IS NULL))`, and a clear row may not carry an encrypting ALGID. The repository's read path errors on a contradictory row instead of coercing it. `unknown` means *not measured*, never a value (the same rule as T-164's `duty_cycle` and T-207's "Not yet classified").
- **A call never walks back towards clear.** Later evidence may sharpen the state, but a call once seen encrypted stays encrypted (repository check plus a schema trigger): a mid-call key change must not read as "listenable after all". **Nothing decrypts anything** — this is a correctness and safety requirement, not a legal one.
- **Identity & lifecycle:** aggregate — opened from a grant, its end and its sharpened encryption state filling in.
- **Storage:** `call_record`, migration 0013, indexed by `(system, t_start)`, `(system, talkgroup, t_start)` and `emitter_id`.
- **Tests:** `hk-model` `repo/trunking.rs` — a record built from a late-entry grant reads back `unknown`; the column cannot be omitted; `clear` without evidence, `unknown` with evidence and a clear row with an encrypting ALGID are all refused; no downgrade from encrypted; `no_audio_column_exists`.

### 2.30 GrantEvent  [C23] (*T-266*, AWARE-067)
The append-only stream of what a control channel said (`hk_model::trunking::GrantEvent`): `trunk_system_id`, optional `call_id`, `kind` (`grant` / `grant-update` / `call-start` / `call-end` / `denied` / `outside-window` / `unmapped-channel`), `t`, `talkgroup`, `unit_id`, `channel`, `slot`, `f_hz`, `encryption` **as that message stated it** (usually `unknown` for a bare grant update), and machine `detail`.
- **A grant that could not be followed is a row, not a silence.** `outside-window` records a grant beyond the ≤20 MHz dwell span and `unmapped-channel` one with no (or a stale) `IDEN` — logged, never silently dropped.
- **Relationships:** drives the metadata-only trunking load index (AWARE-067, T-273); links to the CallRecord it opened.
- **Storage:** `grant_event`, migration 0013, append-only by trigger, indexed by `(system, t)` and `(call_id, event_id)`.
- **Tests:** `hk-model` `repo/trunking.rs` — append-only triggers, time-ordered reads, each event's own encryption state.

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
