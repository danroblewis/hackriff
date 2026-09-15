//! T-124 (AWARE-042, AWARE-044, AWARE-027): time-compressed multi-day `occupancy_markov_scene`s
//! replayed blind through the mock SDR (T-125) with the **bandit scheduler** driving the device
//! under the default scheduler settings (`drive_scheduler`, no `extra.scheduler`; `extra.bandit`
//! as T-139 runs it). Each run's own services are wired as `hk serve` wires them (T-131): the user
//! pins the parked device's site right after start; occupancy, baselines, candidates and novelty
//! alarms run on the live path. The system never sees the scene's truth; only assertion code reads
//! it.
//!
//! **Revisit pattern (scene design, all scenes).** The scene's revisits are two IQ windows per
//! 15-min occupancy interval (ADR-0012 §2.8), [`EDGE_S`] inside its start and its end. Baseline
//! maturity (§3.2, 24 h) and the new-emitter rate (§7.1) accrue each interval's *represented*
//! time, `revisit_mean_s × n_revisits_all` capped at the interval (`represented_s`), so a window
//! near each end makes one interval of a parked device's day cost two short windows of IQ. Irregular
//! revisits would represent a fraction of each interval and need a proportionally longer (slower)
//! scene. Stream time jumps between windows (T-125).
//!
//! **Scenes.**
//! - `RUN` (a, b/g, c, d, h): four Markov channels, an hour-of-week channel, a 00Z/12Z event, a
//!   boring wideband band and the injected channel at [`PRE_FCO`] from hour 0 (learned and matured
//!   from the scene's own earlier hours through the device; nothing seeded; py synth hook
//!   `novelty_pre_fco`), fully occupied from [`INJECT_H`]. At recording start + [`GAIN_STEP_H`] the
//!   user raises the VGA through the device's control interface (time from the recording metadata).
//! - `m2_new_emitter_alarms_on_a_quiet_mature_site` (i): a quiet site, one emitter from
//!   [`NE_INJECT_H`].
//! - `m2_restart_keeps_the_pinned_site` (j): the pipeline stopped and restarted on the same data dir
//!   mid-scene.
//!
//! Thresholds are derived below from ADR-0012 and the scene design, fixed before the first green run.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use hk_context::occupancy::alarm::AlarmConfig;
use hk_context::occupancy::novelty::persistent_single_alpha;
use hk_core::{BlockHeader, Pacing, Source, SourceCapabilities, SourceControl, SourceError};
use hk_e2e::scene::{SceneTruth, join_scene_windows};
use hk_e2e::{SynthOutput, SynthRequest};
use hk_model::attention::alarm::AlarmKind;
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::observation::{ObservationRecord, Tier};
use hk_model::attention::occupancy::{OccupancyStat, OccupancySubject};
use hk_model::attention::report::{ComparisonStatus, ProvenanceStepKind, SurveyReport};
use hk_model::context::Cause;
use hk_model::repo::alarms::{AnomalyQuery, AnomalyView};
use hk_model::sigmf::SigmfMeta;
use hk_model::{FreqRange, Repository, ScanPolicy, TimeRange, Timestamp};
use hk_pipeline::attention::SiteSelect;
use hk_pipeline::reports::ReportService;
use hk_store::observation::RecordQuery;
use hk_store::occupancy::{OccupancyQuery, SeriesInterval};
use num_complex::{Complex, Complex32};
use serde_json::{Value, json};

use crate::blind::{replay_config, start};
use crate::common::*;

const T124: &str = "T-124";
const HOUR_S: f64 = 3600.0;
const HOUR_NS: i64 = 3_600_000_000_000;
const SAMPLE_RATE: f64 = 500e3;
/// Occupancy interval (ADR-0012 §2.8), s.
const INTERVAL_S: f64 = 900.0;
/// Each interval's two windows start `EDGE_S` after its start and end `EDGE_S` before its end.
const EDGE_S: f64 = 5.0;
/// `extra.bandit` as T-139's scheduler run (the scheduler settings stay default).
fn bandit_extra() -> Value {
    json!({ "bandit": { "min_dwell_s": 0.5, "max_dwell_s": 2.0, "sweep_floor_window_s": 10.0 } })
}
/// `max_dwell_s` above: the longest step a window can start.
const MAX_DWELL_S: f64 = 2.0;

// ---- Main scene ----
/// Pools mature at 24 h represented (§3.2) counted from when the injected channel is learned (its
/// first on-window at [`PRE_FCO`], expected within a few hours): injection at 36 h leaves ≥ 8 h.
const SPAN_H: f64 = 46.0;
const INJECT_H: f64 = 36.0;
const PRE_FCO: f64 = 0.05;
/// The user's gain change, hours after the recording's first sample.
const GAIN_STEP_H: i64 = 42;
/// Window length, s. A bandit dwell is admitted only while the background sweep keeps its floor
/// share of radio time over the floor window (`bandit_slot`: discovery ≥ 0.25 · (total + dwell)).
/// An arm with no known burst interval dwells `4 · min_dwell_s` = 2 s, admitted once ≥ 2/3 s has
/// been swept in the window (an interval's second window, 10 s after the first, may still see the
/// first one's dwell in the floor window). One second leaves room for the sweep, then a dwell.
const WINDOW_S: f64 = 1.0;
/// The user's VGA after the step (the recording reads as VGA 20).
const VGA_AFTER_DB: f64 = 24.0;

// ---- New-emitter scene (i) ----
/// T-138's rate gate: at the confirming close μ = 3600 · (k + PRIOR) / observed_s must be ≤ μ*
/// = −ln(1 − α(0.7)) ≈ 0.0113, where k counts the site's accrued first sightings including this
/// one (k = 1 on a quiet site) and PRIOR = 1: observed ≥ 7 200 / 0.0113 s ≈ 7.4 days. Nine days
/// before the injection (≈ 22 % margin for the first intervals and the last window's close).
const NE_INJECT_H: f64 = 216.0;
/// The alarm needs `on_intervals` (2) closes after the injection; one more interval of margin.
const NE_SPAN_H: f64 = 217.5;
/// Sweep-only windows (no dwell needed for a first sighting); short to keep 9 days cheap.
const NE_WINDOW_S: f64 = 0.25;

// ---- Restart scene (j) ----
const RS_SPAN_H: f64 = 3.0;
const RS_RESTART_H: f64 = 1.5;
const RS_WINDOW_S: f64 = 0.5;

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

// ---- Scene generation ----

/// Revisit times (s) of the two-windows-per-interval pattern over `span_h`.
fn edge_revisits(span_h: f64, window_s: f64) -> String {
    let n = (span_h * HOUR_S / INTERVAL_S).floor() as usize;
    (0..n)
        .flat_map(|q| {
            let t0 = q as f64 * INTERVAL_S;
            [t0 + EDGE_S, t0 + INTERVAL_S - EDGE_S - window_s]
        })
        .map(|t| format!("{t:.3}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn generate(request: SynthRequest) -> Option<SynthOutput> {
    match request.generate() {
        Ok(out) => Some(out),
        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {}: {err}", module_path!());
            None
        }
        Err(err) => panic!("synthetic scenario generation failed: {err}"),
    }
}

fn scene_request(span_h: f64, window_s: f64) -> SynthRequest {
    SynthRequest::new("occupancy_markov_scene")
        .seed(7)
        .param("span_hours", span_h)
        .param("sample_rate", SAMPLE_RATE)
        .param("iq_windows_at_revisits", "true")
        .param("revisit_mode", "given")
        .param("revisit_times_s", edge_revisits(span_h, window_s))
        .param("window_duration_s", window_s)
}

/// The first sample time of a SigMF recording (metadata, not truth).
fn first_sample_time(meta: &Path) -> Timestamp {
    let meta = SigmfMeta::read(meta).unwrap();
    hk_core::source::sigmf_replay::parse_sigmf_datetime(
        meta.captures[0].datetime.as_deref().unwrap(),
    )
    .unwrap()
}

/// Splits a joined scene recording at its first capture starting at or after `at`, into two
/// recordings under `dir` (the device replays each as its own capture session).
fn split_recording(meta_path: &Path, at: Timestamp, dir: &Path) -> (PathBuf, PathBuf) {
    let meta = SigmfMeta::read(meta_path).unwrap();
    let bps = meta.global.datatype.bytes_per_sample();
    let data = std::fs::read(meta_path.with_extension("sigmf-data")).unwrap();
    let k = meta
        .captures
        .iter()
        .position(|c| {
            hk_core::source::sigmf_replay::parse_sigmf_datetime(c.datetime.as_deref().unwrap())
                .unwrap()
                .as_unix_nanos()
                >= at.as_unix_nanos()
        })
        .expect("a capture after the split");
    assert!(k > 0, "split before the first capture");
    let s0 = meta.captures[k].sample_start;
    let mut parts = Vec::new();
    for (name, range, caps) in [
        ("part1", 0..(s0 as usize * bps), meta.captures[..k].to_vec()),
        ("part2", (s0 as usize * bps)..data.len(), meta.captures[k..].to_vec()),
    ] {
        let mut m = meta.clone();
        m.captures = caps;
        for c in &mut m.captures {
            c.sample_start -= if name == "part2" { s0 } else { 0 };
        }
        let path = dir.join(format!("{name}.sigmf-meta"));
        m.write(&path).unwrap();
        std::fs::write(path.with_extension("sigmf-data"), &data[range]).unwrap();
        parts.push(path);
    }
    (parts[0].clone(), parts[1].clone())
}

// ---- One device run ----

/// What one blind device run produced, read after it finished.
struct DeviceRun {
    t_first: Timestamp,
    t_end: Timestamp,
    lost_samples: u64,
    /// Whole-span channel/band rows over the device band.
    rows: Vec<OccupancyStat>,
    /// 15-min rows over the run's span.
    interval_rows: Vec<OccupancyStat>,
    f_cell: f64,
    site: SiteKey,
    current_site: Value,
    report: SurveyReport,
    anomalies: Vec<AnomalyView>,
    suppressions: Value,
    alarm_status: Value,
    bandit_outcomes: u64,
    bandit_dwells: usize,
    bandit_s: f64,
    sweep_s: f64,
    run_s: f64,
}

/// Runs `meta` blind through the mock SDR under the bandit scheduler on data dir `dir`. `pin`: the
/// user pins the parked device's site right after start (`PUT /api/sites/current`, T-131).
fn run_device(dir: &Path, meta: &Path, gain_at: Option<Timestamp>, pin: bool) -> DeviceRun {
    let wall = Instant::now();
    let (mut cfg, mut device) = replay_config(dir, meta, bandit_extra(), Pacing::Unpaced);
    cfg.drive_scheduler = true;
    cfg.plan.policy = ScanPolicy::SweepThenDwell;
    let t_first = first_sample_time(meta);
    if let Some(at) = gain_at {
        let caps = device.source.capabilities().clone();
        let inner = std::mem::replace(&mut device.source, Box::new(Detached(caps)));
        device.source = Box::new(GainAt {
            control: inner.control(),
            inner,
            at,
            applied: false,
        });
    }
    let (centre, rate) = (device.info.center_hz, device.info.sample_rate_hz);
    let handle = start(cfg, device);
    let attention = handle.attention().expect("the run's attention service");
    if pin {
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
    }
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
    // `span_stats` answers at most 7 days (the API's cap); the 9-day scene asks for its last 7.
    let week_ns = 7 * 24 * HOUR_NS;
    let stats_span = TimeRange::new(
        Timestamp::from_unix_nanos(
            t_first
                .as_unix_nanos()
                .max(span.end.as_unix_nanos() - week_ns),
        ),
        span.end,
    );
    let rows = occupancy.span_stats(band, stats_span).expect("span stats");
    let (_, f_cell) = occupancy.plan_info();
    let interval_rows = occupancy
        .query(&OccupancyQuery {
            freq: band,
            span,
            interval: SeriesInterval::Min15,
            subject: None,
            f_cell_hz: f_cell,
            limit: 1_000_000,
        })
        .expect("interval rows")
        .rows;

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
        Repository::open(dir.join("hackriff.db")).unwrap(),
    ));
    let (site, _) = attention.site_at(t_end);
    let current_site = attention.current_site_json();
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
    let run = DeviceRun {
        t_first,
        t_end,
        lost_samples: summary.always_on_lost_samples,
        rows,
        interval_rows,
        f_cell,
        site,
        current_site,
        report,
        anomalies,
        suppressions: alarms.suppressions(),
        alarm_status: alarms.status_json(),
        bandit_outcomes,
        bandit_dwells,
        bandit_s,
        sweep_s,
        run_s: wall.elapsed().as_secs_f64(),
    };
    eprintln!(
        "[{T124}] run {:.1} s; lost {}; bandit outcomes {}, {} bandit dwells ({:.1} s), sweep \
         {:.1} s; site {:?}; alarm status {}",
        run.run_s,
        run.lost_samples,
        run.bandit_outcomes,
        run.bandit_dwells,
        run.bandit_s,
        run.sweep_s,
        run.site,
        run.alarm_status
    );
    run
}

// ---- Alarm helpers ----

fn raised(a: &AnomalyView) -> Timestamp {
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

/// Explained by a device step (§7.4): the alarm row names the step, or the top cause is the device.
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

/// Scene seconds of stream time `t`, given the recording's first sample at scene `starts[0]`.
fn scene_s(truth: &SceneTruth, t_first: Timestamp, t: Timestamp) -> f64 {
    (t.as_unix_nanos() - t_first.as_unix_nanos()) as f64 / 1e9 + truth.window_starts_s[0]
}

fn summary(truth: &SceneTruth, t_first: Timestamp, a: &AnomalyView) -> String {
    format!(
        "{:?} {:.1} kHz raised h{:.2} status {:?} explained {} top {:?}",
        kind(a),
        freq(a).center_hz() / 1e3,
        scene_s(truth, t_first, raised(a)) / HOUR_S,
        a.listing.status,
        explained(a),
        a.explanations
            .iter()
            .take(3)
            .map(|e| (&e.cause, (e.score * 100.0).round() / 100.0))
            .collect::<Vec<_>>()
    )
}

fn at_channel(truth: &SceneTruth, kind: &str, f: &FreqRange) -> bool {
    let n = truth.channel(kind).unwrap();
    f.lo_hz <= n.center_hz + n.bandwidth_hz / 2.0 && f.hi_hz >= n.center_hz - n.bandwidth_hz / 2.0
}

/// (b, i) The revisit budget for an alarm on a change at scene time `inject_s`, derived from the
/// ADR-0012 §7.2 hysteresis: the change fills the first interval starting at or after it (the
/// scenes inject on an interval boundary), each close scores it ≥ on, and the alarm raises at the
/// `on_intervals`-th close. One more interval is allowed for the close running behind the history
/// (the occupancy close waits on its interval's frames), and one window for the close's trigger:
/// N = windows starting in [inject, boundary + (on_intervals + 1) · interval] + 1.
fn max_alarm_revisits(truth: &SceneTruth, inject_s: f64) -> usize {
    let on_intervals = AlarmConfig::default().hysteresis.on_intervals as f64;
    let boundary = (inject_s / INTERVAL_S).ceil() * INTERVAL_S;
    let end = boundary + (on_intervals + 1.0) * INTERVAL_S;
    truth
        .window_starts_s
        .iter()
        .filter(|&&s| s >= inject_s - 1e-6 && s <= end)
        .count()
        + 1
}

fn revisits_between(truth: &SceneTruth, from_s: f64, to_s: f64) -> usize {
    truth
        .window_starts_s
        .iter()
        .filter(|&&s| s >= from_s - 1e-6 && s <= to_s)
        .count()
}

/// Smallest k with P(X > k) < `tail` for X ~ Poisson(`mean`).
fn poisson_upper(mean: f64, tail: f64) -> usize {
    let (mut k, mut p) = (0usize, (-mean).exp());
    let mut cdf = p;
    while 1.0 - cdf >= tail {
        k += 1;
        p *= mean / k as f64;
        cdf += p;
    }
    k
}

/// Smallest k with P(X > k) < `tail` for X ~ Binomial(`n`, `p`).
fn binomial_upper(n: usize, p: f64, tail: f64) -> usize {
    let pmf = |k: usize| {
        let ln_c = (1..=k).map(|i| ((n - k + i) as f64 / i as f64).ln()).sum::<f64>();
        (ln_c + k as f64 * p.ln() + (n - k) as f64 * (1.0 - p).ln()).exp()
    };
    let mut cdf = 0.0;
    for k in 0..=n {
        cdf += pmf(k);
        if 1.0 - cdf < tail {
            return k;
        }
    }
    n
}

/// (c, i) The false-alarm bound, derived from ADR-0012 §7: novelty = −log10(p)/6, so a scored
/// input reaches the on level (0.7) under no change with probability ≤ 10^(−6·on); raising needs
/// `on_intervals` consecutive such closes, so ≤ 10^(−6·on) per scored input is conservative. With
/// `inputs_mature` scored inputs the unexplained raises are ≲ Poisson(inputs · 10^(−6·on)); the
/// bound is its 99 % upper quantile.
fn false_alarm_bound(alarm_status: &Value) -> (u64, usize) {
    let on = AlarmConfig::default().hysteresis.on;
    let trials = alarm_status["inputs_mature"].as_u64().unwrap_or(0);
    (
        trials,
        poisson_upper(trials as f64 * 10f64.powf(-6.0 * on), 0.01),
    )
}

/// Wilson score interval at z for a fraction `p` over `n_eff` effective samples (ADR-0012 §2.4).
fn wilson(p: f64, n_eff: f64, z: f64) -> (f64, f64) {
    let z2 = z * z;
    let d = 1.0 + z2 / n_eff;
    let c = (p + z2 / (2.0 * n_eff)) / d;
    let h = z / d * (p * (1.0 - p) / n_eff + z2 / (4.0 * n_eff * n_eff)).sqrt();
    ((c - h).max(0.0), (c + h).min(1.0))
}

/// Length of ∪ [s − before, s + after] over `starts`, clipped to [lo, hi].
fn union_s(starts: &[f64], before: f64, after: f64, lo: f64, hi: f64) -> f64 {
    let (mut total, mut cur): (f64, Option<(f64, f64)>) = (0.0, None);
    for &s in starts {
        let (a, b) = ((s - before).max(lo), (s + after).min(hi));
        if b <= a {
            continue;
        }
        cur = match cur {
            Some((ca, cb)) if a <= cb => Some((ca, cb.max(b))),
            Some((ca, cb)) => {
                total += cb - ca;
                Some((a, b))
            }
            None => Some((a, b)),
        };
    }
    total + cur.map_or(0.0, |(a, b)| b - a)
}

// ---- The main scene ----

struct M2Run {
    truth: SceneTruth,
    /// The user's gain change (stream time).
    gain_at: Timestamp,
    windows: usize,
    run: DeviceRun,
}

impl M2Run {
    fn h(&self, t: Timestamp) -> f64 {
        scene_s(&self.truth, self.run.t_first, t) / HOUR_S
    }

    fn at_injected(&self, f: &FreqRange) -> bool {
        at_channel(&self.truth, "novelty", f)
    }

    fn summary(&self, a: &AnomalyView) -> String {
        summary(&self.truth, self.run.t_first, a)
    }

    fn injected_busier(&self) -> Option<&AnomalyView> {
        self.run
            .anomalies
            .iter()
            .filter(|a| {
                kind(a) == Some(AlarmKind::BusierThanUsual) && self.at_injected(&freq(a))
            })
            .min_by_key(|a| raised(a).as_unix_nanos())
    }
}

static RUN: LazyLock<Option<M2Run>> = LazyLock::new(run_scene);

/// The shared scene run (`None`: the generator is unavailable and not required).
fn scene() -> Option<&'static M2Run> {
    RUN.as_ref()
}

fn run_scene() -> Option<M2Run> {
    let wall = Instant::now();
    let out = generate(
        scene_request(SPAN_H, WINDOW_S)
            .param("novelty_start_hour", INJECT_H)
            .param("novelty_pre_fco", PRE_FCO)
            .param("novelty_fco", 1.0),
    )?;
    let gen_s = wall.elapsed().as_secs_f64();
    let src = TempDir::new("t124src");
    let rec = join_scene_windows(&out, &src.0, "scene").unwrap();
    let dir = TempDir::new("t124");
    let t_first = first_sample_time(&rec.meta);
    let gain_at = t_first.saturating_add_nanos(GAIN_STEP_H * HOUR_NS);
    let run = run_device(&dir.0, &rec.meta, Some(gain_at), true);
    let truth = SceneTruth::load(&out).unwrap();
    eprintln!(
        "[{T124}] main scene {:.1} h simulated, {} windows, {} IQ samples; wall: generation \
         {gen_s:.1} s, run {:.1} s (compression {:.0}x)",
        rec.simulated_span_s / HOUR_S,
        rec.windows,
        rec.samples,
        run.run_s,
        rec.simulated_span_s / run.run_s
    );
    let m = M2Run {
        truth,
        gain_at,
        windows: rec.windows,
        run,
    };
    for a in &m.run.anomalies {
        eprintln!("[{T124}] anomaly: {}", m.summary(a));
    }
    Some(m)
}

/// (a) AWARE-042: per-channel FCO within the confidence interval of the hidden truth.
///
/// Derivation: each learned channel row reports `fco` over its activity-independent visits and the
/// §2.4 `n_eff`; the Wilson 95 % interval from them (z = 1.96) holds the realized FCO with
/// probability 0.95 per channel. Of m matched channels the outside count is ≲ Binomial(m, 0.05); at
/// most its 99 % upper quantile may fall outside. Every truth channel with realized FCO ≥ 5 % must
/// be learned (channels are published by confidence or persistence, §2.7); none may be a phantom.
#[test]
fn m2_fco_per_channel_within_ci_of_hidden_truth() {
    let Some(m) = scene() else { return };
    let run = &m.run;
    assert_eq!(run.lost_samples, 0, "[{T124}] readers lost samples");
    for r in &run.rows {
        r.validate().unwrap();
    }
    let per = m.truth.schedule["stats"]["per_channel"]
        .as_array()
        .unwrap();
    let extent = |r: &OccupancyStat| match r.subject {
        OccupancySubject::Channel { key } => Some(key.freq(run.f_cell)),
        OccupancySubject::Band { .. } => None,
    };
    let (mut matched, mut outside, mut missing, mut busy) = (0, 0, Vec::new(), 0);
    for c in &m.truth.channels {
        let realized = per
            .iter()
            .find(|p| p["channel"].as_u64() == Some(c.channel))
            .and_then(|p| p["fco_realized"].as_f64())
            .unwrap();
        busy += usize::from(realized >= 0.05);
        let row = run.rows.iter().find(|r| {
            r.fco.is_some()
                && extent(r).is_some_and(|f| f.lo_hz <= c.center_hz && c.center_hz <= f.hi_hz)
        });
        match row {
            Some(r) => {
                let n_eff = r.confidence.unwrap().n_eff;
                let (lo, hi) = wilson(r.fco.unwrap(), n_eff, 1.959_964);
                let ok = realized >= lo - 1e-9 && realized <= hi + 1e-9;
                matched += 1;
                outside += usize::from(!ok);
                eprintln!(
                    "[{T124}] fco {:<8} {:>10.4} MHz realized {realized:.4} measured {:.4} \
                     Wilson95 [{lo:.4}, {hi:.4}] n {} n_eff {n_eff:.1} inside {ok}",
                    c.kind,
                    c.center_hz / 1e6,
                    r.fco.unwrap(),
                    r.n_revisits,
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
    let allowed = binomial_upper(matched, 0.05, 0.01);
    eprintln!(
        "[{T124}] fco: {matched} matched ({busy} busy), {outside} outside (allowed {allowed})"
    );
    assert!(
        missing.is_empty(),
        "[{T124}] busy channels not learned: {missing:?}"
    );
    assert!(matched >= busy, "[{T124}] only {matched} channels matched");
    assert!(
        outside <= allowed,
        "[{T124}] realized FCO outside the Wilson 95 % interval for {outside}/{matched} channels"
    );
    for r in &run.rows {
        let Some(f) = extent(r) else { continue };
        let holds = m.truth.channels.iter().any(|c| {
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
/// the scene's own earlier hours) raises busier-than-usual within N revisits
/// ([`max_alarm_revisits`]), with explanations whose top cause is not the device.
#[test]
fn m2_injected_channel_alarmed_busier_than_usual_within_n_revisits() {
    let Some(m) = scene() else { return };
    let inject_s = m.truth.novelty_start_s;
    let max_revisits = max_alarm_revisits(&m.truth, inject_s);
    let alarm = m.injected_busier().unwrap_or_else(|| {
        panic!(
            "[{T124}] no busier-than-usual alarm on the injected channel; status {}",
            m.run.alarm_status
        )
    });
    let raised_s = scene_s(&m.truth, m.run.t_first, raised(alarm));
    let revisits = revisits_between(&m.truth, inject_s, raised_s);
    eprintln!(
        "[{T124}] injected at h{:.2}; busier alarm raised h{:.2} after {revisits} revisits (max \
         {max_revisits}): {}",
        inject_s / HOUR_S,
        raised_s / HOUR_S,
        m.summary(alarm)
    );
    assert!(
        raised_s >= inject_s,
        "[{T124}] busier-than-usual raised before the injection"
    );
    assert!(
        revisits <= max_revisits,
        "[{T124}] alarmed after {revisits} revisits (max {max_revisits})"
    );
    assert!(
        !alarm.explanations.is_empty(),
        "[{T124}] the alarm lists no explanations"
    );
    assert!(
        !explained(alarm),
        "[{T124}] a real change on the channel was blamed on the device: {}",
        m.summary(alarm)
    );
}

/// Unexplained alarms off the injected channel, or on it before the injection.
fn main_false_alarms(m: &M2Run) -> Vec<&AnomalyView> {
    let inject_s = m.truth.novelty_start_s;
    m.run
        .anomalies
        .iter()
        .filter(|a| a.listing.alarm.is_some() && !explained(a))
        .filter(|a| {
            !m.at_injected(&freq(a)) || scene_s(&m.truth, m.run.t_first, raised(a)) < inject_s
        })
        .collect()
}

/// (c): false alarms within [`false_alarm_bound`].
#[test]
fn m2_false_alarms_bounded() {
    let Some(m) = scene() else { return };
    let false_alarms = main_false_alarms(m);
    let (trials, bound) = false_alarm_bound(&m.run.alarm_status);
    eprintln!(
        "[{T124}] {} alarms, {} false (bound {bound} from {trials} mature inputs): {:#?}",
        m.run.anomalies.len(),
        false_alarms.len(),
        false_alarms.iter().map(|a| m.summary(a)).collect::<Vec<_>>()
    );
    assert!(trials > 0, "[{T124}] no mature alarm input was scored");
    assert!(
        false_alarms.len() <= bound,
        "[{T124}] {} false alarms (bound {bound})",
        false_alarms.len()
    );
}

/// (h) AWARE-044: a gain step is a provenance-explained step, not a novelty alarm. The window in
/// which the step could raise an alarm is the same hysteresis horizon as (b): (on_intervals + 1)
/// intervals after the step.
#[test]
fn m2_gain_step_is_provenance_explained_not_an_alarm() {
    let Some(m) = scene() else { return };
    let run = &m.run;
    let intended_h = m.h(m.gain_at);
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
            .map(|s| (m.h(s.t), s.detail.clone()))
            .collect::<Vec<_>>()
    );
    let step = gains
        .iter()
        .find(|s| (m.h(s.t) - intended_h).abs() < INTERVAL_S / HOUR_S)
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
        .filter(|a| explained(a) && raised(a).as_unix_nanos() >= step.t.as_unix_nanos())
        .count();
    let on_intervals = f64::from(AlarmConfig::default().hysteresis.on_intervals);
    let window_end = step
        .t
        .saturating_add_nanos(((on_intervals + 1.0) * INTERVAL_S * 1e9) as i64);
    let injected_busier = m.injected_busier().map(|a| a.listing.anomaly.id);
    let unexplained_after: Vec<_> = run
        .anomalies
        .iter()
        .filter(|a| a.listing.alarm.is_some() && !explained(a))
        .filter(|a| Some(a.listing.anomaly.id) != injected_busier)
        .filter(|a| {
            let t = raised(a).as_unix_nanos();
            t >= step.t.as_unix_nanos() && t <= window_end.as_unix_nanos()
        })
        .collect();
    eprintln!(
        "[{T124}] after the gain step (h{:.2}): provenance-explained suppressions \
         {explained_suppressions}, explained anomalies {explained_anomalies}, unexplained alarms \
         {:?}; suppressions {}",
        m.h(step.t),
        unexplained_after
            .iter()
            .map(|a| m.summary(a))
            .collect::<Vec<_>>(),
        run.suppressions
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
///
/// Coverage derivation (ADR-0012 §5.5, §6.2): the radio can only observe inside the scene's
/// windows, and a step's observed interval is clamped to its planned end on the stream clock, so a
/// dwell started inside a window may claim up to `max_dwell_s` past its last sample (the stream
/// jumps to the next window; time compression only). The sweep observes at least the first half of
/// every window (≥ 2/3 s swept before a dwell is admitted). Hence observed_s ∈ [Σ W/2, Σ (W +
/// max_dwell)] and, for each POI τ, P_POI ∈ [|∪[s−τ, s+W/2]|, |∪[s−τ, s+W+max_dwell]|] / span.
#[test]
fn m2_survey_report_discloses_coverage_and_poi_under_the_bandit() {
    let Some(m) = scene() else { return };
    let run = &m.run;
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
        "[{T124}] the bandit did not dwell: {} outcomes, {} dwells",
        run.bandit_outcomes,
        run.bandit_dwells
    );
    assert!(run.sweep_s > 0.0, "[{T124}] no background sweep observation");
    assert!(
        c.statement.contains("not quiet"),
        "[{T124}] {}",
        c.statement
    );
    assert_eq!(c.poi.len(), 4, "[{T124}] POI rows for τ ∈ {{5 ms, 100 ms, 1 s, 10 s}}");
    let n = m.windows as f64;
    let (obs_lo, obs_hi) = (n * WINDOW_S / 2.0, n * (WINDOW_S + MAX_DWELL_S));
    assert!(
        c.observed_s >= obs_lo && c.observed_s <= obs_hi,
        "[{T124}] observed {} s outside [{obs_lo}, {obs_hi}]",
        c.observed_s
    );
    let span_lo = scene_s(&m.truth, run.t_first, report.span.start);
    let span_hi = scene_s(&m.truth, run.t_first, report.span.end);
    let span = span_hi - span_lo;
    let starts = &m.truth.window_starts_s;
    for p in &c.poi {
        let lo = union_s(starts, p.tau_s, WINDOW_S / 2.0, span_lo, span_hi) / span;
        let hi = union_s(starts, p.tau_s, WINDOW_S + MAX_DWELL_S, span_lo, span_hi) / span;
        eprintln!(
            "[{T124}] poi τ {} s: {:.5} in [{lo:.5}, {hi:.5}]",
            p.tau_s, p.p_poi
        );
        assert!(
            p.p_poi >= lo - 1e-3 && p.p_poi <= hi + 1e-3,
            "[{T124}] P_POI(τ {} s) {} outside [{lo}, {hi}]",
            p.tau_s,
            p.p_poi
        );
    }
    assert!(!c.gaps.is_empty(), "[{T124}] no coverage gap disclosed");
    assert_eq!(
        cmp.status,
        ComparisonStatus::Available,
        "[{T124}] the matured baseline is compared"
    );
    let busier = cmp.changes.iter().any(|ch| {
        ch.kind == AlarmKind::BusierThanUsual
            && match ch.subject {
                OccupancySubject::Channel { key } => m.at_injected(&key.freq(run.f_cell)),
                OccupancySubject::Band { .. } => false,
            }
    });
    assert!(
        busier,
        "[{T124}] the report does not show the injected channel busier than its baseline"
    );
}

/// (i) AWARE-044 (T-136/T-138): a single persistent new emitter appearing on a quiet, mature site
/// raises one `new-emitter` alarm within N revisits ([`max_alarm_revisits`]), on its channel, with
/// explanations whose top cause is not the device; no other unexplained alarm beyond
/// [`false_alarm_bound`].
#[test]
fn m2_new_emitter_alarms_on_a_quiet_mature_site() {
    // A quiet site: no Markov channels, the hour-of-week channel never on, no events, the boring
    // band far below the noise; one emitter always on from NE_INJECT_H.
    let Some(out) = generate(
        scene_request(NE_SPAN_H, NE_WINDOW_S)
            .param("markov_fcos", "")
            .param("diurnal_target_fco", 1e-9)
            .param("event_hours_utc", "")
            .param("boring_power_dbfs", -120.0)
            .param("novelty_start_hour", NE_INJECT_H)
            .param("novelty_fco", 1.0),
    ) else {
        return;
    };
    let src = TempDir::new("t124nesrc");
    let rec = join_scene_windows(&out, &src.0, "quiet").unwrap();
    let dir = TempDir::new("t124ne");
    let run = run_device(&dir.0, &rec.meta, None, true);
    let truth = SceneTruth::load(&out).unwrap();
    let alpha = persistent_single_alpha(AlarmConfig::default().hysteresis.on);
    eprintln!(
        "[{T124}] new-emitter scene {:.1} h, {} windows, {} IQ samples, run {:.1} s; α {alpha:.4}, \
         μ* {:.5}",
        rec.simulated_span_s / HOUR_S,
        rec.windows,
        rec.samples,
        run.run_s,
        -(-alpha).ln_1p()
    );
    for a in &run.anomalies {
        eprintln!("[{T124}] anomaly: {}", summary(&truth, run.t_first, a));
    }
    assert_eq!(run.lost_samples, 0, "[{T124}] readers lost samples");
    assert!(matches!(run.site, SiteKey::Site(_)), "{:?}", run.site);
    let inject_s = truth.novelty_start_s;
    let on_channel: Vec<_> = run
        .anomalies
        .iter()
        .filter(|a| {
            kind(a) == Some(AlarmKind::NewEmitter) && at_channel(&truth, "novelty", &freq(a))
        })
        .collect();
    assert_eq!(
        on_channel.len(),
        1,
        "[{T124}] one new-emitter alarm on the new emitter's channel; status {}",
        run.alarm_status
    );
    let alarm = on_channel[0];
    let raised_s = scene_s(&truth, run.t_first, raised(alarm));
    let revisits = revisits_between(&truth, inject_s, raised_s);
    let max_revisits = max_alarm_revisits(&truth, inject_s);
    eprintln!(
        "[{T124}] new emitter at h{:.2}; alarm raised h{:.2} after {revisits} revisits (max \
         {max_revisits})",
        inject_s / HOUR_S,
        raised_s / HOUR_S
    );
    assert!(raised_s >= inject_s, "[{T124}] raised before the emitter appeared");
    assert!(
        revisits <= max_revisits,
        "[{T124}] alarmed after {revisits} revisits (max {max_revisits})"
    );
    let row = alarm.listing.alarm.as_ref().unwrap();
    assert_eq!(row.detail.observed, 1.0, "[{T124}] one new emitter");
    assert!(!alarm.explanations.is_empty(), "[{T124}] no explanations");
    assert!(!explained(alarm), "[{T124}] blamed on the device");
    let others = run
        .anomalies
        .iter()
        .filter(|a| a.listing.alarm.is_some() && !explained(a))
        .filter(|a| a.listing.anomaly.id != alarm.listing.anomaly.id)
        .count();
    let (trials, bound) = false_alarm_bound(&run.alarm_status);
    assert!(
        others <= bound,
        "[{T124}] {others} other unexplained alarms (bound {bound} from {trials} inputs)"
    );
}

/// (j) T-136: stopping the pipeline mid-scene and restarting it on the same data dir keeps the
/// pinned site: the restarted run reports the same pinned site, keys every interval row it closes
/// by it and never suppresses an alarm input as unassigned (no `Unassigned` gap).
#[test]
fn m2_restart_keeps_the_pinned_site() {
    let Some(out) = generate(scene_request(RS_SPAN_H, RS_WINDOW_S)) else {
        return;
    };
    let src = TempDir::new("t124rssrc");
    let rec = join_scene_windows(&out, &src.0, "restart").unwrap();
    let at = first_sample_time(&rec.meta).saturating_add_nanos((RS_RESTART_H * HOUR_S * 1e9) as i64);
    let (part1, part2) = split_recording(&rec.meta, at, &src.0);
    let dir = TempDir::new("t124rs");
    let first = run_device(&dir.0, &part1, None, true);
    let SiteKey::Site(id) = first.site else {
        panic!("[{T124}] the pinned site before the restart: {:?}", first.site)
    };
    let second = run_device(&dir.0, &part2, None, false);
    eprintln!(
        "[{T124}] restart: before {} / after {}; rows after the restart {}; suppressions after {}",
        first.current_site, second.current_site, second.interval_rows.len(), second.suppressions
    );
    assert_eq!(second.site, SiteKey::Site(id), "[{T124}] the site after the restart");
    assert_eq!(
        second.current_site["site"], first.current_site["site"],
        "[{T124}] current site after the restart"
    );
    assert_eq!(second.current_site["pinned"], true, "[{T124}] still pinned");
    let after: Vec<_> = second
        .interval_rows
        .iter()
        .filter(|r| r.interval.start.as_unix_nanos() >= second.t_first.as_unix_nanos())
        .collect();
    assert!(!after.is_empty(), "[{T124}] no interval closed after the restart");
    for r in &after {
        assert_eq!(r.site, SiteKey::Site(id), "[{T124}] row {:?}", r.interval);
    }
    let gap = second.suppressions.as_object().is_some_and(|by_kind| {
        by_kind
            .values()
            .any(|s| s.get("unassigned-site").is_some() || s.get("mobile-site").is_some())
    });
    assert!(!gap, "[{T124}] unassigned after the restart: {}", second.suppressions);
    assert!(second.t_end.as_unix_nanos() > second.t_first.as_unix_nanos());
}
