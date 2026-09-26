//! **Listen on a bursty channel opens squelched and waits for the carrier** (T-987).
//!
//! # The defect
//!
//! Listen probes the channel once, at the instant it opens, and chooses the mode from what that
//! probe measured. Land-mobile, GMRS/FRS, airband and amateur voice are keyed for a few seconds
//! at a time; a user who opens Listen on such a channel almost always opens it **between** two
//! transmissions, the probe sees noise, and the request was refused `422 no-analog-mode` — every
//! bursty voice channel the explorer tried on 2026-09-25 (461.125, 462.225, 464.700 MHz NBFM,
//! airband) was refused, while the one keyed at the instant of opening (146.561 MHz) played.
//!
//! # The rule
//!
//! When the probe demodulates nothing — no analog mode recognised, or the strongest emission lies
//! outside the selection — Listen asks the **target's own history** before refusing:
//!
//! - **Which history.** An emitter target reads that emitter (its live id, following merges). A
//!   range target reads the inventory emitters whose centre lies inside the range (our own
//!   measured history, never the known-signal database) and takes the one with the most evidence.
//!   A detection target has no history of its own and is refused as before.
//! - **What counts as evidence.** One **observation** per burst: a Classification on the emitter
//!   whose family names an analog mode (the per-burst classifier, [`crate::chains::classify`],
//!   and a demodulated session's own mode) at no less than the family vocabulary's
//!   [`MIN_CONFIDENCE`](crate::family::MIN_CONFIDENCE), and every Demodulation session written for
//!   it, declined ones included (T-416) — each of which carries the estimates its burst measured
//!   (T-953's parameters). A classification and a session written for the same burst share a
//!   timestamp and count once.
//! - **The mode** is the one most observations named (a tie goes to the most recent); the channel
//!   is the emitter's refined centre when output analysis stored one (T-070), else its measured
//!   centre, as wide as the mode's rule makes the bandwidth its sessions measured (else the
//!   emitter's own).
//! - **The squelch** is armed from the silence the probe just read — the channel power through
//!   the very channel filter the demodulator will use ([`hk_demod::audio::channel_noise_power`]) —
//!   so the stream carries status records and no audio until the carrier returns, and audio when
//!   it does. A silence that cannot be measured is refused: a squelch with no floor stays open and
//!   would stream noise labelled as the emitter.
//! - **The header says so** ([`hk_stream::audio::CarrierWait`]): `waiting for carrier (last seen
//!   t, mode nbfm from n of m bursts)`; `mode_confidence` is the history's agreement `n / m` and
//!   `mode_rules` is the selector's version with `+history`, because the selector measured nothing
//!   now.
//!
//! **A target with no analog evidence is still refused**, with the probe's reason and what the
//! history held (nothing, or only non-audio modes such as `2fsk`): the mode is never assumed.

use std::collections::BTreeMap;

use hk_demod::AnalogMode;
use hk_demod::audio::{AudioPlan, Sideband};
use hk_model::{
    EmitterId, EstimatedParams, FreqRange, Region, RepoError, Repository, TimeRange, Timestamp,
};
use hk_stream::audio::CarrierWait;

/// Most sessions read per emitter (newest first); a longer history adds nothing to a majority.
const MAX_SESSIONS: usize = 256;
/// Most range-target candidates considered (the most recently seen).
const MAX_CANDIDATES: usize = 32;

/// One burst's evidence.
#[derive(Clone, Debug)]
struct Observation {
    mode: AnalogMode,
    sideband: Option<Sideband>,
    /// The session's measured parameters (a classification carries none).
    params: Option<EstimatedParams>,
}

/// What one emitter's history says.
#[derive(Clone, Debug)]
struct Tally {
    emitter: EmitterId,
    last_seen: Timestamp,
    center_hz: f64,
    bandwidth_hz: f64,
    /// Analog observations by time (ns), oldest first.
    analog: BTreeMap<i64, Observation>,
    /// Observations that named something other than an analog audio mode, by label.
    other: BTreeMap<String, u32>,
}

/// The mode and channel a target's history chose, for a Listen that opens waiting.
#[derive(Clone, Debug)]
pub(crate) struct History {
    pub emitter: EmitterId,
    pub last_seen: Timestamp,
    pub mode: AnalogMode,
    pub sideband: Option<Sideband>,
    pub center_hz: f64,
    /// Occupied bandwidth the plan is sized from, Hz.
    pub obw_hz: f64,
    pub mode_bursts: u32,
    pub analog_bursts: u32,
    /// The parameters the latest agreeing session measured, or the bandwidth alone.
    pub params: EstimatedParams,
}

impl History {
    /// The plan this history chooses (no noise power yet: that is measured on the silence).
    pub fn plan(&self, cfg: &hk_demod::audio::AudioConfig) -> Option<AudioPlan> {
        AudioPlan::for_mode(
            self.mode,
            self.sideband,
            self.center_hz,
            Some(self.obw_hz),
            cfg,
        )
    }

    /// The history's agreement on its mode, `n / m`.
    pub fn agreement(&self) -> f64 {
        if self.analog_bursts == 0 {
            0.0
        } else {
            f64::from(self.mode_bursts) / f64::from(self.analog_bursts)
        }
    }

    /// The header block, with `probe` the reason the probe demodulated nothing.
    pub fn header(&self, mode_name: &str, probe: &str) -> CarrierWait {
        CarrierWait {
            emitter_id: self.emitter,
            last_seen: self.last_seen,
            mode_bursts: self.mode_bursts,
            analog_bursts: self.analog_bursts,
            probe: probe.to_owned(),
            statement: format!(
                "waiting for carrier (last seen {}, mode {mode_name} from {} of {} bursts)",
                utc_seconds(self.last_seen),
                self.mode_bursts,
                self.analog_bursts
            ),
        }
    }
}

/// What the target's history held, when it chose nothing (for the refusal's reason).
#[derive(Clone, Debug)]
pub(crate) struct NoHistory(pub String);

/// The target's history: an emitter target's own, else the emitters whose centre lies inside
/// `(lo, hi)`. `Err(NoHistory)` says what was there instead of an analog mode.
pub(crate) fn history(
    repo: &Repository,
    emitter: Option<EmitterId>,
    (lo, hi): (f64, f64),
    now: Timestamp,
) -> Result<Result<History, NoHistory>, RepoError> {
    let candidates: Vec<EmitterId> = match emitter {
        Some(id) => vec![repo.live_emitter_id(id)?],
        None => {
            let region = Region::new(
                FreqRange::new(lo, hi),
                TimeRange::new(Timestamp::UNIX_EPOCH, now.saturating_add_nanos(86_400 * NS)),
            );
            repo.emitters_in_region_limited(&region, Some(MAX_CANDIDATES))?
                .into_iter()
                .filter(|e| e.f_center_hz >= lo && e.f_center_hz <= hi)
                .map(|e| e.id)
                .collect()
        }
    };
    if candidates.is_empty() {
        return Ok(Err(NoHistory(
            "no inventory emitter at this selection has a history".into(),
        )));
    }
    let mut tallies = Vec::with_capacity(candidates.len());
    for id in candidates {
        tallies.push(tally(repo, id)?);
    }
    // The emitter with the most analog evidence; a tie goes to the most recently seen.
    let best = tallies
        .iter()
        .max_by(|a, b| {
            a.analog
                .len()
                .cmp(&b.analog.len())
                .then(a.last_seen.cmp(&b.last_seen))
        })
        .expect("at least one candidate");
    if best.analog.is_empty() {
        let mut other: BTreeMap<&str, u32> = BTreeMap::new();
        for t in &tallies {
            for (label, n) in &t.other {
                *other.entry(label.as_str()).or_default() += n;
            }
        }
        let held = if other.is_empty() {
            "no burst in its history carries a mode estimate".to_owned()
        } else {
            let list: Vec<String> = other.iter().map(|(l, n)| format!("{l} ×{n}")).collect();
            format!(
                "its history names no analog audio mode (only {})",
                list.join(", ")
            )
        };
        return Ok(Err(NoHistory(held)));
    }
    let refined = repo.refined_tuning(best.emitter)?;
    Ok(Ok(choose(best, refined.map(|r| r.center_hz))))
}

/// The majority mode of `t`, its channel and parameters.
fn choose(t: &Tally, refined_center_hz: Option<f64>) -> History {
    let mut counts: BTreeMap<(u8, u8), (u32, i64)> = BTreeMap::new();
    for (&at, o) in &t.analog {
        let e = counts.entry(key(o.mode, o.sideband)).or_insert((0, at));
        e.0 += 1;
        e.1 = e.1.max(at);
    }
    let (&(mode_key, sb_key), &(n, _)) = counts
        .iter()
        .max_by(|a, b| a.1.0.cmp(&b.1.0).then(a.1.1.cmp(&b.1.1)))
        .expect("analog evidence");
    let agreeing: Vec<&Observation> = t
        .analog
        .values()
        .filter(|o| key(o.mode, o.sideband) == (mode_key, sb_key))
        .collect();
    let chosen = agreeing.last().expect("agreeing evidence");
    // The sessions' measured occupied bandwidth (median), else the emitter's.
    let mut bws: Vec<f64> = agreeing
        .iter()
        .filter_map(|o| o.params.as_ref()?.bandwidth_hz)
        .filter(|b| b.is_finite() && *b > 0.0)
        .collect();
    bws.sort_by(f64::total_cmp);
    let obw_hz = bws.get(bws.len() / 2).copied().unwrap_or(t.bandwidth_hz);
    let params = agreeing
        .iter()
        .rev()
        .find_map(|o| o.params.clone())
        .unwrap_or_default();
    History {
        emitter: t.emitter,
        last_seen: t.last_seen,
        mode: chosen.mode,
        sideband: chosen.sideband,
        center_hz: refined_center_hz
            .filter(|c| c.is_finite())
            .unwrap_or(t.center_hz),
        obw_hz,
        mode_bursts: n,
        analog_bursts: u32::try_from(t.analog.len()).unwrap_or(u32::MAX),
        params: EstimatedParams {
            bandwidth_hz: Some(obw_hz),
            ..params
        },
    }
}

fn key(mode: AnalogMode, sideband: Option<Sideband>) -> (u8, u8) {
    (
        mode as u8,
        match sideband {
            None => 0,
            Some(Sideband::Upper) => 1,
            Some(Sideband::Lower) => 2,
        },
    )
}

/// One emitter's observations.
fn tally(repo: &Repository, id: EmitterId) -> Result<Tally, RepoError> {
    let e = repo.emitter(id)?;
    let mut t = Tally {
        emitter: e.id,
        last_seen: e.last_seen,
        center_hz: e.f_center_hz,
        bandwidth_hz: e.bandwidth_hz,
        analog: BTreeMap::new(),
        other: BTreeMap::new(),
    };
    for c in &e.classifications {
        if c.confidence.is_nan() || c.confidence < crate::family::MIN_CONFIDENCE {
            continue;
        }
        match AudioPlan::mode_from_label(&c.family) {
            Some((mode, sideband)) => {
                t.analog.entry(c.t.as_unix_nanos()).or_insert(Observation {
                    mode,
                    sideband,
                    params: None,
                });
            }
            None => *t.other.entry(c.family.clone()).or_default() += 1,
        }
    }
    for d in repo.demodulations_for_emitter(e.id, MAX_SESSIONS)? {
        match AudioPlan::mode_from_label(&d.mode) {
            // A session supersedes the classification written for the same burst: it carries
            // the burst's measured parameters.
            Some((mode, sideband)) => {
                t.analog.insert(
                    d.time.end.as_unix_nanos(),
                    Observation {
                        mode,
                        sideband,
                        params: Some(d.params.clone()),
                    },
                );
            }
            None => *t.other.entry(d.mode.clone()).or_default() += 1,
        }
    }
    t.other.remove("unknown");
    Ok(t)
}

const NS: i64 = 1_000_000_000;

/// `t` as ISO-8601 UTC to the second.
fn utc_seconds(t: Timestamp) -> String {
    let whole = Timestamp::from_unix_nanos(t.as_unix_nanos().div_euclid(NS) * NS);
    let s = super::super::record::iso8601(whole);
    match s.split_once('.') {
        Some((head, _)) => format!("{head}Z"),
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::{Classification, Demodulation, DemodulationId, Emitter, Identity, KnownStatus};

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_300_800_000_000_000 + s * NS)
    }

    fn emitter(repo: &mut Repository, f: f64, families: &[(&str, i64, f64)]) -> EmitterId {
        let e = Emitter {
            id: EmitterId::new(),
            f_center_hz: f,
            bandwidth_hz: 11e3,
            first_seen: t(0),
            last_seen: t(100),
            count: families.len() as u64,
            fingerprint: serde_json::Value::Null,
            identity: Identity::Unknown,
            known_status: KnownStatus::Unknown,
            classifications: families
                .iter()
                .map(|&(family, at, confidence)| Classification {
                    t: t(at),
                    family: family.into(),
                    confidence,
                    open_set_score: 1.0 - confidence,
                    model_version: "test".into(),
                })
                .collect(),
            tags: Default::default(),
        };
        repo.insert_emitter(&e).unwrap();
        e.id
    }

    fn session(repo: &mut Repository, id: EmitterId, mode: &str, end: i64, bw: f64) {
        repo.insert_demodulation(&Demodulation {
            id: DemodulationId::new(),
            emitter_ref: Some(id),
            detection_ref: None,
            recording_ref: None,
            mode: mode.into(),
            params: EstimatedParams {
                bandwidth_hz: Some(bw),
                deviation_hz: Some(2_500.0),
                ..EstimatedParams::default()
            },
            lock_quality: None,
            evm_db: None,
            time: TimeRange::new(t(end - 1), t(end)),
            demod_version: "test".into(),
        })
        .unwrap();
    }

    #[test]
    fn the_majority_of_bursts_chooses_the_mode_and_a_session_counts_its_burst_once() {
        let mut repo = Repository::open_in_memory().unwrap();
        let id = emitter(
            &mut repo,
            462.5625e6,
            &[
                ("nbfm", 10, 0.8),
                ("nbfm", 20, 0.7),
                ("am", 30, 0.9),
                // Below the vocabulary's floor: not evidence.
                ("am", 40, 0.2),
                ("unknown", 50, 0.9),
            ],
        );
        // The session written for the burst at 20 s supersedes its classification.
        session(&mut repo, id, "nbfm", 20, 9e3);
        let h = history(&repo, Some(id), (0.0, 0.0), t(200))
            .unwrap()
            .expect("analog evidence");
        assert_eq!(h.mode, AnalogMode::Nbfm);
        assert_eq!((h.mode_bursts, h.analog_bursts), (2, 3));
        assert!((h.agreement() - 2.0 / 3.0).abs() < 1e-12);
        assert_eq!(h.obw_hz, 9e3, "the sessions' measured bandwidth");
        assert_eq!(h.params.deviation_hz, Some(2_500.0));
        assert_eq!(h.center_hz, 462.5625e6);
        let w = h.header("nbfm", "no analog modulation recognised: noise");
        assert_eq!(
            w.statement,
            "waiting for carrier (last seen 2026-09-13T12:01:40Z, mode nbfm from 2 of 3 bursts)"
        );
        // A range covering the emitter's centre reads the same history.
        let r = history(&repo, None, (462.55e6, 462.575e6), t(200))
            .unwrap()
            .expect("the range covers the emitter");
        assert_eq!((r.emitter, r.mode_bursts), (id, 2));
    }

    #[test]
    fn no_analog_evidence_is_no_history_and_says_what_was_there() {
        let mut repo = Repository::open_in_memory().unwrap();
        let id = emitter(&mut repo, 929.6e6, &[("2fsk", 10, 0.9)]);
        session(&mut repo, id, "2fsk", 20, 20e3);
        let NoHistory(why) = history(&repo, Some(id), (0.0, 0.0), t(200))
            .unwrap()
            .expect_err("no analog mode in the history");
        assert!(why.contains("2fsk ×2"), "{why}");
        let NoHistory(why) = history(&repo, None, (100e6, 100.1e6), t(200))
            .unwrap()
            .expect_err("no emitter there");
        assert!(why.contains("no inventory emitter"), "{why}");
    }
}
