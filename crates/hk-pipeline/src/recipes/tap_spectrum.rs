//! Stage tap `view=spectrum` (stream contract §14.4; T-160): a bounded, on-demand PSD of an
//! `iq`/`real` stage output, so the decoder workbench can plot an FM multiplex (MPX) baseband and
//! its subcarriers (19 kHz pilot, 38 kHz stereo, 57 kHz RDS) without any DSP in the UI.
//!
//! **Where this runs.** [`SpectrumTap::push_real`]/[`push_iq`] are called from
//! [`super::taps::StageTap::publish`], inline on the pipeline thread, exactly where the existing
//! `view=raw` tap already copies the port's chunk into a binary record. Nothing here blocks or
//! waits: a segment's window + FFT + accumulate is O(`fft_size`) and runs only when the tap has
//! an open consumer ([`super::taps::StageTap::publish`] resets and skips otherwise, as the raw
//! path does), so an unopened or abandoned tap costs nothing and the decode chain never stalls on
//! a slow reader.
//!
//! **FFT size and averaging.** [`SPECTRUM_FFT_SIZE`] = 4096 with the default 50% overlap gives a
//! bin width of `rate_hz / 4096`; at the WFM MPX rate (`hk_demod::wfm::MPX_RATE_HZ` = 240 kHz)
//! that is ≈ 58.6 Hz, over two orders of magnitude finer than the ≥ 19 kHz spacing between the
//! pilot, stereo and RDS lines, so all three resolve as clearly separated peaks with wide margin
//! at any plausible MPX rate. Segments are averaged in linear power ([`row_averages`]) so each
//! emitted row represents about `1 / SPECTRUM_MAX_ROWS_PER_S` s of signal — smoothing noise
//! without smearing the (stationary) subcarrier lines — while never exceeding the §14.4 cap of
//! 25 rows/s (already inside the §6 gated-spectrum ceiling of 50 rows/s, so the row rate needs no
//! per-class adjustment).

use hk_recipe::{PortType, Recipe};
use hk_stream::{StreamHeader, StreamKind};
use num_complex::Complex32;

use hk_dsp::{PowerUnit, SegmentEngine, Spectrum, WelchConfig, WindowKind};

use super::taps::StreamCtx;

/// FFT length (bins) of a stage-tap spectrum. See the [module docs](self) for why this resolves
/// the 57 kHz RDS subcarrier with wide margin while staying cheap enough to run continuously.
pub const SPECTRUM_FFT_SIZE: usize = 4096;

/// Row rate cap, rows/s (§14.4: `view=spectrum` is served at at most 25 rows/s).
pub const SPECTRUM_MAX_ROWS_PER_S: f64 = 25.0;

fn welch_config() -> WelchConfig {
    let mut w = WelchConfig::new(SPECTRUM_FFT_SIZE);
    w.window = WindowKind::Hann;
    // Holds and spectral kurtosis are extra state this view doesn't use; skip them to keep the
    // per-segment cost to window + FFT + one accumulate.
    w.holds = false;
    w.spectral_kurtosis = false;
    w
}

/// Segments averaged into one row so the row rate stays at or below [`SPECTRUM_MAX_ROWS_PER_S`].
fn row_averages(rate_hz: f64, hop: usize) -> u32 {
    if hop == 0 || !(rate_hz.is_finite() && rate_hz > 0.0) {
        return 1;
    }
    ((rate_hz / (hop as f64 * SPECTRUM_MAX_ROWS_PER_S)).ceil() as u32).max(1)
}

/// The declared row rate for a stage-tap spectrum header at `rate_hz`.
pub fn declared_row_rate_hz(rate_hz: f64) -> f64 {
    let hop = welch_config().hop();
    let k = row_averages(rate_hz, hop);
    if !(rate_hz.is_finite() && rate_hz > 0.0) {
        return 0.0;
    }
    rate_hz / (hop as f64 * f64::from(k))
}

/// A header for a stage tap's `view=spectrum` (§14.4). `center_hz` is `0.0` for a `real` port
/// (a demodulated baseband waveform like FM MPX has no RF reference) and the tapped channel's own
/// centre for an `iq` port (still a downconverted RF signal).
pub fn spectrum_header(
    ctx: &StreamCtx,
    recipe: &Recipe,
    stream_id: String,
    port_ty: PortType,
    rate_hz: f64,
) -> StreamHeader {
    let mut h = StreamHeader::new(
        stream_id,
        StreamKind::Spectrum,
        ctx.class,
        format!("hk-pipeline:recipe:{}@{}", recipe.id, recipe.version),
    );
    h.datatype = Some("rf32_le".into());
    h.fft_size = Some(SPECTRUM_FFT_SIZE as u32);
    h.sample_rate_hz = Some(declared_row_rate_hz(rate_hz));
    h.center_hz = Some(if port_ty == PortType::Real {
        0.0
    } else {
        ctx.center_hz
    });
    h.bandwidth_hz = Some(rate_hz);
    h.emitter_id = ctx.emitter_id;
    h
}

/// Streaming PSD over a stage tap's samples: buffers input, runs complete (50%-overlap) segments
/// through a [`SegmentEngine`], and yields one dBFS/Hz row every [`row_averages`] segments.
pub struct SpectrumTap {
    engine: SegmentEngine,
    buf: Vec<Complex32>,
    spectrum: Spectrum,
    row: Vec<f32>,
    hop: usize,
    fft_len: usize,
    averages: u32,
    segments: u32,
    rate_hz: f64,
    /// Set on a reset (no consumer, or a chunk discontinuity/edit); the next completed row
    /// carries `DISCONTINUITY` so a reader knows the averaging window doesn't span the gap.
    pending_reset: bool,
}

impl SpectrumTap {
    /// A tap engine for a port at `rate_hz`.
    pub fn new(rate_hz: f64) -> Self {
        let config = welch_config();
        // `config` is a fixed, valid constant (fft_len 4096, overlap 2048 < 4096): this can't
        // fail.
        let engine = SegmentEngine::new(config).expect("fixed spectrum tap config is valid");
        let hop = config.hop();
        Self {
            spectrum: engine.empty_spectrum(),
            row: vec![0.0; SPECTRUM_FFT_SIZE],
            hop,
            fft_len: SPECTRUM_FFT_SIZE,
            averages: row_averages(rate_hz, hop),
            segments: 0,
            rate_hz,
            pending_reset: true, // the first row after opening is a fresh window too
            engine,
            buf: Vec::with_capacity(SPECTRUM_FFT_SIZE * 2),
        }
    }

    /// Discards buffered/partial-averaged state (no consumer, or a chunk-level reset).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.engine.reset();
        self.segments = 0;
        self.pending_reset = true;
    }

    /// Feeds real samples (imaginary part 0) and calls `emit(row_dbfs_per_hz, discontinuity)` for
    /// every row a segment completes.
    pub fn push_real(&mut self, samples: &[f32], emit: impl FnMut(&[f32], bool)) {
        self.push(samples.iter().map(|&x| Complex32::new(x, 0.0)), emit);
    }

    /// Feeds complex baseband samples. See [`SpectrumTap::push_real`].
    pub fn push_iq(&mut self, samples: &[Complex32], emit: impl FnMut(&[f32], bool)) {
        self.push(samples.iter().copied(), emit);
    }

    fn push(
        &mut self,
        samples: impl Iterator<Item = Complex32>,
        mut emit: impl FnMut(&[f32], bool),
    ) {
        self.buf.extend(samples);
        while self.buf.len() >= self.fft_len {
            self.engine.process(&self.buf[..self.fft_len]);
            self.buf.drain(..self.hop);
            self.segments += 1;
            if self.segments >= self.averages {
                self.engine
                    .finish_into(self.rate_hz, 0.0, &mut self.spectrum);
                self.spectrum
                    .write_db(&self.spectrum.psd, PowerUnit::DbfsPerHz, &mut self.row);
                self.engine.reset();
                self.segments = 0;
                emit(&self.row, self.pending_reset);
                self.pending_reset = false;
            }
        }
    }
}

/// Encodes one row as little-endian `f32` (§5.2 `rf32_le`), reusing `out`'s capacity.
pub fn encode_row(row: &[f32], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(4 * row.len());
    for v in row {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::ContentClass;
    use std::f32::consts::TAU;

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
            bandwidth_hz: 240_000.0,
            emitter_id: None,
            channels: Vec::new(),
        }
    }

    /// A synthetic MPX signal: pilot at 19 kHz, a (weak) 38 kHz stereo subcarrier and a 57 kHz
    /// RDS subcarrier, at a typical WFM MPX rate. The spectrum must show peaks at all three
    /// within a few bins.
    #[test]
    fn resolves_pilot_stereo_and_rds_subcarriers() {
        const RATE_HZ: f64 = 240_000.0;
        let mut tap = SpectrumTap::new(RATE_HZ);
        let mut rows: Vec<Vec<f32>> = Vec::new();
        // Enough samples for several averaged rows.
        let total = tap.hop * (tap.averages as usize) * 3 + tap.fft_len;
        let mut phase = [0.0f32; 3];
        let freqs = [19_000.0f32, 38_000.0f32, 57_000.0f32];
        let amps = [1.0f32, 0.3f32, 0.3f32];
        let chunk = 4096usize;
        let mut n = 0usize;
        while n < total {
            let m = chunk.min(total - n);
            let mut buf = vec![0.0f32; m];
            for s in &mut buf {
                let mut v = 0.0;
                for k in 0..3 {
                    v += amps[k] * phase[k].sin();
                    phase[k] += TAU * freqs[k] / RATE_HZ as f32;
                }
                *s = v * 0.3; // stay well inside full scale
            }
            tap.push_real(&buf, |row, _disc| rows.push(row.to_vec()));
            n += m;
        }
        assert!(!rows.is_empty(), "expected at least one averaged row");
        let row = rows.last().unwrap();
        let bins = row.len();
        let bin_hz = RATE_HZ / bins as f64;
        let peak_near = |target_hz: f64, tol_hz: f64| -> (usize, f32) {
            // Real input: the spectrum is DC-centred and mirrored; only the positive half
            // (bins >= N/2) carries the physically meaningful frequencies.
            let center = bins / 2;
            let lo = center + ((target_hz - tol_hz) / bin_hz).floor() as usize;
            let hi = (center + ((target_hz + tol_hz) / bin_hz).ceil() as usize).min(bins - 1);
            let (mut best_i, mut best_v) = (lo, f32::NEG_INFINITY);
            for (i, &v) in row.iter().enumerate().take(hi + 1).skip(lo) {
                if v > best_v {
                    best_v = v;
                    best_i = i;
                }
            }
            (best_i, best_v)
        };
        // Tolerance: a couple of bins either side is generous at ~58.6 Hz/bin.
        let tol_hz = 5.0 * bin_hz;
        let noise_floor = {
            let mut v: Vec<f32> = row.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        for &f in &freqs {
            let (i, p) = peak_near(f64::from(f), tol_hz);
            let got_hz = (i as f64 - (bins / 2) as f64) * bin_hz;
            assert!(
                (got_hz - f64::from(f)).abs() <= tol_hz,
                "peak for {f} Hz found at {got_hz} Hz (tol {tol_hz} Hz)"
            );
            assert!(
                p > noise_floor + 10.0,
                "{f} Hz peak ({p} dB) should stand well above the median bin ({noise_floor} dB)"
            );
        }
    }

    #[test]
    fn declared_row_rate_is_bounded() {
        let r = declared_row_rate_hz(2_000_000.0);
        assert!(r > 0.0 && r <= SPECTRUM_MAX_ROWS_PER_S + 1e-9, "{r}");
    }

    #[test]
    fn header_shape() {
        let h = spectrum_header(&ctx(), &recipe(), "s1".into(), PortType::Real, 240_000.0);
        assert_eq!(h.kind, StreamKind::Spectrum);
        assert_eq!(h.datatype.as_deref(), Some("rf32_le"));
        assert_eq!(h.fft_size, Some(SPECTRUM_FFT_SIZE as u32));
        assert_eq!(h.center_hz, Some(0.0), "real ports are baseband, not RF");
        assert_eq!(h.bandwidth_hz, Some(240_000.0));
        assert!(h.sample_rate_hz.unwrap() <= SPECTRUM_MAX_ROWS_PER_S);

        let h_iq = spectrum_header(&ctx(), &recipe(), "s2".into(), PortType::Iq, 240_000.0);
        assert_eq!(h_iq.center_hz, Some(101.3e6), "iq ports keep the RF centre");
    }

    #[test]
    fn reset_marks_the_next_row_as_a_discontinuity() {
        let mut tap = SpectrumTap::new(240_000.0);
        let mut flags = Vec::new();
        let buf = vec![0.01f32; tap.hop * (tap.averages as usize) + tap.fft_len];
        tap.push_real(&buf, |_row, disc| flags.push(disc));
        assert_eq!(flags.first(), Some(&true), "fresh tap starts as a reset");
        tap.reset();
        let mut flags2 = Vec::new();
        tap.push_real(&buf, |_row, disc| flags2.push(disc));
        assert_eq!(flags2.first(), Some(&true), "reset() marks the next row");
        assert!(
            flags2.iter().skip(1).all(|d| !d),
            "only the first row after a reset is marked"
        );
    }
}
