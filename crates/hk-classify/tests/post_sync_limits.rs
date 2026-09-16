//! T-286: what the post-sync stage ADR-0016 §4.5–4.6 names can and cannot measure, and the
//! carrier-presence rule the analog half turns on.
//!
//! T-248 left an open-set residue of 59 of 396 — VSB-AM 29, π/4-DQPSK 15, 16-APSK 13 — and
//! concluded that separating 16-APSK from 16-QAM is an amplitude-ring question and π/4-DQPSK from
//! QPSK a differential-phase one, both **post-sync constellation measurements**, and so the
//! verifier's and the per-family DL stage's job rather than the feature tree's.
//!
//! The analog half of that was a defect and is fixed (`features::CARRIER_GUARD_BINS`,
//! `features::CARRIER_MIN_FRACTION`). **The psk-qam half is a capability gap, and this module is
//! the measurement that establishes it.** Three findings, all reproduced below:
//!
//! 1. **There is no clock to be post-sync of.** C14 reports a *trusted* symbol rate on 0 of 36
//!    16-APSK snippets, and on 0 of 36 genuine **8-PSK** ones. The verifier's `NoClockLock` arm is
//!    therefore not a tuning choice that could be relaxed — for 16-APSK there is no symbol timing
//!    at all, so nothing post-sync can run on 13 of the 59.
//! 2. **Where a clock does lock, the recovered cloud is not a constellation.** π/4-DQPSK locks
//!    32–36 of 36, so the stage *can* run — but it has nothing to measure. Recovering symbols
//!    exactly as `crate::verify` does (RRC matched filter, Oerder–Meyr timing, M-th-power phase),
//!    **genuine QPSK** reports a differential-phase alphabet of 0.488 ± 0.307 when its true value
//!    is 0.0, and π/4-DQPSK 0.520 ± 0.297 when its true value is 1.0. The statistic that would
//!    separate them does not separate them, because it cannot see either one.
//! 3. **Every class fits `qam64` best**, genuine BPSK and QPSK included, and genuine QPSK's own
//!    residual is 7.6 N₀ where a synchronised member sits near 1. This is T-246's open issue — the
//!    psk-qam ALRT being monotone in constellation size — reproduced in a **pure distance test**
//!    with no likelihood in it, which locates the cause in the smeared input rather than in the
//!    ALRT's derivation. A denser constellation always has a nearer point to a smeared cloud.
//!
//! The mechanism behind 2 and 3: an M-th-power estimate removes a constant phase but **not** a
//! residual carrier *frequency* offset, which rotates the constellation through the record and
//! smears it into a ring. Closing this needs genuine carrier recovery (a frequency-locked or
//! decision-directed loop) at C14's geometry — new DSP, not a threshold, exactly as T-248 concluded
//! for the swept-carrier case.
//!
//! Reported, never asserted — the gate is `tests/e2e/tests/acceptance/m3_grid.rs`. The one
//! exception is [`t286_symmetry_abstains_where_there_is_no_carrier`], which guards the rule the
//! analog fix rests on.

use hk_classify::symbols::SymbolEstimator;
use hk_classify::synth::{ACCEPTANCE_SEED_BASE, Class, SynthConfig, generate};
use hk_classify::{
    Classifier, ClassifyRequest, FeatureInput, VerifyInput, VerifyOutcome, features, verify,
};
use hk_model::Timestamp;
use num_complex::Complex64;

const ALPHA_GRID: [f64; 3] = [0.2, 0.35, 0.5];
const MIN_SYMBOLS: usize = 48;
const MAX_SYMBOLS: usize = 256;

// ---------------------------------------------------------------------------------------------
// The analog half: `symmetry` is defined only where there is a carrier.
// ---------------------------------------------------------------------------------------------

/// **The rule the VSB-AM fix rests on**, asserted rather than trusted.
///
/// `symmetry` is sideband balance *about a carrier*. Measuring it where there is none reports the
/// noise floor's asymmetry — `wfm` σ 0.533, `ssb` σ 0.636 over a feature bounded to ±1 — and a
/// dimension that is noise for four of the five analog classes cost known-family top-1 0.9067 →
/// 0.8988 when it was left in. Abstaining instead restored it to 0.9087.
///
/// The emission this must keep measurable is the held-out VSB-AM (carrier fraction 0.97–0.99): it
/// is rejected from `am` at z ≈ −18 on this dimension and on no other, so an abstention here would
/// silently undo the whole open-set gain.
#[test]
fn t286_symmetry_abstains_where_there_is_no_carrier() {
    let measured = |c: Class, snr: f64, seed: u64| -> Option<f64> {
        let s = generate(
            c,
            &SynthConfig::new(snr, ACCEPTANCE_SEED_BASE + 990_000 + seed),
        );
        features(&FeatureInput {
            samples: &s.samples,
            sample_rate_hz: s.sample_rate_hz,
            obw_hz: Some(s.obw_hz),
            snr_db: Some(snr),
            symbols: None,
        })
        .get("symmetry")
    };
    for (class, want) in [
        // A carrier to measure about: AM, a keyed carrier, and the vestigial-sideband negative.
        (Class::Am, true),
        (Class::Cw, true),
        (Class::VsbAm, true),
        // No carrier: the quantity is undefined and the feature must abstain.
        (Class::Wfm, false),
        (Class::Nbfm, false),
        (Class::DsbSc, false),
    ] {
        let present = (0..8u64)
            .filter(|s| measured(class, 25.0, *s).is_some())
            .count();
        if want {
            assert!(
                present >= 7,
                "{}: symmetry measured on only {present}/8 — the carrier-fraction gate is \
                 rejecting an emission that has a carrier",
                class.label()
            );
        } else {
            assert!(
                present <= 1,
                "{}: symmetry measured on {present}/8 snippets that have no carrier — it is \
                 reporting noise, which is the T-286 defect",
                class.label()
            );
        }
    }
    // The separation itself: VSB-AM keeps one sideband, AM keeps both.
    let am: Vec<f64> = (0..8)
        .filter_map(|s| measured(Class::Am, 25.0, s))
        .collect();
    let vsb: Vec<f64> = (0..8)
        .filter_map(|s| measured(Class::VsbAm, 25.0, s))
        .collect();
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    assert!(
        mean(&am).abs() < 0.2,
        "am is double-sideband by construction, so its symmetry is 0; measured {:.3}",
        mean(&am)
    );
    assert!(
        mean(&vsb) < -0.4,
        "VSB-AM keeps one sideband; measured {:.3}",
        mean(&vsb)
    );
}

// ---------------------------------------------------------------------------------------------
// The psk-qam half: symbol recovery exactly as `crate::verify` does it.
// ---------------------------------------------------------------------------------------------

fn rrc(sps: f64, alpha: f64, span: usize) -> Vec<f64> {
    let half = (sps * span as f64).round() as isize;
    (-half..=half)
        .map(|i| {
            let t = i as f64 / sps;
            if t.abs() < 1e-9 {
                return 1.0 - alpha + 4.0 * alpha / std::f64::consts::PI;
            }
            let denom = 1.0 - (4.0 * alpha * t).powi(2);
            if denom.abs() < 1e-9 {
                let a = std::f64::consts::PI / (4.0 * alpha);
                return alpha / 2.0_f64.sqrt()
                    * ((1.0 + 2.0 / std::f64::consts::PI) * a.sin()
                        + (1.0 - 2.0 / std::f64::consts::PI) * a.cos());
            }
            let pt = std::f64::consts::PI * t;
            ((pt * (1.0 - alpha)).sin() + 4.0 * alpha * t * (pt * (1.0 + alpha)).cos())
                / (pt * denom)
        })
        .collect()
}

fn convolve(x: &[Complex64], taps: &[f64]) -> Vec<Complex64> {
    let d = taps.len() / 2;
    (0..x.len())
        .map(|i| {
            let mut acc = Complex64::new(0.0, 0.0);
            for (m, w) in taps.iter().enumerate() {
                let j = i as isize + d as isize - m as isize;
                if j >= 0 && (j as usize) < x.len() {
                    acc += x[j as usize] * *w;
                }
            }
            acc
        })
        .collect()
}

fn timing_line(x: &[Complex64], sps: f64) -> Complex64 {
    let mut acc = Complex64::new(0.0, 0.0);
    for (n, s) in x.iter().enumerate() {
        let a = -std::f64::consts::TAU * n as f64 / sps;
        acc += Complex64::new(a.cos(), a.sin()) * s.norm_sqr();
    }
    acc / x.len() as f64
}

fn symbol_samples(x: &[Complex64], sps: f64) -> Option<Vec<Complex64>> {
    let (filtered, line) = ALPHA_GRID
        .iter()
        .map(|a| {
            let y = convolve(x, &rrc(sps, *a, 6));
            let l = timing_line(&y, sps);
            (y, l)
        })
        .max_by(|a, b| a.1.norm().total_cmp(&b.1.norm()))?;
    let tau = -line.arg() / std::f64::consts::TAU * sps;
    let guard = (2.0 * sps).ceil() as usize;
    let mut out = Vec::new();
    let mut k = 0usize;
    loop {
        let t = tau + k as f64 * sps + guard as f64;
        if t + 1.0 >= filtered.len() as f64 - guard as f64 {
            break;
        }
        let i = t.floor().max(0.0) as usize;
        let frac = t - i as f64;
        out.push(filtered[i] * (1.0 - frac) + filtered[i + 1] * frac);
        k += 1;
    }
    if out.len() < MIN_SYMBOLS {
        return None;
    }
    let p = out.iter().map(Complex64::norm_sqr).sum::<f64>() / out.len() as f64;
    (p.is_finite() && p > 0.0).then(|| {
        let g = 1.0 / p.sqrt();
        out.into_iter().map(|s| s * g).collect()
    })
}

fn constellation(label: &str) -> Option<Vec<Complex64>> {
    let psk = |m: usize| {
        (0..m)
            .map(|k| {
                let a = std::f64::consts::TAU * k as f64 / m as f64;
                Complex64::new(a.cos(), a.sin())
            })
            .collect::<Vec<_>>()
    };
    let qam = |side: i32| {
        let levels: Vec<f64> = (0..side).map(|i| f64::from(2 * i + 1 - side)).collect();
        let raw: Vec<Complex64> = levels
            .iter()
            .flat_map(|i| levels.iter().map(move |q| Complex64::new(*i, *q)))
            .collect();
        let e = raw.iter().map(Complex64::norm_sqr).sum::<f64>() / raw.len() as f64;
        raw.into_iter().map(|s| s / e.sqrt()).collect::<Vec<_>>()
    };
    Some(match label {
        "bpsk" => psk(2),
        "qpsk" => psk(4),
        "8psk" => psk(8),
        "qam16" => qam(4),
        "qam64" => qam(8),
        _ => return None,
    })
}

/// Mean squared distance to the nearest point of `label`'s constellation, in units of N₀, after the
/// same M-th-power phase alignment the verifier uses. A synchronised genuine member sits near 1.
fn residual(y: &[Complex64], label: &str, n0: f64) -> Option<f64> {
    let points = constellation(label)?;
    let m = points.len();
    let order = if m == 2 || m == 4 || m == 8 { m } else { 4 };
    let offset = if m == 2 || m == 4 || m == 8 {
        0.0
    } else {
        std::f64::consts::PI
    };
    let sum: Complex64 = y.iter().map(|s| s.powu(order as u32)).sum();
    let theta = if sum.norm() > 0.0 {
        (sum.arg() - offset) / order as f64
    } else {
        0.0
    };
    let rot = Complex64::new(theta.cos(), -theta.sin());
    let scale = (1.0 - n0).max(0.1).sqrt();
    let acc: f64 = y
        .iter()
        .map(|s| {
            let r = *s * rot;
            points
                .iter()
                .map(|p| (r - *p * scale).norm_sqr())
                .fold(f64::INFINITY, f64::min)
        })
        .sum();
    Some(acc / y.len() as f64 / n0)
}

/// Fraction of symbol-to-symbol phase steps nearest an **odd** multiple of π/4. QPSK steps by
/// multiples of π/2, so the truth is 0.0; π/4-DQPSK steps by ±π/4 or ±3π/4 only, so the truth is
/// 1.0. This is the statistic that would separate them, and it is the one measured to be blind.
fn odd_step_fraction(y: &[Complex64]) -> f64 {
    let mut odd = 0usize;
    for w in y.windows(2) {
        let d = (w[1] * w[0].conj()).arg();
        if ((d / std::f64::consts::FRAC_PI_4).round() as i64).rem_euclid(2) == 1 {
            odd += 1;
        }
    }
    odd as f64 / y.len().saturating_sub(1).max(1) as f64
}

/// Share of symbols at a radius within 15 % of the cloud's mean: the amplitude-ring question.
/// Square 16-QAM puts 8 of its 16 points on the middle ring; 16-APSK has two rings and nothing
/// between them.
fn mid_ring_fraction(y: &[Complex64]) -> f64 {
    let r: Vec<f64> = y.iter().map(|s| s.norm()).collect();
    let mean = r.iter().sum::<f64>() / r.len() as f64;
    if mean <= 0.0 {
        return f64::NAN;
    }
    r.iter()
        .filter(|v| ((*v / mean) - 1.0).abs() < 0.15)
        .count() as f64
        / r.len() as f64
}

fn stat(v: &[f64]) -> String {
    if v.is_empty() {
        return "n/a".to_owned();
    }
    let m = v.iter().sum::<f64>() / v.len() as f64;
    let sd = (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
    format!("{m:6.3}+-{sd:5.3}")
}

/// **Finding 1**: for 16-APSK there is no symbol clock, so no post-sync stage can run at all.
#[test]
fn t286_the_post_sync_stage_has_no_clock_on_16_apsk() {
    let classifier = Classifier::new();
    let mut c14 = SymbolEstimator::new();
    eprintln!("\n[T-286] post-sync preconditions, 12 seeds per class at 20/25/30 dB");
    for class in [
        Class::Qpsk,
        Class::Psk8,
        Class::Qam16,
        Class::Qam64,
        Class::Pi4Dqpsk,
        Class::Apsk16,
    ] {
        let (mut locked, mut total, mut ran) = (0u32, 0u32, 0u32);
        let mut skips: std::collections::BTreeMap<&str, u32> = Default::default();
        for offset in [0.0_f64, 5.0, 10.0] {
            for seed in 0..12u64 {
                let snr = 20.0 + offset;
                let s = generate(
                    class,
                    &SynthConfig::new(snr, ACCEPTANCE_SEED_BASE + 970_000 + seed),
                );
                let symbols = c14.from_samples(
                    &s.symbol_samples,
                    s.symbol_sample_rate_hz,
                    Some(s.obw_hz),
                    Some(snr),
                );
                total += 1;
                if symbols.as_ref().is_some_and(|x| x.rate_trusted()) {
                    locked += 1;
                }
                let mut req =
                    ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
                req.obw_hz = Some(s.obw_hz);
                req.snr_db = Some(snr);
                req.symbols = symbols.as_ref();
                let mut c = classifier.classify(&req);
                match verify(
                    &mut c,
                    &VerifyInput {
                        samples: &s.symbol_samples,
                        sample_rate_hz: s.symbol_sample_rate_hz,
                        symbols: symbols.as_ref(),
                        snr_db: Some(snr),
                    },
                ) {
                    VerifyOutcome::Ran { .. } => ran += 1,
                    VerifyOutcome::Skipped(r) => *skips.entry(r.as_str()).or_default() += 1,
                }
            }
        }
        eprintln!(
            "[T-286] {:<20} trusted clock {locked:>2}/{total}  verifier ran {ran:>2}  {skips:?}",
            class.label().replace("held-out:", "")
        );
    }
}

/// **Findings 2 and 3**: where a clock does lock, the recovered cloud is not a constellation.
#[test]
fn t286_the_recovered_constellation_cannot_separate_the_residue() {
    let mut c14 = SymbolEstimator::new();
    eprintln!("\n[T-286] post-sync statistics over 12 seeds x 20/25/30 dB");
    eprintln!(
        "[T-286] resid = best listed-constellation residual / N0 (truth ~1 for a genuine member)"
    );
    eprintln!("[T-286] odd   = odd-pi/4 step share (truth 0.0 for qpsk, 1.0 for pi4-dqpsk)");
    eprintln!("[T-286] mid   = mid-ring share (qam16 has 8/16 points there, 16-APSK none)");
    for (class, trusted_only) in [
        (Class::Bpsk, true),
        (Class::Qpsk, true),
        (Class::Qam16, true),
        (Class::Qam64, true),
        (Class::Pi4Dqpsk, true),
        // Neither reports a trusted rate at all; forcing C14's best candidate shows the cloud is
        // no more usable when one is supplied.
        (Class::Psk8, false),
        (Class::Apsk16, false),
    ] {
        let (mut resids, mut odds, mut mids) = (Vec::new(), Vec::new(), Vec::new());
        let mut best_label: std::collections::BTreeMap<String, u32> = Default::default();
        let (mut have_rate, mut total) = (0u32, 0u32);
        for offset in [0.0_f64, 5.0, 10.0] {
            for seed in 0..12u64 {
                let snr = 20.0 + offset;
                let s = generate(
                    class,
                    &SynthConfig::new(snr, ACCEPTANCE_SEED_BASE + 980_000 + seed),
                );
                total += 1;
                let Some(sym) = c14.from_samples(
                    &s.symbol_samples,
                    s.symbol_sample_rate_hz,
                    Some(s.obw_hz),
                    Some(snr),
                ) else {
                    continue;
                };
                let rate = if trusted_only {
                    sym.symbol_rate_bd.value().filter(|_| sym.rate_trusted())
                } else {
                    sym.symbol_rate_bd
                        .value()
                        .or_else(|| sym.best_candidate_bd())
                };
                let Some(rate) = rate.filter(|r| *r > 0.0) else {
                    continue;
                };
                have_rate += 1;
                let sps = s.symbol_sample_rate_hz / rate;
                if !(3.0..=64.0).contains(&sps) {
                    continue;
                }
                let want = (sps * MAX_SYMBOLS as f64).ceil() as usize;
                let x: Vec<Complex64> = s.symbol_samples[..s.symbol_samples.len().min(want)]
                    .iter()
                    .map(|v| Complex64::new(f64::from(v.re), f64::from(v.im)))
                    .collect();
                let Some(y) = symbol_samples(&x, sps) else {
                    continue;
                };
                let n0 = 10f64.powf(-snr.clamp(0.0, 30.0) / 10.0);
                let mut best = (f64::INFINITY, String::new());
                for label in ["bpsk", "qpsk", "8psk", "qam16", "qam64"] {
                    if let Some(r) = residual(&y, label, n0) {
                        if r < best.0 {
                            best = (r, label.to_owned());
                        }
                    }
                }
                if best.0.is_finite() {
                    resids.push(best.0);
                    *best_label.entry(best.1).or_default() += 1;
                }
                odds.push(odd_step_fraction(&y));
                mids.push(mid_ring_fraction(&y));
            }
        }
        eprintln!(
            "[T-286] {:<12} {:<9} rate {have_rate:>2}/{total}  resid {}  odd {}  mid {}  best {:?}",
            class.label().replace("held-out:", ""),
            if trusted_only { "trusted" } else { "ANY-RATE" },
            stat(&resids),
            stat(&odds),
            stat(&mids),
            best_label
        );
    }
}
