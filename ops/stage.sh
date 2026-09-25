#!/usr/bin/env bash
# Staging watcher for the bears demo (port 8899, tunnel via cloudflared).
# Rebuilds + restarts on every CODE commit to main and smoke-tests it.
# Prefers the live HackRF; falls back to a looping SigMF replay when the device is busy.
set -uo pipefail

REPO=/Users/daniellewis/hackriff
PORT=8899
S="${HACKRIFF_OPS:-$HOME/.hackriff-ops}"; mkdir -p "$S"
FIX=$REPO/fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta
BIN=$S/target-serve/release/hk
# THE BUILD NEVER READS THE LIVE CHECKOUT (2026-09-24 21:20: the 21:15 build compiled a torn tree - the
# merge runner merged two batches into main's working tree mid-build, hk-model from one tree, hk-api from
# another - and the demo served main's ui/dist, which every UI gate rebuilds in place). It builds in its
# own detached worktree checked out at the LANDED commit, and serves a copy of that build's ui/dist.
SRC=$S/stage-src
DIST=$S/stage-dist
DATA=$S/hk-data
mkdir -p "$S"

# token: reuse if present, else generate one
if [ -f "$S/hk-token-bears" ]; then TOKEN=$(cat "$S/hk-token-bears"); else
  TOKEN=$(openssl rand -hex 32); echo "$TOKEN" > "$S/hk-token-bears"; fi

log(){ echo "[$(date '+%m-%d %H:%M:%S')] $*" | tee -a "$S/stage.log"; }
# Never from a worktree (ops/launch-guard.sh): logs PATH:, refuses before any build or server start.
. "$(dirname "${BASH_SOURCE[0]}")/launch-guard.sh"; launch_guard "${BASH_SOURCE[0]}"

build(){  # $1 = a landed commit
  log "build: cargo (release) + ui from a snapshot of $1 ($SRC)"
  [ -d "$SRC" ] || git -C "$REPO" worktree add -q --detach "$SRC" "$1" > "$S/stage-build.log" 2>&1 \
    || { log "BUILD FAILED: cannot create $SRC (see stage-build.log)"; return 1; }
  git -C "$SRC" checkout -q --force --detach "$1" > "$S/stage-build.log" 2>&1 \
    || { log "BUILD FAILED: cannot check out $1 in $SRC"; return 1; }
  # node_modules: a clone of main's the first time; npm ci whenever the lockfile differs from the one installed.
  [ -d "$SRC/ui/node_modules" ] || cp -c -R -p "$REPO/ui/node_modules" "$SRC/ui/node_modules"
  if ! cmp -s "$SRC/ui/package-lock.json" "$SRC/ui/node_modules/.stage-lock"; then
    ( cd "$SRC/ui" && npm ci --prefer-offline --no-audit --no-fund ) >> "$S/stage-build.log" 2>&1 \
      && cp "$SRC/ui/package-lock.json" "$SRC/ui/node_modules/.stage-lock"
  fi
  if ( cd "$SRC" && CARGO_TARGET_DIR="$S/target-serve" \
       cargo build --release -p hk-cli --bin hk --features hackrf ) >> "$S/stage-build.log" 2>&1 \
     && ( cd "$SRC/ui" && npm run build ) >> "$S/stage-build.log" 2>&1 \
     && mkdir -p "$DIST" && rsync -a --delete "$SRC/ui/dist/" "$DIST/"; then
    echo "$1" > "$S/hk-serve-built-commit"; return 0
  fi
  log "BUILD FAILED (see stage-build.log)"; return 1
}

stop_server(){
  pkill -f "hk serve --bind 127.0.0.1:$PORT" 2>/dev/null
  for _ in $(seq 1 20); do pgrep -f "hk serve --bind 127.0.0.1:$PORT" >/dev/null || break; sleep 0.5; done
  pkill -9 -f "hk serve --bind 127.0.0.1:$PORT" 2>/dev/null; sleep 1
}

start_replay(){
  HK_TOKEN=$TOKEN nohup "$BIN" serve --bind 127.0.0.1:$PORT --ui-dist "$DIST" \
    --data-dir "$DATA" --replay "$FIX" --loop > "$S/hk-serve-bears.log" 2>&1 &
  echo "source: replay" > "$S/hk-serve-source"
}

start_server(){
  rm -rf "$DATA"; mkdir -p "$DATA"   # fresh data dir avoids stale-lock startup hangs
  if hackrf_info >/dev/null 2>&1; then
    HK_TOKEN=$TOKEN nohup "$BIN" serve --bind 127.0.0.1:$PORT --ui-dist "$DIST" \
      --data-dir "$DATA" --hackrf --center-hz 100800000 --rate 2400000 --lna 32 --vga 30 --amp \
      --iq-retention 30m --iq-buffer-max 9GiB \
      > "$S/hk-serve-bears.log" 2>&1 &
    echo "source: live" > "$S/hk-serve-source"
    for _ in $(seq 1 25); do
      grep -q "listening on" "$S/hk-serve-bears.log" && { log "started (live)"; return; }
      if grep -q "Access denied\|receive failed" "$S/hk-serve-bears.log"; then
        log "hackrf busy -> replay"; stop_server; start_replay; log "started (replay)"; return; fi
      sleep 1
    done
    log "live start slow -> replay"; stop_server; start_replay; log "started (replay, live timed out)"
  else
    log "hackrf busy at start -> replay"; start_replay; log "started (replay)"
  fi
}

smoke(){
  sleep 3
  local r sc ws
  r=$(curl -s -o /dev/null -w '%{http_code}' -m6 "http://127.0.0.1:$PORT/")
  [ "$r" = 200 ] || { log "SMOKE FAIL root=$r"; return 1; }
  sc=$(curl -s -o /dev/null -w '%{http_code}' -m6 -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT/api/streams")
  [ "$sc" = 200 ] || { log "SMOKE FAIL streams=$sc"; return 1; }
  ws=$(curl -s -o /dev/null -w '%{http_code}' --http1.1 -m6 -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
       -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
       "http://127.0.0.1:$PORT/ws/spectrum/live?token=$TOKEN")
  [ "$ws" = 101 ] || { log "SMOKE FAIL spectrum-ws=$ws"; return 1; }
  log "SMOKE OK (root 200, streams 200, ws 101)"; echo "ok" > "$S/stage-smoke"; return 0
}

# T-530: 101 is the healthy answer; 503 is the server telling us it is alive and between windows
# (`replumbing`) or at its consumer cap. Neither is a crashed server, and restarting on one takes
# the demo down under the user. A real end says 410 and a dead server says nothing at all, so both
# still fail here. The server waits ~2 s for the re-plumb before it answers 503 at all, so in
# practice this arm is reached only when a re-plumb is genuinely stuck.
healthy(){
  curl -s -o /dev/null -m5 "http://127.0.0.1:$PORT/" || return 1
  local ws
  ws=$(curl -s -o /dev/null -w '%{http_code}' --http1.1 -m8 -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
       -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
       "http://127.0.0.1:$PORT/ws/spectrum/live?token=$TOKEN")
  [ "$ws" = 101 ] || [ "$ws" = 503 ]
}

ensure_tunnel(){
  pgrep -f "cloudflared tunnel --url http://127.0.0.1:$PORT" >/dev/null && return
  log "tunnel down -> restarting cloudflared"
  nohup cloudflared tunnel --url "http://127.0.0.1:$PORT" > "$S/cf-hk.log" 2>&1 &
}

code_changed(){  # $1=old $2=new ; empty old => yes
  [ -z "$1" ] && return 0
  git -C "$REPO" diff --name-only "$1" "$2" 2>/dev/null \
    | grep -Eq '^(crates/|ui/(src|package))' && return 0
  return 1
}

LAST=$(cat "$S/hk-serve-built-commit" 2>/dev/null || true)
log "=== staging watcher up (port $PORT) ==="
while true; do
  ensure_tunnel
  HEAD=$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null)
  # Read between two looks at the marker: a batch that began while HEAD was read has provisional HEAD.
  # A bulk batch commits its merges to main BEFORE its gate runs and rewinds them if it fails
  # ($S/bulk-in-progress marks that window). Building from that main puts un-landed code on the
  # demo (2026-09-23 00:48: built 4338b62b 25 minutes before its gate passed). Wait it out; the
  # landed HEAD is picked up on the next tick.
  IN_BULK=0; [ -e "$S/bulk-in-progress" ] && IN_BULK=1
  [ "$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null)" = "$HEAD" ] || IN_BULK=1
  if [ ! -x "$BIN" ]; then
    log "no binary yet -> building $HEAD"; build "$HEAD" && { stop_server; start_server; smoke; }
    LAST=$HEAD
  elif [ "$HEAD" != "$LAST" ] && [ "$IN_BULK" = 0 ]; then
    if code_changed "$LAST" "$HEAD"; then
      log "new code on main: $HEAD (was ${LAST:-none})"
      build "$HEAD" && { stop_server; start_server; smoke; }
    else
      log "commit $HEAD is docs/tasks only — no rebuild"
    fi
    LAST=$HEAD
  elif ! pgrep -f "hk serve --bind 127.0.0.1:$PORT" >/dev/null; then
    log "server not running -> starting"; start_server; smoke
  elif ! healthy; then
    sleep 3
    if ! healthy; then log "unhealthy (root down or live stream gone, HEAD $HEAD) — restarting"; stop_server; start_server; smoke; fi
  fi
  sleep 45
done
