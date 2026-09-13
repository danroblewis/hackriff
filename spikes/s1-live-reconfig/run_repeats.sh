#!/usr/bin/env bash
# Repeatability runs for S1 (after run_all.sh). Sequential only.
# owned: default thread QoS vs --qos (source thread user-interactive).
# fsdr : 100x8 with 2 MiB and 64 KiB chain edge buffers; drop log shows
#        free output space at the previous work() call (0 = backpressure).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
O="$HERE/owned/target/release/s1-owned"
F="$HERE/fsdr/target/release/s1-fsdr"
R="$HERE/results/repeats"
mkdir -p "$R"

run() {
  local name=$1; shift
  "$@" > "$R/$name.txt" 2>&1 || true
  printf '%-26s %s | %s | %s\n' "$name" \
    "$(grep -E '^RESULT' "$R/$name.txt" | awk '{print $3}')" \
    "$(grep -E 'dropped in window' "$R/$name.txt" | sed 's/.*: //')" \
    "$(grep -E '^attach latency' "$R/$name.txt" | sed 's/.*: //')"
}

for i in 1 2 3; do run "owned-200x1-r$i"          "$O" --cycles 200 --dwell-ms 50; done
for i in 1 2 3; do run "owned-200x1-qos-r$i"      "$O" --cycles 200 --dwell-ms 50 --qos; done
for i in 1 2;   do run "owned-100x8-r$i"          "$O" --cycles 100 --parallel 8 --dwell-ms 50; done
for i in 1 2;   do run "fsdr-200x1-r$i"           "$F" --cycles 200 --dwell-ms 50; done
for i in 1 2 3; do run "fsdr-100x8-r$i"           "$F" --cycles 100 --parallel 8 --dwell-ms 50; done
for i in 1 2 3; do run "fsdr-100x8-buf64k-r$i"    "$F" --cycles 100 --parallel 8 --dwell-ms 50 --chain-buf-kib 64; done
