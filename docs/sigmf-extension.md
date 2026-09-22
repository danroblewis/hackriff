# hackriff SigMF extension (`hackriff` namespace, v0.1.0)

*Seeded by T-001. Status: **PROVISIONAL**. Changes follow the `Provenance` schema in T-002.*

Recordings and fixtures are [SigMF](https://github.com/sigmf/SigMF) datasets (docs/07 §2.12,
§3.3). hackriff adds one optional extension namespace. Files that use it declare it in
`global.core:extensions` as `{"name": "hackriff", "version": "0.1.0", "optional": true}`. Readers
that don't know it can ignore it.

| Key | Where | Value |
|---|---|---|
| `hackriff:provenance` | `global` (whole recording) or a `captures` entry (that segment; overrides global) | A Provenance object (docs/07 §2.6), the JSON form of `hk_model::Provenance`. |
| `hackriff:clip_count` | `captures` entry | Non-negative integer: ADC samples clipped within that segment. Optional; omit when not measured. |
| `hackriff:truth` | `annotations` entry | A free-form ground-truth object, e.g. `{"kind": "cw", "frequency_hz": 1.0001e8}` or decoded fields from a CRC-valid decode. The synthetic generator (T-023) and valid decodes write it; tests assert against it. |

## `hackriff:provenance` fields

```json
{
  "device_id": "hackrf:<serial>",
  "tune": {"center_hz": 433920000.0, "sample_rate_hz": 2000000.0, "lna_db": 32.0,
           "vga_db": 20.0, "amp_on": false, "bandwidth_hz": 1750000.0},
  "overload": false,
  "quantisation_limited": false,
  "noise_sigma_lsb": 2.25,
  "temperature_c": 41.5,
  "antenna_port": "A1",
  "bias_tee": "off",
  "clock_source": "internal",
  "clock_locked": true,
  "calibration_state_ref": "<uuid v7>",
  "spur_mask_ref": "<uuid v7>",
  "timestamp_method": "host-arrival",
  "timestamp_error_budget_ns": 2000000
}
```

- **Required:** `device_id`, `tune` (all six fields), `overload`, `quantisation_limited`,
  `clock_source`, `clock_locked`, `timestamp_method`.
- **Optional:** `noise_sigma_lsb`, `temperature_c`, `antenna_port`, `bias_tee`, `calibration_state_ref`,
  `spur_mask_ref`, and `timestamp_error_budget_ns`. Omit a field when it is unknown; do not write
  `null`.
- `bias_tee` (T-325) is `off` or `on`: the antenna-port bias tee's state under this provenance.
  **An absent key means unknown, and unknown is not `off`** — the writer could not report it (a
  replayed or third-party recording), and the DC may well have been on. A reader must not default
  it to `off`: a bias tee left on into a passive or DC-shorted port is a hardware hazard, and an
  active antenna's LNA moves the noise floor, so bias-tee-on captures are not comparable with
  bias-tee-off ones. Provenance written before T-325 therefore has no key and reads as unknown,
  and an unknown record's canonical JSON is unchanged, so its dedup hash still matches.
- `overload` is sticky tune-state: the front end was judged overloaded under this tune/gain state.
  A change of state is a new provenance object. Every detection under an overloaded provenance is
  flagged `clipped`.
- `quantisation_limited` is true when the noise floor under this gain state is within 3 dB of the
  ADC quantisation floor (added from spike S4, 2026-09-13). It is stable per gain state.
- `noise_sigma_lsb` (T-625) is **ADC fill**: the per-component noise σ in ADC LSB under this
  state. It is the variable the evidence-metric calibration tables are conditioned on
  (ADR-0015 §13.3) and it is **not** the gain setting — T-547 applied 51 dB of gain with the ADC
  skipped and reproduced the float table to 0.02 bits on every metric (docs/21 §4), so a table or
  a breakdown keyed on gain is keyed on a no-op. **An absent key means not measured, and not
  measured is `under_filled`, not `nominal`**: a reader credits calibrated metrics 0 bits for the
  window rather than assuming the fill was fine. A recording with no ADC (a `cf32_le` file) has no
  fill and must omit the key rather than invent one. Provenance written before T-625 has no key,
  and its canonical JSON — and therefore its dedup hash — is unchanged.
- `clock_source` is one of `internal`, `external` (10 MHz into CLKIN) or `gpsdo`.
- `timestamp_method` is one of `host-arrival`, `gnss-tagged`, `external-reference`, `synthetic` or
  `unknown`. HackRF One has no hardware 1PPS, so live captures are `host-arrival` unless
  disciplined.

### Why `clip_count` is not in provenance

Provenance describes *state* and is deduplicated by value: many frames, detections and segments
share one stored row while nothing changes. A clipped-sample count differs from span to span, so
inside provenance it would make nearly every record unique and defeat the deduplication (T-002
review). Counts live per capture segment (`hackriff:clip_count`) and per detection
(`Detection::clip_count`). Readers ignore a legacy `clip_count` key inside `hackriff:provenance`.

## Conventions

- Live captures **must** carry `hackriff:provenance` recording frequency, rate, LNA/VGA/amp and
  antenna (CLAUDE.md coordination rules).
- Unknown keys, including other extensions, are preserved on read and write by both
  implementations: Rust `hk_model::sigmf` and Python `py/hkpy/sigmf.py`.
- Datatypes supported now: `ri8`, `ru8`, `ci8`, `cu8`, and the `_le` forms of `ri16`, `ci16`,
  `ru16`, `cu16`, `ri32`, `ci32`, `rf32`, `cf32`, `rf64`, `cf64`.
