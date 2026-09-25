//! T-852: **through the mock SDR, the M3 classifier names FSK for the FSK scene it was built for.**
//!
//! `fsk_burst_train` is 2-FSK at ±9.6 kHz and 4800 Bd — modulation index h = 4, 23.5 ms bursts —
//! replayed through the device interface (the mock SDR behind the real source contract, never
//! files fed to the pipeline). The fsk chain classifies each emission at the pipeline's single C15
//! call site (`crate::classify::classify_and_record`). Until T-852 every one of those rows came
//! back `unknown` at open-set 1.000: the classifier's `2fsk` dev grid stopped at h = 1.6, so a
//! wide-deviation burst sat far outside the fitted envelope (`obw_over_rs` z = 7.7). T-852 widened
//! the grid to h ≤ 5 and refitted the shipped densities with `bin/fit-densities`.
//!
//! **Blind.** The run gets the blinded recording (`blind_replay_config`); the scene's modulation is
//! known only to this assertion, and the classifier's own threshold table decides what counts as a
//! claim — nothing here is tuned to pass.
//!
//! # What was measured (8 seeds × 25/30 dB, plus 20 dB)
//!
//! - **20 dB scene: every row `BelowGate`/`low_snr`, correctly.** Scene SNR is per Carson
//!   bandwidth; the pipeline measures 18.3–19.1 dB over OBW99, under the FSK family's 20 dB gate.
//!   The gate is a core threshold and is not moved; those rows are only checked for naming no
//!   wrong family.
//! - **Above the gate: 0 of 16 rows claimed `fsk` before T-852, 5 of 16 after.** Not yet the
//!   majority T-852 asks for. The 11 misses split into two causes outside the density refit:
//!   - *4 rows: C13's burst extent overran the burst* (25.6–29.6 ms measured against 23.3 ms),
//!     so the normalised snippet carried noise-only samples and the envelope dimensions read far
//!     outside any constant-envelope class (`low_fraction` z 9–30, `duty` z −3…−10). The extent
//!     was `first..last` sample whose moving average cleared the 6 dB edge threshold anywhere in
//!     the snippet (`hk_estimate::params`, step 6b), and an isolated noise excursion milliseconds
//!     after the burst cleared it. **T-876** counts only runs that hold a sample significant over
//!     the whole snippet (`EstimatorConfig::extent_pfa`): every edge now lands within 0.16 ms of
//!     the truth, and 8 of 16 above-gate rows claim `fsk`.
//!   - *The rest: two device-path features the `2fsk` density had never seen — not the
//!     one-Gaussian tail these docs used to blame.* T-877 split and re-fitted the density and
//!     found the rows fell *further* out, on `cyclic_db` (z −3.3…−3.8 against its components) and
//!     `sigma_aa` (z +2.2…+4.0). T-887 traced each to its source, on 77 bursts of this scene
//!     (seeds 852–855 × 25/30 dB) measured through this call site's own steps:
//!     - **`sigma_aa` was a C13 bias.** Each extent edge sat about one edge window *outside* the
//!       burst (−44 / +56 source samples at 500 kS/s): `first − win/2` subtracted a half window
//!       from a centred moving average's 6 dB crossing, which at any useful SNR is already a half
//!       window early. The normalised snippet carried ~4 noise-only samples at each end of ~1 200,
//!       an envelope sample at the noise floor reads `|a/ā − 1| ≈ 1`, and those eight samples
//!       alone took `sigma_aa` from 0.025 to 0.083. C13 now puts each edge where the moving
//!       average crosses half-way to the burst's on-level (`hk_estimate::params`,
//!       `half_level_edges`): edges within +7 / +4 source samples, `sigma_aa` z **+2.2…+4.3 →
//!       −0.9…+0.5**, and 13 of 16 above-gate rows claimed on the densities of the time.
//!     - **`cyclic_db` is the dev grid's burst length, and is still open.** A cyclic line's
//!       significance grows with the symbols it integrates, ~9.5 dB a decade on an h = 4 packet
//!       (20.0 dB at 112 symbols, 26.5 at 448, 29.3 at 896; flat in C14's samples per OBW), and
//!       the device's 20.2 dB is exactly a 112-symbol burst's: the device measurement is right.
//!       Every dev-grid `2fsk` record holds 270–1 170 symbols, so the density has never seen a
//!       burst this short, and these rows still read `cyclic_db` z −2.8…−1.7. Cutting each
//!       `2fsk` draw's C14 view to a packet of drawn length and refitting took that to
//!       −0.8…+0.1, but it moved one acceptance draw's `2fsk` prior over the verifier's
//!       candidate floor, and the verifier — which prefers `gfsk` on that `2fsk` draw on either
//!       view — confirmed the tree's wrong call past p = 0.9 (`verifier_gain`'s never-more-
//!       confidently-wrong guard, 0 → 1). That generator change is held back until the verifier
//!       half is resolved; the rows are claimed without it.
//!
//! With T-888's refit (`blind_bpsk`/`blind_qpsk` absent where C14 cannot measure them) and the
//! half-level edges, **16 of 16** above-gate rows claim `fsk`. The floor asserted below is 15 —
//! one row of margin; red at 0 before T-852, 5 before T-876, 8 before T-887's extent fix and
//! 12 with T-888 alone.

mod common;

use common::*;
use hk_classify::thresholds::thresholds_of;
use hk_core::Discontinuity;
use hk_core::{Pacing, Source};
use hk_dsp::stft::InputInfo;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_estimate::SnippetRequest;
use hk_model::classify::Stage;
use hk_model::{FreqRange, InventoryQuery, Region, SampleTime, TimeRange, Timestamp};
use hk_pipeline::classify::measure_box;
use hk_pipeline::{NodeSpec, builtin_chains, open_replay};
use num_complex::Complex32;
use serde_json::json;

/// One feature-tree row as a reader of the inventory sees it.
struct Row {
    family: String,
    open_set: f64,
    flags: String,
    reasons: String,
}

/// C13's burst extent over one stored detection, against the hidden truth burst it overlaps.
struct ExtentCheck {
    /// Measured start − true start, s.
    start_err_s: f64,
    /// Measured end − true end, s.
    end_err_s: f64,
    /// Measured duration, s, and the true one.
    measured_s: f64,
    truth_s: f64,
    /// The extent's edge-detector window (`2·fs/OBW`), s: twice the duration's stated sigma.
    window_s: f64,
}

/// One blind run of `fsk_burst_train`: the C15 rows it persisted, and C13's extent over every
/// burst it detected.
struct SceneRun {
    rows: Vec<Row>,
    extents: Vec<ExtentCheck>,
    /// Detections C13 measured no untruncated extent for, with why.
    no_extent: Vec<String>,
}

/// The fsk chain's own pad around a detection box (the built-in `fsk-bursts` node), so the test
/// hands C13 the snippet the chain would.
fn fsk_chain_pad_s() -> f64 {
    builtin_chains()
        .iter()
        .flat_map(|c| c.nodes.iter())
        .find_map(|n| match n {
            NodeSpec::FskBursts { pad_s, .. } => Some(*pad_s),
            _ => None,
        })
        .expect("the built-in registry has an fsk-bursts node")
}

fn run_scene(seed: u64, snr_db: f64) -> SceneRun {
    let out = SynthRequest::new("fsk_burst_train")
        .seed(seed)
        .param("snr_db", snr_db)
        .param("duration_s", 1.2)
        .generate()
        .expect("scene synthesises");
    let fixture = out.fixture(0).unwrap();
    let meta = fixture.meta_path.clone();
    let dir = TempDir::new("t852-device-fsk");
    let (cfg, replay, input) = blind_replay_config(&dir.0, &meta, json!({}), Pacing::Unpaced);
    let s = start(cfg, replay).wait().unwrap();
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    let repo = repo(&dir.0);
    let mut rows = Vec::new();
    for e in inventory(&repo, InventoryQuery::default()) {
        for r in repo.classification_history(e.emitter.id).unwrap() {
            if r.stage != Stage::FeatureTree {
                continue;
            }
            let Some(d) = r.detail else { continue };
            rows.push(Row {
                family: d.family.clone(),
                open_set: d.open_set_score,
                flags: format!("{:?}", d.flags),
                reasons: d.reasons.join(","),
            });
        }
    }

    // T-876: C13's extent over every burst the run detected. The samples come back through the
    // same device interface the run read (a second open of the blinded recording); the boxes are
    // the run's own stored detections; the truth stays with the test.
    let mut source = open_replay(&blind_meta(&meta, &input.0), Pacing::Unpaced, false)
        .unwrap()
        .source;
    let mut iq: Vec<Complex32> = Vec::new();
    let mut block = Vec::new();
    let mut first = None;
    while let Some(h) = source.read_block(&mut block).unwrap() {
        let first = first.get_or_insert_with(|| h.clone());
        assert_eq!(
            h.first_sample(),
            first.first_sample() + iq.len() as u64,
            "the replay is contiguous"
        );
        iq.extend_from_slice(&block);
    }
    let head = first.expect("the replay has samples");
    let tune = head.provenance.tune.clone();
    let fs = tune.sample_rate_hz;
    let s0 = head.first_sample();
    let pad = (fsk_chain_pad_s() * fs) as u64;
    // The hidden truth: each burst's samples and band.
    let bursts: Vec<(u64, u64, f64, f64)> = fixture
        .emissions()
        .iter()
        .map(|t| {
            let a = s0 + t.sample_start;
            (a, a + t.sample_count, t.f_lo_hz, t.f_hi_hz)
        })
        .collect();
    let all = Region::new(
        FreqRange::new(0.0, 1e12),
        TimeRange::new(
            Timestamp::from_unix_nanos(i64::MIN / 2),
            Timestamp::from_unix_nanos(i64::MAX / 2),
        ),
    );
    // Each burst's box is the union of the run's detections on it, merged exactly as the fsk
    // chain merges its member boxes (`chains::fsk::add_member`: overlapping boxes widen one group
    // in time and frequency). Which detections belong to which burst is decided by overlap with
    // the truth after the fact — the blind ground-truth match; the run never saw it.
    let mut groups: Vec<Option<SnippetRequest>> = vec![None; bursts.len()];
    for det in repo.detections_in_region(&all).unwrap() {
        let r = SnippetRequest::from_detection(&det, head.time, &tune);
        let (f_lo, f_hi) = (
            det.f_center_hz - det.obw_hz / 2.0,
            det.f_center_hz + det.obw_hz / 2.0,
        );
        let Some(k) = bursts.iter().position(|&(a, b, lo, hi)| {
            r.start_index < b && a < r.end_index && f_lo < hi && lo < f_hi
        }) else {
            continue; // not on a burst: nothing to hold an extent against
        };
        let g = groups[k].get_or_insert(r);
        let (lo, hi) = (
            (tune.center_hz + g.center_offset_hz - g.bandwidth_hz / 2.0).min(f_lo),
            (tune.center_hz + g.center_offset_hz + g.bandwidth_hz / 2.0).max(f_hi),
        );
        *g = SnippetRequest {
            start_index: g.start_index.min(r.start_index),
            end_index: g.end_index.max(r.end_index),
            center_offset_hz: 0.5 * (lo + hi) - tune.center_hz,
            bandwidth_hz: (hi - lo).max(1.0),
        };
    }
    let (mut extents, mut no_extent) = (Vec::new(), Vec::new());
    for (&(t0, t1, _, _), request) in bursts.iter().zip(&groups) {
        let Some(request) = request else {
            continue; // undetected: a detection-recall question, not an extent one
        };
        let a = request.start_index.saturating_sub(pad).max(s0);
        let b = (request.end_index + pad).min(s0 + iq.len() as u64);
        let info = InputInfo {
            time: SampleTime {
                sample_index: a,
                host_time: head.time.time_of(a, fs),
            },
            discontinuity: Discontinuity::NONE,
            dropped_before: 0,
            provenance: &head.provenance,
        };
        let slice = &iq[(a - s0) as usize..(b - s0) as usize];
        let Some((_, params)) = measure_box(info, slice, request) else {
            no_extent.push(format!("{request:?}: no snippet"));
            continue;
        };
        match params.extent {
            Some(e) if !e.truncated_start && !e.truncated_end => extents.push(ExtentCheck {
                start_err_s: (e.source_start - t0 as f64) / fs,
                end_err_s: (e.source_end - t1 as f64) / fs,
                measured_s: (e.source_end - e.source_start) / fs,
                truth_s: (t1 - t0) as f64 / fs,
                window_s: 2.0
                    * params
                        .duration_s
                        .sigma()
                        .expect("a measured extent has a σ"),
            }),
            other => no_extent.push(format!("{request:?}: {other:?}")),
        }
    }
    SceneRun {
        rows,
        extents,
        no_extent,
    }
}

#[test]
fn wide_deviation_fsk_through_the_mock_sdr_is_claimed_fsk_and_never_a_wrong_family() {
    // The synthesiser is optional tooling; skip (loudly) where it is absent, like every scene test.
    let _probe = synth_or_skip!(SynthRequest::new("fsk_burst_train").seed(852));
    let open_set_max = thresholds_of("fsk").unwrap().open_set_max;
    let (mut above, mut claimed_above) = (0usize, 0usize);
    let mut log = Vec::new();
    let mut extents = Vec::new();
    let mut no_extent = Vec::new();
    for snr_db in [20.0, 25.0, 30.0] {
        for seed in 852u64..860 {
            let run = run_scene(seed, snr_db);
            for x in &run.extents {
                log.push(format!(
                    "{snr_db} dB seed {seed}: extent {:.3} ms (truth {:.3}), start {:+.3} ms, end {:+.3} ms, window {:.3} ms",
                    x.measured_s * 1e3,
                    x.truth_s * 1e3,
                    x.start_err_s * 1e3,
                    x.end_err_s * 1e3,
                    x.window_s * 1e3
                ));
            }
            extents.extend(run.extents);
            no_extent.extend(
                run.no_extent
                    .into_iter()
                    .map(|m| format!("{snr_db} dB seed {seed}: {m}")),
            );
            for r in run.rows {
                // Whatever the gate did, a row may say `fsk` or `unknown` about an FSK emission —
                // never another family.
                assert!(
                    r.family == "fsk" || r.family == "unknown",
                    "{snr_db} dB seed {seed}: named {} ({})",
                    r.family,
                    r.reasons
                );
                // A row the SNR gate held back cannot claim a family at all (ADR-0016 §2).
                if !r.flags.contains("BelowGate") {
                    above += 1;
                    claimed_above += usize::from(r.family == "fsk" && r.open_set < open_set_max);
                }
                log.push(format!(
                    "{snr_db} dB seed {seed}: {} open-set {:.4} flags {} reasons {}",
                    r.family, r.open_set, r.flags, r.reasons
                ));
            }
        }
    }
    for l in &log {
        eprintln!("[T-852] {l}");
    }
    eprintln!("[T-852] above-gate rows claimed fsk: {claimed_above} of {above}");
    for m in &no_extent {
        eprintln!("[T-876] no extent: {m}");
    }
    assert!(
        above >= 12,
        "too few above-gate rows to judge ({above}): {log:#?}"
    );
    // T-876: C13's extent ends where the burst ends. Every detected burst gets an untruncated
    // extent, and each edge sits within three edge-detector windows (`2·fs/OBW`, the slack
    // `hk-estimate`'s synthetic sweep states) of the hidden truth. Measured: every edge within
    // 0.16 ms, at most 1.22 windows (a window is 0.06–0.11 ms here), over 240 bursts. Before
    // T-876 four bursts ran 2.2–6.2 ms past their end (20 dB seed 858, 25 dB seed 853, 30 dB
    // seeds 853 and 856): the first→last crossing of the 6 dB edge threshold took in an isolated
    // noise excursion milliseconds after the burst, and the appended noise-only samples pushed
    // the envelope features out of every FSK class.
    //
    // T-887: and each edge sits **on** the burst, within three quarters of one window. Until then
    // every edge sat about a window *outside* it (start −1.00…+0.50, end +0.25…+1.22 windows over
    // these 240 bursts): the edge was a centred moving average's 6 dB crossing, already half a
    // window early at any useful SNR, less another half window. The noise-only samples that put
    // in the normalised snippet doubled `sigma_aa` (module docs). Measured with the half-level
    // edges: start −0.27…+0.50, end −0.12…+0.27 windows, every edge within 0.044 ms.
    const EDGE_WINDOWS: f64 = 0.75;
    assert!(
        no_extent.is_empty(),
        "detected bursts without an untruncated extent: {no_extent:#?}"
    );
    assert!(extents.len() >= 200, "too few extents ({})", extents.len());
    let bad: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("extent"))
        .zip(&extents)
        .filter(|(_, x)| {
            x.start_err_s.abs() > EDGE_WINDOWS * x.window_s
                || x.end_err_s.abs() > EDGE_WINDOWS * x.window_s
        })
        .map(|(l, _)| l)
        .collect();
    assert!(
        bad.is_empty(),
        "extent edges more than {EDGE_WINDOWS} of a window from the truth: {bad:#?}"
    );
    // Measured 5 of 16 with the T-852 densities and 0 of 16 with the ones before them; 8 of 16
    // once the extent stopped overrunning (T-876); 13 of 16 once its edges stopped running a
    // window into the noise (T-887, `sigma_aa`), and 16 of 16 with T-888's refit on top. Red at 12
    // on T-888's densities with the pre-T-887 estimator.
    assert!(
        claimed_above >= 15,
        "fsk claimed on {claimed_above} of {above} above-gate rows: {log:#?}"
    );
}
