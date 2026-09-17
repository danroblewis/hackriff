# hackriff common commands. `just --list` shows them all.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# THE merge gate (T-396). Looks at what actually changed, classifies it, prints the decision,
# and runs exactly the suites that class needs — so "which tests do I run" is a property of the
# runner, not of whichever agent happened to run it. Both CI jobs and the coordinator's
# per-merge check call this one command, which is how a gate stays deterministic.
#
#   ui/ only (not crates/, not docs/api.md)   ->  test-ui
#   crates/ or docs/api.md                    ->  lint + test + acceptance-ci
#   docs/ only                                ->  nothing (this repo has no link checker or
#                                                 markdown linter; the gate says so out loud)
#   py/ only                                  ->  lint-py + test-py
#   ANYTHING ELSE                             ->  the full gate
#
# That last line is the rule that makes the rest safe: classification **fails closed**. A path
# matching no class runs the full gate, never the cheapest — the repo's own principle applied to
# its own tooling (BiasTee::Unknown is not Off; Coverage::Unobserved is not quiet). So `fixtures/`
# (acceptance input the suites read), the justfile and `.github/` (the gate itself — it must not
# be able to certify its own weakening), `tests/`, `plugins/`, `.config/`, `recipes/`, `Cargo.*`
# and every repo-root file are full, and a new top-level directory nobody classified is full too.
# The classifier is a pure function in py/hkpy/gate.py, tested by class in py/tests/test_gate.py.
#
# Default source: the merge base with main (not main itself — a moved main makes unrelated files
# look changed) plus everything uncommitted, including untracked. Override with `--base REF`,
# `--staged`, `--worktree` or `--files a b c`; `--dry-run` prints the decision and runs nothing;
# `--phase check|acceptance` runs half the chosen suites (how CI's two jobs split it).
#
# `just lint` + `just test` + `just acceptance-ci` by hand remain the periodic/milestone check.
gate *args:
    uv run --locked --project py python -m hkpy.gate {{args}}

# Build the Rust workspace (CPU path; `gpu` off)
build:
    cargo build --workspace

# All offline tests: Rust (T1-T4, no hardware, `gpu` off) + Python tooling + the UI gate.
# THE test gate: CI's `test` job runs this recipe, one step, rather than its own copy of the
# commands (T-353's rule; T-358 finished the conversion). So the membership list below is gated too
# — adding `test-foo` here reaches CI with no workflow edit, which is the drift that hid the UI
# check: `test-ui` was in this list and in nothing CI ran. Requires cargo, uv and node; every member
# fails rather than skips when its toolchain is missing.
# Runs via cargo-nextest for parallelism: heavy/timing-sensitive tests (tests/e2e, hk-pipeline
# listen/retune/lossless/refine/stream tests, hk-api, hk-cli, hk-core ring stress/concurrency, hk-demod::refine_wfm_real)
# are pinned to the serial `heavy-serial` test group in .config/nextest.toml; everything else runs
# fully parallel. hk-e2e (harness + acceptance suites) is excluded here, same as CI's `test` job
# — it is `just acceptance-ci`, the acceptance gate CI's other job runs. Falls back to plain
# `cargo test` if nextest isn't installed.
# See `just test-seq` for a fully sequential run, and `just test-crate`/`just test-one` to run a
# single crate or test (the T1-T4 subset an agent working on one crate should use, not full `test`).
test: test-rust test-doc test-py test-ui

# HK_E2E_REQUIRE_SYNTH=1 is set here, not by the caller: three workspace tests outside hk-e2e
# (hk-detect e2e_synth + aware_006_wide_emissions, hk-context aware_006_e2e) skip silently when the
# synthetic generator is unavailable, and CI's workflow used to carry that strictness in a private
# step `env:` block while this recipe did not — the same copy-drifts-from-the-recipe shape T-353
# closed for lint (T-358). Costs nothing locally: `just test` already hard-requires uv via test-py,
# so any machine that passes `just test` today has the generator.
test-rust:
    #!/usr/bin/env bash
    set -euo pipefail
    export HK_E2E_REQUIRE_SYNTH=1
    if command -v cargo-nextest >/dev/null 2>&1; then
        cargo nextest run --workspace --exclude hk-e2e
    else
        echo "test-rust: cargo-nextest not found; falling back to plain 'cargo test' (see just test-seq)" >&2
        cargo test --workspace --exclude hk-e2e
    fi

# nextest doesn't run doctests, so `just test` runs them separately.
test-doc:
    cargo test --workspace --exclude hk-e2e --doc

# Fully sequential fallback (no nextest, no parallelism, no serial groups needed): matches
# pre-T-077 behaviour, for bisecting a nextest-only failure or when nextest isn't installed.
test-seq:
    cargo test --workspace --exclude hk-e2e
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

# The hk-e2e test targets, split into the three sets the gates are built from and listed by name.
# `cargo test -p hk-e2e` auto-discovers every tests/e2e/tests/*.rs, which is how CI's acceptance job
# silently grew from the M0 slice to all fourteen targets: each new acceptance suite enlisted itself
# with nobody editing the workflow (T-357). Every recipe below names its targets with `--test`, so
# that cannot recur — and `e2e-targets-check` fails when a target on disk is in none of these lists,
# so the opposite drift (a new target quietly running in no gate at all, which is what the harness
# set did locally) cannot recur either. Adding a target means adding it here, deliberately.
e2e_slice := "acceptance_m0"
e2e_harness := "concurrent_demod floor_acceptance listen_live mock_device outputs_record refine smoke spectrum_axis stream_external"
e2e_milestones := "acceptance_m2 acceptance_m3 acceptance_m4 acceptance_chirp"

# M0 slice acceptance suite (T-024, docs/11 §1.1): 7 use cases through the composed pipeline. Missing uv or LFS fixtures fail; only readsb-dependent parts skip. Extra args go to cargo test, e.g. `just acceptance -- --nocapture`
acceptance *args:
    cargo build -p hk-plugins --bins
    HK_E2E_REQUIRE_SYNTH=1 HK_REQUIRE_FIXTURES=1 cargo test -p hk-e2e --test {{e2e_slice}} {{args}}

# The hk-e2e harness targets: the composed pipeline, mock device, Listen, recording, streaming,
# refinement and the C-stage floor acceptance, driven through the e2e harness. These are not
# milestone gates — they are the plumbing every task touches — so they run in CI's acceptance gate.
# `just test` excludes hk-e2e wholesale, so until T-357 they were in no recipe at all and CI's
# accreted `cargo test -p hk-e2e` was the only thing running them. Fixtures and the synthetic
# generator are required here, not optional: a gate that skips is a gate that passes for the wrong
# reason (the T-346/T-353 defect). Extra args go to cargo test.
e2e-harness *args:
    #!/usr/bin/env bash
    set -euo pipefail
    flags=()
    for t in {{e2e_harness}}; do flags+=(--test "$t"); done
    HK_E2E_REQUIRE_SYNTH=1 HK_REQUIRE_FIXTURES=1 cargo test -p hk-e2e "${flags[@]}" {{args}}

# THE acceptance gate: what CI's acceptance job runs, and one tracked definition rather than a copy
# of it in the workflow (T-353's rule, applied to the opposite sign of drift). The M0 vertical slice
# plus the harness targets, after the census that keeps the target lists honest. Deliberately NOT
# the milestone exit gates — see `acceptance-milestones`. Anyone can run this locally; it is the
# same command CI runs.
acceptance-ci: e2e-targets-check (acceptance "--" "--nocapture") (e2e-harness "--" "--nocapture")

# The milestone exit gates in one command: M2 attention, M3 classification, M4 trunking, chirp.
# Deliberate, coordinator-run at milestone boundaries — kept out of CI's per-push gate because they
# are exit gates rather than regression checks (M3's was red *by design* for a stretch, which would
# have pinned CI red), because m2/m3 are explicitly kept apart for wall time, and because scene
# simulations with wall-clock dwell budgets already flake under load on a 28-core Mac and would be
# worse on a 2-vCPU runner. Each also has its own recipe for running one alone.
acceptance-milestones: acceptance-m2 acceptance-m3 acceptance-m4 acceptance-chirp

# Census: every hk-e2e target on disk must appear in exactly one of the three lists above, and
# every listed target must exist. This is the guard that makes the explicit `--test` lists safe —
# without it, naming targets would just swap "a new target enlists itself into CI" for "a new target
# runs nowhere". Pure file/list comparison: no build, no tests, runs in milliseconds.
e2e-targets-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    declared=$(printf '%s\n' {{e2e_slice}} {{e2e_harness}} {{e2e_milestones}} | sort)
    dupes=$(printf '%s\n' "$declared" | uniq -d)
    # `cargo test` targets are tests/*.rs plus any tests/*/main.rs directory.
    found=$( (ls tests/e2e/tests/*.rs 2>/dev/null | sed 's|.*/||; s|\.rs$||'; \
              for d in tests/e2e/tests/*/; do \
                  if [ -f "$d/main.rs" ]; then basename "$d"; fi; done) | sort )
    unlisted=$(comm -13 <(printf '%s\n' "$declared" | uniq) <(printf '%s\n' "$found"))
    missing=$(comm -23 <(printf '%s\n' "$declared" | uniq) <(printf '%s\n' "$found"))
    fail=0
    if [ -n "$unlisted" ]; then
        echo "e2e-targets-check: hk-e2e target(s) in no gate list — add to e2e_slice, e2e_harness or e2e_milestones in the justfile:" >&2
        printf '  %s\n' $unlisted >&2
        fail=1
    fi
    if [ -n "$missing" ]; then
        echo "e2e-targets-check: gate list names target(s) that do not exist under tests/e2e/tests:" >&2
        printf '  %s\n' $missing >&2
        fail=1
    fi
    if [ -n "$dupes" ]; then
        echo "e2e-targets-check: target(s) listed in more than one gate list:" >&2
        printf '  %s\n' $dupes >&2
        fail=1
    fi
    [ "$fail" -eq 0 ] || exit 1
    echo "e2e-targets-check: $(printf '%s\n' "$found" | wc -l | tr -d ' ') hk-e2e targets, all accounted for"

# M2 attention acceptance suite (T-124): a time-compressed multi-day occupancy scene through the mock SDR under the bandit scheduler (FCO vs hidden truth, busier-than-usual alarm, false alarms, gain step, survey report coverage/POI) plus the recorded bandit vs round-robin simulator comparison. Kept apart from `acceptance` for wall time. Extra args go to cargo test.
acceptance-m2 *args:
    HK_E2E_REQUIRE_SYNTH=1 cargo test -p hk-e2e --test acceptance_m2 {{args}}

# M3 classification acceptance suite (T-206, the M3 exit gate, ADR-0016 §7): blind accuracy over the full synthetic acceptance grid against the five a-priori floors (top-1, top-2, wrong-label, unknown recall, false-known), reported per family and per SNR bin, plus blind scenes through the mock SDR for classification, signature match and clustering of repeated unknowns. ~1700 classified snippets, so it is kept apart from `acceptance` for wall time. Extra args go to cargo test.
acceptance-m3 *args:
    HK_E2E_REQUIRE_SYNTH=1 cargo test -p hk-e2e --test acceptance_m3 {{args}}

# M4 (trunking) acceptance: T-267 control-channel hunting through the mock SDR device.
acceptance-m4 *args:
    HK_E2E_REQUIRE_SYNTH=1 cargo test -p hk-e2e --test acceptance_m4 {{args}}

# Chirp acceptance (T-255, CLAUDE.md invariant 1): LoRa up-chirps in 902-928 MHz US ISM through the mock SDR — a signal with a time extent and no stable frequency, against a steady carrier and fixed-frequency bursts as controls. Extra args go to cargo test.
acceptance-chirp *args:
    HK_E2E_REQUIRE_SYNTH=1 cargo test -p hk-e2e --test acceptance_chirp {{args}}

test-py:
    cd py && uv run --locked pytest

# Build the web UI into ui/dist (needs Node >= 20)
ui-build:
    cd ui && npm ci --no-audit --no-fund && npm run build

# UI build + type-check + the ui/test suites. FAILS when node/npm are absent — it used to skip.
#
# T-358: this recipe self-skipped, and CI installed no Node and never called it, so the UI gate was
# authoritative nowhere: on a machine without node `just test` went green by doing nothing, and in
# CI it did not run at all. Six tickets (T-334, T-337, T-338, T-340, T-341, T-362) changed ui/src
# behind it. Skip-versus-fail was decided on which failure mode is worse, and a gate that lies green
# is worse than a gate that blocks a machine that cannot run it: the green lie is silent and
# unbounded in time, while the failure is loud, immediate and names its own fix. It also restores
# consistency — `test-rust`, `test-doc` and `test-py` all fail outright without cargo/uv; test-ui
# was the only member of `just test` that degraded to a no-op rather than an error. The escape hatch
# for a machine with no node is explicit rather than implicit: run the sibling recipes by name.
test-ui:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then
        echo "test-ui: node/npm not found — the UI gate cannot run, so it fails rather than passing." >&2
        echo "  Install Node >= 20 (the dev Mac and CI both run 24), or, to skip the UI deliberately," >&2
        echo "  run the other members of \`just test\` by name: just test-rust test-doc test-py" >&2
        exit 1
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

# Formatting check + clippy with warnings as errors, plus Python tooling lint (ruff). Mirrors
# `test`'s test-rust + test-py split: py/ is small and ruff is near-instant (~30ms empty-cache),
# so folding it in here — rather than a separate recipe an agent could forget to run — is what
# keeps a Python lint failure from sitting on main invisible to every gate the way T-271 found one
# (T-346). ruff is pinned in py/'s dev dependency group, so `uv run --locked` supplies it and the
# recipe doesn't depend on a system install (T-352). CI's `test` job runs this recipe rather than
# its own copy of the commands, so this is the one gate definition (T-353) — a check added here
# reaches CI, and weakening it here weakens CI too.
lint: lint-rust lint-py

lint-rust:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

lint-py:
    cd py && uv run --locked ruff check .

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
