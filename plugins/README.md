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

The host enforces `output.content_class` as a ceiling, clamped further by the input channel's class: plugin messages can't claim a less restrictive class. Input arrives on stdin, either `hackriff-v1` framed or `raw`; messages leave on stdout as NDJSON `decode`/`annotation`/`log` lines. Full spec: [docs/stream-contract.md §9](../docs/stream-contract.md).

**Restricted classes need a metadata allowlist.** If `output.content_class` forbids content (`metadata-only`, `restricted-cellular`, `restricted-paging`), the manifest must declare `output.metadata_keys`: each key the host may keep, typed `integer`, `number`, `boolean`, `hex`/`digits` (with `max_len`) or `enum` (with `values`). Free-text strings cannot be allowlisted. Optional `frame_models`, `labels` and `identity` (`scheme`, `charset`, `max_len`) allowlist the frame model, annotation label and identity shape. Everything else is dropped or replaced by `schema_id` whenever a line's class forbids content, and the plugin's `log` lines and stderr are counted, not stored.

## Trust boundary

**A manifest is the trust boundary, not the plugin's output.** Its `executable` is trusted code that runs as the hackriff user. It receives the raw channel samples (the data plane is not gated) and could forward or store them itself, bypassing every gate in the host. The host's class ceiling, metadata allowlist, log withholding and stream gating contain *accidental* leaks from well-meaning decoders (for example multimon-ng printing pager text). They do not contain a malicious executable. So:

- Only add a manifest for a tool you have reviewed, pinned to a version, and recorded in the ADR-0010 ledger.
- A manifest's `content_class`, `metadata_keys` and `identity` are legal-guardrail declarations. Review them like code.
- Wrappers must not start daemons that escape the plugin's process group (`setsid`). The host kills the group on exit, stall and shutdown; escaped descendants are detected (their output readers are abandoned and counted) but not killed.

- `dummy/`: the test plugin for the host (`hk-dummy-plugin`, a bin target of `crates/hk-plugins`).
