# tests/ — end-to-end IQ-replay scenarios

End-to-end tests replay SigMF fixtures (`fixtures/`) or synthetic scenarios (`py/`) through the
full pipeline. They assert on the data-model objects of `docs/07`: detections, estimated
parameters, decoded bits and inventory entries. Each test names the use-case IDs it covers. No
hardware; these run in CI through `just test`. The seven slice-1 acceptance tests arrive in
T-024. Unit and component tests stay inside their crates.

## Harness: `tests/e2e` (crate `hk-e2e`, T-023)

```rust
use hk_e2e::{synth_or_skip, BoxTolerance, Pipeline, SynthRequest, Tolerance, assert_param, match_detections};

let out = synth_or_skip!(SynthRequest::new("fsk_burst_train").seed(1).param("snr_db", 12));
let fx = out.fixture(0).unwrap();                       // SigMF meta + truth via hk_model::sigmf
let outputs = Pipeline::new().with(my_detector).run(&fx).unwrap();
let bursts = fx.of_kind("fsk-burst");
let report = match_detections(&outputs.detections, &bursts, &fx.artefacts(),
                              BoxTolerance { time_s: 2e-3, freq_hz: 5e3 });
report.assert_all_found(&["AWARE-036"], &bursts);
report.assert_false_alarms_at_most(&["AWARE-036"], &outputs.detections, 0);
assert_param(&["AWARE-036"], "symbol_rate_bd", estimate, bursts[0].expect_f64("symbol_rate_bd"), Tolerance::Rel(0.01));
```

- **Synthetic scenarios.** `SynthRequest::generate` runs `uv run --locked --project py python -m hkpy.synth …` into `target/synth-cache/<scenario>-<hash>/`. The hash covers the request and the generator sources, and a cached directory is reused. When `uv` is missing, `synth_or_skip!` prints `SKIP` and returns; set `HK_E2E_REQUIRE_SYNTH=1` to fail instead. `HK_SYNTH_CACHE` and `HK_UV` override the cache location and the uv binary. The scenarios and the `hackriff:truth` schema are documented in `py/README.md`.
- **Fixtures.** `Fixture::load(path)` works for any SigMF recording. Truth items carry `role` (scenario / emission / artefact / floor / event, or unlabelled for hand annotations), `kind`, a time/frequency box, and typed field access (`f64("symbol_rate_bd")`, `str("/rds/ps")`, `identity()`).
- **Samples.** `samples.rs` is a **seam**: a minimal ci8/cu8/cf32_le reader, to be replaced by hk-core's replay source when T-003 merges.
- **Pipeline.** `Stage` is the plug-in point for capability stages. Stages get metadata with the annotations stripped, so no truth leaks. The output records (`DetectionBox`, `ParameterEstimate`, `DecodedMessage`, `FloorEstimate`, untyped `records`) are provisional stand-ins for the T-002 data-model objects.
- **Assertions.**
  - `match_detections` does one-to-one matching with time/frequency tolerance. It reports `missed` truth, `explained` detections (duplicates, or boxes that overlap an artefact such as a spur or IM3 product), and `false_alarms` outside every box.
  - `assert_param`/`check_param` take an absolute or relative tolerance.
  - Every failure message starts with the use-case IDs.
- `tests/e2e/tests/smoke.rs` has one smoke test per scenario (generate, load, check the truth plumbing against the samples) plus a cache test and a committed-fixture test.
