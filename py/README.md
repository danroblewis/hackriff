# py/ — hackriff Python tooling

Covers SigMF fixture tooling, the synthetic IQ generator (T-023) and research code. **Python is
for orchestration and research only, never the real-time path** (ADR-0010).

```sh
uv sync             # create py/.venv from uv.lock
uv run pytest       # run the tooling tests (also `just test-py` from the repo root)
```

- `hkpy/sigmf.py` reads and writes `.sigmf-meta` documents. It is consistent with the Rust
  `hk_model::sigmf` types, including the `hackriff:` extension (`docs/sigmf-extension.md`).
- `hkpy/synth/` generates labelled synthetic IQ (below). The Rust harness `tests/e2e` calls it.
- `fixtures/` will hold fetch/verify tooling for the external fixture store (`fixtures/README.md`).

## Synthetic IQ generator (`hkpy.synth`)

```sh
uv run python -m hkpy.synth --list                     # scenarios and every parameter default
uv run python -m hkpy.synth fsk_burst_train --seed 1 --out /tmp/fsk \
    --param snr_db=12 --param lo_ppm=3 --param dc_offset_dbfs=-35 [--datatype cf32_le]
just synth adsb_squitter --seed 2 --out /tmp/adsb      # same, from the repo root
```

Output is deterministic in (scenario, seed, params, datatype, generator version): one or more
`<name>.sigmf-data/.sigmf-meta` pairs (`ci8` by default, 8-bit quantised; `cf32_le` on request),
any extra truth files, and `manifest.json` listing them. Lists are comma-separated in `--param`;
`none` sets null.

| Scenario | Use case | Default content |
|---|---|---|
| `tone` | building block | CW at +100 kHz, −20 dBFS, in −40 dBFS noise; 1 Msps, 50 ms |
| `fsk_burst_train` | AWARE-036 | 433.92 MHz, channel +50 kHz, CFO +3 kHz; 2-FSK 4800 bd ±9.6 kHz; bursts every 120 ms ±10 ms; 32-bit `1010…` preamble, sync `2DD4`, 48-bit payload (sensor id `5A3C`, seq, temperature, humidity, flags), **CRC-16/CCITT-FALSE** (poly 0x1021, init 0xFFFF, no reflection, xorout 0); SNR 20 dB in the Carson bandwidth; 500 kSps, 0.6 s |
| `noise_floor_rise` | AWARE-006 | L1 centred 1575.42 MHz, 2 Msps; +10 dB floor step at t0 = 0.1 s (whole band, or `rise_bandwidth_hz`); two weak CW; `start_utc`, `lat`/`lon` for correlation with a frozen feed |
| `injected_floor` | SPACE-050 | six 50 ms captures at 10/144/433.92/915/2450/5800 MHz, each with its own floor (seeded −42…−28 dBFS) and calibration constant K (seeded −80…−60 dB), per-capture provenance; one CW 10 dB below the floor power per segment |
| `occupancy_multi_hour` | AWARE-042 | 8 PMR446-style NBFM channels over 3 h: seeded burst schedule (`schedule.json`) plus two 250 ms IQ windows rendered on demand |
| `fm_broadcast_rds` | SIGNAL-062 | stereo WFM (L 1 kHz, R 400 Hz), 19 kHz pilot, RDS group 0A with PI `C0DE`, PS `HACKRIFF`, PTY 10; 456 kSps, 0.6 s |
| `adsb_squitter` | SIGNAL-001 fallback | four aircraft (`a0b1c2`, `4ca853`, `3c6444`, `c0ffee`), 8 DF17 squitters each cycling identification / even position / odd position / velocity, CRC-24, CPR; 1090 MHz at 2.4 Msps (readsb's rate; 2 Msps also works) |

**Occupancy representation.** Multi-hour IQ is too large to store (3 h at 200 kSps is about
4.3 GB), so the truth is the burst schedule: every burst's channel, start and duration, with exact
occupancy statistics per channel, per window hour and per UTC hour of day, plus a duration
histogram. IQ windows are rendered from the schedule on demand (`windows`, `window_starts_s`, or
`render_start_s`). Rendering is a pure function of the seed, the schedule and the window start, so
the same burst renders identically in overlapping windows. History and occupancy tests can take
detections straight from the schedule. Detector tests replay the rendered windows.

**Impairments** apply to any scenario, in signal-path order (`hkpy/synth/impairments.py`):
- `blocker_dbfs` (+ `blocker_offsets_hz`, `im3_coeff`) adds a two-tone blocker through a cubic front end, producing IM3 at 2f1−f2 and 2f2−f1 with exact truth power, plus desensitisation.
- `lo_ppm` shifts everything by the LO error.
- `phase_noise_linewidth_hz` adds Wiener phase noise.
- `iq_gain_db` / `iq_phase_deg` add IQ imbalance, with images annotated.
- `dc_offset_dbfs` adds a DC offset.
- `spur_dbfs` (+ `spur_step_hz`, default 10 MHz) adds internal spurs at n×step inside the capture, relative to the tuner centre.
- `adc_gain_db` drives the ADC into clipping.
- 8-bit quantisation and clipping are always applied. Clipped runs become `overload` annotations. Each capture gets `hackriff:clip_count` (its clipped-sample count; the key T-002 moves clip counts to), and provenance `overload` is set when the clip fraction exceeds 1e-4.

### Truth conventions and `hackriff:truth` schema

- IQ is normalised to full scale 1.0 per component: `ci8` = round(127·x). **dBFS** = 10·log10(mean |x|²), so a complex tone of amplitude 1 is 0 dBFS. **dBm** = dBFS + `calibration_k_db`, a constant stated per capture.
- Annotation frequency edges and `center_hz` are absolute Hz *as they appear in the recording*. They include CFO and any simulated LO error. `rf_center_hz` is the transmitter's true frequency.
- Every truth object has `role`, `kind`, `t_start_s` and `duration_s`. Annotation boxes are sample-exact.
- The scenario summary describes emitters before impairments; per-annotation truth is updated by every impairment and is authoritative.

| role / kind | Fields |
|---|---|
| `scenario` / `scenario` (one per file, whole-file box) | `scenario`, `recording`, `seed`, `params`, `generator`, `generator_version`, `use_cases`, `datatype`, `sample_rate_hz`, `n_samples`, `duration_s`, `dbfs_reference`, `calibration_rule`, `quantisation_noise_dbfs`, `clip_count`, `clip_fraction`, `overload`, `overload_rule`, `impairments[]`, plus per scenario: `emitter` (FSK), `location`/`t0_utc`/`step_db` (floor rise), `segments[]` (injected floor), `window` (occupancy), `aircraft[]` (ADS-B) |
| `emission` / any | `center_hz`, `rf_center_hz`, `offset_hz`, `bandwidth_hz`, `power_dbfs`, `power_dbm`, `snr_db` (in `bandwidth_hz`, when > 0), `modulation`, `identity: {type, value}` where known, `lo_offset_hz` when shifted |
| `emission` / `cw` | `amplitude`, `phase_rad`, `snr_db_per_hz` (tone) |
| `emission` / `fsk-burst` | `levels`, `symbol_rate_bd`, `deviation_hz`, `mod_index`, `bt`, `nominal_center_hz`, `cfo_hz`, `burst_index`, `bit_order`, `mapping`, `frame: {n_bits, bits_hex, preamble_bits, preamble_hex, sync_hex, payload_hex, crc_hex, layout}`, `payload_fields`, `crc: {algorithm, poly, init, refin, refout, xorout, value, valid}`, `identity` (`sensor_id`) |
| `emission` / `wfm-broadcast` | `peak_deviation_hz`, `stereo`, `preemphasis`, `pilot: {present, frequency_hz, deviation_hz}`, `audio: {...}`, `rds: {pi_hex, pi, ps, pty, tp, ta, music, di, group_types, n_groups, bitrate_bd, subcarrier_hz, deviation_hz, first_bit_s, encoding, check_poly, offset_words, blocks_hex}`, `label_expected`, `identity` (`rds_pi`) |
| `emission` / `adsb-df17` | `df`, `ca`, `icao`, `tc`, `message_kind`, `message_hex`, `crc_hex`, `crc`, `metadata` (`callsign`, or `altitude_ft`/`lat`/`lon`/`cpr_format`/`cpr_lat`/`cpr_lon`, or `ew_velocity_kt`/`ns_velocity_kt`/`vertical_rate_fpm`), `power_definition`, `identity` (`icao`) |
| `emission` / `nbfm-burst` | `deviation_hz`, `audio_tone_hz`, `channel`, `burst_index`, `burst_start_s`, `burst_duration_s`, `clipped_by_window`, `identity` (`channel-user`) |
| `emission` / `blocker` | `input_power_dbfs`, `tone_index` |
| `artefact` / `spur`, `im3`, `iq-image`, `dc-offset`, `overload` | `center_hz`, `offset_hz`, `power_dbfs`, `power_dbm`, plus `harmonic_n`/`spur_step_hz`/`tuner_center_hz`; `order`/`products_of_hz`; `image_of_hz`/`image_of_kind`; `i_offset`/`q_offset`; `clipped_samples`/`merge_gap_samples` |
| `floor` / `noise-floor` | `floor_dbfs` (integrated over the box), `floor_dbfs_per_hz`, `calibration_k_db`, `floor_dbm`, `floor_dbm_per_hz`, `bandwidth_hz`, `quantisation_noise_dbfs`, `expected_floor_dbfs` (analogue + quantisation noise: what a measurement should read) |
| `event` / `noise-floor-rise` | `expected_anomaly_kind`, `band`, `center_hz`, `bandwidth_hz`, `step_db`, `t0_s`, `t0_utc`, `floor_before_dbfs_per_hz`, `floor_after_dbfs_per_hz`, `floor_before_dbm_per_hz`, `floor_after_dbm_per_hz`, `location`, `cause_hint` |

`tests/test_synth.py` checks that the truth matches the samples:
- measured tone frequency and power, floors and floor step, and the SPACE-050 calibrated floor within ±1 dB;
- spur, DC, IQ-image and IM3 powers;
- occupancy statistics recomputed from the schedule, the log-normal median and hour-of-day profile, and deterministic window rendering;
- FSK bits demodulated with CRC checked against the stdlib `binascii.crc_hqx`;
- RDS PI/PS through a reference decoder whose block sync uses the EN 50067 parity-check matrix and published syndromes;
- ADS-B PPM demodulation with an independent CRC-24 and a global CPR decode;
- and, when `readsb` is installed, every squitter decoded by readsb (ci8 converted to uc8 by XOR 0x80).
