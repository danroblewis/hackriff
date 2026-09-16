//! Device conformance suite (T-048/T-049): the checks every device behind the generic contract
//! ([`super`]) must pass — the mock SDR in CI, the HackRF One as a manual HIL case, SoapySDR later.
//!
//! [`run`] opens a source through its [`SourceDriver`] and exercises the contract in one bounded
//! session (every wait has a block budget and a wall-clock deadline). It never transmits. Each
//! check lands in the [`ConformanceReport`]; [`ConformanceReport::assert_passed`] fails a test with
//! every failure listed.
//!
//! | Check | What it asserts |
//! |---|---|
//! | `open` | the driver is available, opens the request, and the source, control and driver report the same capabilities, which support the requested centre and rate |
//! | `start` | `start` succeeds before the first read |
//! | `first-block` | the first block carries `STREAM_START` and no later block does |
//! | `request-applied` | the first block's provenance has the requested centre, rate and quantised named gains |
//! | `device-info` | `device_info` is present, non-empty, and its `device_id` is the provenance's |
//! | `timestamps` | times never go backwards; between blocks without a rate change they follow the sample counter at the tuned rate; the method matches `hardware_timestamps` |
//! | `counter` | the sample counter is monotonic; every jump is flagged `GAP` with the exact `dropped_before` |
//! | `pausable` | matches the expectation; a pausable source loses nothing across a pause |
//! | `tune-out-of-range` | below, above and NaN centres are `OutOfRange` and no retune follows |
//! | `invalid-controls` | an out-of-range rate, unknown gain stage, out-of-range and NaN gains are refused |
//! | `tune-in-range` | the retune reaches a block flagged `RETUNE` with the new centre |
//! | `settle-discard` | that block is a `GAP` whose `dropped_before` covers the counted settle discard |
//! | `sample-rate` | a new rate reaches a block flagged `RATE_CHANGE` (when the spec names one) |
//! | `named-gains` | each stage is set off-step and read back quantised through provenance (LNA/VGA/amp) |
//! | `baseband-filter` | a supported bandwidth reaches provenance; unsupported is refused (or the control is refused without the capability) |
//! | `bias-tee` | works with the capability, else `Unsupported` |
//! | `bias-tee-provenance` | the state reaches provenance: `off` after the check above with the capability, never `on` without it |
//! | `sweep` | works with a sweep capability, else `start_sweep`/`stop_sweep` are `Unsupported` |
//! | `overrun` | an injected overrun (when the spec can inject) is a `GAP` counted in `SourceStats` |
//! | `stats` | every gap is accounted as dropped or discarded samples; block and sample counts cover what was read |
//! | `stop` | after `stop`, reads return `Ok(None)` |

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hk_model::{BiasTee, TimestampMethod};
use num_complex::{Complex, Complex32};

use super::{
    BasebandFilters, OpenRequest, Source, SourceControl, SourceDriver, SourceError, SourceStats,
    SweepPlan,
};
use crate::block::{BlockHeader, Discontinuity};

/// Injects an overrun into the opened device; returns the samples it will drop.
pub type OverrunInjector = Box<dyn Fn() -> u64 + Send + Sync>;

/// What to exercise and expect.
pub struct ConformanceSpec {
    /// The open request.
    pub request: OpenRequest,
    /// An in-range centre to retune to.
    pub retune_hz: f64,
    /// A different supported rate to switch to, if the device and test allow it.
    pub alt_rate_hz: Option<f64>,
    /// Expected `pausable()`, if known.
    pub expect_pausable: Option<bool>,
    /// Control changes discard settling samples (reported as a gap).
    pub expect_settle_discard: bool,
    /// Gaps equal dropped + discarded samples exactly (a device with no in-flight losses).
    pub exact_loss_accounting: bool,
    /// Overrun injection, when the device supports it.
    pub inject_overrun: Option<OverrunInjector>,
    /// Blocks read at the start.
    pub warmup_blocks: usize,
    /// Blocks a posted change may take to reach the stream.
    pub max_blocks_per_wait: usize,
    /// Pause used by the `pausable` check.
    pub pause: Duration,
    /// Tolerance of block times against the sample counter, ns.
    pub time_tolerance_ns: i64,
    /// Wall-clock budget for the whole session.
    pub deadline: Duration,
}

impl ConformanceSpec {
    /// Defaults around `request` and `retune_hz`.
    pub fn new(request: OpenRequest, retune_hz: f64) -> Self {
        Self {
            request,
            retune_hz,
            alt_rate_hz: None,
            expect_pausable: None,
            expect_settle_discard: true,
            exact_loss_accounting: false,
            inject_overrun: None,
            warmup_blocks: 8,
            max_blocks_per_wait: 64,
            pause: Duration::from_millis(200),
            time_tolerance_ns: 1_000,
            deadline: Duration::from_secs(120),
        }
    }
}

/// One check's outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckResult {
    /// Check name (see the module table).
    pub name: &'static str,
    /// Passed.
    pub passed: bool,
    /// Why it failed, or a note.
    pub detail: String,
}

/// Every check's outcome.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConformanceReport {
    /// Driver name.
    pub driver: String,
    /// Results in order.
    pub checks: Vec<CheckResult>,
}

impl ConformanceReport {
    fn record(&mut self, name: &'static str, result: Result<String, String>) {
        let (passed, detail) = match result {
            Ok(d) => (true, d),
            Err(d) => (false, d),
        };
        // A check reported twice keeps its first failure.
        match self.checks.iter_mut().find(|c| c.name == name) {
            Some(c) if c.passed && !passed => {
                c.passed = false;
                c.detail = detail;
            }
            Some(_) => {}
            None => self.checks.push(CheckResult {
                name,
                passed,
                detail,
            }),
        }
    }

    /// The failed checks.
    pub fn failures(&self) -> Vec<&CheckResult> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }

    /// The named check passed.
    pub fn passed(&self, name: &str) -> bool {
        self.checks.iter().any(|c| c.name == name && c.passed)
    }

    /// Panics listing every failure.
    pub fn assert_passed(&self) {
        let failures = self.failures();
        assert!(failures.is_empty(), "{self}");
    }
}

impl fmt::Display for ConformanceReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "device conformance: {}", self.driver)?;
        for c in &self.checks {
            let mark = if c.passed { "ok  " } else { "FAIL" };
            writeln!(f, "  {mark} {:<18} {}", c.name, c.detail)?;
        }
        Ok(())
    }
}

/// The checks every device must pass.
pub const CHECKS: &[&str] = &[
    "open",
    "start",
    "first-block",
    "request-applied",
    "device-info",
    "timestamps",
    "counter",
    "pausable",
    "tune-out-of-range",
    "invalid-controls",
    "tune-in-range",
    "settle-discard",
    "sample-rate",
    "named-gains",
    "baseband-filter",
    "bias-tee",
    "bias-tee-provenance",
    "sweep",
    "overrun",
    "stats",
    "stop",
];

/// Reads blocks and checks the stream invariants on every one.
struct Reader {
    source: Box<dyn Source>,
    ci8: Vec<Complex<i8>>,
    f32s: Vec<Complex32>,
    native: bool,
    deadline: Instant,
    tolerance_ns: i64,
    last: Option<(BlockHeader, usize)>,
    blocks: u64,
    samples: u64,
    gaps: u64,
    counter_errors: Vec<String>,
    time_errors: Vec<String>,
    start_errors: Vec<String>,
}

impl Reader {
    fn read(&mut self) -> Result<Option<(BlockHeader, usize)>, String> {
        if Instant::now() > self.deadline {
            return Err("deadline exceeded".into());
        }
        let got = if self.native {
            match self.source.read_block_ci8(&mut self.ci8) {
                Err(SourceError::Unsupported { .. }) => {
                    self.native = false;
                    return self.read();
                }
                r => r.map(|h| h.map(|h| (h, self.ci8.len()))),
            }
        } else {
            self.source
                .read_block(&mut self.f32s)
                .map(|h| h.map(|h| (h, self.f32s.len())))
        };
        let Some((h, n)) = got.map_err(|e| format!("read failed: {e}"))? else {
            return Ok(None);
        };
        self.check(&h, n);
        self.blocks += 1;
        self.samples += n as u64;
        self.gaps += h.dropped_before;
        self.last = Some((h.clone(), n));
        Ok(Some((h, n)))
    }

    fn check(&mut self, h: &BlockHeader, n: usize) {
        let k = self.blocks;
        let start = h.discontinuity.contains(Discontinuity::STREAM_START);
        if (k == 0) != start {
            self.start_errors.push(format!(
                "block {k}: STREAM_START {start} (first block: {})",
                k == 0
            ));
        }
        if h.dropped_before > 0 && !h.discontinuity.contains(Discontinuity::GAP) {
            self.counter_errors.push(format!(
                "block {k}: dropped_before {} without GAP",
                h.dropped_before
            ));
        }
        if n == 0 {
            self.counter_errors.push(format!("block {k}: empty block"));
        }
        let Some((prev, prev_n)) = &self.last else {
            return;
        };
        let expected = prev.first_sample() + *prev_n as u64;
        if h.first_sample() != expected + h.dropped_before {
            self.counter_errors.push(format!(
                "block {k}: first sample {} but previous end {expected} + dropped_before {}",
                h.first_sample(),
                h.dropped_before
            ));
        }
        let (t0, t1) = (
            prev.time.host_time.as_unix_nanos(),
            h.time.host_time.as_unix_nanos(),
        );
        if t1 < t0 {
            self.time_errors
                .push(format!("block {k}: time went back {t0} -> {t1}"));
        }
        if !h.discontinuity.contains(Discontinuity::RATE_CHANGE) {
            let fs = h.provenance.tune.sample_rate_hz;
            let dt = (h.first_sample() - prev.first_sample()) as f64 * 1e9 / fs;
            let err = (t1 - t0) as f64 - dt;
            if err.abs() > self.tolerance_ns as f64 {
                self.time_errors.push(format!(
                    "block {k}: {} ns after the previous block, the counter says {dt:.0} ns",
                    t1 - t0
                ));
            }
        }
    }

    /// Reads until `pred` holds for a block, within `max` blocks.
    fn wait_for(
        &mut self,
        max: usize,
        pred: impl Fn(&BlockHeader) -> bool,
    ) -> Result<BlockHeader, String> {
        for _ in 0..max {
            match self.read()? {
                Some((h, _)) if pred(&h) => return Ok(h),
                Some(_) => {}
                None => return Err("the stream ended".into()),
            }
        }
        Err(format!("not seen within {max} blocks"))
    }
}

fn is_out_of_range<T>(r: &Result<T, SourceError>) -> bool {
    matches!(r, Err(SourceError::OutOfRange { .. }))
}

fn is_unsupported<T>(r: &Result<T, SourceError>) -> bool {
    matches!(r, Err(SourceError::Unsupported { .. }))
}

fn stats(control: &Arc<dyn SourceControl>) -> Result<SourceStats, String> {
    control
        .stats()
        .ok_or_else(|| "the device keeps no SourceStats".to_string())
}

/// Reads a stage back from provenance (the model stores LNA/VGA/amp).
fn read_back(h: &BlockHeader, stage: &str, max_db: f64) -> Option<f64> {
    let t = &h.provenance.tune;
    match stage {
        "lna" => Some(t.lna_db),
        "vga" => Some(t.vga_db),
        "amp" => Some(if t.amp_on { max_db } else { 0.0 }),
        _ => None,
    }
}

/// Runs the suite against `driver`. Receive only.
pub fn run(driver: &dyn SourceDriver, spec: &ConformanceSpec) -> ConformanceReport {
    let mut report = ConformanceReport {
        driver: driver.name().into(),
        checks: Vec::new(),
    };
    if let Err(e) = session(driver, spec, &mut report) {
        report.record("session", Err(e));
    }
    for name in CHECKS {
        if !report.checks.iter().any(|c| c.name == *name) {
            report.record(name, Err("not reached".into()));
        }
    }
    report
}

fn session(
    driver: &dyn SourceDriver,
    spec: &ConformanceSpec,
    report: &mut ConformanceReport,
) -> Result<(), String> {
    let wait = spec.max_blocks_per_wait;
    // open
    if !driver.available() {
        report.record(
            "open",
            Err(format!("driver {} is not available", driver.name())),
        );
        return Ok(());
    }
    let source = driver
        .open(&spec.request)
        .map_err(|e| format!("open failed: {e}"))?;
    let caps = source.capabilities().clone();
    let control = source.control();
    let open = if driver.capabilities() != caps {
        Err("driver and source capabilities differ".to_string())
    } else if *control.capabilities() != caps {
        Err("control and source capabilities differ".to_string())
    } else if !caps.supports_frequency(spec.request.center_hz)
        || !caps.sample_rates.supports(spec.request.sample_rate_hz)
    {
        Err("the capabilities do not support the request".to_string())
    } else {
        Ok(format!("{} ({:?})", caps.driver, caps.kind))
    };
    report.record("open", open);
    report.record(
        "start",
        control
            .start()
            .map(|_| String::new())
            .map_err(|e| e.to_string()),
    );
    let pausable = source.pausable();
    let mut r = Reader {
        source,
        ci8: Vec::new(),
        f32s: Vec::new(),
        native: true,
        deadline: Instant::now() + spec.deadline,
        tolerance_ns: spec.time_tolerance_ns,
        last: None,
        blocks: 0,
        samples: 0,
        gaps: 0,
        counter_errors: Vec::new(),
        time_errors: Vec::new(),
        start_errors: Vec::new(),
    };

    // First blocks.
    let (first, _) = r.read()?.ok_or("the stream ended before its first block")?;
    let p = first.provenance.get();
    let mut applied = Vec::new();
    if (p.tune.center_hz - spec.request.center_hz).abs() > 1.0 {
        applied.push(format!("centre {} Hz", p.tune.center_hz));
    }
    if p.tune.sample_rate_hz != spec.request.sample_rate_hz {
        applied.push(format!("rate {} Hz", p.tune.sample_rate_hz));
    }
    for g in &spec.request.gains {
        let stage = caps.gain_stage(&g.stage);
        let want = stage.and_then(|s| s.quantise(g.db));
        let got = stage.and_then(|s| read_back(&first, &g.stage, s.max_db));
        if let (Some(w), Some(v)) = (want, got) {
            if (w - v).abs() > 1e-9 {
                applied.push(format!("{} {v} dB, want {w}", g.stage));
            }
        }
    }
    report.record(
        "request-applied",
        if applied.is_empty() {
            Ok(String::new())
        } else {
            Err(applied.join("; "))
        },
    );
    report.record(
        "device-info",
        match control.device_info() {
            None => Err("no DeviceInfo".into()),
            Some(i) if i.driver.is_empty() || i.device_id.is_empty() || i.hw.is_empty() => {
                Err(format!("empty fields: {i:?}"))
            }
            Some(i) if i.device_id != p.device_id => Err(format!(
                "device_id {} but provenance says {}",
                i.device_id, p.device_id
            )),
            Some(i) => Ok(i.device_id),
        },
    );
    let method = p.timestamp_method;
    let method_ok =
        caps.hardware_timestamps || !matches!(method, TimestampMethod::ExternalReference);
    for _ in 0..spec.warmup_blocks {
        r.read()?.ok_or("the stream ended during warm-up")?;
    }

    // pausable
    let pause = match spec.expect_pausable {
        Some(want) if want != pausable => Err(format!("pausable {pausable}, expected {want}")),
        _ if pausable => {
            let before = r.last.as_ref().map(|(h, n)| h.first_sample() + *n as u64);
            std::thread::sleep(spec.pause);
            match (before, r.read()?) {
                (Some(end), Some((h, _))) if h.first_sample() == end && h.dropped_before == 0 => {
                    Ok("pausable: nothing lost across a pause".into())
                }
                (_, Some((h, _))) => Err(format!(
                    "pausable but lost {} samples across a pause",
                    h.dropped_before
                )),
                (_, None) => Err("the stream ended".into()),
            }
        }
        _ => Ok("not pausable (streams in real time)".into()),
    };
    report.record("pausable", pause);

    // Out-of-range tuning and invalid controls.
    let lo = caps
        .frequency_ranges
        .iter()
        .map(|f| f.min_hz)
        .fold(f64::INFINITY, f64::min);
    let hi = caps
        .frequency_ranges
        .iter()
        .map(|f| f.max_hz)
        .fold(f64::NEG_INFINITY, f64::max);
    let bad_tunes = [lo * 0.5 - 1.0, hi * 2.0 + 1.0, f64::NAN, f64::INFINITY];
    let mut oor = Vec::new();
    for hz in bad_tunes {
        if !is_out_of_range(&control.tune(hz)) {
            oor.push(format!("tune({hz}) not OutOfRange"));
        }
    }
    let centre_before = r.last.as_ref().map(|(h, _)| h.provenance.tune.center_hz);
    for _ in 0..3 {
        if let Some((h, _)) = r.read()? {
            if h.discontinuity.contains(Discontinuity::RETUNE)
                || Some(h.provenance.tune.center_hz) != centre_before
            {
                oor.push("a refused tune changed the stream".into());
            }
        }
    }
    report.record(
        "tune-out-of-range",
        if oor.is_empty() {
            Ok(String::new())
        } else {
            Err(oor.join("; "))
        },
    );
    let mut invalid = Vec::new();
    if control.set_sample_rate(-1.0).is_ok() || control.set_sample_rate(f64::NAN).is_ok() {
        invalid.push("an invalid rate was accepted".to_string());
    }
    if control.set_gain("no-such-stage", 0.0).is_ok() {
        invalid.push("an unknown gain stage was accepted".into());
    }
    for s in &caps.gain_stages {
        let over = s.max_db + s.step_db.max(1.0) + 1.0;
        if !is_out_of_range(&control.set_gain(&s.name, over)) {
            invalid.push(format!("{} {over} dB not OutOfRange", s.name));
        }
        if control.set_gain(&s.name, f64::NAN).is_ok() {
            invalid.push(format!("{} NaN accepted", s.name));
        }
    }
    report.record(
        "invalid-controls",
        if invalid.is_empty() {
            Ok(String::new())
        } else {
            Err(invalid.join("; "))
        },
    );

    // Retune in range, with the settle discard.
    let discarded_before = stats(&control).map(|s| s.discarded_samples).unwrap_or(0);
    match control.tune(spec.retune_hz) {
        Err(e) => report.record("tune-in-range", Err(format!("tune refused: {e}"))),
        Ok(()) => match r.wait_for(wait, |h| h.discontinuity.contains(Discontinuity::RETUNE)) {
            Err(e) => report.record("tune-in-range", Err(format!("no RETUNE block: {e}"))),
            Ok(h) => {
                let c = h.provenance.tune.center_hz;
                report.record(
                    "tune-in-range",
                    if (c - spec.retune_hz).abs() <= 1.0 {
                        Ok(format!("{c} Hz"))
                    } else {
                        Err(format!("RETUNE block at {c} Hz, want {}", spec.retune_hz))
                    },
                );
                let delta = stats(&control)
                    .map(|s| s.discarded_samples - discarded_before)
                    .unwrap_or(0);
                let settle = if !spec.expect_settle_discard {
                    Ok("no settle discard expected".into())
                } else if !h.discontinuity.contains(Discontinuity::GAP) || h.dropped_before == 0 {
                    Err("the retune block is not a GAP".into())
                } else if delta == 0 || delta > h.dropped_before {
                    Err(format!(
                        "discarded_samples grew {delta}, the gap is {}",
                        h.dropped_before
                    ))
                } else {
                    Ok(format!(
                        "{delta} samples discarded, gap {}",
                        h.dropped_before
                    ))
                };
                report.record("settle-discard", settle);
            }
        },
    }

    // Sample rate.
    let rate = match spec.alt_rate_hz {
        None => Ok("not exercised (no alternative rate in the spec)".into()),
        Some(rate) => match control.set_sample_rate(rate) {
            Err(e) => Err(format!("set_sample_rate({rate}) refused: {e}")),
            Ok(()) => r
                .wait_for(wait, |h| {
                    h.discontinuity.contains(Discontinuity::RATE_CHANGE)
                })
                .and_then(|h| {
                    if h.provenance.tune.sample_rate_hz == rate {
                        Ok(format!("{rate} Hz"))
                    } else {
                        Err(format!("rate {} Hz", h.provenance.tune.sample_rate_hz))
                    }
                }),
        },
    };
    report.record("sample-rate", rate);

    // Named gains, set off-step and read back quantised.
    let mut gains = Vec::new();
    let mut notes = Vec::new();
    for s in &caps.gain_stages {
        let target = if s.step_db > 0.0 && s.step_db < s.max_db - s.min_db {
            (s.min_db + 1.5 * s.step_db).min(s.max_db)
        } else {
            s.max_db
        };
        let Some(want) = s.quantise(target) else {
            gains.push(format!("{} cannot quantise {target}", s.name));
            continue;
        };
        if let Err(e) = control.set_gain(&s.name, target) {
            gains.push(format!("{} {target} dB refused: {e}", s.name));
            continue;
        }
        match r.wait_for(wait, |h| {
            read_back(h, &s.name, s.max_db).is_none_or(|v| (v - want).abs() < 1e-9)
        }) {
            Ok(h) if read_back(&h, &s.name, s.max_db).is_none() => {
                notes.push(format!("{} not representable in provenance", s.name));
            }
            Ok(_) => notes.push(format!("{} {target}->{want}", s.name)),
            Err(e) => gains.push(format!("{} never read back {want} dB: {e}", s.name)),
        }
    }
    report.record(
        "named-gains",
        if gains.is_empty() {
            Ok(notes.join(", "))
        } else {
            Err(gains.join("; "))
        },
    );

    // Baseband filter.
    let filter = match &caps.baseband_filter {
        None => {
            if control.set_baseband_filter(1e6).is_ok() {
                Err("accepted without the capability".into())
            } else {
                Ok("no baseband filter capability".into())
            }
        }
        Some(f) => {
            let current = r
                .last
                .as_ref()
                .map_or(0.0, |(h, _)| h.provenance.tune.bandwidth_hz);
            let pick = match f {
                BasebandFilters::Discrete(v) => v.iter().copied().find(|w| *w != current),
                BasebandFilters::Continuous { min_hz, .. } => Some(*min_hz),
            };
            if control.set_baseband_filter(0.5).is_ok() {
                Err("an unsupported bandwidth was accepted".into())
            } else if let Some(w) = pick {
                match control.set_baseband_filter(w) {
                    Err(e) => Err(format!("{w} Hz refused: {e}")),
                    Ok(()) => r
                        .wait_for(wait, |h| h.provenance.tune.bandwidth_hz == w)
                        .map(|_| format!("{w} Hz")),
                }
            } else {
                Ok("only one bandwidth".into())
            }
        }
    };
    report.record("baseband-filter", filter);

    // Bias tee.
    let bias = if caps.bias_tee {
        match (control.set_bias_tee(true), control.set_bias_tee(false)) {
            (Ok(()), Ok(())) => Ok("on/off accepted".into()),
            (a, b) => Err(format!("{a:?} {b:?}")),
        }
    } else if is_unsupported(&control.set_bias_tee(true)) {
        Ok("unsupported, reported".into())
    } else {
        Err("no bias tee capability but not Unsupported".into())
    };
    report.record("bias-tee", bias);

    // T-325: the state reaches provenance. The pair above left the bias tee off, so a device with
    // one must now report `off` — proving it reports the field rather than leaving it unknown —
    // and a device without one must never claim `on`. The `on` case is asserted in the mock's own
    // tests, so this suite (which also runs as a HIL case against a real HackRF) never holds DC on
    // an antenna port for longer than the explicit on/off pair above.
    let bias_prov = if caps.bias_tee {
        r.wait_for(wait, |h| h.provenance.bias_tee == BiasTee::Off)
            .map(|_| "off reaches provenance".into())
    } else {
        match r.read() {
            Err(e) => Err(e),
            Ok(None) => Err("the stream ended before the bias-tee state was read".into()),
            Ok(Some((h, _))) if h.provenance.bias_tee == BiasTee::On => {
                Err("no bias tee capability but provenance says on".into())
            }
            Ok(Some((h, _))) => Ok(h.provenance.bias_tee.as_str().into()),
        }
    };
    report.record("bias-tee-provenance", bias_prov);

    // Sweep.
    let sweep = match control.sweep_capability() {
        None => {
            let plan = SweepPlan {
                lo_hz: spec.request.center_hz,
                hi_hz: spec.request.center_hz,
                step_hz: spec.request.sample_rate_hz,
                sample_rate_hz: spec.request.sample_rate_hz,
                samples_per_hop: 1024,
            };
            if is_unsupported(&control.start_sweep(&plan)) && is_unsupported(&control.stop_sweep())
            {
                Ok("unsupported, reported".into())
            } else {
                Err("no sweep capability but start/stop_sweep not Unsupported".into())
            }
        }
        Some(cap) => {
            let rate = match &cap.sample_rates {
                super::SampleRates::Continuous { min_hz, .. } => *min_hz,
                super::SampleRates::Discrete(v) => v.first().copied().unwrap_or(0.0),
            };
            let lo = cap
                .frequency_range
                .min_hz
                .max(spec.request.center_hz - 5.0 * rate);
            let plan = SweepPlan {
                lo_hz: lo,
                hi_hz: lo + 2.0 * rate,
                step_hz: rate,
                sample_rate_hz: rate,
                samples_per_hop: 1024,
            };
            match (control.start_sweep(&plan), control.stop_sweep()) {
                (Ok(()), Ok(())) => Ok("start/stop accepted".into()),
                (a, b) => Err(format!("{a:?} {b:?}")),
            }
        }
    };
    report.record("sweep", sweep);

    // Drain a few blocks so every pending change has settled before the loss checks.
    for _ in 0..4 {
        r.read()?;
    }

    // Overrun.
    let overrun = match &spec.inject_overrun {
        None => Ok("not injectable on this device".into()),
        Some(inject) => {
            let before = stats(&control)?;
            let n = inject();
            r.wait_for(wait, |h| h.dropped_before >= n)
                .map_err(|e| format!("no gap of {n}: {e}"))
                .and_then(|h| {
                    let after = stats(&control)?;
                    if after.overruns > before.overruns
                        && after.dropped_samples >= before.dropped_samples + n
                    {
                        Ok(format!("gap {} for {n} injected", h.dropped_before))
                    } else {
                        Err(format!("stats did not count it: {before:?} -> {after:?}"))
                    }
                })
        }
    };
    report.record("overrun", overrun);

    // Stats.
    let st = stats(&control).and_then(|s| {
        let lost = s.dropped_samples + s.discarded_samples;
        if spec.exact_loss_accounting && r.gaps != lost {
            Err(format!(
                "gaps {} but dropped + discarded {lost} ({s:?})",
                r.gaps
            ))
        } else if r.gaps > lost {
            Err(format!(
                "gaps {} exceed dropped + discarded {lost} ({s:?})",
                r.gaps
            ))
        } else if s.blocks < r.blocks || s.samples < r.samples {
            Err(format!(
                "read {} blocks / {} samples, stats {s:?}",
                r.blocks, r.samples
            ))
        } else {
            Ok(format!("{s:?}"))
        }
    });
    report.record("stats", st);

    // Stream invariants gathered on every read.
    let join = |v: &[String]| v.iter().take(5).cloned().collect::<Vec<_>>().join("; ");
    report.record(
        "first-block",
        if r.start_errors.is_empty() {
            Ok(String::new())
        } else {
            Err(join(&r.start_errors))
        },
    );
    report.record(
        "counter",
        if r.counter_errors.is_empty() {
            Ok(format!("{} blocks, {} gap samples", r.blocks, r.gaps))
        } else {
            Err(join(&r.counter_errors))
        },
    );
    report.record(
        "timestamps",
        if !method_ok {
            Err(format!("method {method:?} without hardware timestamps"))
        } else if r.time_errors.is_empty() {
            Ok(format!("{method:?}"))
        } else {
            Err(join(&r.time_errors))
        },
    );

    // Stop.
    let stop = control.stop().map_err(|e| e.to_string()).and_then(|_| {
        for _ in 0..3 {
            if r.read()?.is_none() {
                return Ok(String::new());
            }
        }
        Err("reads continue after stop".into())
    });
    report.record("stop", stop);
    Ok(())
}
