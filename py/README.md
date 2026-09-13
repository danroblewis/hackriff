# py/ — hackriff Python tooling

Covers SigMF fixture tooling, the synthetic IQ generator (T-023) and research code. **Python is
for orchestration and research only, never the real-time path** (ADR-0010).

```sh
uv sync             # create py/.venv from uv.lock
uv run pytest       # run the tooling tests (also `just test-py` from the repo root)
```

- `hkpy/sigmf.py` reads and writes `.sigmf-meta` documents. It is consistent with the Rust
  `hk_model::sigmf` types, including the `hackriff:` extension (`docs/sigmf-extension.md`).
- `fixtures/` will hold fetch/verify tooling for the external fixture store (`fixtures/README.md`).
