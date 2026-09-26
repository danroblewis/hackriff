//! **The structural boundary that keeps a database suggestion out of the search** (T-569,
//! ADR-0021 §9.3).
//!
//! The known-signal database has exactly two attach points. It may *order* the search, as
//! `prior_bits` on a hypothesis (§9.1), where it neither ranks nor confirms. And it may *explain*
//! a finished result, as `resolution.explanations[]`, computed by `hk-context` after the
//! [`Resolution`] is sealed. The failure this guards is the narrow one: **a suggestion turning an
//! `unknown` into a label.**
//!
//! A comment saying "explanations are computed afterwards" is not a boundary. This is: `hk-synth`
//! does not depend on `hk-context`, so the code that *computes* a suggestion is un-nameable
//! inside the search. Reaching it would require adding a dependency edge to a manifest below,
//! which these tests fail on — the same mechanism ADR-0018's GNSS guard uses, for the same
//! reason.
//!
//! The `Explanation` **type** lives in `hk-model`, shared vocabulary both sides already depend on.
//! That is deliberate and is not a hole: a type with no producer cannot suggest anything, and the
//! seal test below pins that the engine never fills the field itself.

use std::path::{Path, PathBuf};

use hk_model::repo::synthesis::{Explanation, ExplanationSource, ExplanationStatus};
use hk_synth::trace::{Resolution, ResolutionKind};

/// Crates that carry known-signal knowledge, which the search must not reach.
const KNOWN_SIGNAL_CRATES: [&str; 2] = ["hk-context", "hk-gnss"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/hk-synth")
        .to_path_buf()
}

fn manifest_of(crate_name: &str) -> String {
    let path = workspace_root()
        .join("crates")
        .join(crate_name)
        .join("Cargo.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Whether `manifest` declares a dependency on `crate_name`, ignoring comments so prose about the
/// rule cannot trip the scan.
fn declares(manifest: &str, crate_name: &str) -> bool {
    manifest
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .any(|l| l.contains(crate_name))
}

/// Proves the predicate can fail. A boundary test never observed failing is not evidence of a
/// boundary.
#[test]
fn the_guard_itself_catches_the_edge_it_exists_to_catch() {
    let clean = "[dependencies]\nhk-model.workspace = true\nhk-dsp.workspace = true\n";
    assert!(!declares(clean, "hk-context"));

    let violating = "[dependencies]\nhk-model.workspace = true\nhk-context.workspace = true\n";
    assert!(
        declares(violating, "hk-context"),
        "the guard would not notice the dependency edge it exists to forbid"
    );

    // A dev-dependency is a route too: a test could otherwise wire the search to a band plan.
    assert!(declares(
        "[dev-dependencies]\nhk-context.workspace = true\n",
        "hk-context"
    ));

    // ...but a comment about the rule is not a violation of it.
    assert!(!declares(
        "# hk-context is deliberately absent: see ADR-0021 §9.3\n[dependencies]\nhk-model.workspace = true\n",
        "hk-context"
    ));
}

/// **The load-bearing assertion** (ADR-0021 §9.3).
///
/// If this fails, do not relax it. It means the search has been given a route to the known-signal
/// database, which is how a suggestion stops explaining a result and starts becoming one.
#[test]
fn the_search_cannot_reach_the_known_signal_database() {
    let manifest = manifest_of("hk-synth");
    for known in KNOWN_SIGNAL_CRATES {
        assert!(
            !declares(&manifest, known),
            "hk-synth/Cargo.toml names {known}.\n\
             The decoder search must not depend on known-signal knowledge (ADR-0021 §9.3): a \
             suggestion explains a result, it never becomes one, and the only thing keeping a \
             future edit inside hk-synth from reading one is this missing dependency edge.\n\
             Explanations are computed by hk-context AFTER the Resolution is sealed, by \
             hk-pipeline::synth, which hands hk-context an immutable &Resolution and receives \
             back only a Vec<Explanation>. If the search needs a prior, that is `prior_bits` in \
             hk_synth::seed (ADR-0015 §4.2), which orders and never ranks — do not add this edge."
        );
    }
}

/// The reverse direction, for completeness: the explaining crate must not be able to drive the
/// search either, which would be the same leak with the arrow drawn the other way.
#[test]
fn the_explaining_crate_does_not_depend_on_the_search() {
    assert!(
        !declares(&manifest_of("hk-context"), "hk-synth"),
        "hk-context must not depend on hk-synth: the explainer is handed a sealed result through \
         hk-pipeline, and must not be able to reach into the search that produced it"
    );
}

/// The engine seals `explanations` **empty**. Whatever suggestions exist are attached afterwards
/// by `hk-pipeline::synth`; nothing inside the search may put one there, and `not_searched` — the
/// one resolution the engine builds with no search behind it — is the case to pin.
#[test]
fn a_sealed_resolution_carries_no_suggestions_of_its_own() {
    let sealed = Resolution::not_searched(None);
    assert_eq!(sealed.kind, ResolutionKind::NotSearched);
    assert!(
        sealed.explanations.is_empty(),
        "the search sealed a resolution that already names a suggestion"
    );
}

/// Attaching a suggestion is additive and leaves every sealed field alone — the §9.2 table, as a
/// test. The `Explanation` value is built here by hand precisely because `hk-synth` cannot build
/// one from reference data: it has no route to any.
#[test]
fn attaching_a_suggestion_changes_nothing_the_search_decided() {
    let sealed = Resolution::not_searched(Some("2026-09-25T00:00:00Z".to_owned()));
    let mut explained = sealed.clone();
    explained.explanations = vec![Explanation {
        source: ExplanationSource::BandPlan,
        identity: "fm-broadcast".into(),
        score: 0.95,
        distance_hz: Some(-150_000.0),
        status: ExplanationStatus::Unexpected,
        data_age_days: Some(41),
        reasoning: "nearest assignment 150 kHz below the measured centre".into(),
    }];

    assert_eq!(explained.kind, sealed.kind);
    assert_eq!(explained.deepest_verdict, sealed.deepest_verdict);
    assert_eq!(explained.reason, sealed.reason);
    assert_eq!(explained.coverage, sealed.coverage);
    assert_eq!(explained.null_control, sealed.null_control);
    assert_eq!(explained.suspected, sealed.suspected);
    assert_eq!(explained.ruled_out, sealed.ruled_out);
    assert_eq!(explained.retry, sealed.retry);
    assert_eq!(explained.summary, sealed.summary);
}
