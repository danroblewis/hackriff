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

The host enforces `output.content_class` as a ceiling, clamped further by the input channel's class: plugin messages can't claim a less restrictive class. Input arrives on stdin, either `hackriff-v1` framed or `raw`; messages leave on stdout as NDJSON `decode`/`annotation`/`log` lines. `output.schema_id` must be a token (`[A-Za-z0-9_.:/-]{1,64}`). Full spec: [docs/stream-contract.md §9](../docs/stream-contract.md).

**Restricted classes need a metadata allowlist.** If `output.content_class` forbids content (`metadata-only`, `restricted-cellular`, `restricted-paging`), the manifest must declare `output.metadata_keys`:
- Each key the host may keep is typed `integer`, `number`, `boolean`, `hex`/`digits` or `enum` (with `values`). Free-text strings cannot be allowlisted.
- Optional `frame_models`, `labels` and `identity` (`scheme`, `charset`, `max_len`) allowlist the frame model, annotation label and identity shape.
- Everything else is dropped or replaced by `schema_id` whenever a line's class forbids content, and the plugin's `log` lines and stderr are counted, not stored.

**Allowlist defaults.** A policy only ever applies under a restricted class, so each typed field is a covert-channel budget:
- `hex`/`digits` keys and `identity` default to `max_len` **8**. A longer value (up to 64) needs an explicit `max_len` plus a `review_note` string on that key, for example `{"type": "hex", "max_len": 16, "review_note": "why 16, who reviewed, when"}`. Without the note the manifest is refused.
- Reviewed long strings and `integer`/`number` keys (64 bits per record) load with a warning: printed by `PluginManifest::load`, and copied into the plugin's log ring.
- A restricted line's `sample_index` must fall inside the input the host offered to that plugin, or the line is dropped and counted. A wrapper around a `raw`-framed tool must therefore report the host's record sample index, not its own byte count.
- A restricted annotation's `confidence` is rounded to 0.01.

**Republishing restricted output.** The stream publisher reduces restricted messages to *its own* metadata policy, so a republisher for a restricted plugin is created with `Publisher::with_metadata_policy(header, config, manifest.output.metadata_policy)`, with `header.message_schema` set to `output.schema_id`. A plain `Publisher::new` republishes restricted rows with empty metadata, and refuses a restricted-class messages stream outright.

## Example restricted-paging policy

[`crates/hk-plugins/policies/restricted-paging.json`](../crates/hk-plugins/policies/restricted-paging.json) is a documented example `output` block, not a plugin (`hk_plugins::EXAMPLE_RESTRICTED_PAGING_OUTPUT`). It allowlists only:

| Key | Type |
|---|---|
| `capcode` | digits, at most 8 |
| `function` | enum `"0"`–`"3"` |
| `baud` | enum `"512"`, `"1200"`, `"2400"` |
| `encoding` | enum `"numeric"`, `"alpha"`, `"tone"` |

`t` is host-stamped from `sample_index`; it is not a metadata key. **Message bodies, numeric pages included, are content and are never allowlisted.** A paging wrapper must put the page text (alpha or numeric) in `content`, which the host refuses to store or stream under `restricted-paging`.

## Trust boundary

**A manifest is the trust boundary, not the plugin's output.** Manifests are *trusted but reviewed*.

A manifest's `executable` is trusted code that runs as the hackriff user. It receives the raw channel samples (the data plane is not gated) and could forward or store them itself, bypassing every gate in the host. The host contains *accidental* leaks from well-meaning decoders (for example multimon-ng printing pager text); it does not contain a malicious executable. These guards do that containment:
- class ceiling;
- metadata allowlist and its defaults;
- `sample_index` bound;
- log withholding;
- stream gating.

So:

- Only add a manifest for a tool you have reviewed, pinned to a version, and recorded in the ADR-0010 ledger.
- A manifest's `content_class`, `metadata_keys`, `identity`, every `max_len` above 8 and its `review_note` are legal-guardrail declarations. Review them like code, and read the load warnings.
- Wrappers must not start daemons that escape the plugin's process group (`setsid`). The host kills the group on exit, stall and shutdown; escaped descendants are detected (their output readers are abandoned and counted) but not killed.
- Some residual channels remain open to a plugin that modulates them deliberately: timing and ordering of lines, `t` within the input range, and values within their allowlisted types. They are noted in [docs/stream-contract.md §11](../docs/stream-contract.md), not fixed.

- `dummy/`: the test plugin for the host (`hk-dummy-plugin`, a bin target of `crates/hk-plugins`).
- `gnss-sdr/`: GPS L1 full receiver (T-323). GNSS-SDR (GPL-3.0-or-later) exec'd per recorded dwell behind `hk-plugin-gnss-sdr` (a bin target of `crates/hk-gnss`); emits `hackriff.gnss/1` evidence decodes only — no identity, no annotation. See the C36 card.
