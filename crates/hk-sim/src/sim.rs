//! The discrete-event run loop. Events are step boundaries: each step's live window is resolved
//! against the emitters whose frequency span it covers, transmissions are generated lazily, and
//! the clock jumps to the next boundary.

use hk_core::{ScheduleStep, SourceCapabilities};
use hk_model::Timestamp;

use crate::SimError;
use crate::emitter::EmitterState;
use crate::policy::{Detection, Observation, Policy, PolicyRegistry, SimEnv};
use crate::radio::{RadioMode, RadioModel, Tuning};
use crate::report::{
    ClassCount, ClassReport, ComparisonReport, CurvePoint, DiscoveryReport, InjectedTtfd,
    PoiReport, PolicyReport, RegionReport, SCHEMA, ScenarioSummary, SuspectReport, TimeBudget,
    TtfdReport,
};
use crate::scenario::{EmitterClass, S, Scenario, T0_NS};

/// Run settings.
#[derive(Clone, Debug, PartialEq)]
pub struct RunConfig {
    /// Points on the discovery curve (96).
    pub curve_points: usize,
    /// Revisit-bin width, Hz (1 MHz).
    pub revisit_bin_hz: f64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            curve_points: 96,
            revisit_bin_hz: 1e6,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Bin {
    visits: u64,
    last_start: i64,
    last_end: i64,
    interval_sum: i64,
    interval_max: i64,
    observed: i64,
}

impl Bin {
    fn revisit_mean(&self) -> Option<f64> {
        (self.visits >= 2).then(|| self.interval_sum as f64 / (self.visits - 1) as f64)
    }
    fn visit_mean(&self) -> Option<f64> {
        (self.visits >= 1).then(|| self.observed as f64 / self.visits as f64)
    }
}

struct Bins {
    width: f64,
    bins: Vec<Bin>,
}

impl Bins {
    fn new(width: f64, hi_hz: f64) -> Self {
        let n = (hi_hz / width).ceil().max(1.0) as usize + 1;
        Self {
            width,
            bins: vec![Bin::default(); n],
        }
    }

    fn index(&self, f: f64) -> usize {
        ((f / self.width).floor().max(0.0) as usize).min(self.bins.len() - 1)
    }

    /// Bins whose centre lies in `[lo, hi]`.
    fn range(&self, lo: f64, hi: f64) -> std::ops::Range<usize> {
        let first = ((lo / self.width - 0.5).ceil().max(0.0) as usize).min(self.bins.len());
        let last = ((hi / self.width - 0.5).floor() + 1.0).max(0.0) as usize;
        first..last.min(self.bins.len()).max(first)
    }

    /// A live window `[a, b]` of a step that began at `step_start`.
    fn visit(&mut self, lo: f64, hi: f64, step_start: i64, a: i64, b: i64) {
        let range = self.range(lo, hi);
        for bin in &mut self.bins[range] {
            if bin.visits > 0 && bin.last_end >= step_start {
                bin.last_end = b;
            } else {
                if bin.visits > 0 {
                    let iv = a - bin.last_start;
                    bin.interval_sum += iv;
                    bin.interval_max = bin.interval_max.max(iv);
                }
                bin.visits += 1;
                bin.last_start = a;
                bin.last_end = b;
            }
            bin.observed += b - a;
        }
    }
}

/// Runs `policy` over `scenario` and reports.
pub fn run_policy(
    scenario: &Scenario,
    radio: &RadioModel,
    cfg: &RunConfig,
    policy: &mut dyn Policy,
) -> PolicyReport {
    let specs = &scenario.emitters;
    let mut states: Vec<EmitterState> = specs
        .iter()
        .map(|s| EmitterState::new(scenario, s))
        .collect();
    // Fixed-frequency emitters sorted by centre; hoppers scanned by span.
    let mut by_freq: Vec<(f64, usize)> = Vec::new();
    let mut hoppers: Vec<(f64, f64, usize)> = Vec::new();
    for (i, s) in specs.iter().enumerate() {
        let (lo, hi) = s.span_hz();
        if lo == hi {
            by_freq.push((lo, i));
        } else {
            hoppers.push((lo, hi, i));
        }
    }
    by_freq.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));

    let end = scenario.duration_ns;
    let hi_hz = scenario
        .regions
        .iter()
        .map(|r| r.hi_hz)
        .chain(specs.iter().map(|s| s.span_hz().1))
        .fold(0.0, f64::max);
    let mut bins = Bins::new(cfg.revisit_bin_hz, hi_hz + radio.max_span_hz);
    let mut time = TimeBudget::default();
    let mut suspect = SuspectReport::default();
    let curve_every = (end / cfg.curve_points.max(1) as i64).max(1);
    let mut next_curve = curve_every;
    let mut curve = Vec::with_capacity(cfg.curve_points + 1);
    let mut discovered_real = 0usize;
    let mut detections: Vec<Detection> = Vec::with_capacity(64);
    let mut prev: Option<Tuning> = None;
    let mut now = 0i64;

    while now < end {
        let step: ScheduleStep = policy.next_step(Timestamp::from_unix_nanos(T0_NS + now));
        let duration = step.duration_ns.max(1);
        let mode = radio.mode_of(&step);
        let dead = radio.latency_ns(prev, mode, &step);
        if prev.is_some_and(|p| p.mode != mode) {
            time.mode_switches += 1;
        }
        prev = Some(Tuning {
            mode,
            center_hz: step.center_hz,
            rate_hz: step.rate_hz,
        });
        let step_end = now.saturating_add(duration).min(end);
        let a = now.saturating_add(dead).min(step_end);
        let b = step_end;
        let band = radio.band(&step, mode);
        detections.clear();
        if a < b {
            let first = by_freq.partition_point(|e| e.0 < band.0);
            for &(_, i) in by_freq[first..].iter().take_while(|e| e.0 <= band.1) {
                observe_emitter(
                    scenario,
                    radio,
                    &mut states[i],
                    i,
                    mode,
                    band,
                    step.center_hz,
                    a,
                    b,
                    &mut detections,
                    &mut discovered_real,
                );
            }
            for &(lo, hi, i) in &hoppers {
                if hi >= band.0 && lo <= band.1 {
                    observe_emitter(
                        scenario,
                        radio,
                        &mut states[i],
                        i,
                        mode,
                        band,
                        step.center_hz,
                        a,
                        b,
                        &mut detections,
                        &mut discovered_real,
                    );
                }
            }
            bins.visit(band.0, band.1, now, a, b);
        }
        time.steps += 1;
        time.dead_s += (a - now) as f64 / S as f64;
        match mode {
            RadioMode::Sweep => time.sweep_s += (step_end - now) as f64 / S as f64,
            RadioMode::Stream => {
                time.stream_s += (step_end - now) as f64 / S as f64;
                if step
                    .purpose
                    .poi()
                    .and_then(|k| specs.get(usize::try_from(k).ok()?))
                    .is_some_and(|s| s.suspect)
                {
                    suspect.dwell_s_on_suspect_pois += (step_end - now) as f64 / S as f64;
                }
                if !detections.is_empty() && detections.iter().all(|d| d.suspect) {
                    suspect.dwell_s_suspect_only += (b - a) as f64 / S as f64;
                }
            }
        }
        let obs = Observation {
            mode,
            start: Timestamp::from_unix_nanos(T0_NS + a),
            end: Timestamp::from_unix_nanos(T0_NS + b),
            band_hz: band,
            detections: &detections,
        };
        policy.observe(&step, &obs);
        now = now.saturating_add(duration);
        while next_curve <= now.min(end) {
            curve.push(curve_point(scenario, next_curve, discovered_real));
            next_curve += curve_every;
        }
    }
    if curve.last().is_none_or(|p| p.t_s < end as f64 / S as f64) {
        curve.push(curve_point(scenario, end, discovered_real));
    }
    for (st, spec) in states.iter_mut().zip(specs) {
        st.close(scenario, spec, end);
    }

    PolicyReport {
        name: policy.name().to_owned(),
        params: policy.params(),
        time,
        discovery: DiscoveryReport {
            emitters: specs.iter().filter(|s| !s.suspect).count(),
            discovered: discovered_real,
            suspect_discovered: states
                .iter()
                .zip(specs)
                .filter(|(st, s)| s.suspect && st.first_detect_ns.is_some())
                .count(),
            curve,
        },
        classes: class_reports(scenario, &states),
        ttfd: ttfd_report(scenario, &states),
        regions: region_reports(scenario, &states, &bins),
        suspect,
    }
}

#[allow(clippy::too_many_arguments)]
fn observe_emitter(
    scenario: &Scenario,
    radio: &RadioModel,
    st: &mut EmitterState,
    i: usize,
    mode: RadioMode,
    band: (f64, f64),
    center_hz: f64,
    a: i64,
    b: i64,
    detections: &mut Vec<Detection>,
    discovered_real: &mut usize,
) {
    let spec = &scenario.emitters[i];
    while st.cur.end <= a {
        st.finish(scenario, spec);
    }
    let mut det: Option<Detection> = None;
    while st.cur.start < b {
        let tx = st.cur;
        let overlap = tx.end.min(b) - tx.start.max(a);
        if overlap >= radio.min_overlap_ns
            && radio.visible(mode, band, center_hz, tx.freq_hz, tx.snr_db)
        {
            if st.first_detect_ns.is_none() && !spec.suspect {
                *discovered_real += 1;
            }
            st.capture(tx.start.max(a));
            let d = det.get_or_insert(Detection {
                key: spec.id,
                center_hz: tx.freq_hz,
                bandwidth_hz: spec.bandwidth_hz,
                snr_db: tx.snr_db,
                suspect: spec.suspect,
                transmissions: 0,
                continuous: false,
            });
            d.center_hz = tx.freq_hz;
            d.snr_db = d.snr_db.max(tx.snr_db);
            d.transmissions += 1;
            d.continuous = tx.start <= a && tx.end >= b;
        }
        if tx.end <= b {
            st.finish(scenario, spec);
        } else {
            break;
        }
    }
    if let Some(d) = det {
        detections.push(d);
    }
}

fn curve_point(scenario: &Scenario, t: i64, discovered: usize) -> CurvePoint {
    CurvePoint {
        t_s: t as f64 / S as f64,
        present: scenario
            .emitters
            .iter()
            .filter(|s| !s.suspect && s.appears_ns <= t)
            .count(),
        discovered,
    }
}

fn class_reports(scenario: &Scenario, states: &[EmitterState]) -> Vec<ClassReport> {
    let hours = scenario.duration_ns as f64 / (3600.0 * S as f64);
    EmitterClass::ALL
        .iter()
        .map(|&class| {
            let mut r = ClassReport {
                class,
                emitters: 0,
                discovered: 0,
                transmissions: 0,
                captured: 0,
                captured_per_hour: 0.0,
                capture_fraction: None,
            };
            for (s, st) in scenario.emitters.iter().zip(states) {
                if s.class == class {
                    r.emitters += 1;
                    r.discovered += usize::from(st.first_detect_ns.is_some());
                    r.transmissions += st.total;
                    r.captured += st.captured;
                }
            }
            r.captured_per_hour = if hours > 0.0 {
                r.captured as f64 / hours
            } else {
                0.0
            };
            r.capture_fraction =
                (r.transmissions > 0).then(|| r.captured as f64 / r.transmissions as f64);
            r
        })
        .collect()
}

fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    Some(sorted[idx])
}

fn ttfd_report(scenario: &Scenario, states: &[EmitterState]) -> TtfdReport {
    let mut all = Vec::new();
    let mut undiscovered = 0;
    let mut injected = Vec::new();
    for (s, st) in scenario.emitters.iter().zip(states) {
        let ttfd = st
            .first_detect_ns
            .map(|t| (t - s.appears_ns) as f64 / S as f64);
        if s.injected {
            injected.push(InjectedTtfd {
                emitter: s.id,
                class: s.class,
                center_hz: s.center_hz,
                appears_s: s.appears_ns as f64 / S as f64,
                ttfd_s: ttfd,
            });
        }
        if !s.suspect {
            match ttfd {
                Some(v) => all.push(v),
                None => undiscovered += 1,
            }
        }
    }
    all.sort_by(f64::total_cmp);
    TtfdReport {
        injected,
        median_s: percentile(&all, 0.5),
        p90_s: percentile(&all, 0.9),
        undiscovered,
    }
}

fn region_reports(scenario: &Scenario, states: &[EmitterState], bins: &Bins) -> Vec<RegionReport> {
    let s_f = S as f64;
    scenario
        .regions
        .iter()
        .enumerate()
        .map(|(ri, r)| {
            let range = bins.range(r.lo_hz, r.hi_hz);
            let region_bins = &bins.bins[range];
            let revisits: Vec<f64> = region_bins.iter().filter_map(Bin::revisit_mean).collect();
            let visits: u64 = region_bins.iter().map(|b| b.visits).sum();
            let observed: i64 = region_bins.iter().map(|b| b.observed).sum();
            let mut poi = PoiReport {
                emitters: 0,
                transmissions: 0,
                captured: 0,
                measured: None,
                predicted: None,
                std_err: None,
                z: None,
            };
            let (mut pred_sum, mut var_sum) = (0.0, 0.0);
            for (s, st) in scenario.emitters.iter().zip(states) {
                if s.suspect
                    || !matches!(s.class, EmitterClass::Burst | EmitterClass::Beacon)
                    || scenario.region_of(s.center_hz) != Some(ri)
                {
                    continue;
                }
                let tau = s.tau_ns().unwrap_or(0) as f64;
                let bin = &bins.bins[bins.index(s.center_hz)];
                // A bin visited once has no measured revisit: take the run as its revisit.
                let p = match (bin.revisit_mean(), bin.visit_mean()) {
                    (Some(tr), Some(td)) => ((tau + td) / tr).min(1.0),
                    (None, Some(td)) => ((tau + td) / scenario.duration_ns.max(1) as f64).min(1.0),
                    _ => 0.0,
                };
                poi.emitters += 1;
                poi.transmissions += st.total;
                poi.captured += st.captured;
                pred_sum += st.total as f64 * p;
                var_sum += st.total as f64 * p * (1.0 - p);
            }
            if poi.transmissions > 0 {
                let n = poi.transmissions as f64;
                let measured = poi.captured as f64 / n;
                let predicted = pred_sum / n;
                let se = var_sum.sqrt() / n;
                poi.measured = Some(measured);
                poi.predicted = Some(predicted);
                poi.std_err = Some(se);
                poi.z = (se > 0.0).then(|| (measured - predicted) / se);
            }
            RegionReport {
                region: ri,
                freq_hz: [r.lo_hz, r.hi_hz],
                bins: region_bins.len(),
                bins_unvisited: region_bins.iter().filter(|b| b.visits == 0).count(),
                revisit_mean_s: (!revisits.is_empty())
                    .then(|| revisits.iter().sum::<f64>() / revisits.len() as f64 / s_f),
                revisit_max_s: region_bins
                    .iter()
                    .filter(|b| b.visits >= 2)
                    .map(|b| b.interval_max as f64 / s_f)
                    .reduce(f64::max),
                visit_mean_s: (visits > 0).then(|| observed as f64 / visits as f64 / s_f),
                observed_fraction: if region_bins.is_empty() || scenario.duration_ns == 0 {
                    0.0
                } else {
                    observed as f64 / (region_bins.len() as f64 * scenario.duration_ns as f64)
                },
                poi,
            }
        })
        .collect()
}

/// Scenario summary for reports.
pub fn summarize(scenario: &Scenario) -> ScenarioSummary {
    ScenarioSummary {
        name: scenario.name.clone(),
        seed: scenario.seed,
        duration_s: scenario.duration_ns as f64 / S as f64,
        regions_hz: scenario
            .regions
            .iter()
            .map(|r| [r.lo_hz, r.hi_hz])
            .collect(),
        emitters: EmitterClass::ALL
            .iter()
            .map(|&class| ClassCount {
                class,
                count: scenario
                    .emitters
                    .iter()
                    .filter(|e| e.class == class)
                    .count(),
            })
            .collect(),
        injected: scenario.emitters.iter().filter(|e| e.injected).count(),
    }
}

/// Runs the named policies (all registered ones when `names` is empty) on `scenario`, each from
/// a fresh policy and the same transmissions.
pub fn compare(
    scenario: &Scenario,
    radio: &RadioModel,
    cfg: &RunConfig,
    registry: &PolicyRegistry,
    names: &[&str],
) -> Result<ComparisonReport, SimError> {
    let caps = SourceCapabilities::hackrf_one();
    let env = SimEnv {
        scenario,
        radio,
        caps: &caps,
    };
    let names: Vec<&str> = if names.is_empty() {
        registry.names()
    } else {
        names.to_vec()
    };
    let mut policies = Vec::with_capacity(names.len());
    for name in names {
        let mut p = registry.build(name, &env)?;
        policies.push(run_policy(scenario, radio, cfg, p.as_mut()));
    }
    Ok(ComparisonReport {
        schema: SCHEMA,
        scenario: summarize(scenario),
        radio: radio.clone(),
        policies,
    })
}
