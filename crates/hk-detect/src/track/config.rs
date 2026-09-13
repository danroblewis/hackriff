//! [`Tracker`](super::Tracker) settings (C10 "Config": association gates, period search, hop
//! rules, expiry).

/// Periodicity fold settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeriodConfig {
    /// Bursts on the lattice needed before a period is reported (C10 metric: ≥ 10 for < 2 %; 4
    /// reports early with a lower confidence).
    pub min_bursts: usize,
    /// Arrival jitter tolerated around the lattice, as a fraction of the period (0.15).
    pub jitter_fraction: f64,
    /// Minimum `confidence` (inlier fraction × slot coverage) to report a period (0.5).
    pub min_confidence: f64,
    /// Shortest period considered, s (100 µs).
    pub min_period_s: f64,
}

impl Default for PeriodConfig {
    fn default() -> Self {
        Self {
            min_bursts: 4,
            jitter_fraction: 0.15,
            min_confidence: 0.5,
            min_period_s: 1e-4,
        }
    }
}

/// Hop-set settings (docs/04 §4.7: constant BW and dwell, raster frequencies, no temporal overlap
/// between consecutive dwells).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HopConfig {
    /// Look for hop sets.
    pub enabled: bool,
    /// A successor dwell starts within `max(gap_frames · frame, gap_fraction · dwell)` of the
    /// predecessor's end.
    pub gap_frames: f64,
    /// See `gap_frames` (0.25).
    pub gap_fraction: f64,
    /// Bandwidth ratio between consecutive dwells (1.5).
    pub bandwidth_ratio: f64,
    /// Dwell-length ratio between consecutive dwells (1.5).
    pub length_ratio: f64,
    /// Hop links a channel needs to count as a member (2).
    pub min_links_per_channel: u32,
    /// Member channels for a hop set (3).
    pub min_channels: usize,
    /// Hops (links) for a hop set (10).
    pub min_hops: u64,
}

impl Default for HopConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gap_frames: 2.0,
            gap_fraction: 0.25,
            bandwidth_ratio: 1.5,
            length_ratio: 1.5,
            min_links_per_channel: 2,
            min_channels: 3,
            min_hops: 10,
        }
    }
}

/// [`Tracker`](super::Tracker) settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackerConfig {
    /// Centre gate `ε = max(freq_tolerance_bins · bin, freq_tolerance_fraction · BW)` (C10:
    /// max(2 bins, 10 % BW)). A burst also passes when each centre lies inside the other's
    /// occupied extent (as the detector's repeat rule).
    pub freq_tolerance_bins: f64,
    /// See `freq_tolerance_bins`.
    pub freq_tolerance_fraction: f64,
    /// Largest bandwidth ratio for "similar BW" (2; widths floored at `freq_tolerance_bins` bins).
    pub bandwidth_ratio: f64,
    /// Idle timeout, observed seconds (60; ≥ the detector's 10 s repeat window).
    pub idle_timeout_s: f64,
    /// The idle timeout also stretches to this many mean inter-arrival times (4).
    pub idle_timeout_intervals: f64,
    /// Upper bound of the stretched idle timeout, s (3600).
    pub max_idle_timeout_s: f64,
    /// Co-timing tolerance in frames: tone lobes start and end within it; a continuation starts
    /// within it of the split (2).
    pub coincidence_frames: f64,
    /// Tone lobes merge when the frequency gap between their occupied extents is at most this
    /// many times the wider lobe's width (2; a 2-FSK lobe gap is ≈ 2·deviation − lobe width).
    pub lobe_gap_factor: f64,
    /// Records are held this many frames past their end before association, so co-timed lobes
    /// emitted a few frames apart are grouped (8; ≥ coincidence + the detector's gap merge + 1).
    pub hold_frames: u32,
    /// Records shorter than this many frames are dropped unless they continue a split box (2;
    /// T-006 re-probe: split side runs skip the min-duration test and can be 1 frame).
    pub min_part_frames: u64,
    /// A burst closed by a transition continues into the next segment when a box starts at the
    /// segment start within this long, s (1).
    pub max_transition_gap_s: f64,
    /// A burst whose split continuation never arrives is finalised after this long, s (3).
    pub split_wait_s: f64,
    /// Track impulsive (broadband transient) detections (false: skipped and counted).
    pub track_impulsive: bool,
    /// Live-track cap; the stalest track closes when it is reached (4096).
    pub max_live_tracks: usize,
    /// Two live tracks merge when their centres are within `merge_fraction · ε`… (0.5)
    pub merge_fraction: f64,
    /// …and their bandwidth ratio is at most this (1.25).
    pub merge_bandwidth_ratio: f64,
    /// Observation spans closer than this are contiguous, s (2 ms; plus half a frame).
    pub coverage_slack_s: f64,
    /// Seconds of stream time between idle-expiry scans (0.1).
    pub maintain_interval_s: f64,
    /// Periodicity fold.
    pub period: PeriodConfig,
    /// Hop sets.
    pub hop: HopConfig,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            freq_tolerance_bins: 2.0,
            freq_tolerance_fraction: 0.1,
            bandwidth_ratio: 2.0,
            idle_timeout_s: 60.0,
            idle_timeout_intervals: 4.0,
            max_idle_timeout_s: 3600.0,
            coincidence_frames: 2.0,
            lobe_gap_factor: 2.0,
            hold_frames: 8,
            min_part_frames: 2,
            max_transition_gap_s: 1.0,
            split_wait_s: 3.0,
            track_impulsive: false,
            max_live_tracks: 4096,
            merge_fraction: 0.5,
            merge_bandwidth_ratio: 1.25,
            coverage_slack_s: 0.002,
            maintain_interval_s: 0.1,
            period: PeriodConfig::default(),
            hop: HopConfig::default(),
        }
    }
}
