//! T-323: **the GNSS-SDR licence boundary, checked before anything depends on it.**
//!
//! GNSS-SDR is GPL-3.0-or-later. ADR-0010 lets GPL code into the product only behind the plugin
//! process boundary: exec'd as a subprocess, never linked. This test checks that the boundary
//! holds in this crate — the only one that names GNSS-SDR — and that the wrapper stays a
//! producer of evidence, not a second route into blind detection:
//!
//! - no build script, no `links`, no FFI (`extern "C"`, `#[link`) anywhere in `hk-gnss`;
//! - no dependency whose name mentions GNSS-SDR or is a `-sys` crate;
//! - the manifest declares the licence and runs the wrapper, which only *spawns* `gnss-sdr`;
//! - the ADR-0010 ledger carries the row;
//! - `hk-gnss` does not depend on `hk-detect` or `hk-context`, and the wrapper writes no
//!   `identity` and no `annotation` (the end-to-end half of that is in `gnss_sdr_plugin.rs`).
//!
//! Each predicate is shown to fire on a violating input first, so a pass means something.

use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    crate_dir().ancestors().nth(2).unwrap().to_path_buf()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
}

/// Non-comment lines.
fn code_lines(text: &str, comment: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with(comment))
        .map(str::to_string)
        .collect()
}

/// Whether Rust source links anything natively.
fn has_ffi(src: &str) -> bool {
    code_lines(src, "//")
        .iter()
        .any(|l| l.contains("extern \"C\"") || l.contains("#[link") || l.contains("extern crate"))
}

/// Whether a Cargo manifest builds or links natively, or names a GNSS-SDR/`-sys` dependency.
fn manifest_links(toml: &str) -> bool {
    code_lines(toml, "#").iter().any(|l| {
        let key = l.split('=').next().unwrap_or("").trim();
        let key = key.split('.').next().unwrap_or(key);
        key == "build"
            || key == "links"
            || key.contains("gnss-sdr")
            || key.contains("gnss_sdr")
            || key.ends_with("-sys")
    })
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn the_predicates_fire_on_what_they_forbid() {
    assert!(has_ffi("extern \"C\" { fn gnss_sdr_main(); }"));
    assert!(has_ffi("#[link(name = \"gnss-sdr\")]"));
    assert!(!has_ffi("// no extern \"C\" here"));
    assert!(manifest_links("[package]\nbuild = \"build.rs\"\n"));
    assert!(manifest_links("[package]\nlinks = \"gnss\"\n"));
    assert!(manifest_links("[dependencies]\ngnss-sdr-sys = \"0.1\"\n"));
    assert!(manifest_links("[dependencies]\nfoo-sys.workspace = true\n"));
    assert!(!manifest_links(
        "# build = \"build.rs\"\n[dependencies]\nserde = \"1\"\n"
    ));
}

#[test]
fn gnss_sdr_is_executed_never_linked() {
    let dir = crate_dir();
    assert!(
        !dir.join("build.rs").exists(),
        "hk-gnss must have no build script"
    );
    let toml = read(&dir.join("Cargo.toml"));
    assert!(!manifest_links(&toml), "hk-gnss must not link native code");
    let mut srcs = Vec::new();
    rust_sources(&dir.join("src"), &mut srcs);
    assert!(srcs.len() >= 7, "found {srcs:?}");
    for p in &srcs {
        assert!(!has_ffi(&read(p)), "FFI in {}", p.display());
    }
    let wrapper = read(&dir.join("src/bin/hk-plugin-gnss-sdr.rs"));
    assert!(
        wrapper.contains("Command::new(&self.args_gnss_sdr)"),
        "the wrapper reaches GNSS-SDR by spawning it"
    );
}

#[test]
fn the_manifest_declares_the_licence_and_runs_the_wrapper() {
    let m: serde_json::Value = serde_json::from_str(&read(
        &workspace_root().join("plugins/gnss-sdr/manifest.json"),
    ))
    .unwrap();
    assert_eq!(m["licence"], "GPL-3.0-or-later");
    assert_eq!(m["executable"], "hk-plugin-gnss-sdr");
    assert_eq!(m["output"]["schema_id"], hk_gnss::GNSS_SDR_SCHEMA);
}

#[test]
fn the_adr_0010_ledger_carries_gnss_sdr() {
    let ledger = read(&workspace_root().join("docs/adr/0010-language-and-licence-ledger.md"));
    assert!(
        ledger
            .lines()
            .any(|l| l.starts_with("| GNSS-SDR (executable")
                && l.contains("GPL-3.0-or-later")
                && l.contains("hk-plugin-gnss-sdr")),
        "ADR-0010 needs the GNSS-SDR subprocess row"
    );
}

#[test]
fn the_receiver_is_not_a_second_path_into_detection() {
    let toml = read(&crate_dir().join("Cargo.toml"));
    for forbidden in ["hk-detect", "hk-context", "hk-pipeline"] {
        assert!(
            !code_lines(&toml, "#").iter().any(|l| l.contains(forbidden)),
            "hk-gnss must not depend on {forbidden}"
        );
    }
    let wrapper = code_lines(
        &read(&crate_dir().join("src/bin/hk-plugin-gnss-sdr.rs")),
        "//",
    );
    for forbidden in ["\"identity\"", "\"annotation\""] {
        assert!(
            !wrapper.iter().any(|l| l.contains(forbidden)),
            "the wrapper must never emit {forbidden}"
        );
    }
}
