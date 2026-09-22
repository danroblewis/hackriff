//! Links the system libhackrf when the `hackrf` feature is on (T-037a), and the system librtlsdr
//! when the `rtlsdr` feature is on (T-514). Without the features nothing is linked, so CI builds
//! need neither library.
//!
//! Lookup: `HACKRF_LIB_DIR` / `RTLSDR_LIB_DIR` (a directory holding the shared library) when set,
//! else pkg-config `libhackrf` / `librtlsdr` (Homebrew on macOS, `libhackrf-dev` /
//! `librtlsdr-dev` on Debian/JetPack). Both are linked dynamically and never vendored
//! (ADR-0010).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=HACKRF_LIB_DIR");
    println!("cargo:rerun-if-env-changed=RTLSDR_LIB_DIR");
    #[cfg(feature = "hackrf")]
    link_libhackrf();
    #[cfg(feature = "rtlsdr")]
    link_librtlsdr();
}

/// Links the system librtlsdr for the `rtlsdr` feature (T-514).
#[cfg(feature = "rtlsdr")]
fn link_librtlsdr() {
    if let Some(dir) = std::env::var_os("RTLSDR_LIB_DIR") {
        println!(
            "cargo:rustc-link-search=native={}",
            std::path::PathBuf::from(dir).display()
        );
        println!("cargo:rustc-link-lib=dylib=rtlsdr");
        return;
    }
    if let Err(e) = pkg_config::Config::new().probe("librtlsdr") {
        panic!(
            "feature `rtlsdr` needs the system librtlsdr (macOS: `brew install librtlsdr`; Debian \
             or JetPack: `apt install librtlsdr-dev`), found through pkg-config or \
             RTLSDR_LIB_DIR: {e}"
        );
    }
}

#[cfg(feature = "hackrf")]
fn link_libhackrf() {
    if let Some(dir) = std::env::var_os("HACKRF_LIB_DIR") {
        println!(
            "cargo:rustc-link-search=native={}",
            std::path::PathBuf::from(dir).display()
        );
        println!("cargo:rustc-link-lib=dylib=hackrf");
        return;
    }
    // No version constraint: Homebrew's libhackrf.pc (2026.01.3) declares an empty Version.
    if let Err(e) = pkg_config::Config::new().probe("libhackrf") {
        panic!(
            "feature `hackrf` needs the system libhackrf (macOS: `brew install hackrf`; Debian or \
             JetPack: `apt install libhackrf-dev`), found through pkg-config or HACKRF_LIB_DIR: {e}"
        );
    }
}
