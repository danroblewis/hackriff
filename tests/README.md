# tests/ — end-to-end IQ-replay scenarios

End-to-end tests replay SigMF fixtures (`fixtures/`) or synthetic scenarios (`py/`) through the
full pipeline. They assert on the data-model objects of `docs/07`: detections, estimated
parameters, decoded bits and inventory entries. Each test names the use-case IDs it covers. No
hardware; these run in CI through `just test`. The replay harness arrives in T-023, and the seven
slice-1 acceptance tests in T-024. Unit and component tests stay inside their crates.
