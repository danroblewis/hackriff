#!/usr/bin/env bash
# fuzz-rig.sh — long-running backend reliability fuzzer for hackriff.
#
# Runs hk serve against the MOCK SDR (a looping recording behind the real device interface, so no
# HackRF and no USB contention — it restarts cleanly and coexists with the demo), RESTARTS it on
# any crash, records each crash (time + the workload mode/params live at the time + the log tail),
# and drives rotating stress workloads:
#   sweep      — a full/partial spectrum scan (varied ranges + dwell): exercises scan plan + retune
#   hop        — rapid start/stop narrow scans (retune-scheduling churn)
#   tile-flood — GET /api/tiles with fuzzed INDEPENDENT (level_f,level_t), indices, cells, in
#                concurrent bursts — a huge viewport = a ton of tiles (the "UI tiles crash it" case),
#                including out-of-range indices
#   idle       — leave it completely alone for a while (does it crash on its own?)
# Crashes are attributed to whatever mode was live when the server died. No AI needed to run.
# NOTE: device-SPECIFIC crashes (a real-HackRF retune to an edge freq) need the live radio and are
# tracked separately; this rig hunts the device-agnostic + reliability crashes on the mock.
#
# Watch:  tail -f $HACKRIFF_OPS/fuzz-crashes.log   ·   tail -f $HACKRIFF_OPS/fuzz-workload.log
# Stop:   pkill -f fuzz-rig.sh ; pkill -f 'hk serve .*--bind 127.0.0.1:8850'
set -uo pipefail
REPO=/Users/daniellewis/hackriff
S="${HACKRIFF_OPS:-$HOME/.hackriff-ops}"; mkdir -p "$S"
PORT="${FUZZ_PORT:-8850}"
BIN="$S/target-serve/release/hk"
FIX="$REPO/fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta"
TOK=$(openssl rand -hex 24)
DATA="$S/fuzz-data"; SLOG="$S/fuzz-server.log"; CRASHES="$S/fuzz-crashes.log"
WLOG="$S/fuzz-workload.log"; MODEFILE="$S/fuzz-mode"
: > "$WLOG"; : > "$MODEFILE"
wlog(){ echo "[$(date '+%m-%d %H:%M:%S')] $*" | tee -a "$WLOG"; }
set_mode(){ echo "$1" > "$MODEFILE"; wlog "MODE: $1"; }
server_up(){ pgrep -f "hk serve .*--bind 127.0.0.1:$PORT" >/dev/null 2>&1; }
POST(){ curl -s -m 20 -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' -X POST "$@"; }
GET(){  curl -s -m 20 -o /dev/null -H "Authorization: Bearer $TOK" "$@"; }

start_server(){
  rm -rf "$DATA"; mkdir -p "$DATA"
  HK_TOKEN=$TOK nohup "$BIN" serve --device "mock:$FIX" \
    --ui-dist "$REPO/ui/dist" --data-dir "$DATA" --bind 127.0.0.1:"$PORT" \
    --iq-retention 20m --iq-buffer-max 4GiB > "$SLOG" 2>&1 &
}

# Sole (re)starter. First launch is not a crash; every later down IS and is recorded with the mode
# live at the time. Boot grace so startup isn't re-detected as a crash.
monitor(){
  local n=0
  while true; do
    if ! server_up; then
      if [ "$n" -gt 0 ]; then
        { echo "===================================================================="
          echo "CRASH #$n  $(date '+%F %T')  mode=[$(cat "$MODEFILE" 2>/dev/null)]"
          echo "--- last 30 lines of the crashed server log ---"; tail -30 "$SLOG" 2>/dev/null; echo; } >> "$CRASHES"
        wlog "!!! SERVER DOWN (crash #$n) during mode [$(cat "$MODEFILE" 2>/dev/null)] — restarting"
      fi
      start_server; n=$((n+1)); sleep 15
    fi
    sleep 4
  done
}

r(){ echo $(( RANDOM % $1 )); }

do_sweep(){
  local RANGES=("1000000 6000000000" "24000000 6000000000" "24000000 1800000000" \
                "100000000 2000000000" "300000000 3000000000" "1000000 100000000")
  local DWELLS=(0.2 0.5 1 2)
  local rg=${RANGES[$(r ${#RANGES[@]})]}; local d=${DWELLS[$(r ${#DWELLS[@]})]}
  set_mode "sweep ${rg% *}..${rg#* } dwell=$d"
  POST "http://127.0.0.1:$PORT/api/control/scan" -d "{\"f_lo_hz\":${rg% *},\"f_hi_hz\":${rg#* },\"dwell_s\":$d}" >/dev/null
  sleep $(( 90 + RANDOM % 210 ))
  POST "http://127.0.0.1:$PORT/api/control/scan/stop" >/dev/null
}
do_hop(){
  set_mode "hop"; local i c lo hi
  for i in $(seq 1 60); do
    server_up || break
    c=$(( (RANDOM % 5975) + 25 )); lo=$(( (c-2)*1000000 )); [ "$lo" -lt 1000000 ] && lo=1000000; hi=$(( (c+2)*1000000 ))
    POST "http://127.0.0.1:$PORT/api/control/scan" -d "{\"f_lo_hz\":$lo,\"f_hi_hz\":$hi,\"dwell_s\":0.2}" >/dev/null
    sleep $(( 1 + RANDOM % 3 )); POST "http://127.0.0.1:$PORT/api/control/scan/stop" >/dev/null
  done
}
do_tileflood(){
  set_mode "tile-flood"; local CELLS=(8 32 64 256) i lf lt fi ti cc
  for i in $(seq 1 400); do
    server_up || break
    lf=$(r 10); lt=$(r 10); fi=$(( RANDOM % 8192 )); ti=$(( RANDOM * 64 % 4000000 )); cc=${CELLS[$(r ${#CELLS[@]})]}
    GET "http://127.0.0.1:$PORT/api/tiles?level_f=$lf&level_t=$lt&f_index=$fi&t_index=$ti&cells=$cc" &
    if [ $(( i % 24 )) -eq 0 ]; then wait; fi
  done; wait
}
do_idle(){ set_mode "idle"; sleep $(( 600 + RANDOM % 1200 )); }

# --- boot: coexists with the demo (mock uses no HackRF). Monitor owns all starts/restarts. ---
pkill -f "hk serve .*--bind 127.0.0.1:$PORT" 2>/dev/null; sleep 2
wlog "=== fuzz-rig up (mock, port $PORT); crashes -> $CRASHES ==="
monitor &
for _ in $(seq 1 45); do curl -s -m3 -o /dev/null "http://127.0.0.1:$PORT/" && break; sleep 2; done
wlog "server reachable — starting workload"
while true; do
  if ! server_up; then sleep 6; continue; fi
  case $(r 6) in
    0|1) do_sweep ;;
    2)   do_hop ;;
    3|4) do_tileflood ;;
    5)   do_idle ;;
  esac
done
