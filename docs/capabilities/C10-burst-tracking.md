# C10 · burst-tracking
> Layer B — Sense · Status: draft (taxonomy draft 2026-09-13) · Depends on: C09 (C06 time; C05 flags via Detections) · Used by: C04, C12, C15, C18, C23, C27

## Purpose
Links individual Detections over time into **Tracks/Emissions**: one emitter with a timing signature. It computes periodicity, duty cycle, inter-arrival statistics, hop sets and rates, TDMA frame periods and inter-channel co-occurrence (repeater pairs, trunk grants). Timing is often the cheapest identifier: beacons, TPMS, TDMA and hoppers. It feeds the inventory (workflow step 3), classification (step 5) and the scheduler's "expected next burst".

## Interface
- **In:** `Detection` stream (t_start/t_end, f_c, BW, SNR, SK, flags, source sweep|dwell), plus the observation windows actually covered (needed to tell "absent" from "not looked at").
- **Out (provisional `Track`):** `track_id`, member detection ids, `f_centre` ± spread, `bw_hz`, first/last seen, `burst_count`, `on_time` distribution, `inter_arrival` mean/CV, `period_s` + strength, `duty_cycle`, `hop` {set, spacing Δ, rate 1/T_h, sequence period}, `tdma_frame_s`, `next_burst_eta` + confidence, `trust` (share of suspect-flagged members).
- **Out (provisional `CoOccurrence`):** channel pairs/groups with lagged correlation (repeater in/out, CC→voice grant).
- **Out:** emission clusters (feature vector for C18/C27).
- **Config:** association gates (Δf, ΔBW, max gap), period search range, TDMA candidate periods, min bursts per statistic, track expiry.

## Methods
- **Association:** same f_c ± ε and similar BW → same track (`docs/04 §3.6 "Burst detection and segmentation"` step 5). Start with gated nearest-neighbour; ε ≈ max(2 bins, 10% BW) (estimate).
- **Burstiness:** burst count/s, on-time distribution, inter-arrival CV; SK per bin as a cross-check. `docs/04 §2 "What makes a frequency "interesting": a feature taxonomy"` feature 4.
- **Periodicity:** autocorrelation or periodogram of the burst-start series, or FFT of the on/off sequence. Typical scales: TPMS ~minutes, weather sensors 30–60 s, BLE adverts 20 ms–10 s, radar PRF, rotating radar seconds. `docs/04 §2` feature 5.
- **Hop detection:** constant BW and dwell T_h, frequencies on raster f₀ + kΔ, no temporal overlap between consecutive dwells. Hop rate 1/T_h from the dwell-duration mode; Δ = GCD of frequency differences; hop set = unique frequencies; sequence period from autocorrelation of the channel index. `docs/04 §4.7 "Hop detection and burst timing"`.
- **TDMA:** histogram of burst start times modulo candidate periods (DMR 60 ms / 30 ms slots, TETRA 56.67 ms, GSM 4.615 ms). `docs/04 §4.7`.
- **Inter-channel correlation:** cross-correlate on/off sequences and lagged co-occurrence. Known offsets: ±5 MHz UHF, ±600 kHz VHF amateur, 45 MHz at 800 MHz. It confirms trunk-system membership before decoding. `docs/04 §2` feature 14, `docs/04 §8.4 "Architecture for trunking support"`.
- **Occupancy input:** duty cycle ≈ FCO for the track's channel. `docs/04 §3.9 "Occupancy statistics methodology (ITU-R SM.1880 / SM.2256)"`.

## Platform constraints
- **One ≤20 MHz window:** hop sets wider than the IBW are only partly observed. Bluetooth Classic (1600 hops/s over 79 MHz) needs ~80 MHz and cannot be followed. `docs/04 §4.7`.
- **Sweep-sourced detections are sparse and aliased in time** (each 20 MHz chunk seen a few ms per ~0.75 s sweep). Periods shorter than the revisit time are unmeasurable; use dwell data. `docs/01 §1.6 "Firmware, `hackrf_sweep`, and host tools"`, `docs/04 §3.8 "Sweep-based survey vs. real-time IBW"`.
- **Timestamp accuracy:** sample-counter time from C03 is fine within a dwell; across retunes, and against external events, use C06 GNSS time. The HackRF One has no TCXO (ppm unverified), which blurs f_c association over temperature. `docs/01 §1.2 "Specifications"`.
- Half-duplex, single radio: the scheduler (C04) creates observation gaps that must be modelled.

## Prior art and reuse
- **rtl_433 pulse analyzer (`-A`):** pulse/gap timing histograms within a burst (intra-burst; complements inter-burst tracking). Licence: check. `docs/03 §3.3 "Automatic device and protocol decoders"`.
- **Trunk Recorder / SDRTrunk / OP25:** CC-grant → voice-channel association logic. Licences: check. `docs/03 §3.5 "Trunking and digital voice (the "CB/trunk complaint")"`.
- **CRFS RFeye DeepView:** commercial hopper discovery; UX reference only. `docs/03 §3.9 "Commercial analysis and monitoring software (UX references)"`.

## Pitfalls
- **Observation gaps look like silence** and bias period/duty estimates; always normalise by observed time.
- **Fragmentation** from CFO drift, Doppler or low-SNR BW jitter; **merging** of co-channel emitters (many 433.92 MHz sensors share f_c; separate them with C18 fingerprints or decoded IDs).
- **Spurious periodicity** from the processing itself (frame/sweep cadence, scheduler revisit) or from IMD products that inherit a strong signal's cadence. Check suspect flags.
- **Hop-raster aliasing:** GCD on noisy f_c gives a too-small Δ; quantise to bins first.
- **Legal:** grant/co-occurrence analysis is metadata; keep content out of C10 (docs/04 §1.3).

## Testing
- **Synthetic:** (a) 3 emitters on one f_c with periods 30 s, 47 s, 60 s plus jitter → expect 1 track with 3 periodicities, or 3 tracks once fingerprints exist; (b) hopper with Δ = 1 MHz, 50 channels inside 20 MHz, T_h = 10 ms → hop set, Δ and rate recovered; (c) TDMA bursts at 30 ms slots → frame 60 ms found; (d) repeater pair at ±600 kHz with 50 ms lag → co-occurrence found; (e) scheduler-induced gaps inserted.
- **Metrics (proposed):** period error < 2% with ≥10 bursts; Δ exact; hop-set recall ≥ 90% inside IBW; track purity/completeness; false-periodicity rate on Poisson bursts.
- **SigMF fixtures (HackRF One, long dwells):** 433.92 MHz weather sensors (rtl_433 IDs as truth); TPMS near traffic; 2.4 GHz BLE adverts; a DMR repeater (TDMA 30/60 ms); amateur repeater in/out pair.
- **Live only:** multi-hour tracking with scheduler gaps; GNSS time alignment.

## Example use cases
Provisional until docs/06 §3 mapping:
- AWARE-042 — Duty-cycle and occupancy statistics
- AWARE-070 — IoT sensor population census
- AWARE-055 — Over-the-horizon radar signature catalogue
- AWARE-062 — Radiosonde launch correlation
- AWARE-067 — Public-safety trunking load index (metadata only)
- SIGNAL-046 — TPMS
- SIGNAL-072 — NCDXF/IARU beacon chain
- AWARE-035 — Smart-meter mesh as noise contributor

## Open questions
- **Boundary with C18:** docs/06 says C10 "clusters emissions by fingerprint" and C18 clusters unknowns into "same thing I saw before". Who owns clustering?
- Track state in memory (C10) vs persisted in C27; restart behaviour.
- Does inter-channel correlation need C11 per-channel power series? §2.1 shows only C09 upstream.
- The C10 → C04 `next_burst_eta` edge is in the C10 definition, but §2.1 routes it only via C12.

## Reading list
1. `docs/04 §4.7 "Hop detection and burst timing"`
2. `docs/04 §2 "What makes a frequency "interesting": a feature taxonomy"`
3. `docs/04 §3.6 "Burst detection and segmentation"`
4. `docs/04 §8.4 "Architecture for trunking support"`
5. `docs/04 §3.8 "Sweep-based survey vs. real-time IBW"`
6. `docs/03 §3.3 "Automatic device and protocol decoders"`
