# Workflow: change a runner rule (pipeline code)

**When:** a fix belongs in `ops/`, `.claude/hooks`, `justfile`, `.config/nextest.toml`, the e2e harness, or `py/hkpy/{gate,boardmerge,gatediag,flakes,crates,flow,experiment,knobs}.py`. **Output:** a branch in the merge queue, tested, with a rollback that is one revert.

## 1. Cut the branch from the right base

```bash
cd /Users/daniellewis/hackriff
base=$(ls $HACKRIFF_OPS/bulk-in-progress >/dev/null 2>&1 \
        && grep -oE -- '--base [0-9a-f]+' $HACKRIFF_OPS/merge-runner.log | tail -1 | cut -d' ' -f2 \
        || git rev-parse main)
git worktree add -B task-<slug> .claude/worktrees/<slug> "$base"
```

While `bulk-in-progress` exists, `main`'s HEAD is the runner's **provisional** batch merge and is rewound if the gate fails; a branch cut from it silently carries every batch merge (2026-09-23: `task-synthfix` did, twice). The gate's `--base` is the truth.

## 2. Make the change small and reversible

One mechanism per branch. Every rule change carries its evidence in a comment (date, the log line, the cost), the way the runners are written. If the change is a default (a knob's new value), it is one line plus the README line.

## 3. Test what the runner will trust

- Shell: `bash -n`, then a **dry classification** of the new rule against live processes/state without acting (the way the cwd-scoped orphan kill was checked: run the loop with `echo` in place of `kill`).
- Python: the module's tests in `py/tests/`, plus a `--once --dry-run` tick of `ops/work-runner.py` against the live `$HACKRIFF_OPS` in both modes if you touched dispatch.
- Hooks: `py/tests/test_hooks.py`.
- Harness: the affected spec alone, then the full 13-file run at `HK_E2E_CONCURRENCY=3` — only when no gate is running (the hook refuses otherwise; use `just wait-for-gate`, never a hand-rolled loop).
- A rule the merge path trusts (classifier, drain, board driver, hold marker) gets a `reviewer` subagent pass before you queue it.

Targeted tests only. Never `just gate`, `just test`, `just acceptance` in a worktree (the hook blocks them, and they would run beside the real gate).

## 4. Commit, queue, watch

Commit message: what was wrong, the evidence, what the rule is now, what you checked. If the message must mention a spec-run phrase the hook watches (`hk serve`, `npm run e2e`), commit from a file (`git commit -F`), not a heredoc on the command line. Then:

```bash
echo task-<slug> >> $HACKRIFF_OPS/merge-queue.txt
```

`ops/` and `justfile` paths fail closed to the **full** gate (~45 min); batch small pipeline changes into one branch rather than paying that per line. Watch with a line-buffered filter (`grep --line-buffered`, no trailing `cut`) for `BULK attempt`, `gate: just`, `BULK MERGED ✓` / `MERGED task-…`, `BULK gate FAILED`, `GATE FAILED`, `TRIAGE`.

## 5. After landing

Restart the script that changed (`restart-an-ops-script.md`) — the runners check their own version at start and say `VERSION: … STALE` until you do. Then the README line, and the ledger line if the change closes an experiment. Remove the worktree (the runner removes `task-*` worktrees it merged; check).

## The tip rule

The runner **refuses to re-gate a tip that already failed** (`merge-attempts.txt`). A fix to a failed branch must move the tip — a new commit, or a rebase onto the current base — and never rebase a branch the runner has already merged into a running batch (add a new branch instead).
