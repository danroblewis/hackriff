//! T-401: **time-to-outcome**, made assertable, and asserted on the capture clock.
//!
//! # Why this module exists
//!
//! Every other acceptance test in this suite inspects **terminal state**: it replays a whole
//! fixture and then looks at the final inventory. A 40 s time-to-confirm and a 1 s time-to-confirm
//! leave an identical inventory at the end of a 60 s recording, so **the suite was blind to
//! latency by construction** — T-398 found continuous emitters confirming at 60.02 s, the end of
//! the recording, having met the rule's thresholds at about 2.5 s, and every test passed. The
//! user found it by looking at the screen.
//!
//! [`Latencies`] is the generalisation of the shape T-398 built for confirmation alone: the four
//! intervals a user actually feels, read off the durable records the pipeline already writes, and
//! expressed against the **capture clock**.
//!
//! 1. **time-to-first-detection** — the emitter's first sighting.
//! 2. **time-to-family-assignment** — the first classification that is not `unknown`.
//! 3. **time-to-Confirmed** — the lifecycle change into [`LifecycleState::Confirmed`].
//! 4. **time-to-first-decode** — the point in the capture by which a CRC-valid decode had been
//!    attached to the emitter.
//!
//! # Why the capture clock, and why that makes these bounds load-proof
//!
//! This repo already has timing tests that pass alone and fail with three other agents building
//! (T-344, T-383). A suite full of flaky timing tests is worse than one blind to latency, because
//! people learn to re-run red.
//!
//! So every number here is a difference of two **capture** timestamps — the recording's own
//! `core:datetime` plus a sample offset — never a wall clock. T-398's own baselines separate the
//! two deliberately (`family=wfm` at 1.0 s capture but 3.22 s wall), and that separation is the
//! design: a capture-clock interval is a statement about **when in the signal the system decided**,
//! so it is identical whether the replay runs at 1× or 30×, on an idle machine or under four
//! parallel builds. A host that is twice as slow replays the same samples and writes the same
//! capture timestamps. **Nothing in this module can be made to fail by loading the machine.**
//!
//! The one thing a capture-clock bound cannot see is the host being too slow to keep up at all —
//! on a lossless unpaced replay that is not possible (the device is back-pressured, no samples are
//! dropped), and on a paced live run it shows up as ring overruns, which the existing suites
//! already assert on. Wall-clock latency (how long after a sample arrives the decision is
//! *published*) is deliberately **not** bounded here: it is a property of the machine, and T-398
//! measured it separately for exactly that reason.
//!
//! **T-403 turned this module's one pending assertion into a bound.** Time-to-Confirmed for a
//! station with neither a pilot to lock nor an identity to decode was reported and not asserted,
//! because the continuous-and-trusted route was still weighed on track close alone and that variant
//! confirmed at the end of the recording whatever its length — 14.00 s in a 14 s scene. Bounding it
//! then would have made the suite red for a defect T-401 did not fix, and loosening the bound to
//! fit would have enshrined it. The route is now weighed live, the number is 3.00 s, and all three
//! variants take the same budget.
//!
//! One `REPORTED, NOT ASSERTED` line remains, and it belongs to a different ticket:
//! **family-assignment** for [`Degradation::NoPilotNoRds`]. The generator's WFM is tone-modulated
//! to about 103 kHz, under the 106 kHz lower edge of the wideband-FM occupancy window, so at most
//! seeds nothing names it — T-402's defect, in the scene rather than in the pipeline. The two
//! variants that can reach a family bound it. That is the repo's `truth_report` convention (report
//! the gap, say why, name the owner) applied to latency.
//!
//! # Why a degraded fixture
//!
//! The other half of T-398's finding: every synthetic WFM scene in the suite ships **clean RDS
//! with a perfect PI**, so the identity route always fires and the emission route's limitation is
//! never exercised. Suppressing RDS in a synthetic scene reproduced the live bug. **A suite whose
//! fixtures are all best-case cannot find a fallback path's bug**, so [`Degradation`] makes the
//! degraded scene a first-class, named fixture rather than a one-off: the same station, with the
//! evidence a real weak station lacks removed, so the slow route is the only route left.
//!
//! Blind throughout, as the standing rule requires: the station sits at an offset the run never
//! sees, the pipeline detects whatever it detects, and the emitter is matched to the private truth
//! **after** the run by measured centre and bandwidth.

use std::collections::BTreeSet;
use std::fmt;

use hk_e2e::blind::matching;
use hk_e2e::{Fixture, SynthRequest};
use hk_model::sigmf::Datatype;
use hk_model::{CrcStatus, DecodeId, EmitterId, LifecycleState, LinkTarget, Repository, Timestamp};

use crate::blind::{BlindSource, blind_replay, center_tol_hz, recording_start};
use crate::common::repo;

// ---------------------------------------------------------------------------------------------
// The measurement.

/// Seconds from `t0` to `t`, on the capture clock.
fn since(t0: Timestamp, t: Timestamp) -> f64 {
    (t.as_unix_nanos() - t0.as_unix_nanos()) as f64 / 1e9
}

/// A family label that carries no information: not an assignment.
const UNKNOWN: &str = "unknown";

/// The four time-to-outcome latencies for one emitter, in seconds of **capture** time from a
/// reference instant (normally the recording's start, [`crate::blind::recording_start`]).
///
/// `None` means the outcome never happened in the run, which is a different failure from a slow
/// one and is reported as such.
#[derive(Clone, Debug, Default)]
pub struct Latencies {
    /// Capture time of the emitter's first sighting.
    pub first_detection_s: Option<f64>,
    /// Capture time of the first classification whose family is not `unknown`.
    pub family_s: Option<f64>,
    /// That family.
    pub family: Option<String>,
    /// Capture time of the lifecycle change into Confirmed.
    pub confirmed_s: Option<f64>,
    /// The reason recorded with it — which route confirmed it.
    pub confirmed_reason: Option<String>,
    /// Capture time by which the first CRC-valid decode had been **attached to the emitter** —
    /// the end of the demodulation window the decoder was working over when it linked the decode.
    /// This is the decode latency proper: it moves if the chain attaches late, whereas
    /// [`Self::first_decode_frame_s`] does not.
    pub first_decode_s: Option<f64>,
    /// Capture time of the first CRC-valid decoded **frame** — where in the emission the first
    /// good frame sits, which is a property of the signal, not of the system. Reported for
    /// contrast; never bounded on its own, because a chain that starts late and back-decodes from
    /// the ring would still show a frame at 0 s.
    pub first_decode_frame_s: Option<f64>,
    /// Decodes linked to the emitter, valid or not (0 distinguishes "no decoder ran" from
    /// "the decoder ran and every CRC failed").
    pub decodes: usize,
}

impl Latencies {
    /// Measures `id`'s four latencies from `t0`, reading only durable records: the emitter row,
    /// its classification history, its lifecycle history and its decoder evidence. Nothing here
    /// consults truth, and nothing here is instrumentation added for the test — these are the
    /// same rows the UI reads.
    pub fn measure(r: &Repository, id: EmitterId, t0: Timestamp) -> Self {
        let emitter = r.emitter(id).expect("the matched emitter is readable");
        let classifications = r
            .classification_history(id)
            .expect("classification history is readable");
        let first_family = classifications
            .iter()
            .find(|c| !c.classification.family.eq_ignore_ascii_case(UNKNOWN));
        // T-403: **when** it was confirmed is the first transition into Confirmed; **why** it is
        // confirmed is the latest row, because the reason strengthens as better evidence arrives
        // (`Repository::restate_emitter_lifecycle`). Reading the reason off the first row would
        // report whichever route happened to arrive first — the very thing that made the recorded
        // explanation a function of host load rather than of the signal.
        let lifecycle: Vec<_> = r
            .emitter_lifecycle_history(id)
            .expect("lifecycle history is readable")
            .into_iter()
            .filter(|c| c.state == LifecycleState::Confirmed)
            .collect();
        let confirmed = lifecycle.first();
        let explanation = lifecycle.last();
        let decodes = r
            .decode_evidence_for_emitter(id)
            .expect("decoder evidence is readable");
        let valid: BTreeSet<DecodeId> = decodes
            .iter()
            .filter(|d| d.crc_status == CrcStatus::Valid)
            .map(|d| d.decode_id)
            .collect();
        // When the emitter had a CRC-valid decode in hand: the earliest link of one of those
        // decodes, stamped with the end of the demodulation window it came from (capture clock).
        let attached = r
            .emitter_link_history(id)
            .expect("link history is readable")
            .into_iter()
            .filter_map(|l| match l.link.target {
                LinkTarget::Decode(d) if valid.contains(&d) => Some(l.link.linked_at),
                _ => None,
            })
            .min();
        Self {
            first_detection_s: Some(since(t0, emitter.first_seen)),
            family_s: first_family.map(|c| since(t0, c.classification.t)),
            family: first_family.map(|c| c.classification.family.clone()),
            confirmed_s: confirmed.map(|c| since(t0, c.t)),
            confirmed_reason: explanation.map(|c| c.reason.clone()),
            first_decode_s: attached.map(|t| since(t0, t)),
            first_decode_frame_s: decodes
                .iter()
                .find(|d| d.crc_status == CrcStatus::Valid)
                .map(|d| since(t0, d.t)),
            decodes: decodes.len(),
        }
    }

    /// Asserts `outcome` happened and did so within `budget_s` of capture time; returns it.
    ///
    /// The two failures are deliberately distinct messages: never happening is not a slow system,
    /// it is a missing one.
    pub fn require(&self, tag: &str, outcome: Outcome, budget_s: f64) -> f64 {
        let got = self.get(outcome).unwrap_or_else(|| {
            panic!(
                "[{tag}] {outcome} never happened in this run, so its latency is unbounded, not \
                 slow. Measured: {self}"
            )
        });
        assert!(
            got >= 0.0,
            "[{tag}] {outcome} at {got:.2} s cannot precede the capture it was decided from. \
             Measured: {self}"
        );
        assert!(
            got <= budget_s,
            "[{tag}] time-to-{outcome} {got:.2} s of capture exceeds the {budget_s:.1} s budget \
             (CLAUDE.md: a region's few real signals resolve in ~2–10 s). This is capture time, \
             so a loaded machine cannot cause it. Measured: {self}"
        );
        got
    }

    /// One latency, or `None` if that outcome never happened.
    pub fn get(&self, outcome: Outcome) -> Option<f64> {
        match outcome {
            Outcome::FirstDetection => self.first_detection_s,
            Outcome::Family => self.family_s,
            Outcome::Confirmed => self.confirmed_s,
            Outcome::FirstDecode => self.first_decode_s,
        }
    }
}

impl fmt::Display for Latencies {
    /// One line, always printed by every latency test, so a run that still passes but has slowed
    /// leaves the number in the log rather than only in a failure.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = |v: Option<f64>| match v {
            Some(v) => format!("{v:.2}s"),
            None => "never".to_owned(),
        };
        write!(
            f,
            "capture-clock latencies: detect {}, family {} ({}), confirm {} ({}), decode {} \
             (first good frame at {}; {} decode(s) linked)",
            s(self.first_detection_s),
            s(self.family_s),
            self.family.as_deref().unwrap_or("-"),
            s(self.confirmed_s),
            self.confirmed_reason.as_deref().unwrap_or("-"),
            s(self.first_decode_s),
            s(self.first_decode_frame_s),
            self.decodes,
        )
    }
}

/// One of the four outcomes a latency is measured to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The emitter's first sighting.
    FirstDetection,
    /// The first family that is not `unknown`.
    Family,
    /// The lifecycle change into Confirmed.
    Confirmed,
    /// A CRC-valid decode attached to the emitter.
    FirstDecode,
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Outcome::FirstDetection => "first-detection",
            Outcome::Family => "family-assignment",
            Outcome::Confirmed => "Confirmed",
            Outcome::FirstDecode => "first-decode",
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Blind matching.

/// The emitter whose measured extent matches the scene's single truth emission of `kind`, and its
/// measured bandwidth.
///
/// Blind: truth is read **after** the run and only to find which produced row to measure, never to
/// choose a frequency to look at. The widest match is taken, so a skirt fragment inside the
/// station is not mistaken for the station.
pub fn matched_emitter(
    tag: &str,
    fx: &Fixture,
    kind: &str,
    rows: &[serde_json::Value],
) -> (EmitterId, f64) {
    let truth = fx.of_kind(kind);
    assert_eq!(truth.len(), 1, "[{tag}] one truth {kind} in the scene");
    let truth = truth[0];
    let matched = matching(
        truth,
        0.0,
        rows,
        |r| {
            (
                r["f_center_hz"].as_f64().unwrap_or(f64::NAN),
                r["bandwidth_hz"].as_f64().unwrap_or(0.0),
            )
        },
        center_tol_hz(truth),
    );
    assert!(
        !matched.is_empty(),
        "[{tag}] the {kind} was detected blind: truth {:.4}..{:.4} MHz, rows {:?}",
        truth.f_lo_hz / 1e6,
        truth.f_hi_hz / 1e6,
        rows.iter()
            .map(|r| (r["f_center_hz"].clone(), r["state"].clone()))
            .collect::<Vec<_>>()
    );
    let row = matched
        .iter()
        .max_by(|a, b| {
            a["bandwidth_hz"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&b["bandwidth_hz"].as_f64().unwrap_or(0.0))
        })
        .unwrap();
    (
        row["id"].as_str().unwrap().parse().unwrap(),
        row["bandwidth_hz"].as_f64().unwrap_or(0.0),
    )
}

// ---------------------------------------------------------------------------------------------
// The scenes: one station, best-case and degraded.

/// Seconds of scene. Long enough that a run which *fails* a budget still finishes and reports a
/// real number rather than timing out, and long enough for the pre-T-398 behaviour (wait for the
/// track to close) to be visibly outside every budget here.
pub const SCENE_S: f64 = 14.0;

/// The station's blind offset from the tuned centre, Hz. The run never sees it.
pub const OFFSET_HZ: f64 = 500e3;

/// How much of a real station's evidence the scene withholds.
///
/// This is the first-class degraded fixture. The suite's WFM scenes were all [`Pristine`]
/// (`Degradation::Pristine`), which is why the confirmation fallback's bug survived every test: a
/// perfect PI decodes in the first second and the identity route always wins, so the emission
/// route is never the route under test. Each variant below removes one kind of evidence, so the
/// route that remains is forced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Degradation {
    /// Best case, and what every other synthetic WFM scene in the suite ships: stereo pilot plus
    /// clean RDS with a perfect PI. Both the identity route and the emission route are available.
    Pristine,
    /// Stereo pilot, **no RDS subcarrier at all**. There is nothing to decode, so no identity
    /// route exists and the emitter must earn its family and its confirmation from what the
    /// pipeline measures off the air. This is the shape of the live station that took 60 s.
    NoRds,
    /// Mono, and no RDS: no 19 kHz pilot to lock and nothing to decode. Neither fast route is
    /// available. The control that keeps the fast routes honest — remove the evidence and the
    /// fast path must disappear.
    NoPilotNoRds,
}

impl Degradation {
    /// A short tag for run directories and messages.
    pub fn tag(self) -> &'static str {
        match self {
            Degradation::Pristine => "pristine",
            Degradation::NoRds => "no-rds",
            Degradation::NoPilotNoRds => "no-pilot-no-rds",
        }
    }

    /// Whether the scene carries a 19 kHz stereo pilot.
    fn stereo(self) -> bool {
        self != Degradation::NoPilotNoRds
    }

    /// RDS subcarrier deviation, Hz; 0 suppresses RDS entirely.
    fn rds_deviation_hz(self) -> f64 {
        match self {
            Degradation::Pristine => 2000.0,
            Degradation::NoRds | Degradation::NoPilotNoRds => 0.0,
        }
    }
}

/// One WFM broadcast station at [`OFFSET_HZ`] from the tuned centre, degraded per `how`.
///
/// Identical in every other respect across the variants, so a latency difference between two runs
/// of this scene is attributable to the withheld evidence and nothing else.
pub fn wfm_scene(seed: u64, how: Degradation) -> SynthRequest {
    SynthRequest::new("fm_broadcast_rds")
        .seed(seed)
        .datatype(Datatype::Cf32Le)
        .param("sample_rate", 2.4e6)
        .param("center_hz", 100.8e6)
        .param("offset_hz", OFFSET_HZ)
        .param("duration_s", SCENE_S)
        .param("power_dbfs", -16.0)
        .param("noise_dbfs", -60.0)
        .param("stereo", how.stereo())
        .param("rds_deviation_hz", how.rds_deviation_hz())
}

// ---------------------------------------------------------------------------------------------
// Running one.

/// A measured run of [`wfm_scene`]: the latencies of the station the run found, matched blind.
pub struct Measured {
    /// The four latencies, capture clock, from the recording's start.
    pub latencies: Latencies,
    /// Measured bandwidth of the matched emitter, Hz.
    pub bandwidth_hz: f64,
    /// Keeps the run's data directory alive while the caller reads it.
    pub _dir: crate::common::TempDir,
}

/// Generates `how`'s scene at `seed`, replays it blind through the mock SDR device, matches the
/// station by measured extent afterwards and measures its four latencies.
pub fn run_wfm(tag: &str, seed: u64, how: Degradation) -> Option<Measured> {
    let out = match SynthRequest::generate(&wfm_scene(seed, how)) {
        Ok(out) => out,
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {tag}: {e}");
            return None;
        }
        Err(e) => panic!("[{tag}] synthetic scenario generation failed: {e}"),
    };
    let fx = out.fixture(0).unwrap();
    let t0 = recording_start(&fx);
    let run = blind_replay(&out.recordings[0], tag, BlindSource::default());
    let (id, bandwidth_hz) = matched_emitter(tag, &fx, "wfm-broadcast", &run.api_rows);
    let latencies = Latencies::measure(&repo(&run.dir.0), id, t0);
    eprintln!(
        "[{tag}] {} WFM ({:.0} kHz wide): {latencies}",
        how.tag(),
        bandwidth_hz / 1e3
    );
    Some(Measured {
        latencies,
        bandwidth_hz,
        _dir: run.dir,
    })
}

// ---------------------------------------------------------------------------------------------
// The budgets.

/// Longest a strong, unambiguous emission may take to be **detected at all**, s of capture.
/// Detection is the cheapest outcome and gates the other three, so it gets the tightest budget.
pub const DETECT_BUDGET_S: f64 = 2.0;

/// Longest a strong, unambiguous emission may take to be given a **family**, s of capture. This
/// is the number the user reads off the screen as "the system knows what this is".
pub const FAMILY_BUDGET_S: f64 = 4.0;

/// Longest a strong, unambiguous emission may take to reach **Confirmed**, s of capture. The
/// CLAUDE.md invariant says a region's few real signals resolve in ~2–10 s; this takes the fast
/// half, because the evidence (a locked pilot inside a WFM-width emission, or a decoded PI) is
/// available about a second in.
pub const CONFIRM_BUDGET_S: f64 = 5.0;

/// Longest a clean RDS subcarrier may take to yield its **first CRC-valid group**, s of capture.
/// RDS runs at 1187.5 bit/s in 104-bit groups, so a group is 88 ms of air; the rest is the
/// chain attaching, the PLL locking and the bit sync settling.
pub const DECODE_BUDGET_S: f64 = 6.0;

// ---------------------------------------------------------------------------------------------
// The tests.

const T401: &str = "T-401";
const T403: &str = "T-403";

/// Best case — the scene shape every other synthetic WFM test in the suite uses. All four
/// outcomes are bounded, including the decode, which only this variant can reach.
#[test]
fn t401_a_clean_wfm_station_reaches_every_outcome_within_budget() {
    let Some(m) = run_wfm("t401-pristine", 4011, Degradation::Pristine) else {
        return;
    };
    let l = &m.latencies;
    l.require(T401, Outcome::FirstDetection, DETECT_BUDGET_S);
    l.require(T401, Outcome::Family, FAMILY_BUDGET_S);
    l.require(T401, Outcome::Confirmed, CONFIRM_BUDGET_S);
    l.require(T401, Outcome::FirstDecode, DECODE_BUDGET_S);
}

/// The degraded case, and the one that keeps the best case honest: the same station with its RDS
/// removed, so no identity can be decoded and every outcome must be earned from what the pipeline
/// measures off the air.
///
/// This is the shape of the live station T-398 caught confirming at 60.02 s. The suite could not
/// have caught it, because it had no scene like this one.
#[test]
fn t401_a_station_with_no_identity_to_decode_still_resolves_within_budget() {
    let Some(m) = run_wfm("t401-no-rds", 4012, Degradation::NoRds) else {
        return;
    };
    let l = &m.latencies;
    l.require(T401, Outcome::FirstDetection, DETECT_BUDGET_S);
    l.require(T401, Outcome::Family, FAMILY_BUDGET_S);
    l.require(T401, Outcome::Confirmed, CONFIRM_BUDGET_S);
    assert_eq!(
        l.decodes, 0,
        "[{T401}] this scene carries no RDS, so a decode of any kind would mean something read an \
         identity that is not on the air. Measured: {l}"
    );
}

/// T-403: the same station again with **neither** kind of fast evidence — mono, so no pilot to
/// lock, and no RDS, so no identity to decode. It has only continuity and duty cycle to offer, and
/// those are properties a strong receiver artefact shares, so this is the variant where a wrong fix
/// would show up as "confirm anything that stays on".
///
/// Its time-to-Confirmed is bounded here on the same budget as the other two. Until T-403 it could
/// not be: the continuous-and-trusted route was weighed only when a track closed, and a station
/// that never stops transmitting has no close until the idle timeout, so this variant confirmed at
/// the end of the recording whatever its length (14.00 s in this 14 s scene). T-401 reported that
/// number rather than asserting it — the suite's one deliberate unbounded latency.
#[test]
fn t403_a_station_with_neither_pilot_nor_identity_still_resolves_within_budget() {
    let Some(m) = run_wfm("t403-no-pilot-no-rds", 4013, Degradation::NoPilotNoRds) else {
        return;
    };
    let l = &m.latencies;
    l.require(T403, Outcome::FirstDetection, DETECT_BUDGET_S);
    let confirmed = l.require(T403, Outcome::Confirmed, CONFIRM_BUDGET_S);
    // The budget alone would not have caught the defect in a short scene — a close-only decision in
    // a 4 s recording lands inside 5 s by accident. What says the decision tracked the *evidence*
    // is that it came well before the end of the recording, and that its reason names a life still
    // being lived rather than a track that closed.
    assert!(
        confirmed < SCENE_S / 2.0,
        "[{T403}] confirmed at {confirmed:.2} s of a {SCENE_S:.0} s scene: a time-to-Confirmed that \
         tracks the recording length rather than the evidence is the close-only defect, whatever \
         the budget says. Measured: {l}"
    );
    let reason = l.confirmed_reason.as_deref().unwrap_or_default();
    assert!(
        reason.starts_with("continuous") && reason.contains("still on air"),
        "[{T403}] with no pilot and no identity the only route left is the continuous one, weighed \
         live; got: {reason}"
    );
    // A station that never stops transmitting has a duty cycle of 1, and it has it from the first
    // moment it is measured. Reading anything less means the denominator is being charged for
    // capture the detector has not reported on yet — a fixed lag divided by a growing window, so
    // the number climbs towards 1 as a function of how long you watched rather than of what the
    // signal did. That is what put family assignment at 5.00 s on a realistic scene.
    assert!(
        reason.contains("duty cycle 1.00"),
        "[{T403}] a continuously-transmitting station is continuous the first time it is weighed, \
         not after the observation window has amortised the detector's reporting lag; got: {reason}"
    );
    assert_eq!(
        l.decodes, 0,
        "[{T403}] this scene carries no RDS. Measured: {l}"
    );

    // Family: **bounded when it happens, reported with its reason when it does not.** On the
    // generator as it stands the WFM is tone-modulated to about 103 kHz, under the 106 kHz lower
    // edge of `family::WIDEBAND_FM_OBW_HZ`, and the detector measures 66 kHz of it — so at most
    // seeds the occupancy map declines to name it and the chain rejects the mode too. That is
    // T-402's defect, in the scene rather than in the pipeline, and asserting it unconditionally
    // would make this test red for a bug it does not fix.
    //
    // It is *not* a second end-of-recording defect, which is the question T-402's wider scene
    // raised. Run against a composite scaled to the 75 kHz deviation a real limiter enforces
    // (155 kHz measured, 33 analysis bins), this same test reads **family at 1.00 s** and confirm
    // at 3.00 s. Before T-403 it read 14.00 s for both, and for one reason: a continuous carrier
    // got no live sighting at all, so the `track_family` classification a sighting carries — and
    // the confirmation weighed beside it — arrived only when the track closed. One defect, one
    // fix. So the bound below is written to take effect the moment the scene is wide enough to
    // reach a family, with no further edit.
    match l.family_s {
        Some(_) => {
            l.require(T403, Outcome::Family, FAMILY_BUDGET_S);
        }
        None => eprintln!(
            "[{T403}] REPORTED, NOT ASSERTED: no family was ever assigned to a {:.0} kHz emission \
             that is a broadcast FM station — the scene is narrower than the WFM occupancy window \
             (T-402), so neither the occupancy map nor the chain would name it. Confirmation does \
             not depend on it, and the bound above takes effect as soon as the scene is wide \
             enough. Measured: {l}",
            m.bandwidth_hz / 1e3
        ),
    }
}

/// T-403's control, and the one the ticket turns on: **a continuous, unmodulated line must not
/// take the live route however long it stays on.**
///
/// Route C could demand a *lock*, which is positive evidence nothing else produces. Route B has
/// only continuity and duty cycle, and those are exactly what a strong receiver artefact has —
/// this receiver's own 10 MHz reference harmonic is on air for ever at duty cycle 1.00, and so is
/// its LO leakage, its sample-clock comb and every switching-supply tooth. Weighing route B live
/// without a discriminator would confirm all of them as emitters.
///
/// A pure CW tone in white noise is that shape, generated rather than hoped for: continuous for the
/// whole scene, unmodulated, and — because a tone has no occupied bandwidth of its own — no wider
/// than the analysis window can make it look. Nothing in this run may be confirmed by the live
/// continuous route. Blind: the assertion is over every emitter the run produced, and never
/// consults the scene's truth to find one.
///
/// **This scene is where `ConfirmPolicy::min_live_bandwidth_bins` was measured.** Run at four tone
/// powers spanning 28 dB, the widths the detector measured for the line and for the receiver
/// artefacts it produced were, at 4687.5 Hz bins:
///
/// | tone power | measured widths |
/// |---|---|
/// | −30 dBFS | 14.1 kHz ×5, 18.8 kHz |
/// | −16 dBFS | 14.1 kHz ×5, 18.8 kHz |
/// | −6 dBFS  | 9.4 kHz, 14.1 kHz ×3, 18.8 kHz ×2 |
/// | −2 dBFS  | 14.1 kHz ×5, 18.8 kHz |
///
/// — 2 to 4 bins throughout, and **flat in level**: an OBW99 of a windowed tone is a property of
/// the window, not of how strong the tone is, because 99 % of a Hann-windowed tone's energy is
/// inside its 4-bin main lobe at any level (the first sidelobe is −31 dB down). So a line cannot
/// widen its way past the clause by being loud, which is what a threshold picked to fit one
/// measurement would have risked.
#[test]
fn t403_a_continuous_unmodulated_line_never_confirms_live() {
    let request = SynthRequest::new("tone")
        .seed(4031)
        .datatype(Datatype::Cf32Le)
        .param("sample_rate", 2.4e6)
        .param("center_hz", 100.8e6)
        .param("offset_hz", OFFSET_HZ)
        .param("duration_s", SCENE_S)
        .param("power_dbfs", -16.0)
        .param("noise_dbfs", -60.0);
    let out = match SynthRequest::generate(&request) {
        Ok(out) => out,
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP t403-cw-line: {e}");
            return;
        }
        Err(e) => panic!("[{T403}] synthetic scenario generation failed: {e}"),
    };
    let fx = out.fixture(0).unwrap();
    let t0 = recording_start(&fx);
    let run = blind_replay(&out.recordings[0], "t403-cw-line", BlindSource::default());
    let r = repo(&run.dir.0);
    let entries = r
        .query_inventory(&hk_model::InventoryQuery::default())
        .expect("the inventory is readable")
        .entries;
    assert!(
        !entries.is_empty(),
        "[{T403}] a strong CW line must still be *detected* and catalogued — refusing the fast \
         confirmation is not refusing to see it"
    );
    let mut live_confirms = Vec::new();
    for e in &entries {
        let l = Latencies::measure(&r, e.emitter.id, t0);
        eprintln!(
            "[{T403}] CW line entry at {:.4} MHz, {:.1} kHz wide: {l}",
            e.emitter.f_center_hz / 1e6,
            e.emitter.bandwidth_hz / 1e3
        );
        if let Some(reason) = l.confirmed_reason.as_deref()
            && reason.contains("still on air")
        {
            live_confirms.push(format!(
                "{:.4} MHz at {:?} s: {reason}",
                e.emitter.f_center_hz / 1e6,
                l.confirmed_s
            ));
        }
    }
    assert!(
        live_confirms.is_empty(),
        "[{T403}] an unmodulated continuous line took the live continuous route, which is what a \
         receiver's own reference harmonic, LO leakage or clock comb would do — so the route is \
         confirming anything that stays on: {live_confirms:?}"
    );
}
