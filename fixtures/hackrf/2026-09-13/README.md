# HackRF One fixtures, 2026-09-13

Windows cut sample-exactly from the coordinator's receive-only captures in the external store
(`store/2026-09-13/`). Hardware: HackRF One (board rev older than r6, firmware 2026.01.3, libhackrf
0.9.2), internal clock measured −6.8 ppm (19 kHz FM pilot, spike S5), antenna unknown (as
attached by the user). Licence: project-owned capture. Each `.sigmf-data` is 24 000 000 bytes (ci8,
Git LFS).

Regenerate: `just fixtures-build-2026-09-13 --store <store root>` (py/fixtures/build_2026_09_13.py).
Every file carries normalised `hackriff:provenance`, per-capture `hackriff:clip_count`,
`core:global_index` into the source capture, and a `role: scenario` truth annotation with source
sha256, clock, clip and completeness notes. Truth is PHY metadata only; no third-party payload.

| Fixture | Use cases | Truth summary |
|---|---|---|
| `fm_100p8M_2p4M_l32g30a1_t1p5_5s` (1.5 s + 5 s) | SIGNAL-062 | 101.3 MHz WFM at +500 kHz (reads −535 Hz; SNR 18 dB), 19 kHz pilot (18 999.871 Hz). **RDS PI `1694`** (249/249 CRC-valid groups over the 30 s source agree), PTY 7, TP 0. **PS is dynamic** (scrolling song text): complete frames `Unstoppa` ×2, `ppable -`, `Sia     `, `Star    `, `Playing ` over 30 s; the window holds `Unstoppa` ×2. Groups 0A/2A/12A/3A; BLER 8.7 % over 30 s (249 groups), 6.7 % in the window (44 groups). Artefacts: 100.000 MHz reference-harmonic spur (`ref_harmonic`, n = 10), DC. |
| `ism_915M_10M_l24g30a1_t42p3_1p2s` (42.3 s + 1.2 s) | AWARE-036 (recorded companion) | 27 bursts over 12 dB (STFT 1024×4); 18 with fixed-rate sync truth (12 @ 100 kbit/s, 6 @ 150 kbit/s; preamble ≥ 24 bits then sync `0000110001011111`); 3 annotated at ≥ 20 dB (two of them are one edge-aliased transmission), 9 below 12 dB (S5 box SNR). Per burst: `symbol_rate_bd`, `deviation_hz` reference (h ≈ 0.55), `center_hz`/`rf_center_hz`, 200 kHz raster channel, time/frequency box, `snr_db`, sync position. 9 bursts are detection-only (`kind: burst`). 4 pairs of boxes touch ±fs/2 at the same time (one transmission seen at both Nyquist edges, outside the 7 MHz filter): flagged `at_nyquist_edge` with `alias_of_burst_index`, count once. `raster_error_hz` is recorded: most sit on the 200 kHz raster, but one strong burst reads 912.50 MHz. DC artefact. |
| `urban_98M_20M_l32g30a1_t1p0_0p6s` (1.0 s + 0.6 s) | AWARE-042, SPACE-050 (spike S4 overload case) | ADC clipping: 140 176 clipped samples (1.17 %), `overload: true`, `overload` artefact. 7/7 known FM (93.3, 94.9, 96.5, 98.9, 101.3, 104.5, 106.9 MHz) + 7 other raster emitters, 14-line 209.48 kHz comb (`comb`), 100.000 MHz spur (`ref_harmonic`), DC, FFT band-edge bins, unverified lines; S4 labels in `labels/`, each re-measured on the window. |
| `urban_98M_20M_l24g20a0_t1p0_0p6s` (1.0 s + 0.6 s) | AWARE-042, SPACE-050 (S4 gain-step pair) | Same time offset at mid gain: 0 clipped samples; 7/7 known FM + 7 raster; spur, DC; no comb. |
| `ism_433p62M_2M_l24g30a1_t162p0_6s` (162 s + 6 s) | AWARE-042, SPACE-050 (floor), AWARE-036 negative control | No device emissions (rtl_433 decoded nothing in 180 s; S5 detector 0 events here). `role: floor`: −98.98 dBFS/Hz ± 0.05 dB (1 σ over 0.5 s blocks) over 1.39 MHz. 28 persistent narrowband lines ≥ 6 dB at 122 Hz RBW annotated as unverified `narrowband-line` artefacts; the strongest (434.000 MHz, +24 dB, whole 180 s) sits on 217 × fs, so it is likely a sample-clock harmonic (an external 434.000 MHz carrier would read −2.9 kHz with this clock). |

`labels/emitters_*.csv` are the spike S4 per-emitter tables used as label sources (columns as in
`spikes/s4-detection-overload/REPORT.md` §3.4).

Not produced (see `fixtures/manifest.json`, status `blocked`): the ADS-B fixture (0 CRC-valid
messages; the antenna cannot hear 1090 MHz) and the terminated-input spur map (needs a 50 Ω
terminator fitted by the user). Originals of every capture are `external` manifest entries.
