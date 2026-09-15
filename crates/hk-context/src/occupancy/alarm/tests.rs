//! T-122 alarm tests on synthetic novelty snapshots (the truth lives only in the asserts).

use hk_model::attention::baseline::SiteKey;
use hk_model::attention::score::NoveltyScore;
use hk_model::ids::{EmitterId, SiteId};
use hk_model::{AnomalyKind, Cause};

use super::*;
use crate::feeds::FeedAdapter;
use crate::feeds::gpsjam::GpsjamAdapter;
use crate::occupancy::baseline::PoolStats;
use crate::utc::parse_utc;

const CELL_HZ: f64 = 6250.0;
const INTERVAL_S: f64 = 900.0;

fn t0() -> Timestamp {
    parse_utc("2026-09-13T12:00:00Z").unwrap()
}

fn at(i: i64) -> Timestamp {
    add_s(t0(), i as f64 * INTERVAL_S)
}

fn mature() -> Maturity {
    Maturity::Mature {
        resolution: BaselineResolution::AllHours,
    }
}

/// An input on baseline cell `cell` (16 level-0 cells) at `f0` + cell × 100 kHz.
fn input(kind: AlarmKind, f0_hz: f64, cell: i64, novelty: f64) -> AlarmInput {
    let base = (f0_hz / CELL_HZ).round() as i64;
    let (lo, hi) = (base + cell * 16, base + (cell + 1) * 16);
    let (unit, observed, mean, spread) = match kind {
        AlarmKind::LevelAboveBaseline | AlarmKind::ChangePoint => {
            (AlarmUnit::Db, -60.0, -80.0, 1.5)
        }
        AlarmKind::NewEmitter => (AlarmUnit::Count, 1.0, 0.01, 0.1),
        _ => (AlarmUnit::Fraction, 0.6, 0.02, 0.03),
    };
    AlarmInput {
        kind,
        subject: AlarmSubject::Cells {
            scheme: 1,
            lo_cell: lo,
            hi_cell: hi,
        },
        freq: FreqRange::new(lo as f64 * CELL_HZ, hi as f64 * CELL_HZ),
        cal: CalKey::Uncalibrated,
        resolution: BaselineResolution::AllHours,
        slot: HourOfWeek::of(t0(), 0),
        maturity: mature(),
        provenance_explained: false,
        unit,
        observed,
        baseline_mean: mean,
        baseline_spread: spread,
        z: 3.0 + 7.0 * novelty,
        novelty,
        observed_s: INTERVAL_S,
        gain: 0,
        sequential: None,
    }
}

fn snap(site: SiteKey, i: i64, inputs: Vec<AlarmInput>) -> NoveltySnapshot {
    NoveltySnapshot {
        t: at(i),
        site,
        steps: Vec::new(),
        inputs,
    }
}

fn raises(actions: &[AlarmAction]) -> usize {
    actions
        .iter()
        .filter(|a| matches!(a, AlarmAction::Raise { .. }))
        .count()
}

struct Rig {
    repo: Repository,
    engine: AlarmEngine,
    writer: AlarmWriter,
    site: SiteKey,
    events: Vec<AlarmEvent>,
}

impl Rig {
    fn new() -> Self {
        Self {
            repo: Repository::open_in_memory().unwrap(),
            engine: AlarmEngine::new(AlarmConfig::default()).unwrap(),
            writer: AlarmWriter::default(),
            site: SiteKey::Site(SiteId::new()),
            events: Vec::new(),
        }
    }

    fn run(&mut self, s: &NoveltySnapshot, geo: Option<&Site>) -> Vec<AlarmAction> {
        let actions = self.engine.step(s);
        let ev = self
            .writer
            .apply(&mut self.repo, &actions, s.t, &BTreeMap::new(), geo)
            .unwrap();
        self.events.extend(ev);
        actions
    }

    fn alarms(&self) -> Vec<hk_model::repo::alarms::AnomalyListing> {
        self.repo
            .anomalies_page(&hk_model::repo::alarms::AnomalyQuery {
                limit: 100,
                ..Default::default()
            })
            .unwrap()
            .rows
    }
}

/// AWARE-044: an emitter appearing in a quiet channel raises exactly one alarm, explained
/// `unexplained` (no cached event fits), while a GNSS-band level rise during a cached GPS-jamming
/// event ranks the event above `unexplained`.
#[test]
fn alarm_injected_emitter_raises_exactly_one_with_ranked_explanations() {
    let mut rig = Rig::new();
    let geo = Site::new(52.2, 0.12);
    let parsed = GpsjamAdapter::default()
        .parse(
            "2026-09-13",
            "hex,count_good_aircraft,count_bad_aircraft\n84194edffffffff,40,12\n",
            parse_utc("2026-09-14T01:00:00Z").unwrap(),
        )
        .unwrap();
    for e in &parsed.events {
        rig.repo.upsert_external_event(e).unwrap();
    }
    let mut total = 0;
    for i in 0..12 {
        // Hidden truth: the ISM emitter switches on at interval 4 in cells 3–4; the L1 rise at 6.
        let on = |from: i64| if i >= from { 1.0 } else { 0.05 };
        let inputs = vec![
            input(AlarmKind::BusierThanUsual, 433.0e6, 3, on(4)),
            input(AlarmKind::BusierThanUsual, 433.0e6, 4, on(4)),
            input(AlarmKind::BusierThanUsual, 433.0e6, 9, 0.05),
            input(AlarmKind::LevelAboveBaseline, 1575.0e6, 0, on(6)),
        ];
        total += raises(&rig.run(&snap(rig.site, i, inputs), Some(&geo)));
    }
    assert_eq!(total, 2, "one ISM alarm (merged cells) + one L1 alarm");
    let rows = rig.alarms();
    assert_eq!(rows.len(), 2);
    let ism = rows
        .iter()
        .find(|r| r.anomaly.kind == AnomalyKind::BusierThanBaseline)
        .unwrap();
    assert_eq!(ism.status, AnomalyStatus::Open);
    let alarm = ism.alarm.as_ref().unwrap();
    assert!(
        matches!(alarm.key.subject, AlarmSubject::Cells { lo_cell, hi_cell, .. } if hi_cell - lo_cell == 32),
        "adjacent cells merged: {:?}",
        alarm.key
    );
    assert_eq!(
        alarm.detail.stages_applied,
        ExplanationStage::ORDER.to_vec()
    );
    alarm.detail.validate().unwrap();
    let parsed_ref =
        AlarmKey::parse_baseline_ref(ism.anomaly.baseline_ref.as_deref().unwrap()).unwrap();
    assert_eq!(parsed_ref.key, alarm.key);
    let ex = latest_explanations(&rig.repo, ism.anomaly.id).unwrap();
    assert_eq!(ex[0].cause, Cause::Unexplained);
    assert_eq!(ex[0].score, 1.0);

    let l1 = rows
        .iter()
        .find(|r| r.anomaly.kind == AnomalyKind::LevelAboveBaseline)
        .unwrap();
    let ex = latest_explanations(&rig.repo, l1.anomaly.id).unwrap();
    assert!(
        matches!(ex[0].cause, Cause::ExternalEvent { .. }),
        "the jamming event ranks first: {ex:?}"
    );
    assert_eq!(ex.last().unwrap().cause, Cause::Unexplained);
    assert!(ex[0].score > ex.last().unwrap().score);
    // Stream events: a raise per alarm then holds.
    assert_eq!(
        rig.events
            .iter()
            .filter(|e| e.transition == AlarmLifecycle::Raised)
            .count(),
        2
    );
}

/// Novelty flickering around `on` never raises; once open, dipping between `off` and `on` holds.
///
/// T-146: exercised on a level kind. The hysteresis is kind-independent, but a busier/quieter
/// input now also accumulates evidence, and this fixture's z (3 + 7·novelty ≥ 5.45 for 20
/// consecutive intervals) is overwhelming sequential evidence that rightly raises.
#[test]
fn alarm_hysteresis_does_not_flap() {
    let mut rig = Rig::new();
    let mut acts = Vec::new();
    for i in 0..20 {
        let n = if i % 2 == 0 { 0.75 } else { 0.35 };
        acts.extend(rig.run(
            &snap(
                rig.site,
                i,
                vec![input(AlarmKind::LevelAboveBaseline, 1e8, 0, n)],
            ),
            None,
        ));
    }
    assert_eq!(raises(&acts), 0, "flicker across on never raises");
    for i in 20..40 {
        let n = match i {
            20 | 21 => 0.9,
            _ if i % 2 == 0 => 0.35,
            _ => 0.55,
        };
        acts.extend(rig.run(
            &snap(
                rig.site,
                i,
                vec![input(AlarmKind::LevelAboveBaseline, 1e8, 0, n)],
            ),
            None,
        ));
    }
    assert_eq!(raises(&acts), 1);
    assert!(
        !acts.iter().any(|a| matches!(a, AlarmAction::Clear { .. })),
        "a dip below off that recovers above off does not clear"
    );
    assert_eq!(rig.alarms().len(), 1);
}

/// Clear then re-raise within 1 h (sample clock) reopens the same anomaly.
#[test]
fn alarm_reraise_within_cooldown_reopens_same_anomaly() {
    let mut rig = Rig::new();
    let series = [0.9, 0.9, 0.1, 0.1, 0.1, 0.9, 0.9];
    let mut acts = Vec::new();
    for (i, n) in series.iter().enumerate() {
        acts.extend(rig.run(
            &snap(
                rig.site,
                i as i64,
                vec![input(AlarmKind::LevelAboveBaseline, 1e8, 0, *n)],
            ),
            None,
        ));
    }
    let raised = acts
        .iter()
        .find_map(|a| match a {
            AlarmAction::Raise { anomaly, .. } => Some(*anomaly),
            _ => None,
        })
        .unwrap();
    assert!(
        acts.iter()
            .any(|a| matches!(a, AlarmAction::Reopen { anomaly, .. } if *anomaly == raised))
    );
    let rows = rig.alarms();
    assert_eq!(rows.len(), 1, "no second row");
    let alarm = rows[0].alarm.as_ref().unwrap();
    assert_eq!((alarm.state, alarm.reopen_count), (AlarmState::Open, 1));
    let notes: Vec<_> = rig
        .repo
        .anomaly_status_history(raised)
        .unwrap()
        .into_iter()
        .map(|s| (s.status, s.note.unwrap_or_default()))
        .collect();
    assert_eq!(
        notes,
        vec![
            (AnomalyStatus::Open, "raised".into()),
            (AnomalyStatus::Resolved, "cleared".into()),
            (AnomalyStatus::Open, "reopened".into()),
        ]
    );

    // After the cooldown a re-raise is a new alarm; resume keeps the state.
    let mut resumed = AlarmEngine::resume(AlarmConfig::default(), &rig.repo).unwrap();
    assert_eq!(resumed.open_alarms().len(), 1);
    for i in 7..10 {
        let s = snap(
            rig.site,
            i,
            vec![input(AlarmKind::LevelAboveBaseline, 1e8, 0, 0.0)],
        );
        rig.writer
            .apply(
                &mut rig.repo,
                &resumed.step(&s),
                s.t,
                &BTreeMap::new(),
                None,
            )
            .unwrap();
    }
    assert!(resumed.open_alarms().is_empty());
    let mut late = Vec::new();
    for i in 20..22 {
        late.extend(resumed.step(&snap(
            rig.site,
            i,
            vec![input(AlarmKind::LevelAboveBaseline, 1e8, 0, 0.9)],
        )));
    }
    assert!(
        late.iter()
            .any(|a| matches!(a, AlarmAction::Raise { anomaly, .. } if *anomaly != raised))
    );
}

/// A front-end gain step explains a broadband level shift of the same size: a self-inflicted
/// anomaly, never a novelty alarm. A step that does not fit the change explains nothing.
#[test]
fn alarm_gain_step_is_provenance_not_novelty() {
    let mut rig = Rig::new();
    let step = |delta: f64| DeviceStep {
        t: add_s(at(2), -60.0),
        kind: ProvenanceStepKind::Gain,
        freq: None,
        gain_delta_db: Some(delta),
        detail: format!("lna {delta:+} dB"),
    };
    let mut acts = Vec::new();
    for i in 0..8 {
        let n = if i >= 2 { 1.0 } else { 0.0 };
        let mut s = snap(
            rig.site,
            i,
            (0..6)
                .map(|c| {
                    let mut x = input(AlarmKind::LevelAboveBaseline, 2e8, c * 4, n);
                    // Past the lookback, T-119's per-gain-state split flags the new state.
                    x.provenance_explained = i >= 3;
                    x
                })
                .collect(),
        );
        s.steps = vec![step(20.0)];
        acts.extend(rig.run(&s, None));
    }
    assert_eq!(raises(&acts), 0, "no novelty alarm for a gain step");
    let explained: Vec<_> = acts
        .iter()
        .filter(|a| matches!(a, AlarmAction::Explained { .. }))
        .collect();
    assert_eq!(
        explained.len(),
        6,
        "one self-inflicted anomaly per subject, once"
    );
    assert_eq!(
        rig.engine
            .suppressions()
            .total(Suppression::ProvenanceExplained),
        6 * 6
    );
    let rows = rig.alarms();
    assert!(rows.iter().all(|r| r.status == AnomalyStatus::Resolved
        && r.alarm.as_ref().unwrap().state == AlarmState::Explained));
    let ex = latest_explanations(&rig.repo, rows[0].anomaly.id).unwrap();
    assert!(
        matches!(&ex[0].cause, Cause::SelfInflicted { reason } if reason.starts_with("gain")),
        "{ex:?}"
    );

    // The same shift with a −20 dB step (inconsistent direction) is novel.
    let mut other = Rig::new();
    let mut acts = Vec::new();
    for i in 0..4 {
        let mut s = snap(
            other.site,
            i,
            vec![input(AlarmKind::LevelAboveBaseline, 2e8, 0, 1.0)],
        );
        s.steps = vec![step(-20.0)];
        acts.extend(other.run(&s, None));
    }
    assert_eq!(raises(&acts), 1);
}

/// A usual transmitter going silent (negative occupancy z from a T-119 fold) raises
/// quieter-than-usual.
#[test]
fn alarm_silenced_transmitter_raises_quieter_than_usual() {
    let mut rig = Rig::new();
    let obs = IntervalObservation {
        subject: BaselineSubject::Cell { index: 1500 },
        t: at(0),
        gain: 0,
        level_db: None,
        max_db: None,
        occupied_weight_s: 0.0,
        weight_s: 900.0,
        observed_s: 900.0,
        n_eff: 60.0,
        suspect_fraction: 0.0,
        provenance_explained: false,
    };
    let mut acts = Vec::new();
    for i in 0..4 {
        let fold = FoldOutcome {
            novelty: NoveltyScore {
                novelty: 1.0,
                level_z: None,
                occupancy_z: Some(-25.0),
                new_emitter: None,
                observed_s: 900.0,
                maturity: mature(),
                provenance_explained: false,
            },
            change_point: None,
            accrued: true,
            accrued_reference: true,
        };
        let o = IntervalObservation { t: at(i), ..obs };
        let inputs = inputs_from_fold(
            &o,
            &fold,
            CalKey::Uncalibrated,
            HourOfWeek::of(o.t, 0),
            1,
            CELL_HZ,
            16,
            PoolContext {
                resolution: None,
                level: None,
                occupancy: Some((0.95, 0.03)),
            },
            &NoveltyConfig::default(),
        );
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].kind, AlarmKind::QuieterThanUsual);
        acts.extend(rig.run(&snap(rig.site, i, inputs), None));
    }
    assert_eq!(raises(&acts), 1);
    let rows = rig.alarms();
    assert_eq!(rows[0].anomaly.kind, AnomalyKind::QuieterThanBaseline);
    let d = &rows[0].alarm.as_ref().unwrap().detail;
    assert_eq!((d.observed, d.baseline_mean), (0.0, 0.95));
}

/// T-131 review: an immature fold with evidence yields no scored input but is counted as one
/// `immature-baseline` suppression per kind (a mobile site as `mobile-site`), never raised; a
/// mature fold has no unscored evidence.
#[test]
fn alarm_immature_evidence_counted_never_raised() {
    let mut rig = Rig::new();
    let obs = IntervalObservation {
        subject: BaselineSubject::Cell { index: 1500 },
        t: at(0),
        gain: 0,
        level_db: Some(-40.0),
        max_db: Some(-35.0),
        occupied_weight_s: 900.0,
        weight_s: 900.0,
        observed_s: 900.0,
        n_eff: 60.0,
        suspect_fraction: 0.0,
        provenance_explained: false,
    };
    let fold = |maturity| FoldOutcome {
        novelty: NoveltyScore {
            novelty: 0.0,
            level_z: None,
            occupancy_z: None,
            new_emitter: None,
            observed_s: 900.0,
            maturity,
            provenance_explained: false,
        },
        change_point: None,
        accrued: true,
        accrued_reference: true,
    };
    let immature = fold(Maturity::Immature { observed_s: 900.0 });
    let mut acts = Vec::new();
    for i in 0..8 {
        let o = IntervalObservation { t: at(i), ..obs };
        let inputs = inputs_from_fold(
            &o,
            &immature,
            CalKey::Uncalibrated,
            HourOfWeek::of(o.t, 0),
            1,
            CELL_HZ,
            16,
            PoolContext::default(),
            &NoveltyConfig::default(),
        );
        assert!(inputs.is_empty(), "no z against an immature pool");
        let kinds = unscored_evidence(&o, &immature);
        assert_eq!(
            kinds,
            [AlarmKind::LevelAboveBaseline, AlarmKind::BusierThanUsual]
        );
        rig.engine
            .count_unscored(o.t, rig.site, immature.novelty.maturity, false, &kinds);
        acts.extend(rig.run(&snap(rig.site, i, inputs), None));
    }
    assert!(acts.is_empty(), "{acts:?}");
    assert!(rig.alarms().is_empty());
    let c = rig.engine.suppressions();
    assert_eq!(
        c.get(AlarmKind::LevelAboveBaseline, Suppression::ImmatureBaseline),
        8
    );
    assert_eq!(
        c.get(AlarmKind::BusierThanUsual, Suppression::ImmatureBaseline),
        8
    );
    rig.engine.count_unscored(
        at(9),
        SiteKey::Mobile,
        immature.novelty.maturity,
        false,
        &[AlarmKind::BusierThanUsual],
    );
    assert_eq!(rig.engine.suppressions().total(Suppression::MobileSite), 1);
    assert!(unscored_evidence(&obs, &fold(mature())).is_empty());
}

/// New-emitter novelty (synthetic; T-128 wires `FirstSightingRate`) raises on the emitter subject.
#[test]
fn alarm_new_emitter_path() {
    let mut rig = Rig::new();
    let id = EmitterId::new();
    let mut acts = Vec::new();
    for i in 0..3 {
        let inp = new_emitter_input(
            id,
            FreqRange::new(868.0e6, 868.1e6),
            CalKey::Uncalibrated,
            HourOfWeek::of(at(i), 0),
            mature(),
            1,
            0.001,
            0.95,
            900.0,
        );
        acts.extend(rig.run(&snap(rig.site, i, vec![inp]), None));
    }
    assert_eq!(raises(&acts), 1);
    let rows = rig.alarms();
    assert_eq!(rows[0].anomaly.kind, AnomalyKind::NewEmitter);
    assert_eq!(rows[0].anomaly.subject, AnomalySubject::Emitter(id));
}

/// Mobile, unassigned and immature produce no alarms, and the suppressions are counted.
#[test]
fn alarm_suppressed_when_mobile_unassigned_or_immature() {
    let mut rig = Rig::new();
    let mut acts = Vec::new();
    for i in 0..6 {
        let hot = vec![input(AlarmKind::BusierThanUsual, 1e8, 0, 1.0)];
        acts.extend(rig.run(&snap(SiteKey::Mobile, i, hot.clone()), None));
        acts.extend(rig.run(&snap(SiteKey::Unassigned, i, hot.clone()), None));
        let mut immature = hot;
        immature[0].maturity = Maturity::Immature { observed_s: 3600.0 };
        acts.extend(rig.run(&snap(rig.site, i, immature), None));
    }
    assert!(acts.is_empty(), "{acts:?}");
    assert!(rig.alarms().is_empty());
    let c = rig.engine.suppressions();
    let k = AlarmKind::BusierThanUsual;
    assert_eq!(
        (
            c.get(k, Suppression::MobileSite),
            c.get(k, Suppression::UnassignedSite),
            c.get(k, Suppression::ImmatureBaseline)
        ),
        (6, 6, 6)
    );
}

/// A dismissal suppresses the key for 7 days of sample time, then the key must re-raise.
#[test]
fn alarm_dismissal_expires_on_sample_clock() {
    let mut rig = Rig::new();
    let hot = || vec![input(AlarmKind::BusierThanUsual, 1e8, 0, 1.0)];
    let mut acts = Vec::new();
    for i in 0..2 {
        acts.extend(rig.run(&snap(rig.site, i, hot()), None));
    }
    let (key, first) = rig.engine.open_alarms()[0];
    let until = rig.engine.dismiss(key, at(1));
    assert_eq!(secs_between(at(1), until), 7.0 * 86_400.0);
    // 6.9 days of scored intervals (one per 6 h here): all suppressed.
    let day = (86_400.0 / INTERVAL_S) as i64;
    let mut during = Vec::new();
    for i in (2..(day * 7)).step_by(24) {
        during.extend(rig.engine.step(&snap(rig.site, i, hot())));
    }
    assert!(during.is_empty(), "{during:?}");
    assert!(rig.engine.suppressions().total(Suppression::Dismissed) > 20);
    let mut after = Vec::new();
    for i in (day * 7 + 2)..(day * 7 + 4) {
        after.extend(rig.engine.step(&snap(rig.site, i, hot())));
    }
    assert!(
        after
            .iter()
            .any(|a| matches!(a, AlarmAction::Raise { anomaly, .. } if *anomaly != first)),
        "{after:?}"
    );
}

fn explained_count(actions: &[AlarmAction]) -> usize {
    actions
        .iter()
        .filter(|a| matches!(a, AlarmAction::Explained { .. }))
        .count()
}

fn gain_step(t: Timestamp, delta: Option<f64>) -> DeviceStep {
    DeviceStep {
        t,
        kind: ProvenanceStepKind::Gain,
        freq: None,
        gain_delta_db: delta,
        detail: "lna 16→36 dB".into(),
    }
}

/// Level-0 cell of baseline cell `cell` at 100 MHz (as `input` lays them out).
fn cell0(f0_hz: f64, cell: i64) -> i64 {
    (f0_hz / CELL_HZ).round() as i64 + cell * 16
}

/// A +20 dB gain step while a single-channel emitter keys up: the broadband shift (20 cells at
/// +20 dB) is self-inflicted, the emitter (+45 dB, a residual 25 dB beyond Δ) still raises exactly
/// one novelty alarm, annotated with the step. An isolated group shifting by Δ is not broadband.
#[test]
fn alarm_gain_step_does_not_explain_away_coincident_emitter() {
    let mut rig = Rig::new();
    let step = gain_step(add_s(at(2), 60.0), Some(20.0));
    let mut acts = Vec::new();
    for i in 0..6 {
        let n = if i >= 2 { 1.0 } else { 0.0 };
        let mut inputs: Vec<AlarmInput> = (0..20)
            .map(|c| {
                let mut x = input(AlarmKind::LevelAboveBaseline, 2e8, c * 4, n);
                if i < 2 {
                    x.observed = x.baseline_mean;
                }
                // Past the lookback, T-119's per-gain-state split flags the new state.
                x.provenance_explained = i >= 4;
                x
            })
            .collect();
        let mut emitter = input(AlarmKind::LevelAboveBaseline, 2e8, 200, n);
        emitter.observed = emitter.baseline_mean + if i >= 2 { 45.0 } else { 0.0 };
        inputs.push(emitter);
        let mut s = snap(rig.site, i, inputs);
        s.steps = vec![step.clone()];
        acts.extend(rig.run(&s, None));
    }
    assert_eq!(
        explained_count(&acts),
        20,
        "broadband shift self-inflicted once"
    );
    assert_eq!(raises(&acts), 1, "{acts:?}");
    let (key, contributors) = acts
        .iter()
        .find_map(|a| match a {
            AlarmAction::Raise {
                key, contributors, ..
            } => Some((*key, contributors.clone())),
            _ => None,
        })
        .unwrap();
    assert!(
        matches!(key.subject, AlarmSubject::Cells { lo_cell, .. } if lo_cell == cell0(2e8, 200))
    );
    assert_eq!(contributors, vec![step.clone()]);
    let row = rig
        .alarms()
        .into_iter()
        .find(|r| r.alarm.as_ref().unwrap().state == AlarmState::Open)
        .unwrap();
    let ex = latest_explanations(&rig.repo, row.anomaly.id).unwrap();
    assert!(matches!(ex[0].cause, Cause::Unexplained), "{ex:?}");
    assert!(ex.iter().any(|e| matches!(&e.cause,
        Cause::SelfInflicted { reason } if reason.starts_with("possible contributor"))));

    // An isolated two-cell group shifting by exactly Δ among 10 quiet cells: 2/12 moved, novel.
    let mut other = Rig::new();
    let step = gain_step(add_s(at(1), 60.0), Some(20.0));
    let mut acts = Vec::new();
    for i in 0..4 {
        let mut inputs: Vec<AlarmInput> = (0..10)
            .map(|c| {
                let mut x = input(AlarmKind::LevelAboveBaseline, 2e8, c * 4, 0.0);
                x.observed = x.baseline_mean;
                x
            })
            .collect();
        inputs.extend((100..102).map(|c| input(AlarmKind::LevelAboveBaseline, 2e8, c, 1.0)));
        let mut s = snap(other.site, i, inputs);
        s.steps = vec![step.clone()];
        acts.extend(other.run(&s, None));
    }
    assert_eq!((raises(&acts), explained_count(&acts)), (1, 0), "{acts:?}");
}

/// A gain step of unknown size never suppresses activity: a new emitter and a busier channel
/// both raise. Report steps carry Δ only when every part has a dB size.
#[test]
fn alarm_gain_step_without_delta_does_not_suppress_new_emitter() {
    use hk_model::attention::report::ProvenanceStep;
    let report = |detail: &str| {
        DeviceStep::from_report(&ProvenanceStep {
            t: at(0),
            kind: ProvenanceStepKind::Gain,
            freq: None,
            detail: detail.into(),
        })
        .gain_delta_db
    };
    assert_eq!(report("lna 16→32 dB, vga 20→24 dB"), Some(20.0));
    assert_eq!(report("lna 32→24 dB"), Some(-8.0));
    assert_eq!(report("lna 16→32 dB, amp false→true"), None);
    assert_eq!(report("gain table 1→2"), None);

    let mut rig = Rig::new();
    let id = EmitterId::new();
    let step = gain_step(add_s(at(0), 60.0), None);
    let mut acts = Vec::new();
    for i in 0..3 {
        let mut inputs = vec![new_emitter_input(
            id,
            FreqRange::new(868.0e6, 868.1e6),
            CalKey::Uncalibrated,
            HourOfWeek::of(at(i), 0),
            mature(),
            1,
            0.001,
            0.95,
            900.0,
        )];
        inputs.extend((0..6).map(|c| input(AlarmKind::BusierThanUsual, 1e8, c * 4, 1.0)));
        let mut s = snap(rig.site, i, inputs);
        s.steps = vec![step.clone()];
        acts.extend(rig.run(&s, None));
    }
    assert_eq!(explained_count(&acts), 0, "{acts:?}");
    assert_eq!(raises(&acts), 7, "the emitter and all six busier cells");
    assert_eq!(
        rig.engine
            .suppressions()
            .total(Suppression::ProvenanceExplained),
        0
    );
    let emitter = rig
        .alarms()
        .into_iter()
        .find(|r| r.anomaly.kind == AnomalyKind::NewEmitter)
        .unwrap();
    let ex = latest_explanations(&rig.repo, emitter.anomaly.id).unwrap();
    assert!(matches!(ex[0].cause, Cause::Unexplained), "{ex:?}");
}

fn busier(cells: std::ops::Range<i64>) -> Vec<AlarmInput> {
    cells
        .map(|c| input(AlarmKind::BusierThanUsual, 1e8, c, 1.0))
        .collect()
}

fn raised_keys(actions: &[AlarmAction]) -> Vec<AlarmKey> {
    actions
        .iter()
        .filter_map(|a| match a {
            AlarmAction::Raise { key, .. } => Some(*key),
            _ => None,
        })
        .collect()
}

fn cells_of(key: &AlarmKey) -> (i64, i64) {
    let (_, lo, hi) = cells(&key.subject).unwrap();
    (lo, hi)
}

/// Raises [100,104), dismisses it at `at(1)`, and checks it stays suppressed inside its extent.
fn dismissed_rig() -> (Rig, AlarmKey) {
    let mut rig = Rig::new();
    let mut acts = Vec::new();
    for i in 0..2 {
        acts.extend(rig.run(&snap(rig.site, i, busier(100..104)), None));
    }
    assert_eq!(raises(&acts), 1);
    let (key, _) = rig.engine.open_alarms()[0];
    rig.engine.dismiss(key, at(1));
    for i in 2..4 {
        let acts = rig.run(&snap(rig.site, i, busier(100..104)), None);
        assert!(acts.is_empty(), "{acts:?}");
    }
    (rig, key)
}

/// A dismissed key never absorbs a distinct neighbour or a wideband emitter spanning it, and
/// expired keys are evicted; an open key's new neighbour only extends it (§7.2).
#[test]
fn alarm_dismissed_key_does_not_swallow_neighbours() {
    // A distinct emitter at [105,108), one cell past the dismissed extent: its own alarm.
    let (mut rig, dismissed) = dismissed_rig();
    let mut acts = Vec::new();
    for i in 4..6 {
        acts.extend(rig.run(&snap(rig.site, i, busier(105..108)), None));
    }
    let keys = raised_keys(&acts);
    assert_eq!(keys.len(), 1, "{acts:?}");
    assert_ne!(keys[0], dismissed);
    assert_eq!(cells_of(&keys[0]), (cell0(1e8, 105), cell0(1e8, 108)));
    // Past the dismissal and the cooldown the dismissed key is evicted; the open one stays.
    let later = (8.0 * 86_400.0 / INTERVAL_S) as i64;
    rig.run(&snap(rig.site, later, Vec::new()), None);
    assert!(!rig.engine.keys.contains_key(&dismissed));
    assert!(rig.engine.keys.contains_key(&keys[0]));

    // A wideband emitter spanning the dismissed extent raises.
    let (mut rig, dismissed) = dismissed_rig();
    let mut acts = Vec::new();
    for i in 4..6 {
        acts.extend(rig.run(&snap(rig.site, i, busier(96..112)), None));
    }
    let keys = raised_keys(&acts);
    assert_eq!(keys.len(), 1, "{acts:?}");
    assert_ne!(keys[0], dismissed);
    assert_eq!(cells_of(&keys[0]), (cell0(1e8, 96), cell0(1e8, 112)));

    // An open key's new neighbour holds and extends it, never a second alarm.
    let mut rig = Rig::new();
    let mut acts = Vec::new();
    for i in 0..2 {
        acts.extend(rig.run(&snap(rig.site, i, busier(100..104)), None));
    }
    let (open, anomaly) = rig.engine.open_alarms()[0];
    let mut both = busier(100..104);
    both.extend(busier(105..108));
    let acts = rig.run(&snap(rig.site, 2, both), None);
    assert_eq!(raises(&acts), 0, "{acts:?}");
    assert!(acts.iter().any(|a| matches!(a,
        AlarmAction::Hold { key, anomaly: held, .. } if *key == open && *held == anomaly)));
    let hull = rig.alarms()[0].alarm.as_ref().unwrap().freq;
    assert!(hull.hi_hz >= cell0(1e8, 108) as f64 * CELL_HZ, "{hull:?}");
}

// ---- T-146: sequential evidence for busier / quieter than usual ----

fn t146_channel_key() -> hk_model::attention::occupancy::ChannelKey {
    hk_model::attention::occupancy::ChannelKey {
        scheme: 1,
        lo_cell: 69_355,
        hi_cell: 69_357,
    }
}

/// An occupancy-only 15-min interval on the T-146 channel.
fn t146_obs(i: i64, fco: f64, n_eff: f64, gain: u32) -> IntervalObservation {
    IntervalObservation {
        subject: BaselineSubject::Channel {
            key: t146_channel_key(),
        },
        t: at(i),
        gain,
        level_db: None,
        max_db: None,
        occupied_weight_s: fco * INTERVAL_S,
        weight_s: INTERVAL_S,
        observed_s: INTERVAL_S,
        n_eff,
        suspect_fraction: 0.0,
        provenance_explained: false,
    }
}

/// The alarm inputs of `obs` scored against `pool` (as a mature T-119 fold would score it).
fn t146_inputs(pool: &PoolStats, obs: &IntervalObservation) -> Vec<AlarmInput> {
    use crate::occupancy::novelty::{Evidence as Ev, combine, occupancy_z};
    let cfg = NoveltyConfig::default();
    let ev = Ev {
        level_db: None,
        fco: obs.fco(),
        n_eff: obs.n_eff,
        weight_s: obs.weight_s,
        observed_s: obs.observed_s,
    };
    let fold = FoldOutcome {
        novelty: combine(
            mature(),
            None,
            occupancy_z(pool, &ev),
            None,
            obs.observed_s,
            false,
            &cfg,
        ),
        change_point: None,
        accrued: true,
        accrued_reference: false,
    };
    inputs_from_fold(
        obs,
        &fold,
        CalKey::Uncalibrated,
        HourOfWeek::of(obs.t, 0),
        1,
        CELL_HZ,
        16,
        PoolContext::default(),
        &cfg,
    )
}

/// An all-hours reference of 168 slots × 4 intervals, each interval's FCO measured from `n_eff`
/// independent looks at occupancy probability `p` (seeded), with the T-146 sampling moment.
fn t146_pool(p: f64, n_eff: u32, seed: u64) -> PoolStats {
    use hk_model::attention::baseline::SlotStats;
    let mut state = seed;
    let mut next = move || {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut pool = PoolStats::default();
    for _ in 0..168 {
        let mut s = SlotStats::EMPTY;
        for _ in 0..4 {
            let hits = (0..n_eff).filter(|_| next() < p).count() as f64;
            s.fco_var_s =
                SlotStats::fco_var_after(s.fco_var_s, s.weight_s, INTERVAL_S, f64::from(n_eff));
            s.occupied_weight_s += INTERVAL_S * hits / f64::from(n_eff);
            s.weight_s += INTERVAL_S;
            s.observed_s += INTERVAL_S;
        }
        pool.add(
            0.0,
            s.observed_s,
            0.0,
            0.0,
            s.occupied_weight_s,
            s.weight_s,
            0.0,
        );
        pool.add_fco_sampling(s.weight_s, s.fco_var_s);
    }
    pool
}

fn sequential_evidence_value(ev: &AlarmEvent, name: &str) -> Option<f64> {
    ev.explanations
        .iter()
        .filter(|e| e.cause == Cause::Unexplained)
        .flat_map(|e| e.evidence.iter())
        .find_map(|v| match v {
            Evidence::Value { name: n, value } if n == name => Some(*value),
            _ => None,
        })
}

/// T-146 (ADR-0012 §7.2): a sparse-visit channel (FCO 0.05, two effective looks per interval)
/// that goes fully busy scores z ≈ 3.6 per interval, novelty 0.09, below `off`: the
/// single-interval rule never alarms. Its evidence accumulates and raises one busier-than-usual
/// alarm within N intervals.
///
/// N a priori from the rule: the run of k intervals reaches on when Q(z√k) ≤ Q(7.9)²/(2k(k+1))
/// (Q(7.9)² = 1.95·10⁻³⁰); the hysteresis raises one interval later. For the worst case z = 3.4
/// allowed below: k = 12 gives Q(11.78) ≈ 2.5·10⁻³² > 1.95·10⁻³⁰/312 = 6.2·10⁻³³ (not yet), k = 13
/// gives Q(12.26) ≈ 7·10⁻³⁵ ≤ 1.95·10⁻³⁰/364 = 5.4·10⁻³³ (on), so **N = 14**. For the best case
/// z = 3.8: k = 9 gives Q(11.4) ≈ 2·10⁻³⁰ (not yet), k = 10 gives Q(12.02) ≈ 1.4·10⁻³³ ≤
/// 1.95·10⁻³⁰/220 (on), so the raise is no earlier than interval 11.
#[test]
fn alarm_sparse_busier_onset_accumulates_to_one_raise() {
    const N: i64 = 14;
    const EARLIEST: i64 = 11;
    let pool = t146_pool(0.05, 2, 0x146a);
    let first = t146_inputs(&pool, &t146_obs(0, 1.0, 2.0, 0));
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].kind, AlarmKind::BusierThanUsual);
    let z = first[0].z;
    assert!((3.4..=3.8).contains(&z), "per-interval z {z}");
    assert!(first[0].novelty < HysteresisConfig::default().off);

    let mut rig = Rig::new();
    let site = rig.site;
    let mut latency = None;
    for i in 0..20 {
        let inputs = t146_inputs(&pool, &t146_obs(i, 1.0, 2.0, 0));
        let actions = rig.run(&snap(site, i, inputs), None);
        if raises(&actions) > 0 {
            assert_eq!(raises(&actions), 1);
            latency = Some(i + 1);
            break;
        }
    }
    let latency = latency.expect("the sparse onset raises");
    println!("T-146 sparse onset: per-interval z {z:.3}, raised at interval {latency} (N = {N})");
    assert!((EARLIEST..=N).contains(&latency), "latency {latency}");
    let ev = rig.events.last().unwrap();
    assert_eq!(ev.row.key.kind, AlarmKind::BusierThanUsual);
    assert_eq!(
        ev.row.key.subject,
        AlarmSubject::Channel {
            key: t146_channel_key()
        }
    );
    assert!(ev.row.detail.novelty >= 0.7);
    let k = sequential_evidence_value(ev, "sequential_intervals").expect("sequential evidence");
    assert_eq!(k, latency as f64);
    assert!(sequential_evidence_value(ev, "sequential_z").unwrap() >= z * (k.sqrt() - 1e-9));
}

/// T-146: a dense-visit onset (n_eff 80) is novel on its own and raises within 2 intervals, as
/// before the sequential rule.
#[test]
fn alarm_dense_busier_onset_raises_within_two_intervals() {
    let pool = t146_pool(0.05, 80, 0x146b);
    let mut rig = Rig::new();
    let site = rig.site;
    let inputs = t146_inputs(&pool, &t146_obs(0, 0.6, 80.0, 0));
    assert!(
        inputs[0].novelty >= 0.7,
        "single interval z {}",
        inputs[0].z
    );
    assert_eq!(raises(&rig.run(&snap(site, 0, inputs), None)), 0);
    let inputs = t146_inputs(&pool, &t146_obs(1, 0.6, 80.0, 0));
    assert_eq!(raises(&rig.run(&snap(site, 1, inputs), None)), 1);
}

/// T-146: the engine's run resets on a direction change, a gap beyond the horizon, a gain-key
/// change, an immature or provenance-explained input and a site change (and a caller reset).
#[test]
fn alarm_sequential_run_resets_in_the_engine() {
    let pool = t146_pool(0.05, 2, 0x146c);
    let mut e = AlarmEngine::new(AlarmConfig::default()).unwrap();
    let site_id = SiteId::new();
    let site = SiteKey::Site(site_id);
    let subject = AlarmSubject::Channel {
        key: t146_channel_key(),
    };
    let run = |e: &AlarmEngine| e.sequential_run(site_id, subject, CalKey::Uncalibrated);
    let k = |e: &AlarmEngine| run(e).map_or(0, |r| r.k);
    let busy = |i: i64, gain: u32| t146_inputs(&pool, &t146_obs(i, 1.0, 2.0, gain));
    for i in 0..4 {
        e.step(&snap(site, i, busy(i, 0)));
    }
    assert_eq!(k(&e), 4);
    // Direction change: an empty interval scores quieter (z < 0).
    let quiet = t146_inputs(&pool, &t146_obs(4, 0.0, 2.0, 0));
    assert_eq!(quiet[0].kind, AlarmKind::QuieterThanUsual);
    e.step(&snap(site, 4, quiet));
    assert_eq!((run(&e).unwrap().direction, k(&e)), (-1, 1));
    for i in 5..8 {
        e.step(&snap(site, i, busy(i, 0)));
    }
    assert_eq!(k(&e), 3);
    // Gap beyond 2 h.
    e.step(&snap(site, 17, busy(17, 0)));
    assert_eq!(k(&e), 1);
    // Gain key change.
    e.step(&snap(site, 18, busy(18, 0)));
    e.step(&snap(site, 19, busy(19, 9)));
    assert_eq!(k(&e), 1);
    // Immature input.
    e.step(&snap(site, 20, busy(20, 9)));
    assert_eq!(k(&e), 2);
    let mut imm = busy(21, 9);
    imm[0].maturity = Maturity::Immature { observed_s: 0.0 };
    e.step(&snap(site, 21, imm));
    assert!(run(&e).is_none());
    // Provenance-explained input.
    e.step(&snap(site, 22, busy(22, 9)));
    let mut prov = busy(23, 9);
    prov[0].provenance_explained = true;
    e.step(&snap(site, 23, prov));
    assert!(run(&e).is_none());
    // Site change.
    e.step(&snap(site, 24, busy(24, 9)));
    e.step(&snap(site, 25, busy(25, 9)));
    assert_eq!(k(&e), 2);
    e.step(&snap(SiteKey::Site(SiteId::new()), 26, Vec::new()));
    assert!(run(&e).is_none());
    // Caller reset (an unscorable fold).
    e.step(&snap(site, 27, busy(27, 9)));
    assert_eq!(k(&e), 1);
    e.reset_sequential(site, &subject);
    assert!(run(&e).is_none());
}
