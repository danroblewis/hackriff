//! T-607: compile the vendored liquid-dsp with `cc`, not with its own CMake.
//!
//! Why not CMake (measured, docs/18 §7.1): liquid's `FindSIMD.cmake` probes with `try_run`, which
//! cannot execute when cross-compiling, so an aarch64 cross build needs hand-set cache variables;
//! its `FIND_FFTW` defaults ON and would silently link GPLv2 FFTW if the host has it; and it would
//! add `cmake` as a host requirement on the Jetson. The library's own file list is fixed and
//! architecture-independent (SIMD variants are `#include`d and compiled behind `BUILD_*` guards with
//! runtime dispatch), so `sources.txt` — the exact list liquid 1.8.2's CMakeLists.txt compiles —
//! plus a generated `liquid.config.h` is the whole build.
//!
//! What is deliberately NOT built: FFTW (liquid falls back to its own FFT) and libfec. Without
//! libfec, liquid's convolutional and Reed–Solomon codecs are stubs that return NULL — see the
//! crate docs and `tests/coverage.rs`, which pins that.

use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
    let os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS");
    // NEON is architectural on aarch64 (the Mac and the Jetson), so it is on there. x86 SIMD stays
    // off: x86_64 is only CI, and liquid's portable kernels are exact, just slower.
    let neon = u8::from(arch == "aarch64");

    // Mirrors cmake/liquid.config.h.in with -DENABLE_TIMESTAMPS=OFF (reproducible: no host name,
    // date or git hash baked into the binary) and colour off (it writes to stderr).
    let config = format!(
        r#"#ifndef __CONFIG_H__
#define __CONFIG_H__
#define PROJECT_COPYRIGHT   "Copyright (c) 2007 - 2026 Joseph Gaeddert"
#define PROJECT_LICENSE     "MIT"
#define PROJECT_AUTHOR      "Joseph D. Gaeddert"
#define PROJECT_HOMEPAGE    "https://liquidsdr.org"
#define PROJECT_DESCRIPTION "liquid-dsp, vendored in hk-liquid-sys"
#define BUILD_GITHASH       "null"
#define BUILD_DATETIME      "null"
#define BUILD_HOSTNAME      "null"
#define BUILD_OS            "null"
#define BUILD_ARCH          "null"
#define BUILD_TOOLCHAIN     "null"
#define BUILD_TYPE          "Release"
#define BUILD_ENV           "cargo"
#define TARGET_OS           "{os}"
#define TARGET_ARCH         "{arch}"
#define LOGGING_ENABLED     1
#define LOGGING_LEVEL       0
#define COLOR_ENABLED       0
#define BUILD_ALTIVEC       0
#define BUILD_NEON          {neon}
#define BUILD_MMX           0
#define BUILD_SSE           0
#define BUILD_SSE2          0
#define BUILD_SSE3          0
#define BUILD_SSSE3         0
#define BUILD_SSE41         0
#define BUILD_SSE42         0
#define BUILD_AVX           0
#define BUILD_FMA3          0
#define BUILD_AVX2          0
#define BUILD_AVX512        0
#define BUILD_AMX           0
#define BUILD_AMX101        0
#define BUILD_AMX102        0
#endif
"#
    );
    fs::write(out.join("liquid.config.h"), config).expect("write liquid.config.h");

    let sources = fs::read_to_string("sources.txt").expect("read sources.txt");
    let mut build = cc::Build::new();
    build
        .include("vendor/liquid-dsp/include")
        .include(&out)
        .define("LIQUID_LOGGING_ENABLE", None)
        .define("LIQUID_LOG_LEVEL_COMPILE", "0")
        .flag("-std=gnu11")
        // Upstream compiles with -O3 in every build type; keep that in dev/test builds too, so the
        // kernels run at the speed the Jetson will see (the same reason as T-175's opt-levels).
        .opt_level(3)
        // Third-party C: its warnings are not ours to fix, and `cargo:warning` noise would bury
        // the workspace's own.
        .warnings(false)
        .flag_if_supported("-w");
    for file in sources.lines().map(str::trim).filter(|l| !l.is_empty()) {
        build.file(format!("vendor/liquid-dsp/{file}"));
    }
    build.compile("liquid");

    // libm on Linux; on macOS it resolves to libSystem.
    println!("cargo:rustc-link-lib=m");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=sources.txt");
    println!("cargo:rerun-if-changed=vendor/liquid-dsp");
}
