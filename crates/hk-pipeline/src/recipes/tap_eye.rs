//! Stage tap `view=eye` (stream contract §14.4; T-161): the clock-recovery diagnostic — the
//! waveform segment around every symbol instant, folded onto one grid, plus the sample instants
//! themselves — so the Decode workbench can draw an eye/timing diagram (ADR-0013 §4.9 gap 5)
//! instead of guessing whether clock recovery is sampling in the open part of the eye.
//!
//! **Where this runs.** [`EyeTap::push`] is called from [`super::taps::StageTap::publish`],
//! inline on the pipeline thread, at the same point the `view=spectrum` tap
//! ([`super::tap_spectrum`]) computes its PSD. Per sample the work is one multiply-accumulate
//! into a complex rotator (no transcendentals: the rotator is stepped, not recomputed) plus a
//! push into a bounded buffer; per *row* it is [`EYE_ROW_LEN`] linear interpolations. As with the
//! other views, [`super::taps::StageTap::publish`] resets and skips this work entirely while no
//! consumer is attached, and while a row is being dropped for rate ([`row_shape`]) the samples are
//! discarded without buffering or accumulating at all — so an unopened or abandoned tap costs
//! nothing and the decode chain never stalls on a slow reader.
//!
//! **What a row holds.** One row is [`EYE_TRACES_PER_ROW`] consecutive traces of
//! [`EYE_TRACE_LEN`] points, laid out trace-major (trace `k` occupies
//! `[k * EYE_TRACE_LEN, (k + 1) * EYE_TRACE_LEN)`). Trace `k` is the waveform around the `k`-th
//! symbol instant of that row, resampled onto a fixed grid spanning [`EYE_SPAN_SYMBOLS`] symbol
//! periods centred on the instant: point `j` is the waveform at
//! `instant + (j − EYE_TRACE_LEN/2) / EYE_TRACE_LEN * EYE_SPAN_SYMBOLS` symbol periods. With the
//! contract's constants (64 points over 2 symbol periods) that means:
//!
//! - point **32** is the symbol instant — where a correctly timed slicer samples, and where the
//!   eye is **open** (traces separate into the modulation's levels);
//! - points **16** and **48** are half a symbol either side — between instants, where transitions
//!   cross and the eye is **closed**.
//!
//! **The sample instants.** The instants are not assumed, they are estimated here, per row, and
//! reported: the record's `sample_index` is the port element index of the row's **first symbol
//! instant**, and the rest follow at one symbol period each. So a reader gets both halves of what
//! ADR-0013 gap 5 asks for — the per-symbol waveform segments *and* their sample instants — and
//! can line the eye up against the raw `view=raw` tap of the same port. The estimate itself is
//! sub-sample, and the traces are folded on it at full precision; only the reported
//! `sample_index` is rounded to the nearest port element, because it is an integer index. That
//! rounding is at most half a sample — below the trace grid's own step (`EYE_SPAN_SYMBOLS · sps /
//! EYE_TRACE_LEN` samples) whenever there are 32 or more samples per symbol, and never more than
//! a percent of a symbol at the rates a decode chain actually taps.
//!
//! **How the instants are estimated.** The classical square-law (Oerder–Meyr) non-data-aided
//! timing estimate, over exactly the samples of the row. Writing `N` for the samples per symbol
//! and `s(n) = x(n)²`, `s` is periodic at the symbol rate and peaks at the symbol instants (a
//! shaped PAM pulse is at its peak there, and a discriminator waveform dips only through its
//! transitions), so `s(n) ≈ A + B·cos(2π(n − φ)/N)` for an instant phase `φ`. Then
//! `X = Σ s(n)·e^{−j2πn/N} ≈ (BM/2)·e^{−j2πφ/N}`, because the constant term and the second
//! harmonic both average away over whole periods — hence `φ = −arg(X)·N/(2π) (mod N)`, which is
//! what [`EyeTap::instant_phase`] computes. It needs no training sequence, no lock and no state
//! carried between rows, so a row is a fair, independent look at the timing.
//!
//! **Row rate.** Rows are held to [`EYE_MAX_ROWS_PER_S`] (25/s, the same cap as `view=spectrum`
//! and `view=sync_search`) by dropping whole windows once the symbol rate would push past it; a
//! row always shows [`EYE_TRACES_PER_ROW`] *consecutive* symbols, but successive rows need not be
//! contiguous. [`declared_row_rate_hz`] is exact, so the header never promises a rate the tap
//! doesn't keep.
//!
//! **Gating.** An eye row is content (`StreamKind::payload_is_content`, `hk_stream::header`), and
//! more plainly so than `view=sync_search`: point [`EYE_TRACE_LEN`]`/2` of each trace *is* the
//! pre-decision soft symbol, in symbol order, so slicing that one column of a row recovers the
//! demodulated bitstream directly — no caller-chosen probe needed. It is withheld under a class
//! that forbids content, exactly like the `iq`/`real` port it reads (§14.5).

use std::f64::consts::TAU;

use hk_recipe::Recipe;
use hk_stream::{StreamHeader, StreamKind};
use num_complex::Complex32;

use super::taps::StreamCtx;

/// Row-rate cap, rows/s (§14.4: `view=spectrum`, `view=sync_search` and `view=eye` share it).
pub const EYE_MAX_ROWS_PER_S: f64 = 25.0;

/// Points per trace. Fixed by the contract, so a reader needs no extra header field: the traces
/// in a row are `fft_size / EYE_TRACE_LEN`.
pub const EYE_TRACE_LEN: usize = 64;

/// Traces (consecutive symbols) per row.
pub const EYE_TRACES_PER_ROW: usize = 64;

/// Symbol periods a trace spans, centred on the symbol instant. 2 is the classical eye: one
/// instant at the centre and the neighbouring instants at the two edges.
pub const EYE_SPAN_SYMBOLS: f64 = 2.0;

/// `f32`s in one row (`fft_size`). 4096 — the same 16 KiB row as `view=spectrum`.
pub const EYE_ROW_LEN: usize = EYE_TRACE_LEN * EYE_TRACES_PER_ROW;

/// Fewest samples per symbol an eye can be drawn from: below 2 there is nothing *between* the
/// instants to close, so the diagram would be meaningless rather than merely coarse.
pub const EYE_MIN_SPS: f64 = 2.0;

/// Most samples per symbol served, bounding the window a row buffers
/// (`(EYE_TRACES_PER_ROW + 2) * EYE_MAX_SPS` samples ≈ 264 KiB). Past this the port should be
/// decimated before it is tapped.
pub const EYE_MAX_SPS: f64 = 1024.0;

/// Symbols buffered per row: the row's traces plus one symbol of margin at each end, since a
/// trace reaches half a symbol past its own instant.
const WINDOW_SYMBOLS: usize = EYE_TRACES_PER_ROW + 2;

/// Samples per symbol of a port at `rate_hz` carrying `symbol_rate_bd` symbols/s, if an eye can
/// be drawn from it at all (see [`EYE_MIN_SPS`]/[`EYE_MAX_SPS`]).
pub fn samples_per_symbol(rate_hz: f64, symbol_rate_bd: f64) -> Option<f64> {
    let positive = |v: f64| v.is_finite() && v > 0.0;
    if !positive(rate_hz) || !positive(symbol_rate_bd) {
        return None;
    }
    let sps = rate_hz / symbol_rate_bd;
    (EYE_MIN_SPS..=EYE_MAX_SPS).contains(&sps).then_some(sps)
}

/// Samples one row's window buffers, and how many whole windows to drop between published rows so
/// the true row rate never exceeds [`EYE_MAX_ROWS_PER_S`].
fn row_shape(rate_hz: f64, sps: f64) -> (usize, u32) {
    let window = (WINDOW_SYMBOLS as f64 * sps).ceil() as usize;
    let rows_per_s = rate_hz / window as f64;
    let decimate = (rows_per_s / EYE_MAX_ROWS_PER_S).ceil().max(1.0) as u32;
    (window, decimate)
}

/// The declared row rate for a `view=eye` header on a port at `rate_hz` carrying
/// `symbol_rate_bd` symbols/s. Always at most [`EYE_MAX_ROWS_PER_S`]; `0.0` when no eye can be
/// served (see [`samples_per_symbol`]).
pub fn declared_row_rate_hz(rate_hz: f64, symbol_rate_bd: f64) -> f64 {
    let Some(sps) = samples_per_symbol(rate_hz, symbol_rate_bd) else {
        return 0.0;
    };
    let (window, decimate) = row_shape(rate_hz, sps);
    rate_hz / (window as f64 * f64::from(decimate))
}

/// A header for a stage tap's `view=eye` (§14.4). `fft_size` is the row length, the same
/// convention `tap_spectrum::spectrum_header` and `tap_sync_search::sync_search_header` use; there
/// is no RF geometry (`center_hz`/`bandwidth_hz` are left unset — the row's axes are time-within-a-
/// symbol and amplitude, not frequency), and `sample_rate_hz` is the row rate, not the port rate.
pub fn eye_header(
    ctx: &StreamCtx,
    recipe: &Recipe,
    stream_id: String,
    rate_hz: f64,
    symbol_rate_bd: f64,
) -> StreamHeader {
    let mut h = StreamHeader::new(
        stream_id,
        StreamKind::Eye,
        ctx.class,
        format!("hk-pipeline:recipe:{}@{}", recipe.id, recipe.version),
    );
    h.datatype = Some("rf32_le".into());
    h.fft_size = Some(EYE_ROW_LEN as u32);
    h.sample_rate_hz = Some(declared_row_rate_hz(rate_hz, symbol_rate_bd));
    h.emitter_id = ctx.emitter_id;
    h
}

/// Streaming eye/timing diagram over a stage tap's samples: buffers one row's window, estimates
/// the symbol instants in it, and folds the waveform around each instant onto the fixed grid
/// described in the [module docs](self).
pub struct EyeTap {
    sps: f64,
    window: usize,
    decimate: u32,
    /// Samples still to be discarded for a window dropped by [`row_shape`]'s decimation.
    skip_samples: usize,
    buf: Vec<f32>,
    /// Absolute port element index of `buf[0]`.
    win_base: f64,
    /// Oerder–Meyr accumulator `X` over the current window, and its stepped rotator.
    acc: (f64, f64),
    rot: (f64, f64),
    step: (f64, f64),
    rot_age: u32,
    row: Vec<f32>,
    /// Set on a reset (no consumer, or a chunk discontinuity/edit); the next completed row
    /// carries `DISCONTINUITY` so a reader knows it isn't contiguous with the last one.
    pending_reset: bool,
}

impl EyeTap {
    /// A tap engine folding a port at `rate_hz` at `symbol_rate_bd` symbols/s. `None` when no eye
    /// can be drawn from that combination ([`samples_per_symbol`]).
    pub fn new(rate_hz: f64, symbol_rate_bd: f64) -> Option<Self> {
        let sps = samples_per_symbol(rate_hz, symbol_rate_bd)?;
        let (window, decimate) = row_shape(rate_hz, sps);
        let a = -TAU / sps;
        Some(Self {
            sps,
            window,
            decimate,
            skip_samples: 0,
            buf: Vec::with_capacity(window),
            win_base: 0.0,
            acc: (0.0, 0.0),
            rot: (1.0, 0.0),
            step: (a.cos(), a.sin()),
            rot_age: 0,
            row: vec![0.0; EYE_ROW_LEN],
            pending_reset: true, // the first row after opening is a fresh window too
        })
    }

    /// Discards buffered/partial-window state (no consumer, or a chunk-level reset).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.skip_samples = 0;
        self.pending_reset = true;
    }

    /// `f32`s in a row, for callers sizing an encode scratch buffer.
    pub fn row_len(&self) -> usize {
        EYE_ROW_LEN
    }

    /// Feeds real samples whose first element is at port element index `chunk_index`, and calls
    /// `emit(row, discontinuity, first_instant_index)` for every completed row that isn't dropped
    /// for rate. `first_instant_index` is the (fractional) port element index of the row's first
    /// symbol instant.
    pub fn push_real(
        &mut self,
        samples: &[f32],
        chunk_index: f64,
        emit: impl FnMut(&[f32], bool, f64),
    ) {
        self.push(samples.iter().copied(), chunk_index, emit);
    }

    /// Feeds complex baseband samples, folding the **in-phase** component — the same axis
    /// `clock_recovery` slices by default (`soft_from: in-phase`). See [`EyeTap::push_real`].
    pub fn push_iq(
        &mut self,
        samples: &[Complex32],
        chunk_index: f64,
        emit: impl FnMut(&[f32], bool, f64),
    ) {
        self.push(samples.iter().map(|z| z.re), chunk_index, emit);
    }

    fn push(
        &mut self,
        samples: impl Iterator<Item = f32>,
        chunk_index: f64,
        mut emit: impl FnMut(&[f32], bool, f64),
    ) {
        for (i, x) in samples.enumerate() {
            if self.skip_samples > 0 {
                self.skip_samples -= 1;
                continue;
            }
            if self.buf.is_empty() {
                self.win_base = chunk_index + i as f64;
                self.acc = (0.0, 0.0);
                self.rot = (1.0, 0.0);
                self.rot_age = 0;
            }
            self.buf.push(x);
            let p = f64::from(x) * f64::from(x);
            self.acc.0 += p * self.rot.0;
            self.acc.1 += p * self.rot.1;
            self.advance_rotator();
            if self.buf.len() >= self.window {
                let first = self.fold_row();
                emit(&self.row, self.pending_reset, first);
                self.pending_reset = false;
                self.buf.clear();
                self.skip_samples = self.window * (self.decimate as usize - 1);
            }
        }
    }

    /// One step of `e^{−j2πn/sps}`, renormalised often enough that rounding never accumulates.
    fn advance_rotator(&mut self) {
        let (r, i) = self.rot;
        self.rot = (
            r * self.step.0 - i * self.step.1,
            r * self.step.1 + i * self.step.0,
        );
        self.rot_age += 1;
        if self.rot_age >= 1024 {
            let n = self.rot.0.hypot(self.rot.1);
            if n > 0.0 {
                self.rot = (self.rot.0 / n, self.rot.1 / n);
            }
            self.rot_age = 0;
        }
    }

    /// The symbol-instant phase in the buffered window, samples in `[0, sps)`: the Oerder–Meyr
    /// estimate `φ = −arg(X)·N/(2π)` (see the [module docs](self)).
    fn instant_phase(&self) -> f64 {
        let phi = -self.acc.1.atan2(self.acc.0) / TAU * self.sps;
        if phi.is_finite() {
            phi.rem_euclid(self.sps)
        } else {
            0.0
        }
    }

    /// Folds the buffered window into `self.row`; returns the port element index of the first
    /// symbol instant.
    fn fold_row(&mut self) -> f64 {
        let phi = self.instant_phase();
        // Half the trace grid, in samples: a trace spans EYE_SPAN_SYMBOLS symbol periods.
        let per_point = EYE_SPAN_SYMBOLS * self.sps / EYE_TRACE_LEN as f64;
        let centre = (EYE_TRACE_LEN / 2) as f64;
        for k in 0..EYE_TRACES_PER_ROW {
            // One symbol of left margin, so a trace's leading half never reads before buf[0].
            let instant = phi + (k as f64 + 1.0) * self.sps;
            let base = k * EYE_TRACE_LEN;
            for j in 0..EYE_TRACE_LEN {
                let at = instant + (j as f64 - centre) * per_point;
                self.row[base + j] = self.sample_at(at);
            }
        }
        self.win_base + phi + self.sps
    }

    /// Linear interpolation of the window at a (fractional, in-range) sample position.
    fn sample_at(&self, at: f64) -> f32 {
        if !at.is_finite() || at < 0.0 {
            return self.buf.first().copied().unwrap_or(0.0);
        }
        let i = at.floor();
        let frac = (at - i) as f32;
        let i = i as usize;
        let a = self.buf.get(i).copied().unwrap_or(0.0);
        let b = self.buf.get(i + 1).copied().unwrap_or(a);
        a + (b - a) * frac
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::ContentClass;

    fn recipe() -> Recipe {
        serde_json::from_value(serde_json::json!({
            "schema": "hackriff.recipe", "schema_version": 2, "id": "test", "version": 1,
            "name": "test", "input": {"port": "iq"},
            "nodes": [{"id": "a", "block": "identity"}],
            "outputs": [],
            "output_policy": {"content_class": "unrestricted"}
        }))
        .unwrap()
    }

    fn ctx() -> StreamCtx {
        StreamCtx {
            pipeline_id: "p1".into(),
            class: ContentClass::Unrestricted,
            center_hz: 101.3e6,
            bandwidth_hz: 48_000.0,
            emitter_id: None,
            channels: Vec::new(),
            measured: None,
        }
    }

    /// xorshift64*, deterministic and dependency-free.
    struct Rng(u64);
    impl Rng {
        fn bit(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            if self.0 & 1 == 0 { -1.0 } else { 1.0 }
        }
    }

    /// Raised-cosine pulse (roll-off `beta`) at `t` symbol periods from its centre. The *received*
    /// pulse of a matched RRC pair, so it is zero at every other symbol instant: no ISI, and a
    /// wide-open eye at the instants with transitions crossing between them.
    fn raised_cosine(t: f64, beta: f64) -> f64 {
        if t.abs() < 1e-9 {
            return 1.0;
        }
        use std::f64::consts::{FRAC_PI_4, PI};
        let denom = 1.0 - (2.0 * beta * t).powi(2);
        if denom.abs() < 1e-6 {
            // Removable singularity at t = ±1/(2β), where the pulse is (π/4)·sinc(1/(2β)).
            let u = 1.0 / (2.0 * beta);
            return FRAC_PI_4 * (PI * u).sin() / (PI * u);
        }
        let sinc = (PI * t).sin() / (PI * t);
        sinc * (PI * beta * t).cos() / denom
    }

    /// A known 2-PAM (BPSK) waveform: random ±1 symbols, raised-cosine shaped, at `sps` samples
    /// per symbol, with symbol instants at `offset + k*sps` for a **hidden** `offset`.
    fn pam_waveform(n: usize, sps: f64, offset: f64, seed: u64) -> Vec<f32> {
        const BETA: f64 = 0.35;
        const SPAN: i64 = 8; // symbols either side
        let mut rng = Rng(seed);
        let n_syms = (n as f64 / sps).ceil() as usize + 2 * SPAN as usize + 4;
        let syms: Vec<f32> = (0..n_syms).map(|_| rng.bit()).collect();
        let mut out = vec![0.0f32; n];
        for (i, o) in out.iter_mut().enumerate() {
            // Symbol k sits at sample offset + k*sps.
            let t = (i as f64 - offset) / sps; // position in symbol periods
            let k0 = t.round() as i64;
            let mut v = 0.0f64;
            for k in (k0 - SPAN)..=(k0 + SPAN) {
                if k < 0 || k as usize >= syms.len() {
                    continue;
                }
                v += f64::from(syms[k as usize]) * raised_cosine(t - k as f64, BETA);
            }
            *o = v as f32;
        }
        out
    }

    /// The vertical eye opening at trace point `j`: the gap between the lowest trace above zero
    /// and the highest trace below it. Large = open (the levels are cleanly separated); at or
    /// below zero = closed (traces cross through the middle). `None` if one side is empty.
    fn opening(row: &[f32], j: usize) -> Option<f32> {
        let mut lo_pos = f32::INFINITY;
        let mut hi_neg = f32::NEG_INFINITY;
        for k in 0..EYE_TRACES_PER_ROW {
            let v = row[k * EYE_TRACE_LEN + j];
            if v > 0.0 {
                lo_pos = lo_pos.min(v);
            } else {
                hi_neg = hi_neg.max(v);
            }
        }
        (lo_pos.is_finite() && hi_neg.is_finite()).then_some(lo_pos - hi_neg)
    }

    /// **The usefulness test (T-161 acceptance).** On a known 2-PAM signal whose symbol instants
    /// are planted at a hidden offset, the tap must (a) find those instants and (b) produce a
    /// diagram that is open *at* them and closed *between* them.
    ///
    /// **Thresholds, fixed before running and justified:**
    /// - **Instant accuracy ≤ 0.10 symbol periods.** The estimator never sees `TRUE_OFFSET`; a
    ///   slicer timed 10% of a symbol early or late is still comfortably inside a raised-cosine
    ///   eye, so this is the loosest error that would still be *useful*, and far tighter than
    ///   chance (a blind guess is uniform over a whole symbol, so it clears 0.10 only 20% of the
    ///   time).
    /// - **Opening ≥ 0.5·A at the instant** (`A` = the mean |value| there, i.e. the level the
    ///   modulation actually reaches). Half the level separation is the textbook "eye is open";
    ///   a raised-cosine pulse has no ISI at the instants, so the true opening is ~2·A and 0.5·A
    ///   leaves generous room.
    /// - **Opening ≤ 0.15·A half a symbol either side.** Random data guarantees transitions at
    ///   nearly every boundary, and a transition passes through zero there, so the opening
    ///   collapses. 0.15·A allows for interpolation and finite-trace-count slack while still
    ///   being unambiguously "closed" — it is 3.3× below the open bar, so the two can never be
    ///   confused.
    ///
    /// **What it measured** (deterministic — fixed seed, fixed offset): instant error **0.0067**
    /// symbol periods against a 0.10 bar (a third of a sample at 48 samples/symbol); opening at
    /// the instant **1.97·A** against a 0.5·A bar — essentially the whole 2·A a zero-ISI
    /// raised-cosine eye can give; opening half a symbol either side **0.0058·A** against a
    /// 0.15·A bar, i.e. shut. Declared row rate 15.15 rows/s against the 25 cap. The eye is open
    /// where the tap says the symbols are and closed between them, by a margin of ~340×.
    #[test]
    fn eye_opens_at_the_true_symbol_instants_and_closes_between_them() {
        const RATE_HZ: f64 = 48_000.0;
        const SYMBOL_RATE_BD: f64 = 1_000.0;
        const SPS: f64 = RATE_HZ / SYMBOL_RATE_BD; // 48
        // Hidden from the estimator: a deliberately awkward fractional offset.
        const TRUE_OFFSET: f64 = 17.3;

        let mut tap = EyeTap::new(RATE_HZ, SYMBOL_RATE_BD).expect("48 samples/symbol is servable");
        assert_eq!(tap.decimate, 1, "1000 Bd is well inside the row-rate cap");
        let window = tap.window;
        let wave = pam_waveform(window * 2, SPS, TRUE_OFFSET, 0xC0FF_EE15_C0FF_EE15);

        let mut rows: Vec<(Vec<f32>, f64)> = Vec::new();
        tap.push_real(&wave, 0.0, |row, _disc, first| {
            rows.push((row.to_vec(), first))
        });
        assert!(!rows.is_empty(), "expected at least one row");
        let (row, first_instant) = &rows[0];
        assert_eq!(
            row.len(),
            EYE_ROW_LEN,
            "row length is the declared fft_size"
        );

        // (a) The reported instants are the true ones. Both are absolute port element indices, so
        // compare them modulo a symbol period (which symbol the row starts on doesn't matter).
        let err_samples = {
            let d = (first_instant - TRUE_OFFSET).rem_euclid(SPS);
            d.min(SPS - d)
        };
        assert!(
            err_samples / SPS <= 0.10,
            "estimated first instant {first_instant} is {err} symbol periods from the true \
             timing (offset {TRUE_OFFSET}, {SPS} samples/symbol); bar is 0.10",
            err = err_samples / SPS
        );

        // (b) The eye is open at the instant (trace point EYE_TRACE_LEN/2) and closed half a
        // symbol either side (points at ±EYE_TRACE_LEN/4).
        let centre = EYE_TRACE_LEN / 2;
        let level = {
            let s: f32 = (0..EYE_TRACES_PER_ROW)
                .map(|k| row[k * EYE_TRACE_LEN + centre].abs())
                .sum();
            s / EYE_TRACES_PER_ROW as f32
        };
        assert!(
            level > 0.1,
            "the test signal should reach a real level: {level}"
        );

        let open = opening(row, centre).expect("both levels present at the instant");
        assert!(
            open >= 0.5 * level,
            "eye should be OPEN at the symbol instant: opening {open} < 0.5 * level {level}"
        );
        for j in [centre - EYE_TRACE_LEN / 4, centre + EYE_TRACE_LEN / 4] {
            let shut = opening(row, j).expect("both levels present between instants");
            assert!(
                shut <= 0.15 * level,
                "eye should be CLOSED half a symbol from the instant (point {j}): opening \
                 {shut} > 0.15 * level {level} (open-at-instant was {open})"
            );
        }
    }

    /// The instants the tap reports are the instants it folded on: trace point `EYE_TRACE_LEN/2`
    /// of trace `k` must equal the waveform at `first_instant + k` symbol periods. Without this,
    /// a reader could not line the eye up against the raw tap of the same port.
    #[test]
    fn reported_instants_index_the_centre_of_each_trace() {
        const RATE_HZ: f64 = 48_000.0;
        const SYMBOL_RATE_BD: f64 = 1_000.0;
        const SPS: f64 = RATE_HZ / SYMBOL_RATE_BD;
        let mut tap = EyeTap::new(RATE_HZ, SYMBOL_RATE_BD).unwrap();
        let wave = pam_waveform(tap.window * 2, SPS, 17.3, 7);
        let mut rows = Vec::new();
        tap.push_real(&wave, 0.0, |row, _d, first| {
            rows.push((row.to_vec(), first))
        });
        let (row, first) = &rows[0];
        for k in [0usize, 1, 17, EYE_TRACES_PER_ROW - 1] {
            let at = first + k as f64 * SPS;
            let i = at.floor() as usize;
            let frac = (at - at.floor()) as f32;
            let expect = wave[i] + (wave[i + 1] - wave[i]) * frac;
            let got = row[k * EYE_TRACE_LEN + EYE_TRACE_LEN / 2];
            assert!(
                (got - expect).abs() < 1e-5,
                "trace {k} centre {got} should be the waveform at instant {at} ({expect})"
            );
        }
    }

    #[test]
    fn declared_row_rate_is_bounded_at_low_and_extreme_rates() {
        for (rate, bd) in [
            (48_000.0, 1_000.0),
            (48_000.0, 9_600.0),
            (240_000.0, 1_187.5),
            (2_000_000.0, 250_000.0),
            (20_000_000.0, 2_000_000.0),
            (1_000.0, 500.0),
        ] {
            let r = declared_row_rate_hz(rate, bd);
            assert!(
                r > 0.0 && r <= EYE_MAX_ROWS_PER_S + 1e-9,
                "rate_hz={rate} symbol_rate_bd={bd}: declared row rate {r}"
            );
        }
    }

    /// The declared rate is what the tap actually emits, not an aspiration.
    #[test]
    fn declared_row_rate_matches_the_rows_actually_emitted() {
        const RATE_HZ: f64 = 48_000.0;
        for bd in [1_000.0, 9_600.0, 24_000.0] {
            let mut tap = EyeTap::new(RATE_HZ, bd).unwrap();
            let secs = 4.0;
            let n = (RATE_HZ * secs) as usize;
            let wave = pam_waveform(n, RATE_HZ / bd, 3.1, 11);
            let mut count = 0usize;
            tap.push_real(&wave, 0.0, |_r, _d, _f| count += 1);
            let measured = count as f64 / secs;
            let declared = declared_row_rate_hz(RATE_HZ, bd);
            assert!(
                measured <= EYE_MAX_ROWS_PER_S + 1e-9,
                "{bd} Bd: measured {measured} rows/s exceeds the cap"
            );
            assert!(
                (measured - declared).abs() <= declared * 0.05 + 0.3,
                "{bd} Bd: measured {measured} rows/s vs declared {declared}"
            );
        }
    }

    #[test]
    fn header_shape() {
        let h = eye_header(&ctx(), &recipe(), "s1".into(), 48_000.0, 1_000.0);
        assert_eq!(h.kind, StreamKind::Eye);
        assert_eq!(h.datatype.as_deref(), Some("rf32_le"));
        assert_eq!(h.fft_size, Some(EYE_ROW_LEN as u32));
        assert!(h.sample_rate_hz.unwrap() <= EYE_MAX_ROWS_PER_S);
        assert!(
            h.center_hz.is_none() && h.bandwidth_hz.is_none(),
            "an eye row's axes are symbol time and amplitude, not frequency"
        );
    }

    #[test]
    fn reset_marks_the_next_row_as_a_discontinuity() {
        let mut tap = EyeTap::new(48_000.0, 1_000.0).unwrap();
        let wave = pam_waveform(tap.window * 3, 48.0, 0.0, 3);
        let mut flags = Vec::new();
        tap.push_real(&wave, 0.0, |_r, d, _f| flags.push(d));
        assert_eq!(flags.first(), Some(&true), "fresh tap starts as a reset");
        tap.reset();
        let mut flags2 = Vec::new();
        tap.push_real(&wave, 0.0, |_r, d, _f| flags2.push(d));
        assert_eq!(flags2.first(), Some(&true), "reset() marks the next row");
        assert!(
            flags2.iter().skip(1).all(|d| !d),
            "only the first row after a reset is marked"
        );
    }

    /// A row is folded from the samples it was given whatever the chunk boundaries are, and the
    /// reported instant is an absolute port element index.
    #[test]
    fn chunking_does_not_change_the_row_or_the_reported_instant() {
        let whole = {
            let mut tap = EyeTap::new(48_000.0, 1_000.0).unwrap();
            let wave = pam_waveform(tap.window * 2, 48.0, 17.3, 5);
            let mut rows = Vec::new();
            tap.push_real(&wave, 0.0, |r, _d, f| rows.push((r.to_vec(), f)));
            rows.remove(0)
        };
        let split = {
            let mut tap = EyeTap::new(48_000.0, 1_000.0).unwrap();
            let wave = pam_waveform(tap.window * 2, 48.0, 17.3, 5);
            let mut rows = Vec::new();
            for (c, part) in wave.chunks(377).enumerate() {
                tap.push_real(part, (c * 377) as f64, |r, _d, f| {
                    rows.push((r.to_vec(), f))
                });
            }
            rows.remove(0)
        };
        assert_eq!(whole.0, split.0, "same row whatever the chunking");
        assert!((whole.1 - split.1).abs() < 1e-9, "same reported instant");
    }

    #[test]
    fn unservable_symbol_rates_refused() {
        assert!(
            EyeTap::new(48_000.0, 48_000.0).is_none(),
            "1 sample/symbol: nothing between the instants"
        );
        assert!(EyeTap::new(48_000.0, 0.0).is_none(), "zero symbol rate");
        assert!(
            EyeTap::new(48_000.0, -5.0).is_none(),
            "negative symbol rate"
        );
        assert!(EyeTap::new(48_000.0, f64::NAN).is_none(), "NaN symbol rate");
        assert!(
            EyeTap::new(48_000.0, 1.0).is_none(),
            "48000 samples/symbol is past the window bound"
        );
        assert!(
            EyeTap::new(48_000.0, 48_000.0 / EYE_MAX_SPS).is_some(),
            "exactly the bound is servable"
        );
    }

    /// The iq path folds the in-phase axis, the same one `clock_recovery` slices by default.
    #[test]
    fn iq_folds_the_in_phase_axis() {
        let mut tap = EyeTap::new(48_000.0, 1_000.0).unwrap();
        let wave = pam_waveform(tap.window * 2, 48.0, 17.3, 5);
        let mut real_rows = Vec::new();
        tap.push_real(&wave, 0.0, |r, _d, _f| real_rows.push(r.to_vec()));

        let mut tap = EyeTap::new(48_000.0, 1_000.0).unwrap();
        let iq: Vec<Complex32> = wave.iter().map(|&x| Complex32::new(x, -3.0)).collect();
        let mut iq_rows = Vec::new();
        tap.push_iq(&iq, 0.0, |r, _d, _f| iq_rows.push(r.to_vec()));
        assert_eq!(real_rows[0], iq_rows[0]);
    }
}
