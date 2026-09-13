# hackriff

An exploration-first signals-analysis device and software for software-defined radio, meant to
replace the HackRF PortaPack. It **finds** interesting signals: survey, detect, characterise,
decode, and remember what the spectrum looked like over time. You don't have to know the
frequency and mode first. It uses a HackRF One front end and a Jetson for compute. The Rust core
builds and tests on macOS without hardware.

- **Project brief, constraints and working rules:** [`CLAUDE.md`](CLAUDE.md)
- **Research and planning index:** [`docs/README.md`](docs/README.md) (task state:
  [`docs/tasks.yaml`](docs/tasks.yaml))

## Build, test, run

Requires Rust (stable, edition 2024), [`just`](https://github.com/casey/just),
[`uv`](https://github.com/astral-sh/uv) and `git lfs` (run `git lfs install` once).

```sh
just build                                  # cargo build --workspace (CPU path, `gpu` off)
just test                                   # Rust tests + Python tooling tests; no hardware
just lint                                   # cargo fmt --check + clippy -D warnings
just replay fixtures/tiny/tone.sigmf-meta    # run a SigMF fixture through the pipeline
JETSON_HOST=user@jetson just deploy-jetson  # rsync + on-device release build with CUDA (`gpu`)
```

`hk replay` only parses and summarises the SigMF metadata for now; the replay source and harness
land in T-003/T-023.

## Layout

| Path | What |
|---|---|
| `crates/` | Cargo workspace: `hk-model` (data model), `hk-core`, `hk-dsp`, `hk-detect`, `hk-estimate`, `hk-demod`, `hk-store`, `hk-context`, `hk-api`, `hk-plugins`, `hk-cli` (`hackriffd`, `hk`) |
| `py/` | Python tooling (uv): SigMF fixtures, synthetic IQ, research. Never the real-time path |
| `fixtures/` | SigMF test recordings (Git LFS; large captures in an external store) |
| `tests/` | End-to-end IQ-replay scenarios |
| `plugins/`, `ui/`, `spikes/` | Decoder plugins, web client, throwaway spike code |

The project licence is undecided. Dependency licences are tracked in
[ADR-0010](docs/adr/0010-language-and-licence-ledger.md).
