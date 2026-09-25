# Sourced by ops/merge-runner.sh and ops/stage.sh (the bash half of ops/launchpath.py): log the
# path this script runs from, and refuse to run from a worktree - the runner removes a worktree
# when its branch lands, taking a running script's files with it (incident, 2026-09-24: the
# dashboard started from pm-dashmem lost /flow when that branch merged). Uses the caller's log().
launch_guard(){
  local here
  here="$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")"
  log "PATH: $here"
  case "$here" in
    */.claude/worktrees/*)
      log "REFUSED: started from a worktree ($here). Worktrees are removed when their branch lands; ops scripts run from the repo only - /dev-env restart <script> (ops/README.md)"
      exit 2 ;;
  esac
}
