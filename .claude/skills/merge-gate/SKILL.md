---
description: How the merge gate works — what suites run for a given diff, and the merge-index vs working-tree distinction. Use when running or reasoning about a gate/merge. The coordinator runs it; workers never.
disable-model-invocation: false
allowed-tools: Bash(just *), Bash(git *), Read
---

## The merge gate

**One command: `just gate` (T-396).** It inspects the diff (merge base with `main` + anything uncommitted; `--base REF`/`--staged`/`--worktree`/`--files …` override), **classifies** it, prints the decision + deciding files, and runs exactly what that class needs:
- `ui/` only → `just test-ui`
- anything touching `crates/` or `docs/api.md` → `just lint` + `just test` + `just acceptance-ci`
- `docs/` only → nothing (no link/markdown checker exists — the gate says so)
- `py/` only → `just lint-py` + `just test-py`

`--phase check|acceptance` runs half the chosen suites — how CI's two jobs split one decision.

**Classification fails closed.** A path matching no class runs the **full** gate, never the cheapest — `fixtures/`, the `justfile`, `.github/`, `tests/`, `plugins/`, `.config/`, `recipes/`, `Cargo.*`, repo-root files, any new top-level dir. The classifier is `py/hkpy/gate.py`, tested class-by-class in `py/tests/test_gate.py`.

## Merging: `just gate-merge` (T-424)
Run inside `git merge --no-ff --no-commit`. It classifies **the merge index** (what the merge actually puts on `main`), not the working tree — so main's permanently-untracked `tools/` and diagnostic `fixtures/` don't force every merge to full. It requires `MERGE_HEAD`/`SQUASH_MSG` or forces full, prints every uncommitted path it didn't classify, and has **no ignore list** — `fixtures/`/`tools/` staged into a merge is still full.

## Rules
- **The full suite stays full for merging.** Coverage is not negotiable at the gate. Speed it via faster linking / fewer test binaries / fixing flakes — never by cutting what runs. Affected-crate selection is for the **worker iteration path** (`test-crate`/`test-one`), not the gate.
- **A gate is ~25–30 min** (test-binary compile dominates, ~11 min; test run ~5 min). The harness backgrounds anything over its 600s cap. Don't end a turn waiting on it — block on the `.done`/output file and read the verdict same turn.
- **Batch:** octopus-merge all ready non-conflicting branches, gate once. Bisect the culprit on failure, don't re-gate all individually.
- **A failed gate is not committed.** Abort (`git merge --abort`). Never re-gate an unchanged failing commit.
