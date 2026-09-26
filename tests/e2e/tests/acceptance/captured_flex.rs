//! **"All captured signals decode" — the FLEX member** (T-950, `SIGNAL-088`).
//!
//! The set: every signal the explorer agent captures off the air becomes a blind acceptance test
//! of one shape (T-936 names it; T-545's `signal_087` is the pattern). This member is the
//! explorer's `flex-pagers-930p8`: 12 s at 930.8 MHz / 2.4 Msps, San Francisco, 2026-09-25, with
//! FLEX paging on three channels (3, 1 and 2 frame syncs) and no POCSAG — and, before T-950, no
//! FLEX decoder in the app at all.
//!
//! ```text
//! cargo nextest run -p hk-e2e -E 'binary(acceptance_mauto) & test(/captured_flex/)'
//! ```
//!
//! **Registration.** T-936 (the FM members) had not landed when this was written, so this is a
//! sibling file registered the way T-545's were: a module of `acceptance_mauto`, which runs under
//! `just acceptance-mauto` / `e2e_milestones` — a milestone target, never the per-merge gate.
//! When T-936 lands its set file, this member moves beside it unchanged.
//!
//! **The fixture is external** (57.6 MB, past the LFS cap; `fixtures/manifest.json`, T-949): fetch
//! it with `just fixtures-fetch --from <store>` into `fixtures/store/`. Without it every test here
//! prints `SKIP` and passes vacuously (the `real_fixture_in` rule); with `HK_REQUIRE_FIXTURES=1` a
//! missing fixture fails instead.
//!
//! # Blind
//!
//! The run is configured with `json!({})` — the built-in chain registry and nothing else. No
//! frequency, modulation, rate or protocol is passed; the only frequency the system sees is the
//! centre the mock device reports, as a HackRF reports it. The recording's `hackriff:truth` is
//! sealed by the vault before the device opens and read here **only after the run**, only to check
//! the answers (`blind.rs`). What the truth holds is PHY-level: channel centres, sync positions
//! (independent oracle `py/fixtures/flex_ref.py`) — no payload.
//!
//! # What each test proves when it passes
//!
//! | Test | Proves |
//! |---|---|
//! | `a_…` | each channel is found **blind** as a time–frequency region: a narrow detection covers every frame the truth holds, and the channel is **one** inventory emitter, not fragments |
//! | `b_…` | the emitter carries **measured** FSK parameters — data symbol rate, level count and outer deviation — on `docs/07`'s `estimated_params`, and the level count is the one the frames' codewords chose |
//! | `c_…` | the **FLEX decoder was chosen automatically**: decodes with `decoder_id: flex` are linked to the blind emitter, though nothing named FLEX, paging or the channel |
//! | `d_…` | **every** truth sync is a decoded frame at the right time, its frame information word **BCH(31,21)-valid**, its data codewords checking — and the frame numbers the FIWs carry advance with the sample clock across all three channels (the one claim here that is not circular: protocol arithmetic against the ADC's clock) |
//! | `e_…` | a **paging** explanation ranks in the top-k **as an explanation**: it rests on the decode, and the band plan names no identity |
//!
//! # The level-count dispute (T-949), and where this test stands on it
//!
//! The explorer called 929.608 and 929.933 MHz 4-level and 931.158 MHz 2-level; `flex_ref.py`
//! read one or two levels from histograms. Neither is truth here: the fixture records the
//! disagreement, and so this test does **not** assert a level count against truth. It asserts
//! that the emitter's level count is the one the decoder's **codeword evidence** chose
//! (`hk_demod::flex::demod`), and prints that evidence. The answer it found is in the T-950
//! hand-back and on the decodes: every frame on all three channels is 4-level.

use std::sync::{Arc, OnceLock};

use hk_e2e::{Fixture, TruthItem};
use hk_model::{CrcStatus, Decode, EmitterId};
use serde_json::{Value, json};

use crate::blind::{BlindSource, TOP_K, blind_config, recording_start, start};
use crate::common::*;

const T950: &str = "T-950/SIGNAL-088";
const FIXTURE_DIR: &str = "fixtures/store/explorer-2026-09-25";
const FIXTURE: &str = "flex-pagers-930p8";

/// A-priori figures, set from the FLEX description before any run (sigidwiki "FLEX"), never read
/// off a previous run.
pub mod apriori {
    /// Sync-1 and the FIW are always 2-level at this rate, Bd.
    pub const HEADER_BAUD: f64 = 1600.0;
    /// Data symbol rates FLEX defines, Bd.
    pub const DATA_BAUDS: [f64; 2] = [1600.0, 3200.0];
    /// Rate tolerance: 1 %, far inside the 2x step between the two rates.
    pub const RATE_TOL: f64 = 0.01;
    /// Outer FSK deviation, Hz (±4.8 kHz).
    pub const OUTER_DEVIATION_HZ: f64 = 4_800.0;
    /// ±20 %: the 4-level slicer's outer threshold sits at 2/3 of the outer level, so inside this
    /// every symbol still slices; beyond it the estimate is not of this modulation.
    pub const DEVIATION_TOL: f64 = 0.20;
    /// Half a 25 kHz paging channel: wider names a neighbouring channel.
    pub const CHANNEL_TOL_HZ: f64 = 12_500.0;
    /// A detection wider than this is not a channel-shaped region (a wideband transient).
    pub const MAX_CHANNEL_OBW_HZ: f64 = 100_000.0;
    /// Frame time tolerance, s: sixteen header symbols.
    pub const SYNC_TIME_TOL_S: f64 = 0.01;
    /// Detection time tolerance around a sync, s (a detector frame is tens of ms).
    pub const DETECT_TIME_TOL_S: f64 = 0.1;
    /// FLEX frame period, s.
    pub const FRAME_S: f64 = 1.875;
}

/// One truth channel.
struct Channel<'a> {
    truth: &'a TruthItem,
    /// Sync times, s from the recording start (oracle marker bit / 1600 Bd).
    syncs: Vec<f64>,
}

impl Channel<'_> {
    fn center_hz(&self) -> f64 {
        self.truth.center_hz()
    }
}

/// One blind run and what was read off it.
pub struct Run {
    dir: TempDir,
    rows: Vec<Value>,
    fx: Fixture,
}

impl Run {
    fn channels(&self) -> Vec<Channel<'_>> {
        self.fx
            .emissions()
            .into_iter()
            .filter(|t| t.kind == "flex-pager")
            .map(|t| Channel {
                syncs: t
                    .get("/paging/syncs")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s["bit"].as_f64())
                            .map(|bit| t.t_start_s + bit / apriori::HEADER_BAUD)
                            .collect()
                    })
                    .unwrap_or_default(),
                truth: t,
            })
            .collect()
    }

    fn rows_near(&self, f_hz: f64) -> Vec<&Value> {
        self.rows
            .iter()
            .filter(|r| {
                r["f_center_hz"]
                    .as_f64()
                    .is_some_and(|f| (f - f_hz).abs() <= apriori::CHANNEL_TOL_HZ)
            })
            .collect()
    }

    /// Seconds from the recording start.
    fn secs(&self, t: hk_model::Timestamp) -> f64 {
        (t.as_unix_nanos() - recording_start(&self.fx).as_unix_nanos()) as f64 / 1e9
    }

    /// Every decode linked (through its demodulation) to the emitter row `row`.
    fn decodes_of(&self, row: &Value) -> Vec<Decode> {
        let repo = repo(&self.dir.0);
        let Some(id) = row["id"].as_str().and_then(|s| s.parse::<EmitterId>().ok()) else {
            return Vec::new();
        };
        repo.decode_evidence_for_emitter(id)
            .unwrap_or_default()
            .iter()
            .filter_map(|e| repo.decode(e.decode_id).ok())
            .collect()
    }

    /// The FLEX frame decodes of the one emitter near `f_hz` (all emitters near it, if several).
    fn flex_frames(&self, f_hz: f64) -> Vec<Decode> {
        let mut out: Vec<Decode> = self
            .rows_near(f_hz)
            .into_iter()
            .flat_map(|r| self.decodes_of(r))
            .filter(|d| d.decoder_id == "flex" && d.frame_model == "flex-frame")
            .collect();
        out.sort_by_key(|d| d.t);
        out
    }
}

/// The blind run over the capture, once per process; `None` skips (fixture not fetched).
fn run() -> Option<Arc<Run>> {
    static RUN: OnceLock<Option<Arc<Run>>> = OnceLock::new();
    RUN.get_or_init(|| {
        let meta = real_fixture_in(FIXTURE_DIR, FIXTURE)?;
        let fx = Fixture::load(&meta).unwrap();
        // Nothing but the built-in registry: no frequency, modulation or protocol.
        let cfg = blind_config(&meta, "t950", BlindSource::default(), json!({}));
        let dir = cfg.dir;
        let handle = start(cfg.cfg, cfg.replay);
        let counters = handle.counters();
        let summary = finish(handle);
        let c = |p: &str| summary.counter(p);
        eprintln!(
            "[{T950}] run: detections {}, frames chains {} (refused {}, missed {}), FLEX frames {}, \
             pages {}, no-emitter {}",
            c("/detect/detections"),
            c("/chains/frames_attached"),
            c("/chains/frames_admission_refused"),
            c("/chains/frames_missed"),
            c("/chains/flex_frames"),
            c("/chains/flex_pages"),
            c("/chains/frames_no_emitter"),
        );
        let server = serve_api(&dir.0, counters);
        let (_, rows) = api_inventory(server.local_addr());
        Some(Arc::new(Run { dir, rows, fx }))
    })
    .clone()
}

/// Everything the run produced near each channel, printed before any assertion reads truth for
/// an answer, so a red test always shows what the system did see.
fn report(run: &Run) {
    for ch in run.channels() {
        let f = ch.center_hz();
        eprintln!("[{T950}] channel {:.4} MHz:", f / 1e6);
        for r in run.rows_near(f) {
            eprintln!(
                "[{T950}]   emitter {:.4} MHz bw {:.1} kHz family {:?} estimated_params {} \
                 explanations {:?}",
                r["f_center_hz"].as_f64().unwrap_or(f64::NAN) / 1e6,
                r["bandwidth_hz"].as_f64().unwrap_or(f64::NAN) / 1e3,
                r["family"].as_str(),
                r["estimated_params"],
                r["explanations"]
                    .as_array()
                    .map(|a| a
                        .iter()
                        .take(TOP_K)
                        .map(|e| (e["service"].as_str(), e["score"].as_f64()))
                        .collect::<Vec<_>>())
                    .unwrap_or_default(),
            );
        }
        for d in run.flex_frames(f) {
            let m = &d.metadata;
            eprintln!(
                "[{T950}]   FLEX frame t={:.3} s crc {:?} cycle {} frame {} declared {} measured {} \
                 clean {}/{} words, blocks {}, deviation {:.0} Hz, inner {:.3}",
                run.secs(d.t),
                d.crc_status,
                m["cycle"],
                m["frame"],
                m["declared"],
                m["measured"],
                m["clean_words"],
                m["words"],
                m["blocks_on_air"],
                m["deviation_hz"].as_f64().unwrap_or(f64::NAN),
                m["inner_fraction"].as_f64().unwrap_or(f64::NAN),
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// (A) Detection.
// ---------------------------------------------------------------------------------------------

/// **(1) Each channel is one burst (t, f) region, found blind.**
///
/// Passing proves the mock device, the vault and blind detection find every FLEX transmission's
/// centre **and time extent** from the IQ alone (a `docs/07` Detection covering each frame the
/// truth holds), and that the inventory holds the channel as **one** emitter. Red on `main`? No —
/// this is the control: it passed before T-950, and a red here means the harness, not the decoder.
#[test]
fn a_each_flex_channel_is_one_burst_region_found_blind() {
    let Some(run) = run() else { return };
    report(&run);
    let dets = repo(&run.dir.0)
        .detections_in_region(&hk_model::Region::new(
            hk_model::FreqRange::new(0.0, 7.0e9),
            ever(),
        ))
        .unwrap();
    let mut failures = Vec::new();
    for ch in run.channels() {
        let f = ch.center_hz();
        let near: Vec<_> = dets
            .iter()
            .filter(|d| {
                (d.f_center_hz - f).abs() <= apriori::CHANNEL_TOL_HZ
                    && d.obw_hz <= apriori::MAX_CHANNEL_OBW_HZ
            })
            .collect();
        for &t in &ch.syncs {
            let covered = near.iter().any(|d| {
                run.secs(d.time.start) - apriori::DETECT_TIME_TOL_S <= t
                    && run.secs(d.time.end) + apriori::DETECT_TIME_TOL_S >= t
            });
            if !covered {
                failures.push(format!(
                    "{:.4} MHz: no channel-shaped detection covers the frame at {t:.3} s \
                     (detections there: {:?})",
                    f / 1e6,
                    near.iter()
                        .map(|d| (run.secs(d.time.start), run.secs(d.time.end)))
                        .collect::<Vec<_>>(),
                ));
            }
        }
        let rows = run.rows_near(f);
        if rows.len() != 1 {
            failures.push(format!(
                "{:.4} MHz: {} inventory emitters within ±{:.1} kHz — one channel must be one \
                 region, not fragments or nothing",
                f / 1e6,
                rows.len(),
                apriori::CHANNEL_TOL_HZ / 1e3,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "[{T950}] (1) DETECTION:\n{}",
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------------------------
// (B) Parameters.
// ---------------------------------------------------------------------------------------------

/// **(2) The FSK parameters are estimated from the signal.**
///
/// Passing proves the channel's emitter carries, on `/api/inventory`'s `estimated_params`, a data
/// symbol rate within 1 % of a FLEX rate, a level count, and an outer deviation within ±20 % of
/// ±4.8 kHz — **measured**, and the level count the one the decoded frames' codewords chose
/// (their `measured.levels` majority), not a default. Red on `main`: the emitters there carry
/// `estimated_params: null` (nothing demodulated them).
#[test]
fn b_fsk_rate_levels_and_deviation_are_estimated() {
    let Some(run) = run() else { return };
    report(&run);
    let mut failures = Vec::new();
    for ch in run.channels() {
        let f = ch.center_hz();
        let Some(row) = run
            .rows_near(f)
            .into_iter()
            .find(|r| !r["estimated_params"].is_null())
        else {
            failures.push(format!(
                "{:.4} MHz: no emitter carries estimated_params (all null): nothing measured the \
                 FSK structure of this channel",
                f / 1e6
            ));
            continue;
        };
        let p = &row["estimated_params"];
        let rate = p["symbol_rate_hz"].as_f64();
        if !rate.is_some_and(|r| {
            apriori::DATA_BAUDS
                .iter()
                .any(|b| (r - b).abs() <= apriori::RATE_TOL * b)
        }) {
            failures.push(format!(
                "{:.4} MHz: symbol rate {rate:?} Bd is not a FLEX rate (1600/3200 ±1 %): {p}",
                f / 1e6
            ));
        }
        let dev = p["deviation_hz"].as_f64();
        if !dev.is_some_and(|d| {
            (d.abs() - apriori::OUTER_DEVIATION_HZ).abs()
                <= apriori::DEVIATION_TOL * apriori::OUTER_DEVIATION_HZ
        }) {
            failures.push(format!(
                "{:.4} MHz: deviation {dev:?} Hz, want ±4800 ±20 %: {p}",
                f / 1e6
            ));
        }
        let order = p["mod_order"].as_u64();
        // The level count must be the codewords' own answer.
        let frames = run.flex_frames(f);
        let fours = frames
            .iter()
            .filter(|d| d.metadata["measured"]["levels"].as_u64() == Some(4))
            .count();
        let twos = frames
            .iter()
            .filter(|d| d.metadata["measured"]["levels"].as_u64() == Some(2))
            .count();
        let majority = match (fours, twos) {
            (0, 0) => None,
            (a, b) if a >= b => Some(4),
            _ => Some(2),
        };
        eprintln!(
            "[{T950}] (2) {:.4} MHz: {rate:?} Bd, mod_order {order:?}, deviation {dev:?} Hz; \
             frames measured 4-level {fours}, 2-level {twos}",
            f / 1e6
        );
        if order.is_none() || order != majority {
            failures.push(format!(
                "{:.4} MHz: mod_order {order:?} is not the level count the frames' codewords \
                 chose ({majority:?}: 4-level {fours}, 2-level {twos})",
                f / 1e6
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "[{T950}] (2) PARAMETERS:\n{}",
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------------------------
// (C) Automatic choice of the chain.
// ---------------------------------------------------------------------------------------------

/// **(3) The FLEX chain is chosen automatically.**
///
/// Passing proves that with the built-in registry alone — nothing naming FLEX, paging, a rate or
/// a channel — the run decoded FLEX frames and linked them to the channel's **blind** emitter.
/// Red on `main`: there is no FLEX decoder; the only pager path is the POCSAG recipe a user must
/// start, so the emitters there carry no decode at all.
#[test]
fn c_the_flex_chain_is_chosen_automatically() {
    let Some(run) = run() else { return };
    report(&run);
    let mut failures = Vec::new();
    for ch in run.channels() {
        let f = ch.center_hz();
        let frames = run.flex_frames(f);
        if frames.is_empty() {
            let others: Vec<String> = run
                .rows_near(f)
                .into_iter()
                .flat_map(|r| run.decodes_of(r))
                .map(|d| format!("{}:{}", d.decoder_id, d.frame_model))
                .collect();
            failures.push(format!(
                "{:.4} MHz: no FLEX decode is linked to the channel's emitter (decodes there: \
                 {others:?}). Nothing chose a FLEX decoder for a channel whose symbols carry \
                 FLEX sync.",
                f / 1e6
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "[{T950}] (3) AUTO-SELECTION:\n{}",
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------------------------
// (D) Frames: sync and BCH.
// ---------------------------------------------------------------------------------------------

/// **(4) Every frame syncs, and its words are BCH(31,21)-valid.**
///
/// Passing proves, per channel: one decoded frame for **each** truth sync, within 16 header
/// symbols of it; each frame's FIW passed BCH(31,21) (`crc_status` valid, or corrected within the
/// code's two-error bound) and its checksum; at least one block of data codewords per frame
/// checked cleanly (≤ 1 correction, parity holding — the evidence rule random words pass 1.6 % of
/// the time). And across **all** channels the FIW frame numbers advance with the sample clock —
/// one frame per 1.875 s — which is FLEX's network-wide frame clock read out of BCH-checked words
/// and compared with the ADC's timestamps: two independent chains agreeing.
///
/// Red on `main`: no frames.
#[test]
fn d_every_sync_is_a_frame_and_its_words_are_bch_valid() {
    let Some(run) = run() else { return };
    report(&run);
    let mut failures = Vec::new();
    let mut clock: Vec<(f64, i64)> = Vec::new();
    for ch in run.channels() {
        let f = ch.center_hz();
        let frames = run.flex_frames(f);
        if frames.len() != ch.syncs.len() {
            failures.push(format!(
                "{:.4} MHz: {} frames decoded, the truth holds {} syncs",
                f / 1e6,
                frames.len(),
                ch.syncs.len()
            ));
        }
        for &t in &ch.syncs {
            let Some(d) = frames
                .iter()
                .find(|d| (run.secs(d.t) - t).abs() <= apriori::SYNC_TIME_TOL_S)
            else {
                failures.push(format!(
                    "{:.4} MHz: no decoded frame within {:.0} ms of the sync at {t:.3} s \
                     (frames at {:?})",
                    f / 1e6,
                    apriori::SYNC_TIME_TOL_S * 1e3,
                    frames.iter().map(|d| run.secs(d.t)).collect::<Vec<_>>(),
                ));
                continue;
            };
            if !matches!(d.crc_status, CrcStatus::Valid | CrcStatus::Corrected) {
                failures.push(format!(
                    "{:.4} MHz: frame at {t:.3} s has crc_status {:?}",
                    f / 1e6,
                    d.crc_status
                ));
            }
            let clean = d.metadata["clean_words"].as_u64().unwrap_or(0);
            if clean < 8 {
                failures.push(format!(
                    "{:.4} MHz: frame at {t:.3} s has {clean} clean data codewords (< one block)",
                    f / 1e6
                ));
            }
            let (cycle, frame) = (
                d.metadata["cycle"].as_i64().unwrap_or(-1),
                d.metadata["frame"].as_i64().unwrap_or(-1),
            );
            clock.push((run.secs(d.t), cycle * 128 + frame));
        }
    }
    // The network frame clock: frame index against time, one step per 1.875 s, on every channel.
    clock.sort_by(|a, b| a.0.total_cmp(&b.0));
    if let Some(&(t0, n0)) = clock.first() {
        for &(t, n) in &clock[1..] {
            let want = ((t - t0) / apriori::FRAME_S).round() as i64;
            if n - n0 != want {
                failures.push(format!(
                    "frame clock: FIW frame {n} at {t:.3} s is {} frames after the first, the \
                     sample clock says {want}",
                    n - n0
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "[{T950}] (4) FRAMES:\n{}",
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------------------------
// (E) Explanation, never identification.
// ---------------------------------------------------------------------------------------------

/// **(5) A paging explanation ranks in the top-k — as an explanation.**
///
/// Passing proves the known-signal database comes in **after** detection and decoding, as a
/// ranked suggestion: `paging` is among the emitter's top-{TOP_K} explanations, it rests on the
/// decode (`flex` decoder evidence, `evidence_confidence > 0`, not `allocation-only`), and the
/// band plan put no identity on the emitter. It checks an explanation is *offered*; it never looks
/// 929–932 MHz up to decide anything. Red on `main`: the bundled allocation table had no paging
/// row and nothing decoded the channel, so the rows there carry no paging explanation.
#[test]
fn e_a_paging_explanation_ranks_in_the_top_k() {
    let Some(run) = run() else { return };
    report(&run);
    let mut failures = Vec::new();
    for ch in run.channels() {
        let f = ch.center_hz();
        let rows = run.rows_near(f);
        let explained = rows.iter().find_map(|r| {
            r["explanations"]
                .as_array()?
                .iter()
                .take(TOP_K)
                .find(|e| e["service"] == "paging")
                .map(|e| (*r, e))
        });
        let Some((row, e)) = explained else {
            failures.push(format!(
                "{:.4} MHz: no `paging` explanation in any emitter's top-{TOP_K}: {:?}",
                f / 1e6,
                rows.iter()
                    .map(|r| r["explanations"]
                        .as_array()
                        .map(|a| a.iter().map(|e| e["service"].clone()).collect::<Vec<_>>()))
                    .collect::<Vec<_>>(),
            ));
            continue;
        };
        let rests_on_decode = e["evidence_confidence"].as_f64().is_some_and(|c| c > 0.0)
            && e["evidence"].as_array().is_some_and(|ev| {
                ev.iter()
                    .any(|x| x["kind"] == "family" && x["family"] == "flex")
            });
        if !rests_on_decode {
            failures.push(format!(
                "{:.4} MHz: the paging explanation does not rest on the decode (it would be the \
                 same suggestion an empty channel gets): {e}",
                f / 1e6
            ));
        }
        if !row["identity_scheme"].is_null() {
            failures.push(format!(
                "{:.4} MHz: the emitter carries an identity ({}) — an explanation must never \
                 become one",
                f / 1e6,
                row["identity_scheme"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "[{T950}] (5) EXPLANATION:\n{}",
        failures.join("\n")
    );
}
