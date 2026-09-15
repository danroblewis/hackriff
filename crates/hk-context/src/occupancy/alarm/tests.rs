//! T-122 alarm tests on synthetic novelty snapshots (the truth lives only in the asserts).

use hk_model::attention::baseline::SiteKey;
use hk_model::attention::score::NoveltyScore;
use hk_model::ids::{EmitterId, SiteId};
use hk_model::{AnomalyKind, Cause};

use super::*;
use crate::feeds::FeedAdapter;
use crate::feeds::gpsjam::GpsjamAdapter;
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
                vec![input(AlarmKind::BusierThanUsual, 1e8, 0, n)],
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
                vec![input(AlarmKind::BusierThanUsual, 1e8, 0, n)],
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
