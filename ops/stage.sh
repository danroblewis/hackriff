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
DATA=$S/hk-data
mkdir -p "$S"

# token: reuse if present, else generate one
if [ -f "$S/hk-token-bears" ]; then TOKEN=$(cat "$S/hk-token-bears"); else
  TOKEN=$(openssl rand -hex 32); echo "$TOKEN" > "$S/hk-token-bears"; fi

log(){ echo "[$(date '+%m-%d %H:%M:%S')] $*" | tee -a "$S/stage.log"; }

build(){  # $1 = commit
  log "build: cargo (release) + ui"
  if ( cd "$REPO" && CARGO_TARGET_DIR="$S/target-serve" \
       cargo build --release -p hk-cli --bin hk --features hackrf ) > "$S/stage-build.log" 2>&1 \
     && ( cd "$REPO/ui" && npm run build ) >> "$S/stage-build.log" 2>&1; then
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
  HK_TOKEN=$TOKEN nohup "$BIN" serve --bind 127.0.0.1:$PORT --ui-dist "$REPO/ui/dist" \
    --data-dir "$DATA" --replay "$FIX" --loop > "$S/hk-serve-bears.log" 2>&1 &
  echo "source: replay" > "$S/hk-serve-source"
}

start_server(){
  rm -rf "$DATA"; mkdir -p "$DATA"   # fresh data dir avoids stale-lock startup hangs
  if hackrf_info >/dev/null 2>&1; then
    HK_TOKEN=$TOKEN nohup "$BIN" serve --bind 127.0.0.1:$PORT --ui-dist "$REPO/ui/dist" \
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

healthy(){
  curl -s -o /dev/null -m5 "http://127.0.0.1:$PORT/" || return 1
  local ws
  ws=$(curl -s -o /dev/null -w '%{http_code}' --http1.1 -m6 -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
       -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
       "http://127.0.0.1:$PORT/ws/spectrum/live?token=$TOKEN")
  [ "$ws" = 101 ]
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
  if [ ! -x "$BIN" ]; then
    log "no binary yet -> building $HEAD"; build "$HEAD" && { stop_server; start_server; smoke; }
    LAST=$HEAD
  elif [ "$HEAD" != "$LAST" ]; then
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
