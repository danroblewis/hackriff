# fixtures/ — SigMF test recordings

IQ fixtures for replay tests (T3) and acceptance tests. Every fixture is a SigMF dataset: a
`.sigmf-meta` plus a `.sigmf-data`. Use the `hackriff:provenance` and `hackriff:truth` extension
keys (`docs/sigmf-extension.md`) and reference each fixture from its acceptance test by use-case ID.

## Policy

- **Small fixtures (≤ 25 MB per `.sigmf-data`) are committed** under `fixtures/`. The
  `.sigmf-data` goes through **Git LFS** (`.gitattributes`); the `.sigmf-meta` is plain JSON.
  Run `git lfs install` once per clone.
- **Larger captures** (HIL, field, long surveys) live in the **external store `fixtures/store/`**,
  which is gitignored and synced out of band. They are indexed by the committed
  **`fixtures/manifest.json`**, one entry per file:
  `{"path": "store/…", "sha256": "…", "size_bytes": N, "source": "…", "license": "…"}`.
- `py/fixtures/` will hold the fetch/verify tooling (checksums against the manifest).
- **Record capture settings** in every live capture's metadata: frequency, rate, LNA/VGA/amp,
  antenna.
- **Check the licence** of any third-party sample set before committing or indexing it (ADR-0010).
  Put it in `core:license` and the manifest.
- Synthetic scenarios are code (seed + parameters), not files, unless a small generated file is
  needed as a smoke fixture.

## Contents

- `tiny/tone.sigmf-*`: an 8 KB noiseless ci8 CW tone for `just replay` and parser smoke tests.
  Regenerate with `uv run --project py python fixtures/tiny/make_tone.py`.
