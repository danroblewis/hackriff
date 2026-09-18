//! T-483 — **the canvas's finest live tier must reproduce the FFT spectrum frames for the same
//! window, and today it does not.** This file is a PROOF, and it is expected to fail.
//!
//! The user reports that the canvas shows *less detail* than the waterfall T-445 retired: the
//! narrowband emission near 100.465 MHz "reads nearly straight" and the 101.3 MHz station is
//! "washed out". CLAUDE.md already forbids that — *"whenever data exists for that window it must be
//! shown"*, with *"we have it but didn't render it"* named as a bug. So this is not a new
//! requirement; it is an existing one, violated. (Note which way round that is: the honesty rules
//! normally stop the UI **implying** detail the hardware did not capture. Here it is **withholding**
//! detail the hardware did capture.)
//!
//! # What is compared, and what each number is a property of
//!
//! One run, one recording, **one device interface** (`open_mock_replay`, the mock SDR replaying
//! SigMF behind the same contract as the HackRF source — tests never feed files to the pipeline).
//! Two readers of that one capture:
//!
//! - **Path A, the FFT frames** — the `spectrum/live` stream (`hk-pipeline`'s display STFT), which
//!   is what the retired waterfall drew, at the display settings in force.
//! - **Path B, the canvas** — the de-welded view lattice's finest node (`level_f 0, level_t 0`,
//!   `scheme=view`), which is what `/api/tiles` serves and `ui/src/surface` renders. This test reads
//!   the pyramid the route reads, so no colormap and no display range enter the comparison.
//!
//! **The comparison is on VALUES, not pixels**, deliberately: a pixel comparison folds in the ramp
//! and the display range (T-470 is changing exactly those) and would drift for reasons that have
//! nothing to do with fidelity. Every number below is a property of dB values the backend serves.
//!
//! **No second renderer is introduced.** `ui/src/waterfall.ts` is gone and stays gone (T-445: every
//! one of T-420, T-388, T-397 and T-412 was two implementations of one idea drifting apart). Path A
//! here is the *stream*, not a resurrected client.
//!
//! # What the measurement found — and where it differs from the ticket's premise
//!
//! T-483's brief names one mechanism: a level-0 cell averages `f_cell / bin_width` FFT bins
//! (`hk-store/src/history/frame.rs` module docs, `RegridPlan::mean`). That is real, but it is the
//! **`value`/mean** plane — and `/api/tiles` serves **`max_db`** (`tiles.rs::grid_json`), which
//! `ui/src/surface/tile.ts` is the sole consumer of. So the mean-over-bins fold is not the plane the
//! canvas draws, and a proof resting on it would be measuring an adjacent question. The three
//! mechanisms that *do* act on `max_db`, each measured below rather than assumed:
//!
//! 1. **The frames folded into the pyramid are not the display's.** The history/view chain runs its
//!    own STFT at `detection_resolution()`'s `fft_len` (`hk-pipeline/src/history.rs::run`), which at
//!    2.4 Msps is **512 bins = 4687.5 Hz**, against the display stream's 1024 bins = 2343.75 Hz and
//!    the up-to-65536 bins a zoomed client may ask for through `POST /api/control/display`. A
//!    tone's PSD *density* scales inversely with bin width, so a narrow line simply reads lower
//!    here — by `10·log10(bin ratio)` — while the noise floor, a true density, does not move. That
//!    is a resolution loss no choice of `f_cell` can undo.
//! 2. **The cell statistic is a max-hold over ~10³ FFT cells.** Welch holds are on for this chain
//!    and a row averages `k = fs/(hop·history_rows_per_s)` segments, so the stored per-bin peak is
//!    already a max over ~938 segments at 2.4 Msps; the cell then maxes over its bins and rows. The
//!    max of `N` exponential noise samples sits ~`10·log10(ln N + γ)` above their mean, so the
//!    canvas's **floor rises** while a deterministic peak does not — contrast collapses. That is
//!    "washed out", in dB.
//! 3. **One canvas row is one second.** `VIEW_T_CELL` is 1 s against the display stream's 25 rows/s,
//!    so a second of level variation becomes a single number — and a max, which keeps the crests
//!    and discards the troughs. That is "reads nearly straight", and it is measured here as the
//!    fraction of the display path's time variation the canvas retains.
//!
//! **Consequence for T-484 (the fix, which is NOT this ticket):** widening or narrowing `f_cell`
//! alone cannot close this. The frames reaching the pyramid are 512-bin, 10 rows/s, max-held. The
//! live tier reaching FFT resolution means changing what is folded, not only the grid it folds onto.
//!
//! # The fixture's checkable structure
//!
//! `fm_100p8M_2p4M_l32g30a1_t1p5_5s` (HackRF One, 100.8 MHz, 2.4 Msps, 5.0 s). Its truth is
//! stripped before the run (`blind::strip_truth`); only this file holds it:
//!
//! | What | Where | Why it is the right probe |
//! |---|---|---|
//! | reference-harmonic spur | 100.000 MHz | essentially CW — a single-bin line, so it measures resolution and floor lift directly |
//! | narrowband emission | ~100.465 MHz | the one the user calls "wavey": ~28 kHz, varies second to second |
//! | WFM broadcast | 101.3 MHz | wideband — its *density* is resolution-invariant, so its contrast loss isolates the floor lift |
//! | quiet band | 100.50–100.70 MHz | no emission: the noise reference both paths are scored against |
//!
//! # How to run it, and why four tests are `#[ignore]`d
//!
//! ```text
//! cargo test -p hk-e2e --test canvas_fidelity -- --ignored --test-threads 1 --nocapture
//! ```
//!
//! The four proofs are `#[ignore]`d **only** so one deliberately-red measurement does not block
//! every unrelated merge while the fix is built; they are expected to fail and the numbers above
//! are what they print. **Deleting those four `#[ignore]` lines is T-484's definition of done** —
//! after which this file is the standing regression guard that the canvas's native zoom still
//! shows what the FFT frames hold.
//!
//! The fifth test, [`the_two_paths_agree_on_the_noise_floor_so_the_comparison_is_sound`], is **not**
//! ignored: it passes today and must keep passing, because every dB the other four report is a
//! subtraction between the two paths and is meaningless if they ever stop agreeing on scale.
//!
//! Measured 2026-09-18 on `fm_100p8M_2p4M_l32g30a1_t1p5_5s`: the display path serves 1024 bins over
//! 2.400 MHz at 24.9 rows/s (2343.8 Hz × 40.1 ms); the canvas's finest node is 6250 Hz × 1 s, so
//! **one canvas cell stands in for 2.67 × 24.93 = 66.5 FFT cells** (and 171× the finest bin a
//! zoomed client may request). Control: both paths put the quiet band's floor at −84.53 dB/Hz —
//! identical. Canvas `max_db` puts it at −74.02, **+10.5 dB of max-hold floor lift**, which costs
//! 4.5 dB of contrast on the 100.000 MHz spur, 5.0 dB on the 100.465 MHz emission and 1.6 dB on the
//! station; and the one-second cell keeps **19 %** of the 100.465 MHz emission's level variation and
//! **3 %** of the station's.

#[path = "acceptance/common.rs"]
mod common;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use common::{Buf, TempDir, real_fixture};
use hk_core::{MockEnd, Pacing};
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_pipeline::{
    DISPLAY_FFT_MAX, Pipeline, PipelineConfig, TrackInventory, open_mock_replay, replay_plan,
};
use hk_store::{Pyramid, RegionQuery, Resolution};
use hk_stream::{
    BINARY_RECORD_HEADER_LEN, BinaryRecordHeader, Declared, FrameDecoder, MAX_FRAME_LEN,
    StreamHeader, StreamKind,
};

const FIXTURE: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";

/// The reference-harmonic spur: a ~CW line, the sharpest structure in the recording.
const SPUR_HZ: f64 = 100.000e6;
/// The narrowband emission the user describes as wavey (a free-running oscillator harmonic).
const OSC_HZ: f64 = 100.465e6;
/// The WFM broadcast station.
const STATION_HZ: f64 = 101.3e6;
/// Half-width of the window a narrow probe is searched in, Hz.
const PROBE_HALF_HZ: f64 = 30e3;
/// Half-width of the station's own channel, Hz.
const STATION_HALF_HZ: f64 = 90e3;
/// A band with no emission in it: the noise reference.
const QUIET: (f64, f64) = (100.50e6, 100.70e6);
/// Seconds of the recording skipped at each end, so neither path is scored on a partial row.
const EDGE_S: f64 = 1.0;
/// How long the run may take.
const LIMIT: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------------------------
// The run: one capture, read twice.
// ---------------------------------------------------------------------------------------------

/// One display-FFT row: its capture time and its bins, dBFS/Hz.
struct Row {
    t_ns: i64,
    psd_db: Vec<f32>,
}

/// Everything both paths said about the one capture.
struct Fidelity {
    /// Path A: the `spectrum/live` header (centre, span, `fft_size`).
    header: StreamHeader,
    /// Path A: display-FFT bin width, Hz — **measured from the header**, not assumed.
    bin_hz: f64,
    /// Path A: display row period, s — the median gap between record timestamps.
    row_s: f64,
    /// Path A rows, in time order.
    rows: Vec<Row>,
    /// Path B: the finest view-lattice node's frequency cell, Hz.
    f_cell_hz: f64,
    /// Path B: the finest view-lattice node's time cell, s.
    t_cell_s: f64,
    /// Path B: low edge of canvas frequency cell 0, Hz.
    canvas_f_lo: f64,
    /// Path B: start of canvas time cell 0, Unix ns.
    canvas_t0_ns: i64,
    /// Path B: canvas cells, row-major (time then frequency), `max_db` — the plane `/api/tiles`
    /// serves and `ui/src/surface/tile.ts` renders. NaN where nothing was observed.
    canvas_max_db: Vec<f32>,
    /// Path B: the same cells' `mean_db` — the plane the brief's `RegridPlan::mean` produces, kept
    /// so this file can say what each mechanism costs rather than conflating them.
    canvas_mean_db: Vec<f32>,
    /// Path B grid shape.
    canvas_nt: usize,
    canvas_nf: usize,
    /// The window both paths were read over.
    window: (i64, i64),
}

impl Fidelity {
    /// Display-FFT bin index nearest `hz`, if it is inside the window.
    fn bin_of(&self, hz: f64) -> Option<usize> {
        let n = self.header.fft_size? as usize;
        let lo = self.header.center_hz? - self.header.bandwidth_hz? / 2.0;
        let i = ((hz - lo) / self.bin_hz).floor();
        (i >= 0.0 && (i as usize) < n).then_some(i as usize)
    }

    /// Centre frequency of display bin `i`.
    fn bin_hz_of(&self, i: usize) -> f64 {
        let lo = self.header.center_hz.unwrap() - self.header.bandwidth_hz.unwrap() / 2.0;
        lo + (i as f64 + 0.5) * self.bin_hz
    }

    /// Canvas frequency-cell index containing `hz`, if it is in the grid.
    fn cell_of(&self, hz: f64) -> Option<usize> {
        let i = ((hz - self.canvas_f_lo) / self.f_cell_hz).floor();
        (i >= 0.0 && (i as usize) < self.canvas_nf).then_some(i as usize)
    }

    /// `max_db` over every observed canvas cell in `[lo, hi)` across the whole window.
    fn canvas_band_max(&self, lo: f64, hi: f64) -> Vec<f32> {
        self.canvas_band(lo, hi, &self.canvas_max_db)
    }

    fn canvas_band(&self, lo: f64, hi: f64, plane: &[f32]) -> Vec<f32> {
        let (a, b) = (self.cell_of(lo), self.cell_of(hi - 1.0));
        let (Some(a), Some(b)) = (a, b) else {
            return Vec::new();
        };
        (0..self.canvas_nt)
            .flat_map(|t| (a..=b).map(move |f| t * self.canvas_nf + f))
            .map(|i| plane[i])
            .filter(|v| v.is_finite())
            .collect()
    }

    /// Every display-FFT value in `[lo, hi)` over the whole window.
    fn display_band(&self, lo: f64, hi: f64) -> Vec<f32> {
        let (a, b) = match (self.bin_of(lo), self.bin_of(hi - 1.0)) {
            (Some(a), Some(b)) => (a, b),
            _ => return Vec::new(),
        };
        self.rows
            .iter()
            .flat_map(|r| r.psd_db[a..=b].iter().copied())
            .filter(|v| v.is_finite())
            .collect()
    }

    /// Per-row level of the channel `[lo, hi)` in the display path: the row's max over the channel,
    /// dB. The same statistic the canvas cell claims to carry, at the display path's own cadence.
    fn display_channel_track(&self, lo: f64, hi: f64) -> Vec<(i64, f32)> {
        let (Some(a), Some(b)) = (self.bin_of(lo), self.bin_of(hi - 1.0)) else {
            return Vec::new();
        };
        self.rows
            .iter()
            .map(|r| {
                let m = r.psd_db[a..=b]
                    .iter()
                    .copied()
                    .filter(|v| v.is_finite())
                    .fold(f32::NEG_INFINITY, f32::max);
                (r.t_ns, m)
            })
            .filter(|(_, m)| m.is_finite())
            .collect()
    }

    /// **The statistic both paths are scored with**, so no comparison below is between two
    /// different estimators: the level of the typical *row* — the median, over rows, of each row's
    /// own maximum across `[lo, hi)`. A waterfall draws one row at a time, so this is what a row
    /// shows; and using a median over rows on both sides keeps the max-of-N inflation that a bare
    /// `max` carries from landing on one side of the subtraction only.
    fn display_row_level(&self, lo: f64, hi: f64) -> f32 {
        median(
            self.display_channel_track(lo, hi)
                .into_iter()
                .map(|(_, v)| v)
                .collect(),
        )
    }

    /// [`Self::display_row_level`]'s twin on the canvas: the median canvas row's max over the band.
    fn canvas_row_level(&self, lo: f64, hi: f64) -> f32 {
        median(self.canvas_channel_track(lo, hi))
    }

    /// Per-canvas-row level of the channel `[lo, hi)`: the row's max over the channel's cells.
    fn canvas_channel_track(&self, lo: f64, hi: f64) -> Vec<f32> {
        let (Some(a), Some(b)) = (self.cell_of(lo), self.cell_of(hi - 1.0)) else {
            return Vec::new();
        };
        (0..self.canvas_nt)
            .map(|t| {
                (a..=b)
                    .map(|f| self.canvas_max_db[t * self.canvas_nf + f])
                    .filter(|v| v.is_finite())
                    .fold(f32::NEG_INFINITY, f32::max)
            })
            .filter(|m| m.is_finite())
            .collect()
    }

    /// How many display-FFT cells one canvas cell stands in for, on each axis and in total.
    fn fold(&self) -> (f64, f64, f64) {
        let bins = self.f_cell_hz / self.bin_hz;
        let rows = self.t_cell_s / self.row_s;
        (bins, rows, bins * rows)
    }
}

fn median(mut v: Vec<f32>) -> f32 {
    if v.is_empty() {
        return f32::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn max_of(v: &[f32]) -> f32 {
    v.iter().copied().fold(f32::NEG_INFINITY, f32::max)
}

fn std_dev(v: &[f32]) -> f64 {
    if v.len() < 2 {
        return f64::NAN;
    }
    let n = v.len() as f64;
    let mean = v.iter().map(|&x| f64::from(x)).sum::<f64>() / n;
    (v.iter()
        .map(|&x| (f64::from(x) - mean).powi(2))
        .sum::<f64>()
        / (n - 1.0))
        .sqrt()
}

/// Splits a consumer's framed bytes into the stream header and timestamped data rows.
fn decode(bytes: &[u8]) -> (Option<StreamHeader>, Vec<Row>) {
    let mut dec = FrameDecoder::new(MAX_FRAME_LEN);
    dec.push(bytes);
    let mut header = None;
    let mut rows = Vec::new();
    while let Ok(Some(frame)) = dec.next_frame() {
        if header.is_none() {
            header = Some(StreamHeader::from_json_bytes(frame).expect("spectrum header frame"));
            continue;
        }
        let Some(h) = BinaryRecordHeader::decode(frame) else {
            continue;
        };
        // type 1 = data; flags bit 0 = gated (payload withheld, not a row).
        if h.record_type != 1 || h.flags.0 & 1 != 0 {
            continue;
        }
        rows.push(Row {
            t_ns: h.t.as_unix_nanos(),
            psd_db: frame[BINARY_RECORD_HEADER_LEN..]
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        });
    }
    (header, rows)
}

/// The one run. Both paths read the same capture, through the device interface.
fn build() -> Option<Fidelity> {
    let meta = real_fixture(FIXTURE)?;
    let dir = TempDir::new("t483-canvas-fidelity");
    let blind_dir = TempDir::new("t483-blind");
    // Truth out of the system's reach: this file holds it, the pipeline never sees it.
    let blind = hk_e2e::blind::strip_truth(&meta, &blind_dir.0, "blind", 0.0)
        .expect("the fixture's truth was stripped");

    let dev = open_mock_replay(&blind, Pacing::Unpaced, MockEnd::Stop)
        .expect("the mock SDR device opened the recording");
    let info = dev.info;
    let mut cfg = PipelineConfig::new(
        &dir.0,
        replay_plan(info.center_hz, info.sample_rate_hz, info.start_time),
    )
    .expect("pipeline config");
    cfg.source_class = dev.class;
    cfg.lossless = true;
    assert!(
        cfg.settings.view_history,
        "the view lattice is on by default; this test reads its finest node"
    );

    // Path A's tap: the `spectrum/live` stream, exactly as an external consumer receives it.
    let sink = Buf::default();
    let tap = sink.clone();
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        if h.kind == StreamKind::Spectrum {
            handle
                .subscribe(
                    "t483-canvas",
                    Declared::local(tap.clone()),
                    Box::new(|_| {}),
                )
                .expect("subscribed to the spectrum stream");
        }
    }));

    let handle = Pipeline::start(
        cfg,
        Box::new(dev.source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .expect("the pipeline started over the mock device");
    let view: Arc<Mutex<Pyramid>> = handle
        .view_history()
        .expect("the pipeline opened the view-scheme pyramid the canvas reads");

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.wait());
    });
    let summary = rx
        .recv_timeout(LIMIT)
        .expect("the run finished inside the limit")
        .expect("the run finished cleanly");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    // Path A: drain the consumer's writer thread, which may still be flushing.
    let expected = summary.counter("/spectrum/rows") - summary.counter("/spectrum/rows_gated");
    let deadline = Instant::now() + Duration::from_secs(30);
    let (header, mut rows) = loop {
        let got = decode(&sink.0.lock().unwrap());
        if (got.1.len() as u64 >= expected && got.0.is_some()) || Instant::now() > deadline {
            break got;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let header = header.expect("the spectrum stream published a header");
    rows.sort_by_key(|r| r.t_ns);
    assert!(
        rows.len() > 20,
        "the display path produced {} rows; there is nothing to compare against",
        rows.len()
    );

    let fft = header.fft_size.expect("the header declares fft_size") as f64;
    let span = header.bandwidth_hz.expect("the header declares a span");
    let bin_hz = span / fft;
    let mut gaps: Vec<f32> = rows
        .windows(2)
        .map(|w| (w[1].t_ns - w[0].t_ns) as f32 * 1e-9)
        .collect();
    gaps.retain(|g| *g > 0.0);
    let row_s = f64::from(median(gaps));

    // The window: the middle of the recording, so neither path is scored on a partial edge row.
    let t_lo = rows.first().unwrap().t_ns + (EDGE_S * 1e9) as i64;
    let t_hi = rows.last().unwrap().t_ns - (EDGE_S * 1e9) as i64;
    assert!(
        t_hi > t_lo,
        "the recording is shorter than the edges skipped"
    );
    rows.retain(|r| r.t_ns >= t_lo && r.t_ns < t_hi);

    // Path B: the same window, from the pyramid `/api/tiles` reads, at its finest node.
    let f_lo = header.center_hz.expect("centre") - span / 2.0;
    let freq = FreqRange::new(f_lo, f_lo + span);
    let time = TimeRange::new(
        Timestamp::from_unix_nanos(t_lo),
        Timestamp::from_unix_nanos(t_hi),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let hist = loop {
        let h = {
            let mut p = view.lock().unwrap();
            p.materialize(0, freq, time).expect("finest node built");
            p.query(&RegionQuery {
                freq,
                time,
                resolution: Resolution::Level(0),
            })
            .expect("the view pyramid answered")
        };
        let observed = h.cells.iter().filter(|c| c.max_db.is_finite()).count();
        if observed > 0 || Instant::now() > deadline {
            assert!(
                observed > 0,
                "the view pyramid holds nothing for the window both paths cover"
            );
            break h;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    Some(Fidelity {
        header,
        bin_hz,
        row_s,
        rows,
        f_cell_hz: hist.f_cell_hz,
        t_cell_s: hist.t_cell_ns as f64 * 1e-9,
        canvas_f_lo: hist.f_first_cell as f64 * hist.f_cell_hz,
        canvas_t0_ns: hist.t_first_cell * hist.t_cell_ns,
        canvas_max_db: hist.cells.iter().map(|c| c.max_db).collect(),
        canvas_mean_db: hist.cells.iter().map(|c| c.mean_db).collect(),
        canvas_nt: hist.nt,
        canvas_nf: hist.nf,
        window: (t_lo, t_hi),
    })
}

fn run() -> Option<&'static Fidelity> {
    static R: OnceLock<Option<Fidelity>> = OnceLock::new();
    R.get_or_init(build).as_ref()
}

// ---------------------------------------------------------------------------------------------
// 1. Geometry: how many FFT cells one canvas cell stands in for.
// ---------------------------------------------------------------------------------------------

/// **The premise, on the grid.** A view of a window the live IQ covers must be able to show the
/// measurements that window produced. The canvas's finest step is therefore required to be no
/// coarser than the FFT frames the display path is *already serving* for the same capture — the
/// conservative reference, since a zoomed client may request up to `DISPLAY_FFT_MAX` bins and the
/// gap only widens.
///
/// This fails today, and the failure message is the measurement: bins per cell, rows per cell, and
/// the product — how many FFT cells the canvas replaces with one number.
#[test]
#[ignore = "T-483 PROVES THE GAP AND IS EXPECTED TO FAIL. It is `#[ignore]`d only so one \
            known-red proof does not block every other merge; deleting this line is T-484's \
            definition of done. Run it: cargo test -p hk-e2e --test canvas_fidelity -- --ignored \
            --nocapture"]
fn the_finest_live_tier_is_no_coarser_than_the_display_ffts_it_folds() {
    let Some(f) = run() else { return };
    let (bins, rows, fold) = f.fold();
    let finest_bin_hz = f.header.bandwidth_hz.unwrap() / DISPLAY_FFT_MAX as f64;

    eprintln!(
        "T-483 geometry, measured through one capture:\n\
         display FFT : {} bins over {:.3} MHz = {:.1} Hz/bin, {:.1} rows/s ({:.4} s/row)\n\
         canvas cell : {:.1} Hz x {:.3} s (view lattice, level_f 0 / level_t 0)\n\
         => one canvas cell stands in for {bins:.2} bins x {rows:.2} rows = {fold:.1} FFT cells\n\
         => frequency is {:.2}x coarser than the display path serves, and {:.0}x coarser than the \
            {} bins a zoomed client may request ({:.1} Hz/bin)\n\
         => time is {rows:.1}x coarser",
        f.header.fft_size.unwrap(),
        f.header.bandwidth_hz.unwrap() / 1e6,
        f.bin_hz,
        1.0 / f.row_s,
        f.row_s,
        f.f_cell_hz,
        f.t_cell_s,
        bins,
        f.f_cell_hz / finest_bin_hz,
        DISPLAY_FFT_MAX,
        finest_bin_hz,
    );

    assert!(
        f.f_cell_hz <= f.bin_hz,
        "the canvas's finest frequency step is {:.1} Hz against the display path's {:.1} Hz bin: \
         {bins:.2} FFT bins collapse into one cell. Data exists at the finer step and the canvas \
         does not show it (CLAUDE.md: \"whenever data exists for that window it must be shown\").",
        f.f_cell_hz,
        f.bin_hz,
    );
    assert!(
        f.t_cell_s <= f.row_s,
        "the canvas's finest time step is {:.3} s against the display path's {:.4} s row: \
         {rows:.1} FFT rows collapse into one cell.",
        f.t_cell_s,
        f.row_s,
    );
}

// ---------------------------------------------------------------------------------------------
// 2. Values: the numbers, not the pixels.
// ---------------------------------------------------------------------------------------------

/// **The premise, on the values.** "Reproduces the FFT bins" means *the same numbers*: for a window
/// the live IQ covers, every display-FFT value inside a canvas cell should be that cell's value.
/// The measurement is the spread the single number has to stand for — `max − min` of the display
/// values inside one canvas cell's footprint, medianed over the quiet band (where every FFT cell is
/// a sample of one distribution, so a faithful tier would show ~0 spread) and over the station.
///
/// This is a property of **dB values from two backend readers of one capture**. No colormap, no
/// display range, no pixels — T-470 can change both without moving this number.
#[test]
#[ignore = "T-483 PROVES THE GAP AND IS EXPECTED TO FAIL. It is `#[ignore]`d only so one \
            known-red proof does not block every other merge; deleting this line is T-484's \
            definition of done. Run it: cargo test -p hk-e2e --test canvas_fidelity -- --ignored \
            --nocapture"]
fn the_finest_live_tier_reproduces_the_display_ffts_values() {
    let Some(f) = run() else { return };
    let tol_db = 1.0f32;
    let (_, _, fold) = f.fold();

    // Two bands, because a reproduction error measured only in noise answers an adjacent question:
    // the quiet band is where a faithful tier's spread would genuinely be small, and the station's
    // own skirt is where the withheld detail is real structure.
    let bands = [
        ("quiet 100.50-100.70", QUIET),
        (
            "station 101.20-101.40",
            (STATION_HZ - STATION_HALF_HZ, STATION_HZ + STATION_HALF_HZ),
        ),
    ];
    eprintln!("T-483 values. One canvas cell stands in for {fold:.0} display measurements.");
    let mut worst = 0.0f32;
    let mut worst_band = "";
    let mut worst_spread = 0.0f32;
    for (name, (b_lo, b_hi)) in bands {
        let (a, b) = (f.cell_of(b_lo).unwrap(), f.cell_of(b_hi - 1.0).unwrap());
        let mut spreads = Vec::new();
        let mut errors = Vec::new();
        for t in 0..f.canvas_nt {
            let t0 = f.canvas_t0_ns + (t as f64 * f.t_cell_s * 1e9) as i64;
            let t1 = t0 + (f.t_cell_s * 1e9) as i64;
            for c in a..=b {
                let v = f.canvas_max_db[t * f.canvas_nf + c];
                if !v.is_finite() {
                    continue;
                }
                let lo = f.canvas_f_lo + c as f64 * f.f_cell_hz;
                let hi = lo + f.f_cell_hz;
                let inside: Vec<f32> = f
                    .rows
                    .iter()
                    .filter(|r| r.t_ns >= t0 && r.t_ns < t1)
                    .flat_map(|r| {
                        r.psd_db
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| {
                                let hz = f.bin_hz_of(*i);
                                hz >= lo && hz < hi
                            })
                            .map(|(_, &v)| v)
                    })
                    .filter(|v| v.is_finite())
                    .collect();
                if inside.len() < 2 {
                    continue;
                }
                let mx = max_of(&inside);
                let mn = -max_of(&inside.iter().map(|v| -v).collect::<Vec<_>>());
                spreads.push(mx - mn);
                errors.extend(inside.iter().map(|&x| (x - v).abs()));
            }
        }
        assert!(
            !spreads.is_empty(),
            "no canvas cell of the {name} band could be matched to display rows"
        );
        let (spread, err) = (median(spreads.clone()), median(errors));
        eprintln!(
            "  {name:<22} {:>4} cells | display values inside one cell span {spread:5.1} dB | \
             median |display - cell| {err:5.1} dB",
            spreads.len(),
        );
        if err > worst {
            worst = err;
            worst_band = name;
            worst_spread = spread;
        }
    }

    assert!(
        worst <= tol_db,
        "the canvas cell does not reproduce the FFT values it stands in for: in the {worst_band} \
         band the typical display measurement inside a cell differs from the cell's own number by \
         {worst:.1} dB, and the values inside one cell span {worst_spread:.1} dB. A faithful \
         finest tier would carry each of those {fold:.0} measurements, not one max over all of \
         them."
    );
}

/// **The control, and it must PASS.** Everything above subtracts a canvas number from a display
/// number, which is only meaningful if the two chains agree on scale in the first place. They do,
/// and this is the evidence: the pyramid's **mean** plane and the display FFT's values put the
/// quiet band's noise floor at the same dB — both are power *densities* per Hz, so a true density
/// is invariant to how finely it is binned.
///
/// So every dB the other three tests report is **the statistic and the grid, not a calibration
/// difference**. Without this, "the canvas reads 10 dB high" could have been a unit bug; with it,
/// the 10 dB is what the max-hold does.
#[test]
fn the_two_paths_agree_on_the_noise_floor_so_the_comparison_is_sound() {
    let Some(f) = run() else { return };
    let display = median(f.display_band(QUIET.0, QUIET.1));
    let mean_plane = median(f.canvas_band(QUIET.0, QUIET.1, &f.canvas_mean_db));
    let max_plane = median(f.canvas_band_max(QUIET.0, QUIET.1));
    eprintln!(
        "T-483 control, quiet band {:.3}-{:.3} MHz:\n\
         display FFT ({} bins)      {display:7.2} dB/Hz\n\
         canvas mean_db (the fold)  {mean_plane:7.2} dB/Hz  <- agrees: same scale, same capture\n\
         canvas max_db (what /api/tiles serves and the surface draws) {max_plane:7.2} dB/Hz \
         <- {:+.1} dB, the max-hold's floor lift",
        QUIET.0 / 1e6,
        QUIET.1 / 1e6,
        f.header.fft_size.unwrap(),
        max_plane - display,
    );
    assert!(
        (mean_plane - display).abs() <= 1.5,
        "the two paths disagree on the noise floor by {:.2} dB ({display:.2} vs {mean_plane:.2}); \
         until they agree, no dB reported by this file is a fidelity measurement",
        (mean_plane - display).abs(),
    );
}

// ---------------------------------------------------------------------------------------------
// 3. "Washed out": contrast, on structure whose truth this file holds.
// ---------------------------------------------------------------------------------------------

/// **The contrast the canvas loses, in dB, on a line whose width is known.** The 100.000 MHz
/// reference harmonic is essentially CW. Two mechanisms act on it and both are measured here:
/// the history chain's coarser STFT (a tone's PSD *density* falls as `10·log10(bin ratio)`, while
/// the noise floor, a true density, does not move) and the max-hold statistic (which lifts the
/// *floor* while leaving a deterministic peak where it is).
///
/// The number reported is a property of **one emission's SNR against the same recording's own
/// noise, measured twice** — once through each path. It is not a difference between two images.
#[test]
#[ignore = "T-483 PROVES THE GAP AND IS EXPECTED TO FAIL. It is `#[ignore]`d only so one \
            known-red proof does not block every other merge; deleting this line is T-484's \
            definition of done. Run it: cargo test -p hk-e2e --test canvas_fidelity -- --ignored \
            --nocapture"]
fn max_hold_folding_washes_out_the_narrow_spur_and_the_station() {
    let Some(f) = run() else { return };
    let tol_db = 2.0f32;

    let probe = |name: &str, hz: f64, half: f64| {
        let d_peak = f.display_row_level(hz - half, hz + half);
        let d_floor = median(f.display_band(QUIET.0, QUIET.1));
        let c_peak = f.canvas_row_level(hz - half, hz + half);
        let c_floor = median(f.canvas_band_max(QUIET.0, QUIET.1));
        let (d_snr, c_snr) = (d_peak - d_floor, c_peak - c_floor);
        eprintln!(
            "  {name:<22} display {d_peak:7.1} over floor {d_floor:7.1} = {d_snr:5.1} dB | \
             canvas {c_peak:7.1} over floor {c_floor:7.1} = {c_snr:5.1} dB | lost {:5.1} dB",
            d_snr - c_snr
        );
        (d_snr, c_snr)
    };

    let d_floor = median(f.display_band(QUIET.0, QUIET.1));
    let c_floor = median(f.canvas_band_max(QUIET.0, QUIET.1));
    let c_floor_mean = median(f.canvas_band(QUIET.0, QUIET.1, &f.canvas_mean_db));
    let (_, _, fold) = f.fold();
    eprintln!(
        "T-483 contrast. Quiet-band floor: display {d_floor:.1} dB, canvas max_db {c_floor:.1} dB \
         (lifted {:.1} dB), canvas mean_db {c_floor_mean:.1} dB.\n\
         The lift is the max-hold: the max of N exponential noise samples sits ~10*log10(ln N + \
         0.577) dB above their mean, and the canvas's N is the Welch segments per row times the \
         {fold:.0} FFT cells per canvas cell. `mean_db` is the plane T-483's brief describes \
         (RegridPlan::mean); `max_db` is the plane /api/tiles serves and the canvas draws.",
        c_floor - d_floor,
    );

    let (d_spur, c_spur) = probe("spur 100.000 MHz", SPUR_HZ, PROBE_HALF_HZ);
    let (d_osc, c_osc) = probe("narrowband 100.465", OSC_HZ, PROBE_HALF_HZ);
    let (d_stn, c_stn) = probe("WFM 101.3 MHz", STATION_HZ, STATION_HALF_HZ);

    for (name, d, c) in [
        ("the 100.000 MHz spur", d_spur, c_spur),
        ("the 100.465 MHz emission", d_osc, c_osc),
        ("the 101.3 MHz station", d_stn, c_stn),
    ] {
        assert!(
            (d - c) <= tol_db,
            "{name} stands {d:.1} dB above the noise in the FFT frames and only {c:.1} dB above it \
             on the canvas: {:.1} dB of contrast lost between two readings of the same samples. \
             That is the \"washed out\" the user reports, in dB.",
            d - c,
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 4. "Reads nearly straight": the time variation the canvas keeps.
// ---------------------------------------------------------------------------------------------

/// **The wave, measured.** The user's complaint that the 100.465 MHz emission "reads nearly
/// straight" is a claim about variation along the time axis. This measures it: the standard
/// deviation of the channel's level across rows, in each path, over the identical window.
///
/// That number is a property of **one channel's level-versus-time, sampled at two cadences** — the
/// display path's own row rate against the canvas's one-second cell. A canvas row is a *max* over
/// its second, so it keeps the crests and discards the troughs; the variation that survives is what
/// the waterfall can still draw.
#[test]
#[ignore = "T-483 PROVES THE GAP AND IS EXPECTED TO FAIL. It is `#[ignore]`d only so one \
            known-red proof does not block every other merge; deleting this line is T-484's \
            definition of done. Run it: cargo test -p hk-e2e --test canvas_fidelity -- --ignored \
            --nocapture"]
fn one_second_cells_flatten_the_wave_the_display_path_still_shows() {
    let Some(f) = run() else { return };

    let channel = |name: &str, hz: f64, half: f64| {
        let d: Vec<f32> = f
            .display_channel_track(hz - half, hz + half)
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        let c = f.canvas_channel_track(hz - half, hz + half);
        let (ds, cs) = (std_dev(&d), std_dev(&c));
        eprintln!(
            "  {name:<22} display {:>4} rows, sd {ds:5.2} dB | canvas {:>3} rows, sd {cs:5.2} dB \
             | {:.0}% of the variation retained",
            d.len(),
            c.len(),
            100.0 * cs / ds,
        );
        (ds, cs, d.len(), c.len())
    };

    eprintln!(
        "T-483 time detail, window {:.2} s:",
        (f.window.1 - f.window.0) as f64 * 1e-9
    );
    let (d_osc, c_osc, d_rows, c_rows) = channel("narrowband 100.465", OSC_HZ, PROBE_HALF_HZ);
    let (d_stn, c_stn, _, _) = channel("WFM 101.3 MHz", STATION_HZ, STATION_HALF_HZ);

    assert!(
        c_osc >= 0.5 * d_osc,
        "the 100.465 MHz emission varies by {d_osc:.2} dB (sd) across the FFT frames and only \
         {c_osc:.2} dB across the canvas rows — {:.0}% of the wave retained. The station is worse: \
         {d_stn:.2} dB against {c_stn:.2} dB, {:.0}% retained. That is the user's \"reads nearly \
         straight\", as a number.",
        100.0 * c_osc / d_osc,
        100.0 * c_stn / d_stn,
    );
    assert!(
        c_rows >= d_rows,
        "over the same {:.2} s the display path draws {d_rows} rows and the canvas {c_rows}: the \
         canvas resolves {:.0}x less of the time axis, so a second of level change becomes one \
         number.",
        (f.window.1 - f.window.0) as f64 * 1e-9,
        d_rows as f64 / c_rows.max(1) as f64,
    );
}
