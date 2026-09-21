---
description: File or manage a ticket in docs/tasks.yaml. ALWAYS search for an existing ticket first — if one exists, manage it (amend/re-scope/add dependents), never file a duplicate or just report it.
disable-model-invocation: false
allowed-tools: Bash(grep *), Bash(git *), Read, Edit
---

## Managing tickets (docs/tasks.yaml)

**The discipline: an existing ticket gets managed, never just reported.** When a need arises (a user request, a review finding), the first move is to look, not to file.

### 1. Search first
```
grep -niE '<keywords for the concept>' docs/tasks.yaml
```
Also check for a **reverted** ticket + its **REDO** (search the title for `REDO T-`), and for adjacent tickets that already own the area.

### 2. If a matching ticket exists — MANAGE it
Do one of, don't file a duplicate and don't just say "it exists":
- **Amend it** — sharpen the acceptance with the new concrete evidence (e.g. "the signal at 100.45 MHz wiggling ±0.02 MHz must be visibly resolved, matching the pre-canvas waterfall"). A testable, user-grounded definition of done beats an abstract one.
- **Re-scope it** — if the request changed what it should do.
- **Add dependents** — if the new need is a follow-on, file a ticket that `depends_on` it.
- **Drive its unblock chain** — a `todo` behind an in-progress dep still gets pushed: advance the dep, then it. Don't leave a high-priority ticket passive behind a blocker.

### 3. If genuinely new — file it right
- Append an ID (never renumber): `T-<max+1>`. Keep `docs/05` + `use-cases.yaml` in sync if it adds a use case (append an ID, never renumber).
- Set `milestone`, `status: todo`, `priority`, `model`, and — per `docs/06 §3` / `docs/10 §2` — `capabilities` / `hardware_fit` / `accessory` / `fit_flags` / `test_tier`. Use-case IDs are the definition of done.
- **`blocked` requires `blocked_on`** — what specifically unblocks it (a user decision, hardware, or another ticket). No real blocker → `todo`/`deferred`, not `blocked`. Enforced by `py/tests/test_task_board.py`.
- **Quote any value containing a colon** (e.g. a `blocked_on` with "floor: 0" inside) — an unquoted colon breaks the YAML and the task map.

### 4. Don't lose it to the merge churn
tasks.yaml commits can get orphaned by a merge/reset. After filing, confirm the ticket is on `main` (`grep 'id: T-NNN' docs/tasks.yaml` on a clean tree); if a board commit got orphaned, cherry-pick it back rather than re-filing.
