# plugins/ — decoder plugins

One directory per decoder plugin (readsb, rtl_433, multimon-ng, ...) holding its manifest and a
thin wrapper. Plugins run as supervised subprocesses under `hk-plugins` (ADR-0003). The process
boundary keeps GPL decoders out of the core licence (ADR-0010). Add every wrapped tool to the
ADR-0010 ledger with its licence. The first plugin is readsb (T-015), built against the T-014
plugin contract.

## Manifest

Each plugin has `plugins/<id>/manifest.json`. It is JSON, and unknown fields are errors.
- **Required:** `manifest_version` (1), `id`, `version`, `licence`, `executable`, `input` (`kind`, `datatype`) and `output` (`schema_id`, `content_class`).
- **Optional:** `args` (with `{input.*}`/`{param.*}` placeholders), `params`, `restart` and `limits`.

The host enforces `output.content_class` as a ceiling: plugin messages can't claim a less restrictive class. Input arrives on stdin, either `hackriff-v1` framed or `raw`; messages leave on stdout as NDJSON `decode`/`annotation`/`log` lines. Full spec: [docs/stream-contract.md §9](../docs/stream-contract.md).

- `dummy/`: the test plugin for the host (`hk-dummy-plugin`, a bin target of `crates/hk-plugins`).
