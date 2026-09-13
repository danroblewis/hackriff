# hackriff SigMF extension (`hackriff` namespace, v0.1.0)

*Seeded by T-001. Status: **PROVISIONAL**. Changes follow the `Provenance` schema in T-002.*

Recordings and fixtures are [SigMF](https://github.com/sigmf/SigMF) datasets (docs/07 §2.12,
§3.3). hackriff adds one optional extension namespace. Files that use it declare it in
`global.core:extensions` as `{"name": "hackriff", "version": "0.1.0", "optional": true}`. Readers
that don't know it can ignore it.

| Key | Where | Value |
|---|---|---|
| `hackriff:provenance` | `global` (whole recording) or a `captures` entry (that segment; overrides global) | A Provenance object (docs/07 §2.6), the JSON form of `hk_model::Provenance`. |
| `hackriff:truth` | `annotations` entry | A free-form ground-truth object, e.g. `{"kind": "cw", "frequency_hz": 1.0001e8}` or decoded fields from a CRC-valid decode. The synthetic generator (T-023) and valid decodes write it; tests assert against it. |

## `hackriff:provenance` fields

```json
{
  "device_id": "hackrf:<serial>",
  "tune": {"center_hz": 433920000.0, "sample_rate_hz": 2000000.0, "lna_db": 32.0,
           "vga_db": 20.0, "amp_on": false, "bandwidth_hz": 1750000.0},
  "clip_count": 0,
  "overload": false,
  "temperature_c": 41.5,
  "antenna_port": "A1",
  "clock_source": "internal",
  "clock_locked": true,
  "calibration_state_ref": "<uuid v7>",
  "spur_mask_ref": "<uuid v7>",
  "timestamp_method": "host-arrival",
  "timestamp_error_budget_ns": 2000000
}
```

- **Required:** `device_id`, `tune` (all six fields), `clip_count`, `overload`, `clock_source`,
  `clock_locked`, `timestamp_method`.
- **Optional:** `temperature_c`, `antenna_port`, `calibration_state_ref`, `spur_mask_ref`, and
  `timestamp_error_budget_ns`. Omit a field when it is unknown; do not write `null`.
- `clock_source` is one of `internal`, `external` (10 MHz into CLKIN) or `gpsdo`.
- `timestamp_method` is one of `host-arrival`, `gnss-tagged`, `external-reference`, `synthetic` or
  `unknown`. HackRF One has no hardware 1PPS, so live captures are `host-arrival` unless
  disciplined.

## Conventions

- Live captures **must** carry `hackriff:provenance` recording frequency, rate, LNA/VGA/amp and
  antenna (CLAUDE.md coordination rules).
- Unknown keys, including other extensions, are preserved on read and write by both
  implementations: Rust `hk_model::sigmf` and Python `py/hkpy/sigmf.py`.
- Datatypes supported now: `ri8`, `ru8`, `ci8`, `cu8`, and the `_le` forms of `ri16`, `ci16`,
  `ru16`, `cu16`, `ri32`, `ci32`, `rf32`, `cf32`, `rf64`, `cf64`.
