# Vendored liquid-dsp (T-607)

| | |
|---|---|
| Upstream | https://github.com/jgaeddert/liquid-dsp |
| Release | **v1.8.2** (2026-08-06), tag commit `03d052b89b543e6d5f9a882139ba19f7683bcd29` |
| Licence | MIT — `liquid-dsp/LICENSE`, unmodified |
| What is here | Exactly the 157 `.c` files liquid's own `CMakeLists.txt` compiles into `libliquid` (listed in `../sources.txt`), plus every file they `#include` (`*.proto.c` templates, SIMD variants, `include/liquid.h`, `include/liquid.internal.h`): 265 files, 2.9 MB. No file is modified. |
| What is not | autotest, bench, examples, sandbox, doc, scripts, the CMake/autotools build, and `cmake/liquid.config.h.in` (`../build.rs` generates the header instead). |
| Not linked, on purpose | **FFTW** (GPLv2; liquid's CMake links it by default if found — Homebrew's `liquid-dsp` formula does) and **libfec** (LGPL-2.1; liquid's only source of convolutional and Reed–Solomon codecs). See ADR-0010's ledger and docs/18 §7.1.1. |

## Refreshing to a new release

1. Clone the new tag. Configure it once with CMake to get its file list:
   `cmake -S . -B b -DBUILD_EXAMPLES=OFF -DBUILD_AUTOTESTS=OFF -DBUILD_BENCHMARKS=OFF -DBUILD_SHARED_LIBS=OFF -DBUILD_STATIC_LIBS=ON -DFIND_FFTW=OFF -DCMAKE_EXPORT_COMPILE_COMMANDS=ON`,
   then take the `file` entries of `b/compile_commands.json`, relative to the source root, into
   `../sources.txt`.
2. Copy those files plus the transitive closure of their `#include "…"`s into `liquid-dsp/`,
   preserving paths, and copy `LICENSE`.
3. Diff `cmake/liquid.config.h.in` against the header `../build.rs` writes; add any new macro.
4. Bump the version assertion in `../tests/coverage.rs` and re-run `just test-crate hk-liquid-sys`.
   Its FEC test must still show **no** convolutional or Reed–Solomon scheme constructing. If one
   does, libfec got linked, and ADR-0010's ledger has to be re-read before that stays.
