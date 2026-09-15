//! Learned channel plan (T-118, ADR-0012 §2.7): channels learned from blind detections/tracks,
//! snapped to the history level-0 grid; band rasters only as suggestions.
//!
//! **Learning.** Each non-suspect detection's occupied extent (`f_center ± obw/2`) joins the
//! cluster whose median extent holds its centre (or whose median centre its extent holds), else
//! starts one. Clusters keep a bounded sample of centres and OBWs; the channel key is the median
//! extent snapped **outward** to the level-0 grid (`ChannelKey::snap`), so a jittery OBW estimate
//! does not drift the key the way a running union would. Clusters whose median extents come to
//! hold each other's centres merge. A cluster is published as a channel once it has
//! `min_evidence` detections. Any change to the published key set bumps `version`; series keyed
//! by an old extent stay readable (rows carry their key).
//!
//! **Suspects never create or widen a channel** (§2.6): detections flagged `clipped`,
//! `suspect_imd`, `spur_candidate`, a retune-confirmed image, or `compressed` are ignored here.
//!
//! **Rasters are hints.** [`ChannelPlan::suggest_raster`] attaches a `RasterHint` (spacing, the
//! channel centre's offset from the nearest raster point, source) to channels inside a range. It
//! never creates, moves or merges a channel; a non-zero offset is itself interesting.

use std::collections::VecDeque;

use hk_model::attention::occupancy::{Channel, ChannelKey, ChannelSource, RasterHint};
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
            suspect: detection_is_suspect(&d.flags),
        }
    }
}

/// Learning parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LearnConfig {
    /// Detections before a cluster is published as a channel (2).
    pub min_evidence: u64,
    /// Centre/OBW samples kept per cluster (64, newest).
    pub max_samples: usize,
}

impl Default for LearnConfig {
    fn default() -> Self {
        Self {
            min_evidence: 2,
            max_samples: 64,
        }
    }
}

#[derive(Clone, Debug)]
struct Cluster {
    centers: VecDeque<f64>,
    obws: VecDeque<f64>,
    evidence: u64,
    first_learned: Timestamp,
    source: ChannelSource,
    raster_hint: Option<RasterHint>,
}

fn median(v: &VecDeque<f64>) -> f64 {
    let mut s: Vec<f64> = v.iter().copied().collect();
    s.sort_by(f64::total_cmp);
    let m = s.len() / 2;
    if s.len() % 2 == 1 {
        s[m]
    } else {
        0.5 * (s[m - 1] + s[m])
    }
}

impl Cluster {
    fn center(&self) -> f64 {
        median(&self.centers)
    }

    fn obw(&self) -> f64 {
        median(&self.obws)
    }

    fn extent(&self) -> FreqRange {
        FreqRange::centered(self.center(), self.obw())
    }

    fn holds(&self, f_hz: f64, tol_hz: f64) -> bool {
        let e = self.extent();
        f_hz >= e.lo_hz - tol_hz && f_hz <= e.hi_hz + tol_hz
    }

    fn push(&mut self, center: f64, obw: f64, max: usize) {
        self.centers.push_back(center);
        self.obws.push_back(obw);
        while self.centers.len() > max {
            self.centers.pop_front();
            self.obws.pop_front();
        }
        self.evidence += 1;
    }
}

/// A versioned learned channel plan on one history grid.
#[derive(Clone, Debug)]
pub struct ChannelPlan {
    scheme: u16,
    f_cell_hz: f64,
    version: u32,
    cfg: LearnConfig,
    clusters: Vec<Cluster>,
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
        }
    }

    /// Restores a persisted plan: each channel becomes a cluster seeded at its key's centre.
    pub fn from_channels(
        scheme: u16,
        f_cell_hz: f64,
        version: u32,
        channels: &[Channel],
        cfg: LearnConfig,
    ) -> Self {
        let clusters = channels
            .iter()
            .map(|c| {
                let f = c.key.freq(f_cell_hz);
                let obw = if c.obw_hz > 0.0 { c.obw_hz } else { f.width_hz() };
                Cluster {
                    centers: VecDeque::from([0.5 * (f.lo_hz + f.hi_hz)]),
                    obws: VecDeque::from([obw]),
                    evidence: c.evidence.max(cfg.min_evidence),
                    first_learned: c.first_learned,
                    source: c.source,
                    raster_hint: c.raster_hint.clone(),
                }
            })
            .collect();
        Self {
            scheme,
            f_cell_hz,
            version,
            cfg,
            clusters,
        }
    }

    /// Plan version (bumped on every change of the published key set).
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Grid scheme id.
    pub fn scheme(&self) -> u16 {
        self.scheme
    }

    /// Level-0 frequency cell width, Hz.
    pub fn f_cell_hz(&self) -> f64 {
        self.f_cell_hz
    }

    fn channel_of(&self, c: &Cluster) -> Option<Channel> {
        if c.evidence < self.cfg.min_evidence {
            return None;
        }
        let key = ChannelKey::snap(self.scheme, self.f_cell_hz, c.extent())?;
        Some(Channel {
            key,
            source: c.source,
            plan_version: self.version,
            first_learned: c.first_learned,
            evidence: c.evidence,
            obw_hz: c.obw(),
            raster_hint: c.raster_hint.clone(),
        })
    }

    /// Published channels, by key.
    pub fn channels(&self) -> Vec<Channel> {
        let mut v: Vec<Channel> = self
            .clusters
            .iter()
            .filter_map(|c| self.channel_of(c))
            .collect();
        v.sort_by_key(|c| c.key);
        v.dedup_by_key(|c| c.key);
        v
    }

    /// Published channels overlapping `freq`.
    pub fn channels_in(&self, freq: FreqRange) -> Vec<Channel> {
        self.channels()
            .into_iter()
            .filter(|c| c.key.freq(self.f_cell_hz).overlaps(&freq) && freq.width_hz() > 0.0)
            .collect()
    }

    fn keys(&self) -> Vec<ChannelKey> {
        self.channels().into_iter().map(|c| c.key).collect()
    }

    /// Learns from `detections` (suspects ignored). Returns whether the plan version changed.
    pub fn learn(&mut self, detections: &[DetectionExtent]) -> bool {
        let before = self.keys();
        let tol = 0.5 * self.f_cell_hz;
        for d in detections {
            if d.suspect || d.obw_hz <= 0.0 {
                continue;
            }
            let center = 0.5 * (d.freq.lo_hz + d.freq.hi_hz);
            let hit = self.clusters.iter().position(|c| {
                c.holds(center, tol) || (c.center() >= d.freq.lo_hz && c.center() <= d.freq.hi_hz)
            });
            match hit {
                Some(i) => self.clusters[i].push(center, d.obw_hz, self.cfg.max_samples),
                None => {
                    let mut c = Cluster {
                        centers: VecDeque::new(),
                        obws: VecDeque::new(),
                        evidence: 0,
                        first_learned: d.time.start,
                        source: ChannelSource::Learned,
                        raster_hint: None,
                    };
                    c.push(center, d.obw_hz, self.cfg.max_samples);
                    self.clusters.push(c);
                }
            }
            self.merge();
        }
        let changed = self.keys() != before;
        if changed {
            self.version += 1;
        }
        changed
    }

    fn merge(&mut self) {
        let tol = 0.5 * self.f_cell_hz;
        'outer: loop {
            for i in 0..self.clusters.len() {
                for j in i + 1..self.clusters.len() {
                    let (a, b) = (&self.clusters[i], &self.clusters[j]);
                    if a.holds(b.center(), -tol) || b.holds(a.center(), -tol) {
                        let b = self.clusters.remove(j);
                        let a = &mut self.clusters[i];
                        a.centers.extend(b.centers);
                        a.obws.extend(b.obws);
                        while a.centers.len() > self.cfg.max_samples {
                            a.centers.pop_front();
                            a.obws.pop_front();
                        }
                        a.evidence += b.evidence;
                        a.first_learned = a.first_learned.min(b.first_learned);
                        continue 'outer;
                    }
                }
            }
            return;
        }
    }

    /// Attaches a raster suggestion to channels whose centre lies in `range` (never changes keys).
    pub fn suggest_raster(&mut self, range: FreqRange, spacing_hz: f64, origin_hz: f64, source: &str) {
        if !(spacing_hz > 0.0) {
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

    fn det(center: f64, obw: f64, suspect: bool) -> DetectionExtent {
        let t = Timestamp::from_unix_nanos(1_000_000_000);
        DetectionExtent {
            time: TimeRange::new(t, t.saturating_add_nanos(1)),
            freq: FreqRange::centered(center, obw),
            obw_hz: obw,
            suspect,
        }
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
        assert!(f0.lo_hz <= 433_392_500.0 && f0.hi_hz >= 433_407_500.0, "{f0:?}");
        assert!(ch[0].key.hi_cell <= ch[1].key.lo_cell, "channels overlap: {ch:?}");
        assert_eq!(ch[0].evidence, 10);
        // The same evidence again: no new version.
        let v = plan.version();
        assert!(!plan.learn(&dets[..4]));
        assert_eq!(plan.version(), v);
        // One detection is not a channel; suspects never create one.
        let mut p2 = ChannelPlan::new(1, 6250.0, LearnConfig::default());
        assert!(!p2.learn(&[det(1e8, 1e4, false), det(2e8, 1e4, true), det(2e8, 1e4, true)]));
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
        let restored =
            ChannelPlan::from_channels(1, 6250.0, plan.version(), &after, LearnConfig::default());
        assert_eq!(restored.channels()[0].key, after[0].key);
        assert_eq!(restored.version(), plan.version());
    }
}
