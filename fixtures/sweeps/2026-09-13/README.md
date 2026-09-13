# hackrf_sweep surveys, 2026-09-13

Captured by the coordinator with the HackRF One (board rev older than r6, firmware 2026.01.3,
libhackrf 0.9.2), internal clock (measured −6.8 ppm, spike S5), antenna unknown (as attached by the
user). Receive-only. Licence: project-owned capture. Use cases: SPACE-050 and AWARE-042 (survey
floor / occupancy reference); spike S4 used them for the two-gain survey comparison.

| File | Range | Bin width | Gain (LNA/VGA/amp) | Sweeps | Purpose |
|---|---|---|---|---|---|
| `sweep_1-1000M_w100k_l24g20a0.csv` | 1–1000 MHz | 98 039 Hz | 24 / 20 / off | 20 | wide survey, moderate gain |
| `sweep_1-1000M_w100k_l32g40a1.csv` | 1–1000 MHz | 98 039 Hz | 32 / 40 / on | 20 | wide survey, high gain (overload comparison) |
| `sweep_1000-6000M_w500k_l32g30a1.csv` | 1–6 GHz | 454 545 Hz | 32 / 30 / on | 10 | wide survey, upper range |

Each `.json` sidecar holds the exact `hackrf_sweep` command, hardware string, antenna, purpose and
start time (UTC). `hackrf_sweep` ran at 20 Msps with the 15 MHz baseband filter.

## CSV format (`hackrf_sweep` native)

One row per FFT block, no header, comma + space separated:

```
date, time, hz_low, hz_high, hz_bin_width, num_samples, dB, dB, dB, ...
2026-09-13, 04:09:58.210746, 1000000, 6000000, 98039.22, 204, -54.69, -57.80, ...
```

- `date`, `time`: host local time of the sweep row (the sidecar `datetime` is UTC).
- `hz_low`, `hz_high`: the 5 MHz (for `-w 100000`) span this row covers; bin *i* centre is
  `hz_low + (i + 0.5) * hz_bin_width`.
- `num_samples`: FFT size used for the row.
- `dB` values: uncalibrated power per bin, dB relative to full scale. Each row is a single FFT
  (spike S4 measured n_eff ≈ 1.2), so average several sweeps before thresholding.
- Rows from one sweep share a timestamp; a new sweep starts when `hz_low` wraps to the range start.
- Within a sweep the 5 MHz spans are adjacent but written out of frequency order (e.g. 1–6,
  11–16, 6–11 MHz: hackrf_sweep emits two spans per tuning step); sort by `hz_low`.

The n × 10 MHz reference-harmonic spurs dominate the spur family in these surveys (S4 §3.7).
