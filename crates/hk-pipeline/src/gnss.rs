//! The caller for C36: a scheduled L1 dwell, acquisition on that dwell, evidence into C30
//! (T-322; ADR-0018; SIGNAL-030, AWARE-002, AWARE-003).
//!
//! # What T-274 left, and why it left it
//!
//! `hk-gnss` built GPS L1 acquisition — Gold codes, FFT parallel code-phase search, observables,
//! S4, blind jamming assessment — and **deliberately did not wire it in**. ADR-0018 says why in
//! one line: *wiring is where leakage risk lives.* GNSS acquisition is the project's only
//! documented exception to blind-first, because L1 sits 20–30 dB under the noise floor and has to
//! be despread against published codes. An exception is safe only while it cannot spread, and the
//! thing that makes "cannot" structural is the missing dependency edge: `hk-detect`, `hk-core`,
//! `hk-dsp` and `hk-estimate` do not depend on `hk-gnss`, so a `PrnCodebook` is un-nameable inside
//! the detector. `crates/hk-gnss/tests/blind_path_boundary.rs` fails if that edge appears.
//!
//! # So the wiring runs *around* the boundary, not through it
//!
//! This module is the only place in the product that names both `hk_gnss` and the rest of the
//! pipeline, and it is not on the blind path. The route has three legs and each one crosses a
//! crate the detector cannot reach:
//!
//! 1. **C04 asks.** [`crate::control::SchedState`] offers the scheduler a recurring
//!    [`hk_core::scheduler::bandit::ScheduledDwell`] at L1 — tier 3, the scheduled-plan tier —
//!    **only when the run's own scan plan already covers the L1 band**. The scheduler owes nothing
//!    to GNSS: it is asked for a frequency, a rate and a duration, the way anything else asks.
//! 2. **Acquisition consumes that dwell.** When the control thread applies a step whose purpose is
//!    [`GNSS_SCHEDULED_TARGET`], it [`GnssDwell::arm`]s this service for the step's window. The
//!    `hk-gnss` reader then — and only then — takes one contiguous window of raw IQ off the ring,
//!    digitally down-converts L1 to baseband, and runs `hk_gnss::acquire` over it with the
//!    `KnownCodeLed` witness. Nothing acquires without a scheduled dwell to consume, so the
//!    cadence is *once per dwell C04 granted*, not once per block or once per detection.
//! 3. **The result reaches C30 as evidence.** `hk_gnss::assess_jamming` turns the acquisition plus
//!    in-band power into a verdict, which becomes a [`hk_context::GnssServiceEvidence`] — plain
//!    scalars, no codes — and is attached by
//!    [`hk_context::gnss_service::attach_to_open_anomalies`] to anomalies **the blind path already
//!    opened**.
//!
//! # Evidence is not a detection, and this is where that has to be true
//!
//! A Gold code correlating must never put a row in the inventory. That is the same leak as the
//! forbidden dependency edge arriving by a different door, and the door is closed on both sides:
//! `hk-context`'s writer only ever *appends an explanation to an existing anomaly* and cannot name
//! a `Detection` or an `Emitter`; this module never touches the inventory, the tracker, or
//! `hk-detect` at all. If a GNSS dwell finds a dead constellation and the blind path saw nothing
//! unusual, **nothing is written** — that is the designed outcome, not a gap. Satellites lost with
//! no in-band power rise is a blocked antenna (a handheld indoors, a body over the patch), and
//! filling the attack map with the user's own thumb is the C36 pitfall.
//!
//! # What it costs, and why it is a reader thread
//!
//! One acquisition searches the whole 32-satellite codebook over a Doppler grid: tens of
//! milliseconds of IQ, but a few thousand FFT pairs. That belongs on its own thread, like the
//! receiver survey (T-399) — not on the detection path, which must not wait for it. The window it
//! needs is tiny (tens of ms), so it never holds the ring back; it registers a
//! [`crate::gate::GateCursor`] like every other reader so lossless replay stays deterministic.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hk_context::gnss_service::{GnssServiceEvidence, GnssServiceVerdict, L1_BAND_HZ};
use hk_core::ReadOutcome;
use hk_dsp::ddc::{DdcKernel, DdcSpec};
use hk_gnss::{
    AcquisitionConfig, IntegrityConfig, JammingVerdict, KnownCodeLed, LockEvidence, PowerEvidence,
    PrnCodebook, acquire,
};
use hk_model::{FreqRange, Provenance, TimeRange, Timestamp};
use num_complex::{Complex, Complex32};

use crate::run::Shared;

/// The scheduled-dwell id C04 knows this request by. Steps carrying
/// `Purpose::Scheduled { target }` with this value are L1 dwells.
pub const GNSS_SCHEDULED_TARGET: u32 = 0x676e_7331;

/// L1 centre, from the codebook's own constant rather than a literal here.
pub const L1_CENTER_HZ: f64 = hk_gnss::L1_HZ;

/// How the L1 dwell is requested and consumed.
#[derive(Clone, Copy, Debug)]
pub struct GnssDwellConfig {
    /// Dwell centre, Hz.
    pub center_hz: f64,
    /// How long C04 is asked to stay there, ns.
    pub dwell_ns: i64,
    /// How often the dwell repeats, ns.
    pub every_ns: i64,
    /// Contiguous raw IQ taken off the ring per dwell, s. Only a few ms are correlated; the rest
    /// is DDC warm-up and margin.
    pub window_s: f64,
    /// Two-sided bandwidth kept around L1 for acquisition, Hz. The C/A main lobe is ±1.023 MHz.
    pub bandwidth_hz: f64,
    /// Acquisition search settings (rate is filled in from the DDC's output rate).
    pub acq: AcquisitionConfig,
    /// Jamming/spoofing thresholds.
    pub integrity: IntegrityConfig,
    /// Target per-satellite false-alarm probability for the acquisition threshold. The threshold
    /// itself is computed per dwell from this and the profile length; see
    /// [`acquisition_threshold`].
    pub false_alarm: f64,
    /// Fixes the peak-to-mean threshold instead of computing it. For experiments only — a fixed
    /// bar does not survive a change of sample rate.
    pub threshold_override: Option<f32>,
}

impl Default for GnssDwellConfig {
    fn default() -> Self {
        Self {
            center_hz: L1_CENTER_HZ,
            dwell_ns: 500_000_000,
            every_ns: 60_000_000_000,
            window_s: 0.06,
            bandwidth_hz: 2.0e6,
            acq: AcquisitionConfig {
                // 1 ms coherent × 4 non-coherent blocks: 4 ms of signal, ~30 dB of processing
                // gain, and well inside the 20 ms navigation bit.
                coherent_ms: 1,
                noncoherent_blocks: 4,
                doppler_max_hz: 5_000.0,
                doppler_step_hz: 500.0,
                // Overwritten per dwell by `acquisition_threshold`.
                threshold_ratio: 2.5,
                // Overwritten per dwell with the DDC's actual output rate.
                sample_rate_hz: 2_046_000.0,
            },
            integrity: IntegrityConfig::default(),
            false_alarm: 1.0e-4,
            threshold_override: None,
        }
    }
}

/// The peak-to-mean bar for a code-phase profile of `cells` cells accumulated over `blocks`
/// non-coherent blocks, at a target per-satellite false-alarm probability `p_fa`.
///
/// **This has to be computed, not fixed, and getting it wrong is not subtle.**
/// `AcquisitionConfig`'s default of 2.5 is a floor sized for a short profile. The number of
/// code-phase cells searched is the *samples per 1 ms code period*, so it rises with the rate —
/// 2046 cells at the minimum rate, 4000 at 4 Msps, 20 000 at 20 Msps — and each cell is another
/// chance for noise to clear a fixed bar. Left at 2.5 over a 4000-cell profile, acquisition
/// returns **all 32 satellites out of pure noise**, which would make every dwell report an intact
/// constellation and quietly disable the whole jamming assessment.
///
/// The normalised profile is Gamma(`blocks`)/`blocks`, so the per-cell upper tail is
/// `P(X > x) ≈ e^{-k x} (k x)^{k-1} / (k-1)!` with `k = blocks`, and the bar is the `x` where
/// `cells · P(X > x) = p_fa`. Solved by bisection — the tail is monotone.
pub fn acquisition_threshold(cells: usize, blocks: usize, p_fa: f64) -> f32 {
    let k = blocks.max(1) as f64;
    let n = cells.max(1) as f64;
    let target = (p_fa.clamp(1e-12, 0.5) / n).ln();
    // ln P(X > x) = -k·x + (k−1)·ln(k·x) − ln((k−1)!)
    let ln_fact: f64 = (1..blocks.max(1)).map(|i| (i as f64).ln()).sum();
    let ln_tail = |x: f64| -k * x + (k - 1.0) * (k * x).ln() - ln_fact;
    let (mut lo, mut hi) = (1.0f64, 200.0f64);
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if ln_tail(mid) > target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (0.5 * (lo + hi)) as f32
}

/// Reads `extra.gnss` from the scan plan: `{"dwell_s", "every_s", "window_s", "bandwidth_hz",
/// "coherent_ms", "noncoherent_blocks", "doppler_max_hz", "doppler_step_hz", "threshold_ratio"}`.
///
/// Absent or `null` leaves [`GnssDwellConfig::default`]. The **centre is not settable**: L1 is
/// where L1 is, and a knob for it would be a frequency list arriving through config.
pub fn gnss_config(plan: &hk_model::ScanPlan) -> anyhow::Result<GnssDwellConfig> {
    let mut cfg = GnssDwellConfig::default();
    let over = match plan.extra.get("gnss") {
        None | Some(serde_json::Value::Null) => return Ok(cfg),
        Some(serde_json::Value::Object(o)) => o,
        Some(other) => anyhow::bail!("extra.gnss must be an object, not {other}"),
    };
    let secs = |v: &serde_json::Value| -> anyhow::Result<i64> {
        let s = v
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("expected seconds"))?;
        Ok((s * 1e9) as i64)
    };
    let num = |v: &serde_json::Value| -> anyhow::Result<f64> {
        v.as_f64()
            .ok_or_else(|| anyhow::anyhow!("expected a number"))
    };
    let count = |v: &serde_json::Value| -> anyhow::Result<usize> {
        Ok(v.as_u64()
            .ok_or_else(|| anyhow::anyhow!("expected a count"))? as usize)
    };
    for (k, v) in over {
        match k.as_str() {
            "dwell_s" => cfg.dwell_ns = secs(v)?,
            "every_s" => cfg.every_ns = secs(v)?,
            "window_s" => cfg.window_s = num(v)?,
            "bandwidth_hz" => cfg.bandwidth_hz = num(v)?,
            "coherent_ms" => cfg.acq.coherent_ms = count(v)?,
            "noncoherent_blocks" => cfg.acq.noncoherent_blocks = count(v)?,
            "doppler_max_hz" => cfg.acq.doppler_max_hz = num(v)?,
            "doppler_step_hz" => cfg.acq.doppler_step_hz = num(v)?,
            "threshold_ratio" => cfg.threshold_override = Some(num(v)? as f32),
            "false_alarm" => cfg.false_alarm = num(v)?,
            other => anyhow::bail!("extra.gnss: unknown key {other}"),
        }
    }
    Ok(cfg)
}

/// One dwell's measurement, kept so the next dwell has something to be a loss against.
#[derive(Clone, Debug, PartialEq)]
pub struct GnssL1Measurement {
    /// When the dwell's first correlated sample was captured.
    pub t: Timestamp,
    /// Satellites searched (the codebook's size).
    pub svs_searched: u32,
    /// Satellites acquired, strongest first.
    pub acquired_prns: Vec<u8>,
    /// Mean estimated C/N0 of the acquired satellites, dB-Hz.
    pub mean_cn0_dbhz: Option<f64>,
    /// In-band power over the acquisition band, dBFS.
    pub power_dbfs: f64,
    /// Power above the quietest dwell of this capture state, dB.
    pub power_rise_db: f64,
    /// The service verdict.
    pub verdict: GnssServiceVerdict,
    /// Confidence in the verdict.
    pub confidence: f64,
    /// The acquisition's own provenance string: which code set led the search. Recorded because
    /// ADR-0018 requires every known-code-led result to say so.
    pub codebook: &'static str,
}

/// Per-capture-state history: what "normal" looked like here.
#[derive(Default)]
struct StateHistory {
    /// The provenance identity this history belongs to (device, centre, rate, gains).
    key: Option<String>,
    /// Quietest in-band power seen in this state, dBFS — the reference a rise is measured against.
    quiet_dbfs: Option<f64>,
    /// Most satellites acquired in this state — the reference a loss is measured against.
    best_svs: u32,
    /// Mean C/N0 when `best_svs` was acquired.
    best_cn0_dbhz: Option<f64>,
}

#[derive(Default)]
struct State {
    /// The dwell window C04 granted and this service has not yet consumed.
    armed: Option<TimeRange>,
    history: StateHistory,
    last: Option<GnssL1Measurement>,
}

/// Counters a test can hold the wiring to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GnssCounts {
    /// Dwells C04 was asked for (1 per run when the plan covers L1, 0 otherwise).
    pub requested: u64,
    /// Scheduled L1 steps the control thread applied, i.e. dwells C04 actually granted.
    pub granted: u64,
    /// Acquisitions run.
    pub acquisitions: u64,
    /// Dwells that could not be acquired (rate not usable, tune not covering L1, window broken).
    pub abstained: u64,
    /// Explanations written to C30.
    pub explanations: u64,
    /// Time spent acquiring, µs.
    pub cost_us: u64,
}

/// The L1 dwell service: what C04 granted, what acquisition found, and what reached C30.
///
/// Lives for the whole run, like the receiver survey — the constellation overhead and the quiet
/// in-band reference are properties of the site and the receiver, not of a pipeline segment.
pub struct GnssDwell {
    cfg: GnssDwellConfig,
    codebook: PrnCodebook,
    state: Mutex<State>,
    requested: AtomicU64,
    granted: AtomicU64,
    acquisitions: AtomicU64,
    abstained: AtomicU64,
    explanations: AtomicU64,
    cost_us: AtomicU64,
}

impl Default for GnssDwell {
    fn default() -> Self {
        Self::new(GnssDwellConfig::default())
    }
}

impl GnssDwell {
    /// Builds the service with the GPS L1 C/A codebook.
    pub fn new(cfg: GnssDwellConfig) -> Self {
        Self {
            cfg,
            codebook: PrnCodebook::gps_l1_ca(),
            state: Mutex::new(State::default()),
            requested: AtomicU64::new(0),
            granted: AtomicU64::new(0),
            acquisitions: AtomicU64::new(0),
            abstained: AtomicU64::new(0),
            explanations: AtomicU64::new(0),
            cost_us: AtomicU64::new(0),
        }
    }

    /// The dwell settings.
    pub fn config(&self) -> &GnssDwellConfig {
        &self.cfg
    }

    /// The band this service speaks about.
    pub fn band(&self) -> FreqRange {
        FreqRange::new(L1_BAND_HZ.0, L1_BAND_HZ.1)
    }

    /// Records that C04 accepted the standing dwell request.
    pub fn record_request(&self) {
        self.requested.fetch_add(1, Ordering::Relaxed);
    }

    /// C04 granted a dwell over `window`: the reader may take one acquisition from it.
    ///
    /// This is the only way an acquisition is ever reached. Without a scheduled step there is no
    /// armed window, and the reader holds nothing.
    pub fn arm(&self, window: TimeRange) {
        self.granted.fetch_add(1, Ordering::Relaxed);
        lock(&self.state).armed = Some(window);
    }

    /// Whether a granted dwell is waiting to be consumed.
    pub fn is_armed(&self) -> bool {
        lock(&self.state).armed.is_some()
    }

    /// The most recent acquisition, if any.
    pub fn last(&self) -> Option<GnssL1Measurement> {
        lock(&self.state).last.clone()
    }

    /// The counters.
    pub fn counts(&self) -> GnssCounts {
        GnssCounts {
            requested: self.requested.load(Ordering::Relaxed),
            granted: self.granted.load(Ordering::Relaxed),
            acquisitions: self.acquisitions.load(Ordering::Relaxed),
            abstained: self.abstained.load(Ordering::Relaxed),
            explanations: self.explanations.load(Ordering::Relaxed),
            cost_us: self.cost_us.load(Ordering::Relaxed),
        }
    }

    /// Whether `p` is a capture state in which L1 can be acquired at all: the tuned window must
    /// contain the dwell centre with its acquisition bandwidth, and the rate must be decimatable
    /// to a whole number of samples per 1 ms code period.
    fn usable(&self, p: &Provenance) -> Option<f64> {
        let fs = p.tune.sample_rate_hz;
        if !(fs.is_finite() && fs > 0.0) {
            return None;
        }
        let offset = self.cfg.center_hz - p.tune.center_hz;
        if offset.abs() + self.cfg.bandwidth_hz / 2.0 > fs / 2.0 {
            return None;
        }
        acquirable_rate(fs)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The largest integer decimation of `fs` that still gives at least 2 samples per chip and a whole
/// number of samples per 1 ms code period — `hk_gnss::acquire`'s two requirements on its input.
///
/// Returns the decimated rate, or `None` when no decimation of this rate is acquirable (a rate
/// below about 2.046 Msps, or one that is not a whole number of samples per millisecond).
pub fn acquirable_rate(fs: f64) -> Option<f64> {
    const MIN_HZ: f64 = 2_046_000.0;
    if !(fs.is_finite() && fs >= MIN_HZ) {
        return None;
    }
    let max_decim = (fs / MIN_HZ).floor() as usize;
    (1..=max_decim.max(1)).rev().find_map(|d| {
        let out = fs / d as f64;
        let per_ms = out / 1000.0;
        (out >= MIN_HZ && (per_ms - per_ms.round()).abs() < 1e-9).then_some(out)
    })
}

/// The `hk-gnss` reader: consumes one granted L1 dwell at a time (see the [module docs](self)).
pub(crate) fn run(shared: Arc<Shared>, gnss: Arc<GnssDwell>) -> anyhow::Result<()> {
    let mut reader = shared.ring.reader_at(0);
    let cursor = shared.gate.register(0);
    let mut buf = vec![Complex::<i8>::default(); 1 << 16];
    let mut window: Vec<Complex32> = Vec::new();
    let mut held: Option<(hk_core::ProvenanceHandle, hk_model::SampleTime)> = None;
    let mut next_index: Option<u64> = None;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(chunk) => {
                cursor.set(chunk.end_sample());
                if !gnss.is_armed() {
                    // No dwell granted: hold nothing. Acquisition is reachable only through C04.
                    window.clear();
                    held = None;
                    next_index = None;
                    continue;
                }
                let p = chunk.provenance.get();
                let Some(out_rate) = gnss.usable(p) else {
                    // A dwell was granted but this capture state cannot answer it. Give the
                    // window back rather than acquiring on samples that are not L1.
                    lock(&gnss.state).armed = None;
                    gnss.abstained.fetch_add(1, Ordering::Relaxed);
                    window.clear();
                    held = None;
                    next_index = None;
                    continue;
                };
                let fs = p.tune.sample_rate_hz;
                let broken = chunk.block_start && !chunk.discontinuity.is_empty();
                let continues = !broken
                    && next_index == Some(chunk.first_sample())
                    && held
                        .as_ref()
                        .is_some_and(|(h, _)| h.id() == chunk.provenance.id());
                if !continues {
                    window.clear();
                    held = Some((chunk.provenance.clone(), chunk.time));
                }
                let want = ((gnss.cfg.window_s * fs) as usize).max(1);
                window.reserve(want.saturating_sub(window.len()).min(chunk.len));
                window.extend(
                    buf[..chunk.len]
                        .iter()
                        .map(|s| Complex32::new(f32::from(s.re) / 128.0, f32::from(s.im) / 128.0)),
                );
                next_index = Some(chunk.end_sample());
                if window.len() < want {
                    continue;
                }
                window.truncate(want);
                let Some((prov, time)) = held.take() else {
                    continue;
                };
                next_index = None;
                let t0 = Instant::now();
                let measured = measure(&gnss, &window, prov.get(), fs, out_rate, time);
                let cost = t0.elapsed();
                window = Vec::new();
                // The dwell is consumed whatever the outcome: one acquisition per granted dwell.
                lock(&gnss.state).armed = None;
                gnss.cost_us
                    .fetch_add(cost.as_micros() as u64, Ordering::Relaxed);
                match measured {
                    Some(m) => {
                        gnss.acquisitions.fetch_add(1, Ordering::Relaxed);
                        deliver(&shared, &gnss, &m);
                        lock(&gnss.state).last = Some(m);
                    }
                    None => {
                        gnss.abstained.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            ReadOutcome::Overrun { resume_at, .. } => {
                window.clear();
                held = None;
                next_index = None;
                cursor.set(resume_at);
            }
            ReadOutcome::Empty => {}
            ReadOutcome::Closed => break,
        }
    }
    drop(cursor);
    Ok(())
}

/// Down-converts L1 to baseband, acquires, and assesses. Returns `None` when the DDC could not be
/// planned or the window held too little signal to correlate.
fn measure(
    gnss: &GnssDwell,
    window: &[Complex32],
    p: &Provenance,
    fs: f64,
    out_rate: f64,
    time: hk_model::SampleTime,
) -> Option<GnssL1Measurement> {
    let spec = DdcSpec::new(gnss.cfg.center_hz - p.tune.center_hz, gnss.cfg.bandwidth_hz)
        .with_output_rate(out_rate);
    let mut kernel = DdcKernel::new(&spec, fs).ok()?;
    let mut base = Vec::with_capacity(kernel.max_outputs(window.len()));
    kernel.process(window, &mut base);
    // Discard the filter's warm-up: until a whole span of input has gone through, the outputs are
    // a ramp rather than the band.
    let warmup = (kernel.span_samples() as f64 * out_rate / fs).ceil() as usize + 16;
    let per_ms = (out_rate / 1000.0).round() as usize;
    let need = per_ms * gnss.cfg.acq.coherent_ms * gnss.cfg.acq.noncoherent_blocks;
    if base.len() < warmup + need {
        return None;
    }
    let iq = &base[warmup..warmup + need];

    let mean_power: f64 = iq.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>() / iq.len() as f64;
    let power_dbfs = 10.0 * mean_power.max(1e-20).log10();

    let mut cfg = gnss.cfg.acq;
    cfg.sample_rate_hz = out_rate;
    cfg.threshold_ratio = gnss.cfg.threshold_override.unwrap_or_else(|| {
        acquisition_threshold(per_ms, cfg.noncoherent_blocks, gnss.cfg.false_alarm)
    });
    // The witness: this call is known-signal-**led**, and says so in its own signature
    // (ADR-0018 §3). Its only constructor takes the published code set.
    let led = KnownCodeLed::with_codebook(&gnss.codebook);
    let result = acquire(&led, &gnss.codebook, iq, &cfg).ok()?;

    let acquired_prns: Vec<u8> = result.acquired.iter().map(|a| a.prn).collect();
    let mean_cn0_dbhz = (!result.acquired.is_empty()).then(|| {
        result
            .acquired
            .iter()
            .map(|a| f64::from(a.cn0_dbhz))
            .sum::<f64>()
            / result.acquired.len() as f64
    });

    // Update this capture state's references, then measure against them.
    let (quiet_dbfs, svs_reference, reference_cn0) = {
        let mut st = lock(&gnss.state);
        let key = state_key(p);
        if st.history.key.as_deref() != Some(key.as_str()) {
            st.history = StateHistory {
                key: Some(key),
                ..StateHistory::default()
            };
        }
        let quiet = match st.history.quiet_dbfs {
            Some(q) if q <= power_dbfs => q,
            _ => {
                st.history.quiet_dbfs = Some(power_dbfs);
                power_dbfs
            }
        };
        let svs_now = acquired_prns.len() as u32;
        if svs_now > st.history.best_svs {
            st.history.best_svs = svs_now;
            st.history.best_cn0_dbhz = mean_cn0_dbhz;
        }
        (quiet, st.history.best_svs, st.history.best_cn0_dbhz)
    };
    let power_rise_db = power_dbfs - quiet_dbfs;

    let power = PowerEvidence {
        observed_floor_dbfs: power_dbfs as f32,
        baseline_floor_dbfs: quiet_dbfs as f32,
        center_hz: gnss.cfg.center_hz,
        bandwidth_hz: gnss.cfg.bandwidth_hz,
    };
    let cn0_drop = match (reference_cn0, mean_cn0_dbhz) {
        (Some(before), Some(now)) => (before - now).max(0.0) as f32,
        (Some(_), None) => gnss.cfg.integrity.cn0_drop_db,
        _ => 0.0,
    };
    let lock_ev = LockEvidence {
        svs_before: svs_reference.min(u32::from(u8::MAX)) as u8,
        svs_now: acquired_prns.len().min(usize::from(u8::MAX)) as u8,
        mean_cn0_drop_db: cn0_drop,
    };
    let assessment = hk_gnss::assess_jamming(&power, Some(&lock_ev), &gnss.cfg.integrity);

    Some(GnssL1Measurement {
        t: time.host_time,
        svs_searched: gnss.codebook.len() as u32,
        acquired_prns,
        mean_cn0_dbhz,
        power_dbfs,
        power_rise_db,
        verdict: verdict_of(assessment.verdict),
        confidence: f64::from(assessment.confidence),
        codebook: led.codebook(),
    })
}

fn verdict_of(v: JammingVerdict) -> GnssServiceVerdict {
    match v {
        JammingVerdict::Quiet => GnssServiceVerdict::Quiet,
        JammingVerdict::BlockageSuspect => GnssServiceVerdict::BlockageSuspect,
        JammingVerdict::JammingSuspect => GnssServiceVerdict::JammingSuspect,
    }
}

/// The capture state a quiet reference and a satellite count belong to.
fn state_key(p: &Provenance) -> String {
    format!(
        "{}|{:.0}|{:.0}|{:.1}|{:.1}|{}",
        p.device_id,
        p.tune.center_hz,
        p.tune.sample_rate_hz,
        p.tune.lna_db,
        p.tune.vga_db,
        p.tune.amp_on
    )
}

/// Hands the measurement to C30 as evidence on anomalies the **blind path** opened.
///
/// The one thing this must never do is create something to explain. It queries; if the blind
/// detector found nothing in L-band, nothing is written.
fn deliver(shared: &Arc<Shared>, gnss: &GnssDwell, m: &GnssL1Measurement) {
    let st = lock(&gnss.state);
    let svs_reference = st.history.best_svs;
    drop(st);
    let ev = GnssServiceEvidence {
        t: m.t,
        band: gnss.band(),
        svs_searched: m.svs_searched,
        svs_acquired: m.acquired_prns.len() as u32,
        svs_reference,
        mean_cn0_dbhz: m.mean_cn0_dbhz,
        power_rise_db: m.power_rise_db,
        verdict: m.verdict,
        confidence: m.confidence,
        reasons: Vec::new(),
    };
    if !ev.verdict.is_notable() {
        return;
    }
    // Anomalies open anywhere in the retained window: a floor rise that began before this dwell
    // is exactly the case this evidence speaks to.
    let lookback = TimeRange::new(
        Timestamp::from_unix_nanos(m.t.as_unix_nanos().saturating_sub(24 * 3_600_000_000_000)),
        m.t,
    );
    let mut repo = shared.repo();
    match hk_context::gnss_service::attach_to_open_anomalies(&mut repo, &ev, lookback) {
        Ok(written) => {
            gnss.explanations
                .fetch_add(written.len() as u64, Ordering::Relaxed);
        }
        Err(e) => {
            eprintln!("hk-gnss: writing L1 service evidence: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_is_decimated_to_something_acquire_accepts() {
        // Whole samples per millisecond, at least two per chip.
        for fs in [
            2_046_000.0,
            2_400_000.0,
            4_000_000.0,
            8_000_000.0,
            20e6,
            10e6,
        ] {
            let out = acquirable_rate(fs).unwrap_or_else(|| panic!("{fs} should decimate"));
            assert!(out >= 2_046_000.0, "{fs} -> {out}");
            let per_ms = out / 1000.0;
            assert!((per_ms - per_ms.round()).abs() < 1e-9, "{fs} -> {out}");
            let d = fs / out;
            assert!(
                (d - d.round()).abs() < 1e-9,
                "{fs} -> {out} is not integer decimation"
            );
        }
    }

    /// The bar must rise with the number of cells searched. A fixed 2.5 over a 4000-cell profile
    /// acquires the whole constellation out of noise, which is the failure this replaces.
    #[test]
    fn the_acquisition_bar_rises_with_the_profile_it_searches() {
        let short = acquisition_threshold(2046, 4, 1e-3);
        let long = acquisition_threshold(20_000, 4, 1e-3);
        assert!(long > short, "{short} -> {long}");
        assert!(
            short > 2.5,
            "a 2046-cell profile already needs more than 2.5: {short}"
        );
        // Stricter false alarm, higher bar; more non-coherent averaging, lower bar.
        assert!(acquisition_threshold(4000, 4, 1e-6) > acquisition_threshold(4000, 4, 1e-3));
        assert!(acquisition_threshold(4000, 16, 1e-3) < acquisition_threshold(4000, 4, 1e-3));
    }

    /// The tail approximation must actually hit the false-alarm rate it claims. Draws Gamma(k)/k
    /// profiles from a deterministic generator and counts how often the maximum clears the bar.
    #[test]
    fn the_bar_delivers_about_the_false_alarm_rate_it_promises() {
        const CELLS: usize = 4000;
        const BLOCKS: usize = 4;
        let bar = f64::from(acquisition_threshold(CELLS, BLOCKS, 1e-3));
        let mut state = 0x243f_6a88_85a3_08d3u64;
        let mut unit = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f64 / (1u64 << 53) as f64).max(1e-15)
        };
        let mut fired = 0;
        const TRIALS: usize = 2000;
        for _ in 0..TRIALS {
            let mut peak = 0.0f64;
            for _ in 0..CELLS {
                // Gamma(k,1) as a sum of k exponentials, normalised to mean 1.
                let g: f64 = (0..BLOCKS).map(|_| -unit().ln()).sum();
                peak = peak.max(g / BLOCKS as f64);
            }
            if peak > bar {
                fired += 1;
            }
        }
        let rate = fired as f64 / TRIALS as f64;
        assert!(
            rate < 1e-2,
            "{fired}/{TRIALS} noise-only profiles cleared {bar}: {rate}"
        );
    }

    #[test]
    fn a_rate_too_low_for_l1_is_refused_rather_than_fudged() {
        assert_eq!(acquirable_rate(1.0e6), None);
        assert_eq!(acquirable_rate(2.0e6), None);
        assert_eq!(acquirable_rate(f64::NAN), None);
    }

    fn provenance(center_hz: f64, fs: f64) -> Provenance {
        serde_json::from_value(serde_json::json!({
            "device_id": "synthetic:gnss",
            "tune": {"center_hz": center_hz, "sample_rate_hz": fs, "lna_db": 24.0,
                     "vga_db": 20.0, "amp_on": false, "bandwidth_hz": fs * 0.75},
            "overload": false, "quantisation_limited": false, "clock_source": "internal",
            "clock_locked": true, "timestamp_method": "synthetic",
            "timestamp_error_budget_ns": 0,
        }))
        .expect("provenance")
    }

    #[test]
    fn a_tune_that_does_not_contain_l1_is_not_usable() {
        let g = GnssDwell::default();
        assert!(g.usable(&provenance(100.0e6, 2.4e6)).is_none());
        assert!(g.usable(&provenance(L1_CENTER_HZ, 2.4e6)).is_some());
        // L1 inside the window but with the acquisition band hanging over the edge.
        assert!(g.usable(&provenance(L1_CENTER_HZ - 1.0e6, 2.4e6)).is_none());
    }

    /// Acquisition is reachable only through a granted dwell. Nothing else arms the reader.
    #[test]
    fn nothing_is_armed_until_c04_grants_a_dwell() {
        let g = GnssDwell::default();
        assert!(!g.is_armed());
        assert_eq!(g.counts().granted, 0);
        g.arm(TimeRange::new(
            Timestamp::from_unix_nanos(0),
            Timestamp::from_unix_nanos(1),
        ));
        assert!(g.is_armed());
        assert_eq!(g.counts().granted, 1);
    }
}
