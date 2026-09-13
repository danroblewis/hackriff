#!/usr/bin/env bash
# Reproduce spike S1: build both implementations (release) and run every
# measurement sequentially (never in parallel: CPU numbers would interfere).
# Output: results/*.txt next to this script.
#
# Requirements: stable cargo (owned; any Rust >= 1.85) and a nightly toolchain
# for FutureSDR 0.8.0 (uses #![feature(...)]):  rustup toolchain install nightly --profile minimal
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
NIGHTLY="${NIGHTLY:-$HOME/.rustup/toolchains/nightly-aarch64-apple-darwin/bin}"
mkdir -p "$HERE/results"

cargo build --release --manifest-path "$HERE/owned/Cargo.toml"
RUSTC="$NIGHTLY/rustc" "$NIGHTLY/cargo" build --release --manifest-path "$HERE/fsdr/Cargo.toml"

O="$HERE/owned/target/release/s1-owned"
F="$HERE/fsdr/target/release/s1-fsdr"

{
  date
  sw_vers
  sysctl -n machdep.cpu.brand_string hw.ncpu hw.memsize
  echo "owned rustc: $(rustc --version)"
  echo "fsdr rustc : $("$NIGHTLY/rustc" --version)"
} > "$HERE/results/env.txt" 2>&1

run() {
  local name=$1; shift
  echo "### $name: $*"
  "$@" > "$HERE/results/$name.txt" 2>&1 || true
  grep -E "RESULT|attach latency|dropped in window|bench" "$HERE/results/$name.txt" || true
}

run owned-bench          "$O" --bench
run owned-200x1          "$O" --cycles 200 --dwell-ms 50
run owned-100x8          "$O" --cycles 100 --parallel 8 --dwell-ms 50
run owned-100x1-preroll  "$O" --cycles 100 --dwell-ms 50 --preroll 32
run fsdr-bench           "$F" --bench
run fsdr-200x1           "$F" --cycles 200 --dwell-ms 50
run fsdr-100x8           "$F" --cycles 100 --parallel 8 --dwell-ms 50
run fsdr-200x1-buf64k    "$F" --cycles 200 --dwell-ms 50 --chain-buf-kib 64
run fsdr-unpaced         "$F" --unpaced --cycles 0 --run-ms 3000
