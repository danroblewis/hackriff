# Calibration tables (`hackriff.calibration/1`)

One file per block, `<block>.json`, keyed inside by `name@version` (ADR-0015 §2.2, §13.2–§13.3;
T-853 = MAUTO M-2). A block-version bump invalidates its file: a table for `x@1` never scores
`x@2`.

- **What a file is.** Quantile levels of the block's calibrated metric(s) under the **noise
  null**, per support `n`, with the levels the support cannot express refused (`unexpressible`)
  and `admissible_bits` ≤ the 6-bit calibrated claim cap. §13.1's `correlation`/`groups` appear
  where a block publishes more than one metric at a stage.
- **The null.** 8-bit-quantised complex noise (σ = 8 LSB) at 48 kHz through the block's canonical
  prefix (`crates/hk-synth/src/nullchain.rs`): S0 channel 4× oversampled, 4800 Bd symbols,
  9600 chip/s Manchester. Valid for S1 inputs oversampled up to 4×.
- **Fill.** Conditioned on T-619's tighter nominal bucket (σ ≥ 1.0 LSB, clip ≤ 10 %), as
  ADR-0015 §16.4 requires of the first generation; a window outside it scores 0 bits.
- **Regenerate** (≈ 1 min, 4096 windows per cell): `cd py && uv run --locked python -m hkpy.calibrate generate`.
  The Rust blocks produce every raw value (`cargo run --release -p hk-synth --example calibration_draws`);
  the Python side only does the statistics.
- **Checked** by `crates/hk-synth/tests/calibration_tables.rs` on 1000 fresh null windows per
  table, and by `py/tests/test_calibrate.py`.
- **Not yet generated:** the mismatched-parameter nulls (§13.3), and a table for `subcarrier`
  (its `pilot_lock` scores `NoTable`, 0 bits).
