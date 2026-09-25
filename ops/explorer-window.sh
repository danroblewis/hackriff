#!/usr/bin/env bash
# One explorer WINDOW (T-923, user directive 2026-09-25): take the radio lock, run the explorer agent
# in the foreground, and release the lock at window end, on exit and on crash. `ops/launch.sh explorer`
# runs this as its tmux pane process; run it directly only with --dry-run.
#
#   ops/explorer-window.sh --window 3h [--session-id <uuid>] [--dry-run]
#
# Guarantees:
#   * ONE instance: a live pid in $HACKRIFF_OPS/explorer/window.pid refuses a second (exit 3).
#   * The lock is taken BEFORE the agent starts (`just radio take explorer <window> <why>`); a refusal
#     ends the window with exit 4 and nothing to release.
#   * Once taken, the lock is released by an EXIT trap: normal exit, agent crash (any exit status),
#     INT/TERM/HUP (tmux kill-session), and the window-end timer. Only SIGKILL of this script
#     escapes, and T-922's `until` (= the window end) makes that lock stale for the watchdog.
#   * Window end: at deadline-15 min the timer touches explorer/wrap-up (the agent finishes its
#     entry and exits); at the deadline it TERMs the agent, KILLs it after the grace period, and the
#     trap releases.
#
#   * The agent starts only once `just radio status` shows staging on replay under the explorer's
#     lock (bounded by EXPLORER_STAGING_WAIT, default 150 s; exit 5 and release otherwise).
#   * On the way out it stops the agent's leftover descendants and its `hk serve` on EXPLORER_PORT
#     (default 8897, the port .claude/agents/explorer.md uses) BEFORE releasing, so staging can
#     reopen the HackRF.
#
# Test seams (py/tests/test_explorer_launcher.py): EXPLORER_RADIO (default "just radio"),
# EXPLORER_CLAUDE (default "claude"), EXPLORER_KILL_GRACE (s, default 30), EXPLORER_WRAPUP_S (s before
# the deadline, default 900), EXPLORER_POLL (s, default 5). Nothing here touches the HackRF itself.
set -uo pipefail
REPO="${EXPLORER_REPO:-/Users/daniellewis/hackriff}"
S="${HACKRIFF_OPS:-$HOME/.hackriff-ops}"
D="$S/explorer"
RADIO="${EXPLORER_RADIO:-just radio}"
CLAUDE="${EXPLORER_CLAUDE:-claude}"
GRACE="${EXPLORER_KILL_GRACE:-30}"
WRAPUP_S="${EXPLORER_WRAPUP_S:-900}"
STAGING_WAIT="${EXPLORER_STAGING_WAIT:-150}"
POLL="${EXPLORER_POLL:-5}"
PORT="${EXPLORER_PORT:-8897}"
MAX_S=$((8 * 3600))

WINDOW=3h; DRY=0; SID=""
while [ $# -gt 0 ]; do
  case "$1" in
    --window) [ $# -ge 2 ] || { echo "explorer: --window needs a value (e.g. 3h, 90m)" >&2; exit 2; }; WINDOW="$2"; shift 2 ;;
    --window=*) WINDOW="${1#--window=}"; shift ;;
    --dry-run) DRY=1; shift ;;
    # ops/launch.sh picks the session id and records it in role-session/explorer (the dashboard's role map)
    --session-id) [ $# -ge 2 ] || { echo "explorer: --session-id needs a value" >&2; exit 2; }; SID="$2"; shift 2 ;;
    *) echo "explorer: unknown argument '$1' (usage: ops/explorer-window.sh --window 3h [--session-id <uuid>] [--dry-run])" >&2; exit 2 ;;
  esac
done

# 3h | 90m | 45s | 2h30m | a bare number = minutes (T-922's `just radio take` reading). 1 s .. 8 h.
secs_of(){
  local w="$1" total=0 n u
  [[ "$w" =~ ^[0-9]+$ ]] && { echo $((10#$w * 60)); return 0; }
  [[ "$w" =~ ^([0-9]+[hms])+$ ]] || return 1
  while [[ "$w" =~ ^([0-9]+)([hms])(.*)$ ]]; do
    n=$((10#${BASH_REMATCH[1]})); u="${BASH_REMATCH[2]}"; w="${BASH_REMATCH[3]}"
    case "$u" in h) total=$((total + n * 3600)) ;; m) total=$((total + n * 60)) ;; s) total=$((total + n)) ;; esac
  done
  echo "$total"
}
SECS="$(secs_of "$WINDOW")" || { echo "explorer: bad --window '$WINDOW' (e.g. 3h, 90m, 2h30m)" >&2; exit 2; }
if [ "$SECS" -lt 1 ] || [ "$SECS" -gt "$MAX_S" ]; then
  echo "explorer: --window '$WINDOW' is out of range (1 s .. 8 h)" >&2; exit 2
fi
[ "$(uname -s)" = Darwin ] || { echo "explorer: Mac Studio only (this is $(uname -s))" >&2; exit 2; }

NOW=$(date +%s); DEADLINE=$((NOW + SECS))
DEADLINE_HUMAN="$(date -r "$DEADLINE" '+%Y-%m-%d %H:%M %Z' 2>/dev/null || date -d "@$DEADLINE" '+%Y-%m-%d %H:%M %Z')"
JOURNAL="$D/journal-$(date '+%Y%m%d').md"
WHY="explorer window $WINDOW until $DEADLINE_HUMAN (T-923)"
PIDF="$D/window.pid"

PROMPT="Start your explorer window now. Deadline: $DEADLINE_HUMAN (EXPLORER_DEADLINE=$DEADLINE). The launcher has taken the radio lock as owner 'explorer' and staging is already on replay; confirm with 'just radio status' (do NOT take it again - a re-take is refused). Serve on 127.0.0.1:$PORT. Journal: $JOURNAL. Work the first-window targets in order, and release the radio before you exit."
CMD=("$CLAUDE" --agent explorer --model opus --effort high --dangerously-skip-permissions)
[ -n "$SID" ] && CMD+=(--session-id "$SID")
CMD+=("$PROMPT")

mkdir -p "$D"
log(){ echo "[$(date '+%m-%d %H:%M:%S')] $*" | tee -a "$D/window.log"; }

if [ -f "$PIDF" ]; then
  OLD="$(cat "$PIDF" 2>/dev/null)"
  if [ -n "$OLD" ] && kill -0 "$OLD" 2>/dev/null; then
    echo "explorer: a window is already running (pid $OLD, $PIDF) - one instance only" >&2; exit 3
  fi
fi

if [ "$DRY" = 1 ]; then
  echo "explorer dry-run: window $WINDOW = ${SECS}s, deadline $DEADLINE_HUMAN, wrap-up $((SECS > WRAPUP_S ? SECS - WRAPUP_S : 0))s in"
  echo "  would take:    $RADIO take explorer $WINDOW \"$WHY\""
  echo "  would run:     (cd $REPO && ${CMD[*]:0:${#CMD[@]}-1} \"<prompt>\")"
  echo "  would release: $RADIO release explorer   (EXIT/INT/TERM/HUP trap + window-end timer)"
  echo "  journal:       $JOURNAL"
  exit 0
fi

echo $$ > "$PIDF"
# shellcheck disable=SC2086  # RADIO is a command line ("just radio"), word-split on purpose.
if ! $RADIO take explorer "$WINDOW" "$WHY"; then
  log "radio lock refused - window not started ($($RADIO status 2>&1 | tr '\n' ' '))"
  rm -f "$PIDF"; exit 4
fi
log "window start: $WINDOW until $DEADLINE_HUMAN; radio lock taken"
rm -f "$D/wrap-up"

# Every descendant of the given pids (a TERMed agent must not leave a child holding the radio).
tree(){ local p c; for p in "$@"; do for c in $(pgrep -P "$p"); do echo "$c"; tree "$c"; done; done; }

TIMER=""
cleanup(){
  local rc=$?
  trap - EXIT INT TERM HUP
  if [ -n "$TIMER" ]; then  # the timer, then its sleep (collected first: it is orphaned once the timer dies)
    local tk; tk="$(pgrep -P "$TIMER")"
    kill "$TIMER" 2>/dev/null
    # shellcheck disable=SC2086
    [ -n "$tk" ] && kill $tk 2>/dev/null
  fi
  # Whatever the agent left behind: its descendants, and its own `hk serve` (started with nohup, so
  # no longer in the tree) - the radio is not free until that server has closed the HackRF.
  local left; left="$(tree $$)"
  # shellcheck disable=SC2086
  [ -n "$left" ] && kill -TERM $left 2>/dev/null
  if pkill -f "hk serve.*127\.0\.0\.1:$PORT" 2>/dev/null; then
    log "stopped the explorer's hk serve on :$PORT"
    for _ in 1 2 3 4 5 6 7 8 9 10; do pgrep -f "hk serve.*127\.0\.0\.1:$PORT" >/dev/null || break; sleep 1; done
    pkill -9 -f "hk serve.*127\.0\.0\.1:$PORT" 2>/dev/null
  fi
  # shellcheck disable=SC2086
  if $RADIO release explorer; then log "radio lock released (exit $rc)"
  else log "radio release FAILED (exit $rc) - check 'just radio status'"; fi
  rm -f "$PIDF" "$D/wrap-up"
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# Staging (ops/stage.sh, ticking every ~45 s) holds the HackRF until it sees the lock and switches to
# replay. Start the agent only once `just radio status` says so; a staging that never switches ends
# the window here (exit 5) and the trap releases the lock.
waited=0
# shellcheck disable=SC2086
until $RADIO status 2>/dev/null | grep "staging: replay (radio-lock: explorer" >/dev/null; do  # no -q: pipefail + SIGPIPE
  if [ "$waited" -ge "$STAGING_WAIT" ]; then
    log "staging did not switch to replay within ${STAGING_WAIT}s ($($RADIO status 2>&1 | tr '\n' ' ')) - window not started"
    exit 5
  fi
  sleep "$POLL"; waited=$((waited + POLL))
done
log "staging is on replay after ${waited}s - the HackRF is the explorer's"

PARENT=$$
(
  trap - EXIT INT TERM HUP
  ME="$(sh -c 'echo $PPID')"
  if [ "$SECS" -gt "$WRAPUP_S" ]; then
    sleep $((SECS - WRAPUP_S)); touch "$D/wrap-up"; log "wrap-up: $WRAPUP_S s to the deadline"
    sleep "$WRAPUP_S"
  else
    sleep "$SECS"
  fi
  log "window end: stopping the agent"
  agent(){ local k; for k in $(pgrep -P "$PARENT" | grep -vx "$ME"); do echo "$k"; tree "$k"; done; }
  # shellcheck disable=SC2046
  kill -TERM $(agent) 2>/dev/null
  for _ in $(seq 1 "$GRACE"); do [ -z "$(agent)" ] && break; sleep 1; done
  # shellcheck disable=SC2046
  kill -KILL $(agent) 2>/dev/null
) &
TIMER=$!

cd "$REPO" || exit 1
EXPLORER_DEADLINE="$DEADLINE" EXPLORER_DEADLINE_HUMAN="$DEADLINE_HUMAN" EXPLORER_PORT="$PORT" "${CMD[@]}"
RC=$?
log "agent exited ($RC)"
exit "$RC"
