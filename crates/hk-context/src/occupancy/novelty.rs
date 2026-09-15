//! Novelty against a baseline (T-119, ADR-0012 §4.4): level and occupancy z-scores normalised by
//! observation time, new-emitter novelty as a Poisson tail, combined and forced to zero when the
//! baseline is immature or provenance explains the change.
//!
//! - **`level_z`** = (level − pool mean)/max(σ_pool, 1 dB), upward only (`level-above-baseline`;
//!   a drop in level is a front-end or propagation question, not a new signal).
//! - **`occupancy_z`** = (FCO_obs − p)/√(max(p(1−p), q(1−q))/n_eff + v_between), two-sided, with
//!   q the observed FCO shrunk by half a sample (a conservative variance: a few looks cannot
//!   exploit the normal approximation at tiny p). `p` is the pool's
//!   time-weighted FCO shrunk by half an effective sample towards ½ (so an empty channel's p is
//!   small but not 0, and one occupied look is not infinitely novel); `n_eff` is the interval's
//!   effective samples (§2.4), so a short look cannot produce a large z; `v_between` is the
//!   spread of the pool's slot FCOs, so a coarse pool (e.g. all hours) does not call an
//!   hour-of-week pattern novel.
//! - **`new_emitter`** = [`new_emitter_novelty`] of first sightings in a window against the site's
//!   baseline first-sighting rate ([`FirstSightingRate`]).
//! - **Combined** = max of `novelty_from_z(z, 3, 10)` over the z-scores and `new_emitter`.

use std::collections::VecDeque;

use hk_model::attention::baseline::{MATURITY_MIN_OBSERVED_S, Maturity};
use hk_model::attention::score::{NoveltyScore, new_emitter_novelty, novelty_from_z};
use hk_model::time::Timestamp;

use super::baseline::PoolStats;

/// Novelty mapping settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoveltyConfig {
    /// z at which novelty starts (3).
    pub z_min: f64,
    /// z at which novelty saturates at 1 (10).
    pub z_sat: f64,
    /// Floor on the level σ, dB (1).
    pub sigma_floor_db: f64,
}

impl Default for NoveltyConfig {
    fn default() -> Self {
        Self {
            z_min: 3.0,
            z_sat: 10.0,
            sigma_floor_db: 1.0,
        }
    }
}

/// What one interval measured on a subject, as novelty needs it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Evidence {
    /// Representative level, dB.
    pub level_db: Option<f64>,
    /// Time-weighted FCO.
    pub fco: Option<f64>,
    /// Effective independent samples behind `fco` (§2.4).
    pub n_eff: f64,
    /// Σ weight behind `fco`, s.
    pub weight_s: f64,
    /// Represented seconds.
    pub observed_s: f64,
}

/// Level z against a level pool (upward), `None` without a mean.
pub fn level_z(pool: &PoolStats, ev: &Evidence, cfg: &NoveltyConfig) -> Option<f64> {
    let level = ev.level_db?;
    let mean = pool.mean_db()?;
    let sigma = pool.std_db().unwrap_or(0.0).max(cfg.sigma_floor_db);
    Some((level - mean) / sigma)
}

/// The pool's shrunk FCO for an interval whose one effective sample represents `delta_s`.
pub fn shrunk_fco(pool: &PoolStats, delta_s: f64) -> Option<f64> {
    let f = pool.fco()?;
    let d = delta_s.max(1e-9);
    Some((f * pool.weight_s + 0.5 * d) / (pool.weight_s + d))
}

/// σ of an interval FCO with `n_eff` samples around the pool (binomial + between-slot spread).
pub fn occupancy_sigma(pool: &PoolStats, p: f64, n_eff: f64) -> f64 {
    (p * (1.0 - p) / n_eff.max(1.0) + pool.between_var()).sqrt()
}

/// Occupancy z (two-sided sign kept), `None` without FCO on either side.
pub fn occupancy_z(pool: &PoolStats, ev: &Evidence) -> Option<f64> {
    let fco = ev.fco?;
    if ev.n_eff < 1.0 || ev.weight_s <= 0.0 {
        return None;
    }
    let p = shrunk_fco(pool, ev.weight_s / ev.n_eff)?;
    // Conservative variance: the larger of the pool's and the (shrunk) observed binomial
    // variance, so a handful of looks cannot exploit the normal approximation at tiny p.
    let q = (fco * ev.n_eff + 0.5) / (ev.n_eff + 1.0);
    let var = (p * (1.0 - p)).max(q * (1.0 - q)) / ev.n_eff + pool.between_var();
    Some((fco - p) / var.sqrt())
}

/// Combines the components into a [`NoveltyScore`] under the zero rules.
pub fn combine(
    maturity: Maturity,
    level_z: Option<f64>,
    occupancy_z: Option<f64>,
    new_emitter: Option<f64>,
    observed_s: f64,
    provenance_explained: bool,
    cfg: &NoveltyConfig,
) -> NoveltyScore {
    let from_z = |z: Option<f64>| z.map_or(0.0, |z| novelty_from_z(z, cfg.z_min, cfg.z_sat));
    let raw = from_z(level_z)
        .max(from_z(occupancy_z.map(f64::abs)))
        .max(new_emitter.unwrap_or(0.0));
    let novelty = if maturity.is_mature() && !provenance_explained {
        raw
    } else {
        0.0
    };
    NoveltyScore {
        novelty,
        level_z: level_z.filter(|z| z.is_finite()),
        occupancy_z: occupancy_z.filter(|z| z.is_finite()),
        new_emitter: new_emitter.map(|v| v.clamp(0.0, 1.0)),
        observed_s: observed_s.max(0.0),
        maturity,
        provenance_explained,
    }
}

/// The site's baseline rate of first sightings (new emitters per observed second) and a sliding
/// window of recent sightings (§4.4). Mature once `MATURITY_MIN_OBSERVED_S` of observation is
/// behind the rate.
#[derive(Clone, Debug, Default)]
pub struct FirstSightingRate {
    baseline_k: f64,
    baseline_observed_s: f64,
    window: VecDeque<(Timestamp, u64, f64)>,
}

impl FirstSightingRate {
    /// Prior sightings added to the baseline count (so a never-seen rate is not zero).
    pub const PRIOR_COUNT: f64 = 1.0;
    /// Sliding window, s.
    pub const WINDOW_S: f64 = 3600.0;

    /// Folds `k` first sightings over `observed_s` ending at `t`. `accrue` adds them to the
    /// baseline rate (a `Site` key, not novel); the window always gets them.
    pub fn record(&mut self, t: Timestamp, k: u64, observed_s: f64, accrue: bool) {
        if accrue {
            self.baseline_k += k as f64;
            self.baseline_observed_s += observed_s.max(0.0);
        }
        self.window.push_back((t, k, observed_s.max(0.0)));
        let horizon = t.as_unix_nanos() - (Self::WINDOW_S * 1e9) as i64;
        while self
            .window
            .front()
            .is_some_and(|(ts, _, _)| ts.as_unix_nanos() <= horizon)
        {
            self.window.pop_front();
        }
    }

    /// Baseline rate, per observed second; `None` while immature.
    pub fn rate(&self) -> Option<f64> {
        (self.baseline_observed_s >= MATURITY_MIN_OBSERVED_S)
            .then(|| (self.baseline_k + Self::PRIOR_COUNT) / self.baseline_observed_s)
    }

    /// Novelty of the window's sightings; `None` while immature.
    pub fn novelty(&self) -> Option<f64> {
        let rate = self.rate()?;
        let (k, s) = self.window_totals();
        Some(new_emitter_novelty(k, rate, s))
    }

    /// First sightings and observed seconds in the sliding window.
    pub fn window_totals(&self) -> (u64, f64) {
        self.window
            .iter()
            .fold((0, 0.0), |(k, s), (_, dk, ds)| (k + dk, s + ds))
    }

    /// Observed seconds accrued to the baseline rate (mature at `MATURITY_MIN_OBSERVED_S`).
    pub fn baseline_observed_s(&self) -> f64 {
        self.baseline_observed_s
    }
}

// ---- T-146: sequential evidence for busier / quieter than usual (ADR-0012 §7.2) ----

/// A run of consecutive scored intervals whose sequential evidence is reset after a gap longer
/// than this, s (2 h, eight 15-min intervals): evidence is not stitched across a long absence.
pub const SEQUENTIAL_MAX_GAP_S: f64 = 7200.0;

/// ln Q(x), Q the standard normal upper tail, accurate in the far tail (no underflow): the
/// Chebyshev erfc fit of Numerical Recipes (fractional error < 1.2·10⁻⁷ for every x) in log form.
pub fn ln_normal_upper_tail(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x < 0.0 {
        return (-ln_normal_upper_tail(-x).exp()).ln_1p();
    }
    let z = x / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.5 * z);
    let poly = -z * z - 1.265_512_23
        + t * (1.000_023_68
            + t * (0.374_091_96
                + t * (0.096_784_18
                    + t * (-0.186_288_06
                        + t * (0.278_868_07
                            + t * (-1.135_203_98
                                + t * (1.488_515_87 + t * (-0.822_152_23 + t * 0.170_872_77))))))));
    t.ln() + poly - std::f64::consts::LN_2
}

/// The x with ln Q(x) = `ln_p` (bisection on [−40, 40]; monotone).
pub fn normal_upper_tail_inv_ln(ln_p: f64) -> f64 {
    let (mut lo, mut hi) = (-40.0_f64, 40.0_f64);
    for _ in 0..64 {
        let mid = 0.5 * (lo + hi);
        if ln_normal_upper_tail(mid) > ln_p {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// Multiplicity weight of a run of `k` intervals, g(k) = 2k(k+1): Σ_{k≥1} 1/g(k) = ½.
pub fn sequential_weight(k: u32) -> f64 {
    let k = f64::from(k.max(1));
    2.0 * k * (k + 1.0)
}

/// Sequential evidence of a run of consecutive same-direction intervals (T-146).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SequentialEvidence {
    /// Intervals in the run (k).
    pub intervals: u32,
    /// Stouffer z of the run, S = Σzᵢ/√k (signed: the run's direction).
    pub z: f64,
    /// Novelty of the run on the single-interval scale (see [`sequential_novelty`]).
    pub novelty: f64,
}

/// Novelty of a run of `k` intervals with Stouffer statistic `s` = |Σzᵢ|/√k (T-146, ADR-0012
/// §7.2).
///
/// The run's p-value is corrected for having looked at every run length,
/// p_seq = min(1, g(k)·Q(s)) with g(k) = [`sequential_weight`], and mapped to the single-interval
/// scale as z_eq = Q⁻¹(√p_seq), novelty = `novelty_from_z(z_eq, z_min, z_sat)`. So
/// novelty ≥ on ⇔ z_eq ≥ z_on ⇔ p_seq ≤ Q(z_on)², z_on = z_min + on·(z_sat − z_min) (7.9 at the
/// defaults): a run reaches the alarm on level only when it is as improbable under the null as
/// the raise event of the single-interval rule (two consecutive intervals at z_on).
///
/// **False-alarm budget.** Under the null (zᵢ i.i.d. N(0,1)) the run ending at interval t that
/// reaches on has some length k, and then the sum of the last k z's is ≥ c_k√k with
/// Q(c_k) = Q(z_on)²/g(k); that sum is N(0, k), so P(k-run reaches on) ≤ Q(z_on)²/g(k) and
/// P(any run at t reaches on) ≤ Q(z_on)²·Σ1/g(k) = Q(z_on)²/2. Resets (direction, gap, gain,
/// maturity) only shorten runs. The two-interval hysteresis raises only when interval t reaches
/// on, so the sequential rule's null raise rate per subject, per direction, per scored interval
/// is ≤ Q(z_on)²/2 ≈ 9.7·10⁻³¹ at z_on 7.9: within the single-interval rule's budget
/// Q(z_on)² ≈ 1.9·10⁻³⁰ (two consecutive intervals ≥ z_on). The engine alarms on the larger of
/// the single-interval and the sequential novelty, so the combined rule is ≤ Q(z_on)² (single) +
/// Q(z_on)²/2 (sequential at t) + Q(z_on)³ (sequential at t−1 then a single at t) per interval.
pub fn sequential_novelty(k: u32, s: f64, cfg: &NoveltyConfig) -> f64 {
    if !s.is_finite() || s <= 0.0 || cfg.z_sat <= cfg.z_min {
        return 0.0;
    }
    let ln_p_seq = (sequential_weight(k).ln() + ln_normal_upper_tail(s)).min(0.0);
    let ln_p_eq = 0.5 * ln_p_seq;
    if ln_p_eq >= ln_normal_upper_tail(cfg.z_min) {
        return 0.0;
    }
    if ln_p_eq <= ln_normal_upper_tail(cfg.z_sat) {
        return 1.0;
    }
    novelty_from_z(normal_upper_tail_inv_ln(ln_p_eq), cfg.z_min, cfg.z_sat)
}

/// One subject's run of consecutive same-direction scored intervals (T-146).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SequentialRun {
    /// +1 busier, −1 quieter.
    pub direction: i8,
    /// Intervals in the run.
    pub k: u32,
    /// Σ z (signed).
    pub sum_z: f64,
    /// Last interval folded (sample clock).
    pub last_t: Timestamp,
    /// Front-end gain-state key of the run.
    pub gain: u32,
}

/// Steps `run` with an interval scored `z` at `t` under `gain` and returns the run's evidence.
///
/// The run **resets** (starts again at this interval) on a direction change (sign of z), a gain
/// key change, a gap longer than `max_gap_s` since its last interval, or a time going backwards;
/// a z of 0 or non-finite ends it. The same interval fed twice (`t` = last) is not counted
/// twice. Maturity, provenance and site resets are the caller's (the alarm engine's).
pub fn sequential_step(
    run: &mut Option<SequentialRun>,
    t: Timestamp,
    z: f64,
    gain: u32,
    max_gap_s: f64,
    cfg: &NoveltyConfig,
) -> Option<SequentialEvidence> {
    if !z.is_finite() || z == 0.0 {
        *run = None;
        return None;
    }
    let direction: i8 = if z > 0.0 { 1 } else { -1 };
    let gap_s = |r: &SequentialRun| (t.as_unix_nanos() - r.last_t.as_unix_nanos()) as f64 / 1e9;
    match run {
        Some(r)
            if r.direction == direction
                && r.gain == gain
                && gap_s(r) >= 0.0
                && gap_s(r) <= max_gap_s =>
        {
            if t > r.last_t {
                r.k += 1;
                r.sum_z += z;
                r.last_t = t;
            }
        }
        _ => {
            *run = Some(SequentialRun {
                direction,
                k: 1,
                sum_z: z,
                last_t: t,
                gain,
            });
        }
    }
    let r = run.as_ref()?;
    let s = r.sum_z.abs() / f64::from(r.k).sqrt();
    Some(SequentialEvidence {
        intervals: r.k,
        z: f64::from(r.direction) * s,
        novelty: sequential_novelty(r.k, s, cfg),
    })
}

/// T-138 (ADR-0012 §7.1, rule "single persistent new emitter"): the first-sighting count a
/// confirmed persistent single new emitter stands for. Its re-sighting in a later interval is a
/// second, independent look at the same arrival, so it is scored as two first sightings.
pub const PERSISTENT_SINGLE_EQUIVALENT_COUNT: u64 = 2;

/// T-138: novelty of one confirmed persistent new emitter at a site whose baseline first-sighting
/// rate is `rate_per_s`: [`new_emitter_novelty`] of [`PERSISTENT_SINGLE_EQUIVALENT_COUNT`]
/// sightings over a full [`FirstSightingRate::WINDOW_S`] (the global −log10 p / 6 mapping is
/// unchanged). With μ = rate · WINDOW_S, novelty ≥ `on` ⇔ P(X ≥ 1 | μ) ≤
/// [`persistent_single_alpha`]`(on)`, i.e. at the default on level 0.7 the gate
/// P(≥ 1 new emitter in the window | μ) ≤ α ≈ 0.0112 (μ ≤ 0.0113): the rule alarms exactly where
/// two first sightings in one window would reach the existing 0.7 budget (tail 10^−4.2).
pub fn persistent_single_novelty(rate_per_s: f64) -> f64 {
    new_emitter_novelty(
        PERSISTENT_SINGLE_EQUIVALENT_COUNT,
        rate_per_s,
        FirstSightingRate::WINDOW_S,
    )
}

/// T-138: the α equivalent to an alarm on level `on` for [`persistent_single_novelty`]:
/// μ* solves P(X ≥ 2 | μ*) = 10^(−6·on) and α = P(X ≥ 1 | μ*) = 1 − e^(−μ*).
pub fn persistent_single_alpha(on: f64) -> f64 {
    let target = 10f64.powf(-6.0 * on.clamp(0.0, 1.0));
    let tail2 = |mu: f64| -(-mu).exp_m1() - mu * (-mu).exp();
    let (mut lo, mut hi) = (0.0_f64, 50.0_f64);
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if tail2(mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    -(-0.5 * (lo + hi)).exp_m1()
}

#[cfg(test)]
mod tests {
    use hk_model::attention::baseline::BaselineResolution;

    use super::*;
    use crate::occupancy::baseline::BETWEEN_VAR_FLOOR_FRACTION;

    fn pool(levels: &[f64], fcos: &[(f64, f64)]) -> PoolStats {
        let mut p = PoolStats::default();
        for (i, l) in levels.iter().enumerate() {
            let (f, w) = fcos.get(i).copied().unwrap_or((0.0, 0.0));
            p.add(1.0, w, *l, l * l, f * w, w, *l);
        }
        p
    }

    fn mature() -> Maturity {
        Maturity::Mature {
            resolution: BaselineResolution::AllHours,
        }
    }

    #[test]
    fn novelty_level_z_floors_sigma_and_is_upward() {
        let cfg = NoveltyConfig::default();
        let flat = pool(&[-100.0; 10], &[]);
        let ev = |l| Evidence {
            level_db: Some(l),
            fco: None,
            n_eff: 0.0,
            weight_s: 0.0,
            observed_s: 900.0,
        };
        assert_eq!(
            level_z(&flat, &ev(-95.0), &cfg),
            Some(5.0),
            "σ floored at 1 dB"
        );
        let n = combine(mature(), Some(-20.0), None, None, 900.0, false, &cfg);
        assert_eq!(n.novelty, 0.0, "a level drop is not novelty");
        let n = combine(mature(), Some(6.5), None, None, 900.0, false, &cfg);
        assert_eq!(n.novelty, 0.5);
    }

    #[test]
    fn novelty_occupancy_z_is_normalised_by_observation_time() {
        // A channel empty over 30 h of 15-min intervals.
        let quiet = pool(&[-100.0; 120], &[(0.0, 900.0); 120]);
        let look = |n_eff: f64, fco| Evidence {
            level_db: None,
            fco: Some(fco),
            n_eff,
            weight_s: 900.0,
            observed_s: 900.0,
        };
        let short = occupancy_z(&quiet, &look(1.0, 1.0)).unwrap();
        let long = occupancy_z(&quiet, &look(30.0, 0.6)).unwrap();
        assert!(short < 3.0, "one occupied look is not novel: z={short}");
        assert!(long > 5.0, "a sustained emitter is: z={long}");
        // A patterned channel (half the slots 0.1, half 0.7) under a coarse pool: its daytime FCO
        // is not novel thanks to the between-slot spread.
        let fcos: Vec<_> = (0..120)
            .map(|i| (if i % 2 == 0 { 0.1 } else { 0.7 }, 900.0))
            .collect();
        let patterned = pool(&[-100.0; 120], &fcos);
        assert!(occupancy_z(&patterned, &look(30.0, 0.75)).unwrap() < 3.0);
    }

    #[test]
    fn novelty_zero_rules_and_new_emitter_rate() {
        let cfg = NoveltyConfig::default();
        let imm = Maturity::Immature { observed_s: 10.0 };
        let n = combine(imm, Some(50.0), Some(50.0), Some(1.0), 1.0, false, &cfg);
        assert_eq!(n.novelty, 0.0);
        n.validate().unwrap();
        let n = combine(mature(), Some(50.0), None, None, 1.0, true, &cfg);
        assert_eq!(n.novelty, 0.0);
        n.validate().unwrap();

        let t = |h: f64| Timestamp::from_unix_nanos((h * 3.6e12) as i64);
        let mut r = FirstSightingRate::default();
        // Two new emitters a day while building the baseline.
        for h in 0..24 {
            r.record(t(f64::from(h)), u64::from(h % 12 == 0), 3600.0, true);
            assert!(h == 23 || r.novelty().is_none(), "immature before 24 h");
        }
        assert!(r.novelty().unwrap() < 0.1, "the usual rate is not novel");
        // Ten first sightings in the next hour: novel.
        r.record(t(24.5), 10, 3600.0, false);
        assert!(r.novelty().unwrap() > 0.9, "{:?}", r.novelty());
    }

    /// T-138: the persistent single-emitter rule's gate P(≥1 | μ) ≤ α is the 0.7 on level of two
    /// first sightings, and the single-sighting (global) score is untouched.
    #[test]
    fn novelty_persistent_single_alpha_matches_on_level() {
        let alpha = persistent_single_alpha(0.7);
        assert!((alpha - 0.011_21).abs() < 1e-4, "α = {alpha}");
        let w = FirstSightingRate::WINDOW_S;
        for mu in [1e-4, 0.005, 0.0105, 0.0112, 0.0114, 0.012, 0.05, 0.5] {
            let rate = mu / w;
            let p1 = -(-mu).exp_m1();
            assert_eq!(
                persistent_single_novelty(rate) >= 0.7,
                p1 <= alpha,
                "μ = {mu}: novelty {} vs P(≥1) {p1}",
                persistent_single_novelty(rate)
            );
            assert!(
                new_emitter_novelty(1, rate, w) < 0.7,
                "a lone sighting still tops out"
            );
        }
        // One new emitter a week, (k + 1) over 7 days: quiet. One every 2 h: busy.
        let quiet = 1.0 / (7.0 * 86_400.0);
        assert!(persistent_single_novelty(quiet) >= 0.7);
        assert!(persistent_single_novelty(1.0 / 7200.0) < 0.4);
    }

    /// Deterministic splitmix64 with a Box–Muller normal.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
        }
        fn normal(&mut self) -> f64 {
            let (u, v) = (self.next().max(1e-300), self.next());
            (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
        }
    }

    fn t_at(i: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + i * 900_000_000_000)
    }

    /// T-146: the log tail matches known values and its inverse round-trips far into the tail.
    #[test]
    fn novelty_sequential_tail_functions() {
        for (x, q) in [
            (0.0, 0.5),
            (1.959_964, 0.025),
            (3.0, 1.349_898e-3),
            (-1.0, 0.841_344_7),
        ] {
            let got = ln_normal_upper_tail(x).exp();
            assert!(((got - q) / q).abs() < 1e-6, "Q({x}) = {got} vs {q}");
        }
        // Q(7.9) ≈ 1.3954·10⁻¹⁵ and Q(12) ≈ 1.7765·10⁻³³ (Mills ratio series).
        let q79 = ln_normal_upper_tail(7.9).exp();
        assert!((q79 / 1.3954e-15 - 1.0).abs() < 1e-3, "{q79}");
        let q12 = ln_normal_upper_tail(12.0).exp();
        assert!((q12 / 1.7765e-33 - 1.0).abs() < 1e-3, "{q12}");
        for x in [-2.0, 0.3, 4.0, 11.36, 25.0] {
            let back = normal_upper_tail_inv_ln(ln_normal_upper_tail(x));
            assert!((back - x).abs() < 1e-9, "{x} → {back}");
        }
        // One interval: the sequential mapping never exceeds the single-interval one.
        let cfg = NoveltyConfig::default();
        for z in [3.5, 7.9, 9.0, 12.0] {
            assert!(sequential_novelty(1, z, &cfg) <= novelty_from_z(z, cfg.z_min, cfg.z_sat));
        }
    }

    /// Runs the engine's combination (max of the single-interval and sequential novelty) through
    /// the ADR hysteresis on i.i.d. N(0,1) z and returns (single-only, sequential-only, combined)
    /// raise counts (raise or reopen) summed over both directions, and the number of
    /// intervals at which the sequential novelty reached on.
    ///
    /// As in the engine, busier and quieter are separate alarm keys, one shared run per subject:
    /// an interval of z > 0 is a busier input only (its novelty `novelty_from_z(z)`) and steps
    /// only the busier hysteresis; a quieter key is not stepped by a busier interval
    /// ([`crate::occupancy::alarm::inputs_from_fold`]).
    fn null_run(n: usize, cfg: &NoveltyConfig, seed: u64) -> (u64, u64, u64, u64) {
        use hk_model::attention::alarm::{AlarmTransition, HysteresisConfig, HysteresisState};
        let h = HysteresisConfig::default();
        let mut rng = Rng(seed);
        let mut run = None;
        // [busier, quieter] × (single, sequential, combined).
        let mut states = [[HysteresisState::default(); 3]; 2];
        let (mut n_single, mut n_seq, mut n_both, mut seq_on) = (0, 0, 0, 0);
        let raised = |tr| matches!(tr, AlarmTransition::Raise | AlarmTransition::Reopen);
        for i in 0..n {
            let t = t_at(i as i64);
            let z = rng.normal();
            let one = novelty_from_z(z.abs(), cfg.z_min, cfg.z_sat);
            let s = sequential_step(&mut run, t, z, 0, SEQUENTIAL_MAX_GAP_S, cfg)
                .map_or(0.0, |e| e.novelty);
            seq_on += u64::from(s >= h.on);
            let [single, seq, both] = &mut states[usize::from(z < 0.0)];
            n_single += u64::from(raised(single.step(one, t, &h)));
            n_seq += u64::from(raised(seq.step(s, t, &h)));
            n_both += u64::from(raised(both.step(one.max(s), t, &h)));
        }
        (n_single, n_seq, n_both, seq_on)
    }

    /// T-146 null false-alarm Monte Carlo (ADR-0012 §7.2), rates per direction per scored
    /// interval (Q = Q(z_on)).
    ///
    /// - **Single-interval rule (the budget).** A direction's key is stepped only on intervals of
    ///   its sign, so a raise is two consecutive same-signed intervals at z_on: Q·(Q/½) = 2Q²
    ///   (slightly less: an open alarm does not re-raise).
    /// - **Sequential rule.** Its on level is reached at most at Q²/2 ([`sequential_novelty`]);
    ///   a raise needs an on interval, so its raise rate is ≤ Q²/2, within the budget.
    /// - **Combined (max) rule.** ≤ 2Q² + Q²/2 + Q³ (single-single, sequential at t, sequential
    ///   at the previous same-signed step then a single at t).
    ///
    /// At the production z_on (7.9) those rates (~10⁻³⁰) cannot be measured, so the rule is run
    /// with the same code at z_on = 2 (z_min 0, z_sat 2/0.7), Q² = 5.18·10⁻⁴, over 2·10⁶ intervals
    /// (seed fixed). Tolerances fixed before running: the single rule within [0.8·2Q², 2Q² + 5σ],
    /// the sequential on and raise rates ≤ Q²/2 + 3σ, the combined rate ≤ its bound + 3σ (σ the
    /// binomial standard deviation at the bound over 2N direction-steps). Then 10⁶ null intervals
    /// at the production mapping never reach on.
    #[test]
    fn novelty_sequential_null_false_alarm_rate_within_budget() {
        const N: usize = 2_000_000;
        let z_on = 2.0;
        let scaled = NoveltyConfig {
            z_min: 0.0,
            z_sat: z_on / 0.7,
            sigma_floor_db: 1.0,
        };
        let alpha = ln_normal_upper_tail(z_on).exp();
        let q2 = alpha * alpha;
        let (single, seq, both, seq_on) = null_run(N, &scaled, 0x7146);
        // Per direction: both directions' counts over 2N direction-intervals.
        let steps = 2.0 * N as f64;
        let rate = |c: u64| c as f64 / steps;
        let sigma = |p: f64| (p / steps).sqrt();
        let single_rate = 2.0 * q2;
        let seq_bound = q2 / 2.0;
        let both_bound = 2.0 * q2 + q2 / 2.0 + q2 * alpha;
        println!(
            "T-146 null MC at z_on {z_on} (per direction): Q² {q2:.3e}; single {:.3e} (2Q² \
             {single_rate:.3e}), sequential raise {:.3e}, on {:.3e} (bound {seq_bound:.3e}), \
             combined {:.3e} (bound {both_bound:.3e})",
            rate(single),
            rate(seq),
            rate(seq_on),
            rate(both),
        );
        assert!(
            rate(single) <= single_rate + 5.0 * sigma(single_rate)
                && rate(single) >= 0.8 * single_rate,
            "single-interval rule {} vs 2Q² {single_rate}",
            rate(single)
        );
        assert!(rate(seq_on) <= seq_bound + 3.0 * sigma(seq_bound));
        assert!(rate(seq) <= seq_bound + 3.0 * sigma(seq_bound));
        assert!(
            rate(seq) <= rate(single),
            "sequential rule within the single-interval budget"
        );
        assert!(rate(both) <= both_bound + 3.0 * sigma(both_bound));

        let (single, seq, both, seq_on) = null_run(1_000_000, &NoveltyConfig::default(), 0x7147);
        assert_eq!((single, seq, both, seq_on), (0, 0, 0, 0));
    }

    /// T-146: a run resets on a direction change, a gap beyond the horizon, a gain change and
    /// time going backwards; a re-fed interval is not counted twice.
    #[test]
    fn novelty_sequential_run_resets() {
        let cfg = NoveltyConfig::default();
        let gap = SEQUENTIAL_MAX_GAP_S;
        let mut run = None;
        for i in 0..5 {
            let e = sequential_step(&mut run, t_at(i), 4.0, 7, gap, &cfg).unwrap();
            assert_eq!(e.intervals, i as u32 + 1);
            assert!((e.z - 4.0 * f64::from(e.intervals).sqrt()).abs() < 1e-9);
        }
        let e = sequential_step(&mut run, t_at(4), 4.0, 7, gap, &cfg).unwrap();
        assert_eq!(e.intervals, 5, "the same interval is not counted twice");
        // Direction change.
        let e = sequential_step(&mut run, t_at(5), -1.0, 7, gap, &cfg).unwrap();
        assert_eq!((e.intervals, e.z), (1, -1.0));
        for i in 6..9 {
            sequential_step(&mut run, t_at(i), 4.0, 7, gap, &cfg);
        }
        assert_eq!(run.unwrap().k, 3);
        // A gap of 2 h is still one run; a longer one resets.
        let e = sequential_step(&mut run, t_at(16), 4.0, 7, gap, &cfg).unwrap();
        assert_eq!(e.intervals, 4, "8 intervals = the 2 h horizon");
        let e = sequential_step(&mut run, t_at(25), 4.0, 7, gap, &cfg).unwrap();
        assert_eq!(e.intervals, 1, "beyond the horizon");
        // Gain key change.
        sequential_step(&mut run, t_at(26), 4.0, 7, gap, &cfg);
        let e = sequential_step(&mut run, t_at(27), 4.0, 8, gap, &cfg).unwrap();
        assert_eq!(e.intervals, 1, "new gain state");
        // Backwards in time, and a zero z.
        let e = sequential_step(&mut run, t_at(20), 4.0, 8, gap, &cfg).unwrap();
        assert_eq!(e.intervals, 1);
        assert!(sequential_step(&mut run, t_at(21), 0.0, 8, gap, &cfg).is_none());
        assert!(run.is_none());
    }

    /// A pool of `slots` slots of `per_slot` 15-min intervals, each measuring FCO from `n_eff`
    /// independent looks at occupancy probability `p(slot)`, with the T-146 sampling moment.
    fn sampled_pool(
        slots: usize,
        per_slot: usize,
        n_eff: u32,
        p: impl Fn(usize) -> f64,
        rng: &mut Rng,
    ) -> PoolStats {
        use hk_model::attention::baseline::SlotStats;
        let mut pool = PoolStats::default();
        for j in 0..slots {
            let mut s = SlotStats::EMPTY;
            for _ in 0..per_slot {
                let hits = (0..n_eff).filter(|_| rng.next() < p(j)).count() as f64;
                let w = 900.0;
                s.fco_var_s =
                    SlotStats::fco_var_after(s.fco_var_s, s.weight_s, w, f64::from(n_eff));
                s.occupied_weight_s += w * hits / f64::from(n_eff);
                s.weight_s += w;
                s.observed_s += w;
            }
            pool.add(
                0.0,
                s.observed_s,
                0.0,
                0.0,
                s.occupied_weight_s,
                s.weight_s,
                0.0,
            );
            pool.add_fco_sampling(s.weight_s, s.fco_var_s);
        }
        pool
    }

    /// T-146 between-slot variance correction: sparse visits (n_eff 2 per interval) no longer
    /// inflate the reference spread, the true pattern spread is kept, and dense visits (n_eff 80)
    /// are numerically unchanged.
    #[test]
    fn novelty_between_var_sampling_correction_sparse_vs_dense() {
        let mut rng = Rng(0x0007_146b);
        let flat = |_: usize| 0.05;
        let patterned = |j: usize| if j % 2 == 0 { 0.1 } else { 0.7 };
        // Weighted spread of the true slot probabilities (0.1/0.7 alternating): 0.09.
        let tau2 = 0.09;

        let dense_flat = sampled_pool(400, 4, 80, flat, &mut rng);
        let dense_pat = sampled_pool(400, 4, 80, patterned, &mut rng);
        for (name, pool) in [("flat", &dense_flat), ("patterned", &dense_pat)] {
            let (raw, corr) = (pool.between_var_raw(), pool.between_var());
            println!("T-146 dense {name}: between raw {raw:.6}, corrected {corr:.6}");
            assert!((raw - corr).abs() < 1e-3, "dense {name}: {raw} → {corr}");
        }
        let (raw, corr) = (dense_pat.between_var_raw(), dense_pat.between_var());
        assert!(
            (raw - corr).abs() / raw < 0.02,
            "dense patterned relative change"
        );

        let sparse_flat = sampled_pool(400, 4, 2, flat, &mut rng);
        let sparse_pat = sampled_pool(400, 4, 2, patterned, &mut rng);
        let (raw, corr) = (sparse_flat.between_var_raw(), sparse_flat.between_var());
        println!("T-146 sparse flat: between raw {raw:.6}, corrected {corr:.6}");
        // Raw ≈ p(1−p)/(4·2) ≈ 0.0059, all sampling noise: corrected to near the floor.
        assert!(raw > 0.004, "{raw}");
        assert!(corr <= 0.35 * raw && corr >= BETWEEN_VAR_FLOOR_FRACTION * raw - 1e-12);
        let (raw, corr) = (sparse_pat.between_var_raw(), sparse_pat.between_var());
        println!("T-146 sparse patterned: between raw {raw:.6}, corrected {corr:.6} (τ² {tau2})");
        assert!(
            raw > tau2 + 0.015,
            "sampling inflates the raw spread: {raw}"
        );
        assert!(
            (corr - tau2).abs() < 0.015,
            "the pattern spread is kept: {corr}"
        );

        // The onset z of a sparse look (FCO 1 with n_eff 2) rises accordingly, and a patterned
        // channel's daytime FCO is still not novel.
        let look = |fco: f64, n_eff: f64| Evidence {
            level_db: None,
            fco: Some(fco),
            n_eff,
            weight_s: 900.0,
            observed_s: 900.0,
        };
        let z = occupancy_z(&sparse_flat, &look(1.0, 2.0)).unwrap();
        assert!(z > 3.4, "sparse onset z {z}");
        assert!(occupancy_z(&sparse_pat, &look(0.75, 2.0)).unwrap() < 3.0);
        assert!(occupancy_z(&dense_pat, &look(0.75, 80.0)).unwrap() < 3.0);
    }
}
