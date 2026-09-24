//! SIGNAL-062 / AWARE-053 (T3, T-289): blind acceptance over the **second** real off-air capture,
//! `fixtures/hackrf/capture-2026-09-15-fm-band` — 45 s of the FM broadcast band recorded at
//! 100.8 MHz / 2.4 Msps (LNA 32, VGA 30, amp on), served by the mock SDR device.
//!
//! **The fixture's truth is measured, not expected.** An analysis pass (T-289) ran this recording
//! through the composed pipeline and, independently, through a spectral survey of the samples: a
//! mean periodogram against a running-median baseline for continuous emissions, a per-time-bin
//! search against each frequency's own median over time for bursts, and an FM discriminator over
//! the whole capture to test each candidate for a 19 kHz stereo pilot. Every annotation records
//! the measurement that justifies it. The capture's README anticipated a station near 99.75 MHz, a
//! "steady carrier" at 99.99 MHz, a dominant station at 101.45 MHz and an intermittent burst at
//! 100.3 MHz; the measurements put the stations elsewhere, made the 99.99 MHz line a receiver
//! spur, and found **nothing at all** at 100.3 MHz. Only what was measured is annotated, and the
//! windows measured to be silent are recorded in the scenario's `measured_absent` so a test can
//! assert they stay silent.
//!
//! **Blind (T-047).** The truth is stripped before the device ever sees the recording
//! (`blind_replay`), the test names no frequency to tune to, and the private truth is read only
//! here, after the run, to match against everything the system produced.
//!
//! What this fixture adds over `signal_062.rs`'s 5 s window: a 45 s run of real air with three
//! emissions at three very different SNRs (13.7, 7.0 and 4.7 dB), two receiver artefacts the
//! detector must flag rather than catalogue, and — the part no synthetic fixture can give —
//! **measured silence**, which is what keeps a detector's false alarms honest.

use std::sync::OnceLock;

use hk_e2e::blind::{matches_truth, matching, truth_emissions};
use hk_e2e::{Checks, Fixture, TruthItem};
use hk_model::detection::SpurReason;
use hk_model::{
    Detection, FreqRange, IdentityScheme, InventoryEntry, InventoryIdentity, InventoryQuery,
    LifecycleState, LinkTarget, Region,
};

use crate::blind::{BlindRun, BlindSource, assert_truth_found, blind_replay, center_tol_hz};
use crate::common::*;

const SIGNAL_062: &str = "SIGNAL-062";
const AWARE_053: &str = "AWARE-053";
const FIXTURE_DIR: &str = "fixtures/hackrf/capture-2026-09-15-fm-band";

/// One blind run of the capture, shared by the tests below.
pub struct FmBandRun {
    pub dir: TempDir,
    pub summary: hk_pipeline::RunSummary,
    /// The private truth (read only by the asserting test, after the run).
    pub fx: Fixture,
}

/// The shared run; `None` when the LFS data is not fetched (skip).
pub fn run() -> Option<&'static FmBandRun> {
    static RUN: OnceLock<Option<FmBandRun>> = OnceLock::new();
    RUN.get_or_init(|| {
        let meta = real_fixture_in(FIXTURE_DIR, "iq")?;
        let fx = Fixture::load(&meta).unwrap();
        let BlindRun { dir, summary, .. } = blind_replay(&meta, "fmband", BlindSource::default());
        Some(FmBandRun { dir, summary, fx })
    })
    .as_ref()
}

/// Every detection of the run, matched against truth by the caller.
fn detections(r: &FmBandRun) -> Vec<Detection> {
    repo(&r.dir.0)
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
        .unwrap()
        .into_iter()
        .collect()
}

/// The run's Confirmed inventory entries.
fn confirmed(r: &FmBandRun) -> Vec<InventoryEntry> {
    inventory(
        &repo(&r.dir.0),
        InventoryQuery {
            states: vec![LifecycleState::Confirmed],
            ..InventoryQuery::default()
        },
    )
}

/// Whether the detector placed this detection (or emitter extent) **on** `t`: the same
/// centre-tolerance rule the suite matches emissions with ([`center_tol_hz`], which floors at
/// 10 kHz for a line as narrow as an artefact). Plain box overlap will not do here — an artefact's
/// annotated box is a few hundred Hz wide, and this run's band-wide detections (occupied
/// bandwidths of 1.3–2.2 MHz, centred half a megahertz away) sweep across it without being it.
fn on_truth(t: &TruthItem, f_center_hz: f64, bandwidth_hz: f64) -> bool {
    matches_truth(t, 0.0, f_center_hz, bandwidth_hz, center_tol_hz(t))
}

/// The strongest detection matching `t`, dB, or `-inf`.
fn strongest_for(dets: &[Detection], t: &TruthItem) -> f64 {
    let tol = center_tol_hz(t);
    dets.iter()
        .filter(|d| matches_truth(t, 0.0, d.f_center_hz, d.obw_hz, tol))
        .map(|d| d.snr_peak_db)
        .fold(f64::NEG_INFINITY, f64::max)
}

fn fm_band_every_measured_emission_is_found_blind(r: &FmBandRun) {
    assert_eq!(r.summary.always_on_lost_samples, 0);
    assert!(
        r.summary.counter("/chains/attached") >= 1,
        "[{SIGNAL_062}] no chain attached over 45 s of real air"
    );
    // Each annotated emission detected within frequency/extent/time tolerance, and — for the two
    // measured to be WFM broadcast (a 19 kHz pilot, +35.5 dB and +8.7 dB over 45 s) — an FM
    // broadcast explanation in the top-k. The third emission carries no modulation at all — T-317
    // identified it as harmonic 43 of a free-running ~2.3364 MHz oscillator, so its 28 kHz is that
    // oscillator's frequency noise multiplied by 43 — and its kind (`oscillator-harmonic`) has no
    // expected service, so only detection is asserted. The one band-plan row here is
    // `fm-broadcast`, which is precisely what the measurement rules out; letting it name this
    // emission would be DB-as-truth (AWARE-053, docs/10 §3.2).
    let found = assert_truth_found(SIGNAL_062, &r.dir.0, &r.fx, 0.0, true);
    assert_eq!(
        found.len(),
        3,
        "[{SIGNAL_062}] the measured truth list holds three emissions: {found:?}"
    );
}

fn fm_band_confirmed_emitters_are_the_measured_emissions(r: &FmBandRun) {
    let confirmed = confirmed(r);
    let truth = truth_emissions(&r.fx);
    for t in &truth {
        let tol = center_tol_hz(t);
        let m = matching(
            t,
            0.0,
            &confirmed,
            |e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz),
            tol,
        );
        assert!(
            !m.is_empty(),
            "[{SIGNAL_062}] measured emission {} {:?} reached no Confirmed emitter; confirmed: {:?}",
            t.kind,
            t.label,
            confirmed
                .iter()
                .map(|e| e.emitter.f_center_hz)
                .collect::<Vec<_>>()
        );
    }
    // And nothing else was confirmed: a Confirmed row is the inventory's claim that a real emitter
    // is there, so a confirmed row over measured noise is a ghost.
    for e in &confirmed {
        assert!(
            truth.iter().any(|t| matches_truth(
                t,
                0.0,
                e.emitter.f_center_hz,
                e.emitter.bandwidth_hz,
                center_tol_hz(t)
            )),
            "[{SIGNAL_062}] Confirmed emitter at {:.4} MHz (bw {:.1} kHz) matches no measured \
             emission: a ghost",
            e.emitter.f_center_hz / 1e6,
            e.emitter.bandwidth_hz / 1e3
        );
    }
    eprintln!(
        "[{SIGNAL_062}] {} confirmed emitters, {} measured emissions",
        confirmed.len(),
        truth.len()
    );
}

/// Margin, dB, by which a measured-silent window must stay under the weakest measured emission.
/// Measured when the fixture was annotated: 9.3 dB at 100.3 MHz and 6.2 dB at 101.42–101.49 MHz.
const SILENCE_MARGIN_DB: f64 = 5.0;

fn fm_band_measured_silence_is_not_catalogued(r: &FmBandRun) {
    let scenario =
        r.fx.scenario()
            .unwrap_or_else(|| panic!("[{SIGNAL_062}] the fixture has no scenario truth"));
    let windows: Vec<(String, f64, f64)> = scenario
        .get("measured_absent")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("[{SIGNAL_062}] the scenario records no measured_absent windows"))
        .iter()
        .filter(|w| w["assert_silent"].as_bool().unwrap_or(false))
        .map(|w| {
            (
                w["label"].as_str().unwrap_or_default().to_owned(),
                w["f_lo_hz"].as_f64().unwrap(),
                w["f_hi_hz"].as_f64().unwrap(),
            )
        })
        .collect();
    assert!(
        !windows.is_empty(),
        "[{SIGNAL_062}] no measured-silent window is marked for assertion"
    );

    let dets = detections(r);
    let confirmed = confirmed(r);
    // The weakest measured emission's strongest detection is the bar: anything in a window the
    // analysis pass measured as silent must sit well under it.
    let truth = truth_emissions(&r.fx);
    let weakest = truth
        .iter()
        .map(|t| strongest_for(&dets, t))
        .fold(f64::INFINITY, f64::min);
    assert!(
        weakest.is_finite(),
        "[{SIGNAL_062}] a measured emission has no detection at all"
    );

    for (label, lo, hi) in windows {
        for e in &confirmed {
            assert!(
                e.emitter.f_center_hz < lo || e.emitter.f_center_hz > hi,
                "[{SIGNAL_062}] Confirmed emitter at {:.4} MHz inside {label}, which the analysis \
                 pass measured as silent",
                e.emitter.f_center_hz / 1e6
            );
        }
        let strongest = dets
            .iter()
            .filter(|d| d.f_center_hz >= lo && d.f_center_hz <= hi)
            .map(|d| d.snr_peak_db)
            .fold(f64::NEG_INFINITY, f64::max);
        let n = dets
            .iter()
            .filter(|d| d.f_center_hz >= lo && d.f_center_hz <= hi)
            .count();
        eprintln!(
            "[{SIGNAL_062}] {label}: {n} detections, strongest {strongest:.1} dB; weakest measured \
             emission {weakest:.1} dB"
        );
        assert!(
            !strongest.is_finite() || strongest <= weakest - SILENCE_MARGIN_DB,
            "[{SIGNAL_062}] {label} was measured silent, but its strongest detection \
             ({strongest:.1} dB) is within {SILENCE_MARGIN_DB} dB of the weakest measured \
             emission's ({weakest:.1} dB): the detector is promoting noise to signal here"
        );
    }
}

/// Detections allowed in one measured-silent window.
///
/// Derived, not fitted. The detector's seed threshold is a per-cell `Pfa` of 1e-6 and a box needs
/// a seed plus three connected frames, which S4 pinned as "0 false boxes in 3.31 MHz·h";
/// `crates/hk-detect/tests/false_alarm.rs` asserts that design as ≤ 2 boxes per 1.02 MHz·h of
/// ideal noise. Every window here is ~1000× smaller than that exposure (70 kHz × 45 s =
/// 8.8e-4 MHz·h), so the design expectation is 0 and this bound is the same allowance the unit
/// suite uses, carried over unchanged rather than tightened to today's output.
const SILENT_WINDOW_MAX_DETECTIONS: usize = 2;

fn fm_band_measured_silence_stays_within_the_designed_false_alarm_rate(r: &FmBandRun) {
    let scenario =
        r.fx.scenario()
            .unwrap_or_else(|| panic!("[{SIGNAL_062}] the fixture has no scenario truth"));
    let windows: Vec<(String, f64, f64)> = scenario
        .get("measured_absent")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("[{SIGNAL_062}] the scenario records no measured_absent windows"))
        .iter()
        .filter(|w| w["assert_silent"].as_bool().unwrap_or(false))
        .map(|w| {
            (
                w["label"].as_str().unwrap_or_default().to_owned(),
                w["f_lo_hz"].as_f64().unwrap(),
                w["f_hi_hz"].as_f64().unwrap(),
            )
        })
        .collect();
    assert!(!windows.is_empty());

    let dets = detections(r);
    let all = inventory(&repo(&r.dir.0), InventoryQuery::default());

    // The half that proves detection still works: a bound that passes because the detector stopped
    // detecting is the failure mode this guards against, so every measured emission must still
    // carry a detection before any silence bound is read.
    let truth = truth_emissions(&r.fx);
    for t in &truth {
        let snr = strongest_for(&dets, t);
        assert!(
            snr.is_finite(),
            "[{SIGNAL_062}] measured emission {} {:?} has no detection at all: the silence bound \
             below would pass for the wrong reason",
            t.kind,
            t.label
        );
        eprintln!(
            "[{SIGNAL_062}] control: {} {:?} strongest detection {snr:.1} dB",
            t.kind, t.label
        );
    }

    let mut failures = Vec::new();
    for (label, lo, hi) in windows {
        let n = dets
            .iter()
            .filter(|d| d.f_center_hz >= lo && d.f_center_hz <= hi)
            .count();
        let emitters: Vec<f64> = all
            .iter()
            .filter(|e| e.emitter.f_center_hz >= lo && e.emitter.f_center_hz <= hi)
            .map(|e| e.emitter.f_center_hz / 1e6)
            .collect();
        eprintln!(
            "[{SIGNAL_062}] {label}: {n} detections (bound {SILENT_WINDOW_MAX_DETECTIONS}), \
             {} inventory entries {emitters:?}",
            emitters.len()
        );
        if n > SILENT_WINDOW_MAX_DETECTIONS {
            failures.push(format!(
                "{label}: {n} detections against a designed bound of \
                 {SILENT_WINDOW_MAX_DETECTIONS}"
            ));
        }
        // Candidates count too: an unexplained candidate emitter over measured silence is exactly
        // the clutter the Explore lists show, whether or not it ever reaches Confirmed.
        if emitters.len() > SILENT_WINDOW_MAX_DETECTIONS {
            failures.push(format!(
                "{label}: {} inventory entries {emitters:?} over measured silence",
                emitters.len()
            ));
        }
    }
    assert!(failures.is_empty(), "[{SIGNAL_062}] {failures:#?}");
}

fn fm_band_wfm_rds_decoded_from_a_second_real_capture(r: &FmBandRun) {
    // The station the analysis pass measured with a 19 kHz pilot and an RDS PI. Its PI was
    // decoded independently on a different capture of the same station 2.5 days earlier
    // (`fm_100p8M_2p4M_l32g30a1_t1p5_5s`, py/fixtures/rds_ref.py), so it is truth and not this
    // run's own output.
    let truth =
        r.fx.of_kind("wfm-broadcast")
            .into_iter()
            .find(|t| t.str("/rds/pi_hex").is_some())
            .cloned()
            .unwrap_or_else(|| panic!("[{SIGNAL_062}] no WFM truth carries an RDS PI"));
    let tol = center_tol_hz(&truth);
    let repo = repo(&r.dir.0);
    let all = inventory(&repo, InventoryQuery::default());
    let at_station = matching(
        &truth,
        0.0,
        &all,
        |e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz),
        tol,
    );

    let (entry, pi) = at_station
        .iter()
        .find_map(|e| match &e.identity {
            InventoryIdentity::Clear { identity, .. }
                if identity.scheme == IdentityScheme::RdsPi =>
            {
                Some((*e, identity.clone()))
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("[{SIGNAL_062}] no emitter at the measured station decoded an RDS PI")
        });
    let truth_pi = truth.str("/rds/pi_hex").unwrap();
    assert!(
        pi.value.eq_ignore_ascii_case(truth_pi),
        "[{SIGNAL_062}] decoded PI {} vs measured truth {truth_pi}",
        pi.value
    );

    // The mode was chosen by the system, and the pilot it measured agrees with the analysis
    // pass's own discriminator measurement (18994.14 Hz, read in the receiver's uncalibrated
    // clock at 12.21 Hz Welch resolution, so the tolerance is two bins).
    let truth_pilot = truth.expect_f64("/pilot/frequency_hz");
    let mut modes = Vec::new();
    let mut pilots = Vec::new();
    for l in repo.emitter_links(entry.emitter.id).unwrap() {
        if let LinkTarget::Demodulation(id) = l.target {
            let d = repo.demodulation(id).unwrap();
            modes.push(d.mode.clone());
            if let Some(p) = d.params.pilot_hz {
                pilots.push(p);
            }
        }
    }
    assert!(
        !modes.is_empty() && modes.iter().all(|m| m == "wfm"),
        "[{SIGNAL_062}] auto mode selection gave {modes:?}, not WFM"
    );
    assert!(
        !pilots.is_empty(),
        "[{SIGNAL_062}] no WFM demodulation stored its pilot frequency"
    );
    for p in &pilots {
        assert!(
            (p - truth_pilot).abs() <= 25.0,
            "[{SIGNAL_062}] pilot {p} Hz vs the measured {truth_pilot} Hz"
        );
    }
    eprintln!(
        "[{SIGNAL_062}] station at {:.4} MHz: PI {}, modes {modes:?}, pilots {pilots:?}",
        entry.emitter.f_center_hz / 1e6,
        pi.value
    );
}

fn fm_band_receiver_artefacts_are_flagged_not_catalogued(r: &FmBandRun) {
    let dets = detections(r);
    let confirmed = confirmed(r);
    let artefacts = r.fx.artefacts();
    assert_eq!(
        artefacts.len(),
        2,
        "[{AWARE_053}] the analysis pass measured two receiver artefacts"
    );
    for t in artefacts {
        // The 99.9996 MHz line is the one the capture's README called a "steady carrier": the
        // analysis pass measured it steady to 0.93 Hz RMS against the receiver's own clock and
        // sitting on the 10th harmonic of the internal 10 MHz reference, so it is an artefact.
        let want = match (t.kind.as_str(), t.str("reason")) {
            ("spur", Some("ref_harmonic")) => SpurReason::RefHarmonic,
            ("dc-offset", _) => SpurReason::Dc,
            _ => panic!(
                "[{AWARE_053}] no expected spur reason for artefact {}",
                t.kind
            ),
        };
        let at: Vec<&Detection> = dets
            .iter()
            .filter(|d| on_truth(t, d.f_center_hz, d.obw_hz))
            .collect();
        assert!(
            !at.is_empty(),
            "[{AWARE_053}] artefact {:?} produced no detection at all",
            t.label
        );
        let flagged = at
            .iter()
            .filter(|d| d.flags.spur_reason == Some(want))
            .count();
        assert_eq!(
            flagged,
            at.len(),
            "[{AWARE_053}] {:?}: {flagged} of {} detections carry spur_reason {want:?}",
            t.label,
            at.len()
        );
        for e in &confirmed {
            assert!(
                !on_truth(t, e.emitter.f_center_hz, e.emitter.bandwidth_hz),
                "[{AWARE_053}] the receiver artefact {:?} was confirmed as an emitter at {:.4} MHz",
                t.label,
                e.emitter.f_center_hz / 1e6
            );
        }
        eprintln!(
            "[{AWARE_053}] artefact {:?}: {} detections, all flagged {want:?}",
            t.label,
            at.len()
        );
    }
}

/// The module's six assertions, over **one** replay of the 216 MB capture.
///
/// They were six `#[test]`s sharing a `static OnceLock`, which shares nothing under nextest —
/// every test is its own process, so the recording was replayed six times per gate for one run
/// (`docs/test-speed-review-2026-09-22.md` §2.4). Each is still a separately named check that runs
/// even if an earlier one fails; see [`hk_e2e::Checks`].
#[test]
fn fm_band_2026_09_15() {
    let Some(r) = run() else { return };
    let mut c = Checks::new("fm_band_2026_09_15");
    c.check("every_measured_emission_is_found_blind", || {
        fm_band_every_measured_emission_is_found_blind(r)
    });
    c.check("confirmed_emitters_are_the_measured_emissions", || {
        fm_band_confirmed_emitters_are_the_measured_emissions(r)
    });
    c.check("measured_silence_is_not_catalogued", || {
        fm_band_measured_silence_is_not_catalogued(r)
    });
    c.check(
        "measured_silence_stays_within_the_designed_false_alarm_rate",
        || fm_band_measured_silence_stays_within_the_designed_false_alarm_rate(r),
    );
    c.check("wfm_rds_decoded_from_a_second_real_capture", || {
        fm_band_wfm_rds_decoded_from_a_second_real_capture(r)
    });
    c.check("receiver_artefacts_are_flagged_not_catalogued", || {
        fm_band_receiver_artefacts_are_flagged_not_catalogued(r)
    });
    c.finish();
}
