//! T-124 (AWARE-042, AWARE-044, AWARE-027): a time-compressed 56 h `occupancy_markov_scene`
//! replayed blind through one mock SDR (T-125) with the **bandit scheduler** driving the device
//! (`drive_scheduler`, `extra.bandit`), and the run's own services wired as `hk serve` wires them
//! (T-131): the user pins the parked device's site right after start; occupancy, baselines,
//! candidates and novelty alarms run on the live path.
//!
//! **The scene.** Markov channels, an hour-of-week channel, a 00Z/12Z event, a boring wideband band
//! and the injected channel: at 3 % FCO from hour 0 (so its channel is learned and its baseline
//! matures from the scene's own earlier hours, fed through the same device path; nothing is
//! seeded), then fully occupied from hour 30. At stream time recording start + 44 h the user raises
//! the device's VGA by 4 dB through its control interface (a wrapper over the device source calls
//! `set_gain` once block timestamps pass that time; the time comes from the recording's metadata,
//! not the truth). The run has no truth; the schedule is read only in the assertions.
//!
//! **Pass criteria (fixed before the first run):**
//! - (a) FCO: as T-118. Every truth channel with realized FCO ≥ 5 % has a learned channel row with
//!   `fco`; ≥ 5 channels matched; realized FCO inside the row's 95 % interval for ≥ 75 % of them;
//!   no phantom channel above 5 %.
//! - (b, g) the injected channel raises a `busier-than-usual` alarm no earlier than the injection
//!   and within [`MAX_ALARM_REVISITS`] scene revisits of it, listed with explanations whose top
//!   cause is not the device (`self-inflicted`).
//! - (c) false alarms ≤ [`MAX_FALSE_ALARMS`]: alarms not explained by provenance that are either
//!   off the injected channel, or on it before the injection.
//! - (h) the gain step is disclosed as a gain provenance step; the change is provenance-explained
//!   (a `provenance-explained` suppression or an anomaly explained by the step), and no
//!   unexplained alarm other than the injected channel's busier-than-usual is raised within
//!   [`GAIN_STEP_WINDOW_H`] after it.
//! - (d) the survey report over the pinned site validates, discloses coverage (not quiet, 4 POI
//!   rows, observed fraction in (0, 5 %), gaps) and compares against an `available` baseline with
//!   the injected channel busier; the bandit ran (outcomes recorded, bandit-tier dwells logged)
//!   and the sweep kept observing.
//!
//! Deferred to T-136 (product wiring in flight): see the `// T-136:` notes at the end.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use hk_core::{BlockHeader, Source, SourceCapabilities, SourceControl, SourceError};
use hk_e2e::scene::SceneTruth;
use hk_e2e::{SynthOutput, SynthRequest};
use hk_model::attention::alarm::AlarmKind;
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::observation::{ObservationRecord, Tier};
use hk_model::attention::occupancy::{OccupancyStat, OccupancySubject};
use hk_model::attention::report::{ComparisonStatus, ProvenanceStepKind, SurveyReport};
use hk_model::context::Cause;
use hk_model::repo::alarms::{AnomalyQuery, AnomalyView};
use hk_model::{FreqRange, Repository, ScanPolicy, TimeRange, Timestamp};
use hk_pipeline::attention::SiteSelect;
use hk_pipeline::reports::ReportService;
use hk_store::observation::RecordQuery;
use num_complex::{Complex, Complex32};
use serde_json::{Value, json};

use crate::blind::{blind_scene, start};
use crate::common::*;

const T124: &str = "T-124";
const HOUR_S: f64 = 3600.0;
const HOUR_NS: i64 = 3_600_000_000_000;
const SPAN_H: f64 = 56.0;
const INJECT_H: f64 = 30.0;
const PRE_FCO: f64 = 0.03;
/// The user's gain change, hours after the recording's first sample.
const GAIN_STEP_H: i64 = 44;
const REVISIT_MEAN_GAP_S: f64 = 120.0;
const SAMPLE_RATE: f64 = 500e3;
/// Window length: four mock SDR blocks (16 384 samples), longer than one history row at the
/// default 10 rows/s.
const WINDOW_S: f64 = (4 * 16_384) as f64 / SAMPLE_RATE;
/// The user's VGA after the step (the recording reads as VGA 20).
const VGA_AFTER_DB: f64 = 24.0;
/// (b): scene revisits from the injection to the alarm, at most (≈ 4 h at a 120 s mean gap).
const MAX_ALARM_REVISITS: usize = 120;
/// (c): false alarms, at most.
const MAX_FALSE_ALARMS: usize = 2;
/// (h): hours after the gain step in which no unexplained alarm may be raised.
const GAIN_STEP_WINDOW_H: f64 = 3.0;

/// The user's gain change on the device: `set_gain` through the device's control handle once the
/// block timestamps reach `at`.
struct GainAt {
    inner: Box<dyn Source>,
    control: Arc<dyn SourceControl>,
    at: Timestamp,
    applied: bool,
}

impl GainAt {
    fn check(&mut self, header: &Option<BlockHeader>) {
        let Some(h) = header else { return };
        if !self.applied && h.time.host_time.as_unix_nanos() >= self.at.as_unix_nanos() {
            self.applied = true;
            eprintln!("[{T124}] user gain step: vga {VGA_AFTER_DB} dB");
            self.control
                .set_gain("vga", VGA_AFTER_DB)
                .unwrap_or_else(|e| panic!("device refused the gain step: {e}"));
        }
    }
}

impl Source for GainAt {
    fn capabilities(&self) -> &SourceCapabilities {
        self.inner.capabilities()
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        Arc::clone(&self.control)
    }

    fn pausable(&self) -> bool {
        self.inner.pausable()
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        let h = self.inner.read_block(samples)?;
        self.check(&h);
        Ok(h)
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        let h = self.inner.read_block_ci8(samples)?;
        self.check(&h);
        Ok(h)
    }
}

/// Holds the device's place for the instant the source is moved into [`GainAt`]; never read.
struct Detached(SourceCapabilities);

impl Source for Detached {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.0
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        unreachable!("placeholder source")
    }

    fn pausable(&self) -> bool {
        true
    }

    fn read_block(&mut self, _: &mut Vec<Complex32>) -> Result<Option<BlockHeader>, SourceError> {
        Ok(None)
    }

    fn read_block_ci8(
        &mut self,
        _: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        Ok(None)
    }
}

/// What the one scene run produced, read after it finished.
pub struct M2Run {
    truth: SceneTruth,
    /// Scene time 0 on the stream clock.
    t_scene: Timestamp,
    /// The user's gain change (stream time).
    gain_at: Timestamp,
    lost_samples: u64,
    rows: Vec<OccupancyStat>,
    f_cell: f64,
    report: SurveyReport,
    anomalies: Vec<AnomalyView>,
    suppressions: Value,
    alarm_status: Value,
    bandit_outcomes: u64,
    bandit_dwells: usize,
    sweep_s: f64,
}

impl M2Run {
    fn scene_h(&self, t: Timestamp) -> f64 {
        (t.as_unix_nanos() - self.t_scene.as_unix_nanos()) as f64 / 1e9 / HOUR_S
    }

    fn at_injected(&self, f: &FreqRange) -> bool {
        let n = self.truth.channel("novelty").unwrap();
        f.lo_hz <= n.center_hz + n.bandwidth_hz / 2.0
            && f.hi_hz >= n.center_hz - n.bandwidth_hz / 2.0
    }

    fn raised(&self, a: &AnomalyView) -> Timestamp {
        a.listing
            .alarm
            .as_ref()
            .map_or(a.listing.anomaly.region.time.start, |r| r.raised_at)
    }

    fn kind(a: &AnomalyView) -> Option<AlarmKind> {
        a.listing.alarm.as_ref().map(|r| r.key.kind)
    }

    fn freq(a: &AnomalyView) -> FreqRange {
        a.listing
            .alarm
            .as_ref()
            .map_or(a.listing.anomaly.region.freq, |r| r.freq)
    }

    /// Explained by a device step (§7.4): the alarm row names the step, or the top cause is the
    /// device.
    fn explained(a: &AnomalyView) -> bool {
        a.listing
            .alarm
            .as_ref()
            .is_some_and(|r| r.explained_step_t.is_some())
            || matches!(
                a.explanations.first().map(|e| &e.cause),
                Some(Cause::SelfInflicted { .. })
            )
    }

    fn summary(&self, a: &AnomalyView) -> String {
        format!(
            "{:?} {:.1} kHz raised h{:.2} status {:?} explained {} top {:?}",
            Self::kind(a),
            Self::freq(a).center_hz() / 1e3,
            self.scene_h(self.raised(a)),
            a.listing.status,
            Self::explained(a),
            a.explanations
                .iter()
                .take(3)
                .map(|e| (&e.cause, (e.score * 100.0).round() / 100.0))
                .collect::<Vec<_>>()
        )
    }

    fn injected_busier(&self) -> Option<&AnomalyView> {
        self.anomalies
            .iter()
            .filter(|a| {
                Self::kind(a) == Some(AlarmKind::BusierThanUsual)
                    && self.at_injected(&Self::freq(a))
            })
            .min_by_key(|a| self.raised(a).as_unix_nanos())
    }
}

static RUN: LazyLock<Option<M2Run>> = LazyLock::new(run_scene);

/// The shared scene run (`None`: the generator is unavailable and not required).
fn scene() -> Option<&'static M2Run> {
    RUN.as_ref()
}

fn generate() -> Option<SynthOutput> {
    let request = SynthRequest::new("occupancy_markov_scene")
        .seed(7)
        .param(
            "span_hours",
            std::env::var("HK_T124_SPAN_H") // DIAG (removed before commit)
                .map_or(SPAN_H, |s| s.parse::<f64>().unwrap()),
        )
        .param("novelty_start_hour", INJECT_H)
        .param("novelty_pre_fco", PRE_FCO)
        .param("novelty_fco", 1.0)
        .param("sample_rate", SAMPLE_RATE)
        .param("iq_windows_at_revisits", "true")
        .param("revisit_mean_gap_s", REVISIT_MEAN_GAP_S)
        .param("window_duration_s", WINDOW_S);
    match request.generate() {
        Ok(out) => Some(out),
        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {}: {err}", module_path!());
            None
        }
        Err(err) => panic!("synthetic scenario generation failed: {err}"),
    }
}

fn run_scene() -> Option<M2Run> {
    let wall = Instant::now();
    let out = generate()?;
    let gen_s = wall.elapsed().as_secs_f64();

    let dir = TempDir::new("t124");
    let mut bandit = json!({
        "bandit": { "min_dwell_s": 0.5, "max_dwell_s": 2.0, "sweep_floor_window_s": 10.0 }
    });
    // DIAG (removed before commit): scheduler settings under test.
    if let Ok(s) = std::env::var("HK_T124_SCHED") {
        bandit["scheduler"] = serde_json::from_str(&s).unwrap();
    }
    let mut scene = blind_scene(&dir.0, &out, bandit);
    scene.cfg.drive_scheduler = true;
    scene.cfg.plan.policy = ScanPolicy::SweepThenDwell;
    let rec = scene.recording.clone();
    // The recording's own first-sample time (metadata, not truth).
    let meta = hk_model::sigmf::SigmfMeta::read(&rec.meta).unwrap();
    let t_first = hk_core::source::sigmf_replay::parse_sigmf_datetime(
        meta.captures[0].datetime.as_deref().unwrap(),
    )
    .unwrap();
    let gain_at = t_first.saturating_add_nanos(GAIN_STEP_H * HOUR_NS);
    let caps = scene.device.source.capabilities().clone();
    let inner = std::mem::replace(&mut scene.device.source, Box::new(Detached(caps)));
    scene.device.source = Box::new(GainAt {
        control: inner.control(),
        inner,
        at: gain_at,
        applied: false,
    });
    let (centre, rate) = (
        scene.device.info.center_hz,
        scene.device.info.sample_rate_hz,
    );
    let handle = start(scene.cfg, scene.device);
    // The user pins the parked device's site (`PUT /api/sites/current`), as in T-131.
    let attention = handle.attention().expect("the run's attention service");
    attention
        .set_current_site(SiteSelect {
            id: None,
            name: Some("bench".into()),
            lat_deg: Some(51.5),
            lon_deg: Some(-0.1),
            radius_m: None,
            utc_offset_min: Some(0),
            release: false,
        })
        .unwrap();
    let alarms = handle.alarms().expect("the run's alarm service");
    let occupancy = handle.occupancy();
    let product = handle.floor_product();
    let observations = handle.observation_store();
    let counters = handle.counters();
    // The scheduler hub, polled as the API does: the bandit's outcome counter.
    let hub = handle.scheduler_hub();
    let stop = Arc::new(AtomicBool::new(false));
    let poller = {
        let (hub, stop) = (Arc::clone(&hub), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut outcomes = 0;
            while !stop.load(Ordering::Relaxed) {
                if let Some(b) = hub.snapshot().and_then(|s| s.status.bandit.clone()) {
                    outcomes = outcomes.max(b.counters.outcomes);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            outcomes
        })
    };
    let summary = finish(handle);
    stop.store(true, Ordering::Relaxed);
    let bandit_outcomes = poller.join().unwrap();
    let run_s = wall.elapsed().as_secs_f64();
    // DIAG (removed before commit).
    eprintln!(
        "[{T124}] DIAG run {run_s:.1} s; history reader {}; scheduler {}",
        summary.counters.pointer("/readers/history").cloned().unwrap_or_default(),
        summary.counters.pointer("/scheduler").cloned().unwrap_or_default()
    );

    // ---- What the system knows: device tuning, the history's extent, the pinned site. ----
    let t_end = product
        .lock()
        .unwrap()
        .uncalibrated_pyramid()
        .latest_frame_end()
        .expect("[T-124] history holds frames");
    let (tuned, tuned_rate) = counters.tune();
    let band = FreqRange::centered(tuned, 0.9 * tuned_rate);
    let span = TimeRange::new(t_first, t_end.saturating_add_nanos(1_000_000_000));
    let rows = occupancy.span_stats(band, span).expect("span stats");
    let (_, f_cell) = occupancy.plan_info();

    let store = observations
        .clone()
        .expect("the run opened its observation log");
    let (mut records, mut cursor) = (Vec::new(), 0);
    loop {
        let page = store.query(&RecordQuery {
            freq: FreqRange::new(0.0, 7e9),
            span: TimeRange::new(
                t_first.saturating_add_nanos(-HOUR_NS),
                t_end.saturating_add_nanos(HOUR_NS),
            ),
            tier: None,
            cursor,
            limit: 100_000,
        });
        records.extend(page.records);
        match page.next_cursor {
            Some(c) => cursor = c,
            None => break,
        }
    }
    let (mut bandit_dwells, mut bandit_s, mut sweep_s) = (0, 0.0, 0.0);
    for r in &records {
        match r {
            ObservationRecord::Dwell(d) if d.tier == Tier::Bandit => {
                bandit_dwells += 1;
                bandit_s += d.observed_s();
            }
            ObservationRecord::Dwell(d) if d.tier == Tier::BackgroundSweep => {
                sweep_s += d.observed_s();
            }
            ObservationRecord::Sweep(s) => {
                sweep_s += s
                    .visits
                    .iter()
                    .map(|v| f64::from(v.observed_ms) / 1e3)
                    .sum::<f64>();
            }
            _ => {}
        }
    }

    let db = Arc::new(Mutex::new(
        Repository::open(dir.0.join("hackriff.db")).unwrap(),
    ));
    let (site, _) = attention.site_at(t_end);
    assert!(
        matches!(site, SiteKey::Site(_)),
        "[{T124}] pinned site {site:?}"
    );
    let service = ReportService::new(None, Some(product), observations, db)
        .with_attention(Some(occupancy), Some(Arc::clone(&attention)));
    let region = FreqRange::new(centre - rate / 2.0, centre + rate / 2.0);
    let report = service
        .report(region, TimeRange::new(t_first, t_end), site)
        .unwrap();
    let (anomalies, _) = alarms
        .list(&AnomalyQuery {
            limit: 10_000,
            ..Default::default()
        })
        .unwrap();

    // ---- Truth, for assertions only. ----
    let truth = SceneTruth::load(&out).unwrap();
    let t_scene = t_first.saturating_add_nanos(-((truth.window_starts_s[0] * 1e9).round() as i64));
    let run = M2Run {
        truth,
        t_scene,
        gain_at,
        lost_samples: summary.always_on_lost_samples,
        rows,
        f_cell,
        report,
        anomalies,
        suppressions: alarms.suppressions(),
        alarm_status: alarms.status_json(),
        bandit_outcomes,
        bandit_dwells,
        sweep_s,
    };
    eprintln!(
        "[{T124}] scene {:.1} h simulated, {} windows, {} IQ samples; wall: generation {gen_s:.1} \
         s, generation + run {run_s:.1} s, total {:.1} s (compression {:.0}x); lost {}; bandit \
         outcomes {}, {} bandit dwells ({bandit_s:.1} s), sweep {:.1} s; alarm status {}; \
         suppressions {}",
        rec.simulated_span_s / HOUR_S,
        rec.windows,
        rec.samples,
        wall.elapsed().as_secs_f64(),
        rec.simulated_span_s / run_s,
        run.lost_samples,
        run.bandit_outcomes,
        run.bandit_dwells,
        run.sweep_s,
        run.alarm_status,
        run.suppressions
    );
    for a in &run.anomalies {
        eprintln!("[{T124}] anomaly: {}", run.summary(a));
    }
    Some(run)
}

/// (a) AWARE-042: per-channel FCO within the confidence interval of the hidden truth.
#[test]
fn m2_fco_per_channel_within_ci_of_hidden_truth() {
    let Some(run) = scene() else { return };
    assert_eq!(run.lost_samples, 0, "[{T124}] readers lost samples");
    for r in &run.rows {
        r.validate().unwrap();
    }
    let per = run.truth.schedule["stats"]["per_channel"]
        .as_array()
        .unwrap();
    let extent = |r: &OccupancyStat| match r.subject {
        OccupancySubject::Channel { key } => Some(key.freq(run.f_cell)),
        OccupancySubject::Band { .. } => None,
    };
    let (mut matched, mut inside, mut missing) = (0, 0, Vec::new());
    for c in &run.truth.channels {
        let realized = per
            .iter()
            .find(|p| p["channel"].as_u64() == Some(c.channel))
            .and_then(|p| p["fco_realized"].as_f64())
            .unwrap();
        let row = run.rows.iter().find(|r| {
            r.fco.is_some()
                && extent(r).is_some_and(|f| f.lo_hz <= c.center_hz && c.center_hz <= f.hi_hz)
        });
        match row {
            Some(r) => {
                let ci = r.confidence.unwrap();
                let ok = realized >= ci.lo - 1e-9 && realized <= ci.hi + 1e-9;
                matched += 1;
                inside += usize::from(ok);
                eprintln!(
                    "[{T124}] fco {:<8} {:>10.4} MHz realized {realized:.4} measured {:.4} \
                     [{:.4}, {:.4}] n {} n_eff {:.1} inside {ok}",
                    c.kind,
                    c.center_hz / 1e6,
                    r.fco.unwrap(),
                    ci.lo,
                    ci.hi,
                    r.n_revisits,
                    ci.n_eff
                );
            }
            None => {
                eprintln!(
                    "[{T124}] fco {:<8} {:>10.4} MHz realized {realized:.4}: no learned channel",
                    c.kind,
                    c.center_hz / 1e6
                );
                if realized >= 0.05 {
                    missing.push(c.kind.clone());
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "[{T124}] busy channels not learned: {missing:?}"
    );
    assert!(matched >= 5, "[{T124}] only {matched} channels matched");
    assert!(
        inside * 4 >= matched * 3,
        "[{T124}] realized FCO inside the 95 % interval for {inside}/{matched} channels"
    );
    for r in &run.rows {
        let Some(f) = extent(r) else { continue };
        let holds = run.truth.channels.iter().any(|c| {
            let h = c.bandwidth_hz / 2.0;
            c.center_hz + h > f.lo_hz && c.center_hz - h < f.hi_hz
        });
        assert!(
            holds || r.fco.unwrap_or(0.0) <= 0.05,
            "[{T124}] phantom channel {:.4}-{:.4} MHz fco {:?}",
            f.lo_hz / 1e6,
            f.hi_hz / 1e6,
            r.fco
        );
    }
}

/// (b, g) AWARE-044: the injected channel (learned at low FCO from hour 0, its baseline matured by
/// the scene's own earlier hours) raises busier-than-usual within N revisits, with explanations.
#[test]
fn m2_injected_channel_alarmed_busier_than_usual_within_n_revisits() {
    let Some(run) = scene() else { return };
    let inject_s = run.truth.novelty_start_s;
    let alarm = run.injected_busier().unwrap_or_else(|| {
        panic!(
            "[{T124}] no busier-than-usual alarm on the injected channel; status {}, suppressions \
             {}",
            run.alarm_status, run.suppressions
        )
    });
    let raised_s = run.scene_h(run.raised(alarm)) * HOUR_S;
    let revisits = run
        .truth
        .window_starts_s
        .iter()
        .filter(|&&s| s >= inject_s && s <= raised_s)
        .count();
    eprintln!(
        "[{T124}] injected at h{:.2}; busier alarm raised h{:.2} after {revisits} revisits (max \
         {MAX_ALARM_REVISITS}): {}",
        inject_s / HOUR_S,
        raised_s / HOUR_S,
        run.summary(alarm)
    );
    assert!(
        raised_s >= inject_s - WINDOW_S,
        "[{T124}] busier-than-usual raised before the injection"
    );
    assert!(
        revisits <= MAX_ALARM_REVISITS,
        "[{T124}] alarmed after {revisits} revisits"
    );
    assert!(
        !alarm.explanations.is_empty(),
        "[{T124}] the alarm lists no explanations"
    );
    assert!(
        !M2Run::explained(alarm),
        "[{T124}] a real change on the channel was blamed on the device: {}",
        run.summary(alarm)
    );
}

/// (c): alarms not explained by the device, off the injected channel or before its injection.
#[test]
fn m2_false_alarms_bounded() {
    let Some(run) = scene() else { return };
    let inject = run
        .t_scene
        .saturating_add_nanos((run.truth.novelty_start_s * 1e9) as i64);
    let false_alarms: Vec<_> = run
        .anomalies
        .iter()
        .filter(|a| a.listing.alarm.is_some() && !M2Run::explained(a))
        .filter(|a| {
            !run.at_injected(&M2Run::freq(a))
                || run.raised(a).as_unix_nanos() < inject.as_unix_nanos()
        })
        .collect();
    eprintln!(
        "[{T124}] {} alarms, {} false (max {MAX_FALSE_ALARMS}): {:#?}",
        run.anomalies.len(),
        false_alarms.len(),
        false_alarms
            .iter()
            .map(|a| run.summary(a))
            .collect::<Vec<_>>()
    );
    assert!(
        false_alarms.len() <= MAX_FALSE_ALARMS,
        "[{T124}] {} false alarms",
        false_alarms.len()
    );
}

/// (h) AWARE-044: a gain step is a provenance-explained step, not a novelty alarm.
#[test]
fn m2_gain_step_is_provenance_explained_not_an_alarm() {
    let Some(run) = scene() else { return };
    let intended_h = run.scene_h(run.gain_at);
    let gains: Vec<_> = run
        .report
        .provenance_steps
        .iter()
        .filter(|s| s.kind == ProvenanceStepKind::Gain)
        .collect();
    eprintln!(
        "[{T124}] gain step at scene h{intended_h:.2}; report gain steps {:?}",
        gains
            .iter()
            .map(|s| (run.scene_h(s.t), s.detail.clone()))
            .collect::<Vec<_>>()
    );
    let step = gains
        .iter()
        .find(|s| (run.scene_h(s.t) - intended_h).abs() < 1.0)
        .expect("[T-124] the gain step is disclosed as a provenance step");
    let explained_suppressions: u64 = run
        .suppressions
        .as_object()
        .map(|by_kind| {
            by_kind
                .values()
                .filter_map(|s| s["provenance-explained"].as_u64())
                .sum()
        })
        .unwrap_or(0);
    let explained_anomalies = run
        .anomalies
        .iter()
        .filter(|a| M2Run::explained(a) && run.raised(a).as_unix_nanos() >= step.t.as_unix_nanos())
        .count();
    let window_end = step
        .t
        .saturating_add_nanos((GAIN_STEP_WINDOW_H * HOUR_S * 1e9) as i64);
    let injected_busier = run.injected_busier().map(|a| a.listing.anomaly.id);
    let unexplained_after: Vec<_> = run
        .anomalies
        .iter()
        .filter(|a| a.listing.alarm.is_some() && !M2Run::explained(a))
        .filter(|a| Some(a.listing.anomaly.id) != injected_busier)
        .filter(|a| {
            let t = run.raised(a).as_unix_nanos();
            t >= step.t.as_unix_nanos() && t <= window_end.as_unix_nanos()
        })
        .collect();
    eprintln!(
        "[{T124}] after the gain step (h{:.2}): provenance-explained suppressions \
         {explained_suppressions}, explained anomalies {explained_anomalies}, unexplained alarms \
         within {GAIN_STEP_WINDOW_H} h {:?}",
        run.scene_h(step.t),
        unexplained_after
            .iter()
            .map(|a| run.summary(a))
            .collect::<Vec<_>>()
    );
    assert!(
        explained_suppressions + explained_anomalies as u64 > 0,
        "[{T124}] the gain step's change was not explained by provenance: {}",
        run.suppressions
    );
    assert!(
        unexplained_after.is_empty(),
        "[{T124}] the gain step raised novelty alarms"
    );
}

/// (d) AWARE-042/AWARE-027: the survey report over the pinned site discloses coverage and POI and
/// compares against a mature baseline; the run was the bandit scheduler's.
#[test]
fn m2_survey_report_discloses_coverage_and_poi_under_the_bandit() {
    let Some(run) = scene() else { return };
    let report = &run.report;
    report.validate().unwrap();
    let c = &report.coverage;
    let cmp = &report.change_vs_baseline;
    eprintln!(
        "[{T124}] report: observed {:.4} % ({:.1} s), {} gaps{}, {} never-observed, poi {:?}, {} \
         provenance steps; change vs baseline {:?} ({:?}): {:?}; statement {:?}",
        c.observed_fraction * 100.0,
        c.observed_s,
        c.gaps.len(),
        if c.gaps_truncated { " (truncated)" } else { "" },
        c.never_observed.len(),
        c.poi.iter().map(|p| (p.tau_s, p.p_poi)).collect::<Vec<_>>(),
        report.provenance_steps.len(),
        cmp.status,
        cmp.resolution,
        cmp.changes
            .iter()
            .take(8)
            .map(|c| (c.kind, c.subject, (c.z * 10.0).round() / 10.0))
            .collect::<Vec<_>>(),
        c.statement
    );
    // The bandit drove the device, and the sweep floor kept activity-independent visits.
    assert!(
        run.bandit_outcomes > 0 && run.bandit_dwells > 0,
        "[{T124}] the bandit did not run"
    );
    assert!(
        run.sweep_s > 0.0,
        "[{T124}] no background sweep observation"
    );
    assert!(
        c.statement.contains("not quiet"),
        "[{T124}] {}",
        c.statement
    );
    assert_eq!(c.poi.len(), 4, "[{T124}] POI rows");
    assert!(
        c.observed_fraction > 0.0 && c.observed_fraction < 0.05,
        "[{T124}] short windows every ~2 min: {}",
        c.observed_fraction
    );
    assert!(!c.gaps.is_empty(), "[{T124}] no coverage gap disclosed");
    assert_eq!(
        cmp.status,
        ComparisonStatus::Available,
        "[{T124}] the matured baseline is compared"
    );
    let busier = cmp.changes.iter().any(|ch| {
        ch.kind == AlarmKind::BusierThanUsual
            && match ch.subject {
                OccupancySubject::Channel { key } => run.at_injected(&key.freq(run.f_cell)),
                OccupancySubject::Band { .. } => false,
            }
    });
    assert!(
        busier,
        "[{T124}] the report does not show the injected channel busier than its baseline"
    );
}

// T-136: `m2_new_emitter_alarms_once_fed` — a channel silent until the injection raises a
// `new-emitter` alarm once the inventory feeds first sightings to the alarms (needs T-136's
// wiring). Add it here as its own test over its own scene or over `scene()`'s data.
//
// T-136: `m2_restart_keeps_the_pinned_site` — stop the run mid-scene, restart on the same data
// dir, and assert the site pinned before the restart still keys occupancy rows and baselines.
