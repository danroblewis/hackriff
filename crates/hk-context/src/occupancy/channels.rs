//! Learned channel plan (T-118, ADR-0012 §2.7): channels learned from blind detections/tracks,
//! snapped to the history level-0 grid; band rasters only as suggestions.
//!
//! **Learning.** Each non-suspect detection's occupied extent (`f_center ± obw/2`) joins the
//! cluster whose median extent holds its centre (or whose median centre its extent holds), else
//! starts one. Clusters keep a bounded sample of centres, OBWs and SNRs; the channel key is the
//! median extent snapped **outward** to the level-0 grid (`ChannelKey::snap`), so a jittery OBW
//! estimate does not drift the key the way a running union would. Clusters whose median extents
//! come to hold each other's centres merge. A cluster is published as a channel once it has
//! `min_evidence` detections, one of them confident (mean SNR ≥ `min_confident_snr_db`: threshold
//! flicker alone never makes a channel; such places still show in FBO and cell baselines). Any
//! change to the published key set bumps `version`; series keyed by an old extent stay readable
//! (rows carry their key).
//!
//! **In-band fragments (T-129, the T-101 host rule).** A wide emitter (broadcast FM) also yields
//! short narrow detections inside its own band (modulation peaks, threshold flicker). Such a
//! detection — centre inside a published cluster's median extent, that cluster at least
//! `fragment_bw_ratio` × wider and `fragment_snr_margin_db` stronger (median SNR) — is a fragment
//! of the host: it never joins the host's samples (its narrow OBW would drag the median extent down
//! to fragment size) and never starts a channel. A cluster that formed from fragments before its
//! host was published is absorbed (dropped) once the host is.
//!
//! **Neighbours partition.** Two published side-by-side channels whose extents overlap (dense
//! 200 kHz FM, where a 99 % OBW includes the neighbours' skirts) are split at the grid edge nearest
//! the midpoint of their centres, so each channel keys its own emitter.
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

/// Learning parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LearnConfig {
    /// Detections before a cluster is published as a channel (2).
    pub min_evidence: u64,
    /// Centre/OBW samples kept per cluster (64, newest).
    pub max_samples: usize,
    /// A published cluster at least this many times wider than a detection inside its extent
    /// hosts it as an in-band fragment (4, `TrackerConfig::inband_fragment_bw_ratio`).
    pub fragment_bw_ratio: f64,
    /// … when also at least this much stronger, dB (6, the tracker's fragment SNR margin).
    pub fragment_snr_margin_db: f64,
    /// A cluster is published only once one of its detections reached this mean SNR, dB (7).
    pub min_confident_snr_db: f64,
}

impl Default for LearnConfig {
    fn default() -> Self {
        Self {
            min_evidence: 2,
            max_samples: 64,
            fragment_bw_ratio: 4.0,
            fragment_snr_margin_db: 6.0,
            min_confident_snr_db: 7.0,
        }
    }
}

#[derive(Clone, Debug)]
struct Cluster {
    centers: VecDeque<f64>,
    obws: VecDeque<f64>,
    snrs: VecDeque<f64>,
    evidence: u64,
    max_snr_db: f64,
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

    fn snr(&self) -> f64 {
        median(&self.snrs)
    }

    fn extent(&self) -> FreqRange {
        FreqRange::centered(self.center(), self.obw())
    }

    fn holds(&self, f_hz: f64, tol_hz: f64) -> bool {
        let e = self.extent();
        f_hz >= e.lo_hz - tol_hz && f_hz <= e.hi_hz + tol_hz
    }

    fn push(&mut self, center: f64, obw: f64, snr: f64, max: usize) {
        self.centers.push_back(center);
        self.obws.push_back(obw);
        self.snrs.push_back(snr);
        self.trim(max);
        self.evidence += 1;
        self.max_snr_db = self.max_snr_db.max(snr);
    }

    fn trim(&mut self, max: usize) {
        while self.centers.len() > max {
            self.centers.pop_front();
            self.obws.pop_front();
            self.snrs.pop_front();
        }
    }

    fn published(&self, cfg: &LearnConfig) -> bool {
        self.evidence >= cfg.min_evidence && self.max_snr_db >= cfg.min_confident_snr_db
    }

    /// Whether an emission centred at `center` with `obw`/`snr` would be an in-band fragment of
    /// this cluster (publication not checked).
    fn would_host(&self, cfg: &LearnConfig, center: f64, obw: f64, snr: f64) -> bool {
        fragment_of(cfg, self.extent(), self.snr(), center, obw, snr)
    }

    /// Same, for a published cluster: the fragment is dropped.
    fn hosts(&self, cfg: &LearnConfig, center: f64, obw: f64, snr: f64) -> bool {
        self.published(cfg) && self.would_host(cfg, center, obw, snr)
    }

    fn would_host_cluster(&self, cfg: &LearnConfig, other: &Cluster) -> bool {
        self.would_host(cfg, other.center(), other.obw(), other.snr())
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
        && host.width_hz() >= cfg.fragment_bw_ratio * obw
        && center >= host.lo_hz
        && center <= host.hi_hz
        && host_snr >= snr + cfg.fragment_snr_margin_db
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

    /// Restores a persisted plan: each channel becomes a (published) cluster seeded at its key's
    /// centre.
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
                let obw = if c.obw_hz > 0.0 {
                    c.obw_hz
                } else {
                    f.width_hz()
                };
                Cluster {
                    centers: VecDeque::from([0.5 * (f.lo_hz + f.hi_hz)]),
                    obws: VecDeque::from([obw]),
                    // Unknown after a restore: neutral for the host rule until new samples arrive.
                    snrs: VecDeque::from([cfg.min_confident_snr_db]),
                    evidence: c.evidence.max(cfg.min_evidence),
                    max_snr_db: f64::INFINITY,
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

    /// Published channels, by key.
    pub fn channels(&self) -> Vec<Channel> {
        let mut published: Vec<(f64, FreqRange, &Cluster)> = self
            .clusters
            .iter()
            .filter(|c| c.published(&self.cfg))
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
        let mut v: Vec<Channel> = published
            .into_iter()
            .zip(keys)
            .filter_map(|((_, _, c), key)| {
                let key = key.filter(|k| k.hi_cell > k.lo_cell)?;
                Some(Channel {
                    key,
                    source: c.source,
                    plan_version: self.version,
                    first_learned: c.first_learned,
                    evidence: c.evidence,
                    obw_hz: c.obw(),
                    raster_hint: c.raster_hint.clone(),
                })
            })
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

    /// Learns from `detections` (suspects and in-band fragments ignored). Returns whether the plan
    /// version changed.
    pub fn learn(&mut self, detections: &[DetectionExtent]) -> bool {
        let before = self.keys();
        let tol = 0.5 * self.f_cell_hz;
        for d in detections {
            if d.suspect || d.obw_hz <= 0.0 {
                continue;
            }
            let center = 0.5 * (d.freq.lo_hz + d.freq.hi_hz);
            if self
                .clusters
                .iter()
                .any(|c| c.hosts(&self.cfg, center, d.obw_hz, d.snr_db))
            {
                continue;
            }
            // A host never joins (and so widens and pools with) its own fragments' clusters.
            let hit = self.clusters.iter().position(|c| {
                (c.holds(center, tol) || (c.center() >= d.freq.lo_hz && c.center() <= d.freq.hi_hz))
                    && !fragment_of(&self.cfg, d.freq, d.snr_db, c.center(), c.obw(), c.snr())
            });
            let i = match hit {
                Some(i) => i,
                None => {
                    self.clusters.push(Cluster {
                        centers: VecDeque::new(),
                        obws: VecDeque::new(),
                        snrs: VecDeque::new(),
                        evidence: 0,
                        max_snr_db: f64::NEG_INFINITY,
                        first_learned: d.time.start,
                        source: ChannelSource::Learned,
                        raster_hint: None,
                    });
                    self.clusters.len() - 1
                }
            };
            let was_published = self.clusters[i].published(&self.cfg);
            self.clusters[i].push(center, d.obw_hz, d.snr_db, self.cfg.max_samples);
            if !was_published && self.clusters[i].published(&self.cfg) {
                self.absorb_fragments(i);
            }
            self.merge();
        }
        let changed = self.keys() != before;
        if changed {
            self.version += 1;
        }
        changed
    }

    /// Drops the clusters that newly published cluster `host` hosts as in-band fragments.
    fn absorb_fragments(&mut self, host: usize) {
        let h = self.clusters[host].clone();
        let cfg = self.cfg;
        let mut k = 0;
        self.clusters.retain(|c| {
            let keep = k == host || !h.hosts(&cfg, c.center(), c.obw(), c.snr());
            k += 1;
            keep
        });
    }

    fn merge(&mut self) {
        let tol = 0.5 * self.f_cell_hz;
        'outer: loop {
            for i in 0..self.clusters.len() {
                for j in i + 1..self.clusters.len() {
                    let (a, b) = (&self.clusters[i], &self.clusters[j]);
                    if a.holds(b.center(), -tol) || b.holds(a.center(), -tol) {
                        // A host and its fragment cluster never pool: the fragment is dropped
                        // once the host is published, and kept apart until then.
                        let cfg = &self.cfg;
                        let frag = if a.would_host_cluster(cfg, b) {
                            Some((a.published(cfg), j))
                        } else if b.would_host_cluster(cfg, a) {
                            Some((b.published(cfg), i))
                        } else {
                            None
                        };
                        match frag {
                            Some((true, f)) => {
                                self.clusters.remove(f);
                                continue 'outer;
                            }
                            Some((false, _)) => continue,
                            None => {}
                        }
                        let b = self.clusters.remove(j);
                        let a = &mut self.clusters[i];
                        a.centers.extend(b.centers);
                        a.obws.extend(b.obws);
                        a.snrs.extend(b.snrs);
                        a.trim(self.cfg.max_samples);
                        a.evidence += b.evidence;
                        a.max_snr_db = a.max_snr_db.max(b.max_snr_db);
                        a.first_learned = a.first_learned.min(b.first_learned);
                        continue 'outer;
                    }
                }
            }
            return;
        }
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
        let restored =
            ChannelPlan::from_channels(1, 6250.0, plan.version(), &after, LearnConfig::default());
        assert_eq!(restored.channels()[0].key, after[0].key);
        assert_eq!(restored.version(), plan.version());
    }

    /// T-129: a wide station with narrow in-band flicker (before and after the host is
    /// published) learns one channel of the station's OBW; threshold flicker alone in a gap makes
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
        // Flicker in an idle gap: repeated, but never confident.
        for _ in 0..40 {
            dets.push(det_snr(100_350_000.0, 9.4e3, 4.5, false));
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
}
