#!/bin/sh
# Spike S3 measurement matrix. Runs sequentially (never in parallel: runs would contend for GPU/CPU).
# Usage: sh bench/run-matrix.sh [secs]   (from spikes/s3-web-waterfall/)
set -e
SECS=${1:-60}
cd "$(dirname "$0")/../client"
B="node ../bench/bench.mjs --secs $SECS"
for mode in headless headed; do
  for bins in 4096 16384; do
    for fps in 30 60; do
      $B --mode $mode --bins $bins --fps $fps --dtype u8 --persist gpu
    done
  done
done
# worst-case wire format (f32 dBFS, CPU quantise in JS)
$B --mode headed --bins 16384 --fps 60 --dtype f32 --persist gpu
# GPU cost headroom: gl.finish() inside rAF so workMs includes GPU completion
$B --mode headed --bins 16384 --fps 60 --dtype u8 --persist gpu --finish 1
# CPU-side persistence (JS typed arrays + full R32F upload per rAF) for comparison
$B --mode headed --bins 4096 --fps 30 --dtype u8 --persist cpu
$B --mode headed --bins 16384 --fps 30 --dtype u8 --persist cpu
# crude slow-CPU proxy (CDP CPU throttle 6x; does NOT throttle the GPU)
$B --mode headed --bins 16384 --fps 60 --dtype f32 --persist gpu --throttle 6
# larger canvas (on-device screens are smaller; this is a laptop/monitor remote)
$B --mode headed --bins 16384 --fps 60 --dtype u8 --persist gpu --w 2560 --h 1440
# headroom: 1-px readPixels forces a GPU sync so rAF work ≈ CPU+GPU frame cost
# (gl.finish above turned out to be non-blocking in Chrome)
$B --mode headed --bins 16384 --fps 60 --dtype u8 --persist gpu --finish 2
# headroom: 240 rows/s input = 4 persistence passes per 60 Hz rAF (~1 G texel updates/s)
$B --mode headed --bins 16384 --fps 240 --dtype u8 --persist gpu --finish 2
