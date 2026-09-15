//! Frame input and canonical-grid resampling.
//!
//! [`FrameInput`] is the one input type the pyramid folds: a borrowed, linear PSD row with its
//! geometry, time extent and a provenance digest. Converters build it from an hk-dsp STFT
//! [`hk_dsp::SpectrumFrame`] (zero-copy) or from the dB-valued hk-model
//! [`SweepFrame`] / [`hk_model::SpectrumFrame`] (via [`DbScratch`]).
//!
//! # Regridding onto the canonical grid
//!
//! Frame bin `i` covers `[f_lo + i·bw, f_lo + (i+1)·bw)`; level-0 cell `c` covers
//! `[c·w, (c+1)·w)`. For each cell the frame covers by **at least half its width**:
//!
//! - **value** (feeds mean, percentiles, histogram and occupancy) = the overlap-weighted mean of the
//!   linear PSD of every bin that overlaps the cell. PSD is a density, so this conserves
//!   integrated power: finer bins average down, coarser bins spread their density unchanged.
//! - **peak** (feeds max) = the maximum of the per-bin peak (the frame's max-hold when it has one,
//!   else its PSD) over every bin with any positive overlap. A tone therefore keeps its peak level
//!   in every cell its bin touches; max is never diluted by resampling.
//! - **caller floor** (optional, dB) = the overlap-weighted mean of the floor in dB.
//!
//! Cells the frame covers by less than half are not touched (not observed).

use hk_model::attention::baseline::SiteKey;
use hk_model::{
    CalibrationStateId, PowerUnit, SpectrumFrame as ModelSpectrumFrame, SpurMaskId, SweepFrame,
    Timestamp,
};

use super::stats::undb;

/// Front-end gain state, for the per-tile provenance summary.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct GainState {
    /// LNA gain, dB.
    pub lna_db: f32,
    /// VGA gain, dB.
    pub vga_db: f32,
    /// RF amp on.
    pub amp_on: bool,
}

impl GainState {
    /// A stable non-zero 32-bit key of this gain state (FNV-1a of the settings quantised to whole
    /// dB, −0 as +0), the baseline gain-state key (T-132; 0 is reserved for unknown).
    pub fn key(&self) -> u32 {
        // `+ 0.0` turns −0.0 into +0.0.
        let q = |db: f32| (db.round() + 0.0).to_bits().to_le_bytes();
        let mut b = [0u8; 9];
        b[..4].copy_from_slice(&q(self.lna_db));
        b[4..8].copy_from_slice(&q(self.vga_db));
        b[8] = u8::from(self.amp_on);
        let h = b.iter().fold(0x811c_9dc5_u32, |h, &x| {
            (h ^ u32::from(x)).wrapping_mul(0x0100_0193)
        });
        h.max(1)
    }
}

/// A short front-end filter or antenna-port name (at most 16 bytes, longer names truncated at a
/// character boundary), so [`FrameInput`] stays `Copy`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PortTag([u8; 16]);

impl PortTag {
    /// Tag of `name` (truncated to 16 bytes).
    pub fn new(name: &str) -> Self {
        let mut end = name.len().min(16);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let mut b = [0u8; 16];
        b[..end].copy_from_slice(&name.as_bytes()[..end]);
        Self(b)
    }

    /// The raw 16 bytes (zero-padded).
    pub fn bytes(&self) -> [u8; 16] {
        self.0
    }

    /// From raw bytes (as stored).
    pub fn from_bytes(b: [u8; 16]) -> Self {
        Self(b)
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        let n = self.0.iter().position(|&c| c == 0).unwrap_or(16);
        std::str::from_utf8(&self.0[..n]).unwrap_or("")
    }
}

impl std::fmt::Debug for PortTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PortTag({:?})", self.as_str())
    }
}

impl std::fmt::Display for PortTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Front-end configuration beyond the gain state (T-116, C26 "store provenance per tile"): a
/// change in any of these makes a step in the spectrum that is not an event.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct FrontEnd {
    /// Gain-table / gain-calibration profile version in force, caller-assigned.
    pub gain_table: Option<u32>,
    /// Active filter or antenna port (e.g. a filter-bank path or Opera Cake port).
    pub filter: Option<PortTag>,
    /// Spur-mask version in force.
    pub spur_mask: Option<SpurMaskId>,
}

/// The noise statistics of a frame's values, used to bias-correct the stored low percentile
/// (T-116; the Gamma model of `hk_dsp::radiometry::bias`).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum NoiseShape {
    /// Unknown: queries report the raw low percentile only (`floor_db` is NaN).
    #[default]
    Unknown,
    /// An STFT of this resolution: the pyramid derives the level-0 cell shape
    /// (`hk_dsp::radiometry::cell_value_shape`, cached per resolution).
    Spectrum(hk_dsp::spectrum::Resolution),
    /// The Gamma shape `n_c` of a level-0 cell value, when the caller knows it (e.g. synthetic
    /// k-look noise on a grid aligned with the cells).
    CellShape(f32),
    /// The Gamma shape `k` of each **input bin** value (T-126): a sweep or `hackrf_sweep` row whose
    /// look count is known, or estimated by [`super::NoiseShapeEstimator`]. The pyramid derives
    /// the level-0 cell shape as `k · max(1, f_cell / bin_width)`: a cell no wider than a bin reads
    /// one bin's value (exact), a wider cell averages `f_cell / bin_width` bins taken as
    /// independent (rectangular-window FFT bins; a tapered window makes this an overestimate —
    /// use [`NoiseShape::Spectrum`] for those).
    BinShape(f32),
}

/// A stable 64-bit source key for [`FrameInput::source`] from a source name (FNV-1a), e.g. a device
/// id or `"hackrf_sweep:<file>"`. Stable across runs, so persisted step state finds its source.
pub fn source_key(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// One frame to fold into the pyramid. See the [module docs](self) for the resampling rules.
#[derive(Clone, Copy, Debug)]
pub struct FrameInput<'a> {
    /// Start of the observation the frame represents.
    pub t: Timestamp,
    /// Observation time the frame represents, ns (coverage and occupancy weight). The frame is
    /// assigned to the time cell containing its midpoint.
    pub duration_ns: i64,
    /// Lower edge of bin 0, Hz.
    pub f_lo_hz: f64,
    /// Bin spacing, Hz.
    pub bin_width_hz: f64,
    /// Unit of `psd` (must match the pyramid's).
    pub unit: PowerUnit,
    /// Linear power spectral density per bin (FS²/Hz or mW/Hz), ascending frequency.
    pub psd: &'a [f32],
    /// Linear per-bin peak within the frame (e.g. the STFT max-hold), same geometry as `psd`.
    pub peak: Option<&'a [f32]>,
    /// Caller-supplied noise floor per bin, dB/Hz (e.g. from T-005). Occupancy threshold =
    /// floor + margin. When absent, the pyramid's default floor is used.
    pub floor_db: Option<&'a [f32]>,
    /// Gain state, if known.
    pub gain: Option<GainState>,
    /// Front end judged overloaded/clipped for this frame.
    pub suspect: bool,
    /// Samples lost immediately before this frame.
    pub dropped_samples: u64,
    /// Calibration in force.
    pub calibration: Option<CalibrationStateId>,
    /// Gain table, filter and spur-mask versions in force (T-116).
    pub front_end: FrontEnd,
    /// Noise statistics of the values, for the bias-corrected floor (T-116).
    pub noise_shape: NoiseShape,
    /// Which source produced the frame (T-126; e.g. [`source_key`] of a device id, 0 by default).
    /// Provenance steps compare a frame with the previous frame **of the same source**, and that
    /// state is persisted per source, so interleaved sources and store restarts do not show false
    /// steps. T-133: recorded per tile ([`super::ProvenanceSummary::origins`]).
    pub source: u64,
    /// The site the device was at when the frame was taken (T-133; ADR-0012 §3.5), recorded per
    /// tile with `source`. `None` when the caller does not know it: stored as an unknown site,
    /// which only an unfiltered or an explicit `unknown` site filter matches.
    pub site: Option<SiteKey>,
}

/// A frame's origin as the caller states it (T-133): [`FrameInput::source`] and
/// [`FrameInput::site`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct FrameOrigin {
    /// Source key ([`source_key`]).
    pub source: u64,
    /// Site, `None` when unknown.
    pub site: Option<SiteKey>,
}

impl<'a> FrameInput<'a> {
    /// A frame with no peak, floor or provenance detail.
    pub fn new(
        t: Timestamp,
        duration_ns: i64,
        f_lo_hz: f64,
        bin_width_hz: f64,
        unit: PowerUnit,
        psd: &'a [f32],
    ) -> Self {
        Self {
            t,
            duration_ns,
            f_lo_hz,
            bin_width_hz,
            unit,
            psd,
            peak: None,
            floor_db: None,
            gain: None,
            suspect: false,
            dropped_samples: 0,
            calibration: None,
            front_end: FrontEnd::default(),
            noise_shape: NoiseShape::Unknown,
            source: 0,
            site: None,
        }
    }

    /// The frame with `origin`'s source and site (T-133).
    pub fn with_origin(self, origin: FrameOrigin) -> Self {
        Self {
            source: origin.source,
            site: origin.site,
            ..self
        }
    }

    /// Adapts an hk-dsp STFT frame without copying: linear FS²/Hz PSD, max-hold as the peak,
    /// time = first sample, duration = `sample_count / sample_rate`, gain / overload / calibration
    /// from its provenance record.
    pub fn from_dsp(frame: &'a hk_dsp::SpectrumFrame) -> Self {
        let s = &frame.spectrum;
        let p = frame.provenance.get();
        let duration_ns = if s.sample_rate_hz > 0.0 {
            (frame.sample_count as f64 * 1e9 / s.sample_rate_hz).round() as i64
        } else {
            0
        };
        Self {
            t: frame.t.host_time,
            duration_ns,
            f_lo_hz: s.f_lo_hz(),
            bin_width_hz: s.bin_width_hz(),
            unit: PowerUnit::Dbfs,
            psd: &s.psd,
            peak: (s.max_hold.len() == s.psd.len()).then_some(&s.max_hold[..]),
            floor_db: None,
            gain: Some(GainState {
                lna_db: p.tune.lna_db as f32,
                vga_db: p.tune.vga_db as f32,
                amp_on: p.tune.amp_on,
            }),
            suspect: p.overload,
            dropped_samples: frame.dropped_samples,
            calibration: p.calibration_state_ref,
            front_end: FrontEnd {
                gain_table: None,
                filter: p.antenna_port.as_deref().map(PortTag::new),
                spur_mask: p.spur_mask_ref,
            },
            noise_shape: NoiseShape::Spectrum(s.resolution),
            source: 0,
            site: None,
        }
    }

    /// Upper edge of the last bin, Hz.
    pub fn f_hi_hz(&self) -> f64 {
        self.f_lo_hz + self.psd.len() as f64 * self.bin_width_hz
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.psd.is_empty() {
            return Err("empty psd");
        }
        if !(self.bin_width_hz.is_finite() && self.bin_width_hz > 0.0) {
            return Err("bin width must be positive");
        }
        if !self.f_lo_hz.is_finite() {
            return Err("f_lo not finite");
        }
        if self.duration_ns < 0 {
            return Err("negative duration");
        }
        if self.peak.is_some_and(|p| p.len() != self.psd.len()) {
            return Err("peak length differs from psd");
        }
        if self.floor_db.is_some_and(|p| p.len() != self.psd.len()) {
            return Err("floor length differs from psd");
        }
        Ok(())
    }
}

/// Reusable buffer converting dB-valued hk-model frames to [`FrameInput`] (allocation-free once
/// sized).
#[derive(Clone, Debug, Default)]
pub struct DbScratch {
    lin: Vec<f32>,
}

impl DbScratch {
    /// An empty scratch buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// A [`SweepFrame`] (power **per bin**, dB) as a density: `10^(dB/10) / bin_width`.
    /// `duration_ns` is the revisit time the sweep row represents.
    pub fn sweep_frame(&mut self, f: &SweepFrame, duration_ns: i64) -> FrameInput<'_> {
        let bw = f.bin_width_hz;
        self.lin.clear();
        self.lin
            .extend(f.power.iter().map(|&d| (undb(d) / bw) as f32));
        FrameInput::new(f.t, duration_ns, f.freq.lo_hz, bw, f.unit, &self.lin)
    }

    /// An hk-model [`ModelSpectrumFrame`] whose `psd` is a density in dB/Hz.
    pub fn spectrum_frame(&mut self, f: &ModelSpectrumFrame, duration_ns: i64) -> FrameInput<'_> {
        let bw = f.span_hz / f.psd.len().max(1) as f64;
        self.lin.clear();
        self.lin.extend(f.psd.iter().map(|&d| undb(d) as f32));
        FrameInput::new(
            f.t,
            duration_ns,
            f.f_center_hz - f.span_hz / 2.0,
            bw,
            f.unit,
            &self.lin,
        )
    }
}

/// One level-0 cell's footprint in a frame's bins.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CellSpan {
    /// Global level-0 cell index.
    pub cell: i64,
    /// First overlapping bin.
    pub b0: u32,
    /// One past the last overlapping bin.
    pub b1: u32,
    /// Overlap of bin `b0`, Hz.
    pub w_first: f64,
    /// Overlap of bin `b1 − 1`, Hz.
    pub w_last: f64,
    /// Total overlap, Hz.
    pub w_sum: f64,
}

/// The regrid plan for one frame geometry; rebuilt only when the geometry changes.
#[derive(Clone, Debug, Default)]
pub(crate) struct RegridPlan {
    key: Option<(u64, u64, usize, u64)>,
    bin_width_hz: f64,
    pub cells: Vec<CellSpan>,
}

impl RegridPlan {
    /// Makes the plan match `(f_lo, bw, n)` on a grid of `f_cell` Hz.
    pub fn ensure(&mut self, f_lo: f64, bw: f64, n: usize, f_cell: f64) {
        let key = (f_lo.to_bits(), bw.to_bits(), n, f_cell.to_bits());
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        self.bin_width_hz = bw;
        self.cells.clear();
        let f_hi = f_lo + n as f64 * bw;
        let eps = 1e-9 * bw.min(f_cell);
        let c_first = (f_lo / f_cell).floor() as i64;
        let c_last = (f_hi / f_cell).ceil() as i64 - 1;
        let overlap = |i: i64, lo: f64, hi: f64| {
            let a = lo.max(f_lo + i as f64 * bw);
            let b = hi.min(f_lo + (i + 1) as f64 * bw);
            (b - a).max(0.0)
        };
        for c in c_first..=c_last {
            let lo = c as f64 * f_cell;
            let hi = lo + f_cell;
            let a = lo.max(f_lo);
            let b = hi.min(f_hi);
            if b - a < 0.5 * f_cell - eps {
                continue;
            }
            let mut b0 = (((a - f_lo) / bw).floor() as i64).clamp(0, n as i64 - 1);
            let mut b1 = (((b - f_lo) / bw).ceil() as i64).clamp(b0 + 1, n as i64);
            while b0 + 1 < b1 && overlap(b0, lo, hi) <= eps {
                b0 += 1;
            }
            while b1 - 1 > b0 && overlap(b1 - 1, lo, hi) <= eps {
                b1 -= 1;
            }
            self.cells.push(CellSpan {
                cell: c,
                b0: b0 as u32,
                b1: b1 as u32,
                w_first: overlap(b0, lo, hi),
                w_last: overlap(b1 - 1, lo, hi),
                w_sum: b - a,
            });
        }
    }

    /// Overlap-weighted mean of `v` over the span (linear values).
    #[inline]
    pub fn mean(&self, s: &CellSpan, v: &[f32]) -> f64 {
        let (b0, b1) = (s.b0 as usize, s.b1 as usize);
        if b1 - b0 == 1 {
            return f64::from(v[b0]);
        }
        let inner: f64 = v[b0 + 1..b1 - 1].iter().map(|&x| f64::from(x)).sum();
        (s.w_first * f64::from(v[b0]) + s.w_last * f64::from(v[b1 - 1]) + self.bin_width_hz * inner)
            / s.w_sum
    }

    /// Maximum of `v` over every bin with positive overlap.
    #[inline]
    pub fn max(&self, s: &CellSpan, v: &[f32]) -> f32 {
        v[s.b0 as usize..s.b1 as usize]
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-132 review: the gain-state key is quantised to whole dB and −0 equals +0.
    #[test]
    fn gain_state_key_is_quantised_to_whole_db() {
        let g = |lna_db, vga_db, amp_on| GainState {
            lna_db,
            vga_db,
            amp_on,
        };
        assert_eq!(g(16.0, 20.0, false).key(), g(16.3, 19.8, false).key());
        assert_eq!(g(0.0, 0.0, false).key(), g(-0.0, -0.2, false).key());
        assert_ne!(g(16.0, 20.0, false).key(), g(24.0, 20.0, false).key());
        assert_ne!(g(16.0, 20.0, false).key(), g(16.0, 20.0, true).key());
        assert_ne!(g(0.0, 0.0, false).key(), 0);
    }

    fn tone_frame(n: usize, bin: usize, floor: f32, tone: f32) -> Vec<f32> {
        let mut v = vec![floor; n];
        v[bin] = tone;
        v
    }

    #[test]
    fn finer_bins_average_and_keep_the_tone_max() {
        // 1 kHz bins onto 6.25 kHz cells, misaligned start.
        let mut p = RegridPlan::default();
        let f_lo = 100_000.0 + 300.0;
        p.ensure(f_lo, 1000.0, 64, 6250.0);
        let psd = tone_frame(64, 20, 1.0, 1000.0);
        let tone_f = f_lo + 20.5 * 1000.0;
        let cell = (tone_f / 6250.0).floor() as i64;
        let mut seen = 0;
        for s in &p.cells {
            let m = p.max(s, &psd);
            let mean = p.mean(s, &psd);
            if s.cell == cell {
                assert_eq!(m, 1000.0, "tone max preserved");
                // Integrated power conserved: tone bin fully inside the cell.
                let expected = (s.w_sum - 1000.0 + 1000.0 * 1000.0) / s.w_sum;
                assert!((mean - expected).abs() < 1e-9);
                seen += 1;
            } else {
                assert_eq!(m, 1.0);
                assert!((mean - 1.0).abs() < 1e-9);
            }
        }
        assert_eq!(seen, 1);
        // Integrated power over whole cells equals that over the covered bins.
    }

    #[test]
    fn coarser_bins_spread_density() {
        // 100 kHz sweep bins onto 6.25 kHz cells: every covered cell reads its bin's density.
        let mut p = RegridPlan::default();
        p.ensure(0.0, 100_000.0, 4, 6250.0);
        assert_eq!(p.cells.len(), 64);
        let psd = tone_frame(4, 2, 1e-12, 5e-9);
        for s in &p.cells {
            let expect = if (32..48).contains(&s.cell) {
                5e-9f32
            } else {
                1e-12
            };
            assert!((p.mean(s, &psd) / f64::from(expect) - 1.0).abs() < 1e-9);
            assert_eq!(p.max(s, &psd), expect);
        }
    }

    #[test]
    fn half_covered_edge_cells_only() {
        let mut p = RegridPlan::default();
        // Covers [1000, 13000): cell 0 [0, 6250) has 5250 Hz ≥ half → kept; cell 2 [12500,18750)
        // has 500 Hz → dropped.
        p.ensure(1000.0, 1000.0, 12, 6250.0);
        let cells: Vec<i64> = p.cells.iter().map(|s| s.cell).collect();
        assert_eq!(cells, vec![0, 1]);
        // Geometry cache: same inputs do not rebuild.
        let before = p.cells.as_ptr();
        p.ensure(1000.0, 1000.0, 12, 6250.0);
        assert_eq!(before, p.cells.as_ptr());
    }
}
