//! Harmonic-aware rate consensus over the raw cyclic lines and run-length seeds (S5
//! `rate_consensus`).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::transitions::TransitionFit;
use super::{CyclicLine, LineGroup, LineMethod};

/// A line supporting a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LineSupport {
    /// Feature series.
    pub method: LineMethod,
    /// Whitened significance, dB.
    pub significance_db: f64,
}

/// One rate hypothesis after LS refinement and harmonic accounting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RateCandidate {
    /// Rate, Bd (the LS rate when the fit is ok, else the significance-weighted line mean).
    pub rate_bd: f64,
    /// One-sigma uncertainty, Bd.
    pub sigma_bd: f64,
    /// The guarded transition fit seeded here passed.
    pub transition_fit_ok: bool,
    /// Lines within 1 % of the rate (strongest per method).
    pub direct: Vec<LineSupport>,
    /// With an ok fit: lines within 1 % of 2, 3 or 4 × the rate.
    pub harmonic: Vec<LineSupport>,
    /// Supporting methods (+1 for an ok fit).
    pub methods: u32,
}

/// The consensus result.
#[derive(Clone, Debug, Default)]
pub(crate) struct Consensus {
    /// Ranked candidates (best first).
    pub candidates: Vec<RateCandidate>,
    /// Fit of the winning candidate when ok.
    pub fit: Option<TransitionFit>,
    /// The trust rule passed (before the structure / SNR / carrier gates).
    pub trusted: bool,
    /// Independent method groups with a direct line.
    pub groups: Vec<LineGroup>,
    /// Groups with a direct line at or above the trust threshold.
    pub strong_groups: Vec<LineGroup>,
    /// The winning candidate's frequency before LS (for ×½, ×2).
    pub centre_bd: Option<f64>,
}

/// Transition-fit strategy for the consensus (none for linear families).
pub(crate) trait Fitter {
    fn fit(&mut self, seed_bd: f64) -> TransitionFit;
}

pub(crate) struct Thresholds {
    pub count_db: f64,
    pub trust_db: f64,
    /// Lines without an ok fit on transition-structured signals.
    pub lines_without_fit: usize,
}

fn close(a: f64, b: f64) -> bool {
    (a / b - 1.0).abs() < 0.01
}

/// Ranks candidates `{f/4, f/3, f/2, f, 2f}` of every counted line plus `seeds`.
///
/// - With a fitter, each candidate seeds a guarded transition fit; an ok fit replaces the
///   candidate by the LS rate (the fit divides out a common run-count factor, so a 3× seed lands
///   on the rate). Candidates within 1 % of an already evaluated one (same fit status) merge.
/// - Support = direct lines within 1 %; with an ok fit, lines at 2, 3, 4 × also count.
/// - Rank: (fit ok, supporting methods, summed direct significance, lower rate). An LS-confirmed
///   candidate outranks any line vote (S5 pitfall 6).
/// - Trusted iff direct lines ≥ `trust_db` from both independent groups, or an ok fit with ≥ 1
///   direct or harmonic line; a transition-structured signal (fitter given) without an ok fit
///   needs `lines_without_fit` direct methods.
pub(crate) fn rate_consensus(
    lines: &[CyclicLine],
    f_min: f64,
    f_max: f64,
    mut fitter: Option<&mut dyn Fitter>,
    seeds: &[f64],
    th: &Thresholds,
) -> Consensus {
    let sig: Vec<(f64, f64, f64, LineMethod)> = lines
        .iter()
        .filter_map(|l| {
            l.freq_hz
                .filter(|_| l.significance_db >= th.count_db)
                .map(|f| {
                    (
                        f,
                        l.significance_db,
                        l.sigma_hz.unwrap_or(f64::NAN),
                        l.method,
                    )
                })
        })
        .collect();
    let mut raw: Vec<f64> = sig
        .iter()
        .flat_map(|&(f, ..)| [0.25, 1.0 / 3.0, 0.5, 1.0, 2.0].map(|k| f * k))
        .chain(seeds.iter().copied())
        .filter(|&c| c.is_finite() && c > 0.0 && (f_min..=f_max).contains(&c))
        .collect();
    raw.sort_by(f64::total_cmp);
    raw.dedup();

    let mut cache: HashMap<i64, TransitionFit> = HashMap::new();
    let mut evaluated: Vec<(f64, Option<TransitionFit>)> = Vec::new(); // (effective rate, ok fit)
    for c in raw {
        let fit = fitter.as_mut().map(|f| {
            let e = c.log10().floor() as i32;
            let key = (c / 10f64.powi(e - 2)).round() as i64 * 100 + i64::from(e);
            cache.entry(key).or_insert_with(|| f.fit(c)).clone()
        });
        let ok_fit = fit.filter(|f| f.ok && f.rate_bd.is_some());
        let eff = ok_fit.as_ref().and_then(|f| f.rate_bd).unwrap_or(c);
        if evaluated
            .iter()
            .any(|(e, l)| close(eff, *e) && l.is_some() == ok_fit.is_some())
        {
            continue;
        }
        evaluated.push((eff, ok_fit));
    }

    struct Scored {
        key: (bool, usize, f64, f64),
        cand: RateCandidate,
        fit: Option<TransitionFit>,
        direct_groups: Vec<(LineGroup, f64)>,
    }
    let mut scored: Vec<Scored> = Vec::new();
    for (eff, fit) in evaluated {
        let mut direct: HashMap<LineMethod, f64> = HashMap::new();
        let mut harm: HashMap<LineMethod, f64> = HashMap::new();
        for &(f, s, _, m) in &sig {
            if close(f, eff) {
                let e = direct.entry(m).or_insert(s);
                *e = e.max(s);
            } else if fit.is_some() && [2.0, 3.0, 4.0].iter().any(|k| close(f, k * eff)) {
                let e = harm.entry(m).or_insert(s);
                *e = e.max(s);
            }
        }
        if direct.is_empty() && fit.is_none() {
            continue;
        }
        let mut methods: Vec<LineMethod> = direct.keys().chain(harm.keys()).copied().collect();
        methods.sort_by_key(|m| *m as u8);
        methods.dedup();
        let n_methods = methods.len() + usize::from(fit.is_some());
        let sum_direct: f64 = direct.values().sum();
        let (rate, sigma) = match &fit {
            Some(f) => (f.rate_bd.unwrap_or(eff), f.sigma_bd.unwrap_or(f64::NAN)),
            None => {
                let near: Vec<(f64, f64, f64)> = sig
                    .iter()
                    .filter(|(f, ..)| close(*f, eff))
                    .map(|&(f, s, sg, _)| (f, 10f64.powf(s / 10.0), sg))
                    .collect();
                let wsum: f64 = near.iter().map(|n| n.1).sum();
                let v = near.iter().map(|n| n.0 * n.1).sum::<f64>() / wsum;
                let inv: f64 = near
                    .iter()
                    .filter(|n| n.2.is_finite() && n.2 > 0.0)
                    .map(|n| 1.0 / (n.2 * n.2))
                    .sum();
                (
                    v,
                    if inv > 0.0 {
                        inv.sqrt().recip()
                    } else {
                        f64::NAN
                    },
                )
            }
        };
        let support = |m: &HashMap<LineMethod, f64>| {
            let mut v: Vec<LineSupport> = m
                .iter()
                .map(|(&method, &significance_db)| LineSupport {
                    method,
                    significance_db,
                })
                .collect();
            v.sort_by_key(|s| s.method as u8);
            v
        };
        scored.push(Scored {
            key: (fit.is_some(), n_methods, sum_direct, -eff),
            direct_groups: direct.iter().map(|(m, s)| (m.group(), *s)).collect(),
            cand: RateCandidate {
                rate_bd: rate,
                sigma_bd: sigma,
                transition_fit_ok: fit.is_some(),
                direct: support(&direct),
                harmonic: support(&harm),
                methods: n_methods as u32,
            },
            fit,
        });
    }
    scored.sort_by(|a, b| {
        b.key
            .0
            .cmp(&a.key.0)
            .then(b.key.1.cmp(&a.key.1))
            .then(b.key.2.total_cmp(&a.key.2))
            .then(b.key.3.total_cmp(&a.key.3))
    });
    let Some(best) = scored.first() else {
        return Consensus::default();
    };
    let mut groups: Vec<LineGroup> = best.direct_groups.iter().map(|g| g.0).collect();
    groups.sort_by_key(|g| *g as u8);
    groups.dedup();
    let mut strong: Vec<LineGroup> = best
        .direct_groups
        .iter()
        .filter(|g| g.1 >= th.trust_db)
        .map(|g| g.0)
        .collect();
    strong.sort_by_key(|g| *g as u8);
    strong.dedup();
    let mut trusted = strong.len() >= 2
        || (best.fit.is_some() && (!best.cand.direct.is_empty() || !best.cand.harmonic.is_empty()));
    if fitter.is_some() && best.fit.is_none() && best.cand.direct.len() < th.lines_without_fit {
        trusted = false;
    }
    let centre = -best.key.3;
    Consensus {
        fit: best.fit.clone(),
        trusted,
        groups,
        strong_groups: strong,
        centre_bd: Some(centre),
        candidates: scored.into_iter().map(|s| s.cand).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(m: LineMethod, f: f64, s: f64) -> CyclicLine {
        CyclicLine {
            method: m,
            group: m.group(),
            freq_hz: Some(f),
            significance_db: s,
            sigma_hz: Some(1.0),
            whiten_clamped: false,
        }
    }

    const TH: Thresholds = Thresholds {
        count_db: 12.0,
        trust_db: 14.0,
        lines_without_fit: 3,
    };

    /// S5 pitfall 1: |x_c²| is |x|². Two agreeing lines from the *same* (envelope) group are one
    /// piece of evidence: they must not make a rate trusted.
    #[test]
    fn pitfall1_same_group_agreement_is_not_independent() {
        let lines = [
            line(LineMethod::EnvelopeSquare, 25_000.0, 30.0),
            line(LineMethod::EnvelopeDiff, 25_010.0, 28.0),
        ];
        let c = rate_consensus(&lines, 100.0, 60_000.0, None, &[], &TH);
        assert!((c.candidates[0].rate_bd - 25_005.0).abs() < 10.0);
        assert!(!c.trusted, "one group, two methods: {c:?}");
        let lines = [
            line(LineMethod::EnvelopeSquare, 25_000.0, 30.0),
            line(LineMethod::DelayMultiply, 25_010.0, 15.0),
        ];
        let c = rate_consensus(&lines, 100.0, 60_000.0, None, &[], &TH);
        assert!(c.trusted, "two independent groups ≥ 14 dB: {c:?}");
    }

    struct Fixed(f64);
    impl Fitter for Fixed {
        fn fit(&mut self, seed: f64) -> TransitionFit {
            let mut f = crate::blind::transitions::rate_transitions_ls(
                &[0.0; 4],
                1.0,
                0.5,
                1.0,
                None,
                &Default::default(),
            );
            // Pretend: seeds within 2 % of an integer multiple of the true rate fit.
            let k = (seed / self.0).round();
            if k >= 1.0 && (seed / (k * self.0) - 1.0).abs() < 0.02 {
                f.ok = true;
                f.rate_bd = Some(self.0);
                f.sigma_bd = Some(1.0);
                f.failure = None;
            }
            f
        }
    }

    /// S5 pitfall 6: a 0101 preamble puts the strongest line at Rs/2 and biphase edges at 2Rs;
    /// the LS-confirmed candidate outranks line votes and ×½ / ×2 are offered.
    #[test]
    fn pitfall6_ls_confirmed_candidate_outranks_harmonic_lines() {
        let lines = [
            line(LineMethod::IfDiff, 50_000.0, 35.0),
            line(LineMethod::DelayMultiply, 50_020.0, 33.0),
            line(LineMethod::EnvelopeDiff, 200_000.0, 13.0),
        ];
        let mut fitter = Fixed(100_000.0);
        let c = rate_consensus(&lines, 1_000.0, 240_000.0, Some(&mut fitter), &[], &TH);
        let best = &c.candidates[0];
        assert!(best.transition_fit_ok && best.rate_bd == 100_000.0, "{c:?}");
        assert!(c.trusted, "LS ok + harmonic line: {c:?}");
        assert_eq!(best.harmonic.len(), 1);
        // Without a fit the Rs/2 line wins but a transition-structured signal needs 3 methods.
        struct Never;
        impl Fitter for Never {
            fn fit(&mut self, seed: f64) -> TransitionFit {
                let mut f = Fixed(1.0).fit(seed);
                f.ok = false;
                f
            }
        }
        let c = rate_consensus(&lines, 1_000.0, 240_000.0, Some(&mut Never), &[], &TH);
        assert!((c.candidates[0].rate_bd - 50_010.0).abs() < 20.0, "{c:?}");
        assert!(!c.trusted);
    }
}
