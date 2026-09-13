# hackriff common commands. `just --list` shows them all.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Build the Rust workspace (CPU path; `gpu` off)
build:
    cargo build --workspace

# All offline tests: Rust (T1-T4, no hardware, `gpu` off) + Python tooling + UI build check (skipped without node)
test: test-rust test-py test-ui

test-rust:
    cargo test --workspace

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
