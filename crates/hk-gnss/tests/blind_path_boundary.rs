//! **The structural boundary that confines C36's blind-first exception.**
//!
//! GPS L1 acquisition is known-signal-*led*: it despreads against published PRN codes rather than
//! finding energy. That is a real, physically forced exception (the signal is 20–30 dB below the
//! noise floor), and the danger is not that it is wrong but that it **leaks** — that "the
//! database may lead here" becomes reachable from the general detector, and the project quietly
//! turns into a scanner with a frequency list.
//!
//! A comment saying "GNSS only" is not a boundary. This test is.
//!
//! The blind detection path is `hk-detect`, built on `hk-core`, `hk-dsp`, `hk-estimate` and
//! `hk-model`. None of them may depend on `hk-gnss` (the PRN codebook and correlator) or on
//! `hk-context` (the band-plan prior crate). Because Rust cannot name a type from a crate it does
//! not depend on, a `PrnCodebook` or a `BandTable` is *un-nameable* inside the detector: reaching
//! either would require first adding a dependency edge to a manifest below, which this test
//! fails on.
//!
//! That makes the boundary compile-time and machine-checked, rather than a matter of discipline.

use std::path::{Path, PathBuf};

/// Crates that make up the ordinary blind detection path.
const BLIND_PATH: [&str; 4] = ["hk-detect", "hk-core", "hk-dsp", "hk-estimate"];

/// Crates carrying known-signal knowledge, which the blind path must never reach.
const KNOWN_SIGNAL_CRATES: [&str; 2] = ["hk-gnss", "hk-context"];

fn workspace_root() -> PathBuf {
    // crates/hk-gnss → crates → workspace root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/hk-gnss")
        .to_path_buf()
}

/// The text of a crate's manifest.
fn manifest_of(crate_name: &str) -> String {
    let path = workspace_root()
        .join("crates")
        .join(crate_name)
        .join("Cargo.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Whether `manifest` declares a dependency on `crate_name`, ignoring comment lines so that a
/// comment mentioning a crate cannot trip the scan.
///
/// Kept separate from the filesystem so the guard can be *shown to fail* — see
/// [`the_guard_itself_catches_the_edge_it_exists_to_catch`]. A boundary test that has never been
/// observed failing is not evidence of a boundary.
fn declares(manifest: &str, crate_name: &str) -> bool {
    manifest
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .any(|l| l.contains(crate_name))
}

/// Proves the guard below can actually fail. Without this, the real assertions could be passing
/// because the predicate never fires, and nobody would know.
#[test]
fn the_guard_itself_catches_the_edge_it_exists_to_catch() {
    let clean = "[dependencies]\nhk-core.workspace = true\nhk-dsp.workspace = true\n";
    assert!(!declares(clean, "hk-gnss"));

    let violating = "[dependencies]\nhk-core.workspace = true\nhk-gnss.workspace = true\n";
    assert!(
        declares(violating, "hk-gnss"),
        "the guard would not notice the dependency edge it exists to forbid"
    );

    // A dev-dependency is a route too, and must also be caught.
    let dev = "[dev-dependencies]\nhk-context.workspace = true\n";
    assert!(declares(dev, "hk-context"));

    // ...but prose about the rule is not a violation of it.
    let comment =
        "# hk-gnss is deliberately absent here\n[dependencies]\nhk-core.workspace = true\n";
    assert!(
        !declares(comment, "hk-gnss"),
        "a comment mentioning the crate must not trip the guard"
    );
}

/// **The load-bearing assertion.** No crate on the blind detection path may depend on a crate
/// that carries known-signal knowledge — not as a dependency, and not as a dev-dependency.
///
/// If this fails, do not relax it. It means known-signal data has been given a route into blind
/// detection, which is the one thing C36's exception must never do.
#[test]
fn the_blind_detection_path_cannot_reach_known_signal_knowledge() {
    for blind in BLIND_PATH {
        let manifest = manifest_of(blind);
        for known in KNOWN_SIGNAL_CRATES {
            assert!(
                !declares(&manifest, known),
                "{blind}/Cargo.toml names {known}.\n\
                 The blind detection path must not depend on known-signal knowledge. GNSS \
                 acquisition is the project's one documented exception to blind-first \
                 (crates/hk-gnss/src/lib.rs), and it is confined by exactly this missing \
                 dependency edge. Adding it turns the detector into a frequency-list scanner.\n\
                 If GNSS needs something from the detector, invert the direction or move the \
                 shared piece into hk-model/hk-dsp — do not add this edge."
            );
        }
    }
}

/// The exception must not gain a route to drive detection from its own side either.
#[test]
fn gnss_does_not_depend_on_the_detector() {
    let manifest = manifest_of("hk-gnss");
    assert!(
        !declares(&manifest, "hk-detect"),
        "hk-gnss must not depend on hk-detect: the known-code path may not steer blind detection"
    );
}

/// An acquisition is not a detection, and must never be laundered into one.
///
/// `hk-gnss` depends on `hk-model` (for `Timestamp`), so `Detection` is technically nameable
/// here. This asserts it is never named: a GNSS result carries `AcquisitionEvidence`, and
/// nothing in this crate constructs the blind-detection object that the inventory treats as a
/// measurement.
#[test]
fn acquisition_never_constructs_a_blind_detection() {
    let src = workspace_root().join("crates").join("hk-gnss").join("src");
    let mut offenders = Vec::new();

    for entry in std::fs::read_dir(&src).expect("hk-gnss/src is readable") {
        let path = entry.expect("readable dir entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable source");
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            // Doc comments discuss the rule; they are not violations of it.
            if code.starts_with("//") {
                continue;
            }
            if code.contains("Detection") || code.contains("DetectionId") {
                offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "hk-gnss constructs or names a blind Detection:\n{}\n\
         A known-code acquisition is evidence of a different kind and carries \
         AcquisitionEvidence. Writing it into the inventory as a Detection would make the \
         known-signal database the source of a 'measurement'.",
        offenders.join("\n")
    );
}

/// The detector's own inputs are measurements, so the leak has no shape to take even if the
/// dependency edge existed. This pins the fact that `hk-detect`'s public surface takes no
/// frequency list, band plan, or code.
#[test]
fn the_detector_takes_no_known_signal_input() {
    let cfg = workspace_root()
        .join("crates")
        .join("hk-detect")
        .join("src")
        .join("config.rs");
    let text = std::fs::read_to_string(&cfg).expect("hk-detect config is readable");
    for forbidden in ["BandTable", "PrnCodebook", "AllocationRow", "band_plan"] {
        assert!(
            !text.contains(forbidden),
            "hk-detect/src/config.rs names {forbidden}: detection config must carry receiver \
             knowledge (sensitivity profiles, filter edges, measured spur masks) only, never \
             knowledge of what a frequency means"
        );
    }
}
