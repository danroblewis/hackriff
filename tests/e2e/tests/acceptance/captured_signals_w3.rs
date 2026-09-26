//! **The "all captured signals decode" set, window-3 members** (T-986; `SIGNAL-090`, `SIGNAL-091`):
//! the explorer agent's 2026-09-25 window-3 land-mobile captures, replayed blind through the mock
//! SDR exactly as the FM/RDS members in [`captured_signals`](super::captured_signals) are.
//!
//! ```text
//! cargo nextest run -p hk-e2e -E 'binary(acceptance_captured_signals) & test(/^w3_/)'
//! cargo nextest run -p hk-e2e -E 'binary(acceptance_captured_signals) & test(/^w3_/)' --run-ignored all
//! ```
//!
//! # The two captures, and what the evidence says they hold
//!
//! | Capture | Explorer's claim | What the signal holds (hidden truth, `hackriff:truth`) |
//! |---|---|---|
//! | `lmr-461p125-nbfm-ctcss` (external store, 15 s) | an 8.1 s **NBFM voice** burst at 461.125 MHz with a **233.6 Hz CTCSS** tone | a **DMR base station**: `dmr_ref.py` finds 105 BS-data syncs in `[6.9 s, 15.0 s)`, every spacing a whole number of 30 ms bursts, none before the burst. **No CTCSS tone.** Also DMR at 462.225 MHz (36 syncs on the grid). |
//! | `dmr-464p6125-bs` (LFS, 5 s tail trim) | DMR BS data sync ×42 at 464.6125 MHz; a continuous unknown carrier at 463.4 MHz | 41 BS-data syncs at 464.6125 MHz; **463.4 MHz is DMR too** (152 syncs on the grid, continuous). |
//!
//! **The CTCSS dispute is settled against both claimed tones** (the ticket asked for exactly this
//! before any value was asserted). The explorer's `tools/nbfm3.py`, re-run on this clip, still
//! reads **233.0 Hz**; `py/fixtures/ctcss_ref.py` still reads **99.88 Hz**. Both are right about
//! the line they found and wrong to call it a tone: the burst's discriminator carries a **comb of
//! lines on a 16.67 Hz grid** (66.6, 100.0, 166.7, 200.0, 216.7, 233.3 Hz …, each 20–28 dB over the
//! band median), and 16.67 Hz is 1/60 ms — DMR's two-slot TDMA frame (ETSI TS 102 361-1 §4.2). 100 Hz
//! is its 6th harmonic and 233.3 Hz its 14th (a lock-in at 233.6 Hz drifts −0.27 Hz/s: the line is
//! at 233.33, not the table's 233.6). The DMR oracle then confirms what the comb implies. A 4FSK
//! TDMA emission has no analogue sub-audible tone at all, so the evidence-supported value is
//! **"no CTCSS tone"**, which is what T-988's comb guard already answers on this clip
//! (`crates/hk-demod/tests/subaudible_window3.rs`). The settlement is recorded in the fixture's own
//! truth (`ctcss.settled`, written by `py/fixtures/build_explorer_2026_09_25.py`, with both tones'
//! places on the frame comb), not in this file.
//!
//! So the members are the four **DMR emissions** the oracle identifies (`/dmr/identified_dmr`:
//! enough syncs, ≥ 90 % of their spacings on the 30 ms burst grid) plus the **no-tone** assertion
//! on the one emission a tone was claimed for. A later window-3-style capture joins by adding a line
//! to [`CAPTURES`]; its emissions join every test below through their truth, not through this code.
//!
//! # Blind, through the device interface
//!
//! Exactly the FM members' discipline: the mock SDR serves the recording
//! ([`blind_replay`] → `hk_pipeline::open_mock_replay`), the plan is `json!({})`, nothing tells the
//! run a frequency, a modulation or a protocol, the device is given **no controls** (asserted), and
//! the truth is sealed until after the run and read only to check the answer. [`report`] prints
//! everything the run produced before any truth is opened.
//!
//! # The members, and which ticket turns each red green
//!
//! Measured 2026-09-26 on this branch (main at 54a22653, T-988 landed):
//!
//! | Test | Ticket assertion | Red today because | Turned green by |
//! |---|---|---|---|
//! | [`w3_a_every_member_is_detected_blind_and_the_answer_key_is_sound`] | harness control: every member detected as a (t, f) region; the truth carries the oracle's verdicts | — **green control** | — |
//! | [`w3_b_the_461p125_burst_never_claims_a_ctcss_tone`] | (a) the tone half, settled: no CTCSS/tone reading anywhere on the burst | — **green control**, an honesty guard: T-988's comb guard must keep it green | — |
//! | [`w3_c_each_member_is_one_region_with_its_time_extent`] | (a)/(b) one (t, f) region per emission, with the burst's extent | 461.125 MHz **passes** (one Confirmed 461.1226 MHz / 14.1 kHz region, presence from 6.92 s, 7.95 s of the 7.95 s sync span) and so does 462.225 MHz; 464.6125 and 463.4 MHz are one region each but **18.8 kHz** wide — four detector bins for a 7K60FXD emission, over the 12.5 kHz channel + one bin | narrowband region-width refinement (not yet ticketed: T-986's hand-back) |
//! | [`w3_d_each_member_is_estimated_4fsk_at_4800_symbols_per_second`] | (b) estimated 4FSK 4800 sym/s | **no demodulation session at all** on any of the four (the runs attach 9 and 6 chains and write 0 demodulations) | T-989 |
//! | [`w3_e_each_member_is_identified_dmr_from_its_syncs`] | (a)+(b) identified DMR from sync evidence, sync count n/m | no `dmr…` decode on any member (oracle: 105, 36, 41, 152 syncs); every row's family is `None` or `"unknown"` | T-989 |
//! | [`w3_f_a_land_mobile_explanation_ranks_for_each_member`] | "a land-mobile explanation in the top-k" | the only explanation on every member is `unidentified` | a band-plan row for 450–470 MHz (not yet ticketed: T-986's hand-back) |
//!
//! **Header decode is the next member, not this one.** The ticket asks for slot type, colour code
//! and data-header fields "where the oracle/truth provides them"; it does not: `dmr_ref.py` is a
//! sync oracle (PHY facts only — CLAUDE.md legal guardrails keep payload out of fixture truth), so
//! the truth has sync counts, patterns and times and nothing from the CACH or slot type. The member
//! that asserts header fields needs an oracle that decodes the slot type (Golay(20,8)) and the
//! colour code first; `w3_e_…` names it in its failure message.
//!
//! The capture `lmr-461p125-nbfm-ctcss` is **external** (72 MB; `just fixtures-fetch`, or rebuild it
//! with `py/fixtures/build_explorer_2026_09_25.py` from the explorer's captures). Without it the
//! set is the DMR capture alone; with neither, every test skips (and `HK_REQUIRE_FIXTURES=1` makes
//! that a failure).

use std::sync::OnceLock;

use hk_e2e::blind::{matching, truth_emissions};
use hk_e2e::{Fixture, TruthItem};
use hk_model::{
    Decode, Demodulation, Detection, FreqRange, IdleGap, InventoryEntry, InventoryQuery,
    LinkTarget, Region, SubaudibleKind, Timestamp, Watched,
};
use serde_json::Value;

use crate::blind::{BlindRun, BlindSource, TOP_K, blind_replay};
use crate::common::*;

/// The use cases these members assert on.
const UC: &str = "T-986/SIGNAL-090/SIGNAL-091";

/// **The window-3 set.** `(directory, fixture stem, external)`; add a line to enlist a new
/// capture. An **external** capture (`fixtures/manifest.json` `status: external`) lives in the
/// gitignored store, which no checkout is guaranteed to have, so it is skipped with a message when
/// absent even under `HK_REQUIRE_FIXTURES=1`; a committed (LFS) one is not.
const CAPTURES: &[(&str, &str, bool)] = &[
    (
        "fixtures/store/explorer-2026-09-25",
        "lmr-461p125-nbfm-ctcss",
        true,
    ),
    (
        "fixtures/hackrf/explorer-2026-09-25",
        "dmr-464p6125-bs",
        false,
    ),
];

/// Whether an external capture's data is in some store this checkout can see (the worktree's
/// own, or an enclosing checkout's — `real_fixture_in` searches the same way).
fn external_present(dir: &str, name: &str) -> bool {
    let rel = std::path::Path::new(dir).join(format!("{name}.sigmf-data"));
    let mut d = Some(hk_e2e::paths::repo_root());
    while let Some(p) = d {
        if p.join(&rel).is_file() {
            return true;
        }
        d = p.parent().map(std::path::Path::to_path_buf);
    }
    false
}

// -------------------------------------------------------------------------------------------
// A-priori tolerances. Derived before any run; each says from what.
// -------------------------------------------------------------------------------------------

/// A land-mobile channel's spacing, Hz: the 12.5 kHz narrowband raster (47 CFR 90.209(b)(5)), which
/// is also DMR's channel (ETSI TS 102 361-1 §4.1: two slots in one 12.5 kHz channel).
const CHANNEL_HZ: f64 = 12.5e3;

/// The detector's frequency resolution on these 2.4 Msps captures, Hz (512-bin STFT: 4687.5 Hz a
/// bin, the run's own `resolution:` line). A region may be one bin wider than its channel, never
/// more: a box wider than that has swallowed a neighbour or is not the emission's box.
const DETECT_BIN_HZ: f64 = 2.4e6 / 512.0;

/// How close a region's centre must be to the emission's channel, Hz: half a channel, the
/// raster's own ambiguity. A region further out is on the next channel.
const CENTER_TOL_HZ: f64 = 0.5 * CHANNEL_HZ;

/// How far a region's first presence may sit from the burst's first oracle sync, s. Ten times
/// coarser than any detector frame here (≤ 0.1 s) and eight times finer than the 8.1 s burst, so a
/// pass means the region starts *with* the burst rather than somewhere in the capture.
const START_TOL_S: f64 = 1.0;

/// The share of the sync-evidenced on-air span `[first sync, last sync]` a region's recorded
/// presence must cover. Carried unchanged from the FM members' `MIN_EXTENT_COVERAGE`: a coarse
/// floor, read off presence intervals (docs/07 §2.27), never the emitter's first/last-seen hull.
const MIN_EXTENT_COVERAGE: f64 = 0.5;

/// DMR's symbol rate, Bd (ETSI TS 102 361-1 §10.2: 4800 symbols/s, 9600 bit/s).
const DMR_SYMBOL_RATE: f64 = 4800.0;

/// How close an estimated symbol rate must be to 4800 Bd. The neighbouring land-mobile 4FSK
/// rates are 2400 Bd (NXDN48, dPMR) and 9600 Bd — a factor of two away — so ±2 % is unambiguous
/// with a wide margin, and far looser than the receiver clock error (−5.8 to −8.0 ppm measured on
/// the explorer's own FM captures).
const SYMBOL_RATE_TOL: f64 = 0.02;

/// The frame-model prefix a DMR identification is recorded under (a `docs/07` Decode, like
/// `rds-pi` for RDS): T-989 names its decode rows `dmr-…`. The only place this suite fixes it.
const DMR_FRAME_MODEL_PREFIX: &str = "dmr";

/// The Decode metadata keys a DMR identification states its sync evidence under (T-989's "sync
/// n/m"); the first one present is read.
const SYNC_COUNT_KEYS: &[&str] = &["syncs", "sync_count", "n_syncs"];

/// The share of the oracle's sync count the run's own identification must report. The oracle's
/// count is a **lower bound** on the evidence the clip holds (it is one searcher, and a better one
/// finds more), so this is a floor, and half is far from both "one lucky hit" and "every sync".
const MIN_SYNC_SHARE: f64 = 0.5;

/// The table the settled tone verdict rules out: every CTCSS reading the dispute produced.
const DISPUTED_TONES_HZ: &[f64] = &[233.6, 100.0];

// -------------------------------------------------------------------------------------------
// The set.
// -------------------------------------------------------------------------------------------

/// One capture, replayed blind through the mock SDR.
pub struct W3Run {
    /// Fixture stem.
    pub name: &'static str,
    /// Data directory (SQLite, tiles).
    pub dir: TempDir,
    /// Run summary.
    pub summary: hk_pipeline::RunSummary,
    /// The private truth, read only after the run.
    pub fx: Fixture,
    /// `/api/inventory` rows, with their explanations and `estimated_params`.
    pub api_rows: Vec<Value>,
    /// The inventory at stop (every lifecycle state).
    pub inventory: Vec<InventoryEntry>,
    /// Every detection of the run.
    pub detections: Vec<Detection>,
    /// Controls the mock device was given: **0** is the no-lookup-and-tune evidence.
    pub control_changes: u64,
}

fn replay(dir: &'static str, name: &'static str) -> Option<W3Run> {
    let meta = real_fixture_in(dir, name)?;
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
    Some(W3Run {
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

/// Every capture this checkout can replay, replayed once per process.
fn runs() -> &'static [W3Run] {
    static RUNS: OnceLock<Vec<W3Run>> = OnceLock::new();
    RUNS.get_or_init(|| {
        CAPTURES
            .iter()
            .filter(|(d, n, external)| {
                let skip = *external && !external_present(d, n);
                if skip {
                    eprintln!(
                        "SKIP {n}: external capture not in any store (`just fixtures-fetch {n}`)"
                    );
                }
                !skip
            })
            .filter_map(|(d, n, _)| replay(d, n))
            .collect()
    })
}

/// One member: a truth emission the oracle identified as DMR, and the run it was captured in.
pub struct Member {
    /// The run that replayed it.
    pub run: &'static W3Run,
    /// Its private truth.
    pub truth: &'static TruthItem,
}

impl Member {
    fn id(&self) -> String {
        format!(
            "{} / {}",
            self.run.name,
            self.truth.label.as_deref().unwrap_or("<unlabelled>")
        )
    }

    /// The oracle's sync count in the emission's window.
    fn oracle_syncs(&self) -> u32 {
        self.truth.f64("/dmr/n_syncs").unwrap_or(0.0) as u32
    }

    /// The sync-evidenced on-air span, in the run's clock: first to last oracle sync.
    fn on_air(&self) -> (Timestamp, Timestamp) {
        let t0 = crate::blind::recording_start(&self.run.fx);
        let at = |k: &str| {
            let s = self.truth.f64(k).expect("identified member has sync times");
            t0.saturating_add_nanos((s * 1e9) as i64)
        };
        (at("/dmr/first_sync_s"), at("/dmr/last_sync_s"))
    }

    /// The inventory regions on this emission's channel (centre within half a channel, extents
    /// overlapping in frequency; the suite's shared matching rule).
    fn regions(&self) -> Vec<&'static InventoryEntry> {
        matching(
            self.truth,
            0.0,
            &self.run.inventory,
            |e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz),
            CENTER_TOL_HZ,
        )
    }

    /// Detections overlapping the channel inside the on-air span.
    fn detections_on_channel(&self) -> Vec<&'static Detection> {
        let (a, b) = self.on_air();
        self.run
            .detections
            .iter()
            .filter(|d| {
                (d.f_center_hz - self.truth.center_hz()).abs() <= CENTER_TOL_HZ
                    && d.time.end >= a
                    && d.time.start <= b
            })
            .collect()
    }

    /// `(start, seconds of presence inside the on-air span)` of region `e`, off presence intervals.
    fn presence(&self, e: &InventoryEntry) -> (Option<Timestamp>, f64) {
        let (a, b) = self.on_air();
        let iv = repo(&self.run.dir.0)
            .presence_intervals(
                e.emitter.id,
                IdleGap::continuous(),
                ever().end,
                // T-940: unrecorded = the original elapsed-silence reading (as captured_signals.rs).
                &Watched::unrecorded(),
            )
            .unwrap();
        let start = iv.iter().map(|p| p.time.start).min();
        let covered = iv
            .iter()
            .map(|p| {
                let lo = p.time.start.as_unix_nanos().max(a.as_unix_nanos());
                let hi = p.time.end.as_unix_nanos().min(b.as_unix_nanos());
                ((hi - lo).max(0)) as f64 * 1e-9
            })
            .sum();
        (start, covered)
    }

    /// Every demodulation session on one of this member's regions.
    fn demods(&self) -> Vec<Demodulation> {
        let repo = repo(&self.run.dir.0);
        self.regions()
            .iter()
            .flat_map(|e| repo.demodulations_for_emitter(e.emitter.id, 1000).unwrap())
            .collect()
    }

    /// Every decode on one of this member's regions: linked, and provisional.
    fn decodes(&self) -> Vec<Decode> {
        let repo = repo(&self.run.dir.0);
        let mut out = Vec::new();
        for e in self.regions() {
            for l in repo.emitter_links(e.emitter.id).unwrap() {
                if let LinkTarget::Decode(id) = l.target {
                    out.push(repo.decode(id).unwrap());
                }
            }
            out.extend(repo.provisional_decodes_of_emitter(e.emitter.id).unwrap());
        }
        out
    }

    /// This member's `/api/inventory` rows (by region centre).
    fn api_rows(&self) -> Vec<&'static Value> {
        self.regions()
            .iter()
            .filter_map(|e| {
                self.run.api_rows.iter().find(|r| {
                    (r["f_center_hz"].as_f64().unwrap_or(f64::NAN) - e.emitter.f_center_hz).abs()
                        < 1.0
                })
            })
            .collect()
    }
}

/// Every emission of the set the DMR oracle identified: the members.
fn members() -> Vec<Member> {
    runs()
        .iter()
        .flat_map(|run| {
            truth_emissions(&run.fx)
                .into_iter()
                .filter(|t| t.bool("/dmr/identified_dmr") == Some(true))
                .map(move |truth| Member { run, truth })
        })
        .collect()
}

/// Everything the set produced, printed **before** any truth is read.
fn report(members: &[Member]) {
    for run in runs() {
        let c = &run.summary.counters["chains"];
        eprintln!(
            "[{UC}] {}: {} detections, {} inventory rows, {} chains attached, {} demodulations, \
             {} decodes, {} device controls, {} lost samples",
            run.name,
            run.detections.len(),
            run.inventory.len(),
            c["attached"],
            c["demodulations"],
            c["decodes"],
            run.control_changes,
            run.summary.always_on_lost_samples,
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
    let t0 = |m: &Member| crate::blind::recording_start(&m.run.fx).as_unix_nanos();
    for m in members {
        eprintln!(
            "[{UC}] {}: {} detection(s) on this channel in its on-air span; regions:",
            m.id(),
            m.detections_on_channel().len()
        );
        for e in m.regions() {
            let (start, covered) = m.presence(e);
            eprintln!(
                "  {:.4} MHz bw {:.1} kHz {:?} family {:?}; presence from {:?} s, {covered:.2} s \
                 inside the on-air span",
                e.emitter.f_center_hz / 1e6,
                e.emitter.bandwidth_hz / 1e3,
                e.lifecycle,
                e.family,
                start.map(|s| (s.as_unix_nanos() - t0(m)) as f64 * 1e-9),
            );
        }
        for d in m.demods() {
            eprintln!(
                "  demod {} sym {:?} order {:?} dev {:?} bw {:?} subaudible {:?}",
                d.mode,
                d.params.symbol_rate_hz,
                d.params.mod_order,
                d.params.deviation_hz,
                d.params.bandwidth_hz,
                d.params.subaudible.as_ref().map(|s| (s.kind, &s.tones)),
            );
        }
        for d in m.decodes() {
            eprintln!(
                "  decode {} {:?} {}",
                d.frame_model, d.crc_status, d.metadata
            );
        }
        for r in m.api_rows() {
            eprintln!(
                "  api explanations {} estimated_params {}",
                r["explanations"]
                    .as_array()
                    .map(|x| x
                        .iter()
                        .take(TOP_K)
                        .map(|e| e["service"].to_string())
                        .collect::<Vec<_>>()
                        .join(","))
                    .unwrap_or_default(),
                r["estimated_params"]
            );
        }
    }
}

/// The members, or `None` when no capture is fetched (skip).
fn armed() -> Option<Vec<Member>> {
    if runs().is_empty() {
        eprintln!("SKIP {UC}: no window-3 capture is fetched");
        return None;
    }
    let m = members();
    report(&m);
    Some(m)
}

/// The no-lookup-and-tune guard every test runs first.
fn assert_nothing_was_commanded() {
    for run in runs() {
        assert_eq!(
            run.control_changes, 0,
            "[{UC}] {}: the mock device was given {} control change(s); this suite passes no \
             frequency in and commands no tune",
            run.name, run.control_changes
        );
        assert_eq!(
            run.summary.always_on_lost_samples, 0,
            "[{UC}] {}: the always-on readers lost samples, so any red below could be missing \
             data rather than a missing capability",
            run.name
        );
    }
}

// -------------------------------------------------------------------------------------------
// (a) THE HARNESS CONTROL.
// -------------------------------------------------------------------------------------------

/// **Harness control: every member is detected blind as a time–frequency region, and the answer
/// key is sound.**
///
/// What a passing run proves: every DMR emission the oracle identified produced `docs/07`
/// Detections on its channel inside its sync-evidenced on-air span, from the IQ alone, with the
/// truth sealed and the device never commanded; and the answer key says what the reds below rely
/// on — each member's truth was written by `dmr_ref.py`, with a sync count, a sync span and the
/// slot-grid verdict, and the DMR capture's named emission (464.6125 MHz) is among the members.
/// If this goes red the harness or the fixture is broken, not a capability. Keep it green.
#[test]
fn w3_a_every_member_is_detected_blind_and_the_answer_key_is_sound() {
    let Some(members) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for m in &members {
        let decoder = m.truth.str("/dmr/decoder").unwrap_or_default();
        if !decoder.contains("dmr_ref.py") {
            failures.push(format!("{}: truth not written by dmr_ref.py", m.id()));
        }
        if m.truth.f64("/dmr/slot_grid_fraction").unwrap_or(0.0) < 0.9 || m.oracle_syncs() < 8 {
            failures.push(format!(
                "{}: truth's DMR verdict is not the oracle's rule",
                m.id()
            ));
        }
        if m.detections_on_channel().is_empty() {
            failures.push(format!(
                "{}: no detection on {:.4} MHz inside its on-air span",
                m.id(),
                m.truth.center_hz() / 1e6
            ));
        }
    }
    let named = |f: f64| {
        members
            .iter()
            .any(|m| (m.truth.center_hz() - f).abs() < 1.0)
    };
    for run in runs() {
        if run.name == "dmr-464p6125-bs" && !named(464.6125e6) {
            failures.push("464.6125 MHz is not a member: the DMR answer key is broken".into());
        }
        if run.name == "lmr-461p125-nbfm-ctcss" && !named(461.125e6) {
            failures.push("461.125 MHz is not a member: the settled truth is missing".into());
        }
    }
    assert!(
        !members.is_empty(),
        "[{UC}] no member at all: the fixtures carry no oracle-identified DMR emission"
    );
    assert!(failures.is_empty(), "[{UC}] {failures:#?}");
}

// -------------------------------------------------------------------------------------------
// (b) THE SETTLED TONE. Green today and must stay green.
// -------------------------------------------------------------------------------------------

/// **Ticket assertion (a), the tone half, as the evidence settled it: no CTCSS tone is claimed on
/// the 461.125 MHz burst.**
///
/// What a passing run proves: for every emission whose truth carries a `ctcss.settled` verdict of
/// `none` (the 461.125 MHz burst — see the module docs for the 16.67 Hz frame comb and the 105 DMR
/// syncs), **nothing** in the run reports a sub-audible tone on it — no Demodulation's
/// `params.subaudible` of kind `ctcss` or `tone`, no `estimated_params.subaudible` on the API row,
/// and no decode or API field carrying either disputed value (233.6 Hz, 100 Hz). A reading of
/// `none` (T-988's comb guard) is the right answer where the system looks; not looking is honest
/// too. What would turn this red is the defect the dispute was about: a tone snapped to the table
/// from a line of a digital emission's frame comb.
///
/// It also checks it is not passing vacuously: the burst must reach the inventory as a region.
#[test]
fn w3_b_the_461p125_burst_never_claims_a_ctcss_tone() {
    let Some(members) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    let mut checked = 0;
    for m in members
        .iter()
        .filter(|m| m.truth.str("/ctcss/settled/subaudible_kind") == Some("none"))
    {
        checked += 1;
        if m.regions().is_empty() {
            failures.push(format!(
                "{}: no inventory region, so the no-tone check has nothing to check",
                m.id()
            ));
        }
        for d in m.demods() {
            if let Some(s) = &d.params.subaudible
                && matches!(s.kind, SubaudibleKind::Ctcss | SubaudibleKind::Tone)
            {
                failures.push(format!(
                    "{}: demodulation {} reports sub-audible {:?} {:?} on a DMR emission",
                    m.id(),
                    d.mode,
                    s.kind,
                    s.tones
                ));
            }
        }
        for r in m.api_rows() {
            let kind = r["estimated_params"]["subaudible"]["kind"].as_str();
            if matches!(kind, Some("ctcss" | "tone")) {
                failures.push(format!(
                    "{}: /api/inventory estimated_params.subaudible is {kind:?}: {}",
                    m.id(),
                    r["estimated_params"]["subaudible"]
                ));
            }
        }
        for d in m.decodes() {
            let text = d.metadata.to_string().to_ascii_lowercase();
            if text.contains("ctcss") {
                failures.push(format!(
                    "{}: decode {} names a CTCSS reading: {}",
                    m.id(),
                    d.frame_model,
                    d.metadata
                ));
            }
        }
        for r in m.api_rows() {
            for hz in DISPUTED_TONES_HZ {
                if let Some(v) = r["estimated_params"]["subaudible"]["tones"]
                    .as_array()
                    .and_then(|t| t.first())
                    .and_then(|t| t["table_hz"].as_f64())
                    && (v - hz).abs() < 0.05
                {
                    failures.push(format!("{}: API row snapped to {hz} Hz", m.id()));
                }
            }
        }
    }
    let lmr_fetched = runs().iter().any(|r| r.name == "lmr-461p125-nbfm-ctcss");
    assert!(
        checked > 0 || !lmr_fetched,
        "[{UC}] the 461.125 MHz capture replayed but no member carries the settled no-tone \
         verdict: the fixture truth is stale (rebuild it with build_explorer_2026_09_25.py)"
    );
    assert!(failures.is_empty(), "[{UC}] {failures:#?}");
}

// -------------------------------------------------------------------------------------------
// (c)–(f) THE RED PROOFS.
// -------------------------------------------------------------------------------------------

/// **Ticket assertions (a)/(b), region half: each member is ONE time–frequency region, with the
/// burst's time extent.**
///
/// What a passing run proves: every oracle-identified DMR emission is exactly **one** inventory
/// region on its channel, no wider than the 12.5 kHz channel plus one detector bin, whose recorded
/// presence starts within [`START_TOL_S`] of the burst's first sync and covers at least
/// [`MIN_EXTENT_COVERAGE`] of `[first sync, last sync]` — the `[start, end?]` region CLAUDE.md
/// defines a signal to be, not a list of fragments and not a carrier with no extent.
#[test]
#[ignore = "T-986 PROVES THE GAP AND IS EXPECTED TO FAIL. `#[ignore]`d only so one known-red \
            proof does not pin a gate red. Run with `--run-ignored all`. Deleted by the ticket \
            that refines a narrowband region's width (not yet ticketed: T-986's hand-back): \
            464.6125 and 463.4 MHz are boxed 18.8 kHz for a 7.6 kHz DMR emission."]
fn w3_c_each_member_is_one_region_with_its_time_extent() {
    let Some(members) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for m in &members {
        let regions = m.regions();
        if regions.len() != 1 {
            failures.push(format!(
                "{}: {} region(s) on {:.4} MHz, want exactly 1: {:?}",
                m.id(),
                regions.len(),
                m.truth.center_hz() / 1e6,
                regions
                    .iter()
                    .map(|e| (e.emitter.f_center_hz / 1e6, e.emitter.bandwidth_hz / 1e3))
                    .collect::<Vec<_>>()
            ));
            continue;
        }
        let e = regions[0];
        if e.emitter.bandwidth_hz > CHANNEL_HZ + DETECT_BIN_HZ {
            failures.push(format!(
                "{}: region {:.1} kHz wide, over the {:.1} kHz channel + one {:.1} kHz bin",
                m.id(),
                e.emitter.bandwidth_hz / 1e3,
                CHANNEL_HZ / 1e3,
                DETECT_BIN_HZ / 1e3
            ));
        }
        let (a, b) = m.on_air();
        let span = (b.as_unix_nanos() - a.as_unix_nanos()) as f64 * 1e-9;
        let (start, covered) = m.presence(e);
        match start {
            Some(s)
                if ((s.as_unix_nanos() - a.as_unix_nanos()) as f64 * 1e-9).abs() <= START_TOL_S => {
            }
            other => failures.push(format!(
                "{}: presence starts at {:?}, want within {START_TOL_S} s of the first sync",
                m.id(),
                other.map(|s| (s.as_unix_nanos() - a.as_unix_nanos()) as f64 * 1e-9)
            )),
        }
        if covered < MIN_EXTENT_COVERAGE * span {
            failures.push(format!(
                "{}: presence covers {covered:.2} s of the {span:.2} s on-air span",
                m.id()
            ));
        }
    }
    assert!(failures.is_empty(), "[{UC}] {failures:#?}");
}

/// **Ticket assertion (b): each member is estimated 4FSK at 4800 sym/s.**
///
/// What a passing run proves: a demodulation session on each DMR emission's region measured
/// `mod_order` 4 and a symbol rate within [`SYMBOL_RATE_TOL`] of 4800 Bd — the blind parameter
/// estimate (C13/C14) that DMR identification, and any demodulation of it, starts from. Measured
/// values only: an absent estimate is a red, never a default.
#[test]
#[ignore = "T-986 PROVES THE GAP AND IS EXPECTED TO FAIL. `#[ignore]`d only so one known-red \
            proof does not pin a gate red. Run with `--run-ignored all`. T-989 deletes this line \
            (DMR identification on any 4FSK 4800 sym/s emission starts from this estimate)."]
fn w3_d_each_member_is_estimated_4fsk_at_4800_symbols_per_second() {
    let Some(members) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for m in &members {
        let ok = m.demods().iter().any(|d| {
            d.params.mod_order == Some(4)
                && d.params.symbol_rate_hz.is_some_and(|r| {
                    (r - DMR_SYMBOL_RATE).abs() <= SYMBOL_RATE_TOL * DMR_SYMBOL_RATE
                })
        });
        if !ok {
            failures.push(format!(
                "{}: no demodulation estimated 4FSK at 4800 Bd; sessions: {:?}",
                m.id(),
                m.demods()
                    .iter()
                    .map(|d| (d.mode.clone(), d.params.mod_order, d.params.symbol_rate_hz))
                    .collect::<Vec<_>>()
            ));
        }
    }
    assert!(failures.is_empty(), "[{UC}] {failures:#?}");
}

/// **Ticket assertions (a)+(b): each member is identified DMR from its sync evidence, with the
/// count.**
///
/// What a passing run proves: each DMR emission's region carries a `docs/07` Decode whose
/// `frame_model` starts `dmr` (the identification, as `rds-pi` is RDS's), stating its sync count,
/// and that count is at least [`MIN_SYNC_SHARE`] of the syncs the independent oracle found in the
/// same window — the "sync n/m" verdict on the row, measured, not asserted from a band plan. Where
/// the run names the sync pattern it must be the one the oracle saw (`BS_data` on every member).
///
/// The **header** (slot type, colour code, data header) is the next member, not this one: the truth
/// holds sync evidence only (module docs).
#[test]
#[ignore = "T-986 PROVES THE GAP AND IS EXPECTED TO FAIL. `#[ignore]`d only so one known-red \
            proof does not pin a gate red. Run with `--run-ignored all`. T-989 (conventional DMR \
            identification) deletes this line; the header-decode member follows it."]
fn w3_e_each_member_is_identified_dmr_from_its_syncs() {
    let Some(members) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for m in &members {
        let want = m.oracle_syncs();
        let ids: Vec<Decode> = m
            .decodes()
            .into_iter()
            .filter(|d| d.frame_model.starts_with(DMR_FRAME_MODEL_PREFIX))
            .collect();
        let best = ids
            .iter()
            .filter_map(|d| {
                SYNC_COUNT_KEYS
                    .iter()
                    .find_map(|k| d.metadata.get(*k).and_then(Value::as_u64))
            })
            .max();
        match best {
            Some(n) if n as f64 >= MIN_SYNC_SHARE * f64::from(want) => {}
            other => failures.push(format!(
                "{}: not identified DMR from its syncs: {} dmr decode(s), best sync count {other:?} \
                 against the oracle's {want} (need >= {:.0}); then the header member (slot type, \
                 colour code) follows",
                m.id(),
                ids.len(),
                MIN_SYNC_SHARE * f64::from(want)
            )),
        }
        for d in &ids {
            if let Some(p) = d.metadata.get("sync_pattern").and_then(Value::as_str)
                && m.truth.f64(&format!("/dmr/syncs_by_pattern/{p}")).is_none()
            {
                failures.push(format!(
                    "{}: identified by sync pattern {p}, which the oracle never saw ({})",
                    m.id(),
                    m.truth
                        .get("/dmr/syncs_by_pattern")
                        .cloned()
                        .unwrap_or_default()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "[{UC}] {failures:#?}");
}

/// **"A land-mobile explanation in the top-k", for each member.**
///
/// What a passing run proves: each DMR emission's inventory row carries a `land-mobile` allocation
/// among its top-[`TOP_K`] explanations — the band plan offering a ranked, reasoned suggestion
/// *after* blind detection (ADR-0017: an explainer, never a source of truth).
///
/// Red today for a reason outside the pipeline: `crates/hk-context/data/us-47cfr2106-compact.csv`
/// has **no row covering 450–470 MHz** (its UHF land-mobile row starts at the 470 MHz T-band), so no
/// explanation can rank here. Filed from T-986's hand-back, not yet ticketed.
#[test]
#[ignore = "T-986 PROVES THE GAP AND IS EXPECTED TO FAIL. `#[ignore]`d only so one known-red \
            proof does not pin a gate red. Run with `--run-ignored all`. Turned green by a \
            450-470 MHz land-mobile band-plan row (T-986's hand-back asks the coordinator to \
            ticket it) plus the members reaching the inventory (T-989)."]
fn w3_f_a_land_mobile_explanation_ranks_for_each_member() {
    let Some(members) = armed() else { return };
    assert_nothing_was_commanded();
    let mut failures = Vec::new();
    for m in &members {
        let rows = m.api_rows();
        let ranked = rows.iter().any(|r| {
            r["explanations"].as_array().is_some_and(|x| {
                x.iter()
                    .take(TOP_K)
                    .any(|e| e["service"].as_str() == Some("land-mobile"))
            })
        });
        if !ranked {
            failures.push(format!(
                "{}: no region carries a land-mobile allocation in its top-{TOP_K} ({} row(s): {:?})",
                m.id(),
                rows.len(),
                rows.iter()
                    .map(|r| r["explanations"]
                        .as_array()
                        .map(|x| x
                            .iter()
                            .take(TOP_K)
                            .map(|e| e["service"].to_string())
                            .collect::<Vec<_>>())
                        .unwrap_or_default())
                    .collect::<Vec<_>>()
            ));
        }
    }
    assert!(failures.is_empty(), "[{UC}] {failures:#?}");
}
