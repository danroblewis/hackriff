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
#
# T-562: the gate, the acceptance suites and the full workspace test are the COORDINATOR's,
# run once at merge from the main checkout. Measured from /perf: 42 of 47 agents that ran `just
# gate` were implementation agents self-verifying, plus 111 of 122 on `just acceptance` and 93 of
# 108 on full `just test` - about 23 hours of agent time on suites their brief forbids. Prose did
# not stop it, so the rule lives in the runner (the same move as T-396 and T-477).
# HK_ALLOW_FULL=1 lets a genuine repro/debug agent through: a default, not a wall.
_coordinator-only recipe:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "${HK_ALLOW_FULL:-0}" = "1" ]; then exit 0; fi
    root=$(git rev-parse --show-toplevel 2>/dev/null || echo "")
    case "$root" in
      */.claude/worktrees/*)
        echo "REFUSED: 'just {{recipe}}' is the coordinator's, run once at merge from the main checkout." >&2
        echo "You are in an agent worktree: $root" >&2
        echo "" >&2
        echo "Run instead, for what your diff touches:" >&2
        echo "    just test-crate <crate>" >&2
        echo "    cargo nextest run -p <crate> -E 'binary(<name>)'   # one binary" >&2
        echo "NOT 'just test-one': it builds and lists ~280 binaries - over 8 min under load to" >&2
        echo "run one test, against 7.5 s for the scoped form (T-489)." >&2
        echo "" >&2
        echo "Then HAND BACK. A full gate takes 7-25 min, the harness backgrounds it at 600 s, and" >&2
        echo "an agent that waits on it ends its turn stalled with uncommitted work (T-416/426/430)." >&2
        echo "" >&2
        echo "If you genuinely need the full suite to reproduce something: HK_ALLOW_FULL=1 just {{recipe}}" >&2
        exit 1
        ;;
    esac

# THE merge gate (T-396): classify the diff, print the decision, run exactly the suites it needs.
gate *args: (_coordinator-only "gate")
    uv run --locked --project py python -m hkpy.gate {{args}}

# THE COORDINATOR'S per-merge gate (T-424). Run it from inside `git merge --no-ff --no-commit`.
#
# `just gate` and `just gate-merge` answer two different questions, which is why there are two:
#   just gate        "what is in my tree that isn't on main?"   -> an AGENT checking its own work.
#                    Untracked counts: a `newdir/thing.rs` nobody has `git add`ed yet is still
#                    going to be committed, so leaving it out is the one way this fails open.
#   just gate-merge  "what will this merge put on main?"        -> the COORDINATOR, at merge.
#                    git itself built the index from the merge, so the index IS the answer.
#
# Why the narrowing is sound, and it is a narrowing: untracked files are not in the merge index
# and cannot reach main through this merge. The coordinator's checkout permanently holds
# untracked `tools/` (the user's HackRF experiments, never committed) and diagnostic captures
# under `fixtures/`, which forced EVERY merge gate to full for files that can never be merged.
# And classifying untracked paths as full never protected against them anyway: the suites read
# the WORKING TREE, so a stray file changes their result at whatever class was chosen. Fail-closed
# on untracked buys cost, not safety, for this subject.
#
# The guards, so this cannot become a way of certifying a weakening:
#   - it REQUIRES an in-progress merge (MERGE_HEAD, or SQUASH_MSG for a squash). Without one the
#     index is not known to be a merge result, so it forces the FULL gate.
#   - every uncommitted path it did not classify is PRINTED with the class it would have had.
#     No ignore list, nothing silent: `fixtures/` STAGED in a merge is still full.
#
# THE COORDINATOR'S per-merge gate: classify the MERGE INDEX, not the working tree (T-424).
gate-merge *args: (_coordinator-only "gate-merge")
    uv run --locked --project py python -m hkpy.gate --merge {{args}}

# THE SELF-CLEANING BOARD: which in-progress tickets git says are merged, stalled or empty (T-477).
# `in-progress` is a claim about the world and it goes stale silently - a branch lands and the board
# is never flipped, or an agent is lost. Both read identically from the board and differently from
# git. Run it at the START of every tick, before launching anything. --strict exits 1 if anything
# needs attention.
reconcile *args:
    uv run --locked --project py python -m hkpy.reconcile {{args}}

# THE TASK BOARD CLI (user, 2026-09-22): docs/tasks.yaml is edited through THIS, never by hand -
# a PreToolUse hook bans direct Edit/Write/sed-i/redirect edits to it in worktrees. Text-level
# operations on one ticket's block (py/hkpy/tasks.py), never a whole-file YAML re-dump; every
# write is re-read and re-validated, restoring the original bytes on any failure.
#   just task show T-nnn                 print a ticket's raw block
#   just task list [--status S] [--milestone M] [--ready] [--group G] [--json]
#   just task set T-nnn key=value ...     replace/add scalar fields (refuses bad status/blocked)
#   just task result T-nnn (--from|--text)   set the `result:` block
#   just task note T-nnn (--from|--text)     append to the `notes:` block
#   just task new --title T --milestone M [...]   file a ticket, allocating its id
#   just task validate                    strict-parse + the board's own invariants
task *args:
    uv run --locked --project py python -m hkpy.tasks {{args}}

# IS IT SAFE TO LAUNCH ANOTHER BUILDING AGENT (T-559)? CLAUDE.md's worktree-launch cap is "at
# most 4 Rust-building agents" - but a count-the-cargo-processes check misses the `hk serve`
# processes agents leave running (e2e harnesses, demo servers, replay servers), which is exactly
# how a 7-builder day hit load 129-211 and a 62-minute gate. Run this BEFORE launching another
# worktree agent, same as `just reconcile` before launching anything else.
# Prints: cargo/rustc processes grouped by worktree, hk serve/run processes with their bind port
# and worktree, 1-minute load average, free disk (`df -h /`), and a one-line verdict. Counting
# rule: an `hk serve`/`hk run`/`hackriffd` process counts toward the cap ON ITS OWN, whether or
# not anything is compiling in its worktree; the coordinator's own full gate, run from the main
# checkout, groups its cargo/rustc processes into one slot like any other worktree. Read-only -
# it never kills or touches a process. The logic is the pure, tested function `hkpy.builders.assess`
# (`py/tests/test_builders.py`); this recipe is a thin shell over it. `--strict` exits 1 when it
# is not safe to launch another builder.
builders *args:
    uv run --locked --project py python -m hkpy.builders {{args}}
# WHAT THE TICKET CYCLE ACTUALLY COSTS (T-543), from records rather than memory: gate duration
# by class and by phase from $HACKRIFF_OPS/gate-timings.jsonl (which `just gate` writes on every
# run), and branch cut -> first commit -> queued -> merged from ops/merge-runner.log plus git.
# Run it when the loop feels slow; the point is that a slow-down shows up as data before anyone
# has to notice it. First measurement, 2026-09-20: gate median 21.4 min, but QUEUE WAIT median
# 119.7 min and commit->merge median 272.6 min - the gate is ~8 % of a ticket's cycle.
#
# T-763: that 21.4 pools every class (a 17 s `py` gate and a 40 min `full` one in one median) and
# counts only gates that MERGED, so it is not comparable to the full-class number the budget guard
# reports - which is where "60 % slower in two days" came from. `--suites` answers the question
# that matters, per CLASS and per SUITE, from the per-suite lines in merge-runner.log: which half
# of the gate moved. Complete passing runs only; an aborted gate measures a prefix, not the suite.
cycle-time *args:
    uv run --locked --project py python -m hkpy.cycletime {{args}}

# Is the merge suite within its duration budget? Exits 1 if not (T-762). Deliberately NOT part
# of any suite: it reads this machine's recorded history, so no diff can clear it and a merge
# must never hang on it.
budget-check:
    uv run --locked --project py python -m hkpy.cycletime --check-budget

# p50/p90 of the gate, per class and per phase, from $HACKRIFF_OPS/gate-timings.jsonl, plus
# whether any class's ROLLING MEDIAN is over budget. `py/tests/test_gate.py` asserts the same
# budgets, so a slow-down trips a test instead of waiting for someone to notice it.
# The `ops` gate class (py/hkpy/gate.py): orchestration scripts are not linked into any crate, so
# their gate is a syntax check of every script plus the Python suite.
ops-check:
    for f in ops/*.sh .claude/hooks/*.sh; do bash -n "$f" || exit 1; done
    for f in ops/*.py; do python3 -m py_compile "$f" || exit 1; done
    @echo "ops-check: scripts parse"

gate-stats:
    uv run --locked --project py python -m hkpy.cycletime --stats

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
test: (_coordinator-only "test") nextest-config-check test-rust test-doc test-py test-ui

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
    t0=$SECONDS
    # T-492: hk-plugins::host spawns these via CARGO_BIN_EXE_*, resolved at the *test binary's*
    # compile time. `--workspace` does not reliably rebuild them if hk-plugins' own fingerprint
    # is otherwise fresh — the same one line `acceptance` (below) already runs before its
    # hk-e2e tests, for the same reason. A no-op relink on a warm target (measured: ~0.05s).
    # Unconditional: it is cheap, and it is a build the *selected* set may still spawn.
    cargo build -p hk-plugins --bins
    scope=$(just _crate-scope hk-e2e)
    tbuild=$((SECONDS-t0))
    if command -v cargo-nextest >/dev/null 2>&1; then
        cargo nextest run $scope
    else
        echo "test-rust: cargo-nextest not found; falling back to plain 'cargo test' (see just test-seq)" >&2
        cargo test $scope
    fi
    # nextest prints its own `Summary [Ns]`, which covers the RUN only. The difference between
    # that and this is the compile plus the **list** phase — every test binary spawned once with
    # `--list` — which is most of what `just test` used to spend unattributed (R7).
    echo "test-rust: $((SECONDS-t0)) s total, of which $tbuild s before cargo nextest run" >&2

# T-631: a nextest override must be reachable by a nextest run that reads it.
#
# `.config/nextest.toml`'s first override pinned `package(hk-e2e)` into the serial `heavy-serial`
# group, and its header explained at length why hk-e2e is heavy. IT HAD NEVER APPLIED TO A TEST:
# every recipe that ran hk-e2e used plain `cargo test`, which does not read that file, and every
# recipe that used nextest passed `--workspace --exclude hk-e2e`. 125 tests were believed
# serialised for months of commits and were not — a protection everyone reasons about that does
# not exist, which is worse than no protection at all.
#
# So this compares the two files that have to agree: the packages named by override filters, and
# the package scope of the justfile's nextest invocations. It fails when a named package is
# outside every one of them, or names no workspace member (the same defect by typo).
# `just test-crate <crate>` and `just test-one` deliberately do NOT count as evidence — their
# scope comes from whoever types them, and counting them would have made this check pass on the
# very tree that shipped the bug.
#
# It is a member of `just test` rather than only a pytest because it is the cheap one: pure text,
# no build, milliseconds, and it names the file and the filter when it fires.
# Fail if a .config/nextest.toml override names a package no nextest run can ever see (T-631).
# Refuse a tree carrying unresolved merge-conflict markers (T-843). Cheap enough - about a
# second over the whole tree - that it should run for EVERY diff class, including `docs/`, which
# currently runs nothing and is exactly how a half-resolved merge reached main unnoticed.
conflict-check:
    uv run --locked --project py python -m hkpy.conflictmarkers

nextest-config-check:
    uv run --locked --project py python -m hkpy.nextest_config

# The crates whose `src/` holds a code fence rustdoc would actually run — DERIVED, never a
# maintained list (`py/hkpy/doctests.py`). Prints the selection and the skipped crates on stderr,
# and falls back to the whole workspace on any scan failure.
_doctest-scope exclude="":
    @uv run --locked --project py python -m hkpy.doctests {{exclude}}

# nextest doesn't run doctests, so `just test` runs them separately.
#
# **Only over the crates that can have one.** `--workspace --exclude hk-e2e` invoked rustdoc over
# 19 crates to execute 3 doctests (hk-blocks, hk-pipeline::region, hk-recipe); the other 16 print
# `0 passed; 0 failed` and each of those zeroes is still a full `rustdoc --test` of the crate, paid
# on every gate because doctest runs are not fingerprinted. `_doctest-scope` finds the crates by
# **scanning for a runnable doc fence**, so this narrows what is *invoked*, never what is *tested*:
# write a doctest in any crate and that crate is selected again with no edit here. (48 of the 58
# doc fences in this workspace are ```text and 4 more are ```json — rustdoc runs none of them, which
# is why "has a fence" and "has a doctest" are different questions.)
#
# Times itself, because a quarter of `just test` had no owner at all until it was measured
# (docs/test-speed-review-2026-09-22.md §1.2, R7).
test-doc:
    #!/usr/bin/env bash
    set -euo pipefail
    t0=$SECONDS
    scope=$(just _doctest-scope hk-e2e)
    if [ -z "$scope" ]; then
        echo "test-doc: no crate in scope holds a runnable doc fence — nothing to run ($((SECONDS-t0)) s)" >&2
        exit 0
    fi
    cargo test $scope --doc
    echo "test-doc: $((SECONDS-t0)) s" >&2

# T-543. The ONE place `$HK_GATE_CRATES` becomes cargo arguments, printed on stderr so a
# narrowed run always says so. `$1` is a package to drop from the selection (hk-e2e, which
# `just test` has always excluded and which `just acceptance-ci` owns instead).
#
# UNSET MEANS THE WHOLE WORKSPACE, and that is the safety property, not an implementation
# detail: every way this path can go wrong — an old justfile, a `just test-rust` typed by
# hand, a crashed classifier, a shell that dropped the variable — lands on `--workspace`,
# the expensive answer. CORRECTED 2026-09-20: `just gate` only ever sets it when the caller
# passes `--select-crates` (an agent's own opt-in for local iteration), and it is refused
# unconditionally for `just gate-merge` and inside CI regardless of that flag — the merge
# and CI gates always run the whole workspace. When it IS set, `py/hkpy/crates.py` has
# already proved the closure, and returns "whole workspace" whenever it is not certain. Same
# fail-closed shape as the class rule one level up, and the same reason: nothing said is
# never permissive.
_crate-scope exclude="":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "${HK_GATE_CRATES:-}" ]; then
        if [ -n "{{exclude}}" ]; then echo "--workspace --exclude {{exclude}}"; else echo "--workspace"; fi
        exit 0
    fi
    out=""; kept=0
    for p in ${HK_GATE_CRATES}; do
        # `if`, not `[ ... ] && continue`: under `set -e` a false test as the last command
        # of the loop body kills the script, which would silently produce an empty scope.
        if [ "$p" != "{{exclude}}" ]; then
            out="$out -p $p"; kept=$((kept+1))
        fi
    done
    if [ "$kept" -eq 0 ]; then
        # Every selected crate was the excluded one: there is nothing for this suite to run,
        # but "no arguments" would mean the current package, so fall back to the workspace
        # rather than silently running something else.
        if [ -n "{{exclude}}" ]; then echo "--workspace --exclude {{exclude}}"; else echo "--workspace"; fi
        exit 0
    fi
    echo "crate scope (T-543, HK_GATE_CRATES):$out" >&2
    echo "$out"

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

# The `timing` tier (user, 2026-09-22): the throughput tests `.config/nextest.toml` keeps OUT of
# every gate run by `default-filter`, run here and nowhere else - one at a time, on a quiet box or
# nightly, the way HIL (T5) runs. `just gate` never calls this. Not a quarantine: nothing is
# #[ignore]d, and this is the only recipe that runs them, so a red here is a real finding about
# the real-time path's headroom on THIS machine. Log the load average with the result.
timing:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "timing tier: load $(uptime | sed 's/.*load averages*: *//')" >&2
    cargo nextest run --workspace -P timing

# The hk-e2e test targets, split into the three sets the gates are built from and listed by name.
# `cargo test -p hk-e2e` auto-discovers every tests/e2e/tests/*.rs, which is how CI's acceptance job
# silently grew from the M0 slice to all fourteen targets: each new acceptance suite enlisted itself
# with nobody editing the workflow (T-357). Every recipe below names its targets with `--test`, so
# that cannot recur — and `e2e-targets-check` fails when a target on disk is in none of these lists,
# so the opposite drift (a new target quietly running in no gate at all, which is what the harness
# set did locally) cannot recur either. Adding a target means adding it here, deliberately.
e2e_slice := "acceptance_m0"
e2e_harness := "canvas_fidelity concurrent_demod floor_acceptance listen_live mock_device outputs_record refine smoke spectrum_axis stream_external"
e2e_milestones := "acceptance_m2 acceptance_m3 acceptance_m4 acceptance_chirp acceptance_ism acceptance_multipath acceptance_mauto"

# THE ONE PLACE an hk-e2e target set becomes a test command (T-631). Every recipe below calls
# this, so hk-e2e's runner and its parallelism are defined once rather than copied eight times —
# the copies are what let `RUST_TEST_THREADS` reach two recipes and miss the other six.
#
# IT RUNS NEXTEST, which is the whole point. `.config/nextest.toml`'s first override pinned
# `package(hk-e2e)` into a serial group and HAD NEVER APPLIED TO A TEST, because these recipes used
# plain `cargo test` (which never reads that file) and every recipe that did use nextest passed
# `just _crate-scope hk-e2e`, i.e. `--workspace --exclude hk-e2e`. 125 tests were believed
# serialised and were not. Now the config governs them: the `e2e-bounded` group caps hk-e2e at 6
# concurrent tests, the number T-603 MEASURED (the M0 binary failed 2 of 3 at cargo test's default
# 28-way in-binary parallelism and passed 4 of 4 at 6), and `just nextest-config-check` fails if an
# override ever again names a package no nextest run can see.
#
# It is also FASTER than what it replaces. Measured 2026-09-21 over the 82 tests of the acceptance
# gate, all green in every model: `cargo test` at RUST_TEST_THREADS=6 took 390 s; nextest with the
# group at max-threads = 1 took 1133 s; nextest with the group at 6 took 230 s. nextest pools every
# test across the eleven binaries into one queue, while `cargo test` drains the binaries one after
# another — listen_live's single 67 s test used to have the box to itself.
#
# `{{args}}` go to whichever runner is in use, so they are nextest's flags on any machine that has
# it. Note `--no-capture` pins nextest to ONE thread (that is why `acceptance-ci` no longer passes
# it, and why a failure's output is better read from nextest's own per-test capture).
# The `cargo test` path is a fallback for a machine without cargo-nextest and keeps the
# `RUST_TEST_THREADS` cap so it is never the unprotected 28 again.
_e2e-run targets *args:
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v cargo-nextest >/dev/null 2>&1; then
        expr=""
        for t in {{targets}}; do expr="${expr:+$expr + }binary($t)"; done
        exec cargo nextest run -p hk-e2e -E "$expr" {{args}}
    fi
    echo "_e2e-run: cargo-nextest not found; falling back to plain 'cargo test' at RUST_TEST_THREADS=${RUST_TEST_THREADS:-6} (.config/nextest.toml's e2e-bounded group does NOT apply on this path)" >&2
    flags=()
    for t in {{targets}}; do flags+=(--test "$t"); done
    exec env RUST_TEST_THREADS="${RUST_TEST_THREADS:-6}" cargo test -p hk-e2e "${flags[@]}" {{args}}

# M0 slice acceptance suite (T-024, docs/11 §1.1): 7 use cases through the composed pipeline. Missing uv or LFS fixtures fail; only readsb-dependent parts skip. Extra args go to the runner, e.g. `just acceptance --no-capture` (which serialises — see `_e2e-run`).
acceptance *args:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -p hk-plugins --bins
    export HK_E2E_REQUIRE_SYNTH=1 HK_REQUIRE_FIXTURES=1
    just _e2e-run "{{e2e_slice}}" {{args}}

# The hk-e2e harness targets: the composed pipeline, mock device, Listen, recording, streaming,
# refinement and the C-stage floor acceptance, driven through the e2e harness. These are not
# milestone gates — they are the plumbing every task touches — so they run in CI's acceptance gate.
# `just test` excludes hk-e2e wholesale, so until T-357 they were in no recipe at all and CI's
# accreted `cargo test -p hk-e2e` was the only thing running them. Fixtures and the synthetic
# generator are required here, not optional: a gate that skips is a gate that passes for the wrong
# reason (the T-346/T-353 defect). Extra args go to the runner (see `_e2e-run`).
# The hk-e2e harness targets: pipeline, mock device, Listen, recording, streaming, refinement, floor.
e2e-harness *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export HK_E2E_REQUIRE_SYNTH=1 HK_REQUIRE_FIXTURES=1
    just _e2e-run "{{e2e_harness}}" {{args}}

# THE acceptance gate: what CI's acceptance job runs, and one tracked definition rather than a copy
# of it in the workflow (T-353's rule, applied to the opposite sign of drift). The M0 vertical slice
# plus the harness targets, after the census that keeps the target lists honest. Deliberately NOT
# the milestone exit gates — see `acceptance-milestones`. Anyone can run this locally; it is the
# same command CI runs.
# T-631 dropped the `-- --nocapture` it used to pass to both: under nextest that becomes
# `--no-capture`, which pins the run to ONE thread and would undo the measured `e2e-bounded`
# concurrency. nextest captures per test and prints the output of the ones that FAILED, which is
# what the flag was there for and is easier to read than 82 interleaved suites.
acceptance-ci: (_coordinator-only "acceptance-ci") e2e-targets-check acceptance e2e-harness

# The milestone exit gates in one command: M2 attention, M3 classification, M4 trunking, chirp.
# Deliberate, coordinator-run at milestone boundaries — kept out of CI's per-push gate because they
# are exit gates rather than regression checks (M3's was red *by design* for a stretch, which would
# have pinned CI red), because m2/m3 are explicitly kept apart for wall time, and because scene
# simulations with wall-clock dwell budgets already flake under load on a 28-core Mac and would be
# worse on a 2-vCPU runner. Each also has its own recipe for running one alone.
acceptance-milestones: acceptance-m2 acceptance-m3 acceptance-m4 acceptance-chirp acceptance-ism acceptance-multipath acceptance-mauto

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

# M2 attention acceptance suite (T-124): a time-compressed multi-day occupancy scene through the mock SDR under the bandit scheduler (FCO vs hidden truth, busier-than-usual alarm, false alarms, gain step, survey report coverage/POI) plus the recorded bandit vs round-robin simulator comparison. Kept apart from `acceptance` for wall time. Extra args go to the runner (see `_e2e-run`).
acceptance-m2 *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export HK_E2E_REQUIRE_SYNTH=1
    just _e2e-run acceptance_m2 {{args}}

# M3 classification acceptance suite (T-206, the M3 exit gate, ADR-0016 §7): blind accuracy over the full synthetic acceptance grid against the five a-priori floors (top-1, top-2, wrong-label, unknown recall, false-known), reported per family and per SNR bin, plus blind scenes through the mock SDR for classification, signature match and clustering of repeated unknowns. ~1700 classified snippets, so it is kept apart from `acceptance` for wall time. Extra args go to the runner (see `_e2e-run`).
acceptance-m3 *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export HK_E2E_REQUIRE_SYNTH=1
    just _e2e-run acceptance_m3 {{args}}

# M4 (trunking) acceptance: T-267 control-channel hunting through the mock SDR device.
acceptance-m4 *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export HK_E2E_REQUIRE_SYNTH=1
    just _e2e-run acceptance_m4 {{args}}

# MAUTO acceptance (SIGNAL-087, T-545 phase 2 + T-546 phase 3): blind auto-discovery and
# auto-decode of a trunked control channel through the mock SDR — detect blindly, measure the
# symbol structure, auto-select the demod + decode pipeline, and confirm the INVENTORY EMITTER by
# decoding it, at the -9.6 ppm receiver clock error this project measured on its own HackRF. All
# seven tests run; T-545's five red proofs went green with T-546 and their `#[ignore]`s are gone.
acceptance-mauto *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export HK_E2E_REQUIRE_SYNTH=1
    just _e2e-run acceptance_mauto {{args}}

# Chirp acceptance (T-255, CLAUDE.md invariant 1): LoRa up-chirps in 902-928 MHz US ISM through the mock SDR — a signal with a time extent and no stable frequency, against a steady carrier and fixed-frequency bursts as controls. Extra args go to the runner (see `_e2e-run`).
acceptance-chirp *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export HK_E2E_REQUIRE_SYNTH=1
    just _e2e-run acceptance_chirp {{args}}

# Multipath acceptance (T-222, AWARE-053, C40 content half): one 2-FSK transmission received twice - a delayed, attenuated copy on another channel - beside an independent station of the same family, through the mock SDR. Only content separates the pair from the decoy. Extra args go to the runner (see `_e2e-run`).
acceptance-multipath *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export HK_E2E_REQUIRE_SYNTH=1
    just _e2e-run acceptance_multipath {{args}}

# ISM burst acceptance (T-254, CLAUDE.md invariant 1): the 902-928 MHz short-burst playground through the mock SDR and the IQ ring - bounded time extents, one emitter per burst, ephemera catalogued as past events, plus the 100.3 MHz field case. Extra args go to the runner (see `_e2e-run`).
acceptance-ism *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export HK_E2E_REQUIRE_SYNTH=1
    just _e2e-run acceptance_ism {{args}}

# T-364: re-derive both curves of the burst-recall vs open-set trade (docs/17), over N seed bases
# so every figure carries its draw spread (ADR-0016 §7.2). Runs the shipped feature set and the
# four-cyclic-dimension expansion, ~20 min each on the dev Mac. Measurement only: nothing it does
# reaches a default build, and `cyclic-dims` is never on in CI. Extra args go to the binary.
t364-curves *args:
    cargo run --release -p hk-classify --bin t364-curves -- --out /tmp/t364-max.json {{args}}
    cargo run --release -p hk-classify --features cyclic-dims --bin t364-curves -- --out /tmp/t364-four.json {{args}}

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
    # T-543: the gate skips this suite for a diff with no `ui/` path in it, and says so.
    #
    # This is NOT the T-358 defect it superficially resembles. T-358 was a suite that
    # SELF-skipped, silently, when node was missing — green by doing nothing, on a machine
    # nobody was watching. This skip is decided by the RUNNER from the diff, printed by the
    # gate before anything runs, and attributable: same shape as the gate already skipping
    # the Rust suite for a `ui`-only change. It is sound because `ui/` has no generated
    # input — `npm run build` is esbuild over `ui/src`, `typecheck` is `tsc --noEmit` over
    # the same tree, and `npm test` is node over `ui/test`. None of the three reads a Rust
    # artifact, so a `crates/`-only change cannot alter their result.
    #
    # WHAT STILL RUNS, and it is the part that matters: `just test-ui-e2e`, the browser tier,
    # which drives the real `hk serve` and is the ONLY suite that notices when a backend
    # change breaks the page consuming it. It is in the gate's acceptance phase for the
    # `full` class and is not skipped here or anywhere.
    #
    # Unset means RUN, so every failure of this path costs time rather than coverage.
    if [ -n "${HK_GATE_SKIP_UI:-}" ]; then
        echo "test-ui: SKIPPED by the gate — this diff contains no ui/ path, and ui/ has no"
        echo "  generated input, so build+typecheck+node tests over unchanged TypeScript cannot"
        echo "  change their answer. The BROWSER tier (just test-ui-e2e) still runs."
        exit 0
    fi
    if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then
        echo "test-ui: node/npm not found — the UI gate cannot run, so it fails rather than passing." >&2
        echo "  Install Node >= 20 (the dev Mac and CI both run 24), or, to skip the UI deliberately," >&2
        echo "  run the other members of \`just test\` by name: just test-rust test-doc test-py" >&2
        exit 1
    fi
    cd ui
    just _npm-deps
    npm run build
    npm run typecheck
    npm test

# T-543: `npm ci` deletes node_modules and reinstalls it from scratch, every gate, ~30-60 s,
# for a lockfile that almost never changes. Install only when the lockfile actually differs
# from the one the current node_modules was built from.
#
# The stamp is written ONLY after a successful `npm ci`, and it records the lockfile's hash,
# so the three ways this could go wrong all re-install: no stamp (first run, or a wiped
# node_modules), a stamp that does not match (lockfile changed), or a failed install (which
# never writes one). A half-installed tree therefore cannot be mistaken for a good one.
#
# It cds to ui/ itself: `just` runs a recipe from the justfile's directory whatever the
# caller's cwd, so depending on the caller having cd'd would silently read the repo root's
# (non-existent) package-lock.json and reinstall every time.
_npm-deps:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}/ui"
    want=$(shasum -a 256 package-lock.json | cut -d" " -f1)
    stamp=node_modules/.hk-lock-sha256
    if [ -d node_modules ] && [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$want" ]; then
        echo "npm deps: up to date (package-lock.json unchanged since the last npm ci)"
        exit 0
    fi
    npm ci --no-audit --no-fund --prefer-offline
    echo "$want" > "$stamp"

# The BROWSER tier: drive /surface in headless Chrome against a real `hk serve` over a recorded
# fixture, and assert on what is drawn and what is requested (T-455).
#
# Why this exists at all. Two defects in two days passed every other suite:
#   T-450  the renderer COULD NOT LOAD IN A BROWSER. `cellrule.ts` compiled its predicates with
#          `new Function` at module scope and `hk serve` sends `default-src 'self'` with no
#          `unsafe-eval`, so the module threw while being evaluated — while T-441 had proved that
#          same module's shader against a CPU rule on 114973 of 115200 pixels. A correct proof
#          about code that could never run where the product runs.
#   T-454  the tile route's `503` backpressure reaching the user, in a client that already had a
#          cap, an AbortController per request and measured cancellation.
# Neither is reachable by a pixel-rasterizer test or a node unit test: one needs a CSP, the other
# needs real concurrent fetches from a real render loop. That is the gap this tier closes, and it
# closes it at the standard the unit tier already holds — pixel histograms and the requests the
# client actually made, never "it did not throw" (see ui/e2e/README.md).
#
# Why a SEPARATE recipe rather than folding it into `test-ui`: `test-ui` is the fast check every
# `ui/` edit pays (npm ci + esbuild + `tsc --noEmit` + node suites, tens of seconds, no Rust, no
# browser). This one needs the `hk` binary and a Chrome. Conflating them would make the cheap check
# expensive for every typo. It is wired into the gate's ACCEPTANCE phase for both the `ui` and
# `full` classes — an opt-in tier is one people skip, and a tier that does not run is worse than
# none because it looks like coverage.
#
# Dependencies: none new. The CDP driver is `ui/e2e/cdp.mjs` (node 24's built-in WebSocket), and it
# uses whatever Chrome the machine already has — the ms-playwright cache on the dev Mac,
# `google-chrome` on a GitHub runner image. Set CHROME=/path/to/chrome to override.
test-ui-e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then
        echo "test-ui-e2e: node/npm not found — the browser gate cannot run, so it fails rather than passing." >&2
        exit 1
    fi
    # The tier drives the PRODUCT's server, so the CSP and the /api/tiles backpressure under test
    # are the real ones rather than a mock's restatement of them.
    if [ -z "${HK_BIN:-}" ] && [ ! -x target/release/hk ] && [ ! -x target/debug/hk ]; then
        echo "test-ui-e2e: building the hk binary the browser tier serves from..." >&2
        cargo build -p hk-cli --bin hk
    fi
    cd ui
    just _npm-deps
    npm run build
    npm run e2e

# `just test-ui-e2e`'s own non-vacuity check: put each known defect back, rebuild a patched copy of
# ui/src into a scratch dist, and require the suite to go RED — reporting which guard caught it, and
# saying INCONCLUSIVE for any guard that was already failing without the fault. `ui/src` is never
# modified. Not part of any gate: it is how you check the gate still works, ~2 minutes.
test-ui-e2e-selftest:
    cd ui && npm run e2e:selftest

# The per-spec timeout's own non-vacuity check (T-473): run the real `run.mjs` against a
# deliberately hanging spec (`ui/e2e/selftest-fixtures/hang.e2e.mjs`, invisible to every normal run)
# and require it to be killed, reported red by name, and to leave no Chrome or `hk serve` behind.
# ~15 s. Not part of any gate, same as test-ui-e2e-selftest above.
test-ui-e2e-selftest-timeout:
    cd ui && npm run e2e:selftest-timeout

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
    #!/usr/bin/env bash
    set -euo pipefail
    # `cargo fmt` is whole-tree always: it is seconds, and a narrowed format check would be
    # the one place this ticket bought speed with coverage for no measurable gain.
    cargo fmt --all --check
    cargo clippy $(just _crate-scope) --all-targets -- -D warnings

lint-py:
    cd py && uv run --locked ruff check .

# Register the repo-local git merge driver for docs/tasks.yaml (T-582).
#
# `.gitattributes` names the driver and IS committed; the command that implements it lives in
# .git/config and is NOT, so a fresh clone has the attribute pointing at nothing and git fails
# the merge with "custom merge driver hkboard lacks command line". Run this once per clone.
# `ops/merge-runner.sh` also calls it at startup, so the automated path cannot miss it.
#
# It also points git at `.githooks/`, whose `pre-commit` validates docs/tasks.yaml before any
# commit that touches it (T-764). The gate only runs on merges, and the board is committed
# DIRECTLY several times an hour, so the gate cannot be where the board's integrity lives. The
# path is relative, so each worktree uses its own copy of the hook.
setup-git:
    @git config merge.hkboard.name "append-only merge for docs/tasks.yaml (T-582)"
    @git config merge.hkboard.driver "uv run --locked --project py python -m hkpy.boardmerge %O %A %B"
    @git config core.hooksPath .githooks
    @echo "git: merge driver 'hkboard' registered for docs/tasks.yaml; hooks -> .githooks"

# THE CHEAP CHECK TO RUN BEFORE QUEUING A BRANCH — seconds, not a gate.
#
# `lint-rust` is two halves, `cargo fmt --check` AND clippy, and a failure of either reads the
# same in the log: "FAILED just lint". T-574 burned two ~20-minute merge gates on this in one
# night — the first was a genuine clippy::too_many_arguments, the second was pure rustfmt
# whitespace in a test file — and the merge-runner reported both to the coordinator as "tests".
#
# Formatting is never worth a gate cycle. This recipe is the whole-tree fmt check plus clippy
# over the crates a branch actually touched, so it is fast enough to run every time and catches
# the half of `lint` that has no business reaching a gate at all. It is NOT a substitute for the
# gate (the gate stays full — coverage is not negotiable); it is the thing you run before you
# put a branch in the queue.
precheck *crates:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo fmt --all --check
    if [ -n "{{crates}}" ]; then
      cargo clippy $(for c in {{crates}}; do printf -- '-p %s ' "$c"; done) --all-targets -- -D warnings
    else
      cargo clippy $(just _crate-scope) --all-targets -- -D warnings
    fi
    echo "precheck: fmt clean, clippy clean"

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

# Build the web UI + the `hk` binary (with HackRF support) and start the server.
# Autodetects an attached HackRF; then open the http://127.0.0.1:8080/#token=... URL it prints.
# No radio? It tells you how to replay a recording instead. Ctrl-C to stop.
run:
    #!/usr/bin/env bash
    set -euo pipefail
    [ -d ui/node_modules ] || ( cd ui && npm install )
    ( cd ui && npm run build )
    cargo build --release -p hk-cli --bin hk --features hackrf
    BIN=target/release/hk
    if hackrf_info >/dev/null 2>&1; then
        echo ">> HackRF detected - starting live. Open the http://127.0.0.1:8080/#token=... URL printed below."
        exec "$BIN" serve --hackrf --center-hz 100800000 --rate 2400000 --lna 32 --vga 30 --amp --ui-dist ui/dist --bind 127.0.0.1:8080
    else
        echo ">> No HackRF found. Run 'hackrf_info' to check the USB connection."
        echo ">> To try the UI without a radio, run:  just demo"
        exit 1
    fi

# Demo / sample mode: run against a bundled recording — NO HackRF and NO libhackrf needed.
# Great for trying the UI on a machine with no radio. Open the printed .../#token=... URL. Ctrl-C stops it.
demo:
    #!/usr/bin/env bash
    set -euo pipefail
    [ -d ui/node_modules ] || ( cd ui && npm install )
    ( cd ui && npm run build )
    cargo build --release -p hk-cli --bin hk
    FIX=fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta
    DATA="${FIX%.sigmf-meta}.sigmf-data"
    if [ ! -s "$DATA" ] || head -c 80 "$DATA" 2>/dev/null | grep -ql 'git-lfs'; then
        echo ">> The demo recording is a Git LFS pointer, not the real data (cloned without LFS)."
        echo ">> Fetch it once, then re-run 'just demo':"
        echo ">>   git lfs install && git lfs pull"
        exit 1
    fi
    echo ">> Demo (replay) mode - open the http://127.0.0.1:8080/#token=... URL printed below."
    exec target/release/hk serve --replay "$FIX" --loop --ui-dist ui/dist --bind 127.0.0.1:8080
