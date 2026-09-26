//! **The P25 member of "all captured signals decode"** (T-976; `SIGNAL-085`): the explorer's
//! 852.859 MHz P25 C4FM capture, replayed blind through the mock SDR.
//!
//! ```text
//! cargo nextest run -p hk-e2e -E 'binary(acceptance_captured_signals) & test(/^captured_p25::/)'
//! cargo nextest run -p hk-e2e -E 'binary(acceptance_captured_signals) & test(/^captured_p25::/)' --run-ignored all
//! ```
//!
//! # Why a module of its own, not a line in `CAPTURES`
//!
//! The set's rule is one failing-first blind test per captured signal, in the shape T-936 gave it
//! ([`captured_signals`](super::captured_signals)). That module's `CAPTURES` list is the FM/RDS
//! half of the set — every test there asks a broadcast question (a pilot, a PI, a 200 kHz channel,
//! an FM broadcast allocation), so a P25 capture on that list would fail every one of them for
//! the wrong reason. This capture asks different questions of the same harness: the **same** blind
//! replay (`blind_replay`, the mock SDR, plan `json!({})`, nothing tuned), the same sealed truth,
//! the same "print everything before opening the truth" rule, the same one-`#[ignore]`-per-gap
//! discipline. It runs in the same milestone target (`just acceptance-captured-signals`), and a
//! later P25 or trunked capture joins by adding its stem to [`P25_CAPTURES`].
//!
//! # The capture, and what the answer key does and does not say
//!
//! `fixtures/hackrf/explorer-2026-09-25/p25-852p86-2p4M` (T-975): 5 s at 2.4 Msps, tuned
//! 853.3 MHz, one annotated emission — an **intermittent** P25 C4FM channel at 852.8587 MHz. The
//! independent oracle (`py/fixtures/p25_ref.py`: FM discriminator, 4800 Bd, 24-symbol sync
//! correlation of `0x5575F5FF77FF`) found **3** frame syncs in the clip, at 0.67, 2.58 and
//! 4.11 s — not the explorer's 4, a disagreement the fixture records rather than hides. The
//! explorer's re-grade: most likely a conventional/voice channel, **not** an established control
//! channel. So the answer key says *P25 air interface, recognised by its frame sync*, and it says
//! nothing about a CRC-valid TSBK or LDU — the fixture itself is `decoded: false`.
//!
//! What the live app showed on this emission (explorer window 2): **two** overlapping candidates
//! (9.3 and 26.2 kHz), `mod 2fsk`, `family unknown 0.999`, `resolution not-searched`, while its
//! control-channel hunt ran 10 demodulations and confirmed none. Each ticket assertion below is
//! that field report turned into a check on `docs/07` objects.
//!
//! # The tests, and what each red measured (2026-09-26)
//!
//! | Test | Ticket assertion | Today | Turned green by |
//! |---|---|---|---|
//! | [`p25_a_the_emission_is_detected_blind_and_the_answer_key_is_usable`] | harness | **green control** | — |
//! | [`p25_b_it_is_one_time_frequency_region_not_two_overlapping`] | (1) | **green** — one 14.1 kHz row at 852.8587 MHz, on air across every oracle sync: T-978's regression guard on the capture it was found on | — |
//! | [`p25_c_a_public_safety_land_mobile_explanation_ranks_in_the_top_k`] | (3), explanation half | **green control** — `public-safety` ranks 2, flagged `allocation-only` | — |
//! | [`p25_d_c4fm_is_estimated_four_level_at_4800_bd_not_2fsk`] | (2) | red: the only estimate on the row is the burst chain's `hk-demod/c20-fsk` — **4795 Bd, right, and `mod_order` 2, `2fsk`, wrong**; the hunt's four-level measurement never reaches this row | the C4FM-level-count ticket filed from T-976's hand-back |
//! | [`p25_e_it_is_identified_p25_like_from_its_own_frame_syncs`] | (3), identification half | red: no `p25-frame-sync` evidence; the row's family is `2fsk` | the region-accumulated frame-sync ticket filed from T-976's hand-back |
//! | [`p25_f_the_row_carries_the_chains_decode_verdict`] | (4) | red: `resolution: not-searched`, no trunking analysis row — the hunt made **1** pass, demodulated 1 channel and filed **0** verdicts | the hunt-verdict-on-a-short-clip ticket filed from T-976's hand-back |
//!
//! **Why (e) and (f) cannot go green by turning a knob.** The built-in `trunk-cc-hunt` passes once
//! per `period_s` = 10 s of stream, so a 5 s clip gets exactly one pass, at its start, before blind
//! detection has written the emitter a verdict would be filed on (T-977's own tests reach the row
//! only by raising the rate to one pass per 0.5 s, which this suite must not do: nothing is passed
//! in). And even a pass per 0.5 s would very likely not identify this channel, because P25-like
//! needs [`MIN_SYNC_HITS`] = 2 frame syncs **inside one 0.5 s window** and the oracle's three are
//! 1.5–1.9 s apart — on the answer key's evidence no window holds two. (The hunt's own sync search
//! tolerates `hk_detect::trunk::SYNC_TOLERANCE_DIBITS` dibit errors and could in principle find syncs the oracle
//! missed; that is the only way a per-window rule could pass here, and nothing in the clip says it
//! will.) The evidence exists in the clip — 3 syncs over the region clears the floor — but only if
//! it is accumulated over the region's time extent, which is the invariant "decode operates on a
//! captured region and extends with it". The reds say so in their messages.
//!
//! **(4) is deliberately marginal.** Three frame syncs is not enough to demand a CRC-valid TSBK or
//! LDU, and the answer key has no decode to check one against, so no test here asks for one. What
//! (f) asks is that the row carries the chain's **verdict** — that it was tried, how many frame
//! syncs it found, and how many blocks it CRC-checked with how many valid — so the emitter stops
//! reading `not-searched` beside a chain that looked.
//!
//! # Tolerances
//!
//! All a priori. The channel raster is 12.5 kHz (47 CFR 90.617's 800 MHz public-safety band plan,
//! the same source the fixture cites for *where* the explorer looked). C4FM is 4800 Bd with four
//! levels by TIA-102.BAAA. The detector's bin at 2.4 Msps is 4687.5 Hz (a 512-point detect STFT, as
//! `captured_signals` derives it). The per-window sync floor is the product's own
//! [`MIN_SYNC_HITS`], read, not copied.

use std::sync::OnceLock;

use hk_detect::trunk::MIN_SYNC_HITS;
use hk_e2e::blind::truth_emissions;
use hk_e2e::{Fixture, TruthItem};
use hk_model::repo::synthesis::{EmitterSynthesis, ResolutionKind, Stage};
use hk_model::{
    Demodulation, Detection, FreqRange, IdleGap, InventoryEntry, InventoryQuery, Region, Timestamp,
};
use serde_json::Value;

use crate::blind::{BlindRun, BlindSource, TOP_K, assert_truth_found, blind_replay};
use crate::common::*;

const T976: &str = "T-976/SIGNAL-085";

/// Where the set's fixtures live.
const FIXTURE_DIR: &str = "fixtures/hackrf/explorer-2026-09-25";

/// **The P25 half of the set.** One fixture stem per capture; its `p25-c4fm` truth emissions
/// join every assertion below.
const P25_CAPTURES: &[&str] = &["p25-852p86-2p4M"];

/// The truth kind this module asserts on.
const P25_KIND: &str = "p25-c4fm";

// -------------------------------------------------------------------------------------------
// A-priori tolerances.
// -------------------------------------------------------------------------------------------

/// The 800 MHz public-safety channel raster, Hz (47 CFR 90.617).
const RASTER_HZ: f64 = 12.5e3;

/// How near a region's centre must be to be *this* channel: half the raster. Any looser and the
/// neighbouring channel satisfies the assertion (the same bound T-977 uses).
const NEAR_HZ: f64 = 0.5 * RASTER_HZ;

/// The detector's frequency resolution at these fixtures' 2.4 Msps: a 512-point detect STFT.
const DETECT_BIN_HZ: f64 = 2.4e6 / 512.0;

/// The widest box one P25 channel may be drawn as: its 12.5 kHz channel plus one detector bin of
/// quantisation at each edge. The live app's 26.2 kHz candidate is over it; the 9.3 kHz one is
/// not, which is why "one region" is asserted as a **count** and not only as a width.
const MAX_REGION_BW_HZ: f64 = RASTER_HZ + 2.0 * DETECT_BIN_HZ;

/// C4FM's symbol rate, Bd (TIA-102.BAAA).
const C4FM_BD: f64 = 4800.0;

/// Tolerance on a **measured** symbol rate, Bd — carried unchanged from T-977's
/// `t977_cc_verdict::RATE_TOL_BD`, where the estimate is the same cyclostationary line measurement.
const RATE_TOL_BD: f64 = 250.0;

/// C4FM's level count (TIA-102.BAAA: four-level FSK, dibits ±1/±3).
const C4FM_LEVELS: u32 = 4;

/// A P25 Phase 1 frame is 864 symbols (an LDU: 180 ms at 4800 Bd), and every frame carries one
/// frame sync. So a region can hold at most `extent / 180 ms` syncs; a verdict claiming more has
/// counted something that is not a frame sync.
const P25_FRAME_S: f64 = 864.0 / C4FM_BD;

/// The explanation services that are a sensible top-k for a P25 emission. `public-safety` is the
/// band plan's service id whose label is "Public safety / land mobile"; `land-mobile` is the
/// broader LMR service it sits in. Anything else is not an explanation of a P25 channel.
const P25_SERVICES: &[&str] = &["public-safety", "land-mobile"];

// -------------------------------------------------------------------------------------------
// The run.
// -------------------------------------------------------------------------------------------

/// One P25 capture, replayed blind through the mock SDR.
pub struct P25Run {
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

fn replay(name: &'static str) -> Option<P25Run> {
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
    Some(P25Run {
        name,
        dir,
        summary,
        fx,
        api_rows,
        inventory,
        detections,
        control_changes: mock.iter().map(|m| m.control_changes).sum(),
    })
}

/// Every P25 capture this checkout can replay, replayed once per process.
fn runs() -> &'static [P25Run] {
    static RUNS: OnceLock<Vec<P25Run>> = OnceLock::new();
    RUNS.get_or_init(|| P25_CAPTURES.iter().filter_map(|n| replay(n)).collect())
}

/// One captured P25 emission and the run it was captured in.
struct Channel {
    run: &'static P25Run,
    truth: &'static TruthItem,
}

impl Channel {
    fn id(&self) -> String {
        format!(
            "{} / {}",
            self.run.name,
            self.truth.label.as_deref().unwrap_or("<unlabelled>")
        )
    }

    /// The annotated carrier, Hz.
    fn center_hz(&self) -> f64 {
        self.truth
            .f64("center_hz")
            .unwrap_or_else(|| self.truth.center_hz())
    }

    fn t0(&self) -> Timestamp {
        crate::blind::recording_start(&self.run.fx)
    }

    /// An offset into the clip, in the run's clock.
    fn at(&self, s: f64) -> Timestamp {
        self.t0().saturating_add_nanos((s * 1e9) as i64)
    }

    /// The oracle's frame-sync times, s into the clip.
    fn oracle_syncs_s(&self) -> Vec<f64> {
        self.truth
            .get("/p25/syncs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|s| s["t_s"].as_f64()).collect())
            .unwrap_or_default()
    }

    /// Every inventory row whose occupied extent overlaps this channel inside its annotated time
    /// extent: every row that **claims** any of it. Overlap, not centre distance, because the
    /// field defect was a second, wider box stacked over the same emission.
    fn claims(&self) -> Vec<&'static InventoryEntry> {
        let (a, b) = (self.at(self.truth.t_start_s), self.at(self.truth.t_end_s));
        self.run
            .inventory
            .iter()
            .filter(|e| {
                let lo = e.emitter.f_center_hz - 0.5 * e.emitter.bandwidth_hz;
                let hi = e.emitter.f_center_hz + 0.5 * e.emitter.bandwidth_hz;
                hi >= self.truth.f_lo_hz
                    && lo <= self.truth.f_hi_hz
                    && e.emitter.last_seen >= a
                    && e.emitter.first_seen <= b
            })
            .collect()
    }

    /// **The** region: the claiming row whose centre is on this channel, nearest first.
    fn region(&self) -> Option<&'static InventoryEntry> {
        self.claims()
            .into_iter()
            .filter(|e| (e.emitter.f_center_hz - self.center_hz()).abs() <= NEAR_HZ)
            .min_by(|x, y| {
                let d = |e: &InventoryEntry| (e.emitter.f_center_hz - self.center_hz()).abs();
                d(x).total_cmp(&d(y))
            })
    }

    /// The region's `/api/inventory` row.
    fn api_row(&self) -> Option<&'static Value> {
        let e = self.region()?;
        self.run.api_rows.iter().find(|r| {
            (r["f_center_hz"].as_f64().unwrap_or(f64::NAN) - e.emitter.f_center_hz).abs() < 1.0
        })
    }

    /// The region's demodulations (`docs/07` Demodulation), newest first.
    fn demods(&self) -> Vec<Demodulation> {
        self.region().map_or_else(Vec::new, |e| {
            repo(&self.run.dir.0)
                .demodulations_for_emitter(e.emitter.id, 64)
                .unwrap()
        })
    }

    /// The trunking engine's newest analysis row on the region, if the hunt filed one.
    fn verdict(&self) -> Option<EmitterSynthesis> {
        let e = self.region()?;
        repo(&self.run.dir.0)
            .synthesis_history(e.emitter.id)
            .unwrap()
            .into_iter()
            .find(|s| s.engine == hk_pipeline::synth::TRUNK_SYNTH_ENGINE)
    }

    /// Detections overlapping the channel inside its extent: read beside "no region" so *seen and
    /// lost* can be told from *never seen*.
    fn detections_on_channel(&self) -> usize {
        self.run
            .detections
            .iter()
            .filter(|d| {
                d.f_center_hz + 0.5 * d.obw_hz >= self.truth.f_lo_hz
                    && d.f_center_hz - 0.5 * d.obw_hz <= self.truth.f_hi_hz
            })
            .count()
    }
}

/// Every captured P25 emission of the set.
fn set() -> Vec<Channel> {
    runs()
        .iter()
        .flat_map(|run| {
            truth_emissions(&run.fx)
                .into_iter()
                .filter(|t| t.kind == P25_KIND)
                .map(move |truth| Channel { run, truth })
        })
        .collect()
}

fn hunt(run: &P25Run, what: &str) -> u64 {
    run.summary.counter(&format!("/chains/{what}"))
}

/// Everything the run produced, printed **before** any assertion reads the truth.
fn report(set: &[Channel]) {
    for run in runs() {
        let c = &run.summary.counters["chains"];
        eprintln!(
            "[{T976}] {}: {} detections, {} inventory rows, {} chains attached, {} demodulations, \
             {} decodes, {} device controls, {} lost samples; trunk hunt: {} pass(es), {} \
             candidate(s), {} detected-emitter candidate(s), {} demod(s), {} confirmed, {} \
             verdict(s) filed",
            run.name,
            run.detections.len(),
            run.inventory.len(),
            c["attached"],
            c["demodulations"],
            c["decodes"],
            run.control_changes,
            run.summary.always_on_lost_samples,
            hunt(run, "cc_passes"),
            hunt(run, "cc_candidates"),
            hunt(run, "cc_emitter_candidates"),
            hunt(run, "cc_demods"),
            hunt(run, "cc_confirmed"),
            hunt(run, "cc_verdicts"),
        );
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
    for ch in set {
        eprintln!(
            "[{T976}] {}: {} detection(s) on the channel; {} row(s) claim it:",
            ch.id(),
            ch.detections_on_channel(),
            ch.claims().len()
        );
        for e in ch.claims() {
            eprintln!(
                "  {:.5} MHz bw {:.1} kHz {:?} family {:?}",
                e.emitter.f_center_hz / 1e6,
                e.emitter.bandwidth_hz / 1e3,
                e.lifecycle,
                e.family,
            );
        }
        for d in ch.demods() {
            eprintln!("  demod {} ({}): {:?}", d.mode, d.demod_version, d.params);
        }
        if let Some(e) = ch.region() {
            for c in repo(&ch.run.dir.0)
                .classification_history(e.emitter.id)
                .unwrap()
            {
                eprintln!(
                    "  classification {} {:.3} ({})",
                    c.classification.family,
                    c.classification.confidence,
                    c.classification.model_version
                );
            }
        }
        match ch.verdict() {
            Some(v) => {
                eprintln!("  verdict {:?} / {:?}", v.verdict, v.resolution);
                for ev in &v.evidence {
                    eprintln!(
                        "    {:?} {} = {} (n {}): {}",
                        ev.stage, ev.metric, ev.raw, ev.n, ev.summary
                    );
                }
            }
            None => eprintln!("  no trunking analysis row on the region"),
        }
        if let Some(r) = ch.api_row() {
            eprintln!(
                "  served: family {} estimated_params {} resolution {} explanations {:?}",
                r["family"],
                r["estimated_params"],
                r["resolution"],
                services(r),
            );
        }
    }
}

/// The top-[`TOP_K`] explanation services of an `/api/inventory` row.
fn services(row: &Value) -> Vec<String> {
    row["explanations"]
        .as_array()
        .map(|x| {
            x.iter()
                .take(TOP_K)
                .filter_map(|e| e["service"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The set, or `None` when no P25 capture's LFS data is fetched (skip; `HK_REQUIRE_FIXTURES=1`,
/// which the milestone target sets, makes that a failure instead).
fn armed() -> Option<Vec<Channel>> {
    let set = set();
    if set.is_empty() {
        eprintln!("SKIP {T976}: no P25 capture of the set has its LFS data fetched");
        return None;
    }
    report(&set);
    Some(set)
}

/// The no-lookup-and-tune guard every test runs first.
fn assert_nothing_was_commanded() {
    for run in runs() {
        assert_eq!(
            run.control_changes, 0,
            "[{T976}] {}: the mock device was given {} control change(s); this suite passes no \
             frequency in and commands no tune",
            run.name, run.control_changes
        );
        assert_eq!(
            run.summary.always_on_lost_samples, 0,
            "[{T976}] {}: the always-on readers lost samples, so a red below could be missing data \
             rather than a missing capability",
            run.name
        );
    }
}

/// The region every capability test is about, or a failure that says there is none.
fn region_of(ch: &Channel) -> &'static InventoryEntry {
    ch.region().unwrap_or_else(|| {
        panic!(
            "[{T976}] {}: no inventory region within {NEAR_HZ} Hz of the channel, so there is \
             nothing to estimate, identify or decode ({} detection(s) on the channel)",
            ch.id(),
            ch.detections_on_channel()
        )
    })
}

// -------------------------------------------------------------------------------------------
// Green controls.
// -------------------------------------------------------------------------------------------

/// **The harness control: the P25 emission is detected blind, and the answer key is usable.**
///
/// What a passing run proves: the mock device served the capture and was never commanded; blind
/// detection found energy on the annotated channel inside its time extent (`docs/07` Detection),
/// with the truth sealed until after the run; and the answer key is one a red can be read against
/// — a C4FM emission inside the capture's band, a time extent, and frame syncs written by the
/// **independent** oracle, at least [`MIN_SYNC_HITS`] of them (below that, identification from
/// sync would be asking the system for evidence the clip does not hold). Keep it green: it is what
/// makes the reds below mean "the capability is missing" and not "the fixture is empty".
#[test]
fn p25_a_the_emission_is_detected_blind_and_the_answer_key_is_usable() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    for run in runs() {
        assert_truth_found(T976, &run.dir.0, &run.fx, 0.0, false);
    }
    for ch in &set {
        let f = ch.center_hz();
        let centre = ch.run.fx.center_hz_at(0).unwrap();
        assert!(
            (f - centre).abs() <= 0.5 * ch.run.fx.sample_rate,
            "[{T976}] {}: annotated carrier {f} Hz is not inside the capture's band",
            ch.id()
        );
        assert_eq!(
            ch.truth.str("modulation"),
            Some("C4FM"),
            "[{T976}] {}: the truth does not say C4FM",
            ch.id()
        );
        assert!(
            ch.truth.t_end_s > ch.truth.t_start_s,
            "[{T976}] {}: the annotated emission has no time extent",
            ch.id()
        );
        let decoder = ch.truth.str("/p25/decoder").unwrap_or_default();
        assert!(
            decoder.contains("p25_ref.py"),
            "[{T976}] {}: the P25 truth was not written by the independent oracle ({decoder:?})",
            ch.id()
        );
        let syncs = ch.oracle_syncs_s();
        assert!(
            syncs.len() as u32 >= MIN_SYNC_HITS
                && syncs
                    .iter()
                    .all(|t| (ch.truth.t_start_s..=ch.truth.t_end_s).contains(t)),
            "[{T976}] {}: the oracle's frame syncs {syncs:?} are fewer than the {MIN_SYNC_HITS} \
             identification needs, or outside the annotated extent",
            ch.id()
        );
        assert!(
            ch.detections_on_channel() > 0,
            "[{T976}] {}: no detection on the channel",
            ch.id()
        );
    }
}

/// **Ticket assertion (1): ONE time–frequency region at 852.859 MHz, not two overlapping.**
///
/// What a passing run proves: exactly one inventory row claims any part of the 12.5 kHz channel;
/// it is centred on the channel (within half the raster), no wider than the channel plus a detector
/// bin at each edge, and its recorded presence covers **every** instant the oracle saw a frame sync
/// — the emission was on air then, so a region that is absent at any of them has lost part of the
/// signal. Presence is read off presence intervals (`docs/07` §2.27), never the emitter's hull.
///
/// Green since T-978 (overlapping boxes for one emission are re-analysed): the field showed 9.3 and
/// 26.2 kHz candidates stacked on this channel, and this is the guard on the capture that showed it.
#[test]
fn p25_b_it_is_one_time_frequency_region_not_two_overlapping() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    for ch in &set {
        let claims = ch.claims();
        assert_eq!(
            claims.len(),
            1,
            "[{T976}] {}: {} regions claim the channel, not one: {:?}",
            ch.id(),
            claims.len(),
            claims
                .iter()
                .map(|e| (e.emitter.f_center_hz / 1e6, e.emitter.bandwidth_hz / 1e3))
                .collect::<Vec<_>>()
        );
        let e = region_of(ch);
        assert!(
            e.emitter.bandwidth_hz > 0.0 && e.emitter.bandwidth_hz <= MAX_REGION_BW_HZ,
            "[{T976}] {}: the region is {:.1} kHz wide; one 12.5 kHz P25 channel is at most \
             {:.1} kHz at this detector's resolution",
            ch.id(),
            e.emitter.bandwidth_hz / 1e3,
            MAX_REGION_BW_HZ / 1e3
        );
        let intervals = repo(&ch.run.dir.0)
            .presence_intervals(e.emitter.id, IdleGap::continuous(), ever().end)
            .unwrap();
        for s in ch.oracle_syncs_s() {
            let t = ch.at(s);
            assert!(
                intervals
                    .iter()
                    .any(|p| p.time.start <= t && t <= p.time.end),
                "[{T976}] {}: the oracle saw a frame sync at {s:.3} s and the region is not \
                 present then: {:?}",
                ch.id(),
                intervals
                    .iter()
                    .map(|p| (
                        (p.time.start.as_unix_nanos() - ch.t0().as_unix_nanos()) as f64 * 1e-9,
                        (p.time.end.as_unix_nanos() - ch.t0().as_unix_nanos()) as f64 * 1e-9,
                    ))
                    .collect::<Vec<_>>()
            );
        }
    }
}

/// **Ticket assertion (3), explanation half: a "public safety / land mobile" explanation ranks in
/// the top-[`TOP_K`].**
///
/// What a passing run proves: the band plan, consulted **after** blind detection, offers a land
/// mobile allocation as a ranked suggestion on this row. It is an explanation, not an
/// identification — the served evidence says `allocation-only` today, and the identification half
/// ([`p25_e_it_is_identified_p25_like_from_its_own_frame_syncs`]) is required to come from the
/// signal's own frame syncs, never from this allocation.
#[test]
fn p25_c_a_public_safety_land_mobile_explanation_ranks_in_the_top_k() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    for ch in &set {
        region_of(ch);
        let row = ch.api_row().unwrap_or_else(|| {
            panic!(
                "[{T976}] {}: the region is not served by /api/inventory",
                ch.id()
            )
        });
        let top = services(row);
        assert!(
            top.iter().any(|s| P25_SERVICES.contains(&s.as_str())),
            "[{T976}] {}: no public-safety / land-mobile explanation in the top-{TOP_K}: {top:?}",
            ch.id()
        );
    }
}

// -------------------------------------------------------------------------------------------
// Red proofs. Each names the gap it measured and what turns it green.
// -------------------------------------------------------------------------------------------

/// **Ticket assertion (2): estimated as four-level FSK at ~4800 Bd (C4FM), not 2fsk.**
///
/// What a passing run proves: the row's served estimate (`/api/inventory` `estimated_params`, the
/// value the field row showed as `mod 2fsk`) is four-level at C4FM's rate, and at least one
/// `docs/07` Demodulation on the region says so as a measurement. Both numbers are measured
/// blind; neither the rate nor the level count is supplied.
///
/// Red today: the only demodulations on the region are the burst chain's `hk-demod/c20-fsk`,
/// which measures the rate right (4795 Bd) and the alphabet wrong (`mod_order` 2). The hunt's
/// `measure_fm_structure` — which T-977 showed reads four levels on synthetic C4FM — never reaches
/// this row (see [`p25_f_the_row_carries_the_chains_decode_verdict`]).
#[test]
#[ignore = "T-976 PROVES THE GAP AND IS EXPECTED TO FAIL: the region's only estimate is \
            hk-demod/c20-fsk's 2-level `2fsk` at 4795 Bd on a C4FM emission. Green when the \
            C4FM level-count ticket filed from T-976's hand-back lands (the burst chain's level \
            count on real C4FM, or the hunt's 4-level measurement reaching the row). Run with \
            --run-ignored all"]
fn p25_d_c4fm_is_estimated_four_level_at_4800_bd_not_2fsk() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    for ch in &set {
        region_of(ch);
        let served = &ch.api_row().expect("the region is served")["estimated_params"];
        let order = served["mod_order"].as_u64();
        let rate = served["symbol_rate_hz"].as_f64();
        assert!(
            order == Some(u64::from(C4FM_LEVELS))
                && rate.is_some_and(|r| (r - C4FM_BD).abs() <= RATE_TOL_BD)
                && served["modulation"].as_str() != Some("2fsk"),
            "[{T976}] {}: the served estimate is {} at {rate:?} Bd with mod_order {order:?}; \
             C4FM is {C4FM_LEVELS}-level at {C4FM_BD} ± {RATE_TOL_BD} Bd",
            ch.id(),
            served["modulation"]
        );
        let measured: Vec<(u32, f64)> = ch
            .demods()
            .iter()
            .filter_map(|d| Some((d.params.mod_order?, d.params.symbol_rate_hz?)))
            .collect();
        assert!(
            measured
                .iter()
                .any(|&(o, r)| o == C4FM_LEVELS && (r - C4FM_BD).abs() <= RATE_TOL_BD),
            "[{T976}] {}: no demodulation on the region measured {C4FM_LEVELS} levels at \
             {C4FM_BD} Bd; (levels, Bd) measured: {measured:?}",
            ch.id()
        );
    }
}

/// **Ticket assertion (3), identification half: P25-like, from the signal's own frame syncs.**
///
/// What a passing run proves: the region's classification history (`docs/07` Classification)
/// carries the `p25-frame-sync` family — the resemblance id a frame sync earns, deliberately
/// distinct from the `p25-tsbk` a CRC-valid decode earns — at a confidence below a decode's, and
/// the trunking verdict on the row states the evidence as a count: at least [`MIN_SYNC_HITS`]
/// frame syncs, and no more than P25 frames fit in the region's extent. The identification comes
/// from the signal: the `public-safety` allocation beside it ([`p25_c_a_public_safety_land_mobile_explanation_ranks_in_the_top_k`])
/// is not evidence and is not what this reads.
///
/// Red today: no `p25-frame-sync` evidence at all; the row's family is `2fsk`. And the per-window
/// rule is not expected to produce it on this capture: [`MIN_SYNC_HITS`] syncs are required inside
/// one 0.5 s hunt window, and the oracle's three syncs are 1.5–1.9 s apart — so on the answer
/// key's evidence it exists only when accumulated over the region.
#[test]
#[ignore = "T-976 PROVES THE GAP AND IS EXPECTED TO FAIL: no p25-frame-sync evidence on the \
            region (family 2fsk); the hunt needs 2 syncs in one 0.5 s window and the oracle's 3 \
            are 1.5-1.9 s apart. Green when the region-accumulated frame-sync ticket filed from \
            T-976's hand-back lands. Run with --run-ignored all"]
fn p25_e_it_is_identified_p25_like_from_its_own_frame_syncs() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    for ch in &set {
        let e = region_of(ch);
        let history = repo(&ch.run.dir.0)
            .classification_history(e.emitter.id)
            .unwrap();
        let like: Vec<_> = history
            .iter()
            .filter(|c| c.classification.family == "p25-frame-sync")
            .collect();
        assert!(
            !like.is_empty(),
            "[{T976}] {}: the region has no p25-frame-sync family evidence; its classifications \
             are {:?} — the oracle found {} P25 frame syncs in this clip",
            ch.id(),
            history
                .iter()
                .map(|c| (
                    c.classification.family.as_str(),
                    c.classification.confidence
                ))
                .collect::<Vec<_>>(),
            ch.oracle_syncs_s().len()
        );
        for c in &like {
            assert!(
                c.classification.confidence < 1.0,
                "[{T976}] {}: a resemblance carries a decode's certainty ({})",
                ch.id(),
                c.classification.confidence
            );
        }
        let v = ch.verdict().unwrap_or_else(|| {
            panic!(
                "[{T976}] {}: P25-like with no trunking verdict on the row to say how many syncs",
                ch.id()
            )
        });
        let syncs = v
            .evidence
            .iter()
            .find(|ev| ev.stage == Stage::S4Framing)
            .map_or(0.0, |ev| ev.raw);
        let extent_s = (e.emitter.last_seen.as_unix_nanos() - e.emitter.first_seen.as_unix_nanos())
            as f64
            * 1e-9;
        let ceiling = (extent_s / P25_FRAME_S).ceil();
        assert!(
            syncs >= f64::from(MIN_SYNC_HITS) && syncs <= ceiling,
            "[{T976}] {}: the verdict states {syncs} frame syncs; P25-like needs at least \
             {MIN_SYNC_HITS}, and a {extent_s:.2} s region holds at most {ceiling} P25 frames",
            ch.id()
        );
    }
}

/// **Ticket assertion (4): decode is MARGINAL — the row carries the chain's verdict.**
///
/// What a passing run proves: a trunking analysis row (`EmitterSynthesis`, engine
/// `hk-pipeline/trunk-synth`) is filed on the region, so the served `resolution` is a finished
/// search rather than `not-searched`, with a reason; and its evidence says what was tried and what
/// came of it — a framing stage (frame syncs found, from the demodulated channel) and a check
/// stage (how many blocks were CRC-checked and how many were valid). CRC-valid TSBK/LDU frames are
/// **not** asserted: three syncs in the clip is not enough to demand one, and the answer key has
/// no decode to check one against.
///
/// Red today: `resolution: not-searched` and no trunking row. On a 5 s clip the built-in hunt
/// (`period_s` 10 s) makes one pass, at the start, before blind detection has written the emitter a
/// verdict would be filed on — so the channel is demodulated and the finding is dropped.
#[test]
#[ignore = "T-976 PROVES THE GAP AND IS EXPECTED TO FAIL: the region reads resolution \
            not-searched; the built-in hunt's one pass on a 5 s clip lands before the emitter \
            exists and files 0 verdicts. Green when the hunt-verdict-on-a-short-clip ticket filed \
            from T-976's hand-back lands. Run with --run-ignored all"]
fn p25_f_the_row_carries_the_chains_decode_verdict() {
    let Some(set) = armed() else { return };
    assert_nothing_was_commanded();
    for ch in &set {
        region_of(ch);
        let served = &ch.api_row().expect("the region is served")["resolution"];
        let v = ch.verdict().unwrap_or_else(|| {
            panic!(
                "[{T976}] {}: no trunking verdict on the region and it is served as resolution \
                 {served}; the hunt made {} pass(es), demodulated {} channel(s) and filed {} \
                 verdict(s)",
                ch.id(),
                hunt(ch.run, "cc_passes"),
                hunt(ch.run, "cc_demods"),
                hunt(ch.run, "cc_verdicts"),
            )
        });
        let res = v.resolution.as_ref().unwrap_or_else(|| {
            panic!(
                "[{T976}] {}: the verdict resolves nothing, which only a solved (decoded) \
                 channel may: {:?}",
                ch.id(),
                v.verdict
            )
        });
        assert!(
            res.kind != ResolutionKind::NotSearched && res.reason.is_some(),
            "[{T976}] {}: a demodulated channel's verdict still reads {res:?}",
            ch.id()
        );
        assert_ne!(
            served["kind"].as_str(),
            Some("not-searched"),
            "[{T976}] {}: the verdict is filed and /api/inventory still serves not-searched",
            ch.id()
        );
        let framing = v.evidence.iter().find(|ev| ev.stage == Stage::S4Framing);
        let check = v.evidence.iter().find(|ev| ev.stage == Stage::S5Check);
        assert!(
            framing.is_some_and(|f| f.raw >= 1.0),
            "[{T976}] {}: the verdict does not state the frame syncs it found (the oracle found \
             {}): {:?}",
            ch.id(),
            ch.oracle_syncs_s().len(),
            framing
        );
        assert!(
            check.is_some_and(|c| c.raw <= c.n as f64),
            "[{T976}] {}: the verdict does not state its CRC outcome (valid of checked): {:?}",
            ch.id(),
            check
        );
    }
}
