# hackriff common commands. `just --list` shows them all.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Build the Rust workspace (CPU path; `gpu` off)
build:
    cargo build --workspace

# All offline tests: Rust (T1-T4, no hardware, `gpu` off) + Python tooling + UI build check (skipped without node).
# Runs via cargo-nextest for parallelism: heavy/timing-sensitive tests (tests/e2e, hk-pipeline
# listen/retune/lossless/refine/stream tests, hk-api, hk-cli, hk-core ring stress/concurrency, hk-demod::refine_wfm_real)
# are pinned to the serial `heavy-serial` test group in .config/nextest.toml; everything else runs
# fully parallel. hk-e2e (harness + M0 acceptance suite) is excluded here, same as CI's `test` job
# — run it with `just acceptance`. Falls back to plain `cargo test` if nextest isn't installed.
# See `just test-seq` for a fully sequential run, and `just test-crate`/`just test-one` to run a
# single crate or test (the T1-T4 subset an agent working on one crate should use, not full `test`).
test: test-rust test-doc test-py test-ui

test-rust:
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v cargo-nextest >/dev/null 2>&1; then
        cargo nextest run --workspace --exclude hk-e2e
    else
        echo "test-rust: cargo-nextest not found; falling back to plain 'cargo test' (see just test-seq)" >&2
        HK_IQ_BUFFER_MAX=16MiB cargo test --workspace --exclude hk-e2e
    fi

# nextest doesn't run doctests, so `just test` runs them separately.
test-doc:
    cargo test --workspace --exclude hk-e2e --doc

# Fully sequential fallback (no nextest, no parallelism, no serial groups needed): matches
# pre-T-077 behaviour, for bisecting a nextest-only failure or when nextest isn't installed.
test-seq:
    HK_IQ_BUFFER_MAX=16MiB cargo test --workspace --exclude hk-e2e
    cargo test --workspace --exclude hk-e2e --doc

# Targeted run for one crate, e.g. `just test-crate hk-pipeline`. What an agent working on a
# single crate should run instead of the full `just test` — the coordinator runs the full
# suite once per merge.
test-crate crate:
    cargo nextest run -p {{crate}}

# Targeted run for one test by (substring) test-function name or test-file/binary name,
# e.g. `just test-one replumbing_is_503` or `just test-one listen_lifecycle`.
test-one name:
    cargo nextest run -E 'test({{name}}) or binary({{name}})'

# M0 slice acceptance suite (T-024, docs/11 §1.1): 7 use cases through the composed pipeline. Missing uv or LFS fixtures fail; only readsb-dependent parts skip. Extra args go to cargo test, e.g. `just acceptance -- --nocapture`
acceptance *args:
    cargo build -p hk-plugins --bins
    HK_IQ_BUFFER_MAX=16MiB HK_E2E_REQUIRE_SYNTH=1 HK_REQUIRE_FIXTURES=1 cargo test -p hk-e2e --test acceptance_m0 {{args}}

test-py:
    cd py && uv run --locked pytest

# Build the web UI into ui/dist (needs Node >= 20)
ui-build:
    cd ui && npm ci --no-audit --no-fund && npm run build

# UI build + type-check; skipped cleanly when node/npm are absent
test-ui:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then
        echo "test-ui: node/npm not found; skipping the UI build check"
        exit 0
    fi
    cd ui
    npm ci --no-audit --no-fund --prefer-offline
    npm run build
    npm run typecheck
    npm test

# Serve the web UI over a replayed recording, e.g. `just serve fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta --loop`
serve fixture *args:
    cargo run -p hk-cli --bin hk -- serve --replay "{{fixture}}" {{args}}

fmt:
    cargo fmt --all

# Formatting check + clippy with warnings as errors
lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

# Generate a synthetic IQ scenario, e.g. `just synth fsk_burst_train --seed 1 --out /tmp/fsk --param snr_db=12`
synth *args:
    uv run --locked --project py python -m hkpy.synth {{args}}

# Verify committed fixtures against fixtures/manifest.json (`--external` also checks the store)
fixtures-verify *args:
    uv run --locked --project py python py/fixtures/verify.py {{args}}

# Copy/verify external originals from $HACKRIFF_FIXTURE_STORE (or --from DIR) into fixtures/store
fixtures-fetch *args:
    uv run --locked --project py python py/fixtures/fetch.py {{args}}

# Regenerate the 2026-09-13 HackRF fixture set from the external store
fixtures-build-2026-09-13 *args:
    uv run --locked --project py python py/fixtures/build_2026_09_13.py {{args}}

# Run a SigMF fixture once through the whole pipeline and print the run summary, e.g. `just replay fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta --data-dir /tmp/hk`
replay fixture *args:
    cargo run -p hk-cli --bin hk -- replay "{{fixture}}" {{args}}

# Sync the tree to $JETSON_HOST:~/hackriff and build on-device with CUDA kernels
deploy-jetson:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "${JETSON_HOST:-}" ]; then
        echo "error: JETSON_HOST is not set, e.g. JETSON_HOST=user@jetson.local just deploy-jetson" >&2
        exit 1
    fi
    rsync -az --delete \
        --exclude target/ --exclude fixtures/store/ \
        --exclude .git --exclude .venv/ --exclude node_modules/ \
        ./ "$JETSON_HOST:~/hackriff/"
    ssh "$JETSON_HOST" 'source "$HOME/.cargo/env" 2>/dev/null || true; cd ~/hackriff && cargo build --release --features hk-dsp/gpu'
