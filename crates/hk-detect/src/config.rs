//! Detector settings. Every default is S4's recommendation (REPORT §5); all are configurable.
//! [`DetectorConfig::detector_version`] encodes the settings into the `Detection::detector_version`
//! string with a hash, so history can be filtered or re-thresholded by configuration.

use std::fmt;

use hk_core::Discontinuity;
use hk_dsp::floor::FloorConfig;
use hk_dsp::window::WindowKind;
use hk_model::{FreqRange, SpurMask, SurveyId};

use crate::step::StepGuardConfig;

/// OS-CFAR reference window across frequency.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CfarWindow {
    /// Reference cells on each side of the cell under test (16 → N = 32).
    pub reference_per_side: usize,
    /// Guard cells on each side, excluded from the reference (4).
    pub guard_per_side: usize,
    /// Order-statistic rank `k`, 1-based: `Z` is the `k`-th smallest reference cell (24 = 3N/4).
    pub rank: usize,
}

impl Default for CfarWindow {
    fn default() -> Self {
        Self {
            reference_per_side: 16,
            guard_per_side: 4,
            rank: 24,
        }
    }
}

impl CfarWindow {
    /// `N`, the number of reference cells.
    pub fn reference_cells(&self) -> usize {
        2 * self.reference_per_side
    }

    /// Distance from the cell under test to the farthest reference cell.
    pub fn reach(&self) -> usize {
        self.guard_per_side + self.reference_per_side
    }
}

/// How the extend (hysteresis-off) threshold is set.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hysteresis {
    /// From its own per-cell false-alarm probability (S4: 1e-3). **Use this.**
    Pfa(f64),
    /// A fixed margin below the seed thresholds, dB. **Regression only**: at `n = 10` a fixed
    /// −3 dB puts the floor-branch off level ≈ 2 dB above mean noise, noise percolates and false
    /// boxes rise to 35 /MHz/h on ideal noise (S4 §3.3). Kept so a test can show the bug.
    FixedDb(f64),
}

/// Which detector branches are combined (S4 §3.3 compares all three).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Branches {
    /// OS-CFAR **or** floor branch (S4 recommended).
    Or,
    /// OS-CFAR only (fewest false alarms; loses wide-signal interiors).
    OsOnly,
    /// Floor branch only.
    FloorOnly,
}

/// The floor trace used as the floor-branch reference and the OS-branch guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloorReference {
    /// [`FloorFrame::floor`](hk_dsp::floor::FloorFrame::floor): per-frame block FCME. Unbiased on
    /// sloped floors; reads the signal inside flat signals wider than about one block, where the
    /// OS branch is the only cover. Guarded zones of the floor-step guard use the wide reference
    /// where the learned shape explains the step ([`crate::step`]). The default until T-033.
    PerFrame,
    /// [`FloorFrame::wide_floor`](hk_dsp::floor::FloorFrame::wide_floor): keeps wide-signal
    /// interiors and (T-005) is slope-robust on tilts, roll-offs and learned notches. Where it has
    /// cut a floor feature above the band floor (a shelf, a filter-bank passband: floor-like power
    /// statistics), the floor branch uses the per-frame floor instead ([`crate::step`]).
    /// **Default** (T-033 evaluation): every false-alarm case within 1.07× design where the floor
    /// branch runs, with 0 false boxes (T-028's `-20 dB below bin 1200, -10 dB step at 3200`: 614×
    /// → 1.05×; `+6 dB` passbands 100–260× → 1.03–1.07×); flat full chain 1 box in 1.02 MHz·h as
    /// on the per-frame reference; fixture results unchanged. Limit: a stationary noise-like wide
    /// emission (OFDM) reads as a floor feature, so its interior is not covered.
    Wide,
}

/// Seed/extend probabilities and duration rules for one band.
#[derive(Clone, Debug, PartialEq)]
pub struct DetectionProfile {
    /// Short name, part of `detector_version`.
    pub name: String,
    /// Seed per-cell false-alarm probability (both branches).
    pub pfa_on: f64,
    /// Extend threshold rule.
    pub hysteresis: Hysteresis,
    /// Minimum duration of a raw 4-connected component, frames, tested **before** gap merge.
    pub min_frames: u32,
    /// Survivors are merged across gaps of at most this many frames, **after** the duration test.
    pub gap_frames: u32,
    /// Branch combination for the band (e.g. `OsOnly` where an accessory filter edge sits in the
    /// span).
    pub branches: Branches,
}

impl DetectionProfile {
    /// S4 recommended: on 1e-6, off 1e-3, min 3 frames, gap 2 (0 false boxes in 3.31 MHz·h).
    pub fn standard() -> Self {
        Self {
            name: "standard".into(),
            pfa_on: 1e-6,
            hysteresis: Hysteresis::Pfa(1e-3),
            min_frames: 3,
            gap_frames: 2,
            branches: Branches::Or,
        }
    }

    /// S4 short-burst profile for bands where 2–4 ms bursts matter (ISM): on 1e-7, off 1e-3,
    /// min 2 frames (0 false boxes in 1.32 MHz·h).
    pub fn short_burst() -> Self {
        Self {
            name: "short-burst".into(),
            pfa_on: 1e-7,
            hysteresis: Hysteresis::Pfa(1e-3),
            min_frames: 2,
            gap_frames: 2,
            branches: Branches::Or,
        }
    }
}

/// A profile selected when the tuned centre lies in `freq`.
#[derive(Clone, Debug, PartialEq)]
pub struct BandProfile {
    /// Band (by tuned centre frequency).
    pub freq: FreqRange,
    /// Profile for the band.
    pub profile: DetectionProfile,
}

/// Rule 1: reference-harmonic mask (mandatory; the retune test cannot find these).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RefHarmonicRule {
    /// Reference frequency, Hz (10 MHz).
    pub step_hz: f64,
    /// Tolerance, ppm of the harmonic (25).
    pub ppm: f64,
    /// Minimum tolerance, Hz (10 kHz).
    pub min_tolerance_hz: f64,
    /// Only narrow detections, Hz (25 kHz, measured as the x-dB bandwidth).
    pub max_width_hz: f64,
    /// Disabled when bins are at least this wide, Hz (250 kHz: coverage too large).
    pub max_bin_width_hz: f64,
}

impl Default for RefHarmonicRule {
    fn default() -> Self {
        Self {
            step_hz: 10e6,
            ppm: 25.0,
            min_tolerance_hz: 10e3,
            max_width_hz: 25e3,
            max_bin_width_hz: 250e3,
        }
    }
}

/// Sample-clock harmonic: a narrow detection within `max(tolerance_bins · bin, min_tolerance_hz)`
/// of `n × fs` (the receiver's sample clock and LO share one crystal, so its harmonics are
/// crystal-locked lines; T-006 review: 434.000 MHz = 217 × 2 Msps). **Flags, never suppresses**
/// (434.000 MHz is also LPD433 channel 38).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockHarmonicRule {
    /// Apply the rule (true).
    pub enabled: bool,
    /// Tolerance in bins (2).
    pub tolerance_bins: f64,
    /// Minimum tolerance, Hz (2 kHz).
    pub min_tolerance_hz: f64,
    /// Only narrow detections, Hz (25 kHz, the x-dB bandwidth).
    pub max_width_hz: f64,
    /// Disabled when bins are at least this wide, Hz (250 kHz).
    pub max_bin_width_hz: f64,
}

impl Default for ClockHarmonicRule {
    fn default() -> Self {
        Self {
            enabled: true,
            tolerance_bins: 2.0,
            min_tolerance_hz: 2e3,
            max_width_hz: 25e3,
            max_bin_width_hz: 250e3,
        }
    }
}

/// Rule 2: DC / LO leakage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DcRule {
    /// Only detections at most this wide, Hz (40 kHz).
    pub max_width_hz: f64,
    /// The extent must come within this of the tuned centre, Hz (15 kHz).
    pub tolerance_hz: f64,
}

impl Default for DcRule {
    fn default() -> Self {
        Self {
            max_width_hz: 40e3,
            tolerance_hz: 15e3,
        }
    }
}

/// Rule 4: narrowband comb.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CombRule {
    /// Minimum lines on the grid (6).
    pub min_members: usize,
    /// Grid tolerance, Hz (±1.5 kHz).
    pub tolerance_hz: f64,
    /// Smallest spacing, Hz (100 kHz).
    pub min_spacing_hz: f64,
    /// Largest spacing, Hz (2 MHz).
    pub max_spacing_hz: f64,
    /// Largest harmonic of a pair spacing tried (40).
    pub max_harmonic: usize,
    /// Only lines at most this wide, Hz (20 kHz).
    pub max_line_width_hz: f64,
    /// Monte-Carlo trials for the chance probability (200).
    pub trials: usize,
    /// Flag only when the chance probability is below this (5 %).
    pub max_chance: f64,
    /// Lines considered at most (the strongest), bounding the search cost (32; the S4 comb had 14
    /// members among ~25 narrow lines).
    pub max_lines: usize,
    /// Monte-Carlo seed (deterministic).
    pub seed: u64,
}

impl Default for CombRule {
    fn default() -> Self {
        Self {
            min_members: 6,
            tolerance_hz: 1.5e3,
            min_spacing_hz: 100e3,
            max_spacing_hz: 2e6,
            max_harmonic: 40,
            max_line_width_hz: 20e3,
            trials: 200,
            max_chance: 0.05,
            max_lines: 32,
            seed: 0x5eed_c0b5,
        }
    }
}

/// Rule 5: IQ image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageRule {
    /// The mirror (2fc − f) must be at least this much stronger, dB (20).
    pub min_rejection_db: f64,
    /// Mirrored-shape correlation must exceed this (0.5).
    pub min_shape_correlation: f64,
    /// Not applied within this of the centre, Hz (20 kHz; DC).
    pub min_offset_hz: f64,
    /// At most this many bins: too few for a shape, accepted on level alone (4).
    pub max_bins_without_shape: usize,
}

impl Default for ImageRule {
    fn default() -> Self {
        Self {
            min_rejection_db: 20.0,
            min_shape_correlation: 0.5,
            min_offset_hz: 20e3,
            max_bins_without_shape: 4,
        }
    }
}

/// Band-edge zone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeRule {
    /// Usable half-span = `bandwidth_hz/2 · factor` (S4: 8 MHz for the 15 MHz filter → 16/15).
    pub bandwidth_factor: f64,
    /// … and at most `fs/2 − guard_bins·bin_width` (FFT edge bins).
    pub guard_bins: usize,
}

impl Default for EdgeRule {
    fn default() -> Self {
        Self {
            bandwidth_factor: 16.0 / 15.0,
            guard_bins: 8,
        }
    }
}

/// Spur, ghost, image, clip and trust rules (S4 rules 1, 2, 4, 5, 8 and the flag table).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rules {
    /// Rule 1.
    pub ref_harmonic: RefHarmonicRule,
    /// Sample-clock harmonics.
    pub clock_harmonic: ClockHarmonicRule,
    /// Rule 2.
    pub dc: DcRule,
    /// Rule 4.
    pub comb: CombRule,
    /// Rule 5.
    pub image: ImageRule,
    /// Edge zone.
    pub edge: EdgeRule,
    /// Rule 8: a frame is clipped when its clip fraction exceeds this (1e-4).
    pub clip_fraction: f64,
    /// `marginal` below this peak SNR, dB (10).
    pub marginal_snr_db: f64,
    /// A box with more than this fraction of its cells in impulsive frames joins the broadband
    /// impulsive event (0.5).
    pub impulsive_fraction: f64,
    /// T-237: a box spanning at least this fraction of the analysis window's bins is
    /// provenance-suspect and is flagged `marginal` (0.9). A detection that fills its own window
    /// carries no evidence that it is an emission rather than a floor or level artifact of the
    /// window itself: the reference it was measured against is drawn from the same bins, and the
    /// wide reference reads anything wider than ~55 % of the span as floor. Like the DC and image
    /// rules, this is a property of the geometry, not a threshold on the signal, so it neither
    /// hides the box nor changes what was measured.
    pub whole_window_fraction: f64,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            ref_harmonic: RefHarmonicRule::default(),
            clock_harmonic: ClockHarmonicRule::default(),
            dc: DcRule::default(),
            comb: CombRule::default(),
            image: ImageRule::default(),
            edge: EdgeRule::default(),
            clip_fraction: 1e-4,
            marginal_snr_db: 10.0,
            impulsive_fraction: 0.5,
            whole_window_fraction: 0.9,
        }
    }
}

/// The integrated (emitter-level) spectrum: a sliding window of blocks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IntegrationConfig {
    /// Block length, seconds (0.25); evaluated at every block boundary.
    pub block_s: f64,
    /// Blocks in the window (4 → 1 s).
    pub blocks: usize,
    /// Seed threshold over the floor, dB (+6).
    pub seed_db: f64,
    /// Extend threshold over the floor, dB (+3).
    pub extend_db: f64,
    /// An evaluation confirms emitter candidates only when it integrates at least this, s (1).
    pub confirm_s: f64,
}

impl Default for IntegrationConfig {
    fn default() -> Self {
        Self {
            block_s: 0.25,
            blocks: 4,
            seed_db: 6.0,
            extend_db: 3.0,
            confirm_s: 1.0,
        }
    }
}

/// Emitter-candidate confirmation by repeat.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConfirmConfig {
    /// A box repeats an earlier one that ended at most this long before it started, s (10).
    pub repeat_window_s: f64,
    /// Centre-frequency tolerance: `max(min_tolerance_bins · bin, bandwidth_fraction · OBW)`
    /// (C10: max(2 bins, 10 % BW)). A box also repeats when each centre lies inside the other's
    /// occupied extent (`f ± OBW/2`): payload-dependent centroids of wide bursts move by more
    /// than 10 % of the bandwidth, while for 1–2-bin boxes this stays ±1 bin (S4's track rule).
    pub min_tolerance_bins: f64,
    /// See `min_tolerance_bins`.
    pub bandwidth_fraction: f64,
    /// Recent detections remembered (512).
    pub recent_capacity: usize,
}

impl Default for ConfirmConfig {
    fn default() -> Self {
        Self {
            repeat_window_s: 10.0,
            min_tolerance_bins: 2.0,
            bandwidth_fraction: 0.1,
            recent_capacity: 512,
        }
    }
}

/// Frame discontinuities that close every open box (no merge across them).
pub const DETECT_RESET_ON: Discontinuity = Discontinuity::from_bits_truncate(
    Discontinuity::STREAM_START.bits()
        | Discontinuity::RETUNE.bits()
        | Discontinuity::RATE_CHANGE.bits()
        | Discontinuity::GAIN_CHANGE.bits()
        | Discontinuity::GAP.bits(),
);

/// [`Detector`](crate::Detector) settings.
#[derive(Clone, Debug, PartialEq)]
pub struct DetectorConfig {
    /// Survey the detections belong to.
    pub survey_id: SurveyId,
    /// OS-CFAR window.
    pub window: CfarWindow,
    /// OS-branch guard above the floor reference, dB (3). Not applied to the floor branch.
    pub guard_db: f64,
    /// Floor trace for the floor branch and the guard.
    pub floor_reference: FloorReference,
    /// Profile used outside every `band_profiles` entry.
    pub profile: DetectionProfile,
    /// Per-band profiles, first match by tuned centre.
    pub band_profiles: Vec<BandProfile>,
    /// Flag rules.
    pub rules: Rules,
    /// Integrated spectrum.
    pub integration: IntegrationConfig,
    /// Repeat confirmation.
    pub confirm: ConfirmConfig,
    /// A box still open after this long is emitted and continues as a new box, s (1).
    pub max_duration_s: f64,
    /// A finished box waits at most this many frames for undecided components it may merge
    /// with across a gap (64).
    pub max_hold_frames: u32,
    /// The x of the x-dB bandwidth, dB (10).
    pub xdb_level_db: f64,
    /// Occupied-bandwidth power fraction (0.99).
    pub obw_fraction: f64,
    /// Measured terminated-input spur map (rule 3), if any.
    pub spur_mask: Option<SpurMask>,
    /// Discontinuities that close open boxes ([`DETECT_RESET_ON`]).
    pub reset_on: Discontinuity,
    /// Floor-step guard ([`crate::step`]); `None` disables it.
    pub step_guard: Option<StepGuardConfig>,
    /// Known receiver response edges (accessory filter edges, path switches), absolute Hz: the
    /// floor branch is off within the step guard's margin of each.
    pub response_edges_hz: Vec<f64>,
    /// A frame with more region runs than this is dense: it is not labelled, only counted (1024).
    pub max_runs_per_frame: usize,
    /// At most this many live components; runs beyond it are dropped and counted (2048).
    pub max_live_components: usize,
    /// Identifies the floor tracker's method and settings in `detector_version`
    /// ([`Self::with_floor_config`]).
    pub floor_tag: String,
}

/// The spectral context of a segment, part of `detector_version`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RunContext {
    /// Effective averages (the Gamma shape the thresholds use).
    pub n_eff: f64,
    /// FFT length.
    pub fft_len: usize,
    /// Segment overlap, samples.
    pub overlap: usize,
    /// Window.
    pub window: WindowKind,
}

impl DetectorConfig {
    /// S4 defaults for `survey_id`.
    pub fn new(survey_id: SurveyId) -> Self {
        Self {
            survey_id,
            window: CfarWindow::default(),
            guard_db: 3.0,
            floor_reference: FloorReference::Wide,
            profile: DetectionProfile::standard(),
            band_profiles: Vec::new(),
            rules: Rules::default(),
            integration: IntegrationConfig::default(),
            confirm: ConfirmConfig::default(),
            max_duration_s: 1.0,
            max_hold_frames: 64,
            xdb_level_db: 10.0,
            obw_fraction: 0.99,
            spur_mask: None,
            reset_on: DETECT_RESET_ON,
            step_guard: Some(StepGuardConfig::default()),
            response_edges_hz: Vec::new(),
            max_runs_per_frame: 1024,
            max_live_components: 2048,
            floor_tag: "unspecified".into(),
        }
    }

    /// Records the floor tracker's method and settings (`block-fcme-256/64;h=<hash>`) in
    /// `detector_version`, and aligns the step guard's block layout with it.
    pub fn with_floor_config(mut self, floor: &FloorConfig) -> Self {
        self.floor_tag = format!(
            "block-fcme-{}/{};h={:016x}",
            floor.blocks.block_bins,
            floor.blocks.hop_bins,
            fnv1a64(format!("{floor:?}").as_bytes())
        );
        if let Some(g) = &mut self.step_guard {
            g.blocks = floor.blocks;
        }
        self
    }

    /// Checks the settings.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let w = &self.window;
        if w.reference_per_side == 0 || w.rank == 0 || w.rank > w.reference_cells() {
            return Err(ConfigError::Window(*w));
        }
        for p in std::iter::once(&self.profile).chain(self.band_profiles.iter().map(|b| &b.profile))
        {
            check_probability("pfa_on", p.pfa_on)?;
            match p.hysteresis {
                Hysteresis::Pfa(off) => {
                    check_probability("hysteresis pfa", off)?;
                    if off < p.pfa_on {
                        return Err(ConfigError::Invalid {
                            name: "hysteresis pfa must not be below pfa_on",
                            value: off,
                        });
                    }
                }
                Hysteresis::FixedDb(d) => check_non_negative("hysteresis dB", d)?,
            }
            if p.min_frames == 0 {
                return Err(ConfigError::Invalid {
                    name: "min_frames",
                    value: 0.0,
                });
            }
        }
        check_non_negative("guard_db", self.guard_db)?;
        check_positive("max_duration_s", self.max_duration_s)?;
        check_positive("integration.block_s", self.integration.block_s)?;
        if self.integration.blocks == 0 || self.integration.blocks > 4096 {
            return Err(ConfigError::Invalid {
                name: "integration.blocks must be in 1..=4096",
                value: self.integration.blocks as f64,
            });
        }
        if self.max_runs_per_frame == 0 || self.max_live_components == 0 {
            return Err(ConfigError::Invalid {
                name: "max_runs_per_frame and max_live_components must be positive",
                value: 0.0,
            });
        }
        if let Some(g) = &self.step_guard {
            check_positive("step_guard.jump_db", g.jump_db)?;
            check_non_negative("step_guard.margin_blocks", g.margin_blocks)?;
            check_non_negative("step_guard.plateau_match_db", g.plateau_match_db)?;
            check_non_negative("step_guard.wide_residual_db", g.wide_residual_db)?;
            check_positive("step_guard.wide_step_db", g.wide_step_db)?;
            check_positive("step_guard.wide_cut_db", g.wide_cut_db)?;
            check_non_negative("step_guard.release_db", g.release_db)?;
            check_positive(
                "step_guard.stat_time_constant_frames",
                g.stat_time_constant_frames,
            )?;
            check_non_negative("step_guard.floor_like_min", g.floor_like_min)?;
            if g.floor_like_max
                .partial_cmp(&g.floor_like_min)
                .is_none_or(|o| o.is_lt())
            {
                return Err(ConfigError::Invalid {
                    name: "step_guard.floor_like_max must not be below floor_like_min",
                    value: g.floor_like_max,
                });
            }
            if g.blocks.block_bins == 0 || g.blocks.hop_bins == 0 {
                return Err(ConfigError::Invalid {
                    name: "step_guard.blocks",
                    value: 0.0,
                });
            }
        }
        if !(self.obw_fraction > 0.0 && self.obw_fraction < 1.0) {
            return Err(ConfigError::Invalid {
                name: "obw_fraction",
                value: self.obw_fraction,
            });
        }
        check_probability("rules.clip_fraction", self.rules.clip_fraction)?;
        check_probability("rules.impulsive_fraction", self.rules.impulsive_fraction)?;
        check_probability(
            "rules.whole_window_fraction",
            self.rules.whole_window_fraction,
        )?;
        if self.rules.comb.max_lines < 3 || self.confirm.recent_capacity == 0 {
            return Err(ConfigError::Invalid {
                name: "comb.max_lines >= 3 and confirm.recent_capacity >= 1",
                value: self.rules.comb.max_lines as f64,
            });
        }
        Ok(())
    }

    /// Index of the profile for a tuned centre: `0` is [`Self::profile`], `i + 1` is
    /// `band_profiles[i]`.
    pub fn profile_index(&self, center_hz: f64) -> usize {
        self.band_profiles
            .iter()
            .position(|b| b.freq.lo_hz <= center_hz && center_hz <= b.freq.hi_hz)
            .map_or(0, |i| i + 1)
    }

    /// The profile at [`Self::profile_index`].
    pub fn profile_at(&self, index: usize) -> &DetectionProfile {
        if index == 0 {
            &self.profile
        } else {
            &self.band_profiles[index - 1].profile
        }
    }

    /// FNV-1a 64 of every setting except the survey id (and the spur mask body: its id only).
    pub fn settings_hash(&self) -> u64 {
        let text = format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            self.window,
            self.guard_db,
            (
                &self.step_guard,
                &self.response_edges_hz,
                self.max_runs_per_frame,
                self.max_live_components,
                &self.floor_tag
            ),
            self.floor_reference,
            self.profile,
            self.band_profiles,
            self.rules,
            self.integration,
            self.confirm,
            self.max_duration_s,
            self.max_hold_frames,
            (self.xdb_level_db, self.obw_fraction),
            self.spur_mask.as_ref().map(|m| m.id),
            self.reset_on.bits(),
        );
        fnv1a64(text.as_bytes())
    }

    /// The configuration part of `detector_version` for profile `index`, e.g.
    /// `hk-detect/os-cfar@0.1.0;profile=standard;br=or;pfa=1e-6/1e-3;min=3;gap=2;ref=per-frame;cfg=…`.
    pub fn detector_version(&self, index: usize) -> String {
        let p = self.profile_at(index);
        let off = match p.hysteresis {
            Hysteresis::Pfa(x) => format!("{x:e}"),
            Hysteresis::FixedDb(d) => format!("-{d}dB"),
        };
        let reference = match self.floor_reference {
            FloorReference::PerFrame => "per-frame",
            FloorReference::Wide => "wide",
        };
        let branches = match p.branches {
            Branches::Or => "or",
            Branches::OsOnly => "os",
            Branches::FloorOnly => "floor",
        };
        format!(
            "hk-detect/os-cfar@{};profile={};br={branches};pfa={:e}/{off};min={};gap={};ref={reference};cfg={:016x}",
            env!("CARGO_PKG_VERSION"),
            p.name,
            p.pfa_on,
            p.min_frames,
            p.gap_frames,
            self.settings_hash()
        )
    }

    /// The full `Detection::detector_version` of a segment: the configuration part plus the
    /// spectral context and the floor tracker, e.g. `…;n=10.000;fft=4096/0;win=Hann;floor=…`.
    pub fn detector_version_for(&self, index: usize, run: &RunContext) -> String {
        format!(
            "{};n={:.3};fft={}/{};win={:?};floor={}",
            self.detector_version(index),
            run.n_eff,
            run.fft_len,
            run.overlap,
            run.window,
            self.floor_tag
        )
    }
}

/// FNV-1a, 64 bit.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Invalid detector settings.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigError {
    /// Bad OS-CFAR window.
    Window(CfarWindow),
    /// A probability outside (0, 1).
    Probability {
        /// Setting.
        name: &'static str,
        /// Value.
        value: f64,
    },
    /// Any other invalid value.
    Invalid {
        /// Setting.
        name: &'static str,
        /// Value.
        value: f64,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Window(w) => write!(f, "invalid OS-CFAR window {w:?}"),
            ConfigError::Probability { name, value } => {
                write!(f, "{name} = {value} is not a probability in (0, 1)")
            }
            ConfigError::Invalid { name, value } => write!(f, "invalid {name} = {value}"),
        }
    }
}

impl std::error::Error for ConfigError {}

fn check_probability(name: &'static str, value: f64) -> Result<(), ConfigError> {
    if value > 0.0 && value < 1.0 {
        Ok(())
    } else {
        Err(ConfigError::Probability { name, value })
    }
}

fn check_positive(name: &'static str, value: f64) -> Result<(), ConfigError> {
    if value > 0.0 && value.is_finite() {
        Ok(())
    } else {
        Err(ConfigError::Invalid { name, value })
    }
}

fn check_non_negative(name: &'static str, value: f64) -> Result<(), ConfigError> {
    if value >= 0.0 && value.is_finite() {
        Ok(())
    } else {
        Err(ConfigError::Invalid { name, value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_changes_with_settings_and_names_the_profile() {
        let a = DetectorConfig::new(SurveyId::new());
        let mut b = a.clone();
        b.survey_id = SurveyId::new();
        assert_eq!(
            a.detector_version(0),
            b.detector_version(0),
            "survey id is not a setting"
        );
        b.guard_db = 4.0;
        assert_ne!(a.detector_version(0), b.detector_version(0));
        let v = a.detector_version(0);
        assert!(v.starts_with("hk-detect/os-cfar@"), "{v}");
        assert!(
            v.contains("profile=standard;br=or;pfa=1e-6/1e-3;min=3;gap=2;ref=wide;cfg="),
            "{v}"
        );
        assert!(a.validate().is_ok());
        let run = RunContext {
            n_eff: 10.0,
            fft_len: 4096,
            overlap: 0,
            window: WindowKind::Hann,
        };
        let full = a
            .clone()
            .with_floor_config(&FloorConfig::default())
            .detector_version_for(0, &run);
        assert!(
            full.contains(";n=10.000;fft=4096/0;win=Hann;floor=block-fcme-256/64;h="),
            "{full}"
        );
        let mut c = a.clone();
        c.integration.blocks = 20;
        assert!(
            c.validate().is_ok(),
            "more than 16 integration blocks is valid"
        );
        c.integration.blocks = 5000;
        assert!(c.validate().is_err());
    }

    #[test]
    fn band_profiles_select_by_centre() {
        let mut c = DetectorConfig::new(SurveyId::new());
        c.band_profiles.push(BandProfile {
            freq: FreqRange::new(902e6, 928e6),
            profile: DetectionProfile::short_burst(),
        });
        assert_eq!(c.profile_index(915e6), 1);
        assert_eq!(c.profile_at(1).min_frames, 2);
        assert_eq!(c.profile_index(98e6), 0);
        c.profile.pfa_on = 1.0;
        assert!(c.validate().is_err());
    }
}
