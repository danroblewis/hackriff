//! C36 → C30: a measured statement about **GNSS service at this site** reaching event correlation
//! as *evidence*, never as a detection (T-322; ADR-0018; AWARE-002, AWARE-003).
//!
//! # Why this module exists, and what it refuses to do
//!
//! GPS L1 acquisition is the project's one documented exception to blind-first: it despreads
//! against published PRN codes because the signal sits 20–30 dB below the noise floor and there is
//! no energy for a blind detector to find (ADR-0018, `hk-gnss`). An exception is only safe while
//! it cannot leak, and the leak this module has to refuse is a *second door*: a Gold code
//! correlating and a row appearing in the inventory because of it.
//!
//! So the rule here is narrow and absolute.
//!
//! - **This module never inserts an [`hk_model::Anomaly`], a `Detection` or an `Emitter`.** It
//!   only ever *appends an [`Explanation`] to an anomaly the blind path already opened*. If no
//!   blind anomaly exists, a GNSS measurement produces nothing at all — which is the correct
//!   outcome, not a gap: "GPS works here" is not an event, and a handheld indoors losing
//!   satellites *without* a floor rise is a blocked antenna, not an attack (the C36 pitfall).
//! - **The evidence it writes is numbers.** [`Evidence::Value`] rows naming what was measured —
//!   satellites searched and acquired, carrier-to-noise, in-band power rise. Never
//!   [`Evidence::Detection`], never [`hk_model::Cause::Emitter`]: those are the shapes that would
//!   turn a known-code correlation into an inventory measurement.
//! - **It carries no codes.** The type crossing the crate boundary ([`GnssServiceEvidence`]) is a
//!   plain record of scalars and a verdict. `hk-context` does **not** depend on `hk-gnss`, so a
//!   `PrnCodebook` is un-nameable here just as it is in `hk-detect`. The caller
//!   (`hk_pipeline::gnss`) is the only place that holds both.
//!
//! # What a reader of the attack map sees
//!
//! An L-band noise-floor rise found blindly by C08, explained by the gpsjam rule as "an
//! interference cell covers this site" — and beside it, this: "the receiver's own GNSS service
//! went from 9 satellites to 0 while in-band power rose 12 dB." The first is a hypothesis from a
//! cached feed. The second is a local measurement that agrees with it. Ranking them is C30's
//! ordinary job; the measurement's [`hk_model::Cause::OwnHistory`] cause keeps it in the
//! device's-own-observation stage rather than pretending to be an external event.

use hk_model::{
    Anomaly, AnomalyStatus, Cause, CorrelationType, Evidence, Explanation, ExplanationId,
    FreqRange, Region, RepoError, Repository, TimeRange, Timestamp,
};

use crate::correlate::CORRELATED_KINDS;

/// Rule set id written to [`Explanation::rule_version`].
pub const RULE_VERSION: &str = "hk-context.gnss-service@1";

/// The L1 band this evidence speaks about, matching `feeds::gpsjam::GNSS_BANDS`' L1 row so a
/// measurement and a cached interference cell land on the same anomalies.
pub const L1_BAND_HZ: (f64, f64) = (1_559.0e6, 1_610.0e6);

/// What the measurement says about GNSS service, mirroring `hk_gnss::JammingVerdict` without
/// depending on it. The vocabulary is deliberately about *service*, not about an emitter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GnssServiceVerdict {
    /// Satellites where satellites are expected, and no in-band power rise.
    Quiet,
    /// Satellites lost **without** an in-band power rise: an obstructed or disconnected antenna,
    /// a body, a roof. Explicitly *not* jamming — a handheld must not fill the attack map with
    /// its own user.
    BlockageSuspect,
    /// Satellites lost **with** an in-band power rise: consistent with jamming.
    JammingSuspect,
}

impl GnssServiceVerdict {
    /// Stable label used in the explanation's description and as an evidence value.
    pub fn label(self) -> &'static str {
        match self {
            GnssServiceVerdict::Quiet => "quiet",
            GnssServiceVerdict::BlockageSuspect => "blockage-suspect",
            GnssServiceVerdict::JammingSuspect => "jamming-suspect",
        }
    }

    /// Whether the verdict is worth attaching to an anomaly at all. `Quiet` is not: GNSS working
    /// normally explains nothing, and writing it would be noise on every L-band anomaly.
    pub fn is_notable(self) -> bool {
        !matches!(self, GnssServiceVerdict::Quiet)
    }

    fn as_value(self) -> f64 {
        match self {
            GnssServiceVerdict::Quiet => 0.0,
            GnssServiceVerdict::BlockageSuspect => 1.0,
            GnssServiceVerdict::JammingSuspect => 2.0,
        }
    }
}

/// One scheduled L1 dwell's measured statement about GNSS service here.
///
/// Scalars and a verdict: no code, no PRN list, no frequency list, nothing that could seed a
/// detector. See the [module docs](self) for why that matters.
#[derive(Clone, Debug, PartialEq)]
pub struct GnssServiceEvidence {
    /// When the dwell was captured.
    pub t: Timestamp,
    /// The band the dwell covered.
    pub band: FreqRange,
    /// Satellites searched over the dwell (the size of the constellation searched, not a hint
    /// about which ones should be there).
    pub svs_searched: u32,
    /// Satellites whose correlation peak cleared the acquisition threshold.
    pub svs_acquired: u32,
    /// The most satellites acquired in any earlier dwell of this capture state — the reference
    /// this one is a loss against. Zero when there is no earlier dwell.
    pub svs_reference: u32,
    /// Mean estimated carrier-to-noise density of the acquired satellites, dB-Hz. `None` when
    /// none were acquired.
    pub mean_cn0_dbhz: Option<f64>,
    /// In-band power now, relative to the quietest dwell of this capture state, dB. The HackRF
    /// reports no front-end AGC, so in-band power is the available proxy (C36 card).
    pub power_rise_db: f64,
    /// The verdict.
    pub verdict: GnssServiceVerdict,
    /// Confidence in the verdict, 0–1.
    pub confidence: f64,
    /// Why, in the assessor's own words.
    pub reasons: Vec<String>,
}

impl GnssServiceEvidence {
    /// The numeric facts, as [`Evidence::Value`] rows.
    ///
    /// Deliberately **not** [`Evidence::Detection`]: a known-code correlation is not a blind
    /// measurement and must never be linked as one.
    pub fn values(&self) -> Vec<Evidence> {
        let value = |name: &str, value: f64| Evidence::Value {
            name: name.to_owned(),
            value,
        };
        let mut out = vec![
            value("gnss.l1.svs_searched", f64::from(self.svs_searched)),
            value("gnss.l1.svs_acquired", f64::from(self.svs_acquired)),
            value("gnss.l1.svs_reference", f64::from(self.svs_reference)),
            value("gnss.l1.power_rise_db", self.power_rise_db),
            value("gnss.l1.verdict", self.verdict.as_value()),
            value("gnss.l1.confidence", self.confidence),
        ];
        if let Some(cn0) = self.mean_cn0_dbhz {
            out.push(value("gnss.l1.mean_cn0_dbhz", cn0));
        }
        out
    }

    /// One line describing the measurement, for [`Cause::OwnHistory`].
    pub fn description(&self) -> String {
        let cn0 = match self.mean_cn0_dbhz {
            Some(c) => format!(", mean C/N0 {c:.1} dB-Hz"),
            None => String::new(),
        };
        format!(
            "gnss-l1 service {}: {}/{} satellites acquired (was {}), in-band power {:+.1} dB{}",
            self.verdict.label(),
            self.svs_acquired,
            self.svs_searched,
            self.svs_reference,
            self.power_rise_db,
            cn0,
        )
    }

    /// Whether this measurement speaks about `anomaly`: a correlated kind, overlapping the dwell's
    /// band, and open at the time of the dwell.
    ///
    /// The time test is one-sided on purpose — an anomaly that opened before the dwell and is
    /// still open covers it, and a *growing* anomaly's stored `t_end` trails the present, so an
    /// anomaly whose recorded end is earlier than the dwell still qualifies while its status is
    /// open (see [`attach_to_open_anomalies`], which checks the status).
    pub fn speaks_about(&self, anomaly: &Anomaly) -> bool {
        CORRELATED_KINDS.contains(&anomaly.kind)
            && overlaps(anomaly.region.freq, self.band)
            && anomaly.region.time.start <= self.t
    }

    /// Score of the explanation this evidence writes. It is the assessor's confidence, and it is
    /// **not** a claim to have identified an emitter — only that GNSS service here is degraded in
    /// a way consistent with the anomaly.
    fn score(&self) -> f64 {
        self.confidence.clamp(0.0, 1.0)
    }
}

fn overlaps(a: FreqRange, b: FreqRange) -> bool {
    a.lo_hz < b.hi_hz && b.lo_hz < a.hi_hz
}

/// Appends `ev` to `anomaly` as an [`Explanation`], superseding this rule's previous explanation
/// of the same anomaly.
///
/// Returns the explanation written, or `None` when the evidence does not speak about the anomaly
/// or the verdict is not notable. **Never inserts an anomaly, a detection or an emitter.**
pub fn attach(
    repo: &mut Repository,
    anomaly: &Anomaly,
    ev: &GnssServiceEvidence,
) -> Result<Option<Explanation>, RepoError> {
    if !ev.verdict.is_notable() || !ev.speaks_about(anomaly) {
        return Ok(None);
    }
    let supersedes = repo
        .explanations_for_anomaly(anomaly.id)?
        .into_iter()
        .filter(|e| e.rule_version == RULE_VERSION)
        .max_by_key(|e| e.t.as_unix_nanos())
        .map(|e| e.id);
    let mut evidence = vec![Evidence::History {
        region: Region {
            freq: ev.band,
            time: TimeRange::new(ev.t, ev.t),
        },
    }];
    evidence.extend(ev.values());
    let explanation = Explanation {
        id: ExplanationId::new(),
        anomaly_ref: anomaly.id,
        cause: Cause::OwnHistory {
            description: ev.description(),
        },
        correlation_type: CorrelationType::TimeCoincidence,
        score: ev.score(),
        evidence,
        supersedes,
        provisional: false,
        rule_version: RULE_VERSION.into(),
        t: ev.t,
    };
    repo.insert_explanation(&explanation)?;
    Ok(Some(explanation))
}

/// Attaches `ev` to every **open** anomaly of a correlated kind overlapping the dwell's band.
///
/// This is the whole C30 entry point for C36, and the shape is the point: it *queries* for
/// anomalies the blind path opened and adds evidence to them. There is no branch in which it
/// creates one. A dwell that finds nothing wrong, or finds trouble where the blind path saw
/// nothing, writes nothing.
pub fn attach_to_open_anomalies(
    repo: &mut Repository,
    ev: &GnssServiceEvidence,
    lookback: TimeRange,
) -> Result<Vec<Explanation>, RepoError> {
    if !ev.verdict.is_notable() {
        return Ok(Vec::new());
    }
    let anomalies = repo.anomalies_in_region(&Region {
        freq: ev.band,
        time: lookback,
    })?;
    let mut written = Vec::new();
    for a in anomalies {
        if !ev.speaks_about(&a) {
            continue;
        }
        let open = repo
            .anomaly_status_history(a.id)?
            .last()
            .is_some_and(|s| s.status == AnomalyStatus::Open);
        if !open {
            continue;
        }
        if let Some(e) = attach(repo, &a, ev)? {
            written.push(e);
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::{AnomalyId, AnomalyKind, AnomalySubject};

    fn t(ns: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_757_000_000_000_000_000 + ns)
    }

    fn band() -> FreqRange {
        FreqRange::new(L1_BAND_HZ.0, L1_BAND_HZ.1)
    }

    fn evidence(verdict: GnssServiceVerdict) -> GnssServiceEvidence {
        GnssServiceEvidence {
            t: t(2_000_000_000),
            band: band(),
            svs_searched: 32,
            svs_acquired: 0,
            svs_reference: 9,
            mean_cn0_dbhz: None,
            power_rise_db: 12.4,
            verdict,
            confidence: 0.8,
            reasons: vec!["all satellites lost with a floor rise".into()],
        }
    }

    fn open_anomaly(repo: &mut Repository, lo: f64, hi: f64) -> Anomaly {
        let a = Anomaly {
            id: AnomalyId::new(),
            kind: AnomalyKind::NoiseFloorRise,
            subject: AnomalySubject::Region,
            region: Region {
                freq: FreqRange::new(lo, hi),
                time: TimeRange::new(t(1_000_000_000), t(1_500_000_000)),
            },
            score: 0.5,
            detector_version: "test".into(),
            baseline_ref: Some("test".into()),
            t: t(1_000_000_000),
        };
        repo.insert_anomaly(&a).unwrap();
        a
    }

    fn repo() -> Repository {
        Repository::open_in_memory().unwrap()
    }

    #[test]
    fn a_degraded_dwell_explains_the_blind_anomaly_it_overlaps() {
        let mut repo = repo();
        let a = open_anomaly(&mut repo, 1_574.0e6, 1_577.0e6);
        let ev = evidence(GnssServiceVerdict::JammingSuspect);
        let e = attach(&mut repo, &a, &ev).unwrap().expect("an explanation");
        assert_eq!(e.rule_version, RULE_VERSION);
        assert!(matches!(e.cause, Cause::OwnHistory { .. }));
        let values: Vec<_> = e
            .evidence
            .iter()
            .filter_map(|x| match x {
                Evidence::Value { name, value } => Some((name.as_str(), *value)),
                _ => None,
            })
            .collect();
        assert!(
            values.contains(&("gnss.l1.svs_acquired", 0.0)),
            "{values:?}"
        );
        assert!(
            values.contains(&("gnss.l1.svs_searched", 32.0)),
            "{values:?}"
        );
        assert!(values.iter().any(|(n, _)| *n == "gnss.l1.power_rise_db"));
    }

    /// The load-bearing negative. A known-code correlation must not be able to enter the
    /// inventory, and the two shapes that would let it are an `Emitter` cause and a `Detection`
    /// evidence link. Neither is reachable from this module.
    #[test]
    fn the_evidence_never_names_a_detection_or_an_emitter() {
        let mut repo = repo();
        let a = open_anomaly(&mut repo, 1_574.0e6, 1_577.0e6);
        let ev = evidence(GnssServiceVerdict::JammingSuspect);
        let e = attach(&mut repo, &a, &ev).unwrap().unwrap();
        assert!(
            !matches!(e.cause, Cause::Emitter { .. }),
            "a GNSS acquisition may never name an emitter as a cause"
        );
        assert!(
            !e.evidence
                .iter()
                .any(|x| matches!(x, Evidence::Detection { .. })),
            "a GNSS acquisition may never be linked as a blind detection"
        );
    }

    /// The other load-bearing negative: evidence does not mint anomalies. The blind path opens
    /// them or nothing happens.
    #[test]
    fn a_dwell_never_opens_an_anomaly_of_its_own() {
        let mut repo = repo();
        let ev = evidence(GnssServiceVerdict::JammingSuspect);
        let window = TimeRange::new(t(0), t(3_000_000_000));
        let written = attach_to_open_anomalies(&mut repo, &ev, window).unwrap();
        assert!(written.is_empty(), "nothing to explain, nothing written");
        assert!(
            repo.anomalies_in_region(&Region {
                freq: band(),
                time: window,
            })
            .unwrap()
            .is_empty(),
            "the GNSS path must never insert an anomaly"
        );
    }

    #[test]
    fn working_gnss_explains_nothing() {
        let mut repo = repo();
        let a = open_anomaly(&mut repo, 1_574.0e6, 1_577.0e6);
        let mut ev = evidence(GnssServiceVerdict::Quiet);
        ev.svs_acquired = 9;
        assert!(attach(&mut repo, &a, &ev).unwrap().is_none());
    }

    #[test]
    fn an_anomaly_in_another_band_is_not_ours() {
        let mut repo = repo();
        let a = open_anomaly(&mut repo, 88.0e6, 108.0e6);
        let ev = evidence(GnssServiceVerdict::JammingSuspect);
        assert!(attach(&mut repo, &a, &ev).unwrap().is_none());
    }

    #[test]
    fn a_second_dwell_supersedes_the_first() {
        let mut repo = repo();
        let a = open_anomaly(&mut repo, 1_574.0e6, 1_577.0e6);
        let first = attach(&mut repo, &a, &evidence(GnssServiceVerdict::JammingSuspect))
            .unwrap()
            .unwrap();
        let mut later = evidence(GnssServiceVerdict::JammingSuspect);
        later.t = t(3_000_000_000);
        let second = attach(&mut repo, &a, &later).unwrap().unwrap();
        assert_eq!(second.supersedes, Some(first.id));
    }

    #[test]
    fn a_resolved_anomaly_is_left_alone() {
        let mut repo = repo();
        let a = open_anomaly(&mut repo, 1_574.0e6, 1_577.0e6);
        repo.append_anomaly_status(&hk_model::AnomalyStatusChange {
            anomaly_id: a.id,
            status: AnomalyStatus::Resolved,
            t: t(1_600_000_000),
            note: None,
        })
        .unwrap();
        let ev = evidence(GnssServiceVerdict::JammingSuspect);
        let written =
            attach_to_open_anomalies(&mut repo, &ev, TimeRange::new(t(0), t(3_000_000_000)))
                .unwrap();
        assert!(written.is_empty());
    }

    #[test]
    fn an_open_anomaly_in_band_gets_the_evidence() {
        let mut repo = repo();
        let a = open_anomaly(&mut repo, 1_574.0e6, 1_577.0e6);
        let ev = evidence(GnssServiceVerdict::JammingSuspect);
        let written =
            attach_to_open_anomalies(&mut repo, &ev, TimeRange::new(t(0), t(3_000_000_000)))
                .unwrap();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].anomaly_ref, a.id);
    }
}
