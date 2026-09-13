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
The atomic measurement: `detection_id`, `survey_id`, `t_start`, `t_end`, `f_center`, `bandwidth` (OBW + x-dB), `snr_peak`, `snr_mean`, `peak_level_dbfs` (+ optional dBm), `sk`, `clip_count` (per span; not in Provenance, which is deduplicated), `detector_version`, `provenance_ref`, flags: `clipped` (required when `clip_count > 0` or the provenance is overloaded), `spur_candidate` with optional `spur_reason` ∈ {`ref-harmonic`, `dc`, `lo-relative`, `comb`, `spur-map` + SpurMask ref}, `image_candidate` with `image_retune_confirmed`, `marginal`, `suspect_imd`, `compressed`, `impulsive`, `edge` (*`spur_reason`, `image_retune_confirmed`, `suspect_imd`, `compressed`, `impulsive` and `edge` added from spike S4, 2026-09-13*). The IQ snippet and track membership link to the detection from Recording (`trigger`) and the track↔detection link table, so the row never changes.
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
The persistent "thing seen on the air": `emitter_id`, current `f`/`BW`, `fingerprint` (C18), `first_seen`, `last_seen`, `count`, `classification` (family + confidence + open-set score + model version) as an **append-only history** (not overwritten), `identity` (decoded id such as ADS-B hex, RDS PI, MMSI, talkgroup) or `unknown`, `known_status` vs priors (`known` / `unexpected-here` / `unknown`), `tags`, and links to Tracks, Detections, Recordings, Demodulations, Explanations, Annotations.
- **Identity & lifecycle:** `emitter_id` stable for the life of the cluster; created when detections cluster to a new fingerprint (C18); `last_seen`/`count` update continuously; classification is re-run and appended as models improve — the measurement it ran on is unchanged.
- **Relationships:** the hub. Loops with Explanation (subject or cause) and with Decode (identity) and priors (C17).
- **Retention & size:** thousands of rows; never auto-deleted (it's the memory); links may outlive the raw detections they summarise.
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
A demod session on a channel derived from an Emitter/Detection: `demod_id`, `emitter_ref`, `mode`/family, estimated params (symbol rate, deviation, CFO, mod order, roll-off), `lock_quality`/EVM, outputs (`audio_ref` / `symbol_stream_ref` / `bitstream_ref`), `demod_version`.
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
The shared record §5 said doc 07 must define: `anomaly_id`, `kind` (`new-emitter` / `busier-than-baseline` / `noise-floor-rise` / `novelty`), `subject_ref` (Detection/Emitter/region), `region` (`f_lo,f_hi,t`), `score`, `baseline_ref`, `t`.
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

## 3. Storage (provisional — Phase 3 storage ADR finalises)

Three stores under one per-device data directory, so the whole state is one thing to back up, export, or wipe:

### 3.1 Relational state — SQLite (candidate; DuckDB considered)
Holds ScanPlan, Survey, Provenance, CalibrationState, SpurMask, Detection, Track, Emitter, Recording (rows, not bytes), Annotation, Demodulation, Decode, Bitstream descriptors, ExternalEvent, Anomaly, Explanation. SQLite for one-writer simplicity and ubiquity on the Jetson; DuckDB if analytic region/time scans dominate. **Provisional**; the ADR decides, and the pick is isolated behind a repository layer so it's reversible.

### 3.2 Spectrum history — tiled pyramid
SpectrumTiles in a columnar store (Parquet) or a purpose-built ring of downsampled tiles, partitioned by time and frequency block, with a resolution pyramid (recent = fine, old = coarse). Fixed rolling byte budget from config. This is the object that makes "region over time" cheap and bounds disk.

### 3.3 IQ / audio / bits — SigMF files on disk
Recordings and stored Bitstreams are SigMF datasets (data + meta) referenced by URI. Annotations live in the SigMF meta so a recording is self-describing and portable. Quota-managed, ranked by interestingness, `content_class`-gated.

**Disk budget sketch** (device with, say, a 256 GB–1 TB NVMe): relational state MB–low GB; spectrum history a fixed 5–20 GB pyramid; the rest is the IQ snippet pool, quota-capped, oldest/least-interesting evicted. A storage/retention policy object (owner TBD in the ADR, docs/06 §5) enforces the split.

## 4. The central query: "what has this region looked like over time?"

The product's core question (workflow step 3). It resolves against three stores and unions the results into a region-over-time view:

1. **Occupancy & shape** from SpectrumTiles: `WHERE f overlaps [f_lo,f_hi] AND t in [t0,t1]` at the coarsest level that meets the requested resolution → max-hold, mean, percentile bands over time.
2. **Events** from Detection (indexed by `(f_lo,f_hi)`,`t_start`) and Track → the bursts/emitters active in that box, with SNR and timing.
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
- **`content_class` gating point** — provisionally C24 enforces; confirm in the legal-guardrail ADR.
- **Own-key decryption** modelled as a C22 stage with key-source provenance — confirm.
- **Retention/quota policy object** ownership across Recording/SpectrumTile/Detection — Phase 3.
- **Timestamp error budget** for Provenance — Phase 4 spike (no hardware 1PPS on HackRF One).
