# hackriff common commands. `just --list` shows them all.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Build the Rust workspace (CPU path; `gpu` off)
build:
    cargo build --workspace

# All offline tests: Rust (T1-T4, no hardware, `gpu` off) + Python tooling
test: test-rust test-py

test-rust:
    cargo test --workspace

test-py:
    cd py && uv run --locked pytest

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

# Run a SigMF fixture through the pipeline (for now: parse + summarise the metadata)
replay fixture:
    cargo run -p hk-cli --bin hk -- replay "{{fixture}}"

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
