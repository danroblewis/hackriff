//! Links the system libhackrf when the `hackrf` feature is on (T-037a). Without the feature
//! nothing is linked, so CI builds need no libhackrf.
//!
//! Lookup: `HACKRF_LIB_DIR` (a directory holding `libhackrf.{dylib,so}`) when set, else
//! pkg-config `libhackrf` (Homebrew on macOS, `libhackrf-dev` on Debian/JetPack). libhackrf is
//! linked dynamically and never vendored (ADR-0010).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=HACKRF_LIB_DIR");
    #[cfg(feature = "hackrf")]
    link_libhackrf();
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
