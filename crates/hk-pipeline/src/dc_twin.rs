//! T-174 (ADR-0012 §2.6): the per-tuning DC-twin rule on the live candidate path.
//!
//! The detection reader flags a detection within the detector's DC tolerance of its own tuning's
//! LO as a DC spur. That flag is a hypothesis about one tuning: a clean twin of the same emission
//! from a tuning whose DC is elsewhere, within ± one time cell, refutes it (T-147, T-172). The
//! occupancy engine applies the rule to stored detections; [`LiveDcTwins`] applies the same helper
//! ([`refute_dc_suspects`]) to detections as the reader emits them, in either order: a twin
//! already seen refutes a new DC flag at once, and a DC flag stays open until a later twin can no
//! longer start inside its slack.
//!
//! **Cost and bounds.** No locks and no database: the index is the reader's own state. It keeps
//! the clean detections emitted in the last slack + [`EMIT_LAG_NS`] (at most [`MAX_CLEAN`]) and
//! the unrefuted DC flags still inside their window (at most [`MAX_OPEN`]). A batch runs the
//! helper at most twice: its new DC flags against the kept clean detections, O((n + m) log n), and
//! the open DC flags against its own clean detections, O((k + m) log k). A batch with neither DC
//! flags nor open flags costs one scan of the batch.

use std::collections::VecDeque;

use hk_context::occupancy::channels::{DcTwinRule, DetectionExtent, refute_dc_suspects};
use hk_model::DetectionId;

/// Clean detections kept (the oldest emitted go first when full).
pub(crate) const MAX_CLEAN: usize = 4096;
/// Open DC flags kept (the oldest emitted go first when full; they stay suspect).
pub(crate) const MAX_OPEN: usize = 512;
/// How long after a detection's end the reader may still emit a detection starting before it,
/// ns: a record is emitted at its close, at most the detector's maximum duration (1 s) plus gap
/// merges after its start.
pub(crate) const EMIT_LAG_NS: i64 = 5_000_000_000;

/// One detection as the reader emitted it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Observed {
    /// Detection.
    pub id: DetectionId,
    /// Its §2.6 extent (`suspect` by the per-detection rule).
    pub extent: DetectionExtent,
    /// Flagged only as a DC spur ([`hk_context::occupancy::channels::dc_only_suspect`]).
    pub dc_only: bool,
    /// Its own tuning centre, Hz (Provenance `tune.center_hz`).
    pub own_lo: Option<f64>,
}

/// The reader's recent clean detections and open DC flags (see the module docs).
pub(crate) struct LiveDcTwins {
    rule: DcTwinRule,
    clean: VecDeque<Observed>,
    open: VecDeque<Observed>,
    scratch: Vec<DetectionExtent>,
    dc: Vec<bool>,
    lo: Vec<Option<f64>>,
}

impl LiveDcTwins {
    pub fn new(rule: DcTwinRule) -> Self {
        Self {
            rule,
            clean: VecDeque::new(),
            open: VecDeque::new(),
            scratch: Vec::new(),
            dc: Vec::new(),
            lo: Vec::new(),
        }
    }

    /// Folds one batch emitted at stream time `now_ns` and appends the refuted DC flags to `out`:
    /// the batch's own, refuted by a clean twin kept or in the batch, and open ones from earlier
    /// batches, refuted by the batch's clean detections. A refuted flag never becomes a twin.
    pub fn observe(&mut self, now_ns: i64, batch: &[Observed], out: &mut Vec<DetectionId>) {
        self.prune(now_ns);
        let has_clean = batch.iter().any(|o| !o.extent.suspect);
        if has_clean && !self.open.is_empty() {
            let open: Vec<Observed> = self.open.drain(..).collect();
            let fresh = batch.iter().filter(|o| !o.extent.suspect).copied();
            let refuted = self.check(&open, fresh);
            for (o, r) in open.into_iter().zip(refuted) {
                if r {
                    out.push(o.id);
                } else {
                    self.open.push_back(o);
                }
            }
        }
        let flags: Vec<Observed> = batch
            .iter()
            .filter(|o| o.dc_only && o.extent.suspect)
            .copied()
            .collect();
        if !flags.is_empty() {
            let pool: Vec<Observed> = self
                .clean
                .iter()
                .copied()
                .chain(batch.iter().filter(|o| !o.extent.suspect).copied())
                .collect();
            let refuted = self.check(&flags, pool.into_iter());
            for (o, r) in flags.into_iter().zip(refuted) {
                if r {
                    out.push(o.id);
                } else {
                    self.open.push_back(o);
                }
            }
            while self.open.len() > MAX_OPEN {
                self.open.pop_front();
            }
        }
        for o in batch.iter().filter(|o| !o.extent.suspect) {
            self.clean.push_back(*o);
        }
        while self.clean.len() > MAX_CLEAN {
            self.clean.pop_front();
        }
    }

    /// Runs the §2.6 helper for `flags` against `pool` (clean detections); whether each flag was
    /// refuted.
    fn check(&mut self, flags: &[Observed], pool: impl Iterator<Item = Observed>) -> Vec<bool> {
        let (scratch, dc, lo) = (&mut self.scratch, &mut self.dc, &mut self.lo);
        scratch.clear();
        dc.clear();
        lo.clear();
        for f in flags {
            scratch.push(f.extent);
            dc.push(true);
            lo.push(f.own_lo);
        }
        for p in pool {
            scratch.push(p.extent);
            dc.push(false);
            lo.push(p.own_lo);
        }
        refute_dc_suspects(scratch, dc, self.rule, |j| lo[j]);
        scratch[..flags.len()].iter().map(|e| !e.suspect).collect()
    }

    /// Forgets clean detections no future DC flag can reach and open flags no future twin can.
    fn prune(&mut self, now_ns: i64) {
        let reach = self.rule.slack_ns.max(0).saturating_add(EMIT_LAG_NS);
        let stale = |o: &Observed| o.extent.time.end.as_unix_nanos().saturating_add(reach) < now_ns;
        while self.clean.front().is_some_and(stale) {
            self.clean.pop_front();
        }
        self.open.retain(|o| !stale(o));
    }

    #[cfg(test)]
    fn kept(&self) -> (usize, usize) {
        (self.clean.len(), self.open.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_context::occupancy::channels::DC_TWIN_LO_TOLERANCE_HZ;
    use hk_model::{FreqRange, TimeRange, Timestamp};

    const T_CELL: i64 = 1_000_000_000;

    fn rule() -> DcTwinRule {
        DcTwinRule {
            f_cell_hz: 6250.0,
            slack_ns: T_CELL,
            lo_tolerance_hz: DC_TWIN_LO_TOLERANCE_HZ,
        }
    }

    fn ns(s: f64) -> i64 {
        (s * 1e9) as i64
    }

    /// A 10 kHz carrier detection at `f` from `t` s for `dur` s, seen from a tuning at `lo`; DC
    /// flagged when within the tolerance of it (as the detector does).
    fn seen(f: f64, lo: f64, t: f64, dur: f64) -> Observed {
        let freq = FreqRange::centered(f, 10e3);
        let dc = lo >= freq.lo_hz - DC_TWIN_LO_TOLERANCE_HZ
            && lo <= freq.hi_hz + DC_TWIN_LO_TOLERANCE_HZ;
        Observed {
            id: DetectionId::new(),
            extent: DetectionExtent {
                time: TimeRange::new(
                    Timestamp::from_unix_nanos(ns(t)),
                    Timestamp::from_unix_nanos(ns(t + dur)),
                ),
                freq,
                obw_hz: 10e3,
                snr_db: 20.0,
                suspect: dc,
            },
            dc_only: dc,
            own_lo: Some(lo),
        }
    }

    const HOP_A: f64 = 433.5e6;
    const HOP_B: f64 = 433.25e6;
    /// 12.5 kHz from hop A's LO: DC-flagged from hop A, clean from hop B.
    const CARRIER: f64 = 433.5125e6;

    #[test]
    fn live_dc_flag_is_refuted_by_a_clean_twin_from_another_tuning_in_either_order() {
        // Twin first: hop B sees the carrier cleanly, hop A's DC flag 0.6 s later is refuted at once.
        let mut ix = LiveDcTwins::new(rule());
        let mut out = Vec::new();
        let twin = seen(CARRIER, HOP_B, 10.0, 0.5);
        assert!(!twin.extent.suspect);
        ix.observe(ns(10.5), &[twin], &mut out);
        let flag = seen(CARRIER, HOP_A, 11.1, 0.5);
        assert!(flag.dc_only && flag.extent.suspect);
        ix.observe(ns(11.6), &[flag], &mut out);
        assert_eq!(out, vec![flag.id]);
        assert_eq!(
            ix.kept(),
            (1, 0),
            "a refuted flag is neither open nor a twin"
        );

        // Flag first: it stays open, and hop B's clean detection 0.9 s after it ends refutes it.
        let mut ix = LiveDcTwins::new(rule());
        let mut out = Vec::new();
        let flag = seen(CARRIER, HOP_A, 20.0, 0.5);
        ix.observe(ns(20.5), &[flag], &mut out);
        assert!(out.is_empty());
        assert_eq!(ix.kept(), (0, 1));
        let twin = seen(CARRIER, HOP_B, 21.4, 0.5);
        ix.observe(ns(21.9), &[twin], &mut out);
        assert_eq!(out, vec![flag.id]);
        assert_eq!(ix.kept(), (1, 0));

        // Same batch.
        let mut ix = LiveDcTwins::new(rule());
        let mut out = Vec::new();
        let (flag, twin) = (
            seen(CARRIER, HOP_A, 30.0, 0.5),
            seen(CARRIER, HOP_B, 30.2, 0.5),
        );
        ix.observe(ns(30.7), &[flag, twin], &mut out);
        assert_eq!(out, vec![flag.id]);
    }

    #[test]
    fn live_true_lo_leak_that_moves_with_the_tuning_stays_suspect() {
        let mut ix = LiveDcTwins::new(rule());
        let mut out = Vec::new();
        // The leak sits at each hop's LO in turn; real carriers elsewhere are seen cleanly.
        for k in 0..40 {
            let t = f64::from(k) * 0.5;
            let lo = if k % 2 == 0 { HOP_A } else { HOP_B };
            let leak = seen(lo + 1e3, lo, t, 0.4);
            assert!(leak.dc_only);
            let other = seen(if k % 2 == 0 { 433.6e6 } else { 433.3e6 }, lo, t, 0.4);
            assert!(!other.extent.suspect);
            ix.observe(ns(t + 0.4), &[leak, other], &mut out);
        }
        assert!(out.is_empty(), "no leak flag refuted: {out:?}");
        let (clean, open) = ix.kept();
        assert!(clean > 0 && open > 0 && open <= MAX_OPEN);
    }

    #[test]
    fn live_artefact_at_the_other_tunings_lo_or_outside_one_time_cell_does_not_refute() {
        let mut ix = LiveDcTwins::new(rule());
        let mut out = Vec::new();
        let flag = seen(CARRIER, HOP_A, 10.0, 0.5);
        ix.observe(ns(10.5), &[flag], &mut out);
        // An unflagged detection at the same frequency whose own LO is 7.5 kHz away (inside the
        // DC tolerance of its extent): an artefact at that tuning's LO, not a twin.
        let mut artefact = seen(CARRIER, CARRIER - 7.5e3, 10.3, 0.5);
        artefact.extent.suspect = false;
        artefact.dc_only = false;
        // A clean twin 2.5 s after the flag ends: outside ± one time cell.
        let late = seen(CARRIER, HOP_B, 13.0, 0.5);
        // A clean detection 2 cells away from a far tuning: another emission.
        let neighbour = seen(CARRIER + 12.5e3, HOP_B, 10.2, 0.5);
        ix.observe(ns(10.8), &[artefact, neighbour], &mut out);
        ix.observe(ns(13.5), &[late], &mut out);
        assert!(out.is_empty(), "{out:?}");
        // An unknown tuning refutes nothing either.
        let flag = seen(CARRIER, HOP_A, 20.0, 0.5);
        let mut unknown = seen(CARRIER, HOP_B, 20.1, 0.5);
        unknown.own_lo = None;
        ix.observe(ns(20.6), &[flag, unknown], &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn live_dc_twin_index_is_bounded_and_forgets_stale_entries() {
        let mut ix = LiveDcTwins::new(rule());
        let mut out = Vec::new();
        for k in 0..(2 * MAX_CLEAN) {
            let t = 1.0 + k as f64 * 1e-4;
            let batch = [
                // Clean carriers from 440 MHz up: none is the leak's twin.
                seen(440e6 + k as f64 * 20e3, HOP_B, t, 0.01),
                seen(HOP_A + 1e3, HOP_A, t, 0.01),
            ];
            ix.observe(ns(t + 0.01), &batch, &mut out);
        }
        assert_eq!(ix.kept(), (MAX_CLEAN, MAX_OPEN));
        // Past slack + emission lag nothing is kept.
        ix.observe(ns(1.0) + 3 * T_CELL + EMIT_LAG_NS, &[], &mut out);
        assert_eq!(ix.kept(), (0, 0));
        assert!(out.is_empty());
    }
}
