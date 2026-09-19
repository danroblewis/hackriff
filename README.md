# hackriff

An exploration-first signals-analysis tool for software-defined radio, meant to replace the
HackRF PortaPack. It **finds** interesting signals — survey, detect, characterise, decode, and
remember what the spectrum looked like over time — so you don't have to know the frequency and
mode first. It uses a HackRF One as the front end (a Jetson is the eventual compute target). The
Rust core builds and runs on macOS or Linux; the web UI runs in any modern browser.

- **Project brief, constraints and working rules:** [`CLAUDE.md`](CLAUDE.md)
- **Research and planning:** [`docs/README.md`](docs/README.md)

## Quick start — run the server

**1. Install the prerequisites** (once):

| Need | macOS (Homebrew) | Debian/Ubuntu |
|---|---|---|
| Rust (stable) | [rustup.rs](https://rustup.rs) | [rustup.rs](https://rustup.rs) |
| `just` task runner | `brew install just` | `cargo install just` |
| Node + npm (builds the UI) | `brew install node` | `apt install nodejs npm` |
| HackRF driver + tools + pkg-config | `brew install hackrf pkg-config` | `apt install hackrf libhackrf-dev pkg-config` |

Plug in the HackRF and confirm the OS sees it:

```sh
hackrf_info      # should print the serial + firmware; if not, fix the USB/driver first
```

> **Build fails with `` `libhackrf` was not found `` / pkg-config?** The HackRF *dev* library or
> `pkg-config` is missing. Install both per the table above (on Debian it must be `libhackrf-**dev**`,
> not just `libhackrf`). If it's installed but still not found — common on Apple Silicon — run
> `export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig"` before `just run`. As a last resort, set
> `HACKRF_LIB_DIR` to the folder containing `libhackrf.dylib`/`libhackrf.so` to bypass pkg-config.

**2. Build and run** — one command:

```sh
just run
```

That builds the web UI and the `hk` binary (with HackRF support), autodetects the radio, and
starts the server. It prints a line like:

```
open http://127.0.0.1:8080/#token=abc123...
```

**Open that full URL (including the `#token=...`) in a browser** — that's the app. `Ctrl-C` stops
the server. The token is generated automatically; the whole URL is what grants access.

**No HackRF handy?** `just run` will tell you how to replay a bundled recording instead, so you can
see the UI without a radio:

```sh
target/release/hk serve --replay fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta \
  --loop --ui-dist ui/dist --bind 127.0.0.1:8080
```

### Tuning the run

`just run` starts on the FM band (100.8 MHz, 2.4 Msps) as a sane default. To point the radio
elsewhere, run `hk serve` yourself with your own arguments — see `target/release/hk serve --help`.
Common ones: `--center-hz`, `--rate`, `--lna`/`--vga` (gains), `--amp`, `--bind <addr:port>`.

## Building and testing (development)

Additionally needs [`uv`](https://github.com/astral-sh/uv) (Python tooling) and
`git lfs` (run `git lfs install` once, for the recording fixtures).

```sh
just build     # cargo build --workspace (no hardware, no HackRF feature)
just test      # Rust + Python + UI tests; no hardware needed
just lint      # cargo fmt --check + clippy -D warnings
just gate      # the merge gate: inspects the diff and runs exactly the suites it needs
```

The HackRF driver (libhackrf) is only linked when building with `--features hackrf` (which
`just run` does); the plain `just build`/`just test` paths need no radio and no libhackrf.

## Layout

| Path | What |
|---|---|
| `crates/` | Cargo workspace: `hk-model` (data model), `hk-core`, `hk-dsp`, `hk-detect`, `hk-estimate`, `hk-demod`, `hk-store`, `hk-context`, `hk-api`, `hk-pipeline`, `hk-plugins`, `hk-cli` (the `hk` binary + `hackriffd`) |
| `ui/` | TypeScript + WASM web client (built to `ui/dist` by `npm run build`) |
| `ops/` | Long-running dev-orchestration helpers (see [`ops/README.md`](ops/README.md)) — not needed to run the server |
| `py/` | Python tooling (uv): SigMF fixtures, synthetic IQ, research. Never the real-time path |
| `fixtures/` | SigMF test recordings (Git LFS) |
| `tests/`, `plugins/`, `spikes/` | End-to-end IQ-replay scenarios, decoder plugins, throwaway spikes |

The project licence is undecided; dependency licences are tracked in
[ADR-0010](docs/adr/0010-language-and-licence-ledger.md). GPL components stay behind the plugin
process boundary.
