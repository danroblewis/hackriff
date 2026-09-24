//! `EvidenceObjective` (ADR-0015 §2.3; T-858 = MAUTO M-7): T-070's refinement loop, driven by the
//! synthesis evidence ladder.
//!
//! [`EvidenceObjective`] implements `hk_demod::refine::Objective` over a recipe prefix — a beam
//! node's candidate ([`EvidenceObjective::for_candidate`]) or a running pipeline's recipe whose
//! `refine.objective` is `{"evidence": "deepest"}` (recipe schema 3,
//! [`EvidenceObjective::from_recipe`]) — so `RefinementLoop`, `Termination` and hysteresis are
//! reused unchanged. The WFM objective stays the specialised S1 evidence for broadcast FM.
//!
//! # The mapping (ADR-0015 §2.3)
//!
//! - **`space()`**: the channel centre and bandwidth become the `ParameterSpace` centre and
//!   bandwidth axes. The centre is a grid over the start box; the bandwidth is the width
//!   (`2 × cutoff_hz`) of the prefix's **S0 channel filter** (its first `lowpass` node), searched
//!   around the prefix's own width — see "The channel". Each tuned continuous node parameter
//!   (deviation, symbol rate, loop bandwidth…) becomes a `Tuning.mode` axis **named by its path**,
//!   `nodes[<id>].params.<name>`, with a grid around its current value
//!   ([`ParamAxis::from_free`], [`ParamAxis::around`]).
//! - **`evaluate(window, tuning, depth)`**: down-convert the window to the tuning's centre at
//!   the prefix's own input rate, bind the bandwidth and mode values into the prefix, run it with
//!   `hk_blocks::run_window`, score it ([`crate::score`]) and return `Measurement { quality:
//!   evidence_bits, locked: deepest b_k ≥ floor_k }`.
//! - **`EvalDepth`** maps onto the short, search and hold-out windows (§3.1 step 1): `Acquire`
//!   reads the short leading part of the search window and `Track` all of it (both only samples
//!   before [`WindowSplit::holdout_from`]; [`EvidenceObjective::loop_config`] sets the lengths);
//!   `Validate` reads **only the hold-out** (samples from it on). The split is enforced here, so
//!   no configuration can let a search-window measurement vouch for the result.
//!   `RefinementOutcome::locked` therefore means "locked on hold-out" (§3.1 step 7).
//!
//! # The channel (proposed ADR-0015 §2.3 amendment, pending the user)
//!
//! Every calibration null (§2.2, `crate::nullchain`) is **white noise at the prefix's input
//! rate** fed to the prefix's own S0 filter, and the tables are tight: the `lowpass@1` S0 `snr`
//! table credits 6 bits at +0.1 dB (n = 16 384). Measured in T-858: a channeliser whose passband
//! rolls off at the band edges made pure noise score 6 bits at S0, and one narrower than the S0
//! filter's support coloured the noise the S1/S2 blocks saw, so noise locked at S2 on hold-out.
//! So, as a **proposal recorded in ADR-0015 §2.3 for the user to accept or reject**:
//!
//! - the channeliser is **flat and fixed**: [`ChannelSearch::occupancy`] of the prefix rate
//!   (0.9: ±21.6 kHz at 48 kHz); the bandwidth axis moves the S0 filter behind it, kept so its
//!   support (width plus transitions) stays inside that flat passband;
//! - **not implemented, for the user to decide:** leaving S0 out of `quality` and refusing
//!   S0-only prefixes (its `snr` is a whiteness test of the input the objective itself
//!   produced). The code follows the accepted text: S0 is counted, and an S0-only prefix locks
//!   on S0's floor — which a flat-noise window can clear through the channeliser's band-edge
//!   roll-off. Deeper prefixes lock on their deepest stage and are unaffected.
//!
//! # What a measurement is
//!
//! - **`quality`** = `Σ_j min(b_j, cap_j)` over S0 … the prefix's deepest stage (§1.3):
//!   `evidence_bits` **before** the look-elsewhere charge `L_j`. Every tuning the loop compares
//!   is the same hypothesis, so `L_j` is common to all of them and cannot change a comparison;
//!   charging it for the extra measurements a refinement makes is the engine's job, from
//!   `RefinementOutcome::evaluations`, when it records the refined child.
//! - **`locked`** = the lock stage's `b_k` reaches its floor (`stage::default_floor_bits`). The
//!   lock stage is the **deepest stage the prefix reaches** ([`EvidenceObjective::target`]) —
//!   or, for S6, which has no floor (§16.2 C7: S6 ranks only), S5. It is fixed when the
//!   objective is built, never read off a measurement: a shallow measurement (a tuning where
//!   frames never reached the check block) must not be judged against a shallower stage's floor
//!   and lock on weaker evidence (the T-188 lesson). The floor is the beam's **pruning** floor,
//!   so a noise window clears it now and then (S2's two groups of up to 6 bits each clear 6 on
//!   noise a few per cent of the time); that is why only the hold-out vouches.
//! - **No centre estimator**: the evidence ladder does not say which way the emission lies, so
//!   the loop runs its three-point local search on quality.
//! - **Saturation.** Calibrated metrics (S1–S3) are capped at 6 bits each
//!   (`stage::CALIBRATED_CLAIM_CAP_BITS`): a strong emission saturates them over a wide range of
//!   tunings — for unshaped 2-FSK, as soon as one tone passes the S0 filter — and ties keep the
//!   tuning nearest the start. A prefix refined at S2 therefore finds *the emission*, not its
//!   centre to better than the S0 filter's support; the analytic stages (S4/S5), whose bits grow
//!   with the evidence, are what rank finer tunings.
//!
//! # Support alignment
//!
//! Calibrated metrics (S0–S3) score only at the supports their table enumerates
//! (`calibration::support_matches`: a window of `n` in `[cell_n, cell_n + cell_n/50 + 16]`),
//! and a block reports `n` over everything it saw, so a window of arbitrary length scores
//! `NoTable` — 0 bits — at every calibrated stage. And one window cannot land every block on a
//! support at once: `fsk_demod` counts samples, `clock_recovery` symbols. So each measurement
//! runs the prefix once over its whole slice (the analytic stages S4–S6 keep those records: their
//! nulls are closed-form at any `n`), then, **per calibrated block** whose support missed, re-runs
//! it over the **leading** part of the same slice sized to land that block on the largest
//! calibrated support not above what it saw (one linear step and one correction), and takes that
//! block's records from that run. Every record is thus scored against a table for exactly the
//! statistic it measured, on a subset of the data the depth allows — fewer bits than the whole
//! slice might hold, never more. A block that cannot land (its slice is shorter than the smallest
//! support) keeps its `NoTable` 0 bits. Cost: at most `1 + 2 × calibrated blocks` runs per
//! measurement, reported as the `support_runs` mode parameter.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use hk_blocks::{PortSlice, Registry, WindowError};
use hk_demod::DemodError;
use hk_demod::refine::{
    EvalDepth, IqWindow, LoopConfig, Measurement, ModeAxis, Objective, ParameterSpace, RefineStart,
    Termination, Tuning, WindowPlan,
};
use hk_dsp::{Ddc, DdcSpec, IqSample};
use hk_recipe::{EvidenceTarget, ObjectiveForm, PortType, Recipe, parse_param_path};
use num_complex::Complex32;
use serde_json::Value;

use crate::calibration::{Fill, NullKind, Score, support_matches};
use crate::candidate::{Candidate, Domain, FreeParam};
use crate::score::{CalibrationSet, StageLadder, evaluate_window};
use crate::stage::{Stage, default_floor_bits};

/// Objective name and version, as `RefinementOutcome::objective` records it. Bumped when what
/// `quality` or `locked` means changes.
pub const EVIDENCE_OBJECTIVE: &str = "hk-synth/evidence@1";

/// The search share of an analysis window (ADR-0015 §3.1 step 1: "the first 60 %").
pub const SEARCH_FRACTION: f64 = 0.6;

/// Items per `run_window` chunk (as the calibration generator runs its null chains).
pub const CHUNK_ITEMS: usize = 4096;

/// Where an analysis window splits into search and hold-out, in samples of the window the loop
/// is given (every `IqWindow` the loop passes is a leading slice of that window, so an index
/// means the same sample at every depth).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowSplit {
    /// The first hold-out sample. Samples before it are the search window.
    pub holdout_from: usize,
}

impl WindowSplit {
    /// The split at `holdout_from`.
    pub const fn at(holdout_from: usize) -> Self {
        Self { holdout_from }
    }

    /// §3.1's split of a `total`-sample window: the first [`SEARCH_FRACTION`] searches.
    pub fn of_window(total: usize) -> Self {
        Self::at((total as f64 * SEARCH_FRACTION).floor() as usize)
    }
}

/// What every evidence objective reads besides the prefix.
#[derive(Clone)]
pub struct EvidenceContext {
    /// The block catalogue the prefix runs on.
    pub registry: Arc<Registry>,
    /// The calibration tables calibrated metrics are scored against.
    pub calibration: Arc<CalibrationSet>,
    /// The analysis window's recorded ADC fill (§13.3). Unknown σ is under-filled: every
    /// calibrated metric then scores 0 bits.
    pub fill: Fill,
    /// The analysis window's search / hold-out split.
    pub split: WindowSplit,
}

/// How far the channel may move from the start box, and how wide the objective's own
/// channeliser is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelSearch {
    /// The centre may move this far either side, × start bandwidth (½: anywhere in the box).
    pub center_span: f64,
    /// Centre grid step, × start bandwidth.
    pub center_step: f64,
    /// The channeliser's flat passband as a share of the prefix's input rate. It must cover the
    /// prefix's own S0 filter (passband and transition) so that filter sees the white noise its
    /// calibration null was drawn from (module docs, "The channel").
    pub occupancy: f64,
    /// Narrowest channel-filter width tried, × the prefix's own filter width.
    pub bandwidth_min: f64,
    /// Widest channel-filter width tried, × the prefix's own filter width (never so wide that the
    /// filter's support leaves the flat passband).
    pub bandwidth_max: f64,
    /// Channel-filter width step, × the prefix's own filter width.
    pub bandwidth_step: f64,
    /// The chosen width is the narrowest within this many bits of the best.
    pub bandwidth_tolerance_bits: f64,
}

impl Default for ChannelSearch {
    fn default() -> Self {
        Self {
            center_span: 0.5,
            center_step: 0.125,
            occupancy: 0.9,
            bandwidth_min: 0.5,
            bandwidth_max: 1.5,
            bandwidth_step: 0.125,
            bandwidth_tolerance_bits: 1.0,
        }
    }
}

/// One tuned node parameter: a `Tuning.mode` axis named by its path.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamAxis {
    /// `nodes[<id>].params.<name>`.
    pub path: String,
    /// Candidate values; the first is the current one (the loop's default).
    pub values: Vec<f64>,
    /// An integer parameter: values are bound rounded.
    pub integer: bool,
}

impl ParamAxis {
    /// The axis for a continuous free parameter (`float`/`int` domain) at its `current` value,
    /// on the engine's coordinate grid (§3.1 step 3: log scale ×{½, 1, 2} ± 3 steps of 1 %;
    /// linear ± 3 steps over the range). `None` for a discrete domain — those are branched by
    /// the beam, never refined.
    pub fn from_free(free: &FreeParam, current: Option<&Value>) -> Option<Self> {
        let seed = current.or(free.seed.as_ref());
        let (values, _) = crate::engine::continuous_grid(&free.domain, seed)?;
        let values: Vec<f64> = values.iter().filter_map(Value::as_f64).collect();
        (!values.is_empty()).then(|| Self {
            path: free.path.clone(),
            values,
            integer: matches!(free.domain, Domain::Int(_)),
        })
    }

    /// The axis a running pipeline tunes a parameter on (schema-3 `refine.tune` paths): ± 3
    /// steps of 1 % around `current`, the current value first. No ×½ / ×2 branches: a running
    /// pipeline tracks its parameter, it does not re-search its structure.
    pub fn around(path: &str, current: f64, integer: bool) -> Self {
        let mut values = vec![current];
        for k in [-3i32, -2, -1, 1, 2, 3] {
            let v = current * (1.0 + 0.01 * f64::from(k));
            let v = if integer { v.round() } else { v };
            if !values
                .iter()
                .any(|&q| (q - v).abs() <= 1e-9 * v.abs().max(1.0))
            {
                values.push(v);
            }
        }
        Self {
            path: path.to_owned(),
            values,
            integer,
        }
    }
}

/// Why an evidence objective could not be built. Messages never echo signal content.
#[derive(Clone, Debug, PartialEq)]
pub enum ObjectiveError {
    /// The prefix does not read IQ at a declared `input.sample_rate_hz`.
    NotIq,
    /// The recipe's `refine.objective` is not `{"evidence": …}`.
    NotEvidence,
    /// A tuned parameter path is malformed, names no node, or its value is not a number.
    BadParam(String),
}

impl std::fmt::Display for ObjectiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObjectiveError::NotIq => {
                write!(
                    f,
                    "the prefix must read iq at a declared input.sample_rate_hz"
                )
            }
            ObjectiveError::NotEvidence => {
                write!(
                    f,
                    "the recipe's refine.objective is not {{\"evidence\": …}}"
                )
            }
            ObjectiveError::BadParam(p) => {
                write!(f, "tuned parameter {p}: no such numeric node parameter")
            }
        }
    }
}

impl std::error::Error for ObjectiveError {}

/// T-070's objective over the evidence ladder (see the module docs).
pub struct EvidenceObjective {
    mode: String,
    recipe: Recipe,
    rate_hz: f64,
    ctx: EvidenceContext,
    target: Stage,
    axes: Vec<ParamAxis>,
    tune_center: bool,
    tune_bandwidth: bool,
    /// The prefix's S0 channel filter (a `lowpass` node): `(node id, cutoff_hz, transition_hz)`.
    /// The bandwidth axis is its two-sided width, `2 × cutoff_hz`.
    channel_filter: Option<(String, f64, f64)>,
    channel: ChannelSearch,
}

impl EvidenceObjective {
    /// The objective for a beam node (§3.1 step 6): `candidate`'s prefix, locking on its deepest
    /// fixed stage, with a mode axis for every continuous parameter in `free` — the root's
    /// declared parameters, with domains — that the prefix has already bound. Centre and
    /// bandwidth are both tuned.
    pub fn for_candidate(
        candidate: &Candidate,
        free: &[FreeParam],
        ctx: EvidenceContext,
    ) -> Result<Self, ObjectiveError> {
        let mut axes = Vec::new();
        for f in free {
            let Some((id, name)) = parse_param_path(&f.path) else {
                continue;
            };
            let Some(node) = candidate.recipe.nodes.iter().find(|n| n.id == id) else {
                continue; // a later stage's parameter: not bound yet, nothing to refine
            };
            if let Some(axis) = ParamAxis::from_free(f, node.params.get(name)) {
                axes.push(axis);
            }
        }
        let target = candidate.deepest_choice().unwrap_or(Stage::S0);
        Self::build(
            candidate.skeleton.clone(),
            candidate.recipe.clone(),
            target,
            axes,
            (true, true),
            ctx,
        )
    }

    /// The objective a running synthesized pipeline keeps tuning with (§2.3): `recipe`'s
    /// `refine` must be `{"evidence": "deepest"}` (schema 3). "Deepest" is `reached`, the stage
    /// the synthesis result reached (`PipelineResult::stage_reached`); the refinement tunes only
    /// what `refine.tune` lists — `center_hz`, `bandwidth_hz`, and node parameters by path
    /// ([`ParamAxis::around`] their current values).
    pub fn from_recipe(
        recipe: &Recipe,
        reached: Stage,
        ctx: EvidenceContext,
    ) -> Result<Self, ObjectiveError> {
        let refine = recipe.refine.as_ref().ok_or(ObjectiveError::NotEvidence)?;
        let Some(ObjectiveForm::Evidence(EvidenceTarget::Deepest)) = refine.objective.form() else {
            return Err(ObjectiveError::NotEvidence);
        };
        let mut axes = Vec::new();
        for t in &refine.tune {
            if t == "center_hz" || t == "bandwidth_hz" {
                continue;
            }
            let bad = || ObjectiveError::BadParam(t.clone());
            let (id, name) = parse_param_path(t).ok_or_else(bad)?;
            let v = recipe
                .nodes
                .iter()
                .find(|n| n.id == id)
                .and_then(|n| n.params.get(name))
                .ok_or_else(bad)?;
            let current = v.as_f64().filter(|x| x.is_finite()).ok_or_else(bad)?;
            axes.push(ParamAxis::around(t, current, v.is_i64() || v.is_u64()));
        }
        Self::build(
            recipe.id.clone(),
            recipe.clone(),
            reached,
            axes,
            (
                refine.tune.iter().any(|t| t == "center_hz"),
                refine.tune.iter().any(|t| t == "bandwidth_hz"),
            ),
            ctx,
        )
    }

    fn build(
        mode: String,
        recipe: Recipe,
        target: Stage,
        axes: Vec<ParamAxis>,
        (tune_center, tune_bandwidth): (bool, bool),
        ctx: EvidenceContext,
    ) -> Result<Self, ObjectiveError> {
        let rate_hz = recipe
            .input
            .sample_rate_hz
            .filter(|r| r.is_finite() && *r > 0.0)
            .ok_or(ObjectiveError::NotIq)?;
        if recipe.input.port != PortType::Iq {
            return Err(ObjectiveError::NotIq);
        }
        // The S0 channel filter: the first `lowpass` node with a numeric cutoff. A node-parameter
        // axis on its cutoff already tunes it; the bandwidth axis then stays fixed.
        let channel_filter = recipe
            .nodes
            .iter()
            .find(|n| n.block == "lowpass")
            .and_then(|n| {
                let cutoff = n.params.get("cutoff_hz")?.as_f64()?;
                let transition = n
                    .params
                    .get("transition_hz")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                (cutoff.is_finite() && cutoff > 0.0).then(|| (n.id.clone(), cutoff, transition))
            })
            .filter(|(id, _, _)| {
                let path = format!("nodes[{id}].params.cutoff_hz");
                !axes.iter().any(|a| a.path == path)
            });
        Ok(Self {
            mode,
            recipe,
            rate_hz,
            ctx,
            target,
            axes,
            tune_center,
            tune_bandwidth,
            channel_filter,
            channel: ChannelSearch::default(),
        })
    }

    /// With other channel-search bounds.
    pub fn with_channel_search(mut self, channel: ChannelSearch) -> Self {
        self.channel = channel;
        self
    }

    /// The deepest stage the prefix reaches: what "deepest" locks on.
    pub fn target(&self) -> Stage {
        self.target
    }

    /// The stage whose floor decides the lock: [`Self::target`], or the deepest stage above it
    /// that has a floor (S6 has none: it ranks only, §16.2 C7).
    pub fn lock_stage(&self) -> Stage {
        Stage::ALL
            .iter()
            .rev()
            .copied()
            .filter(|&s| s <= self.target)
            .find(|&s| default_floor_bits(s).is_some())
            .unwrap_or(Stage::S0)
    }

    /// The channeliser's flat passband for a window at `rate_hz`, Hz.
    pub fn channel_width(&self, rate_hz: f64) -> f64 {
        self.channel.occupancy * self.rate_hz.min(rate_hz)
    }

    /// The tuned node parameters.
    pub fn axes(&self) -> &[ParamAxis] {
        &self.axes
    }

    /// A loop configuration whose depths read the split the way §3.1 means, for an analysis
    /// window of `total` samples at `rate_hz`: `Acquire` the first half of the search window,
    /// `Track` all of it, `Validate` the whole window (of which this objective reads only the
    /// hold-out). Termination is the loop's default except the measurement budget.
    pub fn loop_config(&self, total: usize, rate_hz: f64, max_evaluations: u32) -> LoopConfig {
        let search_s = self.ctx.split.holdout_from.min(total) as f64 / rate_hz;
        LoopConfig {
            termination: Termination {
                max_evaluations,
                // A backstop only: the evaluation count is the budget (docs/27, T-552).
                time_budget: Duration::from_secs(120),
                ..Termination::default()
            },
            windows: WindowPlan {
                acquire_s: 0.5 * search_s,
                track_s: search_s,
                validate_s: total as f64 / rate_hz,
            },
            ..LoopConfig::default()
        }
    }

    /// The prefix with `tuning`'s bandwidth (on the S0 channel filter) and mode values bound.
    fn bound(&self, tuning: &Tuning) -> Recipe {
        let mut r = self.recipe.clone();
        if self.tune_bandwidth
            && let Some((id, _, _)) = &self.channel_filter
            && tuning.bandwidth_hz.is_finite()
            && tuning.bandwidth_hz > 0.0
            && let Some(n) = r.nodes.iter_mut().find(|n| &n.id == id)
            && let Some(v) = serde_json::Number::from_f64(0.5 * tuning.bandwidth_hz)
        {
            n.params.insert("cutoff_hz".to_owned(), Value::Number(v));
        }
        for axis in &self.axes {
            let Some(&v) = tuning.mode.get(&axis.path) else {
                continue;
            };
            let Some((id, name)) = parse_param_path(&axis.path) else {
                continue;
            };
            let value = if axis.integer {
                Value::from(v.round() as i64)
            } else {
                serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
            };
            if let Some(n) = r.nodes.iter_mut().find(|n| n.id == id) {
                n.params.insert(name.to_owned(), value);
            }
        }
        r
    }

    /// Runs `recipe` over `input` with support alignment (module docs).
    fn measure(&self, recipe: &Recipe, input: &[Complex32]) -> Result<Measured, WindowError> {
        let reg = &*self.ctx.registry;
        let set = &*self.ctx.calibration;
        let fill = self.ctx.fill;
        let run = |len: usize| {
            evaluate_window(
                recipe,
                reg,
                set,
                self.rate_hz,
                PortSlice::Iq(&input[..len.min(input.len())]),
                CHUNK_ITEMS,
                fill,
            )
        };
        let full = run(input.len())?;
        let mut scored = full.scored;
        let mut runs = 1u32;
        // Calibrated blocks whose support missed, each with the support to land on.
        let mut targets: Vec<(String, u32, u32)> = Vec::new(); // (node, seen n, cell n)
        let mut seen_nodes = BTreeSet::new();
        for s in &scored {
            let Some(Score::NoTable { cell }) = &s.score else {
                continue;
            };
            let Some(t) = set.get(&s.block) else {
                continue; // no table at all: aligning cannot help
            };
            if !t.admits(fill) || seen_nodes.contains(&s.node) {
                continue; // outside the table's population: 0 bits at any length
            }
            let n = cell.n;
            let best = t
                .cells()
                .filter(|c| c.metric == s.evidence.metric && c.null == NullKind::Noise)
                .map(|c| c.n)
                .filter(|&c| c <= n)
                .max();
            if let Some(c) = best {
                seen_nodes.insert(s.node.clone());
                targets.push((s.node.clone(), n, c));
            }
        }
        for (node, n, cell_n) in targets {
            let mid = f64::from(cell_n) + f64::from(cell_n / 50 + 16) / 2.0;
            let mut len = (input.len() as f64 * mid / f64::from(n.max(1))).ceil() as usize;
            let mut landed = None;
            for _ in 0..2 {
                let w = run(len)?;
                runs += 1;
                let got = w
                    .scored
                    .iter()
                    .filter(|x| x.node == node && x.score.is_some())
                    .map(|x| x.evidence.n)
                    .next();
                match got {
                    Some(g) if support_matches(cell_n, g) => {
                        landed = Some(w);
                        break;
                    }
                    Some(g) if g > 0 => {
                        let step = (mid - f64::from(g)) * len as f64 / f64::from(g);
                        len = (len as f64 + step).round().max(1.0) as usize;
                    }
                    _ => break,
                }
            }
            if let Some(w) = landed {
                // This block's records come from the run sized for it.
                scored.retain(|x| x.node != node);
                scored.extend(w.scored.into_iter().filter(|x| x.node == node));
            }
        }
        let ladder = StageLadder::of(&scored);
        Ok(Measured { ladder, runs })
    }

    fn measurement(&self, m: &Measured, depth: EvalDepth) -> Measurement {
        let lock = self.lock_stage();
        let floor = default_floor_bits(lock).unwrap_or(0.0);
        let at_lock = m.ladder.stage_bits(lock);
        let quality: f32 = Stage::ALL
            .iter()
            .filter(|&&s| s <= self.target)
            .filter_map(|&s| m.ladder.capped(s))
            .sum();
        let mut mode_params = BTreeMap::new();
        mode_params.insert("evidence_bits".to_owned(), f64::from(quality));
        mode_params.insert("support_runs".to_owned(), f64::from(m.runs));
        for s in Stage::ALL {
            if let Some(b) = m.ladder.stage_bits(s) {
                mode_params.insert(format!("b_{s:?}"), f64::from(b));
            }
        }
        let mut labels = BTreeMap::new();
        labels.insert("lock_stage".to_owned(), format!("{lock:?}"));
        labels.insert(
            "window".to_owned(),
            match depth {
                EvalDepth::Validate => "holdout",
                _ => "search",
            }
            .to_owned(),
        );
        Measurement {
            quality: f64::from(quality),
            locked: at_lock.is_some_and(|b| b >= floor),
            center_correction_hz: None,
            mode_params,
            labels,
        }
    }
}

struct Measured {
    ladder: StageLadder,
    runs: u32,
}

impl Objective for EvidenceObjective {
    fn name(&self) -> &str {
        EVIDENCE_OBJECTIVE
    }

    fn mode(&self) -> &str {
        &self.mode
    }

    fn space(&self, start: &RefineStart, rate_hz: f64) -> ParameterSpace {
        let c = self.channel;
        let bw = start.bandwidth_hz.max(1.0);
        let (center_hz, center_step_hz) = if self.tune_center {
            let span = c.center_span * bw;
            (
                (start.center_hz - span, start.center_hz + span),
                (c.center_step * bw).max(1.0),
            )
        } else {
            ((start.center_hz, start.center_hz), bw)
        };
        // The bandwidth axis is the S0 channel filter's width (module docs, "The channel"); the
        // channeliser in front of it is flat and fixed. The filter's support (width plus both
        // transitions) must stay inside the channeliser's flat passband.
        let (bandwidth_hz, bandwidth_step_hz, nominal_bandwidth_hz) =
            match (&self.channel_filter, self.tune_bandwidth) {
                (Some((_, cutoff, transition)), true) => {
                    let nominal = 2.0 * cutoff;
                    let widest = (self.channel_width(rate_hz) - 2.0 * transition).max(nominal);
                    let hi = (c.bandwidth_max * nominal).min(widest);
                    let lo = (c.bandwidth_min * nominal).max(1.0).min(hi);
                    ((lo, hi), (c.bandwidth_step * nominal).max(1.0), nominal)
                }
                (Some((_, cutoff, _)), false) => {
                    let w = 2.0 * cutoff;
                    ((w, w), w, w)
                }
                (None, _) => {
                    let w = self.channel_width(rate_hz);
                    ((w, w), w, w)
                }
            };
        ParameterSpace {
            center_hz,
            center_step_hz,
            bandwidth_hz,
            bandwidth_step_hz,
            nominal_bandwidth_hz,
            bandwidth_tolerance: c.bandwidth_tolerance_bits,
            mode_axes: self
                .axes
                .iter()
                .map(|a| ModeAxis {
                    name: a.path.clone(),
                    values: a.values.clone(),
                })
                .collect(),
        }
    }

    fn evaluate<T: IqSample>(
        &mut self,
        window: IqWindow<'_, T>,
        tuning: &Tuning,
        depth: EvalDepth,
    ) -> Result<Measurement, DemodError> {
        let split = self.ctx.split.holdout_from;
        let len = window.samples.len();
        let samples = match depth {
            EvalDepth::Acquire | EvalDepth::Track => &window.samples[..len.min(split)],
            EvalDepth::Validate if len > split => &window.samples[split..],
            EvalDepth::Validate => return Ok(Measurement::unlocked("no hold-out samples")),
        };
        let fs_in = window.rate_hz();
        // The tuning's bandwidth is not a filter here: the channeliser is always the flat
        // `channel_width`, so the prefix's S0 filter sees the null's white noise.
        let bw = self.channel_width(fs_in);
        let offset = tuning.center_hz - window.tuned_center_hz();
        if offset.abs() + 0.5 * bw > 0.5 * fs_in {
            return Ok(Measurement::unlocked("channel outside the window's band"));
        }
        let mut ddc = match Ddc::new(
            DdcSpec::new(offset, bw).with_output_rate(self.rate_hz),
            fs_in,
        ) {
            Ok(d) => d,
            Err(e) => return Ok(Measurement::unlocked(format!("channel: {e}"))),
        };
        let input: Vec<Complex32> = match ddc.process(window.stream_start_info(), samples) {
            Ok(b) => b.samples.to_vec(),
            Err(e) => return Ok(Measurement::unlocked(format!("channel: {e}"))),
        };
        if input.is_empty() {
            return Ok(Measurement::unlocked("empty window"));
        }
        let recipe = self.bound(tuning);
        match self.measure(&recipe, &input) {
            Ok(m) => Ok(self.measurement(&m, depth)),
            // A mode value the block refuses, or a prefix that cannot run at this tuning: an
            // unlocked measurement, not a broken configuration.
            Err(e) => Ok(Measurement::unlocked(format!("prefix: {e}"))),
        }
    }
}
