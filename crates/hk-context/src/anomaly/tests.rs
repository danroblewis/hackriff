//! AWARE-006 lifecycle tests against synthetic [`EpisodeSignal`] sequences (independent of the
//! floor tracker's timing): ordering, duplicates, restarts, Extend/Update/Unknown/Fall.

use hk_core::ProvenanceHandle;
use hk_dsp::WindowKind;
use hk_dsp::floor::{EndReason, FloorChangeClass, FloorEvent, FloorEventKind, GainKey};
use hk_model::{AnomalyStatus, Cause, CorrelationType, SampleTime};

use super::*;

const BASE_NS: i64 = 1_789_300_800 * 1_000_000_000; // 2026-09-13T12:00:00Z

fn t(s: f64) -> Timestamp {
    Timestamp::from_unix_nanos(BASE_NS + (s * 1e9) as i64)
}

fn extent(episode: u64, onset_s: f64, f_lo_mhz: f64, f_hi_mhz: f64) -> EpisodeExtent {
    EpisodeExtent {
        episode,
        segment: 0,
        class: EpisodeClass::NoiseLike,
        onset: t(onset_s),
        confirmed: t(onset_s + 1.0),
        f_lo_hz: f_lo_mhz * 1e6,
        f_hi_hz: f_hi_mhz * 1e6,
        step_db: 10.0,
        step_uncertainty_db: 0.2,
        baseline_dbfs_per_hz: -95.0,
    }
}

fn opened(e: EpisodeExtent) -> EpisodeSignal {
    EpisodeSignal::Opened {
        extent: e,
        continued: false,
        split_from: None,
    }
}

fn closed(episode: u64, at: f64, reason: CloseReason) -> EpisodeSignal {
    EpisodeSignal::Closed {
        episode,
        t: t(at),
        reason,
        duration_s: at,
        merged_into: None,
    }
}

fn lifecycle(run: &str) -> FloorAnomalies {
    FloorAnomalies::new(FloorAnomalyConfig::new(run)).unwrap()
}

fn statuses(repo: &Repository, id: AnomalyId) -> Vec<AnomalyStatus> {
    repo.anomaly_status_history(id)
        .unwrap()
        .iter()
        .map(|c| c.status)
        .collect()
}

fn all_anomalies(repo: &Repository) -> Vec<Anomaly> {
    repo.anomalies_in_region(&everything()).unwrap()
}

#[test]
fn aware_006_rise_opens_and_end_closes() {
    let mut repo = Repository::open_in_memory().unwrap();
    let mut life = lifecycle("dev:1");
    let r = life
        .apply(&mut repo, &opened(extent(1, 0.5, 1574.42, 1576.42)))
        .unwrap();
    assert_eq!(r.opened.len(), 1);
    let id = r.opened[0];
    let a = repo.anomaly(id).unwrap();
    assert_eq!(a.kind, AnomalyKind::NoiseFloorRise);
    assert_eq!(a.subject, AnomalySubject::Region);
    assert_eq!(a.region.freq, FreqRange::new(1574.42e6, 1576.42e6));
    assert_eq!(a.region.time, TimeRange::new(t(0.5), t(1.5)));
    assert_eq!(a.t, t(1.5));
    assert!((a.score - 50.0).abs() < 1e-9, "step / uncertainty");
    let key = parse_baseline_ref(a.baseline_ref.as_deref().unwrap()).unwrap();
    assert_eq!(
        key,
        EpisodeKey {
            run: "dev:1".into(),
            segment: 0,
            episode: 1,
            onset_ns: t(0.5).as_unix_nanos()
        }
    );
    // A rise with no End stays open.
    assert_eq!(statuses(&repo, id), [AnomalyStatus::Open]);
    assert_eq!(life.open_anomalies(), [id]);

    let r = life
        .apply(&mut repo, &closed(1, 4.0, CloseReason::Returned))
        .unwrap();
    assert_eq!(r.closed, [id]);
    assert_eq!(
        statuses(&repo, id),
        [AnomalyStatus::Open, AnomalyStatus::Resolved]
    );
    let history = repo.anomaly_status_history(id).unwrap();
    assert_eq!(history[1].t, t(4.0));
    assert!(
        history[1]
            .note
            .as_deref()
            .unwrap()
            .contains("reason=returned")
    );
    assert!(life.open_anomalies().is_empty());
}

#[test]
fn aware_006_duplicates_and_replays_are_idempotent() {
    let mut repo = Repository::open_in_memory().unwrap();
    let mut life = lifecycle("dev:1");
    let rise = opened(extent(1, 0.5, 1574.0, 1577.0));
    let id = life.apply(&mut repo, &rise).unwrap().opened[0];
    let dup = life.apply(&mut repo, &rise).unwrap();
    assert_eq!(dup.ignored, Some("duplicate episode onset"));
    assert!(dup.opened.is_empty());
    assert_eq!(all_anomalies(&repo).len(), 1);

    let end = closed(1, 3.0, CloseReason::Returned);
    assert_eq!(life.apply(&mut repo, &end).unwrap().closed, [id]);
    // Replayed End and replayed Rise after the close write nothing.
    assert!(life.apply(&mut repo, &end).unwrap().ignored.is_some());
    assert!(life.apply(&mut repo, &rise).unwrap().ignored.is_some());
    assert_eq!(all_anomalies(&repo).len(), 1);
    assert_eq!(
        statuses(&repo, id),
        [AnomalyStatus::Open, AnomalyStatus::Resolved]
    );

    // An End for an episode never opened (e.g. its Rise was structured) is ignored.
    assert!(
        life.apply(&mut repo, &closed(9, 3.0, CloseReason::Returned))
            .unwrap()
            .ignored
            .is_some()
    );
}

#[test]
fn aware_006_structured_and_unverified_classes() {
    let mut repo = Repository::open_in_memory().unwrap();
    let mut life = lifecycle("dev:1");
    let mut s = extent(1, 0.5, 1574.0, 1577.0);
    s.class = EpisodeClass::Structured;
    assert!(
        life.apply(&mut repo, &opened(s.clone()))
            .unwrap()
            .ignored
            .is_some()
    );
    assert!(
        life.apply(&mut repo, &EpisodeSignal::Extended(s))
            .unwrap()
            .ignored
            .is_some()
    );
    let mut u = extent(2, 0.5, 1574.0, 1577.0);
    u.class = EpisodeClass::Unverified;
    assert!(
        life.apply(&mut repo, &opened(u.clone()))
            .unwrap()
            .ignored
            .is_some()
    );
    assert!(all_anomalies(&repo).is_empty());

    let mut cfg = FloorAnomalyConfig::new("dev:1");
    cfg.accept_unverified = true;
    let mut accepting = FloorAnomalies::new(cfg).unwrap();
    assert_eq!(
        accepting.apply(&mut repo, &opened(u)).unwrap().opened.len(),
        1
    );
}

#[test]
fn aware_006_reset_then_continued_reopens_the_same_anomaly() {
    let mut repo = Repository::open_in_memory().unwrap();
    let mut life = lifecycle("dev:1");
    let e = extent(3, 0.5, 1574.0, 1577.0);
    let id = life.apply(&mut repo, &opened(e.clone())).unwrap().opened[0];
    let r = life
        .apply(&mut repo, &closed(3, 2.0, CloseReason::Reset))
        .unwrap();
    assert_eq!(r.closed, [id]);
    assert!(
        repo.anomaly_status_history(id).unwrap()[1]
            .note
            .as_deref()
            .unwrap()
            .contains("reason=reset")
    );
    let mut cont = e.clone();
    cont.onset = t(2.2);
    cont.confirmed = t(2.4);
    let r = life
        .apply(
            &mut repo,
            &EpisodeSignal::Opened {
                extent: cont.clone(),
                continued: true,
                split_from: None,
            },
        )
        .unwrap();
    assert_eq!(r.reopened, [id]);
    assert!(r.opened.is_empty());
    // A replayed continuation (nothing closed now, onset not a part key) must not duplicate,
    // in this process or after a restart.
    let replay = EpisodeSignal::Opened {
        extent: cont,
        continued: true,
        split_from: None,
    };
    let again = life.apply(&mut repo, &replay).unwrap();
    assert_eq!(again.ignored, Some("duplicate episode continuation"));
    let mut life = FloorAnomalies::resume(FloorAnomalyConfig::new("dev:1"), &repo).unwrap();
    assert!(life.apply(&mut repo, &replay).unwrap().ignored.is_some());
    assert_eq!(all_anomalies(&repo).len(), 1);
    life.apply(&mut repo, &closed(3, 5.0, CloseReason::Returned))
        .unwrap();
    assert_eq!(
        statuses(&repo, id),
        [
            AnomalyStatus::Open,
            AnomalyStatus::Resolved,
            AnomalyStatus::Open,
            AnomalyStatus::Resolved
        ]
    );
    assert!(life.open_anomalies().is_empty());
}

#[test]
fn aware_006_unknown_closes_the_named_episode_or_the_run() {
    let mut repo = Repository::open_in_memory().unwrap();
    let mut life = lifecycle("dev:1");
    let a = life
        .apply(&mut repo, &opened(extent(1, 0.5, 1574.0, 1575.0)))
        .unwrap()
        .opened[0];
    let b = life
        .apply(&mut repo, &opened(extent(2, 0.7, 1576.0, 1577.0)))
        .unwrap()
        .opened[0];
    let r = life
        .apply(
            &mut repo,
            &EpisodeSignal::Unknown {
                episode: Some(1),
                t: t(3.0),
            },
        )
        .unwrap();
    assert_eq!(r.closed, [a]);
    assert_eq!(life.open_anomalies(), [b]);
    let r = life
        .apply(
            &mut repo,
            &EpisodeSignal::Unknown {
                episode: None,
                t: t(3.5),
            },
        )
        .unwrap();
    assert_eq!(r.closed, [b]);
    assert_eq!(
        repo.anomaly_status_history(b).unwrap()[1].note.as_deref(),
        Some("episode-state-unknown")
    );
}

#[test]
fn aware_006_extend_grows_the_primary_update_clips_and_end_closes() {
    let mut repo = Repository::open_in_memory().unwrap();
    let mut life = lifecycle("dev:1");
    let first = life
        .apply(&mut repo, &opened(extent(1, 0.5, 1574.0, 1575.0)))
        .unwrap()
        .opened[0];
    explain(&mut repo, first);
    // Extend reports only the added extent; the primary grows by supersession.
    let ext = EpisodeSignal::Extended(extent(1, 2.0, 1575.0, 1576.5));
    let r = life.apply(&mut repo, &ext).unwrap();
    let second = r.opened[0];
    assert_eq!(r.superseded, [(first, second)]);
    assert!(r.closed.is_empty());
    let a = repo.anomaly(second).unwrap();
    assert_eq!(a.region.freq, FreqRange::new(1574.0e6, 1576.5e6));
    assert_eq!(a.region.time.start, t(0.5), "onset kept");
    assert_eq!(
        statuses(&repo, first),
        [AnomalyStatus::Open, AnomalyStatus::Resolved]
    );
    assert_eq!(life.open_anomalies(), [second]);
    let copied = repo.explanations_for_anomaly(second).unwrap();
    assert_eq!(copied.len(), 1, "explanation copied to the successor");
    assert_eq!(
        copied[0].supersedes,
        Some(repo.explanations_for_anomaly(first).unwrap()[0].id)
    );
    assert!(
        life.apply(&mut repo, &ext).unwrap().ignored.is_some(),
        "replayed Extend"
    );
    assert_eq!(life.episode_parts(1).len(), 1);

    // The extent shrank: the part is clipped (superseded again).
    let r = life
        .apply(
            &mut repo,
            &EpisodeSignal::Updated {
                episode: 1,
                t: t(4.0),
                f_lo_hz: 1574.2e6,
                f_hi_hz: 1574.8e6,
            },
        )
        .unwrap();
    let third = r.opened[0];
    assert_eq!(r.superseded, [(second, third)]);
    assert_eq!(
        repo.anomaly(third).unwrap().region.freq,
        FreqRange::new(1574.2e6, 1574.8e6)
    );
    // An Update elsewhere (no overlap) resolves it.
    let r = life
        .apply(
            &mut repo,
            &EpisodeSignal::Updated {
                episode: 1,
                t: t(4.5),
                f_lo_hz: 1580e6,
                f_hi_hz: 1581e6,
            },
        )
        .unwrap();
    assert_eq!(r.closed, [third]);
    assert!(
        life.apply(&mut repo, &closed(1, 6.0, CloseReason::Returned))
            .unwrap()
            .ignored
            .is_some()
    );

    // An Extend for an episode whose Rise was never seen still opens a part.
    let orphan = life
        .apply(
            &mut repo,
            &EpisodeSignal::Extended(extent(7, 9.0, 1580.0, 1581.0)),
        )
        .unwrap();
    assert_eq!(orphan.opened.len(), 1);
}

fn explain(repo: &mut Repository, anomaly: AnomalyId) -> ExplanationId {
    let e = Explanation {
        id: ExplanationId::new(),
        anomaly_ref: anomaly,
        cause: Cause::Unexplained,
        correlation_type: CorrelationType::TimeCoincidence,
        score: 0.5,
        evidence: vec![],
        supersedes: None,
        provisional: false,
        rule_version: "test@1".into(),
        t: t(2.0),
    };
    repo.insert_explanation(&e).unwrap();
    e.id
}

#[test]
fn aware_006_merge_reparents_and_split_moves_parts_with_their_explanations() {
    let mut repo = Repository::open_in_memory().unwrap();
    let mut life = lifecycle("dev:1");
    let a = life
        .apply(&mut repo, &opened(extent(1, 0.5, 1574.0, 1575.0)))
        .unwrap()
        .opened[0];
    let b = life
        .apply(&mut repo, &opened(extent(2, 0.6, 1576.0, 1577.0)))
        .unwrap()
        .opened[0];
    let b_expl = explain(&mut repo, b);
    // B merges into A: B's anomaly stays open, re-parented; A grows over bridge + B.
    let r = life
        .apply(
            &mut repo,
            &EpisodeSignal::Closed {
                episode: 2,
                t: t(3.0),
                reason: CloseReason::Merged,
                duration_s: 2.4,
                merged_into: Some(1),
            },
        )
        .unwrap();
    assert_eq!(
        (r.reparented.as_slice(), r.closed.len()),
        ([b].as_slice(), 0)
    );
    // T-029: A's successor covers B, so B is absorbed into it (no overlapping open anomalies)
    // and B's Explanation is copied to it.
    let r = life
        .apply(
            &mut repo,
            &EpisodeSignal::Extended(extent(1, 2.5, 1575.0, 1577.0)),
        )
        .unwrap();
    let a2 = r.opened[0];
    assert!(r.superseded.contains(&(b, a2)), "{r:?}");
    assert_eq!(life.open_anomalies(), [a2]);
    assert!(life.episode_parts(2).is_empty());
    assert_eq!(repo.explanations_for_anomaly(b).unwrap()[0].id, b_expl);
    assert!(
        repo.explanations_for_anomaly(a2)
            .unwrap()
            .iter()
            .any(|e| e.supersedes == Some(b_expl)),
        "B's explanation moves to the absorbing anomaly"
    );
    assert_eq!(statuses(&repo, b).last(), Some(&AnomalyStatus::Resolved));

    // Restart: membership survives.
    let mut life = FloorAnomalies::resume(FloorAnomalyConfig::new("dev:1"), &repo).unwrap();
    assert_eq!(life.episode_parts(1).iter().filter(|p| p.open).count(), 1);

    // The bridge returned: the tracker splits B's region off as episode 3 and updates A. The
    // parent's anomaly is clipped off the split extent at once, and episode 3 opens its own.
    let r = life
        .apply(
            &mut repo,
            &EpisodeSignal::Opened {
                extent: extent(3, 0.5, 1576.0, 1577.0),
                continued: false,
                split_from: Some(1),
            },
        )
        .unwrap();
    assert!(r.reparented.is_empty());
    assert_eq!(r.superseded.len(), 1);
    let c = life.episode_parts(3)[0].anomaly;
    assert_eq!(
        repo.anomaly(c).unwrap().region.freq,
        FreqRange::new(1576.0e6, 1577.0e6)
    );
    let clipped = r.superseded[0].1;
    assert_eq!(
        repo.anomaly(clipped).unwrap().region.freq,
        FreqRange::new(1574.0e6, 1576.0e6)
    );
    let r = life
        .apply(
            &mut repo,
            &EpisodeSignal::Updated {
                episode: 1,
                t: t(6.0),
                f_lo_hz: 1574.0e6,
                f_hi_hz: 1575.0e6,
            },
        )
        .unwrap();
    let a3 = r.opened[0];
    assert_eq!(
        repo.anomaly(a3).unwrap().region.freq,
        FreqRange::new(1574.0e6, 1575.0e6)
    );
    let life2 = FloorAnomalies::resume(FloorAnomalyConfig::new("dev:1"), &repo).unwrap();
    assert_eq!(
        life2.episode_parts(3)[0].anomaly,
        c,
        "split membership resumes"
    );
    assert_eq!(
        life.apply(&mut repo, &closed(3, 8.0, CloseReason::Returned))
            .unwrap()
            .closed,
        [c]
    );
    assert_eq!(life.open_anomalies(), [a3]);
    assert_ne!(a, a3);
}

#[test]
fn aware_006_bare_fall_closes_overlapping_open_anomalies() {
    let mut repo = Repository::open_in_memory().unwrap();
    let mut life = lifecycle("dev:1");
    let id = life
        .apply(&mut repo, &opened(extent(1, 0.5, 1575.0, 1576.0)))
        .unwrap()
        .opened[0];
    let away = EpisodeSignal::Fell {
        t: t(3.0),
        f_lo_hz: 1580e6,
        f_hi_hz: 1581e6,
        interrupted: false,
    };
    assert!(life.apply(&mut repo, &away).unwrap().ignored.is_some());
    let fall = EpisodeSignal::Fell {
        t: t(3.5),
        f_lo_hz: 1575.5e6,
        f_hi_hz: 1577e6,
        interrupted: true,
    };
    assert_eq!(life.apply(&mut repo, &fall).unwrap().closed, [id]);
    assert_eq!(
        repo.anomaly_status_history(id).unwrap()[1].note.as_deref(),
        Some("floor-fall;interrupted")
    );
    assert!(
        life.apply(&mut repo, &fall).unwrap().ignored.is_some(),
        "replayed Fall"
    );
}

#[test]
fn aware_006_restart_mid_episode_then_end_closes_the_right_anomaly() {
    let mut repo = Repository::open_in_memory().unwrap();
    let (ep1, ep2);
    {
        let mut life = lifecycle("dev:1");
        ep1 = life
            .apply(&mut repo, &opened(extent(1, 0.5, 1574.0, 1575.0)))
            .unwrap()
            .opened[0];
        ep2 = life
            .apply(&mut repo, &opened(extent(2, 0.6, 1576.0, 1577.0)))
            .unwrap()
            .opened[0];
        life.apply(&mut repo, &closed(2, 2.0, CloseReason::Returned))
            .unwrap();
        // Process dies here with episode 1 open.
    }
    let mut life = FloorAnomalies::resume(FloorAnomalyConfig::new("dev:1"), &repo).unwrap();
    assert_eq!(life.open_anomalies(), [ep1]);
    assert_eq!(
        life.episode_parts(2)[0],
        EpisodePart {
            anomaly: ep2,
            freq: FreqRange::new(1576e6, 1577e6),
            open: false
        }
    );
    // Replays from before the restart write nothing.
    assert!(
        life.apply(&mut repo, &opened(extent(1, 0.5, 1574.0, 1575.0)))
            .unwrap()
            .ignored
            .is_some()
    );
    assert!(
        life.apply(&mut repo, &opened(extent(2, 0.6, 1576.0, 1577.0)))
            .unwrap()
            .ignored
            .is_some()
    );
    assert!(
        life.apply(&mut repo, &closed(2, 2.0, CloseReason::Returned))
            .unwrap()
            .ignored
            .is_some()
    );
    let r = life
        .apply(&mut repo, &closed(1, 5.0, CloseReason::Returned))
        .unwrap();
    assert_eq!(r.closed, [ep1]);
    assert_eq!(
        statuses(&repo, ep2),
        [AnomalyStatus::Open, AnomalyStatus::Resolved]
    );
    assert_eq!(all_anomalies(&repo).len(), 2);

    // A different tracker run does not adopt dev:1's episodes (ids collide across runs).
    let mut other = FloorAnomalies::resume(FloorAnomalyConfig::new("dev:2"), &repo).unwrap();
    assert!(other.episode_parts(1).is_empty());
    let id = other
        .apply(&mut repo, &opened(extent(1, 0.5, 1574.0, 1575.0)))
        .unwrap()
        .opened[0];
    assert_ne!(id, ep1);
}

#[test]
fn aware_006_orphaned_runs_are_closed_explicitly() {
    let mut repo = Repository::open_in_memory().unwrap();
    let mut old = lifecycle("dev:1");
    let stale = old
        .apply(&mut repo, &opened(extent(1, 0.5, 1574.0, 1575.0)))
        .unwrap()
        .opened[0];
    let mut live = lifecycle("dev:2");
    let current = live
        .apply(&mut repo, &opened(extent(1, 0.5, 1574.0, 1575.0)))
        .unwrap()
        .opened[0];
    assert_eq!(
        close_orphaned(&mut repo, &["dev:2"], t(10.0)).unwrap(),
        [stale]
    );
    assert_eq!(statuses(&repo, current), [AnomalyStatus::Open]);
    assert!(
        close_orphaned(&mut repo, &["dev:2"], t(11.0))
            .unwrap()
            .is_empty(),
        "idempotent"
    );
    let resumed = FloorAnomalies::resume(FloorAnomalyConfig::new("dev:1"), &repo).unwrap();
    assert!(resumed.open_anomalies().is_empty());
}

#[test]
fn run_ids_and_refs_are_validated() {
    assert!(FloorAnomalies::new(FloorAnomalyConfig::new("bad run;x")).is_err());
    assert!(FloorAnomalies::new(FloorAnomalyConfig::new("")).is_err());
    assert_eq!(parse_baseline_ref("baseline:l1:24h"), None);
    assert_eq!(
        parse_baseline_ref("floor-episode:v1;run=a;segment=0;episode=x;onset_ns=1"),
        None
    );
    let mut repo = Repository::open_in_memory().unwrap();
    let mut bad = extent(1, 0.5, 1575.0, 1574.0);
    bad.f_lo_hz = 1576e6;
    assert!(lifecycle("dev:1").apply(&mut repo, &opened(bad)).is_err());
}

fn floor_event(
    kind: FloorEventKind,
    class: FloorChangeClass,
    end: Option<EndReason>,
) -> FloorEvent {
    let prov: hk_model::Provenance = serde_json::from_value(serde_json::json!({
        "device_id": "synthetic:hk-context", "tune": {"center_hz": 1575.42e6, "sample_rate_hz": 2e6,
        "lna_db": 24.0, "vga_db": 20.0, "amp_on": false, "bandwidth_hz": 1.5e6}, "overload": false,
        "quantisation_limited": false, "clock_source": "internal", "clock_locked": true,
        "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .unwrap();
    let st = |s: f64| SampleTime {
        sample_index: (s * 2e6) as u64,
        host_time: t(s),
    };
    FloorEvent {
        kind,
        episode: 4,
        class,
        end_reason: end,
        merged_into: None,
        split_from: None,
        interrupted: false,
        onset_seq: 10,
        onset_t: st(1.0),
        confirmed_seq: 20,
        confirmed_t: st(2.0),
        episode_onset_t: st(1.0),
        duration_s: 1.0,
        bins: 0..1024,
        change_bins: 512..1024,
        change_f_lo_hz: 1575.42e6,
        change_f_hi_hz: 1576.42e6,
        f_lo_hz: 1574.42e6,
        f_hi_hz: 1576.42e6,
        band_fraction: 1.0,
        baseline_dbfs_per_hz: -95.0,
        baseline_segment: 0,
        level_dbfs_per_hz: -85.0,
        step_db: 10.0,
        peak_step_db: 10.5,
        step_uncertainty_db: 0.1,
        uncertainty_db: 0.5,
        sk: Some(1.0),
        excess_std_db: 0.2,
        quantisation_limited_before: false,
        gain: GainKey {
            center_hz: 1575.42e6,
            sample_rate_hz: 2e6,
            bandwidth_hz: 1.5e6,
            lna_db: 24.0,
            vga_db: 20.0,
            amp_on: false,
            antenna_port_hash: None,
            fft_len: 1024,
            overlap: 512,
            n_avg: 10,
            window: WindowKind::Hann,
        },
        segment: 0,
        provenance: ProvenanceHandle::new(prov),
    }
}

#[test]
fn floor_events_map_through_the_single_adapter() {
    let rise = signal_from_floor_event(&floor_event(
        FloorEventKind::Rise,
        FloorChangeClass::NoiseLike,
        None,
    ))
    .unwrap();
    let EpisodeSignal::Opened {
        extent,
        continued,
        split_from,
    } = rise
    else {
        panic!("{rise:?}")
    };
    assert!(!continued && split_from.is_none());
    assert_eq!(
        (extent.episode, extent.class, extent.onset, extent.confirmed),
        (4, EpisodeClass::NoiseLike, t(1.0), t(2.0))
    );
    assert_eq!(
        (extent.f_lo_hz, extent.f_hi_hz, extent.step_db),
        (1574.42e6, 1576.42e6, 10.0)
    );
    let end = signal_from_floor_event(&floor_event(
        FloorEventKind::End,
        FloorChangeClass::NoiseLike,
        Some(EndReason::Reset),
    ))
    .unwrap();
    assert!(matches!(
        end,
        EpisodeSignal::Closed {
            episode: 4,
            reason: CloseReason::Reset,
            ..
        }
    ));
    let fall = signal_from_floor_event(&floor_event(
        FloorEventKind::Fall,
        FloorChangeClass::NoiseLike,
        None,
    ))
    .unwrap();
    assert!(matches!(
        fall,
        EpisodeSignal::Fell {
            interrupted: false,
            ..
        }
    ));
    // The T-005 re-review event kinds.
    let extend = floor_event(FloorEventKind::Extend, FloorChangeClass::NoiseLike, None);
    let Some(EpisodeSignal::Extended(added)) = signal_from_floor_event(&extend) else {
        panic!("extend")
    };
    assert_eq!((added.f_lo_hz, added.f_hi_hz), (1575.42e6, 1576.42e6));
    assert!(matches!(
        signal_from_floor_event(&floor_event(
            FloorEventKind::Update,
            FloorChangeClass::NoiseLike,
            None
        )),
        Some(EpisodeSignal::Updated { episode: 4, .. })
    ));
    let mut merged = floor_event(
        FloorEventKind::End,
        FloorChangeClass::NoiseLike,
        Some(EndReason::Merged),
    );
    merged.merged_into = Some(9);
    assert!(matches!(
        signal_from_floor_event(&merged),
        Some(EpisodeSignal::Closed {
            reason: CloseReason::Merged,
            merged_into: Some(9),
            ..
        })
    ));
    let mut split = floor_event(FloorEventKind::Rise, FloorChangeClass::NoiseLike, None);
    split.split_from = Some(2);
    assert!(matches!(
        signal_from_floor_event(&split),
        Some(EpisodeSignal::Opened {
            split_from: Some(2),
            ..
        })
    ));
    assert!(matches!(
        signal_from_floor_event(&floor_event(
            FloorEventKind::Unknown,
            FloorChangeClass::NoiseLike,
            None
        )),
        Some(EpisodeSignal::Unknown {
            episode: Some(4),
            ..
        })
    ));
    let mut interrupted = floor_event(FloorEventKind::Fall, FloorChangeClass::NoiseLike, None);
    interrupted.interrupted = true;
    assert!(matches!(
        signal_from_floor_event(&interrupted),
        Some(EpisodeSignal::Fell {
            interrupted: true,
            ..
        })
    ));
    let structured = signal_from_floor_event(&floor_event(
        FloorEventKind::Rise,
        FloorChangeClass::Structured,
        None,
    ))
    .unwrap();
    let mut repo = Repository::open_in_memory().unwrap();
    assert!(
        lifecycle("dev:1")
            .apply(&mut repo, &structured)
            .unwrap()
            .ignored
            .is_some()
    );
}
