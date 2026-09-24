//! T-222 (AWARE-053, C40 content half): one emission arriving over two paths, blind, through the
//! mock SDR.
//!
//! **The scene** (`multipath_echo`) puts three narrowband 2-FSK burst emitters in one span, none
//! of their bands overlapping, all of the same family, bandwidth, symbol rate and deviation. Two
//! of them are the **same transmission** — the second is a delayed, attenuated copy of the first,
//! burst for burst. The third is an independent station keying to its own schedule.
//!
//! Nothing in the frequencies, the bandwidths or the modulations separates the pair from the
//! decoy, so every geometric rule the system already has (T-219's band overlap, its image /
//! harmonic / intermod arithmetic, T-598's LO slope) must leave all three alone. Only **content**
//! decides, which is what this suite asserts:
//!
//! 1. [`t222_a_two_path_scene_yields_one_emission_with_a_multipath_relationship`] — the pair
//!    collapses to ONE shown row, with the copy recorded `multipath-of` the direct path; the
//!    measured lag matches the injected delay within the measurement's own stated resolution; and
//!    the claim carries its reasoning and its arithmetic.
//! 2. [`t222_two_distinct_stations_sharing_a_family_are_not_related`] — the decoy stays its own
//!    shown row with no relation claimed against it, and neither of the pair is ever related to
//!    *it*. Same family, same bandwidth, same modulation, different content.
//! 3. [`t222_the_multipath_claim_is_reversible_ranked_evidence_and_never_a_delete`] — the copy
//!    keeps its id, its sightings and its history, is reachable with `relations=all`, and the
//!    claim is an append-only row a revocation can undo.
//!
//! **Blind.** The truth list — which emitter is the direct path, which is the echo, the injected
//! `delay_s` and `attenuation_db` — lives in the fixture's annotations, which `blind_replay`
//! strips and seals before the mock SDR opens the recording. No frequency is handed to the run,
//! nothing is looked up, and truth is opened only in the assertions. **The tolerance is a priori**
//! ([`DELAY_TOLERANCE_RESOLUTIONS`]) and is stated as a multiple of the resolution the system
//! itself reports, never fitted to what it produced.

use std::sync::OnceLock;

use hk_e2e::{Fixture, SynthRequest};
use hk_model::{EmitterId, InventoryQuery, RelationKind, RelationVisibility};
use serde_json::Value;

use crate::blind::{BlindRun, BlindSource, blind_replay};
use crate::common::*;

const T222: &str = "T-222";
const AWARE_053: &str = "AWARE-053";

// ---------------------------------------------------------------------------------------------
// A-priori thresholds. Fixed with their derivations before the suite was first run.
// ---------------------------------------------------------------------------------------------

/// How many of the system's **own reported time resolutions** the measured delay may differ from
/// the injected one by.
///
/// The content series is rasterised from the detection record, so a delay is measured on a grid
/// whose bin the claim discloses (`detail.delay_resolution_s`). Two bins of error covers the
/// quantisation of both edges; three leaves a bin of slack for the detector's own frame
/// boundaries. It is deliberately expressed against the *stated* resolution rather than a fixed
/// millisecond figure: a claim that is honest about what it resolved is testable against it, and
/// a claim that inflates its resolution fails here.
const DELAY_TOLERANCE_RESOLUTIONS: f64 = 3.0;

/// Ceiling on that tolerance whatever the resolution, seconds. Without it a rule could pass by
/// declaring a resolution as coarse as the delay itself.
const DELAY_TOLERANCE_MAX_S: f64 = 0.030;

/// A produced row matches a truth emitter when its centre is this close, Hz. Half the scene's
/// channel spacing, so no row can match two emitters.
const CENTER_TOL_HZ: f64 = 60e3;

// ---------------------------------------------------------------------------------------------
// The scene and the run.
// ---------------------------------------------------------------------------------------------

/// The scene. Shape only: what is on the air, and when, is the fixture's business.
fn scene() -> SynthRequest {
    SynthRequest::new("multipath_echo").seed(222)
}

/// The **false-positive** scene (review of 2026-09-22): the two paired channels are no longer one
/// transmission but TWO INDEPENDENT emitters, each with its own payloads, keying on one fixed
/// 0.5 s cadence a fixed 50 ms apart. Their envelopes are identical and correlate a perfect 1.00
/// at that phase; the only thing that could compete with the peak is the cadence itself, which
/// repeats 0.5 s away — more than three times further than the lag search ever looks.
///
/// This is the shape that must claim NOTHING. The weaker of the two is a real, independent
/// emission, and relating it would hide it from the inventory and silence it in the watch.
fn independent_pair_scene() -> SynthRequest {
    SynthRequest::new("multipath_echo")
        .seed(2229)
        .param("pair_independent", 1)
        .param("cadence_s", 0.5)
}

/// The private truth of the two-path scene, read only after the run.
struct Truth {
    direct_hz: f64,
    echo_hz: f64,
    decoy_hz: f64,
    delay_s: f64,
    attenuation_db: f64,
}

fn truth(fx: &Fixture) -> Truth {
    let s = fx
        .scenario()
        .and_then(|t| t.get("multipath"))
        .unwrap_or_else(|| panic!("[{T222}] the fixture carries no multipath truth"));
    let f = |k: &str| {
        s.get(k)
            .and_then(Value::as_f64)
            .unwrap_or_else(|| panic!("[{T222}] multipath truth has no {k}"))
    };
    Truth {
        direct_hz: f("direct_rf_center_hz"),
        echo_hz: f("echo_rf_center_hz"),
        decoy_hz: f("decoy_rf_center_hz"),
        delay_s: f("delay_s"),
        attenuation_db: f("attenuation_db"),
    }
}

struct Run {
    blind: BlindRun,
    fx: Fixture,
}

/// The shared run; `None` when the synthetic generator is unavailable (skip).
fn run() -> Option<&'static Run> {
    static RUN: OnceLock<Option<Run>> = OnceLock::new();
    RUN.get_or_init(|| {
        let out = match SynthRequest::generate(&scene()) {
            Ok(out) => out,
            Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
                eprintln!("SKIP {}: {err}", module_path!());
                return None;
            }
            Err(err) => panic!("synthetic scenario generation failed: {err}"),
        };
        let fx = out.fixture(0).unwrap();
        let blind = blind_replay(&fx.meta_path, "t222mp", BlindSource::default());
        Some(Run { blind, fx })
    })
    .as_ref()
}

/// The independent-pair run; `None` when the synthetic generator is unavailable (skip).
fn independent_pair_run() -> Option<&'static Run> {
    static RUN: OnceLock<Option<Run>> = OnceLock::new();
    RUN.get_or_init(|| {
        let out = match SynthRequest::generate(&independent_pair_scene()) {
            Ok(out) => out,
            Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
                eprintln!("SKIP {}: {err}", module_path!());
                return None;
            }
            Err(err) => panic!("synthetic scenario generation failed: {err}"),
        };
        let fx = out.fixture(0).unwrap();
        let blind = blind_replay(&fx.meta_path, "t222fp", BlindSource::default());
        Some(Run { blind, fx })
    })
    .as_ref()
}

/// Every inventory row, deferring or not.
fn all_rows(repo: &hk_model::Repository) -> Vec<hk_model::InventoryEntry> {
    inventory(
        repo,
        InventoryQuery {
            relations: RelationVisibility::All,
            limit: hk_model::cluster::MAX_INVENTORY_PAGE,
            ..InventoryQuery::default()
        },
    )
}

/// Every inventory row (deferring or not) whose centre is within [`CENTER_TOL_HZ`] of `f_hz`.
fn rows_at(repo: &hk_model::Repository, f_hz: f64) -> Vec<hk_model::InventoryEntry> {
    all_rows(repo)
        .into_iter()
        .filter(|e| (e.emitter.f_center_hz - f_hz).abs() <= CENTER_TOL_HZ)
        .collect()
}

/// Served `/api/inventory` rows (the default list: deferring rows hidden) near `f_hz`.
fn served_at(rows: &[Value], f_hz: f64) -> Vec<&Value> {
    rows.iter()
        .filter(|r| {
            r["f_center_hz"]
                .as_f64()
                .is_some_and(|f| (f - f_hz).abs() <= CENTER_TOL_HZ)
        })
        .collect()
}

fn row_id(r: &Value) -> EmitterId {
    r["id"].as_str().unwrap().parse().unwrap()
}

// ---------------------------------------------------------------------------------------------
// 1. The finding.
// ---------------------------------------------------------------------------------------------

/// **The acceptance.** A two-path scene yields ONE emission, with a multipath relationship whose
/// measured lag matches the injected delay within the resolution the system itself reports.
#[test]
fn t222_a_two_path_scene_yields_one_emission_with_a_multipath_relationship() {
    let Some(run) = run() else { return };
    let t = truth(&run.fx);
    let r = repo(&run.blind.dir.0);
    for e in all_rows(&r) {
        eprintln!(
            "[{T222}] row {:.4} MHz bw {:.1} kHz state {:?} count {} rel {:?}",
            e.emitter.f_center_hz / 1e6,
            e.emitter.bandwidth_hz / 1e3,
            e.lifecycle,
            e.emitter.count,
            r.emitter_relations(e.emitter.id)
                .unwrap()
                .iter()
                .map(|x| (x.kind, x.source_id))
                .collect::<Vec<_>>(),
        );
    }
    let direct = rows_at(&r, t.direct_hz);
    let echo = rows_at(&r, t.echo_hz);
    eprintln!(
        "[{T222}/{AWARE_053}] direct {:.4} MHz: {} rows; echo {:.4} MHz: {} rows",
        t.direct_hz / 1e6,
        direct.len(),
        t.echo_hz / 1e6,
        echo.len()
    );
    assert!(
        !direct.is_empty() && !echo.is_empty(),
        "[{T222}] both paths must be detected before anything can be said about them"
    );

    // The claim: one of the two rows defers to the other as the delayed copy.
    let mut found = None;
    for e in direct.iter().chain(echo.iter()) {
        for rel in r.emitter_relations(e.emitter.id).unwrap() {
            if rel.kind == RelationKind::MultipathOf {
                found = Some((e.emitter.clone(), rel));
            }
        }
    }
    let (deferring, rel) = found.unwrap_or_else(|| {
        panic!(
            "[{T222}] no multipath relationship between the two paths of one transmission \
             (direct {:.4} MHz, echo {:.4} MHz)",
            t.direct_hz / 1e6,
            t.echo_hz / 1e6
        )
    });
    eprintln!("[{T222}] claim: {}", rel.reason);
    eprintln!(
        "[{T222}] detail: {}",
        rel.detail.clone().unwrap_or_default()
    );

    // It points at the *other* path, and the right way round: the echo is the later, weaker copy.
    let source = r.emitter(rel.source_id).unwrap();
    assert!(
        (deferring.f_center_hz - t.echo_hz).abs() <= CENTER_TOL_HZ,
        "[{T222}] the row that defers must be the echo, not the direct path: {:.4} MHz",
        deferring.f_center_hz / 1e6
    );
    assert!(
        (source.f_center_hz - t.direct_hz).abs() <= CENTER_TOL_HZ,
        "[{T222}] the claim must name the direct path: {:.4} MHz",
        source.f_center_hz / 1e6
    );

    // The measurement: the lag is the injected delay, within the system's own stated resolution.
    let detail = rel
        .detail
        .as_ref()
        .unwrap_or_else(|| panic!("[{T222}] a multipath claim discloses its arithmetic"));
    let delay_s = detail["delay_s"].as_f64().expect("delay_s");
    let resolution_s = detail["delay_resolution_s"]
        .as_f64()
        .expect("delay_resolution_s");
    let tol = (DELAY_TOLERANCE_RESOLUTIONS * resolution_s).min(DELAY_TOLERANCE_MAX_S);
    eprintln!(
        "[{T222}] delay measured {:.1} ms vs injected {:.1} ms (resolution {:.1} ms, tolerance \
         {:.1} ms); path difference {:.0} km +/- {:.0} km",
        delay_s * 1e3,
        t.delay_s * 1e3,
        resolution_s * 1e3,
        tol * 1e3,
        detail["path_difference_m"].as_f64().unwrap_or(f64::NAN) / 1e3,
        detail["path_difference_resolution_m"]
            .as_f64()
            .unwrap_or(f64::NAN)
            / 1e3,
    );
    assert!(
        (delay_s - t.delay_s).abs() <= tol,
        "[{T222}] the measured lag must match the injected delay within the resolution the claim \
         states: measured {delay_s:.6} s, injected {:.6} s, tolerance {tol:.6} s",
        t.delay_s
    );
    // The path difference is the delay read as a reflection, and is reported with its uncertainty.
    let path_m = detail["path_difference_m"]
        .as_f64()
        .expect("path_difference");
    assert!(
        (path_m - delay_s * 299_792_458.0).abs() < 1.0,
        "[{T222}] the path difference is c x the measured delay, disclosed"
    );
    assert!(
        detail["path_difference_resolution_m"]
            .as_f64()
            .is_some_and(|v| v > 0.0),
        "[{T222}] never a distance without its uncertainty"
    );
    // The attenuation the scene injected is measured, not assumed.
    let att = detail["attenuation_db"].as_f64().expect("attenuation_db");
    assert!(
        att > 0.0,
        "[{T222}] the echo must measure weaker than the direct path (injected {:.1} dB): {att:.1} \
         dB",
        t.attenuation_db
    );
    // Its reasoning is disclosed, in words, and names neither a database nor an identity value.
    assert!(
        rel.reason.contains("same content") && rel.reason.contains("path"),
        "[{T222}] the reasoning is rendered and disclosed: {}",
        rel.reason
    );

    // ONE emission on the wire: the default list shows the direct path and not its copy.
    let shown_direct = served_at(&run.blind.api_rows, t.direct_hz);
    let shown_echo = served_at(&run.blind.api_rows, t.echo_hz);
    eprintln!(
        "[{T222}] served: {} row(s) at the direct path, {} at the echo",
        shown_direct.len(),
        shown_echo.len()
    );
    assert_eq!(
        shown_echo.len(),
        0,
        "[{T222}] the delayed copy must not be listed as a second emission: {shown_echo:?}"
    );
    assert_eq!(
        shown_direct.len(),
        1,
        "[{T222}] one emission, one shown row: {shown_direct:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// 2. The guard.
// ---------------------------------------------------------------------------------------------

/// The guard: two distinct stations sharing a family, a bandwidth and a modulation are **not**
/// related. Only their content differs, and only their content may decide.
#[test]
fn t222_two_distinct_stations_sharing_a_family_are_not_related() {
    let Some(run) = run() else { return };
    let t = truth(&run.fx);
    let r = repo(&run.blind.dir.0);
    let decoy = rows_at(&r, t.decoy_hz);
    assert!(
        !decoy.is_empty(),
        "[{T222}] the independent station must be detected before the guard means anything"
    );
    let pair: Vec<EmitterId> = rows_at(&r, t.direct_hz)
        .iter()
        .chain(rows_at(&r, t.echo_hz).iter())
        .map(|e| e.emitter.id)
        .collect();
    for e in &decoy {
        let rels = r.emitter_relations(e.emitter.id).unwrap();
        let multipath: Vec<_> = rels
            .iter()
            .filter(|x| x.kind == RelationKind::MultipathOf)
            .collect();
        assert!(
            multipath.is_empty(),
            "[{T222}] an independent station of the same family is never a delayed copy of \
             another: {multipath:?}"
        );
        // Nor may the pair be claimed to be copies of *it*.
        for id in &pair {
            assert!(
                r.emitter_relations(*id)
                    .unwrap()
                    .iter()
                    .all(|x| x.kind != RelationKind::MultipathOf || x.source_id != e.emitter.id),
                "[{T222}] nothing may defer to the independent station"
            );
        }
    }
    let shown = served_at(&run.blind.api_rows, t.decoy_hz);
    eprintln!(
        "[{T222}] guard station {:.4} MHz: {} shown row(s)",
        t.decoy_hz / 1e6,
        shown.len()
    );
    assert_eq!(
        shown.len(),
        1,
        "[{T222}] the distinct station stays its own shown row: {shown:?}"
    );
    assert!(
        shown[0]["relation"].is_null(),
        "[{T222}] a genuinely distinct station never defers: {}",
        shown[0]
    );
}

// ---------------------------------------------------------------------------------------------
// 3. Ranked evidence, reversible, never a delete.
// ---------------------------------------------------------------------------------------------

/// The claim is ranked evidence with its reasoning, reversible, and never an automatic delete: the
/// copy keeps its id, its sightings and its history, and is still listed with `relations=all`.
#[test]
fn t222_the_multipath_claim_is_reversible_ranked_evidence_and_never_a_delete() {
    let Some(run) = run() else { return };
    let t = truth(&run.fx);
    let r = repo(&run.blind.dir.0);
    let echo = rows_at(&r, t.echo_hz);
    let deferring: Vec<_> = echo
        .iter()
        .filter(|e| {
            r.emitter_relations(e.emitter.id)
                .unwrap()
                .iter()
                .any(|x| x.kind == RelationKind::MultipathOf)
        })
        .collect();
    assert!(
        !deferring.is_empty(),
        "[{T222}] nothing deferred, so there is no claim to check the reversibility of"
    );
    for e in deferring {
        let id = e.emitter.id;
        // Kept in full: the row, its sightings, and its own time extent.
        assert!(
            r.emitter(id).unwrap().count > 0,
            "[{T222}] the deferring row keeps its observations"
        );
        assert!(
            !r.observation_spans(id).unwrap().is_empty(),
            "[{T222}] the deferring row keeps its own time extent"
        );
        // Ranked: the claim carries the score that decided it.
        let rel = r
            .emitter_relations(id)
            .unwrap()
            .into_iter()
            .find(|x| x.kind == RelationKind::MultipathOf)
            .unwrap();
        assert!(
            rel.score.is_some_and(|s| s > 0.0),
            "[{T222}] a multipath claim is ranked evidence and carries its score"
        );
        assert!(rel.active, "[{T222}] the claim is standing");
        // Append-only and auditable: the history holds it.
        let history = r.emitter_relation_history(id).unwrap();
        assert!(
            history.iter().any(|x| x.kind == RelationKind::MultipathOf),
            "[{T222}] the claim is an append-only record"
        );
        // Reversible: a revocation is a new row, and the row is listed again.
        let mut w = repo(&run.blind.dir.0);
        w.record_emitter_relation(&hk_model::RelationClaim {
            emitter_id: id,
            source_id: rel.source_id,
            kind: RelationKind::MultipathOf,
            artifact: None,
            active: false,
            t: rel.t,
            author: hk_model::RelationAuthor::User,
            actor: "t222-acceptance".into(),
            reason: "the user disagrees".into(),
            score: None,
            detail: None,
        })
        .unwrap();
        assert!(
            w.emitter_relations(id)
                .unwrap()
                .iter()
                .all(|x| x.kind != RelationKind::MultipathOf),
            "[{T222}] a revocation removes the standing claim without deleting anything"
        );
        assert_eq!(
            w.emitter_relation_history(id).unwrap().len(),
            history.len() + 1,
            "[{T222}] the revocation is an appended row, never an edit"
        );
        assert!(
            w.emitter(id).unwrap().count > 0,
            "[{T222}] nothing about the row was deleted by any of this"
        );
    }

    // And with `relations=all` the copy was reachable all along.
    let all = rows_at(&r, t.echo_hz);
    let ids: Vec<_> = all.iter().map(row_id_of).collect();
    assert!(
        !ids.is_empty(),
        "[{T222}] the copy is still listed with relations=all"
    );
}

fn row_id_of(e: &hk_model::InventoryEntry) -> EmitterId {
    e.emitter.id
}

// ---------------------------------------------------------------------------------------------
// 4. The false positive the peak value cannot see.
// ---------------------------------------------------------------------------------------------

/// **Two independent emitters on one cadence, a sub-150 ms phase apart, are NOT one emission.**
///
/// Their envelopes are identical, so the correlation peak is perfect and — inside the ±150 ms the
/// lag search covers — nothing competes with it, because the cadence that would repeats 0.5 s
/// away. Peak and dominance alone therefore say "related" about two unrelated signals, and the
/// weaker one would be hidden from the default inventory and silenced in the watch.
///
/// What refuses it is evidence *about the evidence*: the delay has to be earned by independent
/// parts of the window, and neither series may repeat itself at any period — including the ones
/// the lag search is structurally blind to.
///
/// **This test is not vacuous, and that was measured.** With
/// [`hk_model::multipath::MULTIPATH_MIN_SUPPORT`] and
/// [`hk_model::multipath::MULTIPATH_MAX_SELF_SIMILARITY`] disabled, this very scene records
/// `multipath-of` on the weaker row: *"correlate 0.99 at that one lag (2.6x the next best) …
/// 14 966 km of extra path"*. The scene reaches the correlation and is refused by these two
/// guards, not by an earlier one. (Support alone does not save it — both emitters key constantly,
/// so all 8 segments agree; it is the 0.5 s cadence, 3x further away than the lag search looks,
/// that denies the delay. The sparse one-coincidence half of the defect is covered by
/// `hk_model::multipath`'s unit tests.)
#[test]
fn t222_two_independent_emitters_on_one_cadence_are_never_one_emission() {
    let Some(run) = independent_pair_run() else {
        return;
    };
    let t = truth(&run.fx);
    let r = repo(&run.blind.dir.0);
    let first = rows_at(&r, t.direct_hz);
    let second = rows_at(&r, t.echo_hz);
    for e in first.iter().chain(second.iter()) {
        eprintln!(
            "[{T222}] false-positive scene: row {:.4} MHz, relations {:?}",
            e.emitter.f_center_hz / 1e6,
            r.emitter_relations(e.emitter.id)
                .unwrap()
                .iter()
                .map(|x| (x.kind, x.reason.clone()))
                .collect::<Vec<_>>()
        );
    }
    assert!(
        !first.is_empty() && !second.is_empty(),
        "[{T222}] both emitters must be detected before the guard means anything"
    );
    for e in first.iter().chain(second.iter()) {
        let claims: Vec<_> = r
            .emitter_relations(e.emitter.id)
            .unwrap()
            .into_iter()
            .filter(|x| x.kind == RelationKind::MultipathOf)
            .collect();
        assert!(
            claims.is_empty(),
            "[{T222}] two independent emitters sharing a cadence are not one emission over two \
             paths: {claims:?}"
        );
    }
    // And the consequence that matters: neither real emission is hidden.
    for f_hz in [t.direct_hz, t.echo_hz] {
        let shown = served_at(&run.blind.api_rows, f_hz);
        assert_eq!(
            shown.len(),
            1,
            "[{T222}] a real, independent emission stays listed at {:.4} MHz: {shown:?}",
            f_hz / 1e6
        );
        assert!(
            shown[0]["relation"].is_null(),
            "[{T222}] and defers to nothing: {}",
            shown[0]
        );
    }
}
