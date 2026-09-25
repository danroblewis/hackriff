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
- `py/fixtures/` holds the fixture tooling (`trim.py`, `annotate.py`, `verify.py`, `fetch.py`, `build_2026_09_13.py`, reference decoders `rds_ref.py`/`fsk_ref.py`); recipes `just fixtures-verify [--external]`, `just fixtures-fetch`, `just fixtures-build-2026-09-13` (see `fixtures/README.md`).

## Stream clients (`py/examples/`, T-060)

External programs read demodulated outputs (burst bits, soft symbols, Listen audio, and every
always-on stream) from `hk serve`'s token-authenticated TCP stream server (loopback port 8788 by
default, `HK_STREAM_TCP=<addr>` to change it; the address is printed at start and listed by
`GET /api/streams`). The examples use the standard library only; the wire format is
`docs/stream-contract.md` §13.

```sh
# netcat: one handshake line, then the framed stream (here hex-dumped)
printf 'open/bits?token=%s\n' "$HK_TOKEN" | nc 127.0.0.1 8788 | xxd | head -40
# socat half-closes on stdin EOF, which ends the stream: keep it open with -t
printf 'open/bits?token=%s\n' "$HK_TOKEN" | socat -t 86400 - TCP:127.0.0.1:8788 > bursts.hkstream

python3 py/examples/hk_bits.py --port 8788                    # print decoded bursts (HK_TOKEN)
python3 py/examples/hk_bits.py --symbols --emitter <id>       # soft symbols of one emitter
python3 py/examples/hk_bits.py --file bursts.hkstream         # parse a netcat dump
python3 py/examples/hk_audio_wav.py --emitter <id> --seconds 10 --out station.wav
python3 py/examples/hk_audio_wav.py --emitter <id> --stereo --out station-stereo.wav
```

- `hkstream.py` is the reusable parser: frames, header (or a refusal frame), binary records,
  status records, drop markers, `connect()` and `pack_bits()`.
- `hk_bits.py` pairs each burst's status record (sync and payload offsets, bit order, CRC,
  emitter) with its data record and prints the payload bytes.
- `hk_audio_wav.py` writes 48 kHz 16-bit WAV with the stream's own channel count (mono, or
  two channels after `--stereo` — which sends `channels=2` — on a broadcast-FM station; the
  header's `audio.channels` says which it got, T-874), filling `sample_index` gaps with silence.
- `tests/test_stream_examples.py` runs the parser on `tests/data/t060_fsk_bits.hkstream`, bytes read
  over TCP from the mock-SDR e2e test (regenerate with `HK_T060_CAPTURE=<path> cargo test -p
  hk-e2e --test stream_external`).

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
| `fsk_burst_train` | AWARE-036 | 433.92 MHz, channel +50 kHz, CFO +3 kHz; 2-FSK 4800 bd ±9.6 kHz; bursts every 120 ms ±10 ms; 32-bit `1010…` preamble, sync `2DD4`, 48-bit payload (sensor id `5A3C`, seq, temperature, humidity, flags), **CRC-16/CCITT-FALSE** (poly 0x1021, init 0xFFFF, no reflection, xorout 0); SNR 20 dB in the Carson bandwidth; 500 kSps, 0.6 s. **T-622:** `check_width` (8/16/24/32, default 16 preserves the CRC-16/CCITT-FALSE default exactly), `check_poly_hex` (override to land off the RevEng catalogue on purpose, e.g. `0x8F45`), `constant_payload` (beacon mode: the identical payload/CRC every burst, for the "does NOT confirm on repeat count alone" case, docs/22 P7) |
| `generic_fsk_sweep` | RESEARCH-002, SIGNAL-052 | **T-863 (MAUTO M-12): ADR-0015 §7's generic FSK/OOK population**, one draw per seed (`hkpy.synth.generic_fsk`): `modulation` 2-FSK (h 0.8–2) or NRZ OOK, 300 Bd–50 kBd log-uniform, a random 16–32-bit sync (balanced, low autocorrelation sidelobe), `check` = `catalogue` (a RevEng CRC-8/16) · `random-poly` (off-catalogue) · `absent`, `snr_db` in the emission's own bandwidth, CFO uniform within ±0.2 × bandwidth; up to 10 distinct frames in 2 s at 500 kSps (one analyze window). The scenario annotation's `generic_fsk` truth carries every drawn parameter plus **`deepest_achievable`**, stated a priori (`solved` needs a check and ≥ 3 whole frames in the hold-out, else `framed`); per-frame `generic-fsk-frame` annotations carry the exact bits. Draws depend only on the seed, so an SNR sweep over one seed is a paired comparison |
| `noise_floor_rise` | AWARE-006 | L1 centred 1575.42 MHz, 2 Msps; +10 dB floor step at t0 = 0.1 s (whole band, or `rise_bandwidth_hz`); two weak CW; `start_utc`, `lat`/`lon` for correlation with a frozen feed |
| `injected_floor` | SPACE-050 | six 50 ms captures at 10/144/433.92/915/2450/5800 MHz, each with its own floor (seeded −42…−28 dBFS) and calibration constant K (seeded −80…−60 dB), per-capture provenance; one CW 10 dB below the floor power per segment |
| `occupancy_multi_hour` | AWARE-042 | 8 PMR446-style NBFM channels over 3 h: seeded burst schedule (`schedule.json`) plus two 250 ms IQ windows rendered on demand |
| `occupancy_markov_scene` | AWARE-042, AWARE-044, PROP-023 (T-117) | 48 h span, 8 channels: four Markov on/off channels at configured true FCO (1/10/50/100%), one hour-of-week-modulated channel, one novelty emitter silent until hour 30, a periodic launch-like event at 00Z/12Z, and a persistent wideband "boring" band; seeded interval schedule + irregular observation schedule (`schedule.json`) plus a few short IQ windows rendered on demand |
| `fm_broadcast_rds` | SIGNAL-062 | stereo WFM (L 1 kHz, R 400 Hz), 19 kHz pilot, RDS group 0A with PI `C0DE`, PS `HACKRIFF`, PTY 10; 456 kSps, 0.6 s |
| `adsb_squitter` | SIGNAL-001 fallback | four aircraft (`a0b1c2`, `4ca853`, `3c6444`, `c0ffee`), 8 DF17 squitters each cycling identification / even position / odd position / velocity, CRC-24, CPR; 1090 MHz at 2.4 Msps (readsb's rate; 2 Msps also works) |
| `pocsag_pagers` | SIGNAL-062, M1 tutorial 2 fixture (T-098) | three channels (offsets −40/0/+40 kHz) at 512/1200/2400 Bd, BCH(31,21)+even parity, one numeric and two alphanumeric pages (RICs 1234567/1876543/654321); 152.36 MHz, 132.3 kSps (6× 22050 Hz, so decimating to multimon-ng's rate is exact). Oracle: `multimon-ng` (built from source, not in Homebrew core — see `hkpy/synth/pocsag.py`) |
| `acars_message` | SIGNAL-062, M1 tutorial 3 fixture (T-098) | AM carrier (131.55 MHz) with a 2400 Bd MSK-like tone (mark 2400 Hz / space 1200 Hz), SYN·SYN·SOH·mode·reg·ack·label·block_id·STX·text·ETX framing, CRC-16; **synthetic only** — `acarsdec` was not a cheap Homebrew install, so this is this project's own reading of the public framing, cross-checked only by an independent decoder in `tests/test_synth.py`, not a third-party oracle (see `hkpy/synth/acars.py`) |

**Occupancy representation.** Multi-hour IQ is too large to store (3 h at 200 kSps is about
4.3 GB), so the truth is the burst schedule: every burst's channel, start and duration, with exact
occupancy statistics per channel, per window hour and per UTC hour of day, plus a duration
histogram. IQ windows are rendered from the schedule on demand (`windows`, `window_starts_s`, or
`render_start_s`). Rendering is a pure function of the seed, the schedule and the window start, so
the same burst renders identically in overlapping windows. History and occupancy tests can take
detections straight from the schedule. Detector tests replay the rendered windows.

`occupancy_markov_scene` (T-117) uses the same on-demand-IQ idea over a longer (48 h default),
fast-forwardable span, with channels driven by a seeded two-state Markov on/off process instead of
a Poisson burst train, so each channel's *true* FCO is a configured parameter (not just something
recomputed from the draw). It also adds an **observation schedule**: irregular revisit times over
the whole span (random gaps, or an explicit list), independent of which few windows got IQ
rendered, so a test can check a scheduler's sampled FCO against the hidden truth (ITU-R SM.2256
Annex 1 style, via `hkpy.synth.occupancy.wilson_ci`) without paying for full-resolution IQ at every
revisit. `schedule.json` carries per-channel intervals, exact overall and per-calendar-hour-of-week
(168 slots) occupancy, the novelty injection time, the periodic-event schedule, and the observation
schedule.

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
| `scenario` / `scenario` (one per file, whole-file box) | `scenario`, `recording`, `seed`, `params`, `generator`, `generator_version`, `use_cases`, `datatype`, `sample_rate_hz`, `n_samples`, `duration_s`, `dbfs_reference`, `calibration_rule`, `quantisation_noise_dbfs`, `clip_count`, `clip_fraction`, `overload`, `overload_rule`, `impairments[]`, plus per scenario: `emitter` (FSK), `noise_sigma_lsb`/`fill_bucket` (ADC fill, T-625), `negative_population` (T-626's N5: `id`, `shape`, `proposal_grid`, `off_grid` with both grid distances, `analysed_box`, `adjacent_leakage`, `mismatch_vs_miss`, `must_never_return`, `acceptable`, `counted_in`), `location`/`t0_utc`/`step_db` (floor rise), `segments[]` (injected floor), `window` (occupancy), `aircraft[]` (ADS-B) |
| `emission` / any | `center_hz`, `rf_center_hz`, `offset_hz`, `bandwidth_hz`, `power_dbfs`, `power_dbm`, `snr_db` (in `bandwidth_hz`, when > 0), `modulation`, `identity: {type, value}` where known, `lo_offset_hz` when shifted |
| `emission` / `cw` | `amplitude`, `phase_rad`, `snr_db_per_hz` (tone) |
| `emission` / `fsk-burst` | `levels`, `symbol_rate_bd`, `deviation_hz`, `mod_index`, `bt`, `nominal_center_hz`, `cfo_hz`, `burst_index`, `bit_order`, `mapping`, `constant_payload`, `frame: {n_bits, bits_hex, preamble_bits, preamble_hex, sync_hex, payload_hex, crc_hex, layout}`, `payload_fields`, `crc: {algorithm, poly, width, init, refin, refout, xorout, covers, start_bit, covered_bits, tail_bits, bit_order, in_reveng_catalogue, catalogue_name, value, valid}`, `identity` (`sensor_id`) |
| `emission` / `wfm-broadcast` | `peak_deviation_hz`, `stereo`, `preemphasis`, `pilot: {present, frequency_hz, deviation_hz}`, `audio: {...}`, `rds: {pi_hex, pi, ps, pty, tp, ta, music, di, group_types, n_groups, bitrate_bd, subcarrier_hz, deviation_hz, first_bit_s, encoding, check_poly, offset_words, blocks_hex}`, `label_expected`, `identity` (`rds_pi`) |
| `emission` / `adsb-df17` | `df`, `ca`, `icao`, `tc`, `message_kind`, `message_hex`, `crc_hex`, `crc`, `metadata` (`callsign`, or `altitude_ft`/`lat`/`lon`/`cpr_format`/`cpr_lat`/`cpr_lon`, or `ew_velocity_kt`/`ns_velocity_kt`/`vertical_rate_fpm`), `power_definition`, `identity` (`icao`) |
| `emission` / `pocsag-page` | `symbol_rate_bd`, `deviation_hz`, `mod_index`, `preamble_bits`, `sync_hex`, `idle_hex`, `bit_order`, `mapping`, `bch: {algorithm, generator_poly, codeword_bits, layout, corrects}`, `frame: {n_bits, n_codewords, n_batches}`, `ric`, `address`, `frame_position`, `function`, `message_kind`, `message_text`, `message_codewords_hex`, `identity` (`ric`) |
| `emission` / `acars-message` | `carrier_modulation`, `am_depth`, `subcarrier_modulation`, `symbol_rate_bd`, `mark_hz`, `space_hz`, `char_bits`, `framing`, `crc: {algorithm, poly, init, refin, refout, xorout, covers}`, `fields: {mode, reg, label, block_id, text}`, `text_expected`, `frame: {n_bits, chars_hex, crc_hex}`, `identity` (`acars_reg`) |
| `emission` / `nbfm-burst` | `deviation_hz`, `audio_tone_hz`, `channel`, `burst_index`, `burst_start_s`, `burst_duration_s`, `clipped_by_window`, `identity` (`channel-user`) |
| `emission` / `occupancy-markov`, `occupancy-diurnal`, `occupancy-novelty`, `occupancy-event`, `occupancy-boring` | `channel`, `interval_index`, `interval_start_s`, `interval_duration_s`, `clipped_by_window`, `target_fco` (nullable for the event/boring channels), `identity` (`channel-user`) |
| `emission` / `lora-packet` | `spreading_factor`, `chips_per_symbol`, `coding_rate` (`4/5`..`4/8`), `coding_rate_index`, `symbol_duration_s`, `symbol_rate_bd`, `chip_rate_hz`, `bits_per_symbol`, `preamble_symbols`, `sync_word`, `sfd_symbols`, `packet_index`, `payload_symbols[]`, `frame: {payload_hex, crc_hex, crc, n_nibbles, n_blocks, codeword_bits, n_symbols, coding}`, `sweep: {chirp_rate_hz_per_s, sweeps_per_second, f_low_hz, f_high_hz, stable_frequency (false), instantaneous_bandwidth_hz, instantaneous_frame_s, box_to_instantaneous_ratio, ratio_identity, box_is_bounding_box, limitation}`, `sweep_polyline[[t_s, f_hz]]`, `polyline_step_s`, `identity` (`lora_payload`) |
| `emission` / `adjacent-interferer` (T-626, N5) | `keying_bd`, `shaping`, `in_analysed_box` (false), `leaks_into_box` (true), `leak_in_box_dbfs`, `leak_in_box_dbfs_per_hz`, `leak_margin_over_floor_db`, `note` |
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
- and, when `readsb` is installed, every squitter decoded by readsb (ci8 converted to uc8 by XOR 0x80);
- POCSAG pages: an independent FM-discriminator + BCH(31,21) decoder recovers address/function/text
  on all three channels, and, when `multimon-ng` is installed, it independently decodes the same
  address/function/text from each channel's FM-discriminated, decimated-to-22050 Hz audio;
- ACARS: an independent envelope/MSK matched-filter decoder recovers the frame fields and the
  CRC-16 checks out (no third-party oracle — see the `acars_message` row above).
