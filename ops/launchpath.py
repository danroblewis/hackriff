"""Ops scripts run from the repo's own `ops/`, never from a worktree (incident, 2026-09-24).

The dashboard was started from the `pm-dashmem` worktree. Its heavy builds re-ran monitor.py by
the running file's path, and when that branch landed the runner removed the worktree, so /flow
answered `FileNotFoundError: .../worktrees/pm-dashmem/ops/monitor.py`. A worktree is temporary
by design: the runner reaps it on merge. So each script logs the path it is running from at
start (beside VERSION:), and refuses to start from under `.claude/worktrees/`. Restart a script
with `/dev-env restart <script>`, which runs REPO/ops/<script>.
ops/launch-guard.sh is the same check for the two bash scripts.
"""

from __future__ import annotations

import os
import sys

WORKTREES = "/.claude/worktrees/"


def check(path: str, log) -> str:
    """Log `PATH: <real path>`. Exit 2 (after saying why) when it lies inside a worktree."""
    here = os.path.realpath(path)
    log(f"PATH: {here}")
    if WORKTREES in here:
        log(f"REFUSED: started from a worktree ({here}). Worktrees are removed when their branch lands; "
            "ops scripts run from the repo only - `/dev-env restart <script>` (ops/README.md)")
        sys.exit(2)
    return here
