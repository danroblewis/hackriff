//! Automatic front-end gain management (T-945; [docs/28](../../../docs/28-front-end-gain-management.md)).
//!
//! The front end's gain is a **trade the device has to make from its own processed output**, not a
//! reaction to a flag. The evidence is field evidence: on 2026-09-25 the explorer ran a live HackRF
//! in the San Francisco FM band, every clip was flagged `overload: true` at LNA 32 / VGA 30 / amp on
//! and **nothing reduced the gain** — while a manual reduction to 48 dB made the decode *worse* (98.9
//! MHz: 106 RDS groups → 0). Both failure modes are real and they sit on opposite sides of one axis:
//! too much gain saturates the ADC and fills the window with the receiver's own intermodulation; too
//! little drops the emission you were working towards the LSB and the receiver's own post-gain noise.
//! There is an interior optimum, it belongs to the scene and to *what is being decoded*, and it can
//! only be found by measuring.
//!
//! This module is the policy, and only the policy: device-generic, no I/O, nothing about a HackRF.
//!
//! - [`GainLadder`] turns a device's [`SourceCapabilities`] into a monotone one-dimensional ladder
//!   through its gain-stage cross product (§3.1 of the note): on/off stages engaged last, the rest
//!   balanced by fraction of their own range.
//! - [`GainQuality`] is what **one dwell at one state** measured — clip fraction, the device's
//!   overload flag, channel SNR, and optionally [`DecodeQuality`], the processed output. It carries
//!   the state it was measured under, because a measurement that cannot name its state is not
//!   evidence.
//! - [`GainScore`] orders states: **decode rate, then SNR, then clip fraction by decade, then lower
//!   total gain**. The overload evidence is a tiebreak between states whose output is
//!   indistinguishable — never a veto. A term that was not measured sorts below any measured value
//!   (nothing said is not a value, the T-325 rule).
//! - [`GainController`] runs the bounded two-pass search (coarse at a stride over the ladder, then a
//!   fine walk of the finest stage around the coarse best), probes no state twice, commits the
//!   argmax and explains itself in a [`GainReport`].
//!
//! **Off by default.** [`GainPolicy::default`] is disabled, and a disabled controller emits
//! [`GainStep::Disabled`] and no device action, ever: this moves the radio on its own and ships dark
//! until HIL proves it (docs/28 §3.4).
//!
//! The actuator lives in `hk_api::gain`, because every probe must be one
//! `hk_api::DeviceAction::Gains` through the one device gate (T-343). Nothing here touches a device.

use std::fmt;

use crate::source::{GainStage, NamedGain, SourceCapabilities};

/// Two gain values are the same setting (dB, after the device's own quantisation).
const DB_EPS: f64 = 1e-6;

/// `db` with an accumulated-rounding gap to a stage's own limit closed: 32 steps of a continuous
/// stage sum to its maximum in decimal but not in binary, and a ladder whose top rung reads
/// 49.59999999999998 dB claims a gain the device does not have a name for.
fn snap(stage: &GainStage, db: f64) -> f64 {
    if (stage.max_db - db).abs() < 1e-9 {
        stage.max_db
    } else if (db - stage.min_db).abs() < 1e-9 {
        stage.min_db
    } else {
        db
    }
}

/// One complete front-end gain setting: a value for every stage the device declares.
///
/// Canonical: the stages are held sorted by name, so two states built in different orders compare
/// equal, and [`Self::total_db`] is the ladder's ordering key.
#[derive(Clone, Debug, PartialEq)]
pub struct GainState {
    gains: Vec<NamedGain>,
}

impl GainState {
    /// A state from named stage values (order-insensitive; duplicates keep the last).
    pub fn new(gains: impl IntoIterator<Item = NamedGain>) -> Self {
        let mut v: Vec<NamedGain> = Vec::new();
        for g in gains {
            match v.iter_mut().find(|x| x.stage == g.stage) {
                Some(x) => x.db = g.db,
                None => v.push(g),
            }
        }
        v.sort_by(|a, b| a.stage.cmp(&b.stage));
        Self { gains: v }
    }

    /// The stage values, sorted by stage name.
    pub fn gains(&self) -> &[NamedGain] {
        &self.gains
    }

    /// The value of one stage, dB.
    pub fn get(&self, stage: &str) -> Option<f64> {
        self.gains.iter().find(|g| g.stage == stage).map(|g| g.db)
    }

    /// Total front-end gain, dB (the sum over stages: what the ladder is ordered by).
    pub fn total_db(&self) -> f64 {
        self.gains.iter().map(|g| g.db).sum()
    }

    /// The same setting, to within the device's own quantisation.
    pub fn same_as(&self, other: &Self) -> bool {
        self.gains.len() == other.gains.len()
            && self
                .gains
                .iter()
                .zip(&other.gains)
                .all(|(a, b)| a.stage == b.stage && (a.db - b.db).abs() <= DB_EPS)
    }
}

impl fmt::Display for GainState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, g) in self.gains.iter().enumerate() {
            if i > 0 {
                f.write_str(" / ")?;
            }
            write!(f, "{} {:.0}", g.stage, g.db)?;
        }
        write!(f, " ({:.0} dB)", self.total_db())
    }
}

/// Why a gain policy could not be built or driven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GainError {
    /// The device declares no adjustable gain stage, so there is nothing to manage.
    NoStages {
        /// The device's `capabilities.driver`.
        driver: String,
    },
    /// A quality was offered while no probe was outstanding.
    NotProbing,
    /// A quality was offered for a state that is not the outstanding probe.
    StateMismatch {
        /// The state the controller is waiting for.
        expected: String,
        /// The state the quality was measured under.
        got: String,
    },
}

impl fmt::Display for GainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoStages { driver } => write!(
                f,
                "{driver} declares no adjustable gain stage: there is no gain to manage"
            ),
            Self::NotProbing => f.write_str("no gain probe is outstanding"),
            Self::StateMismatch { expected, got } => write!(
                f,
                "this dwell was measured at {got} but the outstanding probe is {expected}: a \
                 measurement that cannot name its own state is not evidence"
            ),
        }
    }
}

impl std::error::Error for GainError {}

/// A monotone one-dimensional path through a device's gain-stage cross product.
///
/// The cross product is not searchable — a HackRF One has 6 × 32 × 2 = 384 states and every probe
/// costs a settle plus a dwell — so the search walks a ladder built from the capabilities alone
/// (docs/28 §3.1):
///
/// - a stage whose one step spans its whole range (an on/off RF amplifier, [`GainStage::is_binary`])
///   is engaged **last**, at the top: a binary control is the least adjustable thing in the chain,
///   so it is the last resort for more gain rather than a rung the search steps through early;
/// - the graded stages ascend **balanced** — each rung raises whichever is furthest below its own
///   maximum as a *fraction of its range*, ties to the finer step. With no chain-order information
///   in the capabilities descriptor, spreading gain across stages is the standard low-risk
///   distribution; on a HackRF it reproduces the practical advice (move LNA and VGA together, leave
///   the amp off until you need it) without naming a HackRF.
///
/// The result is strictly increasing in [`GainState::total_db`], has one rung per available stage
/// step (HackRF One: 37 rungs over 0…113 dB) and moves one stage by one step per rung.
#[derive(Clone, Debug)]
pub struct GainLadder {
    rungs: Vec<GainState>,
    stages: Vec<GainStage>,
}

impl GainLadder {
    /// The ladder for `caps`, or [`GainError::NoStages`] when the device has no adjustable gain.
    pub fn from_capabilities(caps: &SourceCapabilities) -> Result<Self, GainError> {
        let stages: Vec<GainStage> = caps
            .gain_stages
            .iter()
            .filter(|s| s.span_db() > 0.0)
            .cloned()
            .collect();
        if stages.is_empty() {
            return Err(GainError::NoStages {
                driver: caps.driver.clone(),
            });
        }
        let mut cur: Vec<f64> = stages.iter().map(|s| s.min_db).collect();
        let state = |v: &[f64]| {
            GainState::new(
                stages
                    .iter()
                    .zip(v)
                    .map(|(s, &db)| NamedGain::new(s.name.clone(), db)),
            )
        };
        let mut rungs = vec![state(&cur)];
        // The graded stages, balanced by fraction of their own range.
        loop {
            let mut pick: Option<usize> = None;
            for (i, s) in stages.iter().enumerate() {
                if s.is_binary() || cur[i] >= s.max_db - DB_EPS {
                    continue;
                }
                let frac = (cur[i] - s.min_db) / s.span_db();
                let better = match pick {
                    None => true,
                    Some(j) => {
                        let sj = &stages[j];
                        let fj = (cur[j] - sj.min_db) / sj.span_db();
                        // Lowest fraction of its own range; ties to the finer step, then to the
                        // device's declaration order (`i > j` never wins a tie).
                        frac < fj - 1e-9
                            || ((frac - fj).abs() <= 1e-9
                                && s.fine_step_db() < sj.fine_step_db() - 1e-9)
                    }
                };
                if better {
                    pick = Some(i);
                }
            }
            let Some(i) = pick else { break };
            let s = &stages[i];
            let next = snap(
                s,
                s.quantise((cur[i] + s.fine_step_db()).min(s.max_db))
                    .unwrap_or(s.max_db),
            );
            if next <= cur[i] + DB_EPS {
                // A stage that cannot be raised further by its own step (a pathological
                // quantiser): stop using it rather than spin.
                cur[i] = s.max_db;
                continue;
            }
            cur[i] = next;
            rungs.push(state(&cur));
        }
        // Then the binary stages, in declaration order, each one rung.
        for (i, s) in stages.iter().enumerate() {
            if s.is_binary() && cur[i] < s.max_db - DB_EPS {
                cur[i] = s.max_db;
                rungs.push(state(&cur));
            }
        }
        rungs.dedup_by(|a, b| a.same_as(b));
        Ok(Self { rungs, stages })
    }

    /// The rungs, ascending in total gain.
    pub fn rungs(&self) -> &[GainState] {
        &self.rungs
    }

    /// The device's adjustable stages (those with a non-zero range), in declaration order.
    pub fn stages(&self) -> &[GainStage] {
        &self.stages
    }

    /// The rung whose total gain is nearest `total_db`.
    pub fn nearest_to_db(&self, total_db: f64) -> usize {
        let mut best = 0;
        let mut bd = f64::INFINITY;
        for (i, r) in self.rungs.iter().enumerate() {
            let d = (r.total_db() - total_db).abs();
            if d < bd - 1e-9 {
                bd = d;
                best = i;
            }
        }
        best
    }

    /// `state` clamped onto this device: every stage quantised to its own grid, unknown stages
    /// dropped, missing stages filled from the ladder's bottom rung.
    pub fn realizable(&self, state: &GainState) -> GainState {
        GainState::new(self.stages.iter().map(|s| {
            let db = state
                .get(&s.name)
                .map(|v| v.clamp(s.min_db, s.max_db))
                .and_then(|v| s.quantise(v))
                .unwrap_or(s.min_db);
            NamedGain::new(s.name.clone(), db)
        }))
    }

    /// `from` with the finest stage that can still move in `dir` (+1 up, −1 down) stepped once, or
    /// `None` when nothing can move that way.
    ///
    /// This is the fine phase's move (docs/28 §3.3). It walks the *finest* stage rather than the
    /// ladder, because the ladder's rungs are up to a coarse stage's whole step apart — 8 dB on a
    /// HackRF — and a working window can be narrower than that.
    pub fn step_finest(&self, from: &GainState, dir: i32) -> Option<GainState> {
        let mut pick: Option<&GainStage> = None;
        for s in &self.stages {
            let cur = from.get(&s.name).unwrap_or(s.min_db);
            let room = if dir > 0 {
                cur < s.max_db - DB_EPS
            } else {
                cur > s.min_db + DB_EPS
            };
            if !room {
                continue;
            }
            if pick.is_none_or(|p| s.fine_step_db() < p.fine_step_db() - 1e-9) {
                pick = Some(s);
            }
        }
        let s = pick?;
        let cur = from.get(&s.name).unwrap_or(s.min_db);
        let want = if dir > 0 {
            (cur + s.fine_step_db()).min(s.max_db)
        } else {
            (cur - s.fine_step_db()).max(s.min_db)
        };
        let next = snap(s, s.quantise(want).unwrap_or(want));
        if (next - cur).abs() <= DB_EPS {
            return None;
        }
        let mut out = from.clone();
        match out.gains.iter_mut().find(|g| g.stage == s.name) {
            Some(g) => g.db = next,
            None => out.gains.push(NamedGain::new(s.name.clone(), next)),
        }
        Some(self.realizable(&out))
    }
}

/// The **processed output's** quality over one dwell: the thing the gain choice is actually for.
///
/// A rate, not a count, so dwells of different lengths compare. `metric` names what was counted
/// (`"rds-groups"`, `"crc-valid-frames"`, …) so a report can say what decided, and so two states
/// measured by different metrics are never compared as if they were the same number.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodeQuality {
    /// What was counted, e.g. `"rds-groups"`.
    pub metric: String,
    /// Good units per second (higher is better).
    pub rate_per_s: f64,
    /// Fraction of units that passed their integrity check (CRC/syndrome), when known.
    pub valid_fraction: Option<f64>,
}

impl DecodeQuality {
    /// `count` good units over `dwell_s`.
    pub fn from_count(metric: impl Into<String>, count: u64, dwell_s: f64) -> Self {
        Self {
            metric: metric.into(),
            rate_per_s: if dwell_s > 0.0 {
                count as f64 / dwell_s
            } else {
                0.0
            },
            valid_fraction: None,
        }
    }
}

/// What one dwell at one gain state measured.
///
/// [`Self::state`] is mandatory and is the **applied** state — what the device took after its own
/// quantisation, not what was asked for. Every other field is optional because a device or a
/// pipeline may not measure it; an absent field never beats a present one ([`GainScore`]).
#[derive(Clone, Debug, PartialEq)]
pub struct GainQuality {
    /// The state this dwell was taken under.
    pub state: GainState,
    /// Dwell length, s.
    pub dwell_s: f64,
    /// Fraction of I/Q components at full scale over the dwell (C01's clip count / components).
    pub clip_fraction: f64,
    /// The device's own overload flag over the dwell; `None` when it does not report one.
    pub overload: Option<bool>,
    /// Peak level, dBFS, when measured.
    pub peak_dbfs: Option<f64>,
    /// In-channel SNR of what is being worked, dB. The *guide*: it moves while the decode is still
    /// flat at zero, which is how the search finds the neighbourhood at all (docs/28 §2).
    pub snr_db: Option<f64>,
    /// Noise floor, dBFS/Hz, when measured: a floor that rises faster than the gain change is the
    /// intermodulation signature (C05's gain-step test).
    pub noise_floor_dbfs: Option<f64>,
    /// The processed output. The *judge*.
    pub decode: Option<DecodeQuality>,
}

impl GainQuality {
    /// A quality record for `state` over `dwell_s` with nothing measured yet.
    pub fn new(state: GainState, dwell_s: f64) -> Self {
        Self {
            state,
            dwell_s,
            clip_fraction: 0.0,
            overload: None,
            peak_dbfs: None,
            snr_db: None,
            noise_floor_dbfs: None,
            decode: None,
        }
    }

    /// Builder: the clip fraction and the device's overload flag.
    pub fn with_clip(mut self, clip_fraction: f64, overload: Option<bool>) -> Self {
        self.clip_fraction = clip_fraction;
        self.overload = overload;
        self
    }

    /// Builder: in-channel SNR, dB.
    pub fn with_snr(mut self, snr_db: Option<f64>) -> Self {
        self.snr_db = snr_db;
        self
    }

    /// Builder: the processed output's quality.
    pub fn with_decode(mut self, decode: Option<DecodeQuality>) -> Self {
        self.decode = decode;
        self
    }

    /// This state is grossly saturated: past `policy.clip_ceiling`, where the ADC is a limiter and
    /// nothing measured in the window can be trusted. Never committed, even if it scores best.
    pub fn unusable(&self, policy: &GainPolicy) -> bool {
        self.clip_fraction > policy.clip_ceiling
    }

    /// Order key (see [`GainScore`]).
    pub fn score(&self, policy: &GainPolicy) -> GainScore {
        let bucket = |v: Option<f64>, m: f64| match v {
            Some(v) if v.is_finite() && m > 0.0 => (v / m).floor() as i64,
            _ => i64::MIN,
        };
        let decade = if self.clip_fraction <= 0.0 {
            -9
        } else {
            (self.clip_fraction.log10().floor() as i64).clamp(-9, 0)
        };
        GainScore {
            decode: bucket(
                self.decode.as_ref().map(|d| d.rate_per_s),
                policy.decode_margin_per_s,
            ),
            snr: bucket(self.snr_db, policy.snr_margin_db),
            clip: -decade,
            gain: -(self.state.total_db().round() as i64),
        }
    }
}

/// A gain state's place in the order, as a **bucketed lexicographic** key (docs/28 §3.2).
///
/// Field order is the decision order, and the buckets are why it is an order rather than a
/// preference: quantising each term by its own margin makes the comparison transitive, so the
/// argmax does not depend on the order states happened to be probed in.
///
/// 1. `decode` — the processed output's rate, in units of `decode_margin_per_s`. The judge.
/// 2. `snr` — in units of `snr_margin_db`. The guide, and the tiebreak when nothing decodes.
/// 3. `clip` — the clip fraction's **decade**, negated. This is where the overload evidence acts:
///    between two states whose output is indistinguishable, take the one that pins fewer samples.
///    It is a tiebreak, not a veto — the field evidence is a flagged state that decoded 106 groups
///    while the unflagged one it was "fixed" to decoded none.
/// 4. `gain` — total gain, negated: less gain means less intermodulation risk and less heat, and
///    it makes the choice deterministic.
///
/// A term that was not measured is `i64::MIN`, so it loses to any measured value: nothing said is
/// not a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GainScore {
    /// Decode rate bucket.
    pub decode: i64,
    /// SNR bucket.
    pub snr: i64,
    /// Negated clip-fraction decade.
    pub clip: i64,
    /// Negated total gain, dB.
    pub gain: i64,
}

impl fmt::Display for GainScore {
    /// `decode/snr/clip/gain` in bucket units, with `—` for a term that was not measured, so the
    /// report's trace reads rather than printing `i64::MIN`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = |v: i64| {
            if v == i64::MIN {
                "—".to_string()
            } else {
                v.to_string()
            }
        };
        write!(
            f,
            "decode {} / snr {} / clip {} / gain {}",
            b(self.decode),
            b(self.snr),
            b(self.clip),
            b(self.gain)
        )
    }
}

/// Why a gain run started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GainTrigger {
    /// A person asked for it.
    Manual,
    /// The window moved: a new band has its own gain answer.
    Retune,
    /// What is being worked got worse (a decode rate falling, a lock lost).
    QualityLoss,
    /// The front end reported overload.
    Overload,
    /// A scheduled review of a settled state.
    PeriodicReview,
}

impl GainTrigger {
    /// The stable name used in the report.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Retune => "retune",
            Self::QualityLoss => "quality-loss",
            Self::Overload => "overload",
            Self::PeriodicReview => "periodic-review",
        }
    }
}

impl fmt::Display for GainTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the search behaves. **[`Self::default`] is disabled** (docs/28 §3.4).
#[derive(Clone, Debug, PartialEq)]
pub struct GainPolicy {
    /// Run at all. `false` by default: this moves the radio on its own.
    pub enabled: bool,
    /// Coarse-pass spacing along the ladder, dB.
    pub coarse_stride_db: f64,
    /// Coarse probes (beyond the state in force).
    pub coarse_probes: usize,
    /// Fine probes around the coarse best.
    pub fine_probes: usize,
    /// Hard cap on probes per run, including the state in force.
    pub max_probes: usize,
    /// Dwell per probe, s — how long the caller should measure before answering.
    pub dwell_s: f64,
    /// Settle gap after a gain change, s: samples from before it are not evidence.
    pub settle_s: f64,
    /// Clip fraction above which a state is grossly saturated and never committed.
    pub clip_ceiling: f64,
    /// Clip fraction that counts as "this state is clipping", which is what makes the coarse pass
    /// walk **down** first. C05's flag threshold.
    pub clip_significant: f64,
    /// Decode-rate resolution, per second: differences under this are counting noise.
    pub decode_margin_per_s: f64,
    /// SNR resolution, dB.
    pub snr_margin_db: f64,
    /// Shortest interval between runs, s.
    pub min_rerun_s: f64,
    /// Optional operator clamps on total gain, dB.
    pub min_total_gain_db: Option<f64>,
    /// Optional operator clamp on total gain, dB.
    pub max_total_gain_db: Option<f64>,
}

impl Default for GainPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            coarse_stride_db: 8.0,
            coarse_probes: 6,
            fine_probes: 4,
            max_probes: 12,
            dwell_s: 1.0,
            settle_s: 0.05,
            clip_ceiling: 0.10,
            clip_significant: 1e-4,
            decode_margin_per_s: 0.25,
            snr_margin_db: 1.0,
            min_rerun_s: 30.0,
            min_total_gain_db: None,
            max_total_gain_db: None,
        }
    }
}

impl GainPolicy {
    /// The same policy, enabled.
    pub fn enabled(mut self) -> Self {
        self.enabled = true;
        self
    }

    /// `total_db` is inside the operator clamps.
    fn allows(&self, total_db: f64) -> bool {
        self.min_total_gain_db
            .is_none_or(|m| total_db >= m - DB_EPS)
            && self
                .max_total_gain_db
                .is_none_or(|m| total_db <= m + DB_EPS)
    }
}

/// Which pass a probe belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GainPhase {
    /// The state the radio was already in.
    Start,
    /// The strided walk over the ladder.
    Coarse,
    /// The finest-stage walk around the coarse best.
    Fine,
}

impl GainPhase {
    /// The stable name used in the report.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Coarse => "coarse",
            Self::Fine => "fine",
        }
    }
}

/// What the controller wants next.
#[derive(Clone, Debug, PartialEq)]
pub enum GainStep {
    /// The policy is off. No device action, ever.
    Disabled,
    /// Set this state, wait `settle_s`, measure for `dwell_s`, then [`GainController::observe`].
    Probe(GainProbe),
    /// Set this state and stop: the run is over. The report explains the choice.
    Commit {
        /// The state to leave the front end in.
        state: GainState,
        /// Why (see [`GainReport::why`]).
        report: Box<GainReport>,
    },
    /// Settled: nothing to do until something [`GainController::trigger`]s a new run.
    Hold,
}

/// One probe the caller must measure.
#[derive(Clone, Debug, PartialEq)]
pub struct GainProbe {
    /// The state to command.
    pub state: GainState,
    /// Settle gap before measuring, s.
    pub settle_s: f64,
    /// Dwell to measure, s.
    pub dwell_s: f64,
    /// 0-based probe index within this run.
    pub index: usize,
    /// Probe budget for this run.
    pub budget: usize,
    /// Which pass this probe belongs to.
    pub phase: GainPhase,
}

/// One probe, as recorded in the report.
#[derive(Clone, Debug, PartialEq)]
pub struct GainProbeRecord {
    /// 0-based probe index.
    pub index: usize,
    /// Which pass.
    pub phase: GainPhase,
    /// What was commanded.
    pub commanded: GainState,
    /// What the device took (differs when the device quantised it differently).
    pub applied: GainState,
    /// What the dwell measured.
    pub quality: GainQuality,
    /// Its order key.
    pub score: GainScore,
    /// Not grossly saturated, so eligible to be committed.
    pub usable: bool,
}

/// The outcome of one run, and why (docs/28 §3.5).
#[derive(Clone, Debug, PartialEq)]
pub struct GainReport {
    /// Why the run started.
    pub trigger: GainTrigger,
    /// The state the radio was in when it started.
    pub started_from: GainState,
    /// The state committed.
    pub committed: GainState,
    /// Its order key.
    pub committed_score: GainScore,
    /// The best state that was **not** committed, when there was one.
    pub runner_up: Option<GainState>,
    /// Every probe, in order.
    pub probes: Vec<GainProbeRecord>,
    /// Rungs skipped by the upper-bound prune (above a state both saturated and worse).
    pub pruned: usize,
    /// Every probed state was grossly saturated, so the least bad was committed under protest.
    pub all_unusable: bool,
    /// The committed state is flagged/clipping, and was chosen anyway because its output was
    /// better. Stated rather than hidden.
    pub committed_overloaded: bool,
    /// One line naming what decided, for a log or a panel.
    pub why: String,
}

impl GainReport {
    /// The full trace, one line per probe, with the verdict last.
    pub fn explain(&self) -> String {
        let mut s = format!(
            "gain run ({}) from {}: {} probes\n",
            self.trigger,
            self.started_from,
            self.probes.len()
        );
        for p in &self.probes {
            s.push_str(&format!(
                "  #{} {:>6}: {} clip {:.2e}{} snr {} decode {} score [{}]{}\n",
                p.index,
                p.phase.as_str(),
                p.applied,
                p.quality.clip_fraction,
                match p.quality.overload {
                    Some(true) => " overload",
                    Some(false) => "",
                    None => " overload?",
                },
                p.quality
                    .snr_db
                    .map_or("—".into(), |v| format!("{v:.1} dB")),
                p.quality.decode.as_ref().map_or("—".into(), |d| format!(
                    "{:.2} {}/s",
                    d.rate_per_s, d.metric
                )),
                p.score,
                if p.usable { "" } else { " UNUSABLE" },
            ));
        }
        s.push_str(&format!("  → {}\n", self.why));
        s
    }
}

enum Phase {
    /// Nothing running; the last commit's instant in seconds of the caller's clock.
    Idle,
    Running {
        pending: Vec<(GainState, GainPhase)>,
        phase: GainPhase,
    },
}

/// The bounded, non-oscillating search (docs/28 §3.3).
///
/// Drive it: [`Self::trigger`] to start a run, then [`Self::next_step`] / [`Self::observe`] until it
/// answers [`GainStep::Commit`] or [`GainStep::Hold`]. It holds no device handle and does no I/O;
/// `hk_api::gain::GainManager` is the actuator.
///
/// **Never oscillates, by construction:** a state is probed at most once per run (the visited set
/// *is* the search), the run is bounded by `max_probes`, and after committing the controller holds
/// until a trigger — which is refused inside `min_rerun_s` of the last commit, an interval that
/// doubles (bounded) each time a run re-commits the state it started from.
pub struct GainController {
    policy: GainPolicy,
    ladder: GainLadder,
    phase: Phase,
    visited: Vec<GainState>,
    records: Vec<GainProbeRecord>,
    outstanding: Option<GainProbe>,
    started_from: Option<GainState>,
    trigger: GainTrigger,
    pruned: usize,
    settled: Option<GainState>,
    last_commit_s: Option<f64>,
    rerun_backoff_s: f64,
    now_s: f64,
}

impl GainController {
    /// A controller for `caps` under `policy`.
    pub fn new(caps: &SourceCapabilities, policy: GainPolicy) -> Result<Self, GainError> {
        let ladder = GainLadder::from_capabilities(caps)?;
        let backoff = policy.min_rerun_s;
        Ok(Self {
            policy,
            ladder,
            phase: Phase::Idle,
            visited: Vec::new(),
            records: Vec::new(),
            outstanding: None,
            started_from: None,
            trigger: GainTrigger::Manual,
            pruned: 0,
            settled: None,
            last_commit_s: None,
            rerun_backoff_s: backoff,
            now_s: 0.0,
        })
    }

    /// The policy in force.
    pub fn policy(&self) -> &GainPolicy {
        &self.policy
    }

    /// The ladder this device's capabilities produced.
    pub fn ladder(&self) -> &GainLadder {
        &self.ladder
    }

    /// The last committed state, when a run has settled.
    pub fn settled(&self) -> Option<&GainState> {
        self.settled.as_ref()
    }

    /// A run is in progress.
    pub fn running(&self) -> bool {
        matches!(self.phase, Phase::Running { .. })
    }

    /// The caller's monotonic clock, seconds — how `min_rerun_s` is enforced. A caller that never
    /// sets it gets a controller whose clock stands still, which refuses every re-run: a stopped
    /// clock cannot authorise more device actions.
    pub fn set_now_s(&mut self, now_s: f64) {
        self.now_s = now_s;
    }

    /// Starts a run from the state the front end is in. `false` when the policy is disabled, a run
    /// is already in progress, or the last commit was too recent (`min_rerun_s`, backed off).
    pub fn trigger(&mut self, from: &GainState, why: GainTrigger) -> bool {
        if !self.policy.enabled || self.running() {
            return false;
        }
        if let Some(last) = self.last_commit_s
            && self.now_s - last < self.rerun_backoff_s
        {
            return false;
        }
        let start = self.ladder.realizable(from);
        self.visited.clear();
        self.records.clear();
        self.pruned = 0;
        self.trigger = why;
        self.started_from = Some(start.clone());
        self.phase = Phase::Running {
            pending: vec![(start, GainPhase::Start)],
            phase: GainPhase::Start,
        };
        true
    }

    /// What to do next. Idempotent while a probe is outstanding.
    pub fn next_step(&mut self) -> GainStep {
        if !self.policy.enabled {
            return GainStep::Disabled;
        }
        if let Some(p) = &self.outstanding {
            return GainStep::Probe(p.clone());
        }
        loop {
            let (popped, cur) = match &mut self.phase {
                Phase::Idle => return GainStep::Hold,
                Phase::Running { pending, phase } => {
                    if self.records.len() >= self.policy.max_probes {
                        pending.clear();
                    }
                    (pending.pop(), *phase)
                }
            };
            if let Some((state, ph)) = popped {
                if let Phase::Running { phase, .. } = &mut self.phase {
                    *phase = ph;
                }
                let probe = GainProbe {
                    state,
                    settle_s: self.policy.settle_s,
                    dwell_s: self.policy.dwell_s,
                    index: self.records.len(),
                    budget: self.policy.max_probes,
                    phase: ph,
                };
                self.outstanding = Some(probe.clone());
                return GainStep::Probe(probe);
            }
            match cur {
                // The coarse pass is planned once the *start* state has been measured, because its
                // direction is that measurement's answer.
                GainPhase::Start | GainPhase::Coarse => {
                    let plan = self.plan_coarse();
                    if plan.is_empty() {
                        self.plan_fine();
                    } else if let Phase::Running { pending, .. } = &mut self.phase {
                        *pending = plan;
                    }
                }
                GainPhase::Fine => return self.commit(),
            }
            if let Phase::Running { pending, .. } = &self.phase
                && pending.is_empty()
            {
                return self.commit();
            }
        }
    }

    /// Records what the outstanding probe measured. The quality's state is the **applied** one; it
    /// must answer the outstanding probe (docs/28 §3.5).
    pub fn observe(&mut self, quality: GainQuality) -> Result<(), GainError> {
        let Some(probe) = self.outstanding.take() else {
            return Err(GainError::NotProbing);
        };
        let applied = self.ladder.realizable(&quality.state);
        if !applied.same_as(&probe.state) && !quality.state.same_as(&probe.state) {
            let e = GainError::StateMismatch {
                expected: probe.state.to_string(),
                got: quality.state.to_string(),
            };
            self.outstanding = Some(probe);
            return Err(e);
        }
        let score = quality.score(&self.policy);
        let usable = !quality.unusable(&self.policy);
        // Upper-bound prune: above a state that is both grossly saturated and no better than the
        // best so far, more gain cannot help. Only ever prunes *higher* gain.
        if !usable && self.best().is_some_and(|b| score < b.score) {
            let over = applied.total_db();
            if let Phase::Running { pending, .. } = &mut self.phase {
                let before = pending.len();
                pending.retain(|(s, _)| s.total_db() < over + DB_EPS);
                self.pruned += before - pending.len();
            }
        }
        self.visited.push(applied.clone());
        self.records.push(GainProbeRecord {
            index: probe.index,
            phase: probe.phase,
            commanded: probe.state,
            applied,
            quality,
            score,
            usable,
        });
        Ok(())
    }

    /// The best record so far, grossly-saturated states included (used for the prune).
    fn best(&self) -> Option<&GainProbeRecord> {
        self.records.iter().max_by_key(|r| r.score)
    }

    fn seen(&self, s: &GainState) -> bool {
        self.visited.iter().any(|v| v.same_as(s))
            || match &self.phase {
                Phase::Running { pending, .. } => pending.iter().any(|(p, _)| p.same_as(s)),
                Phase::Idle => false,
            }
    }

    /// The coarse pass: rungs at `coarse_stride_db` in the direction the start measurement
    /// suggests, then the other way. Returned in **reverse** order because `next_step` pops from the
    /// end.
    fn plan_coarse(&self) -> Vec<(GainState, GainPhase)> {
        let Some(start) = self.started_from.clone() else {
            return Vec::new();
        };
        if self.records.iter().any(|r| r.phase == GainPhase::Coarse) {
            return Vec::new();
        }
        let clipping = self
            .records
            .first()
            .is_some_and(|r| r.quality.clip_fraction > self.policy.clip_significant);
        let primary = if clipping { -1.0 } else { 1.0 };
        let base = start.total_db();
        let mut out: Vec<(GainState, GainPhase)> = Vec::new();
        let budget = self
            .policy
            .coarse_probes
            .min(self.policy.max_probes.saturating_sub(self.records.len()));
        for dir in [primary, -primary] {
            let mut k = 1.0;
            while out.len() < budget {
                let want = base + dir * k * self.policy.coarse_stride_db;
                let lo = self.ladder.rungs().first().map_or(0.0, |r| r.total_db());
                let hi = self.ladder.rungs().last().map_or(0.0, |r| r.total_db());
                if want < lo - self.policy.coarse_stride_db
                    || want > hi + self.policy.coarse_stride_db
                {
                    break;
                }
                let rung = self.ladder.rungs()[self.ladder.nearest_to_db(want)].clone();
                if self.policy.allows(rung.total_db())
                    && !self.seen(&rung)
                    && !out.iter().any(|(s, _)| s.same_as(&rung))
                {
                    out.push((rung, GainPhase::Coarse));
                }
                k += 1.0;
            }
        }
        out.reverse();
        out
    }

    /// The fine pass: the finest stage stepped either side of the coarse best, alternating.
    fn plan_fine(&mut self) {
        let Some(best) = self.best().map(|r| r.applied.clone()) else {
            return;
        };
        let budget = self
            .policy
            .fine_probes
            .min(self.policy.max_probes.saturating_sub(self.records.len()));
        let mut out: Vec<(GainState, GainPhase)> = Vec::new();
        let mut cursor = [best.clone(), best.clone()];
        let mut live = [true, true];
        while out.len() < budget && (live[0] || live[1]) {
            for (i, dir) in [1i32, -1].into_iter().enumerate() {
                if !live[i] || out.len() >= budget {
                    continue;
                }
                match self.ladder.step_finest(&cursor[i], dir) {
                    Some(s) if self.policy.allows(s.total_db()) => {
                        cursor[i] = s.clone();
                        if !self.seen(&s) && !out.iter().any(|(o, _)| o.same_as(&s)) {
                            out.push((s, GainPhase::Fine));
                        }
                    }
                    _ => live[i] = false,
                }
            }
        }
        out.reverse();
        if let Phase::Running { pending, phase } = &mut self.phase {
            *pending = out;
            *phase = GainPhase::Fine;
        }
    }

    /// Picks the argmax, builds the report and settles.
    fn commit(&mut self) -> GainStep {
        let started_from = self
            .started_from
            .clone()
            .unwrap_or_else(|| GainState::new([]));
        let usable: Vec<&GainProbeRecord> = self.records.iter().filter(|r| r.usable).collect();
        let all_unusable = usable.is_empty() && !self.records.is_empty();
        let pick = if all_unusable {
            // Nowhere good to stand: leave the radio at the least saturated state measured, and
            // say so. Doing nothing would leave it grossly saturated.
            self.records
                .iter()
                .min_by(|a, b| {
                    a.quality
                        .clip_fraction
                        .total_cmp(&b.quality.clip_fraction)
                        .then(a.applied.total_db().total_cmp(&b.applied.total_db()))
                })
                .cloned()
        } else {
            usable.iter().max_by_key(|r| r.score).map(|r| (*r).clone())
        };
        let Some(pick) = pick else {
            // Nothing was measured at all (a run triggered and immediately capped).
            self.phase = Phase::Idle;
            self.settled = Some(started_from.clone());
            return GainStep::Hold;
        };
        let runner_up = self
            .records
            .iter()
            .filter(|r| !r.applied.same_as(&pick.applied) && r.usable)
            .max_by_key(|r| r.score)
            .map(|r| r.applied.clone());
        let start_q = self.records.first().cloned();
        let why = describe(&pick, start_q.as_ref(), runner_up.as_ref(), &self.records);
        let report = GainReport {
            trigger: self.trigger,
            started_from: started_from.clone(),
            committed: pick.applied.clone(),
            committed_score: pick.score,
            runner_up,
            probes: self.records.clone(),
            pruned: self.pruned,
            all_unusable,
            committed_overloaded: pick.quality.overload == Some(true)
                || pick.quality.clip_fraction > self.policy.clip_significant,
            why,
        };
        // A run that re-commits the state it started from has nothing to offer: back off, bounded,
        // so it stops asking. A run that moved the radio resets the interval.
        if pick.applied.same_as(&started_from) {
            self.rerun_backoff_s = (self.rerun_backoff_s * 2.0).min(self.policy.min_rerun_s * 32.0);
        } else {
            self.rerun_backoff_s = self.policy.min_rerun_s;
        }
        self.last_commit_s = Some(self.now_s);
        self.phase = Phase::Idle;
        self.settled = Some(pick.applied.clone());
        GainStep::Commit {
            state: pick.applied,
            report: Box::new(report),
        }
    }
}

/// The one-line "why", naming the term that decided.
fn describe(
    pick: &GainProbeRecord,
    start: Option<&GainProbeRecord>,
    runner_up: Option<&GainState>,
    all: &[GainProbeRecord],
) -> String {
    let metric = pick
        .quality
        .decode
        .as_ref()
        .map(|d| format!("{:.2} {}/s", d.rate_per_s, d.metric))
        .or_else(|| pick.quality.snr_db.map(|v| format!("{v:.1} dB SNR")))
        .unwrap_or_else(|| format!("clip {:.2e}", pick.quality.clip_fraction));
    let mut s = format!(
        "committed {}: {metric}, best of {} probed states",
        pick.applied,
        all.len()
    );
    if let Some(st) = start
        && !st.applied.same_as(&pick.applied)
    {
        let was = st
            .quality
            .decode
            .as_ref()
            .map(|d| format!("{:.2} {}/s", d.rate_per_s, d.metric))
            .or_else(|| st.quality.snr_db.map(|v| format!("{v:.1} dB SNR")))
            .unwrap_or_else(|| "nothing measurable".into());
        s.push_str(&format!(
            "; the state we started at ({}) gave {was} and pinned {:.1} % of components",
            st.applied,
            st.quality.clip_fraction * 100.0
        ));
    }
    if pick.quality.overload == Some(true) {
        s.push_str(
            "; it is flagged overload and was taken anyway because its output is better — the flag \
             is evidence, not a veto",
        );
    }
    if let Some(r) = runner_up {
        s.push_str(&format!("; runner-up {r}"));
    }
    if !pick.usable {
        s.push_str("; NO usable state was found, this is the least saturated one measured");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hackrf() -> SourceCapabilities {
        SourceCapabilities::hackrf_one()
    }

    fn state(lna: f64, vga: f64, amp: f64) -> GainState {
        GainState::new([
            NamedGain::new("lna", lna),
            NamedGain::new("vga", vga),
            NamedGain::new("amp", amp),
        ])
    }

    #[test]
    fn a_state_is_canonical_and_totals_its_stages() {
        let a = state(24.0, 20.0, 0.0);
        let b = GainState::new([
            NamedGain::new("vga", 20.0),
            NamedGain::new("amp", 0.0),
            NamedGain::new("lna", 24.0),
        ]);
        assert_eq!(a, b);
        assert!(a.same_as(&b));
        assert_eq!(a.total_db(), 44.0);
        assert_eq!(a.get("vga"), Some(20.0));
        assert_eq!(a.get("if"), None);
        assert_eq!(a.to_string(), "amp 0 / lna 24 / vga 20 (44 dB)");
    }

    #[test]
    fn the_ladder_is_monotone_covers_the_range_and_leaves_a_binary_stage_last() {
        let l = GainLadder::from_capabilities(&hackrf()).unwrap();
        let totals: Vec<f64> = l.rungs().iter().map(|r| r.total_db()).collect();
        assert!(
            totals.windows(2).all(|w| w[1] > w[0]),
            "not monotone: {totals:?}"
        );
        assert_eq!(totals.first(), Some(&0.0));
        assert_eq!(totals.last(), Some(&113.0), "{totals:?}");
        // One rung per available step: VGA 31 + LNA 5 + amp 1, plus the bottom.
        assert_eq!(l.rungs().len(), 38, "{totals:?}");
        // The amp is off on every rung but the last: a binary stage is the last resort.
        for r in &l.rungs()[..l.rungs().len() - 1] {
            assert_eq!(r.get("amp"), Some(0.0), "{r}");
        }
        assert_eq!(l.rungs().last().unwrap().get("amp"), Some(11.0));
        // Balanced: LNA and VGA move together rather than one saturating first.
        for r in l.rungs() {
            let (lna, vga) = (r.get("lna").unwrap() / 40.0, r.get("vga").unwrap() / 62.0);
            assert!((lna - vga).abs() <= 0.25, "unbalanced rung {r}");
        }
    }

    #[test]
    fn the_ladder_of_a_single_continuous_stage_still_steps() {
        // The RTL declares one stage continuous because `GainStage` cannot express its 29 uneven
        // steps; the ladder must still be finite and monotone.
        let l = GainLadder::from_capabilities(&SourceCapabilities::rtl_sdr_r820t()).unwrap();
        let totals: Vec<f64> = l.rungs().iter().map(|r| r.total_db()).collect();
        assert!(totals.windows(2).all(|w| w[1] > w[0]), "{totals:?}");
        assert_eq!(l.rungs().len(), 33, "{totals:?}");
        assert_eq!(totals.last(), Some(&49.6));
    }

    #[test]
    fn a_device_with_no_gain_stage_has_no_gain_to_manage() {
        let mut caps = hackrf();
        caps.gain_stages.clear();
        assert_eq!(
            GainLadder::from_capabilities(&caps).unwrap_err(),
            GainError::NoStages {
                driver: "hackrf-one".into()
            }
        );
    }

    #[test]
    fn realizable_quantises_onto_the_device_grid() {
        let l = GainLadder::from_capabilities(&hackrf()).unwrap();
        // LNA is 8 dB steps, VGA 2, amp on/off from its midpoint up.
        let r = l.realizable(&state(23.0, 21.0, 7.0));
        assert_eq!(r, state(16.0, 20.0, 11.0));
        // An unknown stage is dropped and a missing one filled from the bottom.
        let r = l.realizable(&GainState::new([NamedGain::new("rf", 30.0)]));
        assert_eq!(r, state(0.0, 0.0, 0.0));
    }

    #[test]
    fn the_fine_step_walks_the_finest_stage_not_the_ladder() {
        let l = GainLadder::from_capabilities(&hackrf()).unwrap();
        let up = l.step_finest(&state(16.0, 26.0, 0.0), 1).unwrap();
        assert_eq!(up, state(16.0, 28.0, 0.0), "the 2 dB VGA, not the 8 dB LNA");
        let down = l.step_finest(&state(16.0, 26.0, 0.0), -1).unwrap();
        assert_eq!(down, state(16.0, 24.0, 0.0));
        // At a stage's limit the next-finest stage carries the step: the 8 dB LNA before the
        // 11 dB amp, and the amp only when nothing graded can move.
        let up = l.step_finest(&state(16.0, 62.0, 0.0), 1).unwrap();
        assert_eq!(up, state(24.0, 62.0, 0.0));
        let up = l.step_finest(&state(40.0, 62.0, 0.0), 1).unwrap();
        assert_eq!(up, state(40.0, 62.0, 11.0));
        assert_eq!(l.step_finest(&state(40.0, 62.0, 11.0), 1), None);
        assert_eq!(l.step_finest(&state(0.0, 0.0, 0.0), -1), None);
    }

    /// The whole point of the ticket, as an order over measurements.
    #[test]
    fn the_decode_decides_and_the_overload_flag_does_not_veto_it() {
        let p = GainPolicy::default();
        // The explorer's own numbers, 2026-09-25, 98.9 MHz (docs/28 §1): LNA 32 / VGA 30 / amp on
        // was flagged overload and decoded 106 RDS groups in ~10 s; the "safe" retry at
        // LNA 24 / VGA 24 / amp off was clean and decoded none.
        let flagged = GainQuality::new(state(32.0, 30.0, 11.0), 10.0)
            .with_clip(2e-3, Some(true))
            .with_decode(Some(DecodeQuality::from_count("rds-groups", 106, 10.0)));
        let clean = GainQuality::new(state(24.0, 24.0, 0.0), 10.0)
            .with_clip(0.0, Some(false))
            .with_decode(Some(DecodeQuality::from_count("rds-groups", 0, 10.0)));
        assert!(
            flagged.score(&p) > clean.score(&p),
            "the flagged state decoded 106 groups and the clean one none: {:?} vs {:?}",
            flagged.score(&p),
            clean.score(&p)
        );
        assert!(
            !flagged.unusable(&p),
            "2e-3 clipped is not gross saturation"
        );
    }

    #[test]
    fn clip_fraction_only_decides_between_states_whose_output_is_indistinguishable() {
        let p = GainPolicy::default();
        let q = |clip: f64, groups: u64, gain: f64| {
            GainQuality::new(state(gain, 0.0, 0.0), 1.0)
                .with_clip(clip, Some(clip > 0.0))
                .with_decode(Some(DecodeQuality::from_count("rds-groups", groups, 1.0)))
        };
        // Same decode: the cleaner state wins.
        assert!(q(0.0, 8, 24.0).score(&p) > q(1e-2, 8, 24.0).score(&p));
        // Within a decade of clip, it is a tie and the lower gain wins.
        assert!(q(1.1e-3, 8, 16.0).score(&p) > q(1.9e-3, 8, 24.0).score(&p));
        // A better decode beats a cleaner state, however many decades cleaner.
        assert!(q(1e-2, 9, 24.0).score(&p) > q(0.0, 8, 24.0).score(&p));
    }

    #[test]
    fn snr_guides_where_nothing_decodes_and_an_unmeasured_term_never_wins() {
        let p = GainPolicy::default();
        let dead = |snr: Option<f64>| {
            GainQuality::new(state(16.0, 0.0, 0.0), 1.0)
                .with_snr(snr)
                .with_decode(Some(DecodeQuality::from_count("rds-groups", 0, 1.0)))
        };
        assert!(dead(Some(14.8)).score(&p) > dead(Some(10.6)).score(&p));
        assert!(
            dead(Some(-3.0)).score(&p) > dead(None).score(&p),
            "a measured SNR beats an unmeasured one: nothing said is not a value"
        );
        // And an unmeasured decode never beats a measured zero.
        let unmeasured = GainQuality::new(state(16.0, 0.0, 0.0), 1.0).with_snr(Some(30.0));
        assert!(dead(Some(0.0)).score(&p) > unmeasured.score(&p));
    }

    #[test]
    fn a_disabled_policy_is_the_default_and_never_moves_the_radio() {
        let mut c = GainController::new(&hackrf(), GainPolicy::default()).unwrap();
        assert!(!GainPolicy::default().enabled);
        assert!(!c.trigger(&state(32.0, 30.0, 11.0), GainTrigger::Overload));
        assert_eq!(c.next_step(), GainStep::Disabled);
        assert!(!c.running());
    }

    /// The shape docs/28 §2 measured on the mock SDR, as a function of total gain: dead at the
    /// bottom (the emission under the LSB and the receiver's own noise), a window in the middle,
    /// dead at the top (ADC saturation and its intermodulation). Returns clip fraction, channel
    /// SNR and CRC-valid groups over a one-second dwell.
    fn scene(total_db: f64) -> (f64, Option<f64>, u64) {
        let clip = if total_db < 46.0 {
            0.0
        } else {
            (1e-3 * 10f64.powf((total_db - 50.0) / 4.0)).min(0.95)
        };
        // A channel measurement refuses to report SNR through heavy clipping, as the real one does.
        let snr = if total_db < 20.0 || clip > 5e-3 {
            None
        } else {
            Some(total_db - 26.0 - 60.0 * clip)
        };
        let groups = match snr {
            // Distortion products land in the channel well before the flag looks alarming.
            Some(s) if s > 12.0 && clip <= 3e-3 => ((s - 12.0) * 1.5) as u64,
            _ => 0,
        };
        (clip, snr, groups)
    }

    fn run(policy: GainPolicy, from: GainState) -> (GainState, GainReport, Vec<GainState>) {
        let mut c = GainController::new(&hackrf(), policy).unwrap();
        c.set_now_s(1000.0);
        assert!(c.trigger(&from, GainTrigger::Overload));
        let mut commanded = Vec::new();
        loop {
            match c.next_step() {
                GainStep::Probe(p) => {
                    commanded.push(p.state.clone());
                    let (clip, snr, groups) = scene(p.state.total_db());
                    let q = GainQuality::new(p.state.clone(), p.dwell_s)
                        .with_clip(clip, Some(clip > 1e-4))
                        .with_snr(snr)
                        .with_decode(Some(DecodeQuality::from_count(
                            "rds-groups",
                            groups,
                            p.dwell_s,
                        )));
                    c.observe(q).unwrap();
                }
                GainStep::Commit { state, report } => return (state, *report, commanded),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn an_overloaded_scene_converges_on_the_state_that_decodes() {
        let (best, report, commanded) = run(
            GainPolicy::default().enabled(),
            state(32.0, 30.0, 11.0), // 73 dB: the explorer's state, 86 % pinned
        );
        let (clip, _, groups) = scene(best.total_db());
        assert!(groups > 0, "committed {best} decodes nothing ({report:?})");
        assert!(clip < 0.05, "committed {best} is saturated");
        assert!(
            best.total_db() < 73.0,
            "the radio never came down: {}",
            report.explain()
        );
        // Bounded, and no state probed twice.
        assert!(commanded.len() <= report.probes.len().max(12));
        for (i, a) in commanded.iter().enumerate() {
            for b in &commanded[i + 1..] {
                assert!(!a.same_as(b), "probed {a} twice:\n{}", report.explain());
            }
        }
        assert!(
            report.why.contains("rds-groups"),
            "the report must name what decided: {}",
            report.why
        );
        assert!(report.explain().lines().count() >= 3);
    }

    #[test]
    fn a_quiet_scene_climbs_instead_of_backing_off() {
        let (best, report, _) = run(GainPolicy::default().enabled(), state(0.0, 0.0, 0.0));
        assert!(
            best.total_db() > 20.0,
            "a clip-free, signal-free start must climb: {}",
            report.explain()
        );
        assert!(scene(best.total_db()).2 > 0, "{}", report.explain());
    }

    #[test]
    fn the_run_holds_after_committing_and_refuses_an_immediate_rerun() {
        let mut c = GainController::new(&hackrf(), GainPolicy::default().enabled()).unwrap();
        c.set_now_s(100.0);
        assert!(c.trigger(&state(32.0, 30.0, 11.0), GainTrigger::Overload));
        let settled = loop {
            match c.next_step() {
                GainStep::Probe(p) => {
                    let (clip, snr, groups) = scene(p.state.total_db());
                    c.observe(
                        GainQuality::new(p.state.clone(), p.dwell_s)
                            .with_clip(clip, Some(clip > 1e-4))
                            .with_snr(snr)
                            .with_decode(Some(DecodeQuality::from_count(
                                "rds-groups",
                                groups,
                                p.dwell_s,
                            ))),
                    )
                    .unwrap();
                }
                GainStep::Commit { state, .. } => break state,
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(c.settled(), Some(&settled));
        assert_eq!(
            c.next_step(),
            GainStep::Hold,
            "a settled run issues no action"
        );
        assert!(
            !c.trigger(&settled, GainTrigger::Overload),
            "a re-run inside min_rerun_s must be refused"
        );
        c.set_now_s(100.0 + GainPolicy::default().min_rerun_s + 0.1);
        assert!(c.trigger(&settled, GainTrigger::QualityLoss));
    }

    #[test]
    fn a_re_run_that_changes_nothing_backs_off_so_it_stops_asking() {
        let policy = GainPolicy::default().enabled();
        let mut c = GainController::new(&hackrf(), policy.clone()).unwrap();
        let settle = |c: &mut GainController, from: &GainState| -> GainState {
            assert!(c.trigger(from, GainTrigger::PeriodicReview));
            loop {
                match c.next_step() {
                    GainStep::Probe(p) => {
                        let (clip, snr, groups) = scene(p.state.total_db());
                        c.observe(
                            GainQuality::new(p.state.clone(), p.dwell_s)
                                .with_clip(clip, Some(clip > 1e-4))
                                .with_snr(snr)
                                .with_decode(Some(DecodeQuality::from_count(
                                    "rds-groups",
                                    groups,
                                    p.dwell_s,
                                ))),
                        )
                        .unwrap();
                    }
                    GainStep::Commit { state, .. } => return state,
                    other => panic!("unexpected {other:?}"),
                }
            }
        };
        c.set_now_s(0.0);
        let s1 = settle(&mut c, &state(32.0, 30.0, 11.0));
        assert!(!s1.same_as(&state(32.0, 30.0, 11.0)), "the radio moved");
        // It moved, so the next run waits the base interval.
        c.set_now_s(policy.min_rerun_s - 0.1);
        assert!(!c.trigger(&s1, GainTrigger::PeriodicReview));
        c.set_now_s(policy.min_rerun_s + 0.1);
        let s2 = settle(&mut c, &s1);
        assert!(s2.same_as(&s1), "{s2} vs {s1}: nothing better exists");
        // That run re-committed its own start state, so the next one waits twice as long.
        c.set_now_s(policy.min_rerun_s * 2.0 + 0.2);
        assert!(
            !c.trigger(&s2, GainTrigger::PeriodicReview),
            "an unproductive run must back off rather than keep spending device time"
        );
        c.set_now_s(policy.min_rerun_s * 3.2);
        assert!(c.trigger(&s2, GainTrigger::PeriodicReview));
    }

    #[test]
    fn a_measurement_that_cannot_name_its_state_is_refused() {
        let mut c = GainController::new(&hackrf(), GainPolicy::default().enabled()).unwrap();
        assert_eq!(
            c.observe(GainQuality::new(state(0.0, 0.0, 0.0), 1.0))
                .unwrap_err(),
            GainError::NotProbing
        );
        assert!(c.trigger(&state(24.0, 20.0, 0.0), GainTrigger::Manual));
        let GainStep::Probe(p) = c.next_step() else {
            panic!()
        };
        let wrong = state(0.0, 62.0, 0.0);
        assert!(!wrong.same_as(&p.state));
        let err = c
            .observe(GainQuality::new(wrong, p.dwell_s))
            .expect_err("a dwell from another state is not evidence");
        assert!(matches!(err, GainError::StateMismatch { .. }), "{err}");
        // The probe is still outstanding: the run is not corrupted by the bad answer.
        assert_eq!(c.next_step(), GainStep::Probe(p.clone()));
        c.observe(GainQuality::new(p.state, p.dwell_s)).unwrap();
    }

    #[test]
    fn a_probe_budget_is_never_exceeded() {
        let mut policy = GainPolicy::default().enabled();
        policy.max_probes = 4;
        let (_, report, commanded) = run(policy, state(32.0, 30.0, 11.0));
        assert_eq!(commanded.len(), 4, "{}", report.explain());
        assert_eq!(report.probes.len(), 4);
    }

    #[test]
    fn a_scene_with_nowhere_good_to_stand_commits_the_least_saturated_and_says_so() {
        // Everything measured is grossly saturated (a scene 40 dB too hot for the ladder).
        let mut c = GainController::new(&hackrf(), GainPolicy::default().enabled()).unwrap();
        assert!(c.trigger(&state(40.0, 62.0, 11.0), GainTrigger::Overload));
        let report = loop {
            match c.next_step() {
                GainStep::Probe(p) => {
                    let clip = (0.9 - p.state.total_db() / 400.0).max(0.2);
                    c.observe(
                        GainQuality::new(p.state.clone(), p.dwell_s)
                            .with_clip(clip, Some(true))
                            .with_decode(Some(DecodeQuality::from_count("rds-groups", 0, 1.0))),
                    )
                    .unwrap();
                }
                GainStep::Commit { report, .. } => break *report,
                other => panic!("unexpected {other:?}"),
            }
        };
        assert!(report.all_unusable);
        assert!(report.why.contains("NO usable state"), "{}", report.why);
        // The least saturated of the states it measured, not the one it started at.
        let least = report
            .probes
            .iter()
            .min_by(|a, b| {
                a.quality
                    .clip_fraction
                    .total_cmp(&b.quality.clip_fraction)
                    .then(a.applied.total_db().total_cmp(&b.applied.total_db()))
            })
            .unwrap();
        assert!(report.committed.same_as(&least.applied));
    }

    #[test]
    fn operator_clamps_bound_every_probe() {
        let mut policy = GainPolicy::default().enabled();
        policy.min_total_gain_db = Some(30.0);
        policy.max_total_gain_db = Some(50.0);
        let (best, report, commanded) = run(policy, state(32.0, 30.0, 11.0));
        for s in &commanded[1..] {
            assert!(
                (30.0..=50.0).contains(&s.total_db()),
                "probed {s} outside the clamp:\n{}",
                report.explain()
            );
        }
        assert!((30.0..=50.0).contains(&best.total_db()));
    }
}
