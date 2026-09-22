#!/usr/bin/env bash
# T-556 spike: reproducible build of the two wrapped GNU Radio OOTs on macOS (Homebrew GNU Radio).
# Every workaround below cost a failed build to discover; they ARE the "build" finding (README §1).
# Outputs go to $T556_WORK (default ~/.hackriff-ops/work/T-556), never into the repo.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
W=${T556_WORK:-$HOME/.hackriff-ops/work/T-556}
LORA_REV=862746dd1cf635c9c8a4bfbaa2c3a0ec3a5306c9   # tapparelj/gr-lora_sdr HEAD, 2026-09-22
SAT_REV=4210dc45c9725e30d8f612eeec614f3f353cd09c    # daniestevez/gr-satellites HEAD, 2026-09-22
GR_PY=/opt/homebrew/opt/gnuradio/libexec/venv/bin/python  # Homebrew bottles GR in its own venv

# (a) A conda/anaconda on PATH leaks its own fmt 11 into find_package(fmt); the OOT then links
#     @rpath/libfmt.11.dylib while GNU Radio links Homebrew fmt 12 -> dlopen failure at import.
export PATH=/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin
# (b) The gnuradio bottle was compiled with pybind11 3.1 (internals ABI v12); Homebrew's installed
#     pybind11 was 3.0.2 (v11). An OOT built against v11 imports but cannot see gr::block
#     ("referenced unknown base type"). Use a private pybind11 matching the bottle.
mkdir -p "$W/src" "$W/prefix"
uv pip install -q --target "$W/pyb11" pybind11==3.1.0
PBD="$W/pyb11/pybind11/share/cmake/pybind11"
# (c) gr-satellites' Python imports construct/requests/websocket-client (documented) and zmq
#     (NOT documented) at `import satellites`. Private target dir; the Homebrew venv is untouched.
uv pip install -q --python "$GR_PY" --target "$W/pydeps" construct requests websocket-client pyzmq

build() { # name url rev [patch]
  local d="$W/src/$1"
  [ -d "$d" ] || git clone -q "$2" "$d"
  git -C "$d" checkout -q "$3"
  if [ -n "${4:-}" ]; then git -C "$d" apply --check "$4" 2>/dev/null && git -C "$d" apply "$4"; fi
  cmake -S "$d" -B "$d/build" -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$W/prefix" \
    -DPYTHON_EXECUTABLE="$GR_PY" -Dfmt_DIR=/opt/homebrew/lib/cmake/fmt -Dpybind11_DIR="$PBD" \
    -DCMAKE_IGNORE_PREFIX_PATH=/opt/homebrew/anaconda3
  cmake --build "$d/build" -j "${CARGO_BUILD_JOBS:-6}"
  cmake --install "$d/build"
}
# (d) gr-lora_sdr drops the input sample offset; the patch carries it to the payload tags (§2.3).
build gr-lora_sdr https://github.com/tapparelj/gr-lora_sdr "$LORA_REV" "$here/patches/lora-sample-offset.patch"
build gr-satellites https://github.com/daniestevez/gr-satellites "$SAT_REV"
echo "installed into $W/prefix; source $here/env.sh to run"
