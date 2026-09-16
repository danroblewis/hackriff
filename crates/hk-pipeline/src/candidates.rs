//! Candidate evidence per track (T-128, ADR-0012 §4.1–§4.5): what the C12 scorer needs about each
//! blind track, gathered at event rate on the control thread (never the sample path) and turned
//! into [`CandidateInput`]s at scoring time.
//!
//! Per track, from its member detections: extent, SNR (EWMA of the members' mean SNR), the share
//! of members with a suspect flag (IMD, spur, image, clipped, compressed), burst starts
//! (periodicity = 1 − CV of the inter-burst intervals over ≥ [`PERIODICITY_MIN_BURSTS`] bursts;
//! expected interval = their mean), whether a chain recipe matches the measured extent/burstiness
//! (`decoder_available`), whether a trust test already classified it, and a short history of its
//! novelty at revisits (the boring prior's `stable`).
//!
//! At scoring time an [`EvidenceSource`] supplies the rest: the baseline novelty and mature-pool
//! FCOs of the learned channel under the track (level/occupancy novelty, `always_on`), the site's
//! new-emitter novelty (applied to tracks first seen inside the first-sighting window), decodes
//! per track (`well_characterised`) and the inventory's class entropy (`None` = never classified =
//! maximally uncertain). The band-plan allocation part of the boring prior stays 0 here (C17
//! suggestions are not joined to tracks yet), so the prior is measured evidence only.
//!
//! # Re-checking a candidate whose signal stopped (T-251, ADR-0017 TM-6)
//!
//! A closed track is **kept**, not dropped. Dropping it on close is what left stopped candidates
//! never re-verified: the moment a signal stopped, its candidate left the C12 set, the bandit lost
//! its arm, and the scheduler never went back to that frequency on its account — so nothing ever
//! established whether the signal had really gone. A candidate whose signal stopped is precisely
//! the one worth re-checking.
//!
//! So [`CandidateTable::on_closed`] records `closed_ns` instead of removing the entry, and it
//! stays a scorer input — an arm the bandit can spend a dwell on — while its
//! [`TrackEvidence::confidence`] decays: `1` until the silence exceeds the idle gap (below that
//! the receiver has observed no absence at all), then one `1/e` per further gap
//! (`hk_model::presence`). At [`hk_model::recheck_horizon_s`] — where that confidence reaches the
//! scheduler's own `MIN_DWELL_SHARE`, about four gaps — [`CandidateTable::prune_stale`] retires
//! it, because a dwell spent there is one taken from a candidate with live evidence.
//!
//! Two things this is **not**. It is not a decrementing timer: nothing is incremented or
//! decremented, and confidence is recomputed from `closed_ns` and the clock on every read, so
//! evidence arriving on the track clears the silence outright ([`CandidateTable::on_member`]).
//! And retiring an entry here removes **only** a scheduler working-set row — never an emitter,
//! a presence interval or anything in History, none of which this module can reach. A signal
//! returning after the horizon arrives as a new track and enters the table fresh, while the
//! inventory keeps it on the *same* emitter (T-262).
//!
//! The table is bounded ([`MAX_TRACKS`]; a retired-but-not-yet-pruned track is evicted first,
//! then the least recently seen).

use std::collections::{HashMap, VecDeque};

use hk_context::occupancy::novelty::FirstSightingRate;
use hk_context::occupancy::score::{
    BoringEvidence, CandidateInput, always_on, stable, well_characterised,
};
use hk_model::attention::baseline::{BaselineResolution, Maturity};
use hk_model::attention::score::{CandidateSubject, NoveltyScore};
use hk_model::{
    FreqRange, IdleGap, Timestamp, TrackId, confidence_after_silence, recheck_horizon_s,
};

/// Most tracks kept.
pub const MAX_TRACKS: usize = 4096;
/// Burst starts kept per track.
pub const MAX_BURST_STARTS: usize = 32;
/// Novelty samples kept per track (the `stable` test needs 10).
pub const MAX_NOVELTY_HISTORY: usize = 16;
/// Bursts needed before periodicity is estimated.
pub const PERIODICITY_MIN_BURSTS: usize = 4;
/// EWMA weight of a new member's SNR.
const SNR_ALPHA: f64 = 0.2;

/// One member detection as candidate evidence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MemberEvidence {
    /// Lower edge, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz.
    pub f_hi_hz: f64,
    /// Start (sample clock), ns.
    pub t_ns: i64,
    /// Emitted at the detector's max duration and still continuing (a steady carrier).
    pub continues: bool,
    /// Mean SNR, dB.
    pub snr_db: Option<f64>,
    /// A suspect flag (IMD, spur, image, clipped, compressed).
    pub suspect: bool,
}

/// What the table knows about one track.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackEvidence {
    /// Lower edge, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz.
    pub f_hi_hz: f64,
    /// First member, ns.
    pub first_ns: i64,
    /// Latest member, ns.
    pub last_ns: i64,
    /// Members seen.
    pub members: u32,
    /// Of those, with a suspect flag.
    pub suspect_members: u32,
    /// EWMA of the members' SNR, dB.
    pub snr_db: Option<f64>,
    /// Bursts (members that are not continuations).
    pub bursts: u32,
    /// Recent burst starts, ns.
    pub burst_starts: VecDeque<i64>,
    /// The detector confirmed it (only confirmed tracks are candidates).
    pub confirmed: bool,
    /// A chain recipe matches its measured extent and burstiness.
    pub recipe_match: bool,
    /// A trust test classified it (no further verification requested).
    pub trust_tested: bool,
    /// Novelty at recent revisits.
    pub novelty_history: VecDeque<f64>,
    /// Latest scored novelty.
    pub last_novelty: f64,
    /// When the track closed (sample clock), ns; `None` while evidence is still arriving. A closed
    /// track is kept as a **re-check request** until its confidence decays past the horizon (module
    /// docs), never dropped on close.
    pub closed_ns: Option<i64>,
    /// `last_ns` at the latest novelty sample (a revisit adds one sample).
    history_ns: i64,
}

impl TrackEvidence {
    fn new(f_lo_hz: f64, f_hi_hz: f64, t_ns: i64) -> Self {
        Self {
            f_lo_hz,
            f_hi_hz,
            first_ns: t_ns,
            last_ns: t_ns,
            members: 0,
            suspect_members: 0,
            snr_db: None,
            bursts: 0,
            burst_starts: VecDeque::with_capacity(MAX_BURST_STARTS),
            confirmed: false,
            recipe_match: false,
            trust_tested: false,
            novelty_history: VecDeque::with_capacity(MAX_NOVELTY_HISTORY),
            last_novelty: 0.0,
            closed_ns: None,
            history_ns: i64::MIN,
        }
    }

    /// Occupied extent.
    pub fn freq(&self) -> FreqRange {
        FreqRange::new(self.f_lo_hz, self.f_hi_hz.max(self.f_lo_hz + 1.0))
    }

    /// Share of members with a suspect flag.
    pub fn suspect_fraction(&self) -> f64 {
        if self.members == 0 {
            0.0
        } else {
            f64::from(self.suspect_members) / f64::from(self.members)
        }
    }

    /// Silence since the track closed, s at `now_ns`; `None` while it is still open — evidence is
    /// still arriving, so no absence has been observed.
    pub fn silence_s(&self, now_ns: i64) -> Option<f64> {
        self.closed_ns
            .map(|c| now_ns.saturating_sub(c).max(0) as f64 / 1e9)
    }

    /// Confidence in this candidate's hypothesis under `gap` (`hk_model::presence`, the same law
    /// the inventory row reads): 1 while it is still on the air, then one `1/e` per idle gap of
    /// observed silence once it has closed. Derived on every call; never stored.
    pub fn confidence(&self, gap: IdleGap, now_ns: i64) -> f64 {
        self.silence_s(now_ns)
            .map_or(1.0, |s| confidence_after_silence(s, gap))
    }

    /// `(periodicity, expected interval s)`: periodicity = 1 − CV of the inter-burst intervals
    /// over ≥ [`PERIODICITY_MIN_BURSTS`] bursts; the interval is their mean (from 2 bursts).
    pub fn periodicity(&self) -> (Option<f64>, Option<f64>) {
        let starts = &self.burst_starts;
        if starts.len() < 2 {
            return (None, None);
        }
        let d: Vec<f64> = starts
            .iter()
            .zip(starts.iter().skip(1))
            .map(|(a, b)| (b - a) as f64 / 1e9)
            .filter(|x| *x > 0.0)
            .collect();
        if d.is_empty() {
            return (None, None);
        }
        let mean = d.iter().sum::<f64>() / d.len() as f64;
        let interval = Some(mean.min(1e7));
        if starts.len() < PERIODICITY_MIN_BURSTS {
            return (None, interval);
        }
        let var = d.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / d.len() as f64;
        let cv = var.sqrt() / mean;
        (Some((1.0 - cv).clamp(0.0, 1.0)), interval)
    }
}

/// A learned channel under a candidate, as the baselines see it.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelContext {
    /// Latest fold's novelty against the reference.
    pub novelty: NoveltyScore,
    /// FCO of each mature reference pool (the boring prior's `always_on`).
    pub pool_fcos: Vec<f64>,
}

/// Evidence the table asks for at scoring time.
pub trait EvidenceSource {
    /// The learned channel overlapping `freq` most, with its baseline novelty.
    fn channel(&self, freq: FreqRange) -> Option<ChannelContext>;
    /// The site's new-emitter novelty over the first-sighting window (`None` while immature).
    fn new_emitter(&self) -> Option<f64>;
    /// Valid decodes written for `track`.
    fn decodes(&self, track: TrackId) -> u64;
    /// Normalised class entropy of `track`'s inventory classification (`None` = never
    /// classified).
    fn class_entropy(&self, track: TrackId) -> Option<f64>;
}

/// The subject key the scheduler's bandit uses for a track candidate.
pub fn track_key(track: TrackId) -> u64 {
    track.as_uuid().as_u128() as u64
}

/// Blind tracks as candidates.
#[derive(Debug, Default)]
pub struct CandidateTable {
    tracks: HashMap<TrackId, TrackEvidence>,
    /// A confirmed track appeared, closed or changed class since the last scoring pass.
    changed: bool,
    /// Newest sample-clock time seen (members, scoring passes), ns.
    now_ns: i64,
    /// The reading's idle gap: the decay constant of a closed track's confidence and, through
    /// `recheck_horizon_s`, how long it is kept as a re-check request. Defaults to the
    /// conservative 60 s, which claims no absence that was not observed.
    idle_gap: IdleGap,
}

impl CandidateTable {
    /// Tracks kept.
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    /// No track kept.
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// One track's evidence.
    pub fn get(&self, track: TrackId) -> Option<&TrackEvidence> {
        self.tracks.get(&track)
    }

    /// Sets the reading's idle gap, from the scheduler's revisit period
    /// (`IdleGap::from_revisit_s`). Unset, it is the conservative 60 s.
    pub fn set_idle_gap(&mut self, gap: IdleGap) {
        self.idle_gap = gap;
    }

    /// The idle gap in force.
    pub fn idle_gap(&self) -> IdleGap {
        self.idle_gap
    }

    /// Closed tracks still being re-checked (module docs).
    pub fn stale(&self) -> usize {
        self.tracks
            .values()
            .filter(|t| t.closed_ns.is_some())
            .count()
    }

    /// Retires closed tracks past [`recheck_horizon_s`]: their confidence has decayed below the
    /// scheduler's own dwell-share floor, so a dwell spent re-checking them is one taken from a
    /// candidate with live evidence.
    ///
    /// This removes a **scheduler working-set row only**. The emitter, its presence intervals and
    /// History are in the repository, which this module cannot reach; a signal returning later
    /// arrives as a new track and is resolved onto the same emitter there (T-262).
    fn prune_stale(&mut self) {
        let horizon_ns = (recheck_horizon_s(self.idle_gap) * 1e9) as i64;
        let now = self.now_ns;
        let mut dropped = false;
        self.tracks.retain(|_, t| {
            let keep = t
                .closed_ns
                .is_none_or(|c| now.saturating_sub(c) <= horizon_ns);
            dropped |= !keep && t.confirmed;
            keep
        });
        self.changed |= dropped;
    }

    /// Publish at the next pass (a verification group ended).
    pub fn mark_changed(&mut self) {
        self.changed = true;
    }

    /// Takes the "publish now" flag (a confirmed track appeared or closed).
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    fn evict_for_insert(&mut self) {
        if self.tracks.len() < MAX_TRACKS {
            return;
        }
        // A closed track is only a re-check request, so it goes before anything still on the air
        // (`false` orders first, and `closed_ns.is_none()` is false for a closed one).
        if let Some(old) = self
            .tracks
            .iter()
            .min_by_key(|(_, t)| (t.closed_ns.is_none(), t.confirmed, t.last_ns))
            .map(|(id, _)| *id)
        {
            let t = self.tracks.remove(&old);
            self.changed |= t.is_some_and(|t| t.confirmed);
        }
    }

    /// Folds a member. Returns whether the track is new to the table and its latest novelty.
    pub fn on_member(&mut self, track: TrackId, m: &MemberEvidence) -> (bool, f64) {
        self.now_ns = self.now_ns.max(m.t_ns);
        let fresh = !self.tracks.contains_key(&track);
        if fresh {
            self.evict_for_insert();
        }
        let e = self
            .tracks
            .entry(track)
            .or_insert_with(|| TrackEvidence::new(m.f_lo_hz, m.f_hi_hz, m.t_ns));
        e.f_lo_hz = e.f_lo_hz.min(m.f_lo_hz);
        e.f_hi_hz = e.f_hi_hz.max(m.f_hi_hz);
        e.first_ns = e.first_ns.min(m.t_ns);
        e.last_ns = e.last_ns.max(m.t_ns);
        // Evidence arrived, so whatever silence had accumulated is over. Nothing was stored, so
        // there is nothing to undo — the same reversibility the presence intervals have.
        e.closed_ns = None;
        if e.members == u32::MAX / 2 {
            e.members /= 2;
            e.suspect_members /= 2;
        }
        e.members += 1;
        e.suspect_members += u32::from(m.suspect);
        if let Some(s) = m.snr_db.filter(|s| s.is_finite()) {
            e.snr_db = Some(e.snr_db.map_or(s, |p| p + SNR_ALPHA * (s - p)));
        }
        if !m.continues {
            e.bursts = e.bursts.saturating_add(1);
            if e.burst_starts.len() == MAX_BURST_STARTS {
                e.burst_starts.pop_front();
            }
            e.burst_starts.push_back(m.t_ns);
        }
        (fresh, e.last_novelty)
    }

    /// T-174 (ADR-0012 §2.6): a member folded as suspect only for its DC flag was refuted by a
    /// clean twin from another tuning, so it no longer counts as suspect.
    pub fn on_member_refuted(&mut self, track: TrackId) {
        if let Some(e) = self.tracks.get_mut(&track) {
            e.suspect_members = e.suspect_members.saturating_sub(1);
        }
    }

    /// Confirmed tracks and their extents.
    pub fn confirmed(&self) -> Vec<(TrackId, FreqRange)> {
        self.tracks
            .iter()
            .filter(|(_, t)| t.confirmed)
            .map(|(id, t)| (*id, t.freq()))
            .collect()
    }

    /// The detector confirmed `track` over `freq`; `recipe_match` when a chain recipe matches.
    /// A track with no member yet starts at the newest member time the table has seen.
    pub fn on_confirmed(&mut self, track: TrackId, freq: FreqRange, recipe_match: bool) {
        if !self.tracks.contains_key(&track) {
            self.evict_for_insert();
        }
        let now = self.now_ns;
        let e = self
            .tracks
            .entry(track)
            .or_insert_with(|| TrackEvidence::new(freq.lo_hz, freq.hi_hz, now));
        e.confirmed = true;
        e.recipe_match |= recipe_match;
        self.changed = true;
    }

    /// `track` closed. It is **kept**, not dropped (module docs): a candidate whose signal
    /// stopped is exactly the one worth re-checking, and dropping it here is what left stopped
    /// candidates never re-verified. It stays a scorer input, with a decaying confidence, until
    /// [`Self::prune_stale`] retires it at the re-check horizon.
    pub fn on_closed(&mut self, track: TrackId) {
        let now = self.now_ns;
        if let Some(e) = self.tracks.get_mut(&track) {
            e.closed_ns.get_or_insert(now);
            self.changed |= e.confirmed;
        }
    }

    /// A trust-test verdict reached the candidate with bandit subject key `key`: it is no longer
    /// asked for verification. Returns the track.
    pub fn on_trust_tested(&mut self, key: u64) -> Option<TrackId> {
        let (id, e) = self
            .tracks
            .iter_mut()
            .find(|(id, _)| track_key(**id) == key)?;
        e.trust_tested = true;
        self.changed = true;
        Some(*id)
    }

    /// Confirmed tracks as scorer inputs at `now_ns`, with a revisit's novelty sample appended to
    /// each track seen again since the last pass.
    pub fn inputs(&mut self, now_ns: i64, src: &dyn EvidenceSource) -> Vec<CandidateInput> {
        self.now_ns = self.now_ns.max(now_ns);
        self.prune_stale();
        let new_emitter = src.new_emitter();
        let window_ns = (FirstSightingRate::WINDOW_S * 1e9) as i64;
        let mut out = Vec::with_capacity(self.tracks.len());
        let mut ids: Vec<TrackId> = self
            .tracks
            .iter()
            .filter(|(_, t)| t.confirmed)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_by_key(|id| id.as_uuid().as_u128());
        for id in ids {
            let e = self.tracks.get_mut(&id).expect("listed");
            let freq = e.freq();
            let channel = src.channel(freq);
            // New-emitter novelty belongs to tracks first seen inside the sighting window.
            let recent = now_ns.saturating_sub(e.first_ns) <= window_ns;
            let ne = new_emitter.filter(|_| recent);
            let novelty = combine_novelty(channel.as_ref().map(|c| &c.novelty), ne);
            e.last_novelty = novelty.novelty;
            if e.last_ns > e.history_ns {
                if e.novelty_history.len() == MAX_NOVELTY_HISTORY {
                    e.novelty_history.pop_front();
                }
                e.novelty_history.push_back(novelty.novelty);
                e.history_ns = e.last_ns;
            }
            let decodes = src.decodes(id);
            let class_entropy = src.class_entropy(id);
            let (periodicity, expected_interval_s) = e.periodicity();
            let history: Vec<f64> = e.novelty_history.iter().copied().collect();
            out.push(CandidateInput {
                subject: CandidateSubject::Track { id },
                freq,
                snr_db: e.snr_db,
                novelty,
                class_entropy,
                decoder_available: e.recipe_match,
                periodicity,
                boring: BoringEvidence {
                    always_on: channel.as_ref().is_some_and(|c| always_on(&c.pool_fcos)),
                    stable: stable(&history),
                    well_characterised: well_characterised(class_entropy, decodes > 0),
                    user_boring: false,
                    allocation_suggested: false,
                    blindly_characterised: true,
                    allocation_mismatch: false,
                },
                suspect_fraction: e.suspect_fraction(),
                trust_tested: e.trust_tested,
                expected_interval_s,
                min_on_off_s: None,
                next_burst_eta: None,
            });
        }
        out
    }
}

/// Channel novelty and new-emitter novelty as one score under the zero rules: an immature
/// channel contributes nothing; a mature site first-sighting rate makes the score mature at the
/// all-hours resolution (new-emitter novelty is a site-wide statistic).
pub fn combine_novelty(channel: Option<&NoveltyScore>, new_emitter: Option<f64>) -> NoveltyScore {
    let ne = new_emitter.map(|v| v.clamp(0.0, 1.0));
    let mut n = channel.copied().unwrap_or(NoveltyScore {
        novelty: 0.0,
        level_z: None,
        occupancy_z: None,
        new_emitter: None,
        observed_s: 0.0,
        maturity: Maturity::Immature { observed_s: 0.0 },
        provenance_explained: false,
    });
    if let Some(v) = ne {
        n.new_emitter = Some(v);
        if !n.provenance_explained {
            if !n.maturity.is_mature() {
                n.maturity = Maturity::Mature {
                    resolution: BaselineResolution::AllHours,
                };
                n.novelty = 0.0;
            }
            n.novelty = n.novelty.max(v);
        }
    }
    n
}

/// Normalised class entropy of an inventory classification: the binary entropy of its
/// confidence, raised to its open-set score; an `unknown` family is maximally uncertain.
pub fn class_entropy(family: &str, confidence: f64, open_set_score: f64) -> f64 {
    if family.eq_ignore_ascii_case("unknown") {
        return 1.0;
    }
    let p = confidence.clamp(0.0, 1.0);
    let h = if p <= 0.0 || p >= 1.0 {
        0.0
    } else {
        -(p * p.log2() + (1.0 - p) * (1.0 - p).log2())
    };
    h.max(open_set_score.clamp(0.0, 1.0)).clamp(0.0, 1.0)
}

/// Stamps `t` from ns.
pub fn ts(ns: i64) -> Timestamp {
    Timestamp::from_unix_nanos(ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Src {
        channel: Option<ChannelContext>,
        new_emitter: Option<f64>,
        decodes: u64,
        entropy: Option<f64>,
    }

    impl EvidenceSource for Src {
        fn channel(&self, _: FreqRange) -> Option<ChannelContext> {
            self.channel.clone()
        }
        fn new_emitter(&self) -> Option<f64> {
            self.new_emitter
        }
        fn decodes(&self, _: TrackId) -> u64 {
            self.decodes
        }
        fn class_entropy(&self, _: TrackId) -> Option<f64> {
            self.entropy
        }
    }

    fn member(t_s: f64, suspect: bool) -> MemberEvidence {
        MemberEvidence {
            f_lo_hz: 433.9e6,
            f_hi_hz: 433.91e6,
            t_ns: (t_s * 1e9) as i64,
            continues: false,
            snr_db: Some(20.0),
            suspect,
        }
    }

    fn none() -> Src {
        Src {
            channel: None,
            new_emitter: None,
            decodes: 0,
            entropy: None,
        }
    }

    /// T-174: a member refuted by a DC twin leaves the suspect count (never below zero); a
    /// twinless LO leak's members stay suspect.
    #[test]
    fn candidates_suspect_members_drop_when_a_dc_flag_is_refuted() {
        let (carrier, leak) = (TrackId::new(), TrackId::new());
        let mut t = CandidateTable::default();
        for k in 0..4 {
            t.on_member(carrier, &member(f64::from(k), true));
            t.on_member(leak, &member(f64::from(k), true));
        }
        t.on_confirmed(carrier, FreqRange::new(433.9e6, 433.91e6), false);
        t.on_confirmed(leak, FreqRange::new(433.9e6, 433.91e6), false);
        for _ in 0..5 {
            t.on_member_refuted(carrier);
        }
        t.on_member_refuted(TrackId::new());
        assert_eq!(t.get(carrier).unwrap().suspect_fraction(), 0.0);
        assert_eq!(t.get(leak).unwrap().suspect_fraction(), 1.0);
        let inputs = t.inputs(ns_of(5.0), &none());
        let frac = |id| {
            inputs
                .iter()
                .find(|c| c.subject == CandidateSubject::Track { id })
                .unwrap()
                .suspect_fraction
        };
        assert_eq!((frac(carrier), frac(leak)), (0.0, 1.0));
    }

    fn ns_of(s: f64) -> i64 {
        (s * 1e9) as i64
    }

    #[test]
    fn candidates_measure_snr_periodicity_suspects_and_unclassified_entropy() {
        let mut t = CandidateTable::default();
        let id = TrackId::new();
        for i in 0..8 {
            t.on_member(id, &member(10.0 * f64::from(i), i % 4 == 0));
        }
        assert!(
            t.inputs(0, &none()).is_empty(),
            "unconfirmed tracks are not candidates"
        );
        t.on_confirmed(id, FreqRange::new(433.9e6, 433.91e6), true);
        assert!(t.take_changed());
        let c = t.inputs(80_000_000_000, &none()).pop().unwrap();
        assert_eq!(c.snr_db, Some(20.0));
        assert_eq!(c.periodicity, Some(1.0), "10 s bursts are periodic");
        assert_eq!(c.expected_interval_s, Some(10.0));
        assert_eq!(c.class_entropy, None, "unclassified = maximally uncertain");
        assert!(c.decoder_available);
        assert!((c.suspect_fraction - 0.25).abs() < 1e-12);
        assert_eq!(c.novelty.novelty, 0.0);
        c.novelty.validate().unwrap();

        // A mostly-suspect track asks for verification until a trust test classifies it.
        let imd = TrackId::new();
        for i in 0..4 {
            t.on_member(imd, &member(f64::from(i), true));
        }
        t.on_confirmed(imd, FreqRange::new(433.9e6, 433.91e6), false);
        let w = hk_model::attention::score::ScoreWeights::default();
        let find = |t: &mut CandidateTable| {
            t.inputs(5_000_000_000, &none())
                .into_iter()
                .find(|c| c.subject == CandidateSubject::Track { id: imd })
                .unwrap()
        };
        let c = find(&mut t);
        assert!(hk_context::occupancy::score::score_candidate(&w, &c).needs_verification);
        assert_eq!(t.on_trust_tested(track_key(imd)), Some(imd));
        let c = find(&mut t);
        assert!(!hk_context::occupancy::score::score_candidate(&w, &c).needs_verification);
    }

    /// T-251 (ADR-0017 TM-6): a track that closes is **kept** as a re-check request with a
    /// decaying confidence, and retired only at the horizon. Before this it was dropped on close,
    /// so the scheduler lost the arm the instant the signal stopped and never went back to find
    /// out whether it had really gone.
    #[test]
    fn candidates_keep_a_closed_track_as_a_recheck_request_until_the_horizon() {
        let gap = IdleGap::from_revisit_s(1.0); // 2 s gap, ~8 s horizon
        let mut t = CandidateTable::default();
        assert_eq!(
            t.idle_gap(),
            IdleGap::conservative(),
            "unset claims no absence it cannot show"
        );
        t.set_idle_gap(gap);
        let id = TrackId::new();
        t.on_member(id, &member(0.0, false));
        t.on_confirmed(id, FreqRange::new(433.9e6, 433.91e6), false);

        // On the air: no observed absence at all.
        let e = t.get(id).unwrap();
        assert_eq!(e.silence_s(ns_of(0.0)), None);
        assert_eq!(e.confidence(gap, ns_of(0.0)), 1.0);

        t.on_closed(id);
        assert!(t.take_changed(), "a closed confirmed track republishes");
        let e = t.get(id).unwrap();
        assert_eq!(e.closed_ns, Some(ns_of(0.0)), "kept, not dropped");
        assert_eq!(e.silence_s(ns_of(4.0)), Some(4.0));
        assert!(e.confidence(gap, ns_of(4.0)) < 1.0, "and ranked lower");

        // Inside the horizon it is still an arm: the scheduler can go back and look.
        assert_eq!(
            t.inputs(ns_of(4.0), &none()).len(),
            1,
            "a stopped candidate is still re-checked"
        );
        assert_eq!(t.stale(), 1);

        // Past it the hypothesis has decayed below the dwell-share floor, so it is retired.
        let horizon = recheck_horizon_s(gap);
        assert!(
            t.inputs(ns_of(horizon + 0.1), &none()).is_empty(),
            "retired at the {horizon} s horizon"
        );
        assert_eq!(t.len(), 0);
        assert!(t.take_changed(), "retiring an arm republishes too");
    }

    /// Reversible, with nothing to undo: evidence arriving on a closed track clears its silence.
    #[test]
    fn candidates_revive_when_evidence_returns() {
        let gap = IdleGap::from_revisit_s(1.0);
        let mut t = CandidateTable::default();
        t.set_idle_gap(gap);
        let id = TrackId::new();
        t.on_member(id, &member(0.0, false));
        t.on_confirmed(id, FreqRange::new(433.9e6, 433.91e6), false);
        t.on_closed(id);
        assert_eq!(t.stale(), 1);

        t.on_member(id, &member(3.0, false));
        assert_eq!(t.get(id).unwrap().closed_ns, None);
        assert_eq!(t.stale(), 0);
        assert_eq!(t.get(id).unwrap().confidence(gap, ns_of(3.0)), 1.0);
        assert_eq!(t.inputs(ns_of(3.0), &none()).len(), 1);
    }

    /// The horizon's floor is the scheduler's own `MIN_DWELL_SHARE`, not a number of this
    /// module's choosing. Asserted across the crate boundary so the two cannot drift apart.
    #[test]
    fn candidates_recheck_floor_is_the_schedulers_dwell_share() {
        assert_eq!(
            hk_model::NEGLIGIBLE_CONFIDENCE,
            hk_core::scheduler::MIN_DWELL_SHARE
        );
    }

    #[test]
    fn candidates_new_emitter_novelty_only_for_recent_tracks_and_boring_evidence() {
        let mut t = CandidateTable::default();
        let (old, new) = (TrackId::new(), TrackId::new());
        let hour = 3_600_000_000_000i64;
        let mut m = member(0.0, false);
        m.continues = true;
        t.on_member(old, &m);
        m.t_ns = 10 * hour;
        t.on_member(new, &m);
        for id in [old, new] {
            t.on_confirmed(id, FreqRange::new(433.9e6, 433.91e6), false);
        }
        let mature = Maturity::Mature {
            resolution: BaselineResolution::HourOfWeek,
        };
        let src = Src {
            channel: Some(ChannelContext {
                novelty: NoveltyScore {
                    novelty: 0.0,
                    level_z: Some(0.5),
                    occupancy_z: Some(0.1),
                    new_emitter: None,
                    observed_s: 900.0,
                    maturity: mature,
                    provenance_explained: false,
                },
                pool_fcos: vec![0.99, 0.97],
            }),
            new_emitter: Some(0.9),
            decodes: 3,
            entropy: Some(0.05),
        };
        let cs = t.inputs(10 * hour + 1, &src);
        let by = |id| {
            cs.iter()
                .find(|c| c.subject == CandidateSubject::Track { id })
                .unwrap()
        };
        assert_eq!(by(new).novelty.novelty, 0.9);
        assert_eq!(by(new).novelty.new_emitter, Some(0.9));
        assert_eq!(
            by(old).novelty.novelty,
            0.0,
            "first seen 10 h ago: not a new emitter"
        );
        let b = by(old).boring;
        assert!(b.always_on && b.well_characterised && !b.stable);
        for c in &cs {
            c.novelty.validate().unwrap();
        }
        // Immature channel, mature site rate: novelty from the new emitter alone, valid.
        let n = combine_novelty(None, Some(0.8));
        assert_eq!(n.novelty, 0.8);
        n.validate().unwrap();
        assert_eq!(combine_novelty(None, None).novelty, 0.0);
        assert!((class_entropy("fsk2", 0.5, 0.0) - 1.0).abs() < 1e-12);
        assert_eq!(class_entropy("unknown", 0.99, 0.0), 1.0);
        assert!(class_entropy("adsb", 0.99, 0.0) < 0.2);
    }
}
