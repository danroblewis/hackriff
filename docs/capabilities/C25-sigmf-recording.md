# C25 · sigmf-recording
> Layer E — Remember · Status: draft (taxonomy draft 2026-09-13) · Depends on: C01, C03, C06, C09, C11, C19 · Used by: C21, C22, C23, C24, C27, C28, C38, C39

## Purpose
Turns transient IQ into durable SigMF evidence: triggered snippets with pre-trigger history, per-channel decimated streams and audio, with provenance and machine annotations, under a disk quota. Serves workflow step 5, and feeds steps 3 and 6 (bits recovered from recordings). Also the offline test-fixture format (CLAUDE.md).

## Interface
- **Input** `RecordRequest` (provisional): trigger source (detection id / scheduler / user / decoder), pre- and post-trigger span, target (full dwell window or channel centre+bandwidth), priority class, content class (see Pitfalls).
- **Output:** SigMF dataset = `.sigmf-data` + `.sigmf-meta` with `global`, `captures`, `annotations` (docs/03 §1.6). `RecordingIndex` row: id, path, t_start/t_end, f range, sample rate, datatype, bytes, site, trigger/emitter links, priority, pinned, checksum, `iq_evicted`.
- **Queries:** by time, frequency overlap, emitter, label, priority; free space and projected time-to-full.
- **Retention config:** total quota, per-class quotas, min free space, max age per class, pin.
- **Rates** (arithmetic):
  - Full 20 Msps window, native 8-bit: 40 MB/s = 144 GB/h (docs/02 §3.1). A 10 s event = 400 MB; a 30 s pre-trigger buffer = 1.2 GB RAM (docs/06 C03).
  - 25 kHz channel at 50 ksps (2× oversampled): int16 200 kB/s (720 MB/h); float32 400 kB/s (1.44 GB/h).
  - Audio 16 kHz mono 16-bit PCM: 32 kB/s (115 MB/h).
  - Example (assumed 256 GB IQ quota): ~640 ten-second full-window events, or ~355 h of int16 25 kHz channel.

## Methods
- **Format:** SigMF v1.2.6, sigmf-python to write/validate (docs/03 §1.6). Keep full-window IQ in native 8-bit complex rather than widening (datatype names per SigMF spec — verify). Provenance (gain, clip count, filter, calibration version, clock lock, GNSS fix) goes in a project extension namespace. Detections and classifications become annotations (docs/03 §1.6 recommendation).
- **Trigger path:** copy the pre-trigger span from the C03 ring buffer, continue until the post-trigger hold expires. Model: SDRangel SigMF File Sink (docs/03 §2.2). Multi-channel events → SigMF collection.
- **Capture everything, decide later** for unknown/unexplained events (docs/03 §5.2); run classifiers and decoders on the recording, not just live (docs/04 §3.8 step 4).
- **Retention:** value-based eviction, not FIFO: pinned/user-labelled > unknown/unexplained > decoder-confirmed known > routine. On eviction keep index row, annotations and derived products ("separate measurement from interpretation", docs/04 §11.2).
- **Write safety:** preallocate; write data, then meta via temp-file+rename; checksum in index; orphan scan at boot.
- **Index:** SQLite beside the inventory (docs/03 §7; docs/04 §11.2 suggests SQLite/Parquet).

## Platform constraints
- USB 2.0 ingest already runs at ~35–40 MB/s (docs/01 §1.4). A simultaneous 40 MB/s write must not starve the channelizer: dedicated I/O thread, bounded queue (estimate-level guidance, benchmark).
- Orin Nano Super: 8 GB unified CPU/GPU memory (docs/02 §3.3). A 1.2 GB pre-trigger buffer is a large share; cap pre-trigger length per mode.
- Write IQ to NVMe (dev kit M.2, docs/02 §3.3). docs/02 §3.4 marks microSD ✔ for continuous 20 MHz, but sustained 40 MB/s is marginal and wears SD (estimate). Don't inherit "SD card as the I/Q sink" (docs/01 §7.2).
- Power (Tier B 15–38 W, docs/02 §7.2): low-power mode records channels/audio only.
- Sample-accurate timestamps from C03, anchored to GNSS/1PPS where fitted (docs/02 §7.3 #5).

## Prior art and reuse
- **SigMF + sigmf-python:** format and validator; active 2026-08 (docs/03 §1.6). Licence/terms: check.
- **SDRangel `sigmffilesink`:** trigger/pre-trigger/hold; appends captures to one file (docs/03 §2.2). Very active. Licence: check.
- **Mayhem Capture:** C8/C16 + `.TXT` sidecar (centre, rate, GPS). Keep the simplicity; import users' old captures (docs/01 §3.5, §7.1 #3).
- **Maia SDR:** SigMF recording on a small device, capped at 400 MiB by RAM (docs/03 §2.4).
- **Trunk Recorder:** a recorder per call on the wideband stream; per-call audio + JSON (docs/03 §3.5; docs/04 §8.4).
- **IQEngine, inspectrum:** consumers of exported recordings (docs/03 §3.4).

## Pitfalls
- **Disk fill in busy bands:** an ISM storm triggers on every burst. Rate-limit triggers per emitter/cluster.
- **Loose files with no index** (docs/03 §5.1 #6): never write outside an index transaction.
- **Annotation coordinates:** sample indices break after decimation or appended captures. Store time/frequency too.
- **Clock steps** (GNSS acquisition, NTP) mid-recording corrupt `captures` datetimes. Record clock source and lock state.
- **IMD-polluted snippets:** clipped or IMD-polluted recordings become false "real signal" fixtures. Carry C05 suspect flags into meta.
- **Legal:** recording is not the risk; divulging is (47 USC 605). Cellular and common-carrier paging contents are off-limits, and encrypted traffic is metadata only (docs/04 §1.3, §8.3). Tag each dataset with a content class so C24/C28 exports refuse third-party content.
## Testing
- **Trigger timing:** replay a SigMF fixture via the C01 file source with scripted bursts at known offsets. Assert the pre-trigger sample count, `captures` start time within 1 sample, and annotation boxes overlapping the injected bursts.
- **Round-trip:** output passes sigmf-python validation and re-replays bit-identically.
- **Quota:** a small tmpfs quota plus a synthetic trigger storm. Assert eviction order by class, pinned datasets never evicted, a consistent index, and no orphans.
- **Crash:** kill -9 mid-write, then assert recovery marks the partial dataset.
- **Legal gating:** export refuses a restricted content class.
- **Needs hardware:** sustained NVMe write alongside USB ingest and the channelizer on the Jetson; thermal throttling; power draw.

## Example use cases
Provisional until docs/06 §3 mapping.
- RESEARCH-075 — Reproducible IQ archives with SigMF
- RESEARCH-070 — Build labeled RF datasets from your own captures
- AWARE-036 — Unknown burst reverse-engineering triage
- SPACE-064 — Jupiter S-burst microstructure
- AWARE-054 — Amateur-band intruder logging
- RESEARCH-076 — Browser-based IQ exploration
- AWARE-056 — Woodpecker history replay

## Open questions
- **File granularity:** one dataset per trigger, or SDRangel-style append? (retention granularity vs file count)
- **Provenance namespace:** the extension namespace must align with docs/07.
- **Audio:** it has no SigMF-native home. WAV/Opus plus a SigMF-style sidecar?
- **Storage budget:** who owns the device-wide budget across C25/C26/C27? docs/06 has no storage-manager capability.
- **Legal export policy:** content-class policy cuts across C22–C25 and C28, and docs/06 names no owner.
- **Boundary with C28:** C25 writes machine annotations at record time; C28 owns edits and exports. Confirm.

## Reading list
1. docs/03 §1.6 "Metadata: SigMF"
2. docs/03 §2.2 "SDRangel — the most engineering-grade open-source receiver" (SigMF File Sink)
3. docs/02 §3.1 "Throughput math"
4. docs/04 §11.2 "Mapping to an exploration device"
5. docs/04 §1.3 "Legal considerations (US; not legal advice)"
6. docs/01 §3.5 "Recording and replay formats"
