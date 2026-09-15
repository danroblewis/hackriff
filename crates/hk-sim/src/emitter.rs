//! Lazily generated transmission streams. Each emitter owns an RNG stream derived from the
//! scenario seed and its id, so its transmissions are identical under every policy (common random
//! numbers) and generation cost is proportional to transmissions, not to observations.

use crate::rng::Rng;
use crate::scenario::{Activity, EmitterSpec, Scenario};

/// One transmission.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tx {
    /// Start, ns from run start.
    pub start: i64,
    /// End, ns (`i64::MAX` for a continuous emitter).
    pub end: i64,
    /// Centre, Hz.
    pub freq_hz: f64,
    /// SNR, dB.
    pub snr_db: f64,
}

/// Per-emitter run state.
#[derive(Clone, Debug)]
pub struct EmitterState {
    rng: Rng,
    /// The current (not yet finished) transmission.
    pub cur: Tx,
    cur_captured: bool,
    beacon_k: i64,
    /// Finished transmissions.
    pub total: u64,
    /// Finished transmissions detected at least once.
    pub captured: u64,
    /// First detection, ns.
    pub first_detect_ns: Option<i64>,
}

impl EmitterState {
    /// Initial state.
    pub fn new(scenario: &Scenario, spec: &EmitterSpec) -> Self {
        let mut s = Self {
            rng: Rng::derive(scenario.seed, spec.id),
            cur: Tx {
                start: i64::MAX,
                end: i64::MAX,
                freq_hz: spec.center_hz,
                snr_db: spec.snr_db,
            },
            cur_captured: false,
            beacon_k: 0,
            total: 0,
            captured: 0,
            first_detect_ns: None,
        };
        s.cur = s.generate(scenario, spec, spec.appears_ns);
        s
    }

    /// Records the current transmission as finished and generates the next.
    pub fn finish(&mut self, scenario: &Scenario, spec: &EmitterSpec) {
        self.total += 1;
        self.captured += u64::from(self.cur_captured);
        self.cur_captured = false;
        let from = self.cur.end;
        self.cur = self.generate(scenario, spec, from);
    }

    /// Marks the current transmission captured, detected at `t_ns`.
    pub fn capture(&mut self, t_ns: i64) {
        self.cur_captured = true;
        if self.first_detect_ns.is_none() {
            self.first_detect_ns = Some(t_ns);
        }
    }

    /// Whether the current transmission is already captured.
    pub fn cur_captured(&self) -> bool {
        self.cur_captured
    }

    fn snr(&mut self, spec: &EmitterSpec) -> f64 {
        if spec.fade_db > 0.0 {
            spec.snr_db + spec.fade_db * self.rng.normal()
        } else {
            spec.snr_db
        }
    }

    /// Next Poisson start at or after `from`, thinned by the time-of-day factor.
    fn poisson_start(
        &mut self,
        scenario: &Scenario,
        spec: &EmitterSpec,
        from: i64,
        mean_gap_ns: i64,
    ) -> i64 {
        let peak = 1.0 + spec.diurnal_depth.max(0.0);
        let mut t = from;
        loop {
            t = t.saturating_add(self.rng.exp(mean_gap_ns as f64 / peak).ceil() as i64);
            if spec.diurnal_depth <= 0.0
                || t == i64::MAX
                || self.rng.f64() * peak < scenario.diurnal_factor(spec.diurnal_depth, t)
            {
                return t;
            }
        }
    }

    fn generate(&mut self, scenario: &Scenario, spec: &EmitterSpec, from: i64) -> Tx {
        let (start, end, freq_hz) = match spec.activity {
            Activity::Continuous => {
                if from > spec.appears_ns {
                    (i64::MAX, i64::MAX, spec.center_hz)
                } else {
                    (spec.appears_ns, i64::MAX, spec.center_hz)
                }
            }
            Activity::Poisson {
                tau_ns,
                mean_gap_ns,
            } => {
                let start = self.poisson_start(scenario, spec, from, mean_gap_ns);
                (start, start.saturating_add(tau_ns), spec.center_hz)
            }
            Activity::Periodic {
                period_ns,
                tau_ns,
                phase_ns,
                jitter_ns,
            } => {
                let base = spec.appears_ns + phase_ns - period_ns;
                let mut start;
                loop {
                    self.beacon_k += 1;
                    let jitter = if jitter_ns > 0 {
                        self.rng.uniform(-(jitter_ns as f64), jitter_ns as f64) as i64
                    } else {
                        0
                    };
                    start = base + self.beacon_k * period_ns + jitter;
                    if start >= from.max(spec.appears_ns) {
                        break;
                    }
                }
                (start, start + tau_ns, spec.center_hz)
            }
            Activity::Hopping {
                channel0_hz,
                spacing_hz,
                channels,
                tau_ns,
                mean_gap_ns,
            } => {
                let start = self.poisson_start(scenario, spec, from, mean_gap_ns);
                let ch = self.rng.below(u64::from(channels)) as f64;
                (
                    start,
                    start.saturating_add(tau_ns),
                    channel0_hz + spacing_hz * ch,
                )
            }
        };
        let snr_db = self.snr(spec);
        Tx {
            start,
            end,
            freq_hz,
            snr_db,
        }
    }

    /// Counts transmissions that start before `end_ns` (including one still running).
    pub fn close(&mut self, scenario: &Scenario, spec: &EmitterSpec, end_ns: i64) {
        while self.cur.start < end_ns {
            if self.cur.end == i64::MAX {
                self.total += 1;
                self.captured += u64::from(self.cur_captured);
                self.cur_captured = false;
                self.cur.start = i64::MAX;
                break;
            }
            self.finish(scenario, spec);
        }
    }
}
