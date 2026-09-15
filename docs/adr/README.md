# Architecture Decision Records

One file per major decision, `NNNN-title.md`. Format: **Context → Options → Trade-offs → Decision → Consequences → Status**, with the capabilities/data-model objects it touches and, where relevant, the Phase 4 spike that would confirm or overturn it.

**Status values:** `PROVISIONAL` (made autonomously during planning, cheapest-to-reverse option chosen, awaiting user review or a spike), `ACCEPTED` (confirmed by the user or a passing spike), `SUPERSEDED by NNNN`.

Changing an ACCEPTED ADR goes to Fable plus the user, never a silent code change (`prompts/model-selection.md`).

| ADR | Title |
|---|---|
| [0001](0001-pipeline-runtime.md) | Pipeline runtime and live reconfiguration |
| [0002](0002-ui-web-vs-native.md) | UI: web versus native |
| [0003](0003-process-plugin-model.md) | Process and plugin model |
| [0004](0004-stream-output-contract.md) | Bitstream and stream-output contract |
| [0005](0005-survey-dwell-scheduler.md) | Survey/dwell scheduler |
| [0006](0006-storage.md) | Storage |
| [0007](0007-compute-placement.md) | Compute placement |
| [0008](0008-offline-first-context.md) | Offline-first external context |
| [0009](0009-hardware-platform.md) | Hardware platform sketch |
| [0010](0010-language-and-licence-ledger.md) | Language, toolchain, and dependency licence ledger |
| [0011](0011-decoder-workbench-contracts.md) | Decoder workbench contracts: blocks, recipes, field maps, inspector stream |
