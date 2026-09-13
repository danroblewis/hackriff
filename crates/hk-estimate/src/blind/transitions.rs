//! Slicer transitions, the run-length seed and the guarded transition least squares (S5
//! `transitions`, `runlength_unit`, `rate_transitions_ls`).

use serde::{Deserialize, Serialize};

use super::util::{median, quantile};

/// Guards of the transition fit (S5 §5 T-011; see [`super::BlindConfig`]).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LsGuards {
    /// Maximum RMS timing residual, unit intervals.
    pub max_jitter_ui: f64,
    /// Minimum fraction of odd run counts (a 2×-rate seed gives ~0 %, random NRZ ~67 %).
    pub min_odd_fraction: f64,
    /// Minimum symbols spanned.
    pub min_symbols: u64,
    /// Maximum fraction of the modal run count (a sliced tone/chirp has one run length).
    pub max_modal_fraction: f64,
    /// Minimum fraction of transitions kept by the 0.3 T outlier gate.
    pub min_kept_fraction: f64,
    /// Maximum relative distance of the fitted period from the (factor-corrected) seed.
    pub max_seed_error: f64,
    /// Runs longer than this many seed periods split the burst into segments.
    pub segment_gap_periods: f64,
    /// Minimum transitions.
    pub min_transitions: usize,
}

impl Default for LsGuards {
    fn default() -> Self {
        Self {
            max_jitter_ui: 0.12,
            min_odd_fraction: 0.40,
            min_symbols: 32,
            max_modal_fraction: 0.90,
            min_kept_fraction: 0.85,
            max_seed_error: 0.02,
            segment_gap_periods: 64.0,
            min_transitions: 12,
        }
    }
}

/// Why a transition fit produced no rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitFailure {
    /// Fewer transitions than `min_transitions`.
    TooShort,
    /// Non-finite or collapsed period.
    DegenerateFit,
}

/// One guarded transition least-squares fit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransitionFit {
    /// Seed rate, Bd.
    pub seed_bd: f64,
    /// Fitted rate `fs / T`, Bd (`None` on failure).
    pub rate_bd: Option<f64>,
    /// One-sigma rate uncertainty from the timing residuals, Bd.
    pub sigma_bd: Option<f64>,
    /// All guards passed.
    pub ok: bool,
    /// Why no rate (when `rate_bd` is `None`).
    pub failure: Option<FitFailure>,
    /// RMS timing residual, unit intervals.
    pub jitter_ui: f64,
    /// Fraction of odd run counts.
    pub odd_fraction: f64,
    /// Fraction of transitions within 0.3 T of the lattice.
    pub kept_fraction: f64,
    /// Fraction of the modal run count.
    pub modal_fraction: f64,
    /// Transitions used.
    pub transitions: usize,
    /// Symbols spanned (after dividing out the common factor).
    pub symbols: u64,
    /// Segments (runs longer than the gap split the burst).
    pub segments: usize,
    /// Common lattice factor divided out (1 = none): the fitted clock was `factor` × too fast.
    pub factor: u32,
    /// A coarser lattice (`k` × the reported period) held most, but not clearly all, kept
    /// transitions: the period may still be a sub-multiple of the clock, so the fit is not ok.
    pub ambiguous_factor: Option<u32>,
    /// Fitted period, samples.
    pub period_samples: f64,
    /// Per-segment lattice offset (mean of the rising and falling offsets present), samples.
    pub segment_offsets: Vec<f64>,
    /// Per-segment `[first, last]` transition time, samples.
    pub segment_spans: Vec<(f64, f64)>,
}

impl TransitionFit {
    fn failed(seed_bd: f64, failure: FitFailure, transitions: usize) -> Self {
        Self {
            seed_bd,
            rate_bd: None,
            sigma_bd: None,
            ok: false,
            failure: Some(failure),
            jitter_ui: f64::NAN,
            odd_fraction: 0.0,
            kept_fraction: 0.0,
            modal_fraction: 1.0,
            transitions,
            symbols: 0,
            segments: 0,
            factor: 1,
            ambiguous_factor: None,
            period_samples: f64::NAN,
            segment_offsets: Vec::new(),
            segment_spans: Vec::new(),
        }
    }
}

/// Fractional sample positions where `v` crosses `thr` (linear interpolation).
pub fn transitions(v: &[f64], thr: f64) -> Vec<f64> {
    let mut out = Vec::new();
    for i in 0..v.len().saturating_sub(1) {
        if (v[i] > thr) != (v[i + 1] > thr) {
            let a = v[i] - thr;
            let b = v[i + 1] - thr;
            let den = a - b;
            let frac = if den != 0.0 { a / den } else { 0.5 };
            out.push(i as f64 + frac);
        }
    }
    out
}

fn valid_at(valid: Option<&[bool]>, t: f64) -> bool {
    match valid {
        None => true,
        Some([]) => true,
        Some(m) => m[(t.max(0.0) as usize).min(m.len() - 1)],
    }
}

/// rtl_433 `-A` style seed: the median of the runs no longer than 1.5 × their 10th percentile
/// (runs shorter than `0.4·fs/f_max` dropped). `None` with fewer than 8 runs.
pub fn runlength_unit(
    v: &[f64],
    fs: f64,
    thr: f64,
    f_max: f64,
    valid: Option<&[bool]>,
) -> Option<f64> {
    let tau: Vec<f64> = transitions(v, thr)
        .into_iter()
        .filter(|&t| valid_at(valid, t))
        .collect();
    let min_run = 0.4 * fs / f_max;
    let runs: Vec<f64> = tau
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|&r| r > min_run)
        .collect();
    if runs.len() < 8 {
        return None;
    }
    let p10 = quantile(&runs, 0.10);
    let short: Vec<f64> = runs.iter().copied().filter(|&r| r <= 1.5 * p10).collect();
    let t = median(&short);
    (t > 0.0).then(|| fs / t)
}

/// Largest common lattice factor searched (T-030: rectangular FSK h = 4–6 seeds at 7–9 ×).
const MAX_FACTOR: u64 = 32;
/// Share of the kept transitions on a coarser lattice that divides it out.
const FACTOR_FRACTION: f64 = 0.9;
/// Share above which a coarser lattice that does not divide out makes the fit ambiguous. Random
/// data at the true clock puts ~½ (k = 2) or less on any one residue.
const AMBIGUOUS_FRACTION: f64 = 0.75;
/// Minimum kept transitions of a (segment, polarity) group for its residue to count.
const MIN_GROUP: usize = 4;

/// How well the kept transitions sit on the lattice `k` × the fitted period: per (segment,
/// polarity) group the modal residue of the lattice index mod `k`; the score is the share of the
/// counted transitions on their group's mode. Rising and falling residues of one segment must be
/// within a quarter of the coarse period of each other (slicer asymmetry); otherwise the coarse
/// lattice is not a clock (a 0101 run at k = 2 puts rises and falls half a period apart) and the
/// score is 0. `None` with fewer than 8 counted transitions. Also returns each group's modal
/// residue.
fn lattice_score(
    n: &[f64],
    group: &[usize],
    ng: usize,
    used: &[bool],
    k: u64,
) -> Option<(f64, Vec<u64>)> {
    let ku = k as usize;
    let mut hist = vec![0usize; ng * ku];
    for i in (0..n.len()).filter(|&i| used[i]) {
        hist[group[i] * ku + (n[i] as u64 % k) as usize] += 1;
    }
    let (mut hit, mut total) = (0usize, 0usize);
    let mut residue = vec![0u64; ng];
    let mut counted = vec![false; ng];
    for g in 0..ng {
        let h = &hist[g * ku..(g + 1) * ku];
        let c: usize = h.iter().sum();
        let (r, m) = h
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(&a.0)))
            .map_or((0, 0), |(r, &m)| (r, m));
        residue[g] = r as u64;
        if c >= MIN_GROUP {
            counted[g] = true;
            hit += m;
            total += c;
        }
    }
    if total < 8 {
        return None;
    }
    for s in 0..ng / 2 {
        if counted[2 * s] && counted[2 * s + 1] {
            let d = (residue[2 * s] + k - residue[2 * s + 1]) % k;
            if 4 * d.min(k - d) > k {
                return Some((0.0, residue));
            }
        }
    }
    Some((hit as f64 / total as f64, residue))
}

/// Fixed-effects least squares `τ_i = a_{g(i)} + T·n_i`. Returns `(T, a_g)`.
fn fit(
    tau: &[f64],
    n: &[f64],
    group: &[usize],
    ng: usize,
    keep: Option<&[bool]>,
) -> Option<(f64, Vec<f64>)> {
    let mut c = vec![0.0f64; ng];
    let mut sn = vec![0.0f64; ng];
    let mut st = vec![0.0f64; ng];
    let mut snn = vec![0.0f64; ng];
    let mut snt = vec![0.0f64; ng];
    for i in 0..tau.len() {
        if keep.is_some_and(|k| !k[i]) {
            continue;
        }
        let g = group[i];
        c[g] += 1.0;
        sn[g] += n[i];
        st[g] += tau[i];
        snn[g] += n[i] * n[i];
        snt[g] += n[i] * tau[i];
    }
    let (mut num, mut den) = (0.0, 0.0);
    for g in 0..ng {
        if c[g] > 0.0 {
            num += snt[g] - sn[g] * st[g] / c[g];
            den += snn[g] - sn[g] * sn[g] / c[g];
        }
    }
    if den <= 0.0 {
        return None;
    }
    let t = num / den;
    let a = (0..ng)
        .map(|g| {
            if c[g] > 0.0 {
                (st[g] - t * sn[g]) / c[g]
            } else {
                f64::NAN
            }
        })
        .collect();
    Some((t, a))
}

/// Refines a clock period seeded at `t0` samples from the transitions of `v` through `thr`.
///
/// Transitions outside `valid` are dropped, as are transitions closer than 0.4·T0 to the
/// previous one. Runs longer than `segment_gap_periods`·T0 split the burst into segments; each
/// segment has its own **rising and falling** offsets (slicer and pulse-shape asymmetry would
/// otherwise let a non-integer sub-multiple of the clock fit) and all share T. Integer run
/// counts `max(1, round(run/T))`, least squares, transitions beyond 0.3 T dropped, iterated five
/// times. The largest common lattice factor k ∈ 32..2 holding > 90 % of the kept transitions is
/// divided out (a seed at k× the rate lands on the rate, T-030); a coarser lattice holding > 75 %
/// that does not divide out sets `ambiguous_factor`. `ok` iff every [`LsGuards`] guard holds and
/// the factor is unambiguous.
pub fn rate_transitions_ls(
    v: &[f64],
    fs: f64,
    thr: f64,
    t0: f64,
    valid: Option<&[bool]>,
    g: &LsGuards,
) -> TransitionFit {
    let seed_bd = fs / t0;
    let mut tau = Vec::new();
    let mut rising = Vec::new();
    for t in transitions(v, thr) {
        if !valid_at(valid, t) {
            continue;
        }
        let r = v[((t as usize) + 1).min(v.len() - 1)] > thr;
        if let Some(&last) = tau.last() {
            if t - last <= 0.4 * t0 {
                continue;
            }
        }
        tau.push(t);
        rising.push(r);
    }
    if tau.len() < g.min_transitions {
        return TransitionFit::failed(seed_bd, FitFailure::TooShort, tau.len());
    }
    let gap = g.segment_gap_periods * t0;
    let runs: Vec<f64> = tau.windows(2).map(|w| w[1] - w[0]).collect();
    let mut seg = vec![0usize; tau.len()];
    for i in 1..tau.len() {
        seg[i] = seg[i - 1] + usize::from(runs[i - 1] > gap);
    }
    let nseg = seg[tau.len() - 1] + 1;
    let group: Vec<usize> = seg
        .iter()
        .zip(&rising)
        .map(|(&s, &r)| 2 * s + usize::from(r))
        .collect();
    let ng = 2 * nseg;
    let mut t = t0;
    let mut inc = vec![0.0f64; runs.len()];
    let mut n = vec![0.0f64; tau.len()];
    let mut res: Vec<f64> = Vec::new();
    let mut kept_frac = 1.0;
    let mut offsets = vec![f64::NAN; ng];
    let mut n_kept_for_sigma = 0usize;
    let mut sxx = 0.0;
    let mut used_last = vec![true; tau.len()];
    for _ in 0..5 {
        for (k, &r) in runs.iter().enumerate() {
            inc[k] = if r > gap {
                0.0
            } else {
                (r / t).round().max(1.0)
            };
        }
        n[0] = 0.0;
        for k in 0..runs.len() {
            n[k + 1] = n[k] + inc[k];
        }
        let Some((t1, a1)) = fit(&tau, &n, &group, ng, None) else {
            return TransitionFit::failed(seed_bd, FitFailure::DegenerateFit, tau.len());
        };
        let resid = |tt: f64, a: &[f64], i: usize| tau[i] - a[group[i]] - tt * n[i];
        let keep: Vec<bool> = (0..tau.len())
            .map(|i| resid(t1, &a1, i).abs() < 0.3 * t1.abs().max(1e-9))
            .collect();
        let nk = keep.iter().filter(|&&k| k).count();
        kept_frac = nk as f64 / tau.len() as f64;
        let (tt, aa, used): (f64, Vec<f64>, Vec<bool>) = if nk >= g.min_transitions {
            match fit(&tau, &n, &group, ng, Some(&keep)) {
                Some((tt, aa)) => (tt, aa, keep),
                None => (t1, a1, vec![true; tau.len()]),
            }
        } else {
            (t1, a1, vec![true; tau.len()])
        };
        res = (0..tau.len())
            .filter(|&i| used[i])
            .map(|i| resid(tt, &aa, i))
            .collect();
        // Σ (n − n̄_g)² over the used transitions, for the slope's standard error.
        let mut cg = vec![(0.0f64, 0.0f64, 0.0f64); ng];
        for i in (0..tau.len()).filter(|&i| used[i]) {
            let e = &mut cg[group[i]];
            e.0 += 1.0;
            e.1 += n[i];
            e.2 += n[i] * n[i];
        }
        sxx = cg
            .iter()
            .filter(|e| e.0 > 0.0)
            .map(|e| e.2 - e.1 * e.1 / e.0)
            .sum();
        n_kept_for_sigma = res.len();
        used_last = used;
        t = tt;
        offsets = aa;
        if !t.is_finite() || t <= 0.25 * t0 || n[tau.len() - 1] < 2.0 {
            return TransitionFit::failed(seed_bd, FitFailure::DegenerateFit, tau.len());
        }
    }
    let mut counted: Vec<u64> = inc
        .iter()
        .filter(|&&c| c > 0.0)
        .map(|&c| c as u64)
        .collect();
    let mut nsym = n[tau.len() - 1] as u64;
    // Common lattice factor (T-030). The fit holds on any sub-multiple T/k of the clock, and a
    // prime k ≥ 7 keeps ~⅔ odd run counts, so every other guard passes. Search k from the largest
    // plausible (coarse runs ≥ ½ period for > 90 % of runs, k ≤ 32) down and divide out the first
    // lattice holding > 90 % of the kept transitions; a coarser lattice holding > 75 % that does
    // not divide out leaves the fit ambiguous (not ok).
    let mut factor = 1u32;
    let mut ambiguous_factor = None;
    let mut residue = Vec::new();
    if counted.len() >= 8 {
        let mut sorted = counted.clone();
        sorted.sort_unstable();
        let kmax = (2 * sorted[sorted.len() / 2]).min(MAX_FACTOR);
        for k in (2..=kmax).rev() {
            let long = counted.iter().filter(|&&c| 2 * c >= k).count() as f64;
            if long <= FACTOR_FRACTION * counted.len() as f64 {
                continue;
            }
            let Some((score, r)) = lattice_score(&n, &group, ng, &used_last, k) else {
                continue;
            };
            if score > FACTOR_FRACTION {
                factor = k as u32;
                residue = r;
                break;
            }
            if score > AMBIGUOUS_FRACTION && ambiguous_factor.is_none() {
                ambiguous_factor = Some(k as u32);
            }
        }
    }
    let t_unit = t;
    if factor > 1 {
        let k = u64::from(factor);
        t *= f64::from(factor);
        // Each group's lattice point moves to its modal residue (rising and falling residues may
        // differ by up to a quarter coarse period).
        for (a, &r) in offsets.iter_mut().zip(&residue) {
            *a += r as f64 * t_unit;
        }
        counted
            .iter_mut()
            .for_each(|c| *c = ((*c as f64 / k as f64).round() as u64).max(1));
        nsym = counted.iter().sum();
    }
    let odd = if counted.is_empty() {
        0.0
    } else {
        counted.iter().filter(|&&c| c % 2 == 1).count() as f64 / counted.len() as f64
    };
    let jitter = super::util::std(&res) / t;
    let modal = if counted.is_empty() {
        1.0
    } else {
        let max = *counted.iter().max().unwrap_or(&0) as usize;
        let mut bins = vec![0usize; max + 1];
        for &c in &counted {
            bins[c as usize] += 1;
        }
        *bins.iter().max().unwrap_or(&0) as f64 / counted.len() as f64
    };
    let ok = jitter < g.max_jitter_ui
        && odd > g.min_odd_fraction
        && (t / (t0 * f64::from(factor)) - 1.0).abs() < g.max_seed_error
        && nsym >= g.min_symbols
        && modal <= g.max_modal_fraction
        && kept_frac >= g.min_kept_fraction
        && ambiguous_factor.is_none();
    // Slope standard error (in the pre-factor unit), scaled to the final period.
    let dof = (n_kept_for_sigma as f64 - ng as f64 - 1.0).max(1.0);
    let rss: f64 = res.iter().map(|r| r * r).sum();
    let sigma_t = if sxx > 0.0 {
        (rss / dof / sxx).sqrt() * f64::from(factor)
    } else {
        t_unit
    };
    let mut segment_offsets = Vec::with_capacity(nseg);
    let mut segment_spans = Vec::with_capacity(nseg);
    for s in 0..nseg {
        let pair = [offsets[2 * s], offsets[2 * s + 1]];
        let present: Vec<f64> = pair.into_iter().filter(|v| v.is_finite()).collect();
        segment_offsets.push(if present.is_empty() {
            f64::NAN
        } else {
            present.iter().sum::<f64>() / present.len() as f64
        });
        let idx: Vec<usize> = (0..tau.len()).filter(|&i| seg[i] == s).collect();
        segment_spans.push((tau[idx[0]], tau[*idx.last().unwrap_or(&idx[0])]));
    }
    TransitionFit {
        seed_bd,
        rate_bd: Some(fs / t),
        sigma_bd: Some(fs * sigma_t / (t * t)),
        ok,
        failure: None,
        jitter_ui: jitter,
        odd_fraction: odd,
        kept_fraction: kept_frac,
        modal_fraction: modal,
        transitions: tau.len(),
        symbols: nsym,
        segments: nseg,
        factor,
        ambiguous_factor,
        period_samples: t,
        segment_offsets,
        segment_spans,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A band-limited NRZ level sequence at `sps` samples per bit.
    fn nrz(bits: &[u8], sps: f64) -> Vec<f64> {
        let n = (bits.len() as f64 * sps) as usize;
        let raw: Vec<f64> = (0..n)
            .map(|i| {
                if bits[(i as f64 / sps) as usize] == 1 {
                    1.0
                } else {
                    0.0
                }
            })
            .collect();
        super::super::util::moving_avg(&raw, (sps / 4.0) as usize)
    }

    fn bits(seed: u64, n: usize) -> Vec<u8> {
        let mut rng = hk_dsp::synth::Rng::new(seed);
        (0..n).map(|_| (rng.next_u64() & 1) as u8).collect()
    }

    #[test]
    fn fits_the_true_clock_and_divides_out_a_multiple_seed() {
        let sps = 25.3;
        let v = nrz(&bits(1, 300), sps);
        let g = LsGuards::default();
        let f = rate_transitions_ls(&v, 1000.0, 0.5, sps * 1.01, None, &g);
        assert!(f.ok, "{f:?}");
        assert!((f.period_samples / sps - 1.0).abs() < 2e-3, "{f:?}");
        // Seeded at 3× the rate: run counts are multiples of 3, factor divides out.
        let f3 = rate_transitions_ls(&v, 1000.0, 0.5, sps / 3.0, None, &g);
        assert_eq!(f3.factor, 3, "{f3:?}");
        assert!(
            f3.ok && (f3.period_samples / sps - 1.0).abs() < 2e-3,
            "{f3:?}"
        );
    }

    /// S5 pitfall 5: outlier rejection must not hide a wrong clock. A 2.5× seed "fits" the
    /// transitions it keeps; the kept-fraction, odd-run and factor guards refuse it.
    #[test]
    fn pitfall5_outlier_rejection_does_not_hide_a_wrong_clock() {
        let sps = 40.0;
        let v = nrz(&bits(7, 400), sps);
        let g = LsGuards::default();
        let wrong = rate_transitions_ls(&v, 1000.0, 0.5, sps / 2.5, None, &g);
        assert!(!wrong.ok, "2.5× seed accepted: {wrong:?}");
        let loose = LsGuards {
            min_kept_fraction: 0.0,
            min_odd_fraction: 0.0,
            ..g
        };
        let hidden = rate_transitions_ls(&v, 1000.0, 0.5, sps / 2.5, None, &loose);
        eprintln!("2.5× seed with the guards: {wrong:?}\nwithout: {hidden:?}");
        let right = rate_transitions_ls(&v, 1000.0, 0.5, sps, None, &g);
        assert!(right.ok, "{right:?}");
    }

    /// T-030: seeds at prime (7×) and composite (9×) multiples of the rate divide out to the clock.
    /// Before, only k ∈ 5..2 was tried: a 7× seed stayed at 7× with ~⅔ odd runs (ok), and a 9×
    /// seed divided by 3 stayed at 3× (ok).
    #[test]
    fn t030_high_multiple_seeds_divide_out_to_the_clock() {
        let sps = 63.7;
        let v = nrz(&bits(5, 400), sps);
        let g = LsGuards::default();
        for k in [6u32, 7, 9, 11] {
            let f = rate_transitions_ls(&v, 1000.0, 0.5, sps / f64::from(k), None, &g);
            assert_eq!(f.factor, k, "{k}× seed: {f:?}");
            assert!(
                f.ok && (f.period_samples / sps - 1.0).abs() < 2e-3,
                "{k}× seed: {f:?}"
            );
        }
    }

    /// T-030: slicer asymmetry (rising and falling crossings a sub-period apart at the seed's
    /// lattice) must not stop the factor dividing out.
    #[test]
    fn t030_factor_survives_rise_fall_asymmetry() {
        let sps = 70.0;
        let raw: Vec<f64> = bits(9, 400)
            .iter()
            .flat_map(|&b| std::iter::repeat_n(f64::from(b), sps as usize))
            .collect();
        // A long edge sliced low: rises cross early, falls late (≈ 0.2 T apart).
        let v = super::super::util::moving_avg(&raw, 20);
        let g = LsGuards::default();
        let f = rate_transitions_ls(&v, 1000.0, 0.15, sps / 7.0, None, &g);
        assert_eq!(f.factor, 7, "{f:?}");
        assert!(f.ok && (f.period_samples / sps - 1.0).abs() < 2e-3, "{f:?}");
    }

    #[test]
    fn runlength_seed_near_the_unit() {
        let sps = 12.5;
        let v = nrz(&bits(3, 200), sps);
        let r = runlength_unit(&v, 1000.0, 0.5, 400.0, None).unwrap();
        assert!((r / (1000.0 / sps) - 1.0).abs() < 0.1, "{r}");
    }
}
