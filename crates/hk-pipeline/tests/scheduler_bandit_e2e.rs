//! T-127 end to end through the mock SDR: a scheduler-driven ScanPlan run with `extra.bandit`
//! attaches bandit dwells to a detected bursty emitter (an active candidate arm over it, visited,
//! with outcomes recorded), keeps the sweep floor, publishes the scheduler to the API hub (lease
//! create and release served by the control thread), and the observation log records
//! bandit-tier dwells with bandit reasons. With the flag absent the same run has no bandit and
//! logs no dwells with bandit reasons (v1 WRR POI dwells may still log in the bandit tier; the
//! default path is unchanged).
//!
//! T-128: the bandit's provider is the run's C12 scorer over blind tracks. A clipping (overloaded,
//! IMD-like) emitter is a suspect candidate: it gets its one verification group and, still flagged
//! afterwards, is banned rather than dwelt on.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::*;
use hk_core::scheduler::Lease;
use hk_core::{MockEnd, Pacing};
use hk_model::attention::observation::{LeaseKind, ObservationRecord, Reason, Tier};
use hk_model::{FreqRange, ScanPolicy, TimeRange};
use hk_pipeline::control::HubSnapshot;
use hk_pipeline::{Pipeline, PipelineConfig, TrackInventory, open_mock_replay, replay_plan};
use hk_store::observation::RecordQuery;

const FS: f64 = 2e6;
const CENTER: f64 = 433.92e6;
/// `tone_recording`'s tone offset.
const EMITTER: f64 = CENTER + 50e3;
const SECS: f64 = 12.0;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-t127-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The tone recording gated into 120 ms bursts every 600 ms (noise between bursts). With `clip`,
/// the bursts are driven 4× into the ADC rails (every burst detection is flagged clipped).
fn bursty_recording(dir: &Path, clip: bool) -> PathBuf {
    let meta = tone_recording(dir, "bursty", FS, SECS, CENTER, None);
    let data = dir.join("bursty.sigmf-data");
    let mut bytes = std::fs::read(&data).unwrap();
    let (period, on) = ((0.6 * FS) as usize, (0.12 * FS) as usize);
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for (i, pair) in bytes.chunks_exact_mut(2).enumerate() {
        if clip && i % period < on {
            for b in pair.iter_mut() {
                *b = ((*b as i8) as i32 * 4).clamp(-128, 127) as i8 as u8;
            }
        } else if i % period >= on {
            for b in pair {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b =
                    (((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0).round() as i8 as u8;
            }
        }
    }
    std::fs::write(&data, bytes).unwrap();
    meta
}

struct Run {
    records: Vec<ObservationRecord>,
    last: Option<Arc<HubSnapshot>>,
    lease_added: Option<Result<Lease, hk_pipeline::control::HubError>>,
    lease_released: Option<Result<bool, hk_pipeline::control::HubError>>,
}

fn run(dir: &TempDir, bandit: bool) -> Run {
    run_with(dir, bandit, false)
}

fn run_with(dir: &TempDir, bandit: bool, clip: bool) -> Run {
    let rec = bursty_recording(&dir.0.join("src"), clip);
    let replay = open_mock_replay(&rec, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let mut plan = replay_plan(
        replay.info.center_hz,
        replay.info.sample_rate_hz,
        replay.info.start_time,
    );
    plan.policy = ScanPolicy::SweepThenDwell;
    if bandit {
        let mut extra = plan.extra.as_object().cloned().unwrap_or_default();
        extra.insert(
            "bandit".into(),
            serde_json::json!({
                "min_dwell_s": 0.5,
                "max_dwell_s": 2.0,
                "sweep_floor_window_s": 10.0,
            }),
        );
        plan.extra = serde_json::Value::Object(extra);
    }
    let mut cfg = PipelineConfig::new(dir.0.join("data"), plan).unwrap();
    cfg.source_class = replay.class;
    cfg.lossless = true;
    cfg.drive_scheduler = true;
    cfg.device_id = replay.device.device_id.clone();
    let t0 = replay.info.start_time;
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let store = handle.observation_store().expect("the run opened its log");
    let hub = handle.scheduler_hub();

    // Polls the hub like the API does; once the scheduler is up, pins a lease and releases it.
    let done = Arc::new(AtomicBool::new(false));
    let poller = {
        let (done, hub) = (Arc::clone(&done), Arc::clone(&hub));
        std::thread::spawn(move || {
            let (mut last, mut added, mut released) = (None, None, None);
            let mut polls = 0u32;
            while !done.load(Ordering::Relaxed) {
                if let Some(s) = hub.snapshot() {
                    polls += 1;
                    if added.is_none() && polls > 50 {
                        added = Some(hub.add_lease(Lease {
                            id: 0,
                            kind: LeaseKind::UserPin,
                            center_hz: CENTER,
                            rate_hz: 0.0,
                            gains: None,
                            duration_ns: None,
                        }));
                    } else if released.is_none() && polls > 60 {
                        if let Some(Ok(l)) = &added {
                            released = Some(hub.release_lease(l.id));
                        }
                    }
                    last = Some(s);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            (last, added, released)
        })
    };
    let (_summary, stopped) = wait_guarded(handle, Duration::from_secs(300));
    done.store(true, Ordering::Relaxed);
    let (last, lease_added, lease_released) = poller.join().unwrap();
    assert!(!stopped, "the run finished on its own");
    assert!(
        hub.snapshot().is_none(),
        "the hub empties when the scheduler stops"
    );

    let span = TimeRange::new(
        t0.saturating_add_nanos(-3_600_000_000_000),
        t0.saturating_add_nanos(3_600_000_000_000),
    );
    let page = store.query(&RecordQuery {
        freq: FreqRange::new(0.0, 7e9),
        span,
        tier: None,
        cursor: 0,
        limit: 100_000,
    });
    assert!(page.next_cursor.is_none());
    Run {
        records: page.records,
        last,
        lease_added,
        lease_released,
    }
}

fn observed_s(records: &[ObservationRecord], tier: Tier) -> f64 {
    records
        .iter()
        .map(|r| match r {
            ObservationRecord::Dwell(d) if d.tier == tier => d.observed_s(),
            ObservationRecord::Sweep(s) if tier == Tier::BackgroundSweep => s
                .visits
                .iter()
                .map(|v| f64::from(v.observed_ms) / 1e3)
                .sum(),
            _ => 0.0,
        })
        .sum()
}

#[test]
fn bandit_on_attaches_dwells_to_the_bursty_emitter_keeps_the_floor_and_logs_reasons() {
    let dir = TempDir::new("on");
    let r = run(&dir, true);
    let snap = r.last.expect("the hub published snapshots");
    let status = snap.status;
    let bandit = status.bandit.expect("the plan flag enabled the bandit");
    eprintln!("bandit {bandit:?}");
    eprintln!("arms {:?}", snap.arms);
    assert!(bandit.counters.repacks > 0 && bandit.counters.outcomes > 0);

    // A candidate arm over the detected emitter, visited.
    let usable = 0.5 * FS * 0.8;
    let arm = snap
        .arms
        .iter()
        .find(|a| {
            a.active && !a.exploration && a.visits > 0 && (EMITTER - a.center_hz).abs() < usable
        })
        .expect("a visited candidate arm over the emitter");
    assert!(arm.lead.is_some());

    // The log: bandit-tier dwells with bandit reasons, some on that arm.
    let mut on_arm = 0;
    let mut bandit_dwells = 0;
    for rec in &r.records {
        if let ObservationRecord::Dwell(d) = rec {
            if d.tier != Tier::Bandit {
                continue;
            }
            bandit_dwells += 1;
            let a = match d.reason {
                Reason::Novelty { arm, .. }
                | Reason::Explore { arm }
                | Reason::BeaconDue { arm, .. } => arm,
                other => panic!("a bandit dwell with reason {other:?}"),
            };
            if a == arm.index {
                on_arm += 1;
            }
        }
    }
    assert!(bandit_dwells > 0 && on_arm > 0, "{bandit_dwells} {on_arm}");

    // The sweep floor holds over the run (tolerance for the last, partly planned window).
    let sweep = observed_s(&r.records, Tier::BackgroundSweep);
    let total = sweep
        + observed_s(&r.records, Tier::Bandit)
        + observed_s(&r.records, Tier::PinnedLease)
        + observed_s(&r.records, Tier::ScheduledPlan);
    assert!(
        sweep >= (bandit.config.sweep_floor - 0.05) * total,
        "sweep {sweep} s of {total} s"
    );

    // Lease create and release through the hub, served by the control thread.
    let lease = r
        .lease_added
        .expect("a lease was tried")
        .expect("lease added");
    assert!(lease.id >= 1 && lease.rate_hz == FS);
    assert_eq!(r.lease_released.map(Result::unwrap), Some(true));
}

#[test]
fn bandit_off_by_default_logs_no_bandit_dwells() {
    let dir = TempDir::new("off");
    let r = run(&dir, false);
    let snap = r.last.expect("the hub published snapshots");
    assert!(snap.status.bandit.is_none() && snap.arms.is_empty());
    // v1 WRR POI dwells share the bandit tier (ADR-0012 §5.4); bandit reasons never appear.
    assert!(r.records.iter().all(|rec| !matches!(
        rec,
        ObservationRecord::Dwell(d) if matches!(
            d.reason,
            Reason::Novelty { .. } | Reason::Explore { .. } | Reason::BeaconDue { .. }
        )
    )));
    assert!(observed_s(&r.records, Tier::BackgroundSweep) > 0.0);
}

/// T-128 (ADR-0012 §5.3): suspect flags reach the bandit through the C12 candidates. The clipped
/// emitter's candidate asks for verification, gets exactly one verification group, and is banned
/// when the republished set still flags it (a replay cannot clear it: the gain step is virtual).
#[test]
fn bandit_suspect_candidate_gets_one_verification_then_is_banned() {
    let dir = TempDir::new("suspect");
    let r = run_with(&dir, true, true);
    let snap = r.last.expect("the hub published snapshots");
    let bandit = snap
        .status
        .bandit
        .expect("the plan flag enabled the bandit");
    let c = &bandit.counters;
    eprintln!(
        "[T-128] suspect: verifications started {} failed {} passed {} dropped {}, banned {}, \
         pending {}, suspect wasted {:.2} s, provider v{:?}",
        c.verifications_started,
        c.verifications_failed,
        c.verifications_passed,
        c.verifications_dropped,
        bandit.banned,
        bandit.pending_verifications,
        c.suspect_wasted_s,
        bandit.provider_version
    );
    assert!(c.verifications_started >= 1, "a verification group ran");
    assert!(
        c.verifications_failed >= 1 && bandit.banned >= 1,
        "the still-suspect candidate is banned"
    );
    assert_eq!(
        c.verifications_passed, 0,
        "nothing cleared the clipped emitter"
    );
    // One verification per suspect candidate: never re-verified while banned.
    assert!(
        c.verifications_started <= c.verifications_failed + bandit.pending_verifications as u64,
        "re-verified while banned"
    );
}
