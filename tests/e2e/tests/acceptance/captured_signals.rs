//! **The "all captured signals decode" set** (T-936): one blind acceptance test per assertion,
//! over **every signal the explorer agent has captured off the air**, replayed through the mock
//! SDR device.
//!
//! ```text
//! cargo nextest run -p hk-e2e -E 'binary(acceptance_captured_signals)'                 # the controls
//! cargo nextest run -p hk-e2e -E 'binary(acceptance_captured_signals)' --run-ignored all  # + the red proofs
//! just acceptance-captured-signals
//! ```
//!
//! # The rule the set exists to enforce
//!
//! The user's standing rule (2026-09-25): **every signal the explorer captures becomes a
//! failing-first blind acceptance test.** Not a decoder catalogue and not one fixture per
//! milestone — a *growing set*, so that the honest answer to "does hackriff decode what it hears"
//! is a test result rather than a memory of a good night. This module is the set's first two
//! members, the explorer's 2026-09-25 FM captures (T-935), carrying **three** captured signals
//! between them.
//!
//! **How a later capture joins.** Add its fixture stem to [`CAPTURES`]. Nothing else: the stations
//! come from the fixture's own `hackriff:truth` emissions, every assertion below iterates the whole
//! set, and each failure names the station it belongs to. A capture with two emissions contributes
//! two signals to every test. That is the whole point of the shape — the next capture costs one
//! line, so there is no excuse not to add it.
//!
//! # What each test is and is not
//!
//! Blind, through the device interface, exactly as `docs/10 §1.1` requires and as
//! [`signal_087`](super::signal_087) established for `SIGNAL-087`:
//!
//! * The recording is served by the **mock SDR** ([`blind_replay`] → `hk_pipeline::open_mock_replay`),
//!   never fed to the pipeline as a file.
//! * **No frequency, modulation or protocol is passed in.** The plan is `json!({})` — the built-in
//!   chain registry and nothing else — no `demod_freq_hz`, no recipe, no `POST /api/pipelines`, and
//!   the only frequency anything downstream sees is the centre the mock device reports, exactly as
//!   a HackRF reports where it is tuned. The device is given **no controls at all**, which every
//!   test checks by asserting the mock's `control_changes` is 0.
//! * The truth is stripped from the copy the device serves and sealed in a `TruthVault`; it is read
//!   **after** the run, only to check the answer. Every test prints everything the run produced
//!   ([`report`]) before it opens the truth, so a red is always read beside what the system did see.
//!
//! # Red by design, and each red names its ticket
//!
//! T-936 is phase 2 of the user's three-phase pattern (research/capture → failing tests → make them
//! pass), so most of this file is **expected to fail today**. Each red is `#[ignore]`d — so one
//! known-red proof does not pin a gate red — and its `#[ignore]` names the ticket whose definition
//! of done includes deleting that line:
//!
//! | Test | Ticket assertion | Red today because | Turned green by |
//! |---|---|---|---|
//! | [`a_every_captured_signal_is_detected_blind_as_a_time_frequency_region`] | (1), detection half | — **green control** | — |
//! | [`b_a_sensible_fm_broadcast_explanation_ranks_without_identifying`] | (5) | — **green control** | — |
//! | [`c_the_dynamic_ps_frame_the_oracle_read_is_decoded`] | (4), PS half | — **green control** | — |
//! | [`d_each_captured_signal_is_one_region_of_the_right_width_and_extent`] | (1), region half | 98.9 MHz is **three** inventory regions (98.9127 + 98.9163 + 98.9494 MHz); 101.3's box is 126.6 kHz of a 200 kHz channel, 98.9's is 266.3 kHz; 98.1's region is presence-1.0 s of a 5 s continuous carrier | T-937 (fragmentation), T-940 (on-air read ended) |
//! | [`e_the_carrier_centre_is_refined_not_left_in_the_detector_bin`] | (1), centre half | 98.9 is recorded 12.68 kHz high and 98.1 12.59 kHz low — 2.7 detector bins, `center_source: "detected"`; only 101.3 says `"refined"` | T-938 (centre refinement from pilot/discriminator) |
//! | [`f_the_wfm_rds_chain_auto_attaches_to_every_captured_signal`] | (3) | 98.1 MHz gets **no** demodulation at all: only the two strongest stations were given the chain | T-926 (auto-attach across all detections) |
//! | [`g_every_captured_signal_is_classified_wfm_with_its_pilot_measured`] | (2) | 98.1 MHz: `family = None`, no pilot, no estimated parameters | T-926 |
//! | [`h_the_rds_pi_is_decoded_where_the_truth_has_one_and_pilot_without_pi_where_it_does_not`] | (4), PI half | 98.1 MHz is not reported as *pilot without PI*; it is not reported as anything | T-926 |
//!
//! **The three green controls are the reason the five reds mean anything.** `a_…` proves the
//! fixture, the mock device and the truth vault are sound; `b_…` proves the explanation path runs;
//! `c_…` proves the decode path reaches this test, PS text and all. If those went red the honest
//! reading would be "the harness is broken", not "the capability is missing" — the distinction
//! `docs/19 §5.4` draws about a capture, applied to a suite.
//!
//! # Where the truth comes from, and why it is not this code's output
//!
//! `fixtures/hackrf/explorer-2026-09-25/README.md`: every `hackriff:truth.rds` block was decoded
//! **independently** by `py/fixtures/rds_ref.py` over each fixture's own 5 s window, not copied
//! from the live app, and the oracle's PI agrees with the explorer's live claim on both stations
//! (`1694`, `A4FF`) and agrees that 98.1 MHz has a locked pilot and no decodable PI. So a station's
//! PI, its pilot frequency and its PS frame are an answer key written by a different decoder, which
//! is what makes assertions (2) and (4) checks rather than a circular re-read of this repo's own
//! RDS chain. The one caveat the fixture states plainly: both captures are **overloaded** (98.4 %
//! and 12.0 % of samples clipped at LNA 32 / VGA 30 / amp on), which is a front-end limit of this
//! location, not a bug — and it makes every assertion here *harder*, never easier.
//!
//! # Tolerances
//!
//! Every bound in this file is **a priori**: derived from the FM broadcast standards, from the
//! detector's own resolution, or carried unchanged from an existing suite. None was read off a run.
//! Each constant says where it comes from; [`report`] prints the measured values beside them so the
//! distance to a bound is visible in the log whether the test is red or green.
//!
//! # The cost of one test per assertion
//!
//! Under nextest every test is its own process, so each of these replays the whole set (≈50 s per
//! 5 s capture on a 28-core box, detection-bound). [`hk_e2e::Checks`] is the usual answer to that
//! and is deliberately **not** used here: `#[ignore]` is per `#[test]`, and this suite's whole
//! purpose is that each ticket assertion can be run, and its red deleted, on its own. Three tests
//! run by default; the five `#[ignore]`d reds cost nothing until someone asks for them.

use std::sync::OnceLock;

use hk_e2e::blind::{matching, truth_emissions};
use hk_e2e::{Fixture, TruthItem};
use hk_model::{
    Demodulation, Detection, FreqRange, IdentityScheme, IdleGap, InventoryEntry, InventoryIdentity,
    InventoryQuery, LinkTarget, Region, Timestamp,
};
use serde_json::Value;

use crate::blind::{BlindRun, BlindSource, TOP_K, assert_truth_found, blind_replay, center_tol_hz};
use crate::common::*;

/// The use case every capture in the set is annotated with.
const SIGNAL_062: &str = "SIGNAL-062";

/// Where the set's fixtures live.
const FIXTURE_DIR: &str = "fixtures/hackrf/explorer-2026-09-25";

/// **The set.** One fixture stem per explorer capture; add a line to enlist a new capture, and its
/// `hackriff:truth` emissions join every assertion below.
const CAPTURES: &[&str] = &["fm-101p3-pi1694", "fm-98p9-piA4FF"];

// -------------------------------------------------------------------------------------------
// A-priori tolerances. Derived before any run; each says from what.
// -------------------------------------------------------------------------------------------

/// How close a **refined** carrier centre must sit to the station's true carrier, Hz.
///
/// Derived from the detector's own resolution, which is the only thing refinement has to beat: at
/// these fixtures' 2.4 Msps the detect STFT is 512 bins, so a bin is **4687.5 Hz** (the run's own
/// `resolution:` line prints it). A centre still further out than one bin has not been refined at
/// all — it is wherever the box landed. For scale, the physics allows far better: 47 CFR 73.1545(b)
/// holds an FM broadcast carrier within ±2000 Hz of its assignment, and this receiver's clock error
/// measured off the fixtures' own 19 kHz pilots is −5.8 to −8.0 ppm, i.e. 0.6–0.8 kHz at 100 MHz.
/// So 5 kHz is a ceiling with room to spare, not a target.
const REFINED_CENTER_TOL_HZ: f64 = 5e3;

/// The occupied bandwidth an FM broadcast station's region must lie within, Hz.
///
/// 47 CFR 73.207 assigns FM channels on a **200 kHz** raster, and 47 CFR 73.317(a)–(b) authorises
/// the emission out to **±120 kHz** before demanding ≥25 dB of attenuation — so 240 kHz is the
/// widest a single station may legally be, and anything wider has swallowed a neighbour. At the
/// other end a stereo station carrying RDS puts a subcarrier at 57 kHz, so its two-sided multiplex
/// alone is ~114 kHz before deviation: a box under 150 kHz has cut the station up rather than
/// measured it (the ticket's "not the 13 narrow fragments the live app produced").
const OBW_MIN_HZ: f64 = 150e3;
/// See [`OBW_MIN_HZ`].
const OBW_MAX_HZ: f64 = 240e3;

/// How close a measured 19 kHz pilot must be to the oracle's, Hz. Carried unchanged from
/// `fm_band_2026_09_15`, where it is two Welch bins of that analysis pass at 12.21 Hz.
const PILOT_TOL_HZ: f64 = 25.0;

/// The share of a station's annotated time extent its region's **presence** must cover.
///
/// Every signal in this set is a broadcast carrier that was radiating for the whole clip (the truth
/// says `duration_s` = the capture's 5 s, and the oracle decoded RDS across the window), so a
/// region that claims a one-second event has lost the emission, not measured it. Half is a coarse
/// floor deliberately far from both sides of what is measured today (1.0 s of 5 s on one station,
/// 4.0 s and 5.0 s on the others) rather than a number fitted between them. Read off presence
/// intervals (docs/07 §2.27), never the emitter's `first_seen`/`last_seen` hull.
const MIN_EXTENT_COVERAGE: f64 = 0.5;

// -------------------------------------------------------------------------------------------
// The set: one blind replay per capture, and one `Station` per captured signal.
// -------------------------------------------------------------------------------------------

/// One capture of the set, replayed blind through the mock SDR.
pub struct CapturedRun {
    /// Fixture stem.
    pub name: &'static str,
    /// Data directory (SQLite, tiles).
    pub dir: TempDir,
    /// Run summary.
    pub summary: hk_pipeline::RunSummary,
    /// The private truth, read only after the run.
    pub fx: Fixture,
    /// `/api/inventory` rows, with their explanations.
    pub api_rows: Vec<Value>,
    /// The inventory at stop (every lifecycle state).
    pub inventory: Vec<InventoryEntry>,
    /// Every detection of the run.
    pub detections: Vec<Detection>,
    /// Controls the mock device was given: **0** is the no-lookup-and-tune evidence.
    pub control_changes: u64,
}

fn replay(name: &'static str) -> Option<CapturedRun> {
    let meta = real_fixture_in(FIXTURE_DIR, name)?;
    let fx = Fixture::load(&meta).unwrap();
    let BlindRun {
        dir,
        summary,
        api_rows,
        mock,
    } = blind_replay(&meta, name, BlindSource::default());
    let repo = repo(&dir.0);
    let detections = repo
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
        .unwrap();
    let inventory = inventory(&repo, InventoryQuery::default());
    Some(CapturedRun {
        name,
        summary,
        fx,
        api_rows,
        inventory,
        detections,
        control_changes: mock.iter().map(|m| m.control_changes).sum(),
        dir,
    })
}

/// Every capture of the set that this checkout can replay (LFS data present), replayed once per
/// process.
fn runs() -> &'static [CapturedRun] {
    static RUNS: OnceLock<Vec<CapturedRun>> = OnceLock::new();
    RUNS.get_or_init(|| CAPTURES.iter().filter_map(|n| replay(n)).collect())
}

/// One captured signal: a truth emission and the run it was captured in.
pub struct Station {
    /// The run that replayed it.
    pub run: &'static CapturedRun,
    /// Its private truth.
    pub truth: &'static TruthItem,
}

impl Station {
    /// Fixture and station, for a message.
    fn id(&self) -> String {
        format!(
            "{} / {}",
            self.run.name,
            self.truth.label.as_deref().unwrap_or("<unlabelled>")
        )
    }

    /// The inventory regions the run put on this station's channel, by the suite's shared matching
    /// rule ([`center_tol_hz`]: half the 200 kHz channel here, and extents must overlap).
    fn regions(&self) -> Vec<&'static InventoryEntry> {
        matching(
            self.truth,
            0.0,
            &self.run.inventory,
            |e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz),
            center_tol_hz(self.truth),
        )
    }

    /// The demodulations linked to one of this station's regions, with the region's centre.
    fn demods(&self) -> Vec<(f64, Demodulation)> {
        let repo = repo(&self.run.dir.0);
        let mut out = Vec::new();
        for e in self.regions() {
            for l in repo.emitter_links(e.emitter.id).unwrap() {
                if let LinkTarget::Demodulation(id) = l.target {
                    out.push((e.emitter.f_center_hz, repo.demodulation(id).unwrap()));
                }
            }
        }
        out
    }

    /// Seconds this region was recorded as present inside the station's annotated extent, read off
    /// presence intervals.
    fn presence_s(&self, e: &InventoryEntry) -> f64 {
        let (a, b) = self.truth_window();
        repo(&self.run.dir.0)
            .presence_intervals(e.emitter.id, IdleGap::continuous(), ever().end)
            .unwrap()
            .iter()
            .map(|p| {
                let lo = p.time.start.as_unix_nanos().max(a.as_unix_nanos());
                let hi = p.time.end.as_unix_nanos().min(b.as_unix_nanos());
                ((hi - lo).max(0)) as f64 * 1e-9
            })
            .sum()
    }

    /// The station's annotated extent in the run's clock.
    fn truth_window(&self) -> (Timestamp, Timestamp) {
        let t0 = crate::blind::recording_start(&self.run.fx);
        (
            t0.saturating_add_nanos((self.truth.t_start_s * 1e9) as i64),
            t0.saturating_add_nanos((self.truth.t_end_s * 1e9) as i64),
        )
    }

    /// The top-[`TOP_K`] explanation services of each region, from `/api/inventory`.
    fn explanations(&self) -> Vec<(f64, Vec<String>, Value)> {
        self.regions()
            .iter()
            .filter_map(|e| {
                self.run
                    .api_rows
                    .iter()
                    .find(|r| {
                        (r["f_center_hz"].as_f64().unwrap_or(f64::NAN) - e.emitter.f_center_hz)
                            .abs()
                            < 1.0
                    })
                    .map(|r| {
                        let services = r["explanations"]
                            .as_array()
                            .map(|x| {
                                x.iter()
                                    .take(TOP_K)
                                    .filter_map(|e| e["service"].as_str().map(str::to_owned))
                                    .collect()
                            })
                            .unwrap_or_default();
                        (e.emitter.f_center_hz, services, r["explanations"].clone())
                    })
            })
            .collect()
    }

    /// The RDS identity the run put on one of this station's regions, if any.
    fn decoded_pi(&self) -> Option<(f64, String)> {
        self.regions().iter().find_map(|e| match &e.identity {
            InventoryIdentity::Clear { identity, .. }
                if identity.scheme == IdentityScheme::RdsPi =>
            {
                Some((e.emitter.f_center_hz, identity.value.clone()))
            }
            _ => None,
        })
    }
}

/// Every captured signal of the set. Empty when no fixture's LFS data is present, which is the
/// one case a test may return without asserting (`HK_REQUIRE_FIXTURES=1` makes that a failure
/// instead, in `real_fixture_in`).
fn set() -> Vec<Station> {
    runs()
        .iter()
        .flat_map(|run| {
            truth_emissions(&run.fx)
                .into_iter()
                .map(move |truth| Station { run, truth })
        })
        .collect()
}

/// Everything the set produced, printed **before** any truth is read: every failure message below
/// is read together with this, so a red always shows what the system saw.
fn report(set: &[Station]) {
    for run in runs() {
        let c = &run.summary.counters["chains"];
        eprintln!(
            "[{SIGNAL_062}] {}: {} detections, {} inventory rows, {} chains attached, \
             {} demodulations, {} decodes, {} mode rejected, {} device controls, {} lost samples",
            run.name,
            run.detections.len(),
            run.inventory.len(),
            c["attached"],
            c["demodulations"],
            c["decodes"],
            c["mode_rejected"],
            run.control_changes,
            run.summary.always_on_lost_samples,
        );
    }
    for st in set {
        eprintln!("[{SIGNAL_062}] {}: regions on this channel:", st.id());
        for e in st.regions() {
            let demods: Vec<String> = st
                .demods()
                .iter()
                .filter(|(f, _)| (f - e.emitter.f_center_hz).abs() < 1.0)
                .map(|(_, d)| {
                    format!(
                        "{} pilot {:?} lock {:?} bw {:?}",
                        d.mode, d.params.pilot_hz, d.lock_quality, d.params.bandwidth_hz
                    )
                })
                .collect();
            eprintln!(
                "  {:.4} MHz bw {:.1} kHz {:?} family {:?} identity {:?} present {:.2} s; demods {:?}",
                e.emitter.f_center_hz / 1e6,
                e.emitter.bandwidth_hz / 1e3,
                e.lifecycle,
                e.family,
                match &e.identity {
                    InventoryIdentity::Clear { identity, .. } =>
                        Some(format!("{:?} {}", identity.scheme, identity.value)),
                    _ => None,
                },
                st.presence_s(e),
                demods,
            );
        }
    }
}

/// The set, or `None` when no fixture is fetched (skip).
fn armed() -> Option<Vec<Station>> {
    let set = set();
    if set.is_empty() {
        eprintln!("SKIP {SIGNAL_062}: no capture of the set has its LFS data fetched");
        return None;
    }
    report(&set);
    Some(set)
}

/// The no-lookup-and-tune guard every test runs first: the device was never commanded, so nothing
/// in the run can have tuned to a frequency a test knew.
fn assert_nothing_was_commanded() {
    for run in runs() {
        assert_eq!(
            run.control_changes, 0,
            "[{SIGNAL_062}] {}: the mock device was given {} control change(s); this suite passes \
             no frequency in and commands no tune",
            run.name, run.control_changes
        );
        assert_eq!(
            run.summary.always_on_lost_samples, 0,
            "[{SIGNAL_062}] {}: the always-on readers lost samples, so any red below could be \
             missing data rather than a missing capability",
            run.name
        );
    }
}

// -------------------------------------------------------------------------------------------
// (a) THE HARNESS CONTROL. Green today and must stay green.
// -------------------------------------------------------------------------------------------

/// **Ticket assertion (1), detection half: every captured signal is detected blind as a
/// time–frequency region.**
///
/// What a passing run proves: for each of the set's stations the run found energy at the station's
/// centre, overlapping its bandwidth, inside its time extent, from the IQ alone — a `docs/07`
/// Detection with a start and an end, not a carrier on a list — with the truth sealed until after
/// the run and the device never commanded.
///
/// **This is the suite's first control**, and it also checks the *answer key* rather than only the
/// run: each station's truth must carry a usable carrier, a pilot verdict and an RDS block decoded
/// by the named oracle, because a red from an empty or malformed fixture teaches nothing. It is why
/// the five reds below mean "the capability is missing" rather than "the fixture is empty or the
/// harness is broken". Keep it green.
#[test]
fn a_every_captured_signal_is_detected_blind_as_a_time_frequency_region() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    for run in runs() {
        assert_truth_found(SIGNAL_062, &run.dir.0, &run.fx, 0.0, false);
    }
    // The answer key is usable: a carrier inside the capture's own band, an explicit pilot verdict,
    // and an RDS block that names the independent oracle that wrote it.
    for st in &set {
        let f = st.truth.center_hz();
        let centre = st.run.fx.center_hz_at(0).unwrap();
        let half = 0.5 * st.run.fx.sample_rate;
        assert!(
            (f - centre).abs() <= half && st.truth.bandwidth_hz() > 0.0,
            "[{SIGNAL_062}] {}: annotated carrier {f} Hz is not inside the capture's band",
            st.id()
        );
        assert!(
            st.truth.bool("/pilot/present").is_some(),
            "[{SIGNAL_062}] {}: the truth records no pilot verdict",
            st.id()
        );
        let decoder = st.truth.str("/rds/decoder").unwrap_or_default();
        assert!(
            decoder.contains("rds_ref.py"),
            "[{SIGNAL_062}] {}: the RDS truth was not written by the independent oracle              (decoder {decoder:?})",
            st.id()
        );
        assert!(
            st.truth.t_end_s > st.truth.t_start_s,
            "[{SIGNAL_062}] {}: the annotated emission has no time extent",
            st.id()
        );
    }
    eprintln!(
        "[{SIGNAL_062}] {} captured signal(s) in the set, all detected blind",
        set.len()
    );
}

// -------------------------------------------------------------------------------------------
// (b) THE EXPLANATION CONTROL. Green today and must stay green.
// -------------------------------------------------------------------------------------------

/// **Ticket assertion (5): a sensible explanation ranks in the top suggestions, without
/// identifying the station.**
///
/// What a passing run proves: each captured signal reaches `/api/inventory` with an **FM broadcast
/// allocation** among its top-[`TOP_K`] explanations — the band plan offering a ranked, reasoned
/// suggestion *after* blind detection — and that the suggestion stays an allocation: no explanation
/// names the station's identity. The identity the run does hold comes from the RDS decode
/// (`IdentityScheme::RdsPi`), never from a database, which is the ADR-0017 rule that the known-signal
/// database is an explainer and never a source of truth.
///
/// The PI-substring check looks only at the explanation's **string** values, so a number that
/// happens to contain the hex digits cannot fake it.
#[test]
fn b_a_sensible_fm_broadcast_explanation_ranks_without_identifying() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for st in &set {
        let expl = st.explanations();
        let ranked = expl
            .iter()
            .any(|(_, services, _)| services.iter().any(|s| s == "fm-broadcast"));
        if !ranked {
            failures.push(format!(
                "{}: no region carries an fm-broadcast allocation in its top-{TOP_K}: {:?}",
                st.id(),
                expl.iter()
                    .map(|(f, s, _)| (f / 1e6, s))
                    .collect::<Vec<_>>()
            ));
        }
        // Nothing in the ranking may name the station. The truth's PI is the identity here.
        if let Some(pi) = st.truth.str("/rds/pi_hex") {
            for (f, _, raw) in &expl {
                let mut strings = Vec::new();
                collect_strings(raw, &mut strings);
                if let Some(hit) = strings
                    .iter()
                    .find(|s| s.to_ascii_uppercase().contains(&pi.to_ascii_uppercase()))
                {
                    failures.push(format!(
                        "{}: the explanation at {:.4} MHz names the station identity ({pi}) in \
                         {hit:?}: the database is explaining, not identifying",
                        st.id(),
                        f / 1e6
                    ));
                }
            }
        }
        // An identity, if there is one, is a decode's and not a band plan's.
        for e in st.regions() {
            if let InventoryIdentity::Clear { identity, .. } = &e.identity {
                assert_eq!(
                    identity.scheme,
                    IdentityScheme::RdsPi,
                    "[{SIGNAL_062}] {}: identity scheme {:?} at {:.4} MHz came from something \
                     other than the RDS decode",
                    st.id(),
                    identity.scheme,
                    e.emitter.f_center_hz / 1e6
                );
            }
        }
    }
    assert!(failures.is_empty(), "[{SIGNAL_062}] {failures:#?}");
}

/// Every string value inside `v`, recursively (keys excluded).
fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

// -------------------------------------------------------------------------------------------
// (c) THE DECODE-PATH CONTROL. Green today and must stay green.
// -------------------------------------------------------------------------------------------

/// **Ticket assertion (4), PS half: the PS text the independent oracle read is decoded, and none is
/// invented where the oracle found none.**
///
/// What a passing run proves: the RDS chain's group decode reaches this test with its **content**,
/// not only an identity — `py/fixtures/rds_ref.py` read one complete PS frame, `"Animals "`, in
/// 101.3 MHz's 5 s window (the station's PS is a scrolling song title, so that fragment *is* the
/// window's truth), and this run's `rds-pi` / `rds-group-0-ps-frame` decodes must carry the same
/// text. Where the oracle found no PS (98.9 MHz: 3 CRC-valid groups, no complete frame) the run
/// must claim none.
///
/// **This is the suite's third control**: it is the one test that reads a `Decode`'s metadata, so
/// if the decode path stopped reaching the inventory the reds below would be ambiguous. Keep it
/// green. RT is not asserted — the fixtures' truth records no `rt` field, only a comment, so there
/// is no answer key for it.
#[test]
fn c_the_dynamic_ps_frame_the_oracle_read_is_decoded() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for st in &set {
        let Some(pi) = st.truth.str("/rds/pi_hex") else {
            continue;
        };
        let decodes = repo(&st.run.dir.0)
            .decodes_for_identity(&hk_model::DecodedIdentity {
                scheme: IdentityScheme::RdsPi,
                value: pi.to_owned(),
            })
            .unwrap();
        let seen: Vec<String> = decodes
            .iter()
            .filter_map(|d| d.metadata.get("ps").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        eprintln!(
            "[{SIGNAL_062}] {}: {} decode(s) for PI {pi}, PS {seen:?} (oracle {:?})",
            st.id(),
            decodes.len(),
            st.truth.str("/rds/ps")
        );
        match st.truth.str("/rds/ps") {
            Some(want) => {
                if !seen.iter().any(|s| s == want) {
                    failures.push(format!(
                        "{}: the oracle read PS {want:?} in this window; the run decoded {seen:?}",
                        st.id()
                    ));
                }
            }
            None => {
                if !seen.is_empty() {
                    failures.push(format!(
                        "{}: the oracle read no complete PS frame in this window, but the run \
                         claims {seen:?}",
                        st.id()
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "[{SIGNAL_062}] {failures:#?}");
}

// -------------------------------------------------------------------------------------------
// (d)–(g) THE RED PROOFS.
// -------------------------------------------------------------------------------------------

/// **Ticket assertion (1), region half: each captured signal is ONE time–frequency region, of the
/// width and extent the emission actually has.**
///
/// What a passing run would prove: the Explore lists show the user *one* box per station — one
/// `docs/07` Emitter on the 200 kHz channel, its occupied bandwidth inside the legal mask
/// ([`OBW_MIN_HZ`]–[`OBW_MAX_HZ`]), present for at least half the extent it was radiating over.
/// Not the fragments the live app produced, and not two overlapping boxes, which ADR-0017 calls an
/// error signal: real emissions do not overlap in time–frequency, so two boxes on one station are
/// proof the analysis is wrong.
///
/// What its red message tells T-937 (and T-940) to build. Measured 2026-09-25 on this set:
/// * **98.9 MHz is three regions** — Confirmed 98.9127 MHz / 266.3 kHz, Candidate 98.9163 MHz /
///   140.6 kHz (overlapping it) and Candidate 98.9494 MHz / 9.4 kHz. One station, three rows.
/// * **Widths are wrong in both directions**: 101.3 MHz is boxed at 126.6 kHz — narrower than the
///   stereo multiplex it decoded RDS from at 57 kHz — and 98.9 MHz at 266.3 kHz, wider than the
///   240 kHz the FCC mask allows one station.
/// * **98.1 MHz is present for 1.0 s of a 5 s continuous carrier** (T-940: on-air stations read
///   ended), while 98.9's Confirmed row stops at exactly 4.0 s, the built-in chain's `window_s`.
#[test]
#[ignore = "T-936 PROVES THE GAP AND IS EXPECTED TO FAIL. `#[ignore]`d only so one known-red \
            proof does not pin a gate red; deleting this line is part of T-937's definition of \
            done (with T-940 for the time extent). Run it: cargo nextest run -p hk-e2e \
            -E 'binary(acceptance_captured_signals)' --run-ignored all"]
fn d_each_captured_signal_is_one_region_of_the_right_width_and_extent() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for st in &set {
        let regions = st.regions();
        if regions.len() != 1 {
            failures.push(format!(
                "{}: {} regions on one station's channel, not 1: {:?}",
                st.id(),
                regions.len(),
                regions
                    .iter()
                    .map(|e| (
                        e.emitter.f_center_hz / 1e6,
                        e.emitter.bandwidth_hz / 1e3,
                        e.lifecycle
                    ))
                    .collect::<Vec<_>>()
            ));
        }
        // Width and extent are asked of the region that *is* the station — the one closest to its
        // carrier. Judging the fragments too would only repeat the count failure above in other
        // words; the capability is "one region, and it is the right shape".
        let truth_hz = st.truth.center_hz();
        let best = regions
            .iter()
            .min_by(|a, b| {
                (a.emitter.f_center_hz - truth_hz)
                    .abs()
                    .total_cmp(&(b.emitter.f_center_hz - truth_hz).abs())
            })
            .copied();
        for e in best.iter() {
            let bw = e.emitter.bandwidth_hz;
            if !(OBW_MIN_HZ..=OBW_MAX_HZ).contains(&bw) {
                failures.push(format!(
                    "{}: region at {:.4} MHz is {:.1} kHz wide, outside the {:.0}-{:.0} kHz an FM \
                     broadcast station occupies",
                    st.id(),
                    e.emitter.f_center_hz / 1e6,
                    bw / 1e3,
                    OBW_MIN_HZ / 1e3,
                    OBW_MAX_HZ / 1e3
                ));
            }
            let extent_s = st.truth.t_end_s - st.truth.t_start_s;
            let present = st.presence_s(e);
            if present < MIN_EXTENT_COVERAGE * extent_s {
                failures.push(format!(
                    "{}: region at {:.4} MHz is present {present:.2} s of the {extent_s:.2} s this \
                     carrier was radiating ({:.0} % < {:.0} %)",
                    st.id(),
                    e.emitter.f_center_hz / 1e6,
                    100.0 * present / extent_s,
                    100.0 * MIN_EXTENT_COVERAGE
                ));
            }
        }
    }
    assert!(failures.is_empty(), "[{SIGNAL_062}] {failures:#?}");
}

/// **Ticket assertion (1), centre half: the carrier centre is refined from the signal, not left in
/// the detector bin the box landed in.**
///
/// What a passing run would prove: the run measured each station's carrier to better than its own
/// [`REFINED_CENTER_TOL_HZ`] — which the pilot and the discriminator can do to a few hundred hertz
/// — so a "150 kHz off raster" flag means the *station* is off its assignment rather than the
/// receiver being vague, which is the mismatch ADR-0017 asks to flag rather than snap.
///
/// What its red message tells T-938 to build. Measured 2026-09-25: 98.9 MHz is recorded at
/// 98.9127 MHz (**+12.68 kHz**, 2.7 detector bins) and 98.1 MHz at 98.0874 MHz (**−12.59 kHz**),
/// both with `center_source: "detected"` in their raster evidence, while 101.3 MHz — the one
/// station that got the WFM chain and a pilot lock — says `"refined"` and lands 0.47 kHz out. The
/// gap is not the estimator: it is that only a station with an attached chain gets one.
#[test]
#[ignore = "T-936 PROVES THE GAP AND IS EXPECTED TO FAIL. `#[ignore]`d only so one known-red \
            proof does not pin a gate red; deleting this line is part of T-938's definition of \
            done. Run it: cargo nextest run -p hk-e2e \
            -E 'binary(acceptance_captured_signals)' --run-ignored all"]
fn e_the_carrier_centre_is_refined_not_left_in_the_detector_bin() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for st in &set {
        let truth_hz = st.truth.center_hz();
        let best = st
            .regions()
            .iter()
            .map(|e| e.emitter.f_center_hz)
            .min_by(|a, b| (a - truth_hz).abs().total_cmp(&(b - truth_hz).abs()));
        match best {
            None => failures.push(format!("{}: no region at all", st.id())),
            Some(f) => {
                eprintln!(
                    "[{SIGNAL_062}] {}: closest centre {:.4} MHz, {:+.2} kHz from the carrier",
                    st.id(),
                    f / 1e6,
                    (f - truth_hz) / 1e3
                );
                if (f - truth_hz).abs() > REFINED_CENTER_TOL_HZ {
                    failures.push(format!(
                        "{}: closest region centre is {:+.2} kHz from the carrier, over the \
                         {:.1} kHz one detector bin allows",
                        st.id(),
                        (f - truth_hz) / 1e3,
                        REFINED_CENTER_TOL_HZ / 1e3
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "[{SIGNAL_062}] {failures:#?}");
}

/// **Ticket assertion (3): the WFM+RDS chain auto-attaches to every captured signal, with nobody
/// asked.**
///
/// What a passing run would prove: the user's rule — RDS decodes across the whole FM band without
/// selecting a station (T-926) — holds for every station the run detected, not just the strongest:
/// each region carries a `Demodulation` the run attached **itself**, from the built-in registry,
/// with no recipe posted, no chain override in the plan (`json!({})`) and no control ever sent to
/// the device ([`assert_nothing_was_commanded`]). A chain that attached and then *declined* the
/// window still satisfies this test — declining is an honest measurement and is recorded as one
/// (`hk_demod::record::write_declined`); never attaching is the gap.
///
/// What its red message tells T-926 to build. Measured 2026-09-25: of three captured stations, two
/// get a WFM demodulation (101.3 and 98.9) and **98.1 MHz gets none at all** — 2 demodulations on
/// the run, 3 mode-emitters withheld, 7 classify chains refused at the cap. The chain is attached
/// per confirmed track, and the third station never reaches one.
#[test]
#[ignore = "T-936 PROVES THE GAP AND IS EXPECTED TO FAIL. `#[ignore]`d only so one known-red \
            proof does not pin a gate red; deleting this line is part of T-926's definition of \
            done. Run it: cargo nextest run -p hk-e2e \
            -E 'binary(acceptance_captured_signals)' --run-ignored all"]
fn f_the_wfm_rds_chain_auto_attaches_to_every_captured_signal() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for st in &set {
        let demods = st.demods();
        eprintln!(
            "[{SIGNAL_062}] {}: {} demodulation(s) attached: {:?}",
            st.id(),
            demods.len(),
            demods
                .iter()
                .map(|(f, d)| (f / 1e6, d.mode.clone()))
                .collect::<Vec<_>>()
        );
        if demods.is_empty() {
            failures.push(format!(
                "{}: no chain attached to this station, so nothing could demodulate or decode it \
                 unprompted",
                st.id()
            ));
        }
    }
    assert!(failures.is_empty(), "[{SIGNAL_062}] {failures:#?}");
}

/// **Ticket assertion (2): every captured signal is classified WFM with its pilot, and its
/// parameters are estimated from the signal.**
///
/// What a passing run would prove: the run measured *each* station's mode and pilot and
/// **persisted them** — the inventory row's family reads `wfm`, and a linked demodulation carries a
/// `pilot_hz` within [`PILOT_TOL_HZ`] of the independent oracle's measurement of that station's
/// 19 kHz pilot, plus a measured occupied bandwidth. `docs/api.md` is explicit that
/// `estimated_params` holds measured values only and never a fabricated default, so absent is
/// honest and absent is also a red here: measuring inside a chain and throwing it away is not the
/// capability.
///
/// What its red message tells T-926 to build. Measured 2026-09-25: 101.3 MHz reads `family = wfm`,
/// pilot 18999.8906 Hz against the oracle's 18999.8903 Hz (0.0003 Hz out), and 98.9 MHz
/// 18999.8215 vs 18999.8485 Hz — both excellent. **98.1 MHz reads `family = None`, no pilot, no
/// parameters**, even though the oracle measured a locked 19 kHz pilot there (18999.8900 Hz). The
/// estimator works; it is not being run on the third station.
#[test]
#[ignore = "T-936 PROVES THE GAP AND IS EXPECTED TO FAIL. `#[ignore]`d only so one known-red \
            proof does not pin a gate red; deleting this line is part of T-926's definition of \
            done. Run it: cargo nextest run -p hk-e2e \
            -E 'binary(acceptance_captured_signals)' --run-ignored all"]
fn g_every_captured_signal_is_classified_wfm_with_its_pilot_measured() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for st in &set {
        if st.truth.bool("/pilot/present") != Some(true) {
            continue; // a captured signal with no pilot in the truth is not a WFM claim to check
        }
        let families: Vec<&str> = st
            .regions()
            .iter()
            .filter_map(|e| e.family.as_deref())
            .collect();
        if !families.contains(&"wfm") {
            failures.push(format!(
                "{}: no region on this station is classified wfm (families {families:?})",
                st.id()
            ));
        }
        let truth_pilot = st.truth.expect_f64("/pilot/frequency_hz");
        let pilots: Vec<f64> = st
            .demods()
            .iter()
            .filter(|(_, d)| d.mode == "wfm")
            .filter_map(|(_, d)| d.params.pilot_hz)
            .collect();
        if pilots.is_empty() {
            failures.push(format!(
                "{}: no WFM demodulation stored a pilot frequency; the oracle measured \
                 {truth_pilot:.4} Hz here",
                st.id()
            ));
        }
        for p in &pilots {
            if (p - truth_pilot).abs() > PILOT_TOL_HZ {
                failures.push(format!(
                    "{}: pilot {p:.4} Hz vs the oracle's {truth_pilot:.4} Hz",
                    st.id()
                ));
            }
        }
        let widths: Vec<f64> = st
            .demods()
            .iter()
            .filter(|(_, d)| d.mode == "wfm")
            .filter_map(|(_, d)| d.params.bandwidth_hz)
            .collect();
        if widths.is_empty() {
            failures.push(format!(
                "{}: no WFM demodulation stored a measured bandwidth",
                st.id()
            ));
        }
    }
    assert!(failures.is_empty(), "[{SIGNAL_062}] {failures:#?}");
}

/// **Ticket assertion (4), PI half: the RDS PI is decoded where the truth has one, and a station
/// with a pilot and no PI is reported as exactly that.**
///
/// What a passing run would prove, in both directions:
/// * every station the oracle read a PI from reaches its **inventory emitter** with that PI as an
///   `IdentityScheme::RdsPi` identity — 1694 at 101.3 MHz, A4FF at 98.9 MHz — so the decode lands on
///   the thing the run detected rather than in a side table;
/// * the station the oracle found a **locked pilot and no decodable PI** at (98.1 MHz, 0 CRC-valid
///   groups in this window) is reported as *pilot without PI*: the pilot measured and stated, and
///   **no identity invented**. Claiming a PI there would be worse than missing one — it is the
///   over-claim the negative controls of `mauto_negatives` guard against in general.
///
/// What its red message tells T-926 to build. Measured 2026-09-25: both PIs decode (this half is
/// already green, and stays asserted so it cannot regress), and the "no invented PI" half is green
/// too. What fails is the *positive* half of pilot-without-PI: 98.1 MHz is not reported as a
/// pilot-without-PI station, because it is not reported as a station at all — no chain, no pilot,
/// nothing for the UI to show but an unexplained candidate.
#[test]
#[ignore = "T-936 PROVES THE GAP AND IS EXPECTED TO FAIL. `#[ignore]`d only so one known-red \
            proof does not pin a gate red; deleting this line is part of T-926's definition of \
            done. Run it: cargo nextest run -p hk-e2e \
            -E 'binary(acceptance_captured_signals)' --run-ignored all"]
fn h_the_rds_pi_is_decoded_where_the_truth_has_one_and_pilot_without_pi_where_it_does_not() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for st in &set {
        let decoded = st.decoded_pi();
        match st.truth.str("/rds/pi_hex") {
            Some(want) => match &decoded {
                Some((f, got)) if got.eq_ignore_ascii_case(want) => eprintln!(
                    "[{SIGNAL_062}] {}: PI {got} on the emitter at {:.4} MHz",
                    st.id(),
                    f / 1e6
                ),
                Some((f, got)) => failures.push(format!(
                    "{}: emitter at {:.4} MHz decoded PI {got}, the oracle read {want}",
                    st.id(),
                    f / 1e6
                )),
                None => failures.push(format!(
                    "{}: the oracle read PI {want} here; no emitter on this station carries an \
                     RDS identity",
                    st.id()
                )),
            },
            None => {
                // Pilot without PI: nothing invented, and the pilot itself reported.
                if let Some((f, got)) = &decoded {
                    failures.push(format!(
                        "{}: the oracle decoded no PI here, but the emitter at {:.4} MHz claims \
                         {got}",
                        st.id(),
                        f / 1e6
                    ));
                }
                if st.truth.bool("/pilot/present") == Some(true) {
                    let pilots: Vec<f64> = st
                        .demods()
                        .iter()
                        .filter_map(|(_, d)| d.params.pilot_hz)
                        .collect();
                    if pilots.is_empty() {
                        failures.push(format!(
                            "{}: the oracle measured a locked pilot at {:.4} Hz and no PI; the run \
                             reports no pilot, so this station is not reported as \
                             pilot-without-PI — it is not reported at all",
                            st.id(),
                            st.truth.expect_f64("/pilot/frequency_hz")
                        ));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "[{SIGNAL_062}] {failures:#?}");
}
