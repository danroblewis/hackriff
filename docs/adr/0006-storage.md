# ADR-0006 — Storage

**Status:** PROVISIONAL (engine choices confirmed after a small benchmark in Phase 4/early build)
**Touches:** C25, C26, C27; all persisted objects ([docs/07 §3](../07-data-model.md))

## Context

A handheld with a 256 GB–1 TB NVMe must keep: relational state (plans, detections, emitters, decodes, events), long spectrum history for "region over time", and IQ/audio recordings — while raw IQ at 20 Msps is 144 GB/h ([docs/02 §3.1](../02-sdr-landscape.md)). The design rule is: record spectrogram metadata continuously, IQ only around detections, with a pre-trigger ring buffer.

## Decision (provisional) — three stores under one data directory

1. **Relational state: SQLite** (single-writer, ubiquitous, zero-ops on the Jetson) behind a repository layer. DuckDB is the fallback if analytic region/time scans dominate; the repository layer keeps the choice reversible. Holds every row-shaped object in [docs/07](../07-data-model.md).
2. **Spectrum history: a tiled multi-resolution pyramid** (Parquet tiles, or a purpose-built ring), partitioned by time and frequency block, downsampled as it ages (recent fine, old coarse), under a **fixed rolling byte budget** from config (target 5–20 GB). This is the object that makes "region over time" cheap and bounds disk. Reads by `(f-range, t-range, level)`.
3. **IQ/audio/bits: SigMF files** on disk referenced by URI from Recording rows; annotations live in the SigMF meta so files are portable. Quota-managed pool, evicted oldest/least-interesting first, `content_class`-gated.
4. **Pre-trigger: a RAM ring buffer** in the core ([docs/07 §2.3](../07-data-model.md)); a trigger flushes the pre-trigger window plus post-trigger hold to a SigMF Recording.

- **Retention policy object** (owner TBD, docs/06 §5) enforces the split of the disk budget across the three stores and the eviction ranking. **Provisional.**

## Consequences

- One directory is the whole device state: back up, export, or wipe as a unit; matches "offline-first, don't rule out sharing".
- The pyramid decouples history size from observation time — the key to long surveys on small disks.
- SigMF everywhere makes recordings interoperable with inspectrum/IQEngine and usable directly as test fixtures ([docs/10](../10-test-strategy.md)).
- Risk: SQLite write throughput under high detection rates in dense bands — mitigated by batching detection writes and keeping frames out of the DB. Benchmarked before commit.
