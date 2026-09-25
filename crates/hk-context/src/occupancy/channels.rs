//! Learned channel plan (T-118, ADR-0012 §2.7): channels learned from blind detections/tracks,
//! snapped to the history level-0 grid; band rasters only as suggestions.
//!
//! **Learning.** Each non-suspect detection's occupied extent (`f_center ± obw/2`) joins the
//! cluster whose median extent holds its centre (or whose median centre its extent holds), else
//! starts one, provided the two OBWs are within `fragment_bw_ratio` of each other (nested
//! emitters of very different width stay apart). Clusters keep a bounded window of the newest samples (centre, OBW, SNR, time); the
//! channel key is the median extent snapped **outward** to the level-0 grid (`ChannelKey::snap`),
//! so a jittery OBW estimate does not drift the key the way a running union would. Clusters whose
//! median extents come to hold each other's centres merge. Any change to the published key set
//! bumps `version`; series keyed by an old extent stay readable (rows carry their key).
//!
//! **Publication (T-129): confident or persistent.** A cluster with `min_evidence` detections is
//! published when it is
//! - **confident:** the median SNR of the non-fragment detections in its (bounded, newest-first)
//!   window is ≥ `min_confident_snr_db`, so one strong flicker never holds a channel; or
//! - **persistent:** detections at a stable centre (within ± max(1 level-0 cell, 0.1 × OBW) / 2 of
//!   the median, and at least half the window there) recur in ≥ `persist_intervals` separated
//!   intervals with cumulative detected duration ≥ `persist_min_detected_ns`, so a steady weak
//!   unknown carrier gets a channel. Short, scattered threshold flicker is neither; such places
//!   still show in FBO and cell baselines.
//!
//! **In-band fragments (the T-101 host rule, with time).** A wide emitter (broadcast FM) also
//! yields short narrow detections inside its own band. A detection centred inside a published
//! cluster's median extent, that cluster ≥ `fragment_bw_ratio` × wider and
//! `fragment_snr_margin_db` stronger and **detected at the same time** (± `fragment_time_slack_ns`),
//! is a fragment unless its own cluster is persistent. A detection never joins a cluster that
//! would host it (its narrow OBW would drag the host's median extent down); fragments only count
//! as persistence evidence in their own cluster, never towards confidence, so they never seed,
//! join or narrow a channel. When a host gains evidence, earlier samples of other clusters it
//! hosts are re-flagged as fragments, except in clusters already published or persistent: fragment
//! absorption never removes a published channel.
//!
//! **Restore.** The plan persists each channel's [`ChannelEvidence`] (median centre and SNR,
//! interval and duration evidence), so a restarted host keeps hosting its fragments and keeps its
//! width and `first_learned`. Plans saved without evidence restore as confident.
//!
//! **Neighbours partition.** Two published side-by-side channels whose extents overlap (dense
//! 200 kHz FM, where a 99 % OBW includes the neighbours' skirts) are split at the grid edge nearest
//! the midpoint of their centres, so each channel keys its own emitter.
//!
//! **Suspects never create or widen a channel** (§2.6): detections flagged `clipped`,
//! `suspect_imd`, `spur_candidate`, a retune-confirmed image, or `compressed` are ignored here.
//! A DC spur flag is per tuning (T-147, T-172): [`extents_of`] clears it, per detection, when a
//! clean detection of the same emission, from a tuning whose own LO is not at it, lies within one
//! time cell.
//!
//! **Rasters are hints.** [`ChannelPlan::suggest_raster`] attaches a `RasterHint` (spacing, the
//! channel centre's offset from the nearest raster point, source) to channels inside a range. It
//! never creates, moves or merges a channel; a non-zero offset is itself interesting.

use std::collections::VecDeque;

use hk_model::attention::occupancy::{
    Channel, ChannelEvidence, ChannelKey, ChannelSource, RasterHint,
};
use hk_model::detection::SpurReason;
use hk_model::{Detection, DetectionFlags, FreqRange, TimeRange, Timestamp};

/// The §2.6 suspect rule for a detection's flags.
pub fn detection_is_suspect(f: &DetectionFlags) -> bool {
    f.clipped || f.suspect_imd || f.spur_candidate || f.image_retune_confirmed || f.compressed
}

/// What the occupancy engine needs of a detection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetectionExtent {
    /// When it was seen.
    pub time: TimeRange,
    /// Occupied extent.
    pub freq: FreqRange,
    /// Occupied bandwidth, Hz.
    pub obw_hz: f64,
    /// Mean SNR, dB (0 when unknown).
    pub snr_db: f64,
    /// §2.6 suspect.
    pub suspect: bool,
}

impl DetectionExtent {
    /// From a stored detection.
    pub fn of(d: &Detection) -> Self {
        let obw = if d.obw_hz.is_finite() && d.obw_hz > 0.0 {
            d.obw_hz
        } else {
            0.0
        };
        Self {
            time: d.time,
            freq: FreqRange::centered(d.f_center_hz, obw),
            obw_hz: obw,
            snr_db: if d.snr_mean_db.is_finite() {
                d.snr_mean_db
            } else {
                0.0
            },
            suspect: detection_is_suspect(&d.flags),
        }
    }
}

impl DetectionExtent {
    /// From a T-904 rollup of pruned detections: its time hull and frequency envelope, the mean
    /// OBW and mean SNR, and suspect only when every member carried a common §2.6 suspect flag
    /// (`flags_all`) — a rollup cannot say more than that, and the DC-twin refutation, which needs
    /// each member's own tuning, is not applied to it.
    pub fn of_rollup(r: &hk_model::DetectionRollup) -> Self {
        let obw = if r.obw_mean_hz.is_finite() && r.obw_mean_hz > 0.0 {
            r.obw_mean_hz
        } else {
            0.0
        };
        Self {
            time: r.time,
            freq: r.freq,
            obw_hz: obw,
            snr_db: if r.snr_mean_db.is_finite() {
                r.snr_mean_db
            } else {
                0.0
            },
            suspect: detection_is_suspect(&r.flags_all),
        }
    }
}

/// A detection is suspect only for its DC (tuned-centre) spur flag: `spur_reason = Dc` with no
/// other §2.6 suspect flag.
pub fn dc_only_suspect(f: &DetectionFlags) -> bool {
    f.spur_candidate
        && matches!(f.spur_reason, Some(SpurReason::Dc))
        && !(f.clipped || f.suspect_imd || f.image_retune_confirmed || f.compressed)
}

/// The detector's DC-spur tolerance, Hz (`hk_detect::DcRule::default().tolerance_hz`): a DC twin's
/// own tuning centre must lie further than this outside its extent (T-172).
pub const DC_TWIN_LO_TOLERANCE_HZ: f64 = 15e3;

/// Parameters of the §2.6 DC-twin rule (T-147, T-172).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DcTwinRule {
    /// Level-0 frequency cell, Hz.
    pub f_cell_hz: f64,
    /// Time slack, ns: one level-0 time cell, the grid's `t_cell_ns` (the engine's visit-window
    /// slack), passed in so the two cannot diverge.
    pub slack_ns: i64,
    /// A twin's own tuning centre must lie more than this outside its extent, Hz
    /// ([`DC_TWIN_LO_TOLERANCE_HZ`]).
    pub lo_tolerance_hz: f64,
}

/// §2.6 extents of stored detections with DC flags checked across tunings
/// ([`refute_dc_suspects`]). `tuning_centre` gives a detection's own tuning centre, Hz (its
/// Provenance `tune.center_hz`), or `None` when unknown; it is asked only of candidate twins.
pub fn extents_of(
    detections: &[Detection],
    rule: DcTwinRule,
    mut tuning_centre: impl FnMut(&Detection) -> Option<f64>,
) -> Vec<DetectionExtent> {
    let mut out: Vec<DetectionExtent> = detections.iter().map(DetectionExtent::of).collect();
    let dc: Vec<bool> = detections
        .iter()
        .map(|d| dc_only_suspect(&d.flags))
        .collect();
    refute_dc_suspects(&mut out, &dc, rule, |j| tuning_centre(&detections[j]));
    out
}

/// T-147 (§2.6): DC-ness belongs to a tuning, not to a frequency. A DC flag (`dc_only[i]`) is a
/// hypothesis about the capture whose LO sat there; it is refuted, and `extents[i]` made
/// non-suspect, when a **clean twin** exists: a clean (non-suspect, so not itself DC-flagged)
/// detection of the same emission (centres within half a level-0 cell plus half the narrower OBW
/// of each other), overlapping it within ± `rule.slack_ns`, whose own tuning centre
/// (`own_lo(j)`) lies more than `rule.lo_tolerance_hz` outside its extent (T-172: an unflagged
/// image or intermod sitting at its own LO is not a twin; an unknown tuning refutes nothing). A
/// true DC spur moves with the tuning, has no such twin and stays suspect; a real carrier 2 cells
/// away does not refute it. The rule is per detection: other DC flags at the frequency with no
/// twin in their own window stay suspect.
///
/// **Cost (T-172).** Clean detections are indexed by (level-0 cell of their centre, start time)
/// with a per-cell running maximum of end times, so each DC flag visits only the few cells its
/// reach spans and, within each, binary-searches its ± slack window: O((n + m) log n) for n clean
/// and m DC-flagged detections, not O(n × m).
pub fn refute_dc_suspects(
    extents: &mut [DetectionExtent],
    dc_only: &[bool],
    rule: DcTwinRule,
    mut own_lo: impl FnMut(usize) -> Option<f64>,
) {
    struct Clean {
        cell: i64,
        start: i64,
        end: i64,
        /// Running maximum of `end` over this cell's entries up to here (non-decreasing).
        max_end: i64,
        idx: usize,
    }
    if !dc_only.iter().any(|&b| b) {
        return;
    }
    let centre = |e: &DetectionExtent| 0.5 * (e.freq.lo_hz + e.freq.hi_hz);
    let f_cell = if rule.f_cell_hz.is_finite() && rule.f_cell_hz > 0.0 {
        rule.f_cell_hz
    } else {
        0.0
    };
    let cell_of = |f: f64| {
        if f_cell > 0.0 {
            (f / f_cell).floor() as i64
        } else {
            0
        }
    };
    let slack = rule.slack_ns.max(0);
    let tol = rule.lo_tolerance_hz.max(0.0);
    let mut clean: Vec<Clean> = extents
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.suspect && centre(e).is_finite())
        .map(|(i, e)| Clean {
            cell: cell_of(centre(e)),
            start: e.time.start.as_unix_nanos(),
            end: e.time.end.as_unix_nanos(),
            max_end: i64::MIN,
            idx: i,
        })
        .collect();
    if clean.is_empty() {
        return;
    }
    clean.sort_unstable_by_key(|c| (c.cell, c.start, c.idx));
    let mut run = i64::MIN;
    for k in 0..clean.len() {
        if k > 0 && clean[k].cell != clean[k - 1].cell {
            run = i64::MIN;
        }
        run = run.max(clean[k].end);
        clean[k].max_end = run;
    }
    let half = 0.5 * f_cell;
    let mut refuted = Vec::new();
    for (i, e) in extents.iter().enumerate() {
        if !(e.suspect && dc_only.get(i).copied().unwrap_or(false)) {
            continue;
        }
        let c = centre(e);
        if !c.is_finite() {
            continue;
        }
        let (s, t) = (e.time.start.as_unix_nanos(), e.time.end.as_unix_nanos());
        let reach = half + 0.5 * e.obw_hz;
        let (cell_lo, cell_hi) = (cell_of(c - reach), cell_of(c + reach));
        let mut k = clean.partition_point(|x| x.cell < cell_lo);
        let mut twin = false;
        while !twin && k < clean.len() && clean[k].cell <= cell_hi {
            let cell = clean[k].cell;
            let seg_end = k + clean[k..].partition_point(|x| x.cell == cell);
            let seg = &clean[k..seg_end];
            // Entries before `from` all end (running max included) before `s - slack`; entries
            // from `to` on all start after `t + slack`.
            let from = seg.partition_point(|x| x.max_end.saturating_add(slack) < s);
            let to = seg.partition_point(|x| x.start <= t.saturating_add(slack));
            if from < to {
                twin = seg[from..to].iter().any(|x| {
                    let w = &extents[x.idx];
                    (centre(w) - c).abs() <= half + 0.5 * w.obw_hz.min(e.obw_hz)
                        && x.end.saturating_add(slack) >= s
                        && own_lo(x.idx)
                            .is_some_and(|lo| lo < w.freq.lo_hz - tol || lo > w.freq.hi_hz + tol)
                });
            }
            k = seg_end;
        }
        if twin {
            refuted.push(i);
        }
    }
    for i in refuted {
        extents[i].suspect = false;
    }
}

/// Learning parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LearnConfig {
    /// Detections before a cluster can be published as a channel (2).
    pub min_evidence: u64,
    /// Samples kept per cluster (64, newest).
    pub max_samples: usize,
    /// A published cluster at least this many times wider than a detection inside its extent
    /// hosts it as an in-band fragment (4, `TrackerConfig::inband_fragment_bw_ratio`).
    pub fragment_bw_ratio: f64,
    /// … when also at least this much stronger, dB (6, the tracker's fragment SNR margin).
    pub fragment_snr_margin_db: f64,
    /// … and detected within this long of one of the host's detections, ns (1 s).
    pub fragment_time_slack_ns: i64,
    /// Confident: median non-fragment SNR over the sample window at least this, dB (7).
    pub min_confident_snr_db: f64,
    /// Persistent: detections at a stable centre in at least this many separated intervals (3).
    pub persist_intervals: u32,
    /// Interval length for `persist_intervals`, ns (15 min).
    pub persist_interval_ns: i64,
    /// Persistent: cumulative detected duration at the stable centre at least this, ns (0.5 s).
    pub persist_min_detected_ns: i64,
    /// T-558: most clusters the plan holds. Beyond it the weakest are forgotten — unpublished
    /// first, then by least evidence and oldest last seen.
    pub max_clusters: usize,
    /// T-558: an **unpublished** cluster unseen for this long is forgotten, ns. Defaults to the
    /// whole persistence window (`persist_intervals × persist_interval_ns`), so a cluster is only
    /// dropped once it has been quiet for longer than it would have taken to earn a channel.
    pub forget_unpublished_ns: i64,
}

impl Default for LearnConfig {
    fn default() -> Self {
        Self {
            min_evidence: 2,
            max_samples: 64,
            fragment_bw_ratio: 4.0,
            fragment_snr_margin_db: 6.0,
            fragment_time_slack_ns: 1_000_000_000,
            min_confident_snr_db: 7.0,
            persist_intervals: 3,
            persist_interval_ns: 900_000_000_000,
            persist_min_detected_ns: 500_000_000,
            max_clusters: 4096,
            forget_unpublished_ns: 3 * 900_000_000_000,
        }
    }
}

/// Newest distinct intervals remembered per cluster (for de-duplicating the interval count).
const RECENT_INTERVALS: usize = 8;

#[derive(Clone, Copy, Debug)]
struct Sample {
    center: f64,
    obw: f64,
    snr: f64,
    start_ns: i64,
    end_ns: i64,
    /// An in-band fragment of a host: persistence evidence only.
    fragment: bool,
}

impl Sample {
    fn overlaps(&self, start_ns: i64, end_ns: i64, slack_ns: i64) -> bool {
        self.start_ns <= end_ns.saturating_add(slack_ns)
            && self.end_ns.saturating_add(slack_ns) >= start_ns
    }
}

#[derive(Clone, Debug)]
struct Cluster {
    samples: VecDeque<Sample>,
    /// Cached `median(center)`, `median(obw)` and the non-fragment median SNR of `samples`
    /// (T-558). They were recomputed — each allocating and sorting a Vec — on every one of the
    /// `O(n²)` comparisons `merge` makes, which is what made a survey's channel plan the busiest
    /// thing in the process. [`Cluster::recompute`] is called from every site that mutates
    /// `samples` or a sample's `fragment` flag.
    med_center: f64,
    med_obw: f64,
    med_snr: f64,
    /// Newest sample end, ns: when this cluster was last seen (`i64::MIN` for a restored seed).
    last_ns: i64,
    evidence: u64,
    /// Non-fragment detections.
    clean: u64,
    intervals: u32,
    recent_intervals: VecDeque<i64>,
    detected_ns: i64,
    /// Published at the end of the last `learn` (or restored).
    visible: bool,
    first_learned: Timestamp,
    source: ChannelSource,
    raster_hint: Option<RasterHint>,
}

fn median(mut s: Vec<f64>) -> f64 {
    if s.is_empty() {
        return f64::NAN;
    }
    s.sort_by(f64::total_cmp);
    let m = s.len() / 2;
    if s.len() % 2 == 1 {
        s[m]
    } else {
        0.5 * (s[m - 1] + s[m])
    }
}

impl Cluster {
    fn new(first_learned: Timestamp) -> Self {
        Self {
            samples: VecDeque::new(),
            med_center: f64::NAN,
            med_obw: f64::NAN,
            med_snr: f64::NEG_INFINITY,
            last_ns: i64::MIN,
            evidence: 0,
            clean: 0,
            intervals: 0,
            recent_intervals: VecDeque::new(),
            detected_ns: 0,
            visible: false,
            first_learned,
            source: ChannelSource::Learned,
            raster_hint: None,
        }
    }

    /// Recomputes the cached medians. Every mutation of `samples`, or of a sample's `fragment`
    /// flag, ends in a call to this (T-558).
    fn recompute(&mut self) {
        self.med_center = median(self.samples.iter().map(|s| s.center).collect());
        self.med_obw = median(self.samples.iter().map(|s| s.obw).collect());
        let v: Vec<f64> = self
            .samples
            .iter()
            .filter(|s| !s.fragment)
            .map(|s| s.snr)
            .collect();
        self.med_snr = if v.is_empty() {
            f64::NEG_INFINITY
        } else {
            median(v)
        };
        self.last_ns = self
            .samples
            .iter()
            .map(|s| s.end_ns)
            .max()
            .unwrap_or(i64::MIN);
    }

    fn center(&self) -> f64 {
        self.med_center
    }

    fn obw(&self) -> f64 {
        self.med_obw
    }

    /// Median SNR of the window's non-fragment samples; −∞ when there are none.
    fn snr(&self) -> f64 {
        self.med_snr
    }

    fn extent(&self) -> FreqRange {
        FreqRange::centered(self.center(), self.obw())
    }

    fn holds(&self, f_hz: f64, tol_hz: f64) -> bool {
        let e = self.extent();
        f_hz >= e.lo_hz - tol_hz && f_hz <= e.hi_hz + tol_hz
    }

    /// Half the allowed centre spread: max(1 level-0 cell, 0.1 × OBW) / 2.
    fn stable_tol(&self, f_cell_hz: f64) -> f64 {
        0.5 * f_cell_hz.max(0.1 * self.obw())
    }

    fn note_interval(&mut self, k: i64) -> bool {
        if self.recent_intervals.contains(&k) {
            return false;
        }
        self.recent_intervals.push_back(k);
        if self.recent_intervals.len() > RECENT_INTERVALS {
            self.recent_intervals.pop_front();
        }
        true
    }

    fn push(&mut self, s: Sample, cfg: &LearnConfig, f_cell_hz: f64) {
        let stable = self.samples.is_empty()
            || (s.center - self.center()).abs() <= self.stable_tol(f_cell_hz);
        if stable {
            self.detected_ns = self
                .detected_ns
                .saturating_add((s.end_ns - s.start_ns).max(0));
            if cfg.persist_interval_ns > 0
                && self.note_interval(s.start_ns.div_euclid(cfg.persist_interval_ns))
            {
                self.intervals += 1;
            }
        }
        self.samples.push_back(s);
        self.trim(cfg.max_samples);
        self.evidence += 1;
        self.clean += u64::from(!s.fragment);
        self.recompute();
    }

    fn trim(&mut self, max: usize) {
        while self.samples.len() > max.max(1) {
            self.samples.pop_front();
        }
    }

    fn confident(&self, cfg: &LearnConfig) -> bool {
        self.clean >= cfg.min_evidence && self.snr() >= cfg.min_confident_snr_db
    }

    fn persistent(&self, cfg: &LearnConfig, f_cell_hz: f64) -> bool {
        if self.evidence < cfg.min_evidence
            || self.intervals < cfg.persist_intervals
            || self.detected_ns < cfg.persist_min_detected_ns
        {
            return false;
        }
        let (c, tol) = (self.center(), self.stable_tol(f_cell_hz));
        let stable = self
            .samples
            .iter()
            .filter(|s| (s.center - c).abs() <= tol)
            .count();
        2 * stable >= self.samples.len()
    }

    fn published(&self, cfg: &LearnConfig, f_cell_hz: f64) -> bool {
        self.evidence >= cfg.min_evidence
            && (self.confident(cfg) || self.persistent(cfg, f_cell_hz))
    }

    /// Whether an emission centred at `center` with `obw`/`snr` would be an in-band fragment of
    /// this cluster by extent and level (publication and time not checked).
    fn would_host(&self, cfg: &LearnConfig, center: f64, obw: f64, snr: f64) -> bool {
        fragment_of(cfg, self.extent(), self.snr(), center, obw, snr)
    }

    fn would_host_cluster(&self, cfg: &LearnConfig, other: &Cluster) -> bool {
        self.would_host(cfg, other.center(), other.obw(), other.snr())
    }

    /// A non-fragment sample of this cluster overlaps `[start_ns, end_ns]` ± `slack_ns`.
    fn overlaps_in_time(&self, start_ns: i64, end_ns: i64, slack_ns: i64) -> bool {
        self.samples
            .iter()
            .any(|s| !s.fragment && s.overlaps(start_ns, end_ns, slack_ns))
    }
}

/// The T-101 host rule on extents: `host` at least `fragment_bw_ratio` × wider, holding the
/// fragment's centre, and `fragment_snr_margin_db` stronger.
fn fragment_of(
    cfg: &LearnConfig,
    host: FreqRange,
    host_snr: f64,
    center: f64,
    obw: f64,
    snr: f64,
) -> bool {
    cfg.fragment_bw_ratio > 0.0
        && host_snr.is_finite()
        && host.width_hz() >= cfg.fragment_bw_ratio * obw
        && center >= host.lo_hz
        && center <= host.hi_hz
        && host_snr >= snr + cfg.fragment_snr_margin_db
}

/// Two OBWs are the same emitter's estimates only within `fragment_bw_ratio` of each other: a
/// wide detection never joins (and widens) a narrow cluster whose centre it holds, nor the reverse.
fn comparable_widths(cfg: &LearnConfig, a: f64, b: f64) -> bool {
    cfg.fragment_bw_ratio <= 0.0 || (a < cfg.fragment_bw_ratio * b && b < cfg.fragment_bw_ratio * a)
}

/// A versioned learned channel plan on one history grid.
#[derive(Clone, Debug)]
pub struct ChannelPlan {
    scheme: u16,
    f_cell_hz: f64,
    version: u32,
    cfg: LearnConfig,
    clusters: Vec<Cluster>,
    /// Cluster-pair comparisons [`Self::merge_at`] has made since the plan was built (T-558).
    ///
    /// The work the survey's cost is made of, counted rather than timed. The defect this bounds
    /// was a pairwise merge over every pair after every detection; a wall-clock budget only
    /// notices that once the machine is slow enough or `n` large enough, whereas the count
    /// separates `O(clusters)` a detection from `O(clusters²)` on the first call and on any
    /// hardware. One `u64` add per comparison, which is far less than the comparison itself.
    comparisons: u64,
}

impl ChannelPlan {
    /// An empty plan (version 0) on the grid `scheme`/`f_cell_hz`.
    pub fn new(scheme: u16, f_cell_hz: f64, cfg: LearnConfig) -> Self {
        Self {
            scheme,
            f_cell_hz,
            version: 0,
            cfg,
            clusters: Vec::new(),
            comparisons: 0,
        }
    }

    /// Restores a persisted plan: each channel becomes a published cluster seeded from its
    /// [`ChannelEvidence`] (matched by key); a channel without evidence is seeded confident.
    pub fn from_channels(
        scheme: u16,
        f_cell_hz: f64,
        version: u32,
        channels: &[Channel],
        evidence: &[ChannelEvidence],
        cfg: LearnConfig,
    ) -> Self {
        let clusters = channels
            .iter()
            .map(|c| {
                let f = c.key.freq(f_cell_hz);
                let ev = evidence.iter().find(|e| e.key == c.key);
                let obw = if c.obw_hz > 0.0 {
                    c.obw_hz
                } else {
                    f.width_hz()
                };
                let center = ev
                    .map(|e| e.center_hz)
                    .filter(|x| x.is_finite())
                    .unwrap_or(0.5 * (f.lo_hz + f.hi_hz));
                // Without evidence: confident, and strong enough to host threshold fragments.
                let (snr, fragment) = match ev {
                    Some(e) => (e.snr_db.unwrap_or(f64::NEG_INFINITY), e.snr_db.is_none()),
                    None => (cfg.min_confident_snr_db + cfg.fragment_snr_margin_db, false),
                };
                let evidence = c.evidence.max(cfg.min_evidence);
                // A few seeded samples give the restored median some inertia.
                let n = evidence.min((cfg.max_samples / 4).max(1) as u64) as usize;
                let seed = Sample {
                    center,
                    obw,
                    snr,
                    start_ns: i64::MIN,
                    end_ns: i64::MIN,
                    fragment,
                };
                let mut cluster = Cluster {
                    samples: std::iter::repeat_n(seed, n).collect(),
                    med_center: f64::NAN,
                    med_obw: f64::NAN,
                    med_snr: f64::NEG_INFINITY,
                    last_ns: i64::MIN,
                    evidence,
                    clean: ev.map_or(evidence, |e| e.clean),
                    intervals: ev.map_or(0, |e| e.intervals),
                    recent_intervals: ev
                        .map(|e| e.recent_intervals.iter().copied().collect())
                        .unwrap_or_default(),
                    detected_ns: ev.map_or(0, |e| e.detected_ns),
                    visible: true,
                    first_learned: c.first_learned,
                    source: c.source,
                    raster_hint: c.raster_hint.clone(),
                };
                cluster.recompute();
                cluster
            })
            .collect();
        Self {
            scheme,
            f_cell_hz,
            version,
            cfg,
            clusters,
            comparisons: 0,
        }
    }

    /// Plan version (bumped on every change of the published key set).
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Clusters held (published and not): the plan's residency, for tests and measurement.
    pub fn cluster_count(&self) -> usize {
        self.clusters.len()
    }

    /// Cluster-pair comparisons made since the plan was built ([`Self::comparisons`] on the
    /// struct says why this is counted): the survey's learning cost, as work rather than seconds.
    pub fn comparisons(&self) -> u64 {
        self.comparisons
    }

    /// Grid scheme id.
    pub fn scheme(&self) -> u16 {
        self.scheme
    }

    /// Level-0 frequency cell width, Hz.
    pub fn f_cell_hz(&self) -> f64 {
        self.f_cell_hz
    }

    /// Published clusters with their (neighbour-partitioned) keys, by key.
    fn published(&self) -> Vec<(ChannelKey, &Cluster)> {
        let mut published: Vec<(f64, FreqRange, &Cluster)> = self
            .clusters
            .iter()
            .filter(|c| c.published(&self.cfg, self.f_cell_hz))
            .map(|c| (c.center(), c.extent(), c))
            .collect();
        published.sort_by(|a, b| a.0.total_cmp(&b.0));
        let cell = self.f_cell_hz;
        let mut keys: Vec<Option<ChannelKey>> = published
            .iter()
            .map(|(_, e, _)| ChannelKey::snap(self.scheme, cell, *e))
            .collect();
        // Side-by-side neighbours whose keys overlap split at the grid edge nearest the midpoint
        // of their centres; the extent is about the centre (OBW), so that bound applies to both
        // sides (nested extents are left alone).
        for i in 1..published.len() {
            let (c0, e0, _) = published[i - 1];
            let (c1, e1, _) = published[i];
            let (Some(k0), Some(k1)) = (keys[i - 1], keys[i]) else {
                continue;
            };
            if !(k0.hi_cell > k1.lo_cell && e0.lo_hz < e1.lo_hz && e0.hi_hz < e1.hi_hz && c0 < c1) {
                continue;
            }
            let m = (0.5 * (c0 + c1) / cell).round() as i64;
            if m <= k0.lo_cell || m >= k1.hi_cell {
                continue;
            }
            let k0 = keys[i - 1].as_mut().unwrap();
            k0.hi_cell = k0.hi_cell.min(m);
            k0.lo_cell = k0.lo_cell.max((2.0 * c0 / cell - m as f64).floor() as i64);
            let k1 = keys[i].as_mut().unwrap();
            k1.lo_cell = k1.lo_cell.max(m);
            k1.hi_cell = k1.hi_cell.min((2.0 * c1 / cell - m as f64).ceil() as i64);
        }
        let mut v: Vec<(ChannelKey, &Cluster)> = published
            .into_iter()
            .zip(keys)
            .filter_map(|((_, _, c), key)| Some((key.filter(|k| k.hi_cell > k.lo_cell)?, c)))
            .collect();
        v.sort_by_key(|(k, _)| *k);
        v.dedup_by_key(|(k, _)| *k);
        v
    }

    /// Published channels, by key.
    pub fn channels(&self) -> Vec<Channel> {
        self.published()
            .into_iter()
            .map(|(key, c)| Channel {
                key,
                source: c.source,
                plan_version: self.version,
                first_learned: c.first_learned,
                evidence: c.evidence,
                obw_hz: c.obw(),
                raster_hint: c.raster_hint.clone(),
            })
            .collect()
    }

    /// The learning evidence of the published channels, by key (persisted with the plan).
    pub fn evidence(&self) -> Vec<ChannelEvidence> {
        self.published()
            .into_iter()
            .map(|(key, c)| ChannelEvidence {
                key,
                center_hz: c.center(),
                snr_db: Some(c.snr()).filter(|x| x.is_finite()),
                clean: c.clean,
                intervals: c.intervals,
                recent_intervals: c.recent_intervals.iter().copied().collect(),
                detected_ns: c.detected_ns,
            })
            .collect()
    }

    /// Published channels overlapping `freq`.
    pub fn channels_in(&self, freq: FreqRange) -> Vec<Channel> {
        self.channels()
            .into_iter()
            .filter(|c| c.key.freq(self.f_cell_hz).overlaps(&freq) && freq.width_hz() > 0.0)
            .collect()
    }

    fn keys(&self) -> Vec<ChannelKey> {
        self.published().into_iter().map(|(k, _)| k).collect()
    }

    /// Learns from `detections` (suspects ignored; in-band fragments count only as persistence
    /// evidence). Returns whether the plan version changed.
    pub fn learn(&mut self, detections: &[DetectionExtent]) -> bool {
        let before = self.keys();
        let (cfg, cell) = (self.cfg, self.f_cell_hz);
        let tol = 0.5 * cell;
        let mut now_ns = self
            .clusters
            .iter()
            .map(|c| c.last_ns)
            .max()
            .unwrap_or(i64::MIN);
        for d in detections {
            if d.suspect || d.obw_hz <= 0.0 {
                continue;
            }
            let center = 0.5 * (d.freq.lo_hz + d.freq.hi_hz);
            let (start_ns, end_ns) = (d.time.start.as_unix_nanos(), d.time.end.as_unix_nanos());
            // Its own cluster: never one that would host it (a fragment never narrows its host)
            // nor one it would host (a host never pools with its fragments' clusters).
            let hit = self.clusters.iter().position(|c| {
                (c.holds(center, tol) || (c.center() >= d.freq.lo_hz && c.center() <= d.freq.hi_hz))
                    && comparable_widths(&cfg, c.obw(), d.obw_hz)
                    && !c.would_host(&cfg, center, d.obw_hz, d.snr_db)
                    && !fragment_of(&cfg, d.freq, d.snr_db, c.center(), c.obw(), c.snr())
            });
            let persistent = hit.is_some_and(|i| self.clusters[i].persistent(&cfg, cell));
            let fragment = !persistent
                && self.clusters.iter().any(|c| {
                    c.published(&cfg, cell)
                        && c.would_host(&cfg, center, d.obw_hz, d.snr_db)
                        && c.overlaps_in_time(start_ns, end_ns, cfg.fragment_time_slack_ns)
                });
            let i = hit.unwrap_or_else(|| {
                self.clusters.push(Cluster::new(d.time.start));
                self.clusters.len() - 1
            });
            self.clusters[i].push(
                Sample {
                    center,
                    obw: d.obw_hz,
                    snr: d.snr_db,
                    start_ns,
                    end_ns,
                    fragment,
                },
                &cfg,
                cell,
            );
            let i = self.merge_at(i);
            if !fragment && self.clusters[i].published(&cfg, cell) {
                self.flag_fragments(i);
            }
            now_ns = now_ns.max(end_ns);
        }
        self.forget(now_ns);
        for c in &mut self.clusters {
            c.visible = c.published(&cfg, cell);
        }
        let changed = self.keys() != before;
        if changed {
            self.version += 1;
        }
        changed
    }

    /// Re-flags as fragments the samples of other clusters that published cluster `host` hosts
    /// and overlaps in time (detections that arrived before their host). Clusters already
    /// published or persistent are left alone: absorption never removes a channel.
    fn flag_fragments(&mut self, host: usize) {
        let (cfg, cell) = (self.cfg, self.f_cell_hz);
        let h = &self.clusters[host];
        let (he, hs) = (h.extent(), h.snr());
        if !hs.is_finite() {
            return;
        }
        let times: Vec<(i64, i64)> = h
            .samples
            .iter()
            .filter(|s| !s.fragment)
            .map(|s| (s.start_ns, s.end_ns))
            .collect();
        let hosted = |s: &Sample| {
            !s.fragment
                && fragment_of(&cfg, he, hs, s.center, s.obw, s.snr)
                && times
                    .iter()
                    .any(|&(a, b)| s.overlaps(a, b, cfg.fragment_time_slack_ns))
        };
        for (k, c) in self.clusters.iter_mut().enumerate() {
            if k == host || c.visible || !c.samples.iter().any(hosted) || c.persistent(&cfg, cell) {
                continue;
            }
            let mut touched = false;
            for s in c.samples.iter_mut() {
                if hosted(s) {
                    s.fragment = true;
                    c.clean = c.clean.saturating_sub(1);
                    touched = true;
                }
            }
            if touched {
                c.recompute();
            }
        }
    }

    /// Merges cluster `at` into, or with, any other it has come to overlap, and returns where
    /// the survivor ended up.
    ///
    /// **Why only `at` (T-558).** The old pass compared every pair after every detection, so one
    /// `learn` call cost `O(detections × clusters²)`, each comparison recomputing four medians.
    /// Measured over a 1 MHz–6 GHz sweep it reached **1.46 s per sweep step by step 236 of 2999**
    /// and rose quadratically from there — a survey that cannot finish. Nothing but the cluster
    /// that just took a sample can have *become* mergeable, though: every other pair was compared
    /// when one of them last changed and neither has moved since. Following the survivor (a merge
    /// moves its median, which can open another) keeps the closure the old loop's `continue
    /// 'outer` gave, at `O(clusters)` a detection.
    fn merge_at(&mut self, at: usize) -> usize {
        let tol = 0.5 * self.f_cell_hz;
        let cfg = self.cfg;
        let mut i = at;
        loop {
            let counted = &mut self.comparisons;
            let clusters = &self.clusters;
            let Some(j) = (0..clusters.len()).find(|&j| {
                if j == i {
                    return false;
                }
                *counted += 1;
                let (a, b) = (&clusters[i], &clusters[j]);
                (a.holds(b.center(), -tol) || b.holds(a.center(), -tol))
                    // Nested emitters of very different width, and a host and its fragments'
                    // cluster, never pool.
                    && comparable_widths(&cfg, a.obw(), b.obw())
                    && !a.would_host_cluster(&cfg, b)
                    && !b.would_host_cluster(&cfg, a)
            }) else {
                return i;
            };
            let b = self.clusters.remove(j);
            if j < i {
                i -= 1;
            }
            let a = &mut self.clusters[i];
            let shared = b
                .recent_intervals
                .iter()
                .filter(|k| a.recent_intervals.contains(k))
                .count() as u32;
            a.intervals += b.intervals.saturating_sub(shared);
            for k in b.recent_intervals {
                a.note_interval(k);
            }
            a.samples.extend(b.samples);
            a.trim(cfg.max_samples);
            a.evidence += b.evidence;
            a.clean += b.clean;
            a.detected_ns = a.detected_ns.saturating_add(b.detected_ns);
            a.visible |= b.visible;
            a.first_learned = a.first_learned.min(b.first_learned);
            if a.raster_hint.is_none() {
                a.raster_hint = b.raster_hint;
            }
            a.recompute();
        }
    }

    /// Forgets clusters until the plan is within [`LearnConfig::max_clusters`], and forgets any
    /// **unpublished** cluster unseen for [`LearnConfig::forget_unpublished_ns`] (T-558).
    ///
    /// A device-wide survey meets a great many emitters, and before this the plan kept a cluster
    /// for every one of them for ever — at step 236 of a 1 MHz–6 GHz sweep, 1896 clusters of
    /// which **none** had published, because most were one detection that never recurred. That is
    /// the unbounded seen-count accumulator the inventory model rejects: a candidate is a
    /// hypothesis about a region, and when the region goes quiet it expires.
    ///
    /// Order of forgetting: unpublished before published, then least evidence, then oldest last
    /// seen. A published channel is real measured structure, so it goes last and only to keep the
    /// hard bound honest — the bound has to hold whatever the survey meets, or it is not a bound.
    fn forget(&mut self, now_ns: i64) {
        let (cfg, cell) = (self.cfg, self.f_cell_hz);
        if cfg.forget_unpublished_ns > 0 {
            let cutoff = now_ns.saturating_sub(cfg.forget_unpublished_ns);
            self.clusters
                .retain(|c| c.published(&cfg, cell) || c.last_ns > cutoff);
        }
        if cfg.max_clusters == 0 || self.clusters.len() <= cfg.max_clusters {
            return;
        }
        let mut order: Vec<usize> = (0..self.clusters.len()).collect();
        order.sort_by_key(|&i| {
            let c = &self.clusters[i];
            (c.published(&cfg, cell), c.evidence, c.last_ns)
        });
        let drop: std::collections::HashSet<usize> = order
            .into_iter()
            .take(self.clusters.len() - cfg.max_clusters)
            .collect();
        let mut i = 0;
        self.clusters.retain(|_| {
            let keep = !drop.contains(&i);
            i += 1;
            keep
        });
    }

    /// Attaches a raster suggestion to channels whose centre lies in `range` (never changes keys).
    pub fn suggest_raster(
        &mut self,
        range: FreqRange,
        spacing_hz: f64,
        origin_hz: f64,
        source: &str,
    ) {
        if spacing_hz.is_nan() || spacing_hz <= 0.0 {
            return;
        }
        for c in &mut self.clusters {
            let f = c.center();
            if f >= range.lo_hz && f <= range.hi_hz {
                c.raster_hint = Some(raster_hint(f, spacing_hz, origin_hz, source));
            }
        }
    }
}

/// A raster suggestion for a channel centred at `center_hz`: offset from the nearest point of
/// `origin_hz + k·spacing_hz`.
pub fn raster_hint(center_hz: f64, spacing_hz: f64, origin_hz: f64, source: &str) -> RasterHint {
    let k = ((center_hz - origin_hz) / spacing_hz).round();
    RasterHint {
        spacing_hz,
        offset_hz: center_hz - (origin_hz + k * spacing_hz),
        source: source.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-904: a rollup of pruned detections reads as one extent over its hull and envelope, and
    /// is suspect only when a suspect flag was common to every member.
    #[test]
    fn a_rollup_is_one_extent_suspect_only_if_every_member_was() {
        let t = |s: i64| Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + s * 1_000_000_000);
        let clipped = DetectionFlags {
            clipped: true,
            ..DetectionFlags::default()
        };
        let mut r = hk_model::DetectionRollup {
            id: 1,
            track_id: None,
            survey_id: hk_model::SurveyId::new(),
            provenance_ref: hk_model::ProvenanceId::new(),
            time: TimeRange::new(t(0), t(60)),
            on_air_ns: 30_000_000_000,
            freq: FreqRange::new(433.90e6, 433.94e6),
            f_center_mean_hz: 433.92e6,
            obw_mean_hz: 12e3,
            obw_max_hz: 40e3,
            snr_peak_max_db: 30.0,
            snr_mean_db: 14.0,
            peak_level_dbfs_max: -20.0,
            detections: 600,
            flags_any: clipped,
            flags_all: DetectionFlags::default(),
            clip_count: 3,
        };
        let e = DetectionExtent::of_rollup(&r);
        assert_eq!(e.time, r.time);
        assert_eq!(e.freq, r.freq);
        assert_eq!((e.obw_hz, e.snr_db), (12e3, 14.0));
        assert!(
            !e.suspect,
            "one clipped member does not make the run suspect"
        );
        r.flags_all = clipped;
        assert!(DetectionExtent::of_rollup(&r).suspect);
    }

    fn det_snr(center: f64, obw: f64, snr_db: f64, suspect: bool) -> DetectionExtent {
        let t = Timestamp::from_unix_nanos(1_000_000_000);
        DetectionExtent {
            time: TimeRange::new(t, t.saturating_add_nanos(1)),
            freq: FreqRange::centered(center, obw),
            obw_hz: obw,
            snr_db,
            suspect,
        }
    }

    fn det(center: f64, obw: f64, suspect: bool) -> DetectionExtent {
        det_snr(center, obw, 20.0, suspect)
    }

    /// A non-suspect detection starting `start_s` for `dur_s`.
    fn det_at(center: f64, obw: f64, snr_db: f64, start_s: f64, dur_s: f64) -> DetectionExtent {
        let t = Timestamp::from_unix_nanos((start_s * 1e9) as i64);
        DetectionExtent {
            time: TimeRange::new(t, t.saturating_add_nanos((dur_s * 1e9) as i64)),
            freq: FreqRange::centered(center, obw),
            obw_hz: obw,
            snr_db,
            suspect: false,
        }
    }

    /// T-172: a multi-day hop pattern, one visit every 10 s. Tuning A (LO 433.4125 MHz) DC-flags a
    /// real 433.4 MHz carrier that tuning B (LO 434 MHz) sees clean 55 ms later; tuning A also
    /// carries a true LO leak at 433.5 MHz with no twin; 20 clean carriers elsewhere. Returns the
    /// extents, DC-only flags and each detection's own LO.
    fn dc_twin_scenario(hours: u64) -> (Vec<DetectionExtent>, Vec<bool>, Vec<f64>) {
        let (mut ext, mut dc, mut lo) = (Vec::new(), Vec::new(), Vec::new());
        let at = |c: f64, from: f64, to: f64, suspect: bool| {
            let mut e = det_at(c, 3000.0, 20.0, from, to - from);
            e.suspect = suspect;
            e
        };
        for k in 0..hours * 360 {
            let t = 1.0e6 + 10.0 * k as f64;
            ext.push(at(433_400_000.0, t + 0.01, t + 0.05, true));
            dc.push(true);
            lo.push(433_412_500.0);
            ext.push(at(433_500_000.0, t + 0.01, t + 0.05, true));
            dc.push(true);
            lo.push(433_500_000.0);
            ext.push(at(433_400_000.0, t + 0.065, t + 0.1, false));
            dc.push(false);
            lo.push(434_000_000.0);
            for m in 0..20 {
                ext.push(at(
                    433_600_000.0 + 25e3 * f64::from(m),
                    t + 0.065,
                    t + 0.1,
                    false,
                ));
                dc.push(false);
                lo.push(434_000_000.0);
            }
        }
        (ext, dc, lo)
    }

    const TWIN_RULE: DcTwinRule = DcTwinRule {
        f_cell_hz: 6250.0,
        slack_ns: 1_000_000_000,
        lo_tolerance_hz: DC_TWIN_LO_TOLERANCE_HZ,
    };

    /// T-172: the twin lookup is time-indexed. 18 h → 72 h (4× the detections) must cost about
    /// 4 × log; the T-147 centre-only scan was quadratic here (ratio 15.25: 0.039 s → 0.597 s).
    #[test]
    fn dc_twin_lookup_scales_n_log_n_over_days() {
        let run = |hours: u64| {
            let (ext, dc, lo) = dc_twin_scenario(hours);
            let mut best = f64::INFINITY;
            let mut out = Vec::new();
            for _ in 0..3 {
                let mut e = ext.clone();
                let t0 = std::time::Instant::now();
                refute_dc_suspects(&mut e, &dc, TWIN_RULE, |j| Some(lo[j]));
                best = best.min(t0.elapsed().as_secs_f64());
                out = e;
            }
            (ext.len(), best, out)
        };
        let (n1, t1, _) = run(18);
        let (n2, t2, out) = run(72);
        eprintln!(
            "dc twin lookup: n={n1} {t1:.4}s, n={n2} {t2:.4}s, ratio {:.2}",
            t2 / t1
        );
        // The carrier's DC flags are all refuted; the twinless LO leak stays suspect.
        assert!(out.iter().step_by(23).all(|e| !e.suspect));
        assert!(out.iter().skip(1).step_by(23).all(|e| e.suspect));
        assert!(t2 / t1 < 8.0, "ratio {:.2}: not O(n log n)", t2 / t1);
    }

    /// T-172: a twin must be off its own LO, lie within the rule's slack, and refutes only the DC
    /// flags in its own window.
    #[test]
    fn dc_twin_must_be_off_its_own_lo_and_within_one_time_cell() {
        let f = 433_400_000.0;
        let flagged = |start_s: f64| {
            let mut e = det_at(f, 2000.0, 20.0, start_s, 0.04);
            e.suspect = true;
            e
        };
        let refute = |ext: &[DetectionExtent], lo: &[Option<f64>], rule: DcTwinRule| {
            let mut e = ext.to_vec();
            let dc: Vec<bool> = ext.iter().map(|x| x.suspect).collect();
            refute_dc_suspects(&mut e, &dc, rule, |j| lo[j]);
            e.iter().map(|x| x.suspect).collect::<Vec<_>>()
        };
        // Tuning A's LO leak at f; tuning B's unflagged artefact at the same RF frequency, B's LO
        // 10 kHz away (inside the DC tolerance): not a twin, A's flag stands.
        let a = flagged(100.0);
        let artefact = det_at(f, 2000.0, 20.0, 100.05, 0.04);
        let ext = [a, artefact];
        assert_eq!(
            refute(&ext, &[Some(f), Some(f + 10e3)], TWIN_RULE),
            [true, false]
        );
        // Unknown tuning: refutes nothing.
        assert_eq!(refute(&ext, &[Some(f), None], TWIN_RULE), [true, false]);
        // A genuine carrier seen off-DC from tuning C (LO 600 kHz away) clears it.
        assert_eq!(
            refute(&ext, &[Some(f), Some(f + 600e3)], TWIN_RULE),
            [false, false]
        );
        // 1.5 s apart: outside a 1 s time cell, inside a 2 s one.
        let late = det_at(f, 2000.0, 20.0, 101.59, 0.04);
        let ext = [a, late];
        let lo = [Some(f), Some(f + 600e3)];
        assert_eq!(refute(&ext, &lo, TWIN_RULE), [true, false]);
        let two_s = DcTwinRule {
            slack_ns: 2_000_000_000,
            ..TWIN_RULE
        };
        assert_eq!(refute(&ext, &lo, two_s), [false, false]);
        // Per detection: the twin clears the flag beside it, not one an hour later at the same
        // frequency; a long twin overlapping a later flag still reaches it (running max end).
        let hour = flagged(3700.0);
        let long = det_at(f, 2000.0, 20.0, 50.0, 3600.0);
        let short = det_at(f, 2000.0, 20.0, 100.05, 0.04);
        let ext = [a, hour, short];
        let lo = [Some(f), Some(f), Some(f + 600e3)];
        assert_eq!(refute(&ext, &lo, TWIN_RULE), [false, true, false]);
        let ext = [long, short, hour];
        let lo = [Some(f + 600e3), Some(f + 600e3), Some(f)];
        assert_eq!(refute(&ext, &lo, TWIN_RULE), [false, false, true]);
        let far = flagged(3649.5);
        let ext = [short, long, far];
        let lo = [Some(f + 600e3), Some(f + 600e3), Some(f)];
        assert_eq!(refute(&ext, &lo, TWIN_RULE), [false, false, false]);
    }

    /// Deterministic uniform draw in [0, 1).
    fn lcg(x: &mut u64) -> f64 {
        *x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (*x >> 11) as f64 / (1u64 << 53) as f64
    }

    #[test]
    fn occupancy_channels_learned_blind_snap_outward_and_keep_neighbours_apart() {
        let mut plan = ChannelPlan::new(1, 6250.0, LearnConfig::default());
        // Two 15 kHz channels 25 kHz apart, OBW estimates jittering ±3 kHz, plus a suspect IMD.
        let mut dets = Vec::new();
        for k in 0..10 {
            let j = f64::from(k % 3 - 1) * 3000.0;
            dets.push(det(433_400_000.0 + j * 0.1, 15_000.0 + j, false));
            dets.push(det(433_425_000.0 - j * 0.1, 15_000.0 - j, false));
        }
        dets.push(det(433_450_000.0, 15_000.0, true));
        assert!(plan.learn(&dets));
        let ch = plan.channels();
        assert_eq!(ch.len(), 2, "{ch:?}");
        let f0 = ch[0].key.freq(6250.0);
        assert!(
            f0.lo_hz <= 433_392_500.0 && f0.hi_hz >= 433_407_500.0,
            "{f0:?}"
        );
        assert!(
            ch[0].key.hi_cell <= ch[1].key.lo_cell,
            "channels overlap: {ch:?}"
        );
        assert_eq!(ch[0].evidence, 10);
        // The same evidence again: no new version.
        let v = plan.version();
        assert!(!plan.learn(&dets[..4]));
        assert_eq!(plan.version(), v);
        // One detection is not a channel; suspects never create one.
        let mut p2 = ChannelPlan::new(1, 6250.0, LearnConfig::default());
        assert!(!p2.learn(&[
            det(1e8, 1e4, false),
            det(2e8, 1e4, true),
            det(2e8, 1e4, true)
        ]));
        assert!(p2.channels().is_empty());
    }

    #[test]
    fn occupancy_channels_restore_and_raster_hints_do_not_move_keys() {
        let mut plan = ChannelPlan::new(1, 6250.0, LearnConfig::default());
        plan.learn(&[det(100_010_000.0, 12_000.0, false); 3]);
        let before = plan.channels();
        plan.suggest_raster(FreqRange::new(99e6, 101e6), 12_500.0, 0.0, "band-table");
        let after = plan.channels();
        assert_eq!(before[0].key, after[0].key);
        let h = after[0].raster_hint.as_ref().unwrap();
        assert!((h.offset_hz - 10_000.0).abs() < 1e-6 || (h.offset_hz + 2_500.0).abs() < 1e-6);
        let restored = ChannelPlan::from_channels(
            1,
            6250.0,
            plan.version(),
            &after,
            &plan.evidence(),
            LearnConfig::default(),
        );
        assert_eq!(restored.channels()[0].key, after[0].key);
        assert_eq!(restored.version(), plan.version());
    }

    /// T-129: a wide station with narrow in-band flicker (before and after the host is
    /// published) learns one channel of the station's OBW; short scattered flicker in a gap makes
    /// no channel; overlapping neighbours partition at the midpoint.
    #[test]
    fn occupancy_channel_learning_merges_inband_fragments_into_one_station_channel() {
        let cell = 6250.0;
        let st = 101_300_000.0;
        let mut dets = Vec::new();
        // Flicker fragments inside the station first (before any host exists).
        for k in 0..12 {
            dets.push(det_snr(st - 80e3 + f64::from(k) * 15e3, 14e3, 4.5, false));
        }
        for k in 0..5 {
            dets.push(det_snr(st + f64::from(k) * 300.0, 200e3, 17.0, false));
            for j in 0..6 {
                dets.push(det_snr(st - 60e3 + f64::from(j) * 20e3, 9.4e3, 4.6, false));
            }
        }
        // Short random flicker in an idle gap over two days: scattered centres, durations,
        // levels and times; never confident, never persistent.
        let mut x = 0x5EED_u64;
        for _ in 0..120 {
            let c = 100_300_000.0 + 100e3 * lcg(&mut x);
            let start = 3600.0 + 48.0 * 3600.0 * lcg(&mut x);
            let dur = 0.005 + 0.045 * lcg(&mut x);
            dets.push(det_at(c, 9.4e3, 3.5 + 2.5 * lcg(&mut x), start, dur));
        }
        // A weak narrow real carrier elsewhere (confident): its own channel.
        for _ in 0..5 {
            dets.push(det_snr(100_440_000.0, 37.5e3, 10.0, false));
        }
        let mut plan = ChannelPlan::new(1, cell, LearnConfig::default());
        assert!(plan.learn(&dets));
        let ch = plan.channels();
        assert_eq!(ch.len(), 2, "{ch:?}");
        let f = ch[1].key.freq(cell);
        assert!(f.lo_hz <= st - 95e3 && f.hi_hz >= st + 95e3, "{f:?}");
        assert!(f.width_hz() <= 220e3, "{f:?}");
        // More fragments later never narrow it.
        let v = plan.version();
        let late: Vec<_> = (0..50)
            .map(|k| det_snr(st - 50e3 + f64::from(k % 10) * 10e3, 9.4e3, 5.0, false))
            .collect();
        assert!(!plan.learn(&late));
        assert_eq!(plan.version(), v);

        // Dense FM: three stations 200 kHz apart whose OBW includes the neighbours' skirts.
        let mut dense = Vec::new();
        for k in 0..4 {
            for s in [-200e3, 0.0, 200e3] {
                dense.push(det_snr(st + s + f64::from(k) * 100.0, 300e3, 18.0, false));
            }
        }
        let mut plan = ChannelPlan::new(1, cell, LearnConfig::default());
        plan.learn(&dense);
        let ch = plan.channels();
        assert_eq!(ch.len(), 3, "{ch:?}");
        for (c, s) in ch.iter().zip([-200e3, 0.0, 200e3]) {
            let f = c.key.freq(cell);
            assert!(f.lo_hz < st + s && f.hi_hz > st + s, "{f:?}");
            assert!(f.width_hz() <= 215e3, "{f:?}");
        }
        assert!(ch.windows(2).all(|w| w[0].key.hi_cell <= w[1].key.lo_cell));
    }

    /// T-129 review: a steady 5 dB narrow carrier in a gap is persistent and publishes; a
    /// persistent narrow emitter 6 dB under a station, inside its skirt, gets its channel; a
    /// channel published before its host appears is never absorbed.
    #[test]
    fn occupancy_channel_learning_publishes_persistent_weak_and_skirt_emitters() {
        let (cell, cfg) = (6250.0, LearnConfig::default());
        let gap = 100_350_000.0;
        let steady = |k: u32| -> Vec<DetectionExtent> {
            (0..3)
                .map(|j| {
                    let t = 3600.0 + 900.0 * f64::from(k) + 2.0 * f64::from(j);
                    det_at(gap + f64::from(j - 1) * 300.0, 9.4e3, 5.0, t, 1.0)
                })
                .collect()
        };
        let mut plan = ChannelPlan::new(1, cell, cfg);
        assert!(!plan.learn(&steady(0)), "one visit is not persistent");
        assert!(!plan.learn(&steady(1)));
        assert!(plan.learn(&steady(2)));
        let ch = plan.channels();
        assert_eq!(ch.len(), 1, "{ch:?}");
        let f = ch[0].key.freq(cell);
        assert!(
            f.lo_hz <= gap - 4e3 && f.hi_hz >= gap + 4e3 && f.width_hz() <= 3.0 * cell,
            "{f:?}"
        );

        let st = 101_300_000.0;
        let mut plan = ChannelPlan::new(1, cell, cfg);
        // A narrow 12 dB carrier at −120 kHz, published on its own.
        assert!(plan.learn(&[
            det_at(st - 120e3, 9.4e3, 12.0, 3605.0, 1.0),
            det_at(st - 120e3, 9.4e3, 12.0, 3625.0, 1.0),
        ]));
        // Then a 16 dB station (344 kHz 99 % OBW) over the same time, with a 10 dB narrow
        // emitter at +100 kHz inside its skirt on every visit.
        let visit = |k: u32| {
            let t0 = 3600.0 + 900.0 * f64::from(k);
            let mut v = vec![
                det_at(st, 344e3, 16.0, t0, 60.0),
                det_at(st + 500.0, 344e3, 16.5, t0 + 60.0, 60.0),
            ];
            for j in 0..4 {
                v.push(det_at(
                    st + 100e3,
                    9.4e3,
                    10.0,
                    t0 + 5.0 + 20.0 * f64::from(j),
                    1.0,
                ));
            }
            v
        };
        let narrow = |ch: &[Channel], f_hz: f64| {
            ch.iter().any(|c| {
                let f = c.key.freq(cell);
                f.lo_hz < f_hz && f.hi_hz > f_hz && f.width_hz() <= 3.0 * cell
            })
        };
        plan.learn(&visit(0));
        let ch = plan.channels();
        assert_eq!(
            ch.len(),
            2,
            "one visit: station + the earlier carrier: {ch:?}"
        );
        assert!(narrow(&ch, st - 120e3), "{ch:?}");
        assert!(!narrow(&ch, st + 100e3), "{ch:?}");
        for k in 1..4 {
            plan.learn(&visit(k));
        }
        let ch = plan.channels();
        assert_eq!(ch.len(), 3, "{ch:?}");
        assert!(narrow(&ch, st - 120e3) && narrow(&ch, st + 100e3), "{ch:?}");
        let wide = ch
            .iter()
            .map(|c| c.key.freq(cell))
            .find(|f| f.width_hz() > 300e3);
        assert!(
            wide.is_some_and(|f| f.lo_hz <= st - 170e3 && f.hi_hz >= st + 170e3),
            "{ch:?}"
        );
    }

    /// T-129 review: restoring the plan mid-run (with its persisted evidence, or from an older
    /// plan without it) keeps the station's width and `first_learned` while fragments arrive,
    /// including fragments that come before the station's next detection.
    #[test]
    fn occupancy_channel_plan_restart_mid_run_keeps_station_width_and_first_learned() {
        let (cell, cfg, st) = (6250.0, LearnConfig::default(), 101_300_000.0);
        let station = |t: f64| det_at(st, 344e3, 17.0, t, 10.0);
        let frags = |t: f64| {
            (0..6).map(move |j| {
                det_at(
                    st - 60e3 + 20e3 * f64::from(j),
                    9.4e3,
                    4.5,
                    t + 1.0 + f64::from(j),
                    0.02,
                )
            })
        };
        let mut plan = ChannelPlan::new(1, cell, cfg);
        let mut first = vec![station(100.0), station(110.0)];
        first.extend(frags(100.0));
        assert!(plan.learn(&first));
        let before = plan.channels();
        assert_eq!(before.len(), 1, "{before:?}");
        for evidence in [plan.evidence(), Vec::new()] {
            let mut r =
                ChannelPlan::from_channels(1, cell, plan.version(), &before, &evidence, cfg);
            let mut later: Vec<_> = frags(120.0).chain(frags(121.0)).collect();
            later.push(station(120.0));
            later.extend(frags(130.0));
            later.push(station(130.0));
            later.extend(frags(131.0));
            assert!(
                !r.learn(&later),
                "evidence {}: {:?}",
                evidence.len(),
                r.channels()
            );
            let after = r.channels();
            assert_eq!(after.len(), 1, "{after:?}");
            assert_eq!(after[0].key, before[0].key);
            assert_eq!(after[0].first_learned, before[0].first_learned);
            assert_eq!(r.version(), plan.version());
        }
    }
}
