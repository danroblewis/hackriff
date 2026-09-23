//! Region watch (T-166, ADR-0013 §4.9 gap 9): the selection-scoped rule behind "Watch: alert on
//! new activity".
//!
//! A user arms a watch on a [`Selection`] ([`hk_model::SelectionWatch`]). From then on, every
//! emission the blind pipeline first sights inside that selection's extent is offered here, and
//! the rule decides whether it is **new activity worth telling the user about**.
//!
//! # The rule
//!
//! In order, and every step's answer is recorded:
//!
//! 1. **Extent.** The sighting's measured band must overlap the selection's, and — when the
//!    selection is bounded in time — fall inside its window. A watch is scoped to what the user
//!    drew, nothing wider.
//! 2. **Standing relationship (the gate that matters).** A row that currently defers to another
//!    row under the T-219 rules is **not new activity**, and never alerts:
//!    - [`RelationKind::SuppressedBy`] — it overlaps a Confirmed entry with nothing to tell them
//!      apart, so it is the signal the user already confirmed, seen again;
//!    - [`RelationKind::DuplicateOf`] — it is the weaker box of a duplicate group;
//!    - [`RelationKind::ArtifactOf`] — it is an image, harmonic or intermod product of a confirmed
//!      source: the receiver's own arithmetic, not something on the air;
//!    - [`RelationKind::RetuneSiblingOf`] (T-598) — it is the same LO-relative receiver artefact
//!      as another row, seen from a different tuning centre: the cross-centre retune test already
//!      resolved that family, and the row representing it is the one a watch may speak about.
//!
//!    This is the difference between a watch worth arming and one the user learns to ignore. A
//!    watch that fires on the front end's own images, or on a second box over a station already
//!    confirmed, is worse than no watch: it trains the user to dismiss it, and the one genuinely
//!    new emission is then lost in the noise it taught them to skip.
//! 3. **Already told.** One alert per emitter per watch. A row keeps being sighted; the user is
//!    told it arrived once.
//!
//! # What an alert is, and is not
//!
//! An alert is an `Anomaly` row plus an `Explanation` carrying the rule's reasoning in words, and
//! a message on the `anomalies` stream. That is all it is.
//!
//! - **It carries its reasoning.** Both answers do: [`WatchDecision::Alert`] says why this counts
//!   as new activity, and [`WatchSkip`] says which relationship stopped it, quoting that
//!   relationship's own recorded reason (an artifact claim's arithmetic included). Neither is ever
//!   a bare verdict.
//! - **It is reversible, and never an automatic action.** Raising an alert tunes nothing, records
//!   nothing, deletes nothing and changes no other row. The user dismisses or re-opens it like any
//!   other anomaly, and disarming the watch stops new alerts while keeping every alert already
//!   raised, with its reasoning and history intact.
//! - **A skip is disclosed, never silent.** Suppressed activity is counted and kept for the
//!   selection's watch report, the same stance ADR-0012 §7.3 takes towards alarm suppressions: the
//!   user can always see what the watch decided not to tell them, and why.
//!
//! The rule is pure — no I/O, no clock. The caller reads the relationships in force
//! (`Repository::emitter_relations`, which returns exactly the standing ones) and hands them in.

use std::collections::BTreeSet;

use hk_model::{
    ArtifactKind, EmitterId, EmitterRelation, FreqRange, RelationKind, Selection, SelectionId,
    TimeRange, Timestamp,
};

/// Rule id written to `Explanation::rule_version` and `Anomaly::detector_version`.
pub const RULE_VERSION: &str = "hk-context.region-watch@1";

/// Prefix of the `Anomaly::baseline_ref` key a watch alert carries.
pub const KEY_PREFIX: &str = "region-watch:v1";

/// The `Anomaly::baseline_ref` of a watch alert: `region-watch:v1;selection=<uuid>;emitter=<uuid>`.
///
/// It is parseable ([`parse_baseline_ref`]), so a selection's alerts can be found again without a
/// second table, and it is visibly **not** a `c12-alarm:v1` baseline reference — a watch alert has
/// no baseline and claims none.
pub fn baseline_ref(selection: SelectionId, emitter: EmitterId) -> String {
    format!("{KEY_PREFIX};selection={selection};emitter={emitter}")
}

/// Parses [`baseline_ref`]; `None` for any other key (a floor episode, a C12 alarm).
pub fn parse_baseline_ref(s: &str) -> Option<(SelectionId, EmitterId)> {
    let mut parts = s.split(';');
    if parts.next()? != KEY_PREFIX {
        return None;
    }
    let (mut selection, mut emitter) = (None, None);
    for p in parts {
        let (k, v) = p.split_once('=')?;
        match k {
            "selection" => selection = v.parse::<SelectionId>().ok(),
            "emitter" => emitter = v.parse::<EmitterId>().ok(),
            _ => return None,
        }
    }
    Some((selection?, emitter?))
}

/// One armed watch: a selection's extent and identity, as the rule needs them.
#[derive(Clone, Debug, PartialEq)]
pub struct WatchRegion {
    /// The selection.
    pub selection: SelectionId,
    /// Its name, for the reasoning.
    pub name: String,
    /// The watched band.
    pub freq: FreqRange,
    /// The watched window; `None` = any time.
    pub time: Option<TimeRange>,
}

impl WatchRegion {
    /// The watch armed on `s`, or `None` when it has none or it is disarmed.
    pub fn armed(s: &Selection) -> Option<Self> {
        s.watching().then(|| Self {
            selection: s.id,
            name: s.name.clone(),
            freq: FreqRange::new(s.f_lo_hz, s.f_hi_hz),
            time: match (s.t_lo, s.t_hi) {
                (Some(lo), Some(hi)) => Some(TimeRange::new(lo, hi)),
                _ => None,
            },
        })
    }

    /// Whether `freq` and `t` lie in this watch's extent.
    pub fn covers(&self, freq: FreqRange, t: Timestamp) -> bool {
        self.freq.overlaps(&freq) && self.time.is_none_or(|w| w.start <= t && t <= w.end)
    }
}

/// Every armed watch among `selections`.
pub fn armed_regions(selections: &[Selection]) -> Vec<WatchRegion> {
    selections.iter().filter_map(WatchRegion::armed).collect()
}

/// One relationship currently in force on a row (T-219): what it defers to, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct StandingRelation {
    /// Which claim.
    pub kind: RelationKind,
    /// The mechanism, for [`RelationKind::ArtifactOf`].
    pub artifact: Option<ArtifactKind>,
    /// The row it defers to.
    pub source: EmitterId,
    /// The relationship's own recorded reasoning (an artifact claim's arithmetic included).
    pub reason: String,
}

impl StandingRelation {
    /// The relationship, when `r` is in force; `None` for a revoked row (so a caller may pass a
    /// full history and still get only what stands).
    pub fn from_relation(r: &EmitterRelation) -> Option<Self> {
        r.active.then(|| Self {
            kind: r.kind,
            artifact: r.artifact,
            source: r.source_id,
            reason: r.reason.clone(),
        })
    }

    /// Every standing relationship among `relations`.
    pub fn standing(relations: &[EmitterRelation]) -> Vec<Self> {
        relations.iter().filter_map(Self::from_relation).collect()
    }

    /// How the claim reads in a sentence.
    pub fn label(&self) -> String {
        match (self.kind, self.artifact) {
            (RelationKind::SuppressedBy, _) => {
                "suppressed by a confirmed entry it overlaps, with nothing to tell them apart"
                    .into()
            }
            (RelationKind::DuplicateOf, _) => "the weaker box of a duplicate group".into(),
            (RelationKind::ArtifactOf, Some(k)) => format!(
                "a receiver artifact ({}) of a confirmed source, not an emission on the air",
                k.as_str()
            ),
            (RelationKind::ArtifactOf, None) => {
                "a receiver artifact of a confirmed source, not an emission on the air".into()
            }
            // T-598: the cross-centre retune test found this sighting and another on one
            // LO-relative invariant coordinate, so they are one receiver artefact seen from two
            // centres. The row it names is the sighting that represents the family.
            (RelationKind::RetuneSiblingOf, _) => {
                "the same LO-relative receiver artefact as another row, seen from a different \
                 tuning centre, so not an emission on the air"
                    .into()
            }
            // T-222: the same content as another row, arriving later and weaker over a second
            // path. Already-known activity, not a new emission, so a watch does not alert on it.
            (RelationKind::MultipathOf, _) => {
                "the same emission as another row, arriving later and weaker over a second path"
                    .into()
            }
        }
    }
}

/// The first relationship in force, if any. Any one of them means the row is not new activity, so
/// the first is the one reported.
pub fn deferring_relation(relations: &[StandingRelation]) -> Option<&StandingRelation> {
    relations.first()
}

/// One emission the pipeline first sighted, offered to the watches.
#[derive(Clone, Debug, PartialEq)]
pub struct WatchActivity {
    /// The inventory row.
    pub emitter: EmitterId,
    /// Its measured extent.
    pub freq: FreqRange,
    /// When it was counted (sample clock).
    pub t: Timestamp,
    /// The T-219 relationships in force on it. Empty is the normal case.
    pub relations: Vec<StandingRelation>,
}

/// Why a sighting did not alert.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WatchSkipReason {
    /// Outside the watched band or window.
    OutsideExtent,
    /// It defers to another row under T-219: not new activity.
    Deferred,
    /// This watch already told the user about this emitter.
    AlreadyAlerted,
}

impl WatchSkipReason {
    /// Stable kebab-case name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OutsideExtent => "outside-extent",
            Self::Deferred => "deferred",
            Self::AlreadyAlerted => "already-alerted",
        }
    }
}

/// A sighting the watch did not alert on, with its reasoning.
#[derive(Clone, Debug, PartialEq)]
pub struct WatchSkip {
    /// Why.
    pub reason: WatchSkipReason,
    /// The relationship that stopped it, for [`WatchSkipReason::Deferred`].
    pub relation: Option<StandingRelation>,
    /// The reasoning, rendered.
    pub text: String,
}

/// What the rule decided about one sighting under one watch.
#[derive(Clone, Debug, PartialEq)]
pub enum WatchDecision {
    /// New activity: raise an alert, with this reasoning.
    Alert(String),
    /// Not new activity, with this reasoning.
    Skip(WatchSkip),
}

impl WatchDecision {
    /// The reasoning either way. Never empty: an alert and a skip both say why.
    pub fn reason(&self) -> &str {
        match self {
            Self::Alert(text) => text,
            Self::Skip(s) => &s.text,
        }
    }

    /// Whether this decision alerts.
    pub fn alerts(&self) -> bool {
        matches!(self, Self::Alert(_))
    }
}

fn mhz(hz: f64) -> String {
    format!("{:.6} MHz", hz / 1e6)
}

fn khz(hz: f64) -> String {
    format!("{:.3} kHz", hz / 1e3)
}

/// The rule (module docs): whether `activity` is new activity under `region`, given the emitters
/// this watch has already alerted on.
///
/// The steps are ordered so the reasoning names the most informative fact: the extent first
/// (outside it, the watch has no opinion at all), then the T-219 relationship, then the
/// one-alert-per-emitter rule.
pub fn evaluate(
    region: &WatchRegion,
    activity: &WatchActivity,
    already_alerted: &BTreeSet<EmitterId>,
) -> WatchDecision {
    let who = format!(
        "emitter {} at {} (width {})",
        activity.emitter,
        mhz(activity.freq.center_hz()),
        khz(activity.freq.width_hz()),
    );
    let watched = format!(
        "watched selection {:?} ({}-{})",
        region.name,
        mhz(region.freq.lo_hz),
        mhz(region.freq.hi_hz),
    );

    if !region.covers(activity.freq, activity.t) {
        return WatchDecision::Skip(WatchSkip {
            reason: WatchSkipReason::OutsideExtent,
            relation: None,
            text: format!("{who} is outside the {watched}"),
        });
    }

    // The T-219 gate. A row that defers to another row is that other row seen again, or the
    // receiver's own arithmetic — either way it is not new activity, however new the id is.
    if let Some(rel) = deferring_relation(&activity.relations) {
        return WatchDecision::Skip(WatchSkip {
            reason: WatchSkipReason::Deferred,
            relation: Some(rel.clone()),
            text: format!(
                "not new activity in the {watched}: {who} is {} ({}), recorded against {} - {}",
                rel.label(),
                rel.kind.as_str(),
                rel.source,
                rel.reason,
            ),
        });
    }

    if already_alerted.contains(&activity.emitter) {
        return WatchDecision::Skip(WatchSkip {
            reason: WatchSkipReason::AlreadyAlerted,
            relation: None,
            text: format!("{who} already raised an alert on the {watched}"),
        });
    }

    WatchDecision::Alert(format!(
        "new activity in the {watched}: {who} was first sighted inside it, and no standing \
         suppression, duplicate or receiver-artifact relationship explains it as another row",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::{RelationAuthor, Timestamp};

    fn eid(n: u8) -> EmitterId {
        EmitterId::from_uuid(uuid::Uuid::from_bytes([n; 16]))
    }

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    fn watched() -> WatchRegion {
        WatchRegion {
            selection: SelectionId::new(),
            name: "FM band".into(),
            freq: FreqRange::new(99.6e6, 102.0e6),
            time: None,
        }
    }

    fn activity(f_center: f64, bw: f64) -> WatchActivity {
        WatchActivity {
            emitter: eid(7),
            freq: FreqRange::centered(f_center, bw),
            t: t(100),
            relations: Vec::new(),
        }
    }

    fn relation(kind: RelationKind, artifact: Option<ArtifactKind>) -> StandingRelation {
        StandingRelation {
            kind,
            artifact,
            source: eid(9),
            reason: "the recorded reasoning".into(),
        }
    }

    #[test]
    fn a_selection_is_watched_only_when_armed() {
        let mut s = Selection::new("FM band", 99.6e6, 102.0e6);
        assert!(WatchRegion::armed(&s).is_none(), "no watch by default");
        s.watch = Some(hk_model::SelectionWatch { enabled: false });
        assert!(WatchRegion::armed(&s).is_none(), "disarmed");
        s.watch = Some(hk_model::SelectionWatch { enabled: true });
        let r = WatchRegion::armed(&s).expect("armed");
        assert_eq!((r.freq.lo_hz, r.freq.hi_hz), (99.6e6, 102.0e6));
        assert_eq!(r.time, None, "unbounded in time");
    }

    #[test]
    fn a_watch_is_scoped_to_its_selections_band_and_window() {
        let mut region = watched();
        assert!(evaluate(&region, &activity(100.3e6, 180e3), &BTreeSet::new()).alerts());
        // Outside the band.
        let out = evaluate(&region, &activity(88.5e6, 180e3), &BTreeSet::new());
        assert!(matches!(
            out,
            WatchDecision::Skip(WatchSkip {
                reason: WatchSkipReason::OutsideExtent,
                ..
            })
        ));
        // Inside the band but outside a bounded window.
        region.time = Some(TimeRange::new(t(0), t(50)));
        let late = evaluate(&region, &activity(100.3e6, 180e3), &BTreeSet::new());
        assert!(matches!(
            late,
            WatchDecision::Skip(WatchSkip {
                reason: WatchSkipReason::OutsideExtent,
                ..
            })
        ));
    }

    /// The heart of T-166: a row the relationship rules record as deferring to another row is
    /// never new activity, whichever claim it is — and the skip quotes that claim's own reasoning
    /// rather than just refusing.
    #[test]
    fn a_suppressed_duplicate_or_attributed_artifact_never_alerts() {
        let region = watched();
        for (kind, artifact) in [
            (RelationKind::SuppressedBy, None),
            (RelationKind::DuplicateOf, None),
            (RelationKind::ArtifactOf, Some(ArtifactKind::Image)),
            (RelationKind::ArtifactOf, Some(ArtifactKind::Harmonic)),
            (RelationKind::ArtifactOf, Some(ArtifactKind::Intermod)),
            (RelationKind::RetuneSiblingOf, None),
        ] {
            let mut a = activity(100.3e6, 180e3);
            a.relations = vec![relation(kind, artifact)];
            let d = evaluate(&region, &a, &BTreeSet::new());
            let WatchDecision::Skip(skip) = &d else {
                panic!("{kind:?}/{artifact:?} alerted as new activity: {d:?}");
            };
            assert_eq!(skip.reason, WatchSkipReason::Deferred);
            assert_eq!(skip.relation.as_ref().map(|r| r.kind), Some(kind));
            assert!(
                skip.text.contains("not new activity")
                    && skip.text.contains("the recorded reasoning")
                    && skip.text.contains(kind.as_str()),
                "the skip must carry the relationship's own reasoning: {}",
                skip.text
            );
        }
    }

    /// A revoked relationship is not a standing one: the row is an independent emission again, and
    /// a watch over it alerts. Relationships are reversible, and so is what the watch makes of one.
    #[test]
    fn a_revoked_relationship_no_longer_stops_an_alert() {
        let rows = |active: bool| EmitterRelation {
            relation_id: 1,
            emitter_id: eid(7),
            source_id: eid(9),
            kind: RelationKind::ArtifactOf,
            artifact: Some(ArtifactKind::Image),
            active,
            t: t(1),
            author: RelationAuthor::System,
            actor: "hk-pipeline/overlap@1".into(),
            reason: "image arithmetic".into(),
            score: None,
            detail: None,
        };
        let region = watched();
        let mut a = activity(100.3e6, 180e3);

        a.relations = StandingRelation::standing(&[rows(true)]);
        assert!(
            !evaluate(&region, &a, &BTreeSet::new()).alerts(),
            "in force"
        );

        a.relations = StandingRelation::standing(&[rows(false)]);
        assert!(a.relations.is_empty(), "a revoked claim does not stand");
        assert!(evaluate(&region, &a, &BTreeSet::new()).alerts(), "revoked");
    }

    #[test]
    fn one_alert_per_emitter_per_watch() {
        let region = watched();
        let a = activity(100.3e6, 180e3);
        assert!(evaluate(&region, &a, &BTreeSet::new()).alerts());
        let already = BTreeSet::from([a.emitter]);
        let d = evaluate(&region, &a, &already);
        assert!(matches!(
            d,
            WatchDecision::Skip(WatchSkip {
                reason: WatchSkipReason::AlreadyAlerted,
                ..
            })
        ));
    }

    /// Every decision says why, alert or skip. An alert with no reasoning is exactly the kind of
    /// bare verdict this system never issues.
    #[test]
    fn every_decision_carries_its_reasoning() {
        let region = watched();
        let mut cases = vec![
            evaluate(&region, &activity(100.3e6, 180e3), &BTreeSet::new()),
            evaluate(&region, &activity(88.5e6, 180e3), &BTreeSet::new()),
        ];
        let mut deferred = activity(100.3e6, 180e3);
        deferred.relations = vec![relation(RelationKind::SuppressedBy, None)];
        cases.push(evaluate(&region, &deferred, &BTreeSet::new()));
        for d in &cases {
            assert!(
                d.reason().len() > 40 && d.reason().contains("FM band"),
                "thin reasoning: {:?}",
                d.reason()
            );
        }
    }

    #[test]
    fn the_alert_key_round_trips_and_is_not_a_baseline_reference() {
        let (s, e) = (SelectionId::new(), eid(3));
        let key = baseline_ref(s, e);
        assert_eq!(parse_baseline_ref(&key), Some((s, e)));
        assert!(parse_baseline_ref("c12-alarm:v1;kind=new-emitter").is_none());
        assert!(parse_baseline_ref("floor-episode:v1;run=a").is_none());
    }
}
