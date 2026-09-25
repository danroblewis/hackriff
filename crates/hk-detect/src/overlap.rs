//! **T-978: overlap is an error signal, and the answer comes from the spectrum.**
//!
//! CLAUDE.md: *"Real emissions essentially never overlap in time–frequency (two that truly did
//! would not demodulate), so overlapping Confirmed/Candidate boxes are proof the analysis is wrong,
//! with at least one true signal inside the union. The system **detects the overlap and
//! automatically re-analyzes that region** to resolve it to the real signal(s)."*
//!
//! T-369 built the detecting half in `hk_model::repo` (stage 4 of `resolve_overlaps`), but its
//! "re-analysis" reads the **stored bands of the rows' own detections** and merges them into
//! contiguous modes. For a region whose boxes overlap by construction that answer is one mode
//! almost always, so the verdict fell through to `hk_model::relate::distinguishing_evidence`, whose
//! bandwidth-ratio guard reads two cuts of one emission as two emissions and contests the region
//! for ever. That is what the explorer saw over the air on 2026-09-25: 852.8586 MHz (9.3 kHz) and
//! 852.8591 MHz (26.2 kHz) served side by side for one P25 emission, and a 557 kHz box at
//! 861.4346 MHz served beside the 20–80 kHz fragments it covered.
//!
//! This module is the measuring half: it re-analyses the union region against the **integrated
//! power spectrum** — the same mean PSD and mean floor the emitter-level detector already
//! maintains ([`crate::IntegratedSnapshot`]) — and answers what emissions are actually there, with
//! the resolution it answered at. [`hk_model::region_verdicts`] then maps the rows onto them.
//!
//! # The measurement
//!
//! Over the region's bins, and against the floor those same bins carry:
//!
//! 1. a bin is **in** an emission when its SNR clears [`IntegrationConfig::extend_db`], and a run
//!    of such bins is an emission only if one of them clears [`IntegrationConfig::seed_db`] — the
//!    detector's own published seed/extend rule (`detect_integrated`), applied to a region rather
//!    than to the whole span, so the same energy gets the same answer here as it does there;
//! 2. runs separated by fewer than [`OverlapConfig::split_bins`] cells are one emission: a gap of
//!    one cell is the resolution, not evidence of two signals;
//! 3. each emission's **centre** is the excess-weighted centroid and its **occupied bandwidth**
//!    the [`OverlapConfig::obw_fraction`] span of that excess — OBW99 by default, the same
//!    convention `hk_estimate` uses, re-estimated here from the spectrum rather than taken from a
//!    box.
//!
//! # What it refuses to say
//!
//! The measurement carries its resolution, and `hk_model::region_verdicts` refuses to act when the
//! region's narrowest box spans fewer than `hk_model::REGION_MIN_BINS` cells. A 9 kHz box in a
//! spectrum drawn at 4.9 kHz cells is not something this can have an opinion about, and saying so
//! is the same honesty rule as the canvas's three tiers.
//!
//! # Cost (T-453)
//!
//! One pass over the region's bins, twice: once to find the runs, once per run for the centroid
//! and the OBW span. No allocation per bin; one `Vec` of emissions, capped by
//! [`OverlapConfig::max_emissions`]. Nothing here runs on the capture thread — it runs where the
//! inventory writer already holds the region, and only for a region an overlap was found in.

use hk_model::{FreqRange, MeasuredEmission, RegionMeasurement};

use crate::alpha::db;
use crate::config::IntegrationConfig;
use crate::integrated::IntegratedSnapshot;

/// Settings of the region re-analysis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlapConfig {
    /// Seed and extend thresholds over the floor: the detector's own integrated rule.
    pub integration: IntegrationConfig,
    /// Cells of floor that must separate two runs before they are two emissions. One cell is the
    /// resolution itself, so the least honest value is 2.
    pub split_bins: usize,
    /// Share of an emission's excess power inside its reported occupied band (OBW99).
    pub obw_fraction: f64,
    /// Most emissions reported for one region; past it the measurement says nothing rather than a
    /// truncated part of the truth.
    pub max_emissions: usize,
}

impl Default for OverlapConfig {
    fn default() -> Self {
        Self {
            integration: IntegrationConfig::default(),
            split_bins: 2,
            obw_fraction: 0.99,
            max_emissions: 32,
        }
    }
}

/// Re-analyses `region` against `spectrum` and reports the emissions actually measured there.
///
/// `None` when the region lies outside the spectrum's span — the front end was not tuned there, so
/// there is no measurement and nothing may be concluded (never an empty answer, which would read
/// as "nothing is on air").
pub fn measure_region(
    region: FreqRange,
    spectrum: &IntegratedSnapshot,
    cfg: &OverlapConfig,
) -> Option<RegionMeasurement> {
    let g = &spectrum.geometry;
    if !(g.bin_width_hz.is_finite() && g.bin_width_hz > 0.0) || g.bins == 0 {
        return None;
    }
    if spectrum.mean_psd.len() < g.bins || spectrum.mean_floor.len() < g.bins {
        return None;
    }
    let lo = g.bin_at_or_above(region.lo_hz);
    let hi = g.bin_at_or_above(region.hi_hz).max(lo + 1).min(g.bins);
    if lo >= hi {
        return None;
    }
    // The region must lie inside the spectrum's usable span: an edge-zone answer is the filter
    // skirt's, not the air's.
    let usable = |b: usize| g.offset_hz(b as f64).abs() <= g.usable_half_hz;
    if !usable(lo) || !usable(hi - 1) {
        return None;
    }

    let mut out = RegionMeasurement::empty(region, g.bin_width_hz, hi - lo);
    out.span_s = spectrum.span_s;
    out.obw_fraction = cfg.obw_fraction;

    let snr_db =
        |b: usize| db((spectrum.mean_psd[b] / spectrum.mean_floor[b].max(1e-300)).max(1e-30));
    let excess = |b: usize| (spectrum.mean_psd[b] - spectrum.mean_floor[b]).max(0.0);

    // 1–2. Runs of bins over the extend threshold, bridged across gaps narrower than `split_bins`.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut b = lo;
    while b < hi {
        if snr_db(b) < cfg.integration.extend_db {
            b += 1;
            continue;
        }
        let start = b;
        while b < hi && snr_db(b) >= cfg.integration.extend_db {
            b += 1;
        }
        match runs.last_mut() {
            Some((_, end)) if start - *end < cfg.split_bins.max(1) => *end = b,
            _ => runs.push((start, b)),
        }
    }

    for (r0, r1) in runs {
        if !(r0..r1).any(|b| snr_db(b) >= cfg.integration.seed_db) {
            continue;
        }
        if out.emissions.len() == cfg.max_emissions {
            return None;
        }
        let total: f64 = (r0..r1).map(excess).sum();
        if !(total.is_finite() && total > 0.0) {
            continue;
        }
        let center_hz = (r0..r1)
            .map(|b| g.bin_hz(b as f64) * excess(b))
            .sum::<f64>()
            / total;
        // 3. The OBW span: the shortest run of cells holding `obw_fraction` of the excess, taken
        // symmetrically from the tails so an asymmetric skirt does not move the band.
        let tail = 0.5 * (1.0 - cfg.obw_fraction.clamp(0.0, 1.0)) * total;
        let (mut cum, mut b_lo, mut b_hi) = (0.0, r0, r1);
        for b in r0..r1 {
            cum += excess(b);
            if cum >= tail {
                b_lo = b;
                break;
            }
        }
        cum = 0.0;
        for b in (r0..r1).rev() {
            cum += excess(b);
            if cum >= tail {
                b_hi = b + 1;
                break;
            }
        }
        let (f_lo, f_hi) = g.extent_hz(b_lo, b_hi.max(b_lo + 1));
        let peak_snr_db = (r0..r1).map(snr_db).fold(f64::NEG_INFINITY, f64::max);
        out.emissions.push(MeasuredEmission {
            band: FreqRange::new(f_lo, f_hi),
            center_hz,
            obw_hz: (f_hi - f_lo).max(g.bin_width_hz),
            peak_snr_db,
        });
    }
    Some(out)
}
