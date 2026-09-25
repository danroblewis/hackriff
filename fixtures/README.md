# fixtures/ — SigMF test recordings

IQ fixtures for replay tests (T3) and acceptance tests. Every fixture is a SigMF dataset: a
`.sigmf-meta` plus a `.sigmf-data`. Use the `hackriff:provenance`, `hackriff:clip_count` and
`hackriff:truth` extension keys (`docs/sigmf-extension.md`) and reference each fixture from its
acceptance test by use-case ID.

## Policy

- **Small fixtures (≤ 25 MB per `.sigmf-data`; the tooling enforces 25 000 000 bytes) are
  committed** under `fixtures/`. The `.sigmf-data` goes through **Git LFS** (`.gitattributes`); the
  `.sigmf-meta` is plain JSON. Run `git lfs install` once per clone.
- **Larger captures** (HIL, field, long surveys) live in the **external store `fixtures/store/`**
  (or `$HACKRIFF_FIXTURE_STORE`), which is gitignored and synced out of band. They are indexed by
  the committed **`fixtures/manifest.json`**.
- **Record capture settings** in every live capture's metadata: frequency, rate, LNA/VGA/amp,
  antenna.
- **Check the licence** of any third-party sample set before committing or indexing it (ADR-0010).
  Put it in `core:license` and the manifest.
- Synthetic scenarios are code (seed + parameters), not files, unless a small generated file is
  needed as a smoke fixture.
- **Legal:** receive-only captures. Truth for third-party traffic is PHY metadata only; never
  store payload content of unidentified traffic, cellular or paging (CLAUDE.md).

## `manifest.json` (version 2)

`{"version": 2, "store": "fixtures/store", "entries": [...]}`, one entry per file. Paths are
relative to `fixtures/`; external ones start with `store/` (resolved against the store directory).

| Field | Meaning |
|---|---|
| `name` | Fixture or capture name (a fixture's `.sigmf-data` and `.sigmf-meta` share it) |
| `status` | `committed` (in git / LFS), `external` (store only) or `blocked` (not producible yet) |
| `path`, `size_bytes`, `sha256` | Required for committed and external entries |
| `kind` | `sigmf-data`, `sigmf-meta`, `sweep-csv`, `sweep-sidecar`, `labels` |
| `source` | Where it came from (a store path for trimmed fixtures) plus `source_sha256`, `source_start_s` |
| `use_cases` | Use-case IDs served |
| `license` | e.g. `project-owned capture` |
| `reason` | Required for `blocked` entries |
| `truth_summary`, `purpose`, `clip_count`, `lfs`, `datetime` | Informational |

## Tooling (`py/fixtures/`, scripts run with the hkpy environment)

| Command | What |
|---|---|
| `just fixtures-verify [--external]` | sizes + sha256 of committed files, LFS pointers resolved, size cap; `--external` also checks store originals |
| `just fixtures-fetch [--from DIR] [NAME…]` | copy/verify external originals into `fixtures/store/` with checksums (no network) |
| `just fixtures-build-2026-09-13 [--store DIR]` | regenerate `hackrf/2026-09-13/`, `sweeps/2026-09-13/` and the manifest from the store |
| `uv run --project py python py/fixtures/trim.py SRC DST --start-s S --duration-s D` | sample-exact SigMF window (`core:global_index`, `core:datetime`, clip counts, provenance) |
| `uv run --project py python py/fixtures/annotate.py META TRUTH.json` | write `hackriff:truth` annotations |

`rds_ref.py` (RDS PI/PS reference decoder) and `fsk_ref.py` (S5 fixed-rate FSK sync truth) are
the truth generators the build uses. Tests: `py/tests/test_fixture_tooling.py`.

## Contents

- `tiny/tone.sigmf-*`: an 8 KB noiseless ci8 CW tone for `just replay` and parser smoke tests.
  Regenerate with `uv run --project py python fixtures/tiny/make_tone.py`.
- `hackrf/fm-stations/`: the per-station RDS set (T-926): one narrow capture per broadcast station
  (2.4 Msps, ~5 s, 24 MB), clipped from the live app's IQ ring by the explorer agent, with hidden
  truth — the PI an independent decoder (`py/fixtures/rds_ref.py`) read from the same IQ, and the PS
  only where it is stable. `tests/e2e/tests/acceptance/fm_stations.rs` runs every capture in the
  directory blind through the mock SDR and requires every station's PI, so the set grows by
  adding files.
- `hackrf/2026-09-13/`: five annotated windows of the 2026-09-13 HackRF One captures (FM + RDS,
  915 MHz FHSS FSK, urban FM-band clipped/mid gain pair, 433 MHz noise-only control). See its README.
- `sweeps/2026-09-13/`: three `hackrf_sweep` surveys (plain CSV, not LFS). See its README.
