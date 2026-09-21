//! T-586 (AWARE-011): **retune diversity** — one region surveyed from several centres, through
//! the mock SDR, blind.
//!
//! **The field report this exists for.** 2026-09-21, staging server on the live HackRF: signals
//! appeared, shifted and vanished as the *centre frequency* changed. A line at 100.8 MHz was
//! present from some centres and absent from others while staying put when tuned directly to it,
//! and a bright line moved to a **new absolute frequency** on retune.
//!
//! **Why that is a measurement and not an opinion.** A receiver artefact — DC/LO leakage, an
//! internal spur at a fixed IF offset, an IQ image, an IM3 product — is manufactured at a
//! frequency the local oscillator defines, so it moves with the centre. A real emission exists at
//! an absolute radio frequency and does not. Retune diversity turns "which is it?" into a
//! measured slope (`hk_model::retune`), and that is what this suite drives end to end.
//!
//! **The scene is deliberately featureless.** Every line in the fixture is a CW tone in white
//! noise, so nothing about a line's *shape* can distinguish the classes and no assertion here can
//! quietly succeed by recognising a modulation. Only the behaviour across centres separates them.
//!
//! **Blind.** `blind_replay` seals the fixture's annotations and serves a truth-stripped copy to
//! the device, which visits each recorded centre in turn through the real `Source` interface
//! (`blind::SurveyDevice`, one mock SDR per centre — [`the_device_really_did_retune`] asserts the
//! retune happened rather than assuming it). The truth list is opened only afterwards, to assert.
//! No frequency is ever looked up in the known-signal database, and nothing is tuned to a truth.
//!
//! **An artefact correctly flagged is a success.** The truth list carries *both* classes, and a
//! run passes only if it puts every member of each in the right one — a system that reported
//! nothing at all would fail [`emitters_and_centres_compared`], which asserts the counts that
//! make the rest non-vacuous.

use std::collections::BTreeMap;

use hk_e2e::fixture::Role;
use hk_e2e::{Fixture, SynthRequest, TruthItem, synth_or_skip};
use hk_model::retune::{RetuneObservation, RetuneSlope, RetuneSummary, RetuneTolerance, classify};
use hk_model::{Detection, FreqRange, Region, Repository};

use crate::blind::{BlindRun, BlindSource, blind_replay};
use crate::common::*;

const T586: &str = "T-586";

// ---------------------------------------------------------------------------------------------
// A-priori thresholds. Fixed here, with their arithmetic, before the suite was ever run against
// the fixture. None is a number that came out of a measurement.
// ---------------------------------------------------------------------------------------------

/// How close a produced group's invariant coordinate must be to the truth's, Hz.
///
/// The scene's lines are CW, so the only error is the detector's own centroid: the dwell FFT bin
/// at these rates is low-kHz and a centroid over a handful of bins wanders a bin or two. 20 kHz is
/// a few bins of slack, and is **two orders below** the 500 kHz the centre moves between captures
/// and an order below the 180 kHz minimum in-capture line separation the generator enforces — so
/// it cannot be loose enough to confuse two lines or two slopes.
const MATCH_TOL_HZ: f64 = 20e3;

/// Distinct centres the survey must have been driven at for the invariant to mean anything.
///
/// Two would measure a slope; three is the fixture's diversity and is asserted so that a run which
/// silently collapsed to one capture fails loudly instead of passing vacuously.
const REQUIRED_CENTRES: usize = 3;

/// Real emitters the fixture puts on the air, all in every capture's passband.
const REQUIRED_EMITTERS: usize = 3;

/// LO-relative artefact families the fixture manufactures (DC leakage, one internal spur).
const REQUIRED_ARTEFACTS: usize = 2;

// ---------------------------------------------------------------------------------------------
// The run.
// ---------------------------------------------------------------------------------------------

fn request() -> SynthRequest {
    SynthRequest::new("retune_diversity").seed(7)
}

/// Detections of a finished run paired with the LO they were measured under.
///
/// The LO is read the way the data model already records it: the detection's `provenance_ref`,
/// resolved to its `Provenance`, whose `tune.center_hz` is the centre the front end was on. No
/// side channel, no capture index smuggled through the test.
fn observations(repo: &Repository, dets: &[Detection]) -> Vec<RetuneObservation> {
    dets.iter()
        .map(|d| {
            let p = repo
                .provenance(d.provenance_ref)
                .expect("every detection resolves its provenance");
            RetuneObservation {
                lo_hz: p.tune.center_hz,
                f_center_hz: d.f_center_hz,
                bandwidth_hz: d.obw_hz,
            }
        })
        .collect()
}

/// A finished blind survey: its detections, their LOs, and the retune verdicts over them.
struct Survey {
    run: BlindRun,
    fixture: Fixture,
    dets: Vec<Detection>,
    obs: Vec<RetuneObservation>,
    summary: RetuneSummary,
}

impl Survey {
    fn open() -> Option<Self> {
        let out = match SynthRequest::generate(&request()) {
            Ok(out) => out,
            Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
                eprintln!("SKIP {}: {err}", module_path!());
                return None;
            }
            Err(err) => panic!("[{T586}] synthetic scenario generation failed: {err}"),
        };
        let fixture = out.fixture(0).unwrap();
        let run = blind_replay(&fixture.meta_path, "t586", BlindSource::default());
        let repo = repo(&run.dir.0);
        let dets = repo
            .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
            .expect("detections");
        let obs = observations(&repo, &dets);
        let summary = classify(&obs, &RetuneTolerance::default());
        eprintln!(
            "[{T586}] {} detections over {} distinct LOs {:?} -> {} groups, {} unexplained",
            obs.len(),
            summary.centres(),
            summary
                .los_hz
                .iter()
                .map(|f| format!("{:.4} MHz", f / 1e6))
                .collect::<Vec<_>>(),
            summary.groups.len(),
            summary.unexplained.len()
        );
        for g in &summary.groups {
            eprintln!(
                "[{T586}]   {:>9} invariant {:>12.1} Hz  centres {}  members {}  spread {:.0} Hz",
                g.slope.as_str(),
                g.invariant_hz,
                g.centres(),
                g.members.len(),
                g.spread_hz
            );
        }
        Some(Self {
            run,
            fixture,
            dets,
            obs,
            summary,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The private truth. Read here and nowhere else; the run was told none of it.
// ---------------------------------------------------------------------------------------------

/// Distinct absolute frequencies of the truth's real emissions, and how many captures each is in.
fn truth_emitters(fx: &Fixture) -> BTreeMap<i64, usize> {
    group(fx.truth.iter().filter(|t| t.role == Role::Emission))
}

/// Distinct LO offsets of the truth's LO-relative artefacts, and how many captures each is in.
///
/// The offset is `center_hz − tuner centre`, taken from the artefact's own truth fields: a DC
/// artefact is offset 0 and the internal spur is at its fixed IF offset.
fn truth_artefact_offsets(fx: &Fixture) -> BTreeMap<i64, usize> {
    let items = fx
        .truth
        .iter()
        .filter(|t| t.role == Role::Artefact && matches!(t.kind.as_str(), "dc-offset" | "lo-spur"));
    let mut out: BTreeMap<i64, usize> = BTreeMap::new();
    for t in items {
        let offset = t
            .f64("offset_hz")
            .unwrap_or_else(|| panic!("[{T586}] artefact truth {} has no offset_hz", t.kind));
        *out.entry(key(offset)).or_default() += 1;
    }
    out
}

fn group<'a>(items: impl Iterator<Item = &'a TruthItem>) -> BTreeMap<i64, usize> {
    let mut out: BTreeMap<i64, usize> = BTreeMap::new();
    for t in items {
        *out.entry(key(t.center_hz())).or_default() += 1;
    }
    out
}

/// Frequencies within a kHz are the same line, for counting truth items only.
fn key(hz: f64) -> i64 {
    (hz / 1e3).round() as i64
}

fn hz(k: i64) -> f64 {
    k as f64 * 1e3
}

// ---------------------------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------------------------

/// The whole acceptance run: one survey, then every check over it.
///
/// It is one `#[test]` because the survey is the expensive part and nextest gives each test its
/// own process; the checks below are ordered so that the ones which make the others non-vacuous
/// (the retune actually happened; the counts compared) run first and fail first.
#[test]
fn t586_retune_diversity_separates_emitters_from_receiver_artefacts() {
    let Some(s) = Survey::open() else { return };
    the_device_really_did_retune(&s);
    emitters_and_centres_compared(&s);
    a_real_emitter_keeps_its_absolute_frequency_across_centres(&s);
    an_artefact_moves_with_the_lo_and_is_found_lo_relative(&s);
    the_two_classes_are_not_confused(&s);
    the_verdict_is_recorded_on_the_detections_existing_flags(&s);
    the_survey_produced_an_inventory(&s);
}

/// The device really was retuned, and the run really did see several centres.
///
/// Without this the rest is vacuous: a cross-centre check over one centre compares nothing and
/// passes. It also checks the mock honoured the retunes at all — the survey device serves one mock
/// SDR per recorded centre, and if it collapsed them the LOs on the stored provenance would not
/// differ.
fn the_device_really_did_retune(s: &Survey) {
    assert_eq!(
        s.summary.centres(),
        REQUIRED_CENTRES,
        "[{T586}] the survey was driven at {} distinct LOs, not {REQUIRED_CENTRES}: {:?}",
        s.summary.centres(),
        s.summary.los_hz
    );
    // Each centre must have produced measurements of its own, or a "survey" is one dwell with
    // extra metadata.
    let mut per_lo: BTreeMap<i64, usize> = BTreeMap::new();
    for o in &s.obs {
        *per_lo.entry(key(o.lo_hz)).or_default() += 1;
    }
    eprintln!("[{T586}] detections per LO: {per_lo:?}");
    assert_eq!(per_lo.len(), REQUIRED_CENTRES);
    for (lo, n) in &per_lo {
        assert!(
            *n >= REQUIRED_EMITTERS,
            "[{T586}] LO {:.4} MHz produced only {n} detections",
            hz(*lo) / 1e6
        );
    }
}

/// What the run actually compared, asserted as counts, so nothing below can pass on an empty set.
fn emitters_and_centres_compared(s: &Survey) {
    let emitters = truth_emitters(&s.fixture);
    let artefacts = truth_artefact_offsets(&s.fixture);
    eprintln!(
        "[{T586}] compared {} truth emitters x {} centres and {} artefact families x {} centres, \
         over {} detections",
        emitters.len(),
        s.summary.centres(),
        artefacts.len(),
        s.summary.centres(),
        s.dets.len()
    );
    assert_eq!(emitters.len(), REQUIRED_EMITTERS);
    assert_eq!(artefacts.len(), REQUIRED_ARTEFACTS);
    for (f, n) in &emitters {
        assert_eq!(
            *n,
            REQUIRED_CENTRES,
            "[{T586}] truth emitter {:.4} MHz is not in every capture",
            hz(*f) / 1e6
        );
    }
    for (o, n) in &artefacts {
        assert_eq!(
            *n,
            REQUIRED_CENTRES,
            "[{T586}] truth artefact at LO offset {:.1} kHz is not in every capture",
            hz(*o) / 1e3
        );
    }
    assert!(
        s.dets.len() >= (REQUIRED_EMITTERS + REQUIRED_ARTEFACTS) * REQUIRED_CENTRES,
        "[{T586}] only {} detections: too few for {REQUIRED_EMITTERS} emitters and \
         {REQUIRED_ARTEFACTS} artefact families over {REQUIRED_CENTRES} centres",
        s.dets.len()
    );
}

/// **The invariant.** Every real emitter agrees in *absolute* frequency across every centre.
fn a_real_emitter_keeps_its_absolute_frequency_across_centres(s: &Survey) {
    let emitters = truth_emitters(&s.fixture);
    let mut agreed = 0;
    for f in emitters.keys() {
        let f_hz = hz(*f);
        let g = s
            .summary
            .find(RetuneSlope::Absolute, f_hz, MATCH_TOL_HZ)
            .unwrap_or_else(|| {
                panic!(
                    "[{T586}] truth emitter {:.4} MHz: no absolute-invariant group within \
                     {MATCH_TOL_HZ} Hz. Groups: {:?}",
                    f_hz / 1e6,
                    s.summary.groups
                )
            });
        assert_eq!(
            g.centres(),
            REQUIRED_CENTRES,
            "[{T586}] truth emitter {:.4} MHz agreed across only {} centres: {g:?}",
            f_hz / 1e6,
            g.centres()
        );
        assert!(
            g.spread_hz <= MATCH_TOL_HZ,
            "[{T586}] truth emitter {:.4} MHz moved {:.0} Hz across centres",
            f_hz / 1e6,
            g.spread_hz
        );
        eprintln!(
            "[{T586}] emitter {:.4} MHz: agreed across {} centres, spread {:.0} Hz",
            f_hz / 1e6,
            g.centres(),
            g.spread_hz
        );
        agreed += 1;
    }
    assert_eq!(agreed, REQUIRED_EMITTERS);
}

/// **The converse.** Every LO-relative artefact is found to move with the LO, at every centre.
fn an_artefact_moves_with_the_lo_and_is_found_lo_relative(s: &Survey) {
    let artefacts = truth_artefact_offsets(&s.fixture);
    for offset in artefacts.keys() {
        let off_hz = hz(*offset);
        let g = s
            .summary
            .find(RetuneSlope::LoLocked, off_hz, MATCH_TOL_HZ)
            .unwrap_or_else(|| {
                panic!(
                    "[{T586}] truth artefact at LO offset {:.1} kHz: no LO-locked group within \
                     {MATCH_TOL_HZ} Hz. Groups: {:?}",
                    off_hz / 1e3,
                    s.summary.groups
                )
            });
        assert_eq!(
            g.centres(),
            REQUIRED_CENTRES,
            "[{T586}] artefact at LO offset {:.1} kHz tracked the LO over only {} centres",
            off_hz / 1e3,
            g.centres()
        );
        assert!(g.slope.is_receiver_artefact());
        eprintln!(
            "[{T586}] artefact at LO offset {:.1} kHz: tracked the LO across {} centres \
             (absolute frequencies {:?})",
            off_hz / 1e3,
            g.centres(),
            g.members
                .iter()
                .map(|&i| format!("{:.4} MHz", s.obs[i].f_center_hz / 1e6))
                .collect::<Vec<_>>()
        );
    }
}

/// The two classes never swap: no artefact is reported absolute-invariant, and no real emitter is
/// reported LO-relative.
///
/// This is what makes the two tests above a diagnosis rather than two independent searches. A
/// system that flagged *everything* passes them separately and fails here.
fn the_two_classes_are_not_confused(s: &Survey) {
    for f in truth_emitters(&s.fixture).keys() {
        let f_hz = hz(*f);
        for g in s
            .summary
            .groups
            .iter()
            .filter(|g| g.slope.is_receiver_artefact())
        {
            let claimed: Vec<f64> = g
                .members
                .iter()
                .map(|&i| s.obs[i].f_center_hz)
                .filter(|c| (c - f_hz).abs() <= MATCH_TOL_HZ)
                .collect();
            assert!(
                claimed.is_empty(),
                "[{T586}] truth emitter {:.4} MHz was claimed by the {} group at {:.1} Hz: {claimed:?}",
                f_hz / 1e6,
                g.slope.as_str(),
                g.invariant_hz
            );
        }
    }
    for offset in truth_artefact_offsets(&s.fixture).keys() {
        let off_hz = hz(*offset);
        for lo in &s.summary.los_hz {
            let f_hz = lo + off_hz;
            assert!(
                s.summary
                    .find(RetuneSlope::Absolute, f_hz, MATCH_TOL_HZ)
                    .is_none(),
                "[{T586}] the artefact at {:.4} MHz (LO {:.4} MHz + {:.1} kHz) was reported as a \
                 fixed-frequency emission",
                f_hz / 1e6,
                lo / 1e6,
                off_hz / 1e3
            );
        }
    }
}

/// The verdict lands on the existing provenance-backed suspect flags, not a new side channel.
///
/// `RetuneSlope::apply` is the one writer, and it writes what the data model already defines:
/// `spur_candidate` with `SpurReason::LoRelative` for an LO-locked line, `image_retune_confirmed`
/// for a mirrored one. The result must be a self-consistent `DetectionFlags` — `inconsistency()`
/// is the model's own invariant check — and a real emitter must come out of it unflagged by the
/// retune test.
fn the_verdict_is_recorded_on_the_detections_existing_flags(s: &Survey) {
    let mut verdict: Vec<Option<RetuneSlope>> = vec![None; s.obs.len()];
    for g in &s.summary.groups {
        for &i in &g.members {
            verdict[i] = Some(g.slope);
        }
    }
    // What the single-capture flaggers already knew, before any retune was considered. This is
    // the gap the cross-centre check closes, printed rather than asserted: DC leakage is caught by
    // the DC rule from one capture, but an internal spur at an arbitrary IF offset matches no
    // single-capture rule and is indistinguishable from an emission until the centre moves.
    for g in &s.summary.groups {
        let already: Vec<&str> = g
            .members
            .iter()
            .map(|&i| match s.dets[i].flags.spur_reason {
                Some(r) => r.kind_str(),
                None if s.dets[i].flags.spur_candidate => "spur-candidate",
                None => "-",
            })
            .collect();
        eprintln!(
            "[{T586}]   {:>9} invariant {:>12.1} Hz: single-capture spur reasons {already:?}",
            g.slope.as_str(),
            g.invariant_hz
        );
    }
    let (mut flagged, mut clean) = (0usize, 0usize);
    for (i, d) in s.dets.iter().enumerate() {
        let Some(slope) = verdict[i] else { continue };
        let mut flags = d.flags;
        slope.apply(&mut flags);
        assert!(
            flags.inconsistency().is_none(),
            "[{T586}] retune verdict {} left inconsistent flags on {:.4} MHz: {:?}",
            slope.as_str(),
            d.f_center_hz / 1e6,
            flags.inconsistency()
        );
        if slope.is_receiver_artefact() {
            assert!(
                flags.spur_candidate || flags.image_candidate,
                "[{T586}] {} line at {:.4} MHz was not flagged suspect",
                slope.as_str(),
                d.f_center_hz / 1e6
            );
            flagged += 1;
        } else {
            assert!(
                !(flags.spur_candidate
                    && flags.spur_reason == Some(hk_model::detection::SpurReason::LoRelative)),
                "[{T586}] fixed-frequency line at {:.4} MHz was flagged lo-relative",
                d.f_center_hz / 1e6
            );
            assert!(
                !flags.image_retune_confirmed,
                "[{T586}] fixed-frequency line at {:.4} MHz was flagged a confirmed image",
                d.f_center_hz / 1e6
            );
            clean += 1;
        }
    }
    eprintln!(
        "[{T586}] retune verdicts recorded on flags: {flagged} artefact, {clean} absolute, over \
         {} detections",
        s.dets.len()
    );
    assert!(flagged >= REQUIRED_ARTEFACTS * REQUIRED_CENTRES);
    assert!(clean >= REQUIRED_EMITTERS * REQUIRED_CENTRES);
}

/// The run produced outputs at all (and the blind harness's truth-isolation checks ran).
fn the_survey_produced_an_inventory(s: &Survey) {
    eprintln!(
        "[{T586}] /api/inventory rows: {}, detections: {}",
        s.run.api_rows.len(),
        s.dets.len()
    );
    assert!(!s.run.api_rows.is_empty());
}

/// The generator's own layout guard: every scenario parameter this suite's thresholds depend on is
/// the one the fixture was built with, so a later parameter change cannot silently loosen them.
#[test]
fn the_fixture_layout_is_the_one_the_thresholds_assume() {
    let out = synth_or_skip!(request());
    let fx = out.fixture(0).unwrap();
    let centres: Vec<f64> = fx
        .meta
        .captures
        .iter()
        .filter_map(|c| c.frequency)
        .collect();
    assert_eq!(centres.len(), REQUIRED_CENTRES, "captures: {centres:?}");
    let mut sorted = centres.clone();
    sorted.sort_by(f64::total_cmp);
    let step = sorted[1] - sorted[0];
    assert!(
        step > 10.0 * MATCH_TOL_HZ,
        "[{T586}] centres {step:.0} Hz apart: too close for a {MATCH_TOL_HZ} Hz match tolerance"
    );
    eprintln!(
        "[{T586}] fixture: {} centres {:?} MHz, {step:.0} Hz apart",
        centres.len(),
        sorted.iter().map(|f| f / 1e6).collect::<Vec<_>>()
    );
}
