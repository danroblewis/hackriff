//! **The "all captured signals decode" set** (T-936, extended by T-969): one blind acceptance test
//! per assertion, over **every signal the explorer agent has captured off the air**, replayed
//! through the mock SDR device.
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
//! is a test result rather than a memory of a good night. The set is the explorer's 2026-09-25 FM
//! captures — all four of them, which the user asked for by name: 101.3 MHz and 98.9 MHz (T-935,
//! enlisted by T-936) and 88.5 MHz and 106.1 MHz (T-960, enlisted by T-969). Between them they
//! carry **seven** captured signals, because each capture's `hackriff:truth` annotates every
//! station the oracle examined in its 2.4 MHz window, not only the one it is named for:
//!
//! | Capture | Captured signals (hidden truth) |
//! |---|---|
//! | `fm-101p3-pi1694` | 101.3 MHz, PI 1694 (oracle 56 votes, PS `"Animals "`) |
//! | `fm-98p9-piA4FF` | 98.9 MHz, PI A4FF (3 votes) · 98.1 MHz, pilot and no PI |
//! | `fm-88p5-pi3AAB` | 88.5 MHz, PI 3AAB (4 votes) · 89.435 MHz, pilot and no PI |
//! | `fm-106p1-pi1323` | 106.1 MHz, PI 1323 (22 votes) · 106.907 MHz, pilot and no PI |
//!
//! The four weak-or-absent-RDS stations are as much of the answer key as the four PIs: three of
//! them are the *pilot without PI* case, which the set asserts in the same breath as a decode
//! (assertion (4) below), because inventing an identity there is worse than missing one.
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
//! Measured over all four captures on 2026-09-25 (T-969), after T-937, T-938, T-961 and T-962 had
//! landed:
//!
//! | Test | Ticket assertion | Red today because | Turned green by |
//! |---|---|---|---|
//! | [`a_every_captured_signal_is_detected_blind_as_a_time_frequency_region`] | (1), detection half | — **green control** | — |
//! | [`b_a_sensible_fm_broadcast_explanation_ranks_without_identifying`] | (5), path half | — **green control** | — |
//! | [`c_the_dynamic_ps_frame_the_oracle_read_is_decoded`] | (4), PS half | — **green control** | — |
//! | [`i_a_decoded_pi_lands_on_the_stations_own_row_with_the_votes_its_scheme_demands`] | (4), attachment half | — **green control**, and T-961's and T-962's fixture-level guard | — |
//! | [`d_each_captured_signal_is_one_region_of_the_right_width_and_extent`] | (1), region half | 88.5 and 89.435 MHz have **no region at all**; 98.9 MHz is **three** (98.9128 + 98.9163 + 98.9494 MHz); 101.3's box is 126.6 kHz of a 200 kHz channel, 98.9's 263.9 kHz and 106.907's 140.6 kHz; 98.1's region is presence-1.0 s of a 5 s continuous carrier | T-937 (fragmentation), T-940 (on-air read ended), T-926 (the two missing rows, with `j_…`) |
//! | [`e_the_carrier_centre_is_refined_not_left_in_the_detector_bin`] | (1), centre half | 98.9 MHz +12.78 kHz, 98.1 −12.59 kHz, **106.1 +15.82 kHz**, 106.907 +7.34 kHz — only 101.3 MHz is inside the one-detector-bin bound. 106.1 MHz is the new evidence: it carries the WFM chain, a 0.892 lock and a measured pilot, and is still boxed 3.4 detector bins high — so the gap is not only "a station without a chain gets no refinement" | T-938 (centre refinement from pilot/discriminator) |
//! | [`f_the_wfm_rds_chain_auto_attaches_to_every_captured_signal`] | (3) | 98.1, 88.5, 89.435 and 106.907 MHz get **no** demodulation at all: of seven captured signals only the three strongest were given the chain | T-926 (auto-attach across all detections) |
//! | [`g_every_captured_signal_is_classified_wfm_with_its_pilot_measured`] | (2) | the same four: `family` `None` (98.1, 88.5, 89.435) or `"analog"` (106.907), no pilot, no estimated parameters — while the oracle measured a locked 19 kHz pilot on every one of them | T-926 |
//! | [`h_the_rds_pi_is_decoded_where_the_truth_has_one_and_pilot_without_pi_where_it_does_not`] | (4), PI half | **88.5 MHz reports no PI at all**, committed or provisional, though the oracle read 3AAB there; 98.1, 89.435 and 106.907 MHz are not reported as *pilot without PI* — they are not reported as anything | T-926 |
//! | [`j_every_captured_signal_reaches_the_inventory_with_an_fm_broadcast_explanation`] | (5), reach half | 88.5 and 89.435 MHz never become an inventory row, so there is nothing to explain, demodulate or decode — `fm-88p5-pi3AAB` yields 175 detections (14 and 28 of them on those two channels) and **one** emitter: `89.0880 MHz / 10.3 kHz / Candidate`, the tuned centre's own DC artefact | T-926 |
//!
//! **The four green controls are the reason the six reds mean anything.** `a_…` proves the
//! fixture, the mock device and the truth vault are sound; `b_…` proves the explanation path runs;
//! `c_…` proves the decode path reaches this test, PS text and all; `i_…` proves a decode that does
//! happen lands on the station's own row with the evidence its scheme demands. If those went red
//! the honest reading would be "the harness is broken", not "the capability is missing" — the
//! distinction `docs/19 §5.4` draws about a capture, applied to a suite.
//!
//! Keeping that distinction is why T-969 **moved** the per-station half of assertion (5) out of
//! `b_…` and into `j_…`: 88.5 MHz has no inventory row, so `b_…` would have gone red on a station
//! the explanation path never saw, and the set would have lost its "is the harness sound?" signal
//! at the moment it gained a station. `b_…` now asks the path of the rows that exist (and refuses
//! to pass if *none* do); `j_…` owns "every captured signal gets a row, with an allocation beside
//! it" and names its ticket. Neither station lost an assertion in the move.
//!
//! # What T-961 and T-962 already fixed, and what these members now hold
//!
//! Two of the four captures are the clips their defects were found on, so their members are
//! **regression guards, not requests**:
//!
//! * **106.1 MHz (T-961)** was the *split row*: the recipe decode minted its own 200 kHz candidate
//!   carrying `rds-pi:1323` beside a blind-detection row that stayed `unknown 0.999`. Today this
//!   capture replays to **one** row — 106.1158 MHz, 168.8 kHz, Confirmed, `wfm`, PI 1323, present
//!   for the whole 5 s — which is `i_…`'s subject, and the fixture-level guard T-961's own
//!   hand-back said belonged with this capture.
//! * **88.5 MHz (T-962)** is where the vote bar bites honestly: the oracle read PI 3AAB from only
//!   **4** agreeing votes in the 5 s window, under [`RDS_PI_COMMIT_VOTES`], so a *commit* is not
//!   what this member asks for. `h_…` asks that the PI be **reported** — committed, or as the
//!   provisional reading T-962 writes — and `i_…` asks that a commit, where one happens, can show
//!   its votes. Requiring a commit from 4 votes would have made the suite demand the false-confirm
//!   behaviour T-962 removed; the oracle's count is a lower bound on the evidence in the clip, so
//!   it is used one way only: **at or over the bar, the run must commit** (101.3's 56 and 106.1's
//!   22), and below it either answer is honest.
//!
//! The reds that remain on these two captures are all downstream of the same gap: 88.5 MHz never
//! becomes a row at all, and none of the three pilot-only stations gets a chain.
//!
//! # Where the truth comes from, and why it is not this code's output
//!
//! `fixtures/hackrf/explorer-2026-09-25/README.md`: every `hackriff:truth.rds` block was decoded
//! **independently** by `py/fixtures/rds_ref.py` over each fixture's own 5 s window, not copied
//! from the live app, and the oracle's PI agrees with the explorer's live claim on all four stations
//! that have one (`1694`, `A4FF`, `3AAB`, `1323`) and agrees that 98.1, 89.435 and 106.907 MHz have
//! a locked pilot and no decodable PI. So a station's PI, its pilot frequency and its PS frame are
//! an answer key written by a different decoder, which is what makes assertions (2) and (4) checks
//! rather than a circular re-read of this repo's own RDS chain. The one caveat the fixtures state
//! plainly: **every one of the four is overloaded** (98.4 %, 12.0 %, 4.8 % and 5.8 % of samples
//! clipped at LNA 32 / VGA 30 / amp on, and the explorer's one gain-reduction retry made RDS
//! *worse*), which is a front-end limit of this location, not a bug — and it makes every assertion
//! here *harder*, never easier. It is also why the oracle's own vote counts run from 56 down to 3:
//! the answer key is as weak as the air was, which is what `h_…`'s rule about the commit bar takes
//! account of.
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
    InventoryQuery, LinkTarget, RDS_PI_COMMIT_VOTES, RDS_PI_COMMIT_WINDOW_NS, Region, Timestamp,
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
const CAPTURES: &[&str] = &[
    "fm-101p3-pi1694",
    "fm-98p9-piA4FF",
    "fm-88p5-pi3AAB",
    "fm-106p1-pi1323",
];

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

    /// Detections of the run whose occupied extent overlaps this station's annotated channel,
    /// inside its time extent. Printed by [`report`] so "no region" can be read apart from "not
    /// detected": a station with detections and no row was seen and then lost, which is a
    /// different gap from one that was never seen at all.
    fn detections_on_channel(&self) -> usize {
        let (a, b) = self.truth_window();
        self.run
            .detections
            .iter()
            .filter(|d| {
                let lo = d.f_center_hz - 0.5 * d.obw_hz;
                let hi = d.f_center_hz + 0.5 * d.obw_hz;
                hi >= self.truth.f_lo_hz
                    && lo <= self.truth.f_hi_hz
                    && d.time.end >= a
                    && d.time.start <= b
            })
            .count()
    }

    /// The agreeing CRC-valid votes the **independent oracle** counted for this station's PI over
    /// the fixture's own window (`/rds/pi_votes/<PI>`), or 0 where it read no PI.
    ///
    /// This is a **lower bound on the evidence the clip contains**, not an upper bound on what a
    /// decoder may see: `py/fixtures/rds_ref.py` is one decoder, and a better one legitimately
    /// reads more groups out of the same samples. It is in the same unit as T-962's bar
    /// ([`RDS_PI_COMMIT_VOTES`] agreeing votes within [`RDS_PI_COMMIT_WINDOW_NS`]) — which is why
    /// [`i_a_decoded_pi_lands_on_the_stations_own_row_with_the_votes_its_scheme_demands`] first
    /// checks that the station's extent is no longer than that window.
    fn oracle_votes(&self) -> u32 {
        self.truth
            .str("/rds/pi_hex")
            .and_then(|pi| self.truth.f64(&format!("/rds/pi_votes/{pi}")))
            .unwrap_or(0.0)
            .max(0.0) as u32
    }

    /// Every PI the run recorded on one of this station's regions: as a **committed**
    /// `IdentityScheme::RdsPi` identity, or as a **provisional** reading — a decode row carrying
    /// the PI with no identity and `identity_provisional: true` — which is what T-962 writes when
    /// the agreeing votes have not cleared [`RDS_PI_COMMIT_VOTES`] within
    /// [`RDS_PI_COMMIT_WINDOW_NS`].
    fn pi_readings(&self) -> Vec<PiReading> {
        let repo = repo(&self.run.dir.0);
        let mut out = Vec::new();
        for e in self.regions() {
            if let InventoryIdentity::Clear { identity, .. } = &e.identity {
                if identity.scheme == IdentityScheme::RdsPi {
                    out.push(PiReading {
                        entry: e,
                        pi: identity.value.clone(),
                        committed: true,
                        votes_in_window: self
                            .committed_pi_rows(&identity.value)
                            .iter()
                            .filter_map(votes_in_window)
                            .max(),
                    });
                }
            }
            for d in repo.provisional_decodes_of_emitter(e.emitter.id).unwrap() {
                if let Some(pi) = d.metadata.get("pi").and_then(Value::as_str) {
                    out.push(PiReading {
                        entry: e,
                        pi: pi.to_owned(),
                        committed: false,
                        votes_in_window: votes_in_window(&d),
                    });
                }
            }
        }
        out
    }

    /// The `rds-pi` decode rows the run wrote with `pi` as a committed identity.
    fn committed_pi_rows(&self, pi: &str) -> Vec<hk_model::Decode> {
        repo(&self.run.dir.0)
            .decodes_for_identity(&hk_model::DecodedIdentity {
                scheme: IdentityScheme::RdsPi,
                value: pi.to_owned(),
            })
            .unwrap()
            .into_iter()
            .filter(|d| d.frame_model == "rds-pi")
            .collect()
    }

    /// A WFM demodulation linked to `e`: the evidence that the row the PI sits on is the row the
    /// **detector** found and a chain ran on, rather than one a decode minted for itself (T-961).
    fn has_wfm_demod(&self, e: &InventoryEntry) -> bool {
        self.demods()
            .iter()
            .any(|(f, d)| (f - e.emitter.f_center_hz).abs() < 1.0 && d.mode == "wfm")
    }
}

/// One PI the run recorded for a station, committed or provisional (T-962).
struct PiReading {
    /// The inventory row it sits on.
    entry: &'static InventoryEntry,
    /// The PI as the run recorded it.
    pi: String,
    /// `true` when it is an `IdentityScheme::RdsPi` **identity**; `false` for a provisional reading.
    committed: bool,
    /// The agreeing in-window votes the decode row states, where it states them.
    votes_in_window: Option<u32>,
}

/// The agreeing in-window votes a decode row reports (T-962's `identity_votes_in_window`).
fn votes_in_window(d: &hk_model::Decode) -> Option<u32> {
    d.metadata
        .get("identity_votes_in_window")
        .and_then(Value::as_u64)
        .map(|v| v as u32)
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
        // Every row the run produced, not only the ones a station claims: when a captured signal
        // has *no* region (88.5 MHz today), the question a reader asks next is "then what did
        // reach the inventory?", and the answer belongs in the same log as the red.
        for e in &run.inventory {
            eprintln!(
                "  row {:.4} MHz bw {:.1} kHz {:?} family {:?}",
                e.emitter.f_center_hz / 1e6,
                e.emitter.bandwidth_hz / 1e3,
                e.lifecycle,
                e.family,
            );
        }
    }
    for st in set {
        eprintln!(
            "[{SIGNAL_062}] {}: {} detection(s) on this channel; regions on this channel:",
            st.id(),
            st.detections_on_channel()
        );
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
    let mut with_a_region = 0;
    for st in &set {
        let expl = st.explanations();
        // A station with no inventory row at all has nothing for the band plan to explain, and
        // that gap is a capability red with its own ticket
        // ([`j_every_captured_signal_reaches_the_inventory_with_an_fm_broadcast_explanation`]), not
        // evidence that the explanation path is broken. This control asks the path itself, so it
        // asks it of the rows that exist — and refuses to pass on an empty set below.
        if st.regions().is_empty() {
            eprintln!(
                "[{SIGNAL_062}] {}: no region, so no explanation to rank here (see j_…)",
                st.id()
            );
        } else {
            with_a_region += 1;
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
    // The control cannot pass vacuously: if the run stopped putting captured signals in the
    // inventory altogether, this is a broken harness/pipeline and not a ranking question.
    assert!(
        with_a_region > 0,
        "[{SIGNAL_062}] not one of the {} captured signals reached the inventory, so this control \
         proves nothing about the explanation path",
        set.len()
    );
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
/// must claim none — which, on this set, is three of the four PI stations: the oracle completed no
/// PS frame on 98.9, 88.5 or 106.1 MHz in their 5 s windows.
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
/// What its red message tells T-937 (and T-940) to build. Measured 2026-09-25 over all four
/// captures, after T-937 landed:
/// * **98.9 MHz is still three regions** — Confirmed 98.9128 MHz / 263.9 kHz, Candidate
///   98.9163 MHz / 140.6 kHz (overlapping it) and Candidate 98.9494 MHz / 9.4 kHz. One station,
///   three rows.
/// * **88.5 MHz and 89.435 MHz are zero regions**: the whole `fm-88p5-pi3AAB` capture yields one
///   emitter, on neither station's channel. That half of this red is
///   [`j_every_captured_signal_reaches_the_inventory_with_an_fm_broadcast_explanation`]'s subject
///   and T-926's to fix; it is repeated here because "one region" is false in both directions.
/// * **Widths are wrong in both directions**: 101.3 MHz is boxed at 126.6 kHz and 106.907 MHz at
///   140.6 kHz — narrower than the stereo multiplex a station carrying a 57 kHz subcarrier must
///   occupy — and 98.9 MHz at 263.9 kHz, wider than the 240 kHz the FCC mask allows one station.
/// * **98.1 MHz is present for 1.0 s of a 5 s continuous carrier** (T-940: on-air stations read
///   ended), while 98.9's Confirmed row stops at exactly 4.0 s, the built-in chain's `window_s`.
///   106.1 MHz, the one new station that does get a row, is present for the whole 5.00 s.
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
/// What its red message tells T-938 to build. Measured 2026-09-25 over all four captures, after
/// T-938 landed: 98.9 MHz is recorded **+12.78 kHz** out (2.7 detector bins), 98.1 MHz
/// **−12.59 kHz**, 106.907 MHz **+7.34 kHz**, and **106.1 MHz +15.82 kHz** — 3.4 bins — while
/// 101.3 MHz lands inside the bound. 106.1 MHz is the interesting one and it is new here: unlike
/// 98.1 or 106.907 it *does* carry the WFM chain, a 0.892 lock and a pilot measured to 18999.880 Hz,
/// and its box is still where the detector put it. So "only a station with an attached chain gets a
/// refined centre" no longer covers the gap: this station has the chain and the refinement does not
/// reach its emitter.
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
/// What its red message tells T-926 to build. Measured 2026-09-25 over all four captures: of
/// **seven** captured signals, **three** get a WFM demodulation (101.3, 98.9 and 106.1 MHz) and
/// **four get none at all** — 98.1, 88.5, 89.435 and 106.907 MHz. Each capture attaches its chain
/// to the strongest station or two and leaves the rest (`fm-88p5-pi3AAB`: 1 chain, 0
/// demodulations, 1 mode rejected; `fm-106p1-pi1323`: 5 chains, 2 demodulations, 5 classify chains
/// refused at the cap). The chain is attached per confirmed track, and the quieter stations never
/// reach one — 88.5 MHz never even reaches a row.
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
/// What its red message tells T-926 to build. Measured 2026-09-25 over all four captures: the three
/// stations that get the chain measure their pilot superbly — 101.3 MHz 18999.8906 Hz against the
/// oracle's 18999.8903 Hz (0.0003 Hz out), 98.9 MHz 18999.8463 vs 18999.8485 Hz, 106.1 MHz
/// 18999.8804 vs 18999.8936 Hz. The other **four read `family = None`** (98.1, 88.5, 89.435 MHz) or
/// **`"analog"`** (106.907 MHz), with no pilot and no parameters, even though the oracle measured a
/// locked 19 kHz pilot on every one of them. The estimator works; it is not being run on four of
/// the seven captured signals. `"analog"` on 106.907 MHz is the sharper form of the gap: that
/// station *was* classified, as far as "an analog emission", and then left there.
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
/// * every station the oracle read a PI from **reports that PI on its own inventory emitter** —
///   1694 at 101.3 MHz, A4FF at 98.9 MHz, 3AAB at 88.5 MHz, 1323 at 106.1 MHz — so the decode lands
///   on the thing the run detected rather than in a side table, and never a *different* PI;
/// * every station the oracle found a **locked pilot and no decodable PI** at (98.1, 89.435 and
///   106.907 MHz, 0 CRC-valid groups apiece) is reported as *pilot without PI*: the pilot measured
///   and stated, and **no identity invented**. Claiming a PI there would be worse than missing one
///   — it is the over-claim the negative controls of `mauto_negatives` guard against in general.
///
/// **Committed or provisional (T-962).** "Reports the PI" is deliberately not "commits it as an
/// identity". T-962 made an RDS PI an identity only above [`RDS_PI_COMMIT_VOTES`] agreeing
/// CRC-valid votes inside [`RDS_PI_COMMIT_WINDOW_NS`]; below the bar the decoder writes the PI as a
/// **provisional reading** with its vote and attaches no identity, which is the honest answer on a
/// 5 s clip the oracle itself only got 4 votes out of (88.5 MHz) or 3 (98.9 MHz). Demanding a
/// commit there would be this suite demanding the false confirm T-962 removed. So the oracle's vote
/// count is used in one direction only — it is a *lower bound* on the evidence the clip holds, a
/// better decoder may legitimately see more — and the rule is: at or over the bar (101.3's 56,
/// 106.1's 22) the run **must** commit; below it, committed or provisional both pass, but the PI
/// must be there and must be the right one. `i_…` then asks that any commit can show its votes.
/// In the other direction a *provisional* reading where the oracle read nothing is not an
/// invention and is only printed; a committed identity there is the red.
///
/// What its red message tells T-926 to build. Measured 2026-09-25 over all four captures: three of
/// the four PIs reach their emitter as a committed identity (1694, A4FF, 1323 — this half stays
/// asserted so it cannot regress), and nothing invents a PI anywhere. What fails:
/// * **88.5 MHz reports no PI at all**, committed or provisional, though the oracle read 3AAB there
///   and the app committed it live when prompted — because this capture produces no inventory row
///   for the station, so no chain, no demodulation and no decode ever run on it;
/// * the *positive* half of pilot-without-PI, on all three pilot-only stations (98.1, 89.435,
///   106.907 MHz): none is reported as a pilot-without-PI station, because none is reported as a
///   station at all — no chain, no pilot, nothing for the UI to show but an unexplained candidate.
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
        let readings = st.pi_readings();
        let oracle = st.oracle_votes();
        match st.truth.str("/rds/pi_hex") {
            Some(want) => {
                // Never a *different* PI, whether committed or provisional.
                for r in readings.iter().filter(|r| !r.pi.eq_ignore_ascii_case(want)) {
                    failures.push(format!(
                        "{}: the row at {:.4} MHz reports PI {} ({}), the oracle read {want}",
                        st.id(),
                        r.entry.emitter.f_center_hz / 1e6,
                        r.pi,
                        if r.committed {
                            "committed"
                        } else {
                            "provisional"
                        }
                    ));
                }
                let agreeing: Vec<&PiReading> = readings
                    .iter()
                    .filter(|r| r.pi.eq_ignore_ascii_case(want))
                    .collect();
                let committed = agreeing.iter().find(|r| r.committed);
                if agreeing.is_empty() {
                    failures.push(format!(
                        "{}: the oracle read PI {want} here ({oracle} agreeing votes); the run \
                         reports no PI on this station at all, committed or provisional",
                        st.id()
                    ));
                } else if oracle >= RDS_PI_COMMIT_VOTES && committed.is_none() {
                    // The evidence is demonstrably in the clip: an independent decoder found more
                    // agreeing votes there than T-962's bar asks for, over a window no longer than
                    // the bar's own. Staying provisional is then under-reading, not caution.
                    failures.push(format!(
                        "{}: the oracle read {oracle} agreeing votes for {want}, over the \
                         {RDS_PI_COMMIT_VOTES}-vote bar, but the run holds it only as a \
                         provisional reading ({:?} in-window votes): no emitter carries the \
                         identity",
                        st.id(),
                        agreeing
                            .iter()
                            .map(|r| r.votes_in_window)
                            .collect::<Vec<_>>()
                    ));
                } else {
                    let r = committed.copied().unwrap_or(agreeing[0]);
                    eprintln!(
                        "[{SIGNAL_062}] {}: PI {want} on the emitter at {:.4} MHz ({}, {:?} \
                         in-window votes; oracle {oracle})",
                        st.id(),
                        r.entry.emitter.f_center_hz / 1e6,
                        if r.committed {
                            "committed identity"
                        } else {
                            "provisional reading, oracle below the bar too"
                        },
                        r.votes_in_window,
                    );
                }
            }
            None => {
                // Pilot without PI: nothing invented, and the pilot itself reported. A
                // *provisional* reading is not an invention — T-962 records it as evidence with
                // its vote and attaches no identity — and the oracle reading no PI is a lower
                // bound, so only a committed identity is the over-claim here.
                for r in readings.iter().filter(|r| r.committed) {
                    failures.push(format!(
                        "{}: the oracle decoded no PI here, but the emitter at {:.4} MHz claims \
                         {} as an identity",
                        st.id(),
                        r.entry.emitter.f_center_hz / 1e6,
                        r.pi,
                    ));
                }
                for r in readings.iter().filter(|r| !r.committed) {
                    eprintln!(
                        "[{SIGNAL_062}] {}: provisional PI {} at {:.4} MHz ({:?} in-window votes), \
                         no identity claimed — the oracle read none here",
                        st.id(),
                        r.pi,
                        r.entry.emitter.f_center_hz / 1e6,
                        r.votes_in_window,
                    );
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

// -------------------------------------------------------------------------------------------
// (i) THE ATTACHMENT + EVIDENCE CONTROL (T-969). The fixture-level guard T-961 and T-962 asked for.
// -------------------------------------------------------------------------------------------

/// **Ticket assertion (4), attachment half: a decoded PI lands on the station's own detected row,
/// and a *committed* one carries the votes its scheme demands.**
///
/// What a passing run proves, and why this test exists at all: the 106.1 MHz capture is the clip
/// the explorer measured the **split row** on — `rds-pi:1323` on a 200 kHz row the recipe decode
/// minted for itself, beside the blind detection row that stayed `unknown 0.999` (T-961). Two
/// overlapping boxes for one station break ADR-0017's overlap rule, and a CRC-valid decode that
/// neither confirms nor classifies the station it came from is a decode landing in a side table.
/// T-961 fixed that at the seams (`hk_model::cluster` rule 2b and the decoder's family-evidence
/// rank) and its hand-back said the **fixture-level** guard belonged with this capture; this is it.
/// So, per station the run decoded a PI for:
///
/// * the PI sits on a row that is the station's own emission — **Confirmed**, classified `wfm`, and
///   carrying a WFM `Demodulation` — so it is the row the detector found and a chain ran on, not a
///   200 kHz rectangle a decode invented;
/// * exactly **one** row on the channel carries that PI (the split row was a second one);
/// * and where it is committed as an identity it states at least [`RDS_PI_COMMIT_VOTES`] agreeing
///   in-window votes (T-962: below the bar a PI is a provisional reading, never an identity, and a
///   commit that cannot show its votes is the 98.085 MHz false confirm returning).
///
/// It cannot pass vacuously: a station whose own oracle read **more** agreeing votes than the bar
/// asks for, over a window no longer than the bar's, must have a committed identity here — so a
/// pipeline that stopped decoding fails this control rather than skipping it. Which is also why the
/// unit check comes first: the oracle counted over the whole clip, and T-962's bar is a rate.
#[test]
fn i_a_decoded_pi_lands_on_the_stations_own_row_with_the_votes_its_scheme_demands() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    let window_s = RDS_PI_COMMIT_WINDOW_NS as f64 / 1e9;
    let mut failures = Vec::new();
    for st in &set {
        let extent_s = st.truth.t_end_s - st.truth.t_start_s;
        let oracle = st.oracle_votes();
        if oracle > 0 && extent_s > window_s {
            failures.push(format!(
                "{}: the oracle's {oracle} votes were counted over {extent_s:.2} s, longer than \
                 the {window_s:.1} s commit window, so they are not comparable to the \
                 {RDS_PI_COMMIT_VOTES}-vote bar; this member needs a per-window count",
                st.id()
            ));
            continue;
        }
        let readings = st.pi_readings();
        for r in &readings {
            eprintln!(
                "[{SIGNAL_062}] {}: {} PI {} on {:?} row {:.4} MHz family {:?}, wfm demod {}, \
                 {:?} in-window votes (oracle {oracle})",
                st.id(),
                if r.committed {
                    "committed"
                } else {
                    "provisional"
                },
                r.pi,
                r.entry.lifecycle,
                r.entry.emitter.f_center_hz / 1e6,
                r.entry.family,
                st.has_wfm_demod(r.entry),
                r.votes_in_window,
            );
        }
        if let Some(want) = st.truth.str("/rds/pi_hex") {
            let committed: Vec<&PiReading> = readings
                .iter()
                .filter(|r| r.committed && r.pi.eq_ignore_ascii_case(want))
                .collect();
            if oracle >= RDS_PI_COMMIT_VOTES && committed.is_empty() {
                failures.push(format!(
                    "{}: the oracle read {oracle} agreeing votes for PI {want} in this window, \
                     over the {RDS_PI_COMMIT_VOTES}-vote bar, and no row carries it as an identity",
                    st.id()
                ));
            }
            if committed.len() > 1 {
                failures.push(format!(
                    "{}: {} rows carry PI {want} as an identity ({:?}) — one station, one row \
                     (T-961)",
                    st.id(),
                    committed.len(),
                    committed
                        .iter()
                        .map(|r| r.entry.emitter.f_center_hz / 1e6)
                        .collect::<Vec<_>>()
                ));
            }
            for r in &committed {
                let e = r.entry;
                if e.lifecycle != hk_model::LifecycleState::Confirmed {
                    failures.push(format!(
                        "{}: the row carrying PI {want} at {:.4} MHz is {:?}, not Confirmed: a \
                         CRC-valid decode of a station is evidence that confirms it",
                        st.id(),
                        e.emitter.f_center_hz / 1e6,
                        e.lifecycle
                    ));
                }
                if e.family.as_deref() != Some("wfm") {
                    failures.push(format!(
                        "{}: the row carrying PI {want} at {:.4} MHz reads family {:?}: an RDS \
                         decode supersedes an unknown classification, it does not sit beside it",
                        st.id(),
                        e.emitter.f_center_hz / 1e6,
                        e.family
                    ));
                }
                if !st.has_wfm_demod(e) {
                    failures.push(format!(
                        "{}: the row carrying PI {want} at {:.4} MHz has no WFM demodulation, so \
                         it is not the row the detector found and demodulated — the split row \
                         T-961 fixed",
                        st.id(),
                        e.emitter.f_center_hz / 1e6
                    ));
                }
                match r.votes_in_window {
                    Some(v) if v >= RDS_PI_COMMIT_VOTES => {}
                    v => failures.push(format!(
                        "{}: PI {want} is committed as an identity at {:.4} MHz on {v:?} agreeing \
                         in-window votes, under T-962's bar of {RDS_PI_COMMIT_VOTES}",
                        st.id(),
                        e.emitter.f_center_hz / 1e6
                    )),
                }
            }
        }
    }
    assert!(failures.is_empty(), "[{SIGNAL_062}] {failures:#?}");
}

// -------------------------------------------------------------------------------------------
// (j) A RED PROOF (T-969): the captured signal that never reaches the inventory at all.
// -------------------------------------------------------------------------------------------

/// **Ticket assertion (5), reach half: every captured signal reaches the inventory as a region, with
/// an FM broadcast allocation among its top-[`TOP_K`] explanations.**
///
/// What a passing run would prove: the user sees *each* station the receiver heard — a
/// `docs/07` Emitter on the Explore lists with a ranked, reasoned suggestion beside it — rather
/// than only the ones strong enough to win a track. This is the half of assertion (5) that
/// [`b_a_sensible_fm_broadcast_explanation_ranks_without_identifying`] deliberately does not ask:
/// that control is about the explanation **path** and asks it of the rows that exist, so that a
/// station missing from the inventory reads as the capability gap it is here, in one place, with
/// its own ticket — not as a broken harness.
///
/// What its red message tells T-926 to build. Measured 2026-09-25 over the four captures:
/// **88.5 MHz and 89.435 MHz have no inventory row at all.** `fm-88p5-pi3AAB` produces 175
/// detections — 14 of them on 88.5 MHz's channel and 28 on 89.435 MHz's — opens 2 tracks, and
/// leaves exactly **one** emitter in the inventory: `89.0880 MHz, 10.3 kHz, Candidate, family
/// None`, which is the tuned centre, i.e. the receiver's own DC/LO artefact. So the only thing
/// this capture shows the user is the radio looking at itself, while the station whose PI the app
/// committed live when prompted (3AAB, `Confirmed fm-broadcast`) is not on the list at all, and no
/// chain, decode or explanation can run on it. The detections are there ([`report`] prints how many
/// fall on each channel): these are stations seen and then lost between detection and the
/// inventory, not stations that were never seen.
#[test]
#[ignore = "T-969 PROVES THE GAP AND IS EXPECTED TO FAIL. `#[ignore]`d only so one known-red \
            proof does not pin a gate red; deleting this line is part of T-926's definition of \
            done. Run it: cargo nextest run -p hk-e2e \
            -E 'binary(acceptance_captured_signals)' --run-ignored all"]
fn j_every_captured_signal_reaches_the_inventory_with_an_fm_broadcast_explanation() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for st in &set {
        if st.regions().is_empty() {
            failures.push(format!(
                "{}: no inventory row on this station's channel, though the run has {} detection(s) \
                 on it: nothing for the user to see and nothing to explain, demodulate or decode",
                st.id(),
                st.detections_on_channel()
            ));
            continue;
        }
        let expl = st.explanations();
        if !expl
            .iter()
            .any(|(_, services, _)| services.iter().any(|s| s == "fm-broadcast"))
        {
            failures.push(format!(
                "{}: no region carries an fm-broadcast allocation in its top-{TOP_K}: {:?}",
                st.id(),
                expl.iter()
                    .map(|(f, s, _)| (f / 1e6, s))
                    .collect::<Vec<_>>()
            ));
        }
    }
    assert!(failures.is_empty(), "[{SIGNAL_062}] {failures:#?}");
}
