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
#
#   * ONE `hk serve` for the whole window, started and owned by THIS SCRIPT, not the agent (T-983:
#     each per-target server the agent used to start on its own preallocated its own multi-GB IQ
#     ring that nothing reaped). Its URL/token/data-dir are exported to the agent
#     (EXPLORER_SERVER_URL/_TOKEN/_DATADIR) and written to explorer/server.{url,token,datadir}; the
#     agent retunes it between targets and never starts its own.
#   * A background watcher polls every EXPLORER_WATCH_POLL seconds for a SECOND `hk serve` under the
#     window's tree (a fresh --data-dir, or a second listener on the one already in use) and stops it
#     the moment it's seen, with a window.log line and a red alert, before reaping its ring - EXCLUDING
#     the kept server's own data dir always, however the rogue's claimed --data-dir relates to it (the
#     same dir, or a parent of it), so a rogue can never take the kept server's still-live ring with it.
#   * On every way out this script stops its own `hk serve` (specifically, then by the old
#     pattern-match as a fallback) BEFORE releasing the lock, so staging can reopen the HackRF, and
#     reaps every IQ ring directory left under the window's tree (`iqbuffer`/`iqbuffer-devices` -
#     never history, recordings, a *.db or logs), logging the bytes reclaimed. `--dry-run` prints
#     what a reap would find without starting anything or deleting anything.
#
# Test seams (py/tests/test_explorer_launcher.py): EXPLORER_RADIO (default "just radio"),
# EXPLORER_CLAUDE (default "claude"), EXPLORER_HK (default target-serve/release/hk), EXPLORER_UI_DIST,
# EXPLORER_DATA_DIR, EXPLORER_CENTER_HZ, EXPLORER_RATE_HZ, EXPLORER_TOKEN, EXPLORER_KILL_GRACE (s,
# default 30), EXPLORER_WRAPUP_S (s before the deadline, default 900), EXPLORER_POLL (s, default 5),
# EXPLORER_WATCH_POLL (s, default 10), EXPLORER_SERVER_SETTLE (s, default 1: how long to wait before
# checking the new hk serve is still alive), EXPLORER_PS_OUTPUT (a file of `ps` lines, for the
# rogue-server watcher; tests only - default runs real `ps`), EXPLORER_REAP_PY (path to the reaper
# module, default alongside this script). Nothing here touches the HackRF itself.
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
WATCH_POLL="${EXPLORER_WATCH_POLL:-10}"
PORT="${EXPLORER_PORT:-8897}"
MAX_S=$((8 * 3600))

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PY_REAP="${EXPLORER_REAP_PY:-$SCRIPT_DIR/../py/hkpy/explorer_reap.py}"
ALERT_PY="$SCRIPT_DIR/alert.py"
alert(){ python3 "$ALERT_PY" "$@" >/dev/null 2>&1 || true; }

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
  echo "  would start:   ONE hk serve for the window (127.0.0.1:$PORT), token/url/data-dir exported to the agent"
  echo "  would run:     (cd $REPO && $CLAUDE --agent explorer --model opus --effort high --dangerously-skip-permissions \"<prompt>\")"
  echo "  would watch:   every ${WATCH_POLL}s for a second hk serve under $D and stop it on sight"
  echo "  would reap now (leftover from a prior window, if any):"
  python3 "$PY_REAP" reap --window "$D" --dry-run 2>/dev/null | sed 's/^/    /'
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

# Reap every IQ ring directory left under the window's tree (this window's own server's, and
# anything a rogue left before the watcher or a prior crash caught it) - never history, recordings,
# a *.db or logs, which reap only ever touches iqbuffer/iqbuffer-devices under a data dir (T-983).
reap_rings(){
  local out total
  out="$(python3 "$PY_REAP" reap --window "$D" 2>/dev/null)"
  total="$(printf '%s\n' "$out" | awk -F'\t' '/^TOTAL/{print $2}')"
  if [ -n "$total" ] && [ "$total" != 0 ]; then
    log "reaped IQ ring(s): ${total} bytes freed under $D"
  else
    log "reap: no IQ ring bytes to reclaim under $D"
  fi
}

TIMER=""; WATCHER=""; SERVER_PID=""
cleanup(){
  local rc=$?
  trap - EXIT INT TERM HUP
  if [ -n "$WATCHER" ]; then kill "$WATCHER" 2>/dev/null; fi
  # Only while it is still our child: after the window-end path it may have exited, been reaped,
  # and its pid gone to a stranger.
  if [ -n "$TIMER" ] && [ "$(ps -o ppid= -p "$TIMER" 2>/dev/null | tr -d ' ')" = "$$" ]; then
    # STOP, collect, KILL - neither can be caught, ignored or deferred. A timer that outlives this
    # shell sleeps on for the whole window holding the pane's stdout (five were found leaked,
    # PPID 1 in `sleep 3599`, 2026-09-25), and at its deadline would pgrep -P a reused pid.
    # Stopped, it cannot fork a sleep we would miss.
    kill -STOP "$TIMER" 2>/dev/null
    local tk; tk="$(tree "$TIMER")"
    # shellcheck disable=SC2086
    kill -KILL "$TIMER" $tk 2>/dev/null
  fi
  # Whatever the agent left behind: its descendants - never its `hk serve`, which is this script's,
  # not the agent's, and is stopped below by pid then by the old pattern-match as a fallback.
  local left; left="$(tree $$)"
  # shellcheck disable=SC2086
  [ -n "$left" ] && kill -TERM $left 2>/dev/null
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill -TERM "$SERVER_PID" 2>/dev/null
    for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$SERVER_PID" 2>/dev/null || break; sleep 1; done
    kill -KILL "$SERVER_PID" 2>/dev/null
    log "stopped the window's hk serve (pid $SERVER_PID)"
  fi
  if pkill -f "hk serve.*127\.0\.0\.1:$PORT" 2>/dev/null; then
    log "stopped a leftover hk serve on :$PORT"
    for _ in 1 2 3 4 5 6 7 8 9 10; do pgrep -f "hk serve.*127\.0\.0\.1:$PORT" >/dev/null || break; sleep 1; done
    pkill -9 -f "hk serve.*127\.0\.0\.1:$PORT" 2>/dev/null
  fi
  reap_rings
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

# The window's ONE hk serve, started here, not by the agent (T-983 - see the header comment). Its
# URL/token/data-dir are exported below and written to $D/server.* so the agent can retune it
# instead of starting its own.
HK_BIN="${EXPLORER_HK:-$S/target-serve/release/hk}"
UI_DIST="${EXPLORER_UI_DIST:-$S/stage-dist}"
DATADIR="${EXPLORER_DATA_DIR:-$D/data}"
CENTER="${EXPLORER_CENTER_HZ:-98000000}"
RATE="${EXPLORER_RATE_HZ:-2400000}"
TOKEN="${EXPLORER_TOKEN:-$(openssl rand -hex 16)}"
mkdir -p "$DATADIR"
HK_TOKEN="$TOKEN" nohup "$HK_BIN" serve --hackrf --bind "127.0.0.1:$PORT" --ui-dist "$UI_DIST" \
  --data-dir "$DATADIR" --center-hz "$CENTER" --rate "$RATE" --lna 32 --vga 30 --amp \
  > "$D/hk-serve.log" 2>&1 &
SERVER_PID=$!
SERVER_URL="http://127.0.0.1:$PORT"
echo "$SERVER_PID" > "$D/server.pid"
echo "$SERVER_URL" > "$D/server.url"
printf '%s' "$TOKEN" > "$D/server.token"; chmod 600 "$D/server.token"
echo "$DATADIR" > "$D/server.datadir"
sleep "${EXPLORER_SERVER_SETTLE:-1}"
if kill -0 "$SERVER_PID" 2>/dev/null; then
  log "started the window's one hk serve (pid $SERVER_PID) at $SERVER_URL, data-dir $DATADIR"
else
  log "the window's hk serve (pid $SERVER_PID) did not stay up - check $D/hk-serve.log"
fi

# Every EXPLORER_WATCH_POLL seconds: a second hk serve under this window's tree - a fresh --data-dir
# the agent started despite the rule, or a second listener on the one already in use - is stopped the
# moment it's seen, its ring reaped, and an alert raised. The agent is never trusted to comply on its
# own (the 2026-09-25 06:43 amendment: it was told at launch and started one anyway).
watch_rogues(){
  local rogue_args rogue_out rpid rdd rplan rtotal
  while :; do
    sleep "$WATCH_POLL"
    rogue_args=(rogue --window "$D" --keep-pid "$SERVER_PID")
    [ -n "${EXPLORER_PS_OUTPUT:-}" ] && rogue_args+=(--ps-output "$EXPLORER_PS_OUTPUT")
    rogue_out="$(python3 "$PY_REAP" "${rogue_args[@]}" 2>/dev/null)"
    [ -z "$rogue_out" ] && continue
    while IFS=$'\t' read -r rpid rdd; do
      [ -z "$rpid" ] && continue
      log "ALERT: rogue hk serve detected (pid $rpid, data-dir $rdd) - the window runs ONE server; stopping it"
      kill -TERM "$rpid" 2>/dev/null
      for _ in $(seq 1 "$GRACE"); do kill -0 "$rpid" 2>/dev/null || break; sleep 1; done
      kill -KILL "$rpid" 2>/dev/null
      # --exclude "$DATADIR": a rogue's --data-dir is its own claim, not something to trust - one
      # that names the kept server's data dir (or a parent of it) must never cost the kept
      # server's still-live ring (T-983 fix round 2). Only a genuinely separate ring is ever
      # removed here; the window-end reap_rings() still takes the kept ring once it too is stopped.
      rplan="$(python3 "$PY_REAP" reap --window "$rdd" --exclude "$DATADIR" 2>/dev/null)"
      rtotal="$(printf '%s\n' "$rplan" | awk -F'\t' '/^TOTAL/{print $2}')"
      log "rogue server's ring reaped: ${rtotal:-0} bytes freed from $rdd (the kept server's own ring at $DATADIR is never touched here)"
      alert red "explorer: rogue hk serve stopped" "pid $rpid data-dir $rdd (window $D, kept pid $SERVER_PID)" --key "explorer:rogue-$rpid"
    done <<< "$rogue_out"
  done
}
watch_rogues &
WATCHER=$!

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
  # Never in this set: SERVER_PID/WATCHER - both children of $PARENT too, now that this script owns
  # the server (T-983) - so the EXIT trap's cleanup() stops them itself, with its own log lines,
  # rather than a redundant, unlogged kill racing it here.
  agent(){
    local k
    for k in $(pgrep -P "$PARENT" | grep -vx "$ME" | grep -vx "${SERVER_PID:-x}" | grep -vx "${WATCHER:-x}"); do
      echo "$k"; tree "$k"
    done
  }
  # shellcheck disable=SC2046
  kill -TERM $(agent) 2>/dev/null
  for _ in $(seq 1 "$GRACE"); do [ -z "$(agent)" ] && break; sleep 1; done
  # shellcheck disable=SC2046
  kill -KILL $(agent) 2>/dev/null
) &
TIMER=$!

PROMPT="Start your explorer window now. Deadline: $DEADLINE_HUMAN (EXPLORER_DEADLINE=$DEADLINE). The launcher has taken the radio lock as owner 'explorer' and staging is already on replay; confirm with 'just radio status' (do NOT take it again - a re-take is refused). The launcher has already started your one hk serve at $SERVER_URL (data-dir $DATADIR) - its token is in EXPLORER_SERVER_TOKEN; retune it between targets through the app's control API, never start your own (a second one is detected and stopped within ${WATCH_POLL}s). Journal: $JOURNAL. Work the first-window targets in order, and release the radio before you exit (leave the hk serve for the launcher to stop)."
CMD=("$CLAUDE" --agent explorer --model opus --effort high --dangerously-skip-permissions)
[ -n "$SID" ] && CMD+=(--session-id "$SID")
CMD+=("$PROMPT")

cd "$REPO" || exit 1
EXPLORER_DEADLINE="$DEADLINE" EXPLORER_DEADLINE_HUMAN="$DEADLINE_HUMAN" EXPLORER_PORT="$PORT" \
  EXPLORER_SERVER_URL="$SERVER_URL" EXPLORER_SERVER_TOKEN="$TOKEN" EXPLORER_SERVER_DATADIR="$DATADIR" \
  EXPLORER_SERVER_PID="$SERVER_PID" "${CMD[@]}"
RC=$?
log "agent exited ($RC)"
exit "$RC"
