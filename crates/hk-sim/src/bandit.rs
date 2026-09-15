//! The T-120 bandit revisit policy under simulation: the hk-core scheduler with
//! `Scheduler::enable_bandit` (ADR-0012 §5), fed by a detection-count interestingness stub
//! (ADR-0012 §5.6) published through [`SharedInterestingness`].
//!
//! The stub sees only what every policy sees (measured detections keyed by tracker id, never the
//! truth population):
//! - score components: measured SNR; novelty = 1 − age / `novelty_window_s` since first sighting;
//!   class entropy unknown (scored 1); boring prior 1 once a detection was continuous in at least
//!   `boring_revisits` sightings and at least half of them;
//! - `needs_verification` and `suspect_fraction` from the C05 suspect flag;
//! - `expected_interval_s` = live stream seconds per detected transmission once ≥ 2 were seen.
//!
//! It re-scores every `rescore_s` of simulated time when a new track appeared, and at least every
//! `refresh_s` (novelty decays). Dwell outcomes: new non-suspect detections, bursts (transmissions
//! of non-continuous detections), Σ novelty of the other non-suspect detections, suspect
//! detections.

use std::collections::BTreeMap;
use std::sync::Arc;

use hk_core::ScheduleStep;
use hk_core::scheduler::{CompiledPlan, Purpose, Scheduler, SyntheticClock};
use hk_model::attention::baseline::{BaselineResolution, Maturity};
use hk_model::attention::schedule::{BanditConfig, DwellOutcome};
use hk_model::attention::score::{
    Candidate, CandidateSet, CandidateSubject, NoveltyScore, ScoreComponents, ScoreWeights,
    SharedInterestingness, interestingness, normalised,
};
use hk_model::{EmitterId, FreqRange, ScanPolicy, Timestamp};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::SimError;
use crate::policy::{Observation, Policy, PolicyRegistry, SimEnv};
use crate::radio::RadioMode;
use crate::scenario::{S, T0_NS};

/// Name of the bandit policy.
pub const BANDIT: &str = "bandit";

/// Parameters of the `bandit` policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BanditParams {
    /// ADR-0012 §5 settings (defaults: c 0.5, floors 15 % / 25 %, 30 min, 6 h half-life).
    pub config: BanditConfig,
    /// Re-score cadence after a new track, s (10: ADR-0012 §4.5).
    pub rescore_s: f64,
    /// Re-score at least this often, s (60).
    pub refresh_s: f64,
    /// Novelty decays to 0 over this age, s (3600: "first seen in the last hour", §5.6).
    pub novelty_window_s: f64,
    /// Continuous sightings before the boring prior applies (10, §4.3).
    pub boring_revisits: u32,
    /// Bandit dwell slots after each sweep pass (1; the sweep floor gates them).
    pub dwells_per_pass: u32,
}

impl Default for BanditParams {
    fn default() -> Self {
        Self {
            config: BanditConfig::default(),
            rescore_s: 10.0,
            refresh_s: 60.0,
            novelty_window_s: 3600.0,
            boring_revisits: 10,
            dwells_per_pass: 1,
        }
    }
}

/// Registers `bandit` with default parameters.
pub fn register(registry: &mut PolicyRegistry) {
    registry.register(
        BANDIT,
        Box::new(|env| {
            Ok(Box::new(BanditPolicy::new(env, BanditParams::default())?) as Box<dyn Policy>)
        }),
    );
}

#[derive(Clone, Copy, Debug)]
struct Track {
    center_hz: f64,
    bandwidth_hz: f64,
    snr_db: f64,
    suspect: bool,
    first_seen_ns: i64,
    sightings: u32,
    continuous: u32,
    transmissions: u64,
    live_ns: i64,
}

/// The bandit scheduler plus the detection-count interestingness stub.
pub struct BanditPolicy {
    p: BanditParams,
    params: Value,
    clock: SyntheticClock,
    sched: Scheduler<SyntheticClock>,
    provider: Arc<SharedInterestingness>,
    weights: ScoreWeights,
    tracks: BTreeMap<u64, Track>,
    dirty: bool,
    last_publish_ns: i64,
}

impl BanditPolicy {
    /// `SweepThenDwell` over the scenario regions: one sweep pass per cycle, then
    /// `dwells_per_pass` bandit slots.
    pub fn new(env: &SimEnv<'_>, p: BanditParams) -> Result<Self, SimError> {
        let plan = env.scenario.plan(ScanPolicy::SweepThenDwell, Value::Null);
        let mut cfg = env.radio.sweep_scheduler_config();
        cfg.validate(env.caps)?;
        let hops = CompiledPlan::compile(&plan, &cfg, env.caps)?.hops.len();
        cfg.sweeps_per_cycle = u32::try_from(hops).unwrap_or(u32::MAX).max(1);
        cfg.dwells_per_cycle = p.dwells_per_pass;
        let clock = SyntheticClock::new(Timestamp::from_unix_nanos(T0_NS));
        let mut sched = Scheduler::new(&plan, cfg, env.caps, clock.clone())?;
        let provider = Arc::new(SharedInterestingness::default());
        sched.enable_bandit(p.config, Arc::clone(&provider) as _)?;
        let c = p.config;
        let params = json!({
            "plan_policy": "sweep-then-dwell",
            "hops_per_pass": hops,
            "dwells_per_pass": p.dwells_per_pass,
            "ucb_c": c.ucb_c,
            "discount_half_life_s": c.discount_half_life_s,
            "exploration_floor": c.exploration_floor,
            "sweep_floor": c.sweep_floor,
            "sweep_floor_window_s": c.sweep_floor_window_s,
            "max_arm_staleness_s": c.max_arm_staleness_s,
            "dwell_s": [c.min_dwell_s, c.max_dwell_s],
            "dwell_periods": c.dwell_periods,
            "max_arms": c.max_arms,
            "suspect_ban_s": c.suspect_ban_s,
            "rescore_s": p.rescore_s,
            "refresh_s": p.refresh_s,
            "novelty_window_s": p.novelty_window_s,
            "boring_revisits": p.boring_revisits,
            "honours_suspect": true,
        });
        Ok(Self {
            p,
            params,
            clock,
            sched,
            provider,
            weights: ScoreWeights::default(),
            tracks: BTreeMap::new(),
            dirty: false,
            last_publish_ns: T0_NS,
        })
    }

    /// The wrapped scheduler.
    pub fn scheduler(&self) -> &Scheduler<SyntheticClock> {
        &self.sched
    }

    fn novelty(&self, first_seen_ns: i64, now_ns: i64) -> f64 {
        let age = (now_ns - first_seen_ns).max(0) as f64 / S as f64;
        (1.0 - age / self.p.novelty_window_s).clamp(0.0, 1.0)
    }

    fn candidate(&self, key: u64, t: &Track, now_ns: i64) -> Candidate {
        let novelty = self.novelty(t.first_seen_ns, now_ns);
        let boring = t.continuous >= self.p.boring_revisits && 2 * t.continuous >= t.sightings;
        let components = ScoreComponents {
            snr_db: Some(t.snr_db),
            novelty,
            class_entropy: None,
            decoder_available: false,
            periodicity: None,
            boring_prior: if boring { 1.0 } else { 0.0 },
        };
        let score = interestingness(&self.weights, &components);
        let continuous = 2 * t.continuous >= t.sightings;
        Candidate {
            subject: CandidateSubject::Emitter {
                id: EmitterId::from_uuid(Uuid::from_u128(u128::from(key))),
            },
            freq: FreqRange::centered(t.center_hz, t.bandwidth_hz.max(1.0)),
            score,
            score_norm: normalised(&self.weights, score),
            components,
            novelty: NoveltyScore {
                novelty,
                level_z: None,
                occupancy_z: None,
                new_emitter: Some(novelty),
                observed_s: t.live_ns as f64 / S as f64,
                maturity: Maturity::Mature {
                    resolution: BaselineResolution::AllHours,
                },
                provenance_explained: false,
            },
            suspect_fraction: if t.suspect { 1.0 } else { 0.0 },
            needs_verification: t.suspect,
            expected_interval_s: (!continuous && t.transmissions >= 2)
                .then(|| t.live_ns as f64 / S as f64 / t.transmissions as f64),
            min_on_off_s: None,
            next_burst_eta: None,
        }
    }

    fn publish(&mut self, now_ns: i64) {
        let mut cands: Vec<(u64, Candidate)> = self
            .tracks
            .iter()
            .map(|(&k, t)| (k, self.candidate(k, t, now_ns)))
            .collect();
        cands.sort_by(|a, b| b.1.score.total_cmp(&a.1.score).then(a.0.cmp(&b.0)));
        let mut set = CandidateSet::empty(Timestamp::from_unix_nanos(now_ns));
        set.weights = self.weights;
        set.candidates = cands.into_iter().map(|c| c.1).collect();
        self.provider
            .publish(set)
            .expect("the stub candidate set validates");
        self.dirty = false;
        self.last_publish_ns = now_ns;
    }
}

impl Policy for BanditPolicy {
    fn name(&self) -> &str {
        BANDIT
    }

    fn params(&self) -> Value {
        self.params.clone()
    }

    fn next_step(&mut self, now: Timestamp) -> ScheduleStep {
        self.clock.set(now);
        self.sched.next_step()
    }

    fn observe(&mut self, step: &ScheduleStep, obs: &Observation<'_>) {
        let (a, b) = (obs.start.as_unix_nanos(), obs.end.as_unix_nanos());
        let live = (b - a).max(0);
        let mut outcome = DwellOutcome {
            seq: step.seq,
            arm: self.sched.arm_key_of(step),
            dwell_s: live as f64 / S as f64,
            new_detections: 0,
            bursts: 0,
            novelty_sum: 0.0,
            valid_decodes: 0,
            suspect_detections: 0,
        };
        for d in obs.detections {
            let prior = self.tracks.get(&d.key).map(|t| t.first_seen_ns);
            let novelty = prior.map_or(0.0, |first| self.novelty(first, b));
            let t = self.tracks.entry(d.key).or_insert(Track {
                center_hz: d.center_hz,
                bandwidth_hz: d.bandwidth_hz,
                snr_db: d.snr_db,
                suspect: d.suspect,
                first_seen_ns: b,
                sightings: 0,
                continuous: 0,
                transmissions: 0,
                live_ns: 0,
            });
            t.center_hz = d.center_hz;
            t.bandwidth_hz = d.bandwidth_hz;
            t.snr_db = t.snr_db.max(d.snr_db);
            t.suspect = d.suspect;
            t.sightings += 1;
            t.continuous += u32::from(d.continuous);
            if obs.mode == RadioMode::Stream {
                t.transmissions += u64::from(d.transmissions);
                t.live_ns += live;
            }
            if d.suspect {
                outcome.suspect_detections += 1;
            } else if prior.is_none() {
                outcome.new_detections += 1;
                self.dirty = true;
            } else {
                if !d.continuous {
                    outcome.bursts += d.transmissions;
                }
                outcome.novelty_sum += novelty;
            }
            if prior.is_none() {
                self.dirty = true;
            }
        }
        if matches!(step.purpose, Purpose::Bandit { .. }) && live > 0 {
            self.clock.set(obs.end);
            self.sched.record_outcome(&outcome);
        }
        let since = (b - self.last_publish_ns) as f64 / S as f64;
        if (self.dirty && since >= self.p.rescore_s) || since >= self.p.refresh_s {
            self.publish(b);
        }
    }
}
