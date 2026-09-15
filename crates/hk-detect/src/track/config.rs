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
    /// A member channel's hop links must number at least this fraction of its bursts (0.5): a
    /// hopper's dwells almost all link, independent emitters whose bursts abut by chance rarely.
    pub min_link_fraction: f64,
    /// Hops (links) for a hop set (10).
    pub min_hops: u64,
    /// A member channel's mean detection SNR (`snr_mean`), dB, must be at least this to count
    /// (8; `f64::NEG_INFINITY` disables). T-084: near-threshold flicker inside weak or dense FM
    /// channels (narrow fragments at 4–7 dB mean SNR on the 8-bit HackRF) linked by chance into a
    /// "hop set" whose 1.1 MHz extent became one wide inventory row hiding the stations. A hopper
    /// is asserted only from channels that clearly stand above the floor; weak ones stay channel
    /// tracks.
    pub min_channel_snr_db: f64,
    /// Bursty hoppers (packets separated by silence, T-031): a finished burst with no contiguous
    /// predecessor links to the nearest preceding similar burst on another channel when the
    /// silence between them is at most this long, s (0.5; 0 disables). The link also needs that
    /// burst's own nearest similar predecessor on a third channel, the three centres on a common
    /// raster, no concurrent similar burst, and neither track periodic.
    pub max_silence_s: f64,
    /// Burst-length ratio for a bursty link (3: packet lengths vary with payload).
    pub bursty_length_ratio: f64,
    /// A track with at least this many bursts on a period lattice is a periodic emitter, not a
    /// channel of a bursty hopper (5)…
    pub periodic_veto_bursts: u64,
    /// …when the fold's confidence is at least this (0.8).
    pub periodic_veto_confidence: f64,
    /// Bounded membership (T-064): a closed member is dropped from its hop set once a newer
    /// qualifying member covers its channel (centres closer than half their summed bandwidths)
    /// and its last burst is more than this many stream seconds old (10; below the 60 s idle
    /// timeout, so a channel's idle-closed track makes way for its successor at once). Its
    /// detections still count towards the set. `f64::INFINITY` keeps every member (before T-064).
    pub member_retention_s: f64,
    /// Members per hop set (256): beyond it the oldest closed members are dropped
    /// (`usize::MAX`: no cap).
    pub max_members: usize,
    /// Raster refits (T-064): an open set's raster is refitted only when its qualifying channel
    /// set changed since the last fit (a member joined, left or (re)qualified, or a channel centre
    /// moved by more than this many bins) (0.75; 0 refits on every drain of a changed set, as
    /// before T-064)…
    pub raster_drift_bins: f64,
    /// …and at most once per this many stream seconds (1; 0 = no limit). Formation, merges and
    /// closing always refit, so `HopSetFormed`/`HopSetClosed` and the closed row are exact; only
    /// the open set's intermediate rows can carry a raster up to this old.
    pub raster_refit_s: f64,
}

impl HopConfig {
    /// These settings with the T-064 bounds off: every member kept and the raster refitted on
    /// every drain of a changed set (the pre-T-064 behaviour, for parity checks).
    pub fn without_scaling_bounds(self) -> Self {
        Self {
            member_retention_s: f64::INFINITY,
            max_members: usize::MAX,
            raster_drift_bins: 0.0,
            raster_refit_s: 0.0,
            ..self
        }
    }
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
            min_link_fraction: 0.5,
            min_hops: 10,
            min_channel_snr_db: 8.0,
            max_silence_s: 0.5,
            bursty_length_ratio: 3.0,
            periodic_veto_bursts: 5,
            periodic_veto_confidence: 0.8,
            member_retention_s: 10.0,
            max_members: 256,
            raster_drift_bins: 0.75,
            raster_refit_s: 1.0,
        }
    }
}

/// Split trigger: a track whose latest bursts form two stable centre (or bandwidth) clusters,
/// both active through the window, splits in two (the minority cluster becomes a new track with
/// `split_from`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitConfig {
    /// Look for splits.
    pub enabled: bool,
    /// Bursts each cluster needs in the 16-burst window (5).
    pub min_per_cluster: usize,
    /// Cluster centres at least this many ε apart (0.6; above the merge rule's 0.5 so a split
    /// never re-merges)…
    pub min_separation_eps: f64,
    /// …and at least this many pooled within-cluster standard deviations (4).
    pub separation_sigma: f64,
    /// Or: cluster mean bandwidths at least this ratio apart (1.6; above the merge rule's 1.25).
    pub bandwidth_ratio: f64,
}

impl Default for SplitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_per_cluster: 5,
            min_separation_eps: 0.6,
            separation_sigma: 4.0,
            bandwidth_ratio: 1.6,
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
    /// A new track is tentative (no `Opened`, not persisted, links held) until it has this many
    /// bursts (2)…
    pub confirm_bursts: u64,
    /// …or this much on-time, s (0.1), or a hop link. A track closing tentative is discarded.
    pub confirm_on_time_s: f64,
    /// Split trigger.
    pub split: SplitConfig,
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
            confirm_bursts: 2,
            confirm_on_time_s: 0.1,
            split: SplitConfig::default(),
            period: PeriodConfig::default(),
            hop: HopConfig::default(),
        }
    }
}
