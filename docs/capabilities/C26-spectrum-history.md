# C26 · spectrum-history
> Layer E — Remember · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C02, C05, C06, C07 · Used by: C12, C17, C30, C33, C39

## Purpose
Keeps a compressed, multi-resolution record of power spectra over months, from sweep rows and dwell spectrograms, so the user can pick a region and time span and see what was active. Serves workflow step 1 (open reports of past surveys) and step 3 (review history). It is the raw material for occupancy baselines (C12) and for the attack map's "what changed" (C30). No open-source receiver keeps persistent survey history (docs/03 §5.1 #5).

## Interface
- **Inputs:**
  - SweepFrames (C02): dB per bin, per-slice provenance, one timestamp per sweep.
  - SpectrumFrames (C07): PSD rows, persistence, SK.
  - Position/time (C06) and calibration id (C05).
- **Output** `SpectrumTile` (provisional): site/session, f range, time bucket, tier, bin width, stats {max, mean, low percentile, high percentile}, provenance (gain table, filter, cal id, clip/suspect fraction), coverage mask of bins actually observed.
- **Queries:**
  - `history(f_lo, f_hi, t0, t1, max_px)` picks the coarsest adequate tier.
  - `report(region, span)` builds a survey report: occupancy (C12), top emitters (C27), change vs baseline.
  - Export to PNG, or to CSV in hackrf_sweep format `date, time, hz_low, hz_high, bin_width, num_samples, dB…` (docs/01 §1.6).
- **Retention config:** per-tier max age and byte quota; per-region overrides ("keep 433 MHz at 1 s for 90 days").
- **Rates** (arithmetic, before compression):
  - Dwell spectrogram, 4096 bins float32 at 30 lines/s: ≈0.5 MB/s (docs/02 §3.1), 1.8 GB/h.
  - 0–6 GHz sweep at 100 kHz bins: 60,000 bins every ~0.75 s (docs/04 §3.8) = 80,000 bins/s. float32 0.32 MB/s (1.15 GB/h); uint8 at 0.5 dB steps 80 kB/s (6.9 GB/day).
  - 1-min tier with 3 stats × uint8: 259 MB/day at 60,000 bins; at 1 MHz bins, 26 MB/day ≈ 9.5 GB/year.

## Methods
- **Multi-resolution pyramid:** raw (hours) → 1 s → 1 min → 15 min → 1 h, with 2× frequency decimation per level (tier choices are estimates).
  - Keep **max** per cell for bursts and persistent emitters, **mean**, and a **low percentile** for the noise floor (percentile/minimum statistics, docs/04 §3.2).
  - The discovery sweep already maintains per-bin max-hold, mean and SK (docs/04 §3.8 step 1).
- **Store dBFS plus calibration id**, not only dBm, so later dBFS→dBm tables (docs/04 §10.2) can be reapplied.
- **Regrid heterogeneous inputs** (varying bin widths, sweep vs dwell) to a canonical grid per tier. Keep a coverage mask: "not observed" ≠ "quiet".
- **Storage:** chunked, compressed array tiles (compression ratio unmeasured) with a SQLite index. Alternatives: Parquet/DuckDB (docs/03 §7; docs/04 §11.2), or SQL rows/blobs as in Spectre (docs/03 §3.1). Decide in an ADR.
- **Resolution floor:** bin width ≤ narrowest channel spacing; thresholds ≥3–5 dB above noise; ≥24 h when time patterns are unknown (docs/04 §3.9). C12 computes the stats; C26 must retain enough resolution.
- **Reports** follow the professional loop: scan → detect → compare with licence DB → log → report (docs/04 §11.1).

## Platform constraints
- **Disk:** continuous uint8 sweeps at ~7 GB/day (above) fill a small disk within weeks. The raw tier must age out in hours to days.
- **Write pattern:** appends every second. Batch them into per-minute tile flushes to limit NVMe/SD wear and wakeups (estimate).
- **Low-power mode:** sweeping costs ~1 core (docs/06 C02). History must tolerate gaps, marked in the coverage mask.
- **Moving handheld:** key tiles by site/session from C06. Mixing locations corrupts baselines.
- **Timing:** hackrf_sweep stamps each full sweep once (docs/01 §1.6), so per-bin time accuracy is ~0.75 s.

## Prior art and reuse
- **Spectre (HB9TF):** long-term hackrf_sweep/rtl_power collection into SQLite/MySQL; waterfall images; filters by time, frequency and source. Go; active 2026-07 (docs/03 §3.1). Licence: check.
- **NTIA SCOS sensor / scos-actions:** task scheduling and SigMF metadata model; research-grade (docs/03 §3.1). Licence: check.
- **rtl_power + heatmap.py:** CSV → waterfall PNG; stable/old (docs/03 §3.1). `tools/sweep_plot.py` is the user's equivalent.
- **R&S Spectrum Rider K15:** spectrogram recording up to 999 h (docs/02 §6).
- **CRFS, R&S ARGUS:** occupancy and long-term statistics (docs/03 §5.2).
- **Tektronix DPX:** persistence as a statistic worth keeping (docs/03 §3.9).

## Pitfalls
- **Averaging erases bursts.** Never downsample with mean alone.
- **Sweeps under-sample bursts:** sub-ms dwell per step gives ~0.7% POI for a 5 ms burst (docs/04 §3.8). Don't present absence as "no activity".
- **Front-end changes look like events:** gain-table or filter changes create steps. Store provenance per tile so C30 can rule them out first.
- **IMD and spurs pollute long-term maxima** in cities (docs/04 §10.3–10.4). Record the spur-mask version.
- **Clock errors** (no GNSS fix, RTC drift) break joins with feeds and hour-of-week baselines.
- **Schema drift** across tiers and releases: version the tile format.

## Testing
- **Burst survival:** synthetic SweepFrames with a scripted emitter (on 10 s every 5 min) plus noise. After 1 h of downsampling the max tier still shows it, and the low percentile matches the injected noise ±0.5 dB.
- **Tier consistency:** `history()` agrees across tier boundaries where resolution allows; the coverage mask marks injected gaps.
- **Retention:** a fast-forwarded clock; assert tier ageing and quota.
- **Provenance:** an injected gain step appears in the tile provenance.
- **Import:** a recorded hackrf_sweep CSV fixture (user's own); compare peaks with `tools/sweep_plot.py`.
- **Needs hardware:** multi-day storage rate and wear on the target disk.

## Example use cases
Regenerated from `use-cases.yaml`:
- AWARE-031 — Long-term noise-floor trend logger
- AWARE-042 — Duty-cycle and occupancy statistics
- SPACE-050 — Natural radio noise floor survey
- AWARE-033 — Radio-quiet-zone style site survey
- SPACE-003 — e-CALLISTO solar burst spectrograms
- AWARE-066 — Legacy network sunset tracker
- RESEARCH-050 — SDR as spectrum analyzer / power survey
- PROP-031 — Propagation beacon logging

## Open questions
- **Tile format ADR:** chunked arrays vs Parquet/DuckDB vs SQLite blobs.
- **Time-series home:** do per-channel noise-floor (C08) and occupancy (C12) series persist here or in their own stores? docs/06 is silent.
- **Site model** for a moving device: discrete sites or geohash buckets?
- **Retention defaults** per tier need user input.
- **Report ownership:** the split between C26 ("survey reports"), C12 and C39 is implicit in docs/06.
- **Dependency sketch:** docs/06 §2.1 draws no arrows into Layer E. Suggest C02/C07 → C26 → C12.

## Reading list
1. docs/04 §3.8 "Sweep-based survey vs. real-time IBW"
2. docs/04 §3.9 "Occupancy statistics methodology (ITU-R SM.1880 / SM.2256)"
3. docs/02 §3.1 "Throughput math" (Recording takeaway)
4. docs/03 §3.1 "Wideband sweep / survey"
5. docs/01 §1.6 "Firmware, `hackrf_sweep`, and host tools"
6. docs/03 §5.1 "Concrete problems" (#5)
