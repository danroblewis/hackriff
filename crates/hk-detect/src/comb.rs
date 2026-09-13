//! Rule 4: a narrowband comb, ≥ 6 lines on an arithmetic grid (±1.5 kHz, spacing 100 kHz–2 MHz)
//! whose Monte-Carlo chance probability is below 5 % (S4 `find_comb` / `comb_flags`).
//!
//! **Search.** Every pair of lines `(i < j)` and harmonic `m ≤ 40` proposes a spacing
//! `d = (f_j − f_i)/m` in range; the proposal with the most lines within tolerance of
//! `f_i + k·d` wins (first maximum in `(i, j, m)` order, like `np.argmax`). With ≥ 3 members the
//! spacing is refined by least squares on the members and the members recounted.
//!
//! **Chance.** On `n` uniform random lines over the usable span, up to `trials` times: the fraction
//! that holds a grid of at least as many members. It depends only on `(n, members, span)`, so it
//! is cached. Trials use a pruned threshold search (forward from each anchor, abandoning anchors and
//! scans that cannot reach the count) and stop early once the decision is settled: at
//! `⌊max_chance·trials⌋ + 1` hits (not a comb), or after `⌈ln 0.05 / ln(1 − max_chance)⌉` (59)
//! trials with no hit, when the 95 % upper bound on the chance is already below `max_chance`.
//! Buffers are sized once (`max_lines`), so evaluation allocates nothing.

use crate::config::CombRule;

/// Result of a comb evaluation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Comb {
    /// Lines on the best grid.
    pub members: usize,
    /// Grid spacing, Hz (`NaN` when no comb).
    pub spacing_hz: f64,
    /// Monte-Carlo chance probability (`NaN` when not computed).
    pub chance: f64,
    /// Members ≥ `min_members` and chance < `max_chance`.
    pub flagged: bool,
    /// Member line frequencies, Hz.
    pub member_hz: Vec<f64>,
}

/// The comb search with its buffers and chance cache.
#[derive(Clone, Debug)]
pub struct CombFinder {
    rule: CombRule,
    sorted: Vec<f64>,
    order: Vec<usize>,
    mask: Vec<bool>,
    trial: Vec<f64>,
    cache: Vec<f32>,
    cache_span_hz: f64,
}

impl CombFinder {
    /// A finder for `rule`.
    pub fn new(rule: CombRule) -> Self {
        let m = rule.max_lines;
        Self {
            rule,
            sorted: Vec::with_capacity(m),
            order: Vec::with_capacity(m),
            mask: vec![false; m],
            trial: Vec::with_capacity(m),
            cache: vec![f32::NAN; (m + 1) * (m + 1)],
            cache_span_hz: f64::NAN,
        }
    }

    /// The best grid among sorted frequencies `fs`: `(members, spacing)`; `mask` marks members.
    fn find_sorted(rule: &CombRule, fs: &[f64], mask: &mut [bool]) -> (usize, f64) {
        let n = fs.len();
        mask[..n].fill(false);
        if n < 3 {
            return (0, f64::NAN);
        }
        let tol = rule.tolerance_hz;
        let count = |anchor: f64, d: f64| -> usize {
            fs.iter()
                .filter(|&&f| {
                    let k = (f - anchor) / d;
                    (k - k.round()).abs() * d <= tol
                })
                .count()
        };
        let (mut best, mut best_anchor, mut best_d) = (0usize, 0.0f64, f64::NAN);
        for i in 0..n {
            for j in i + 1..n {
                let dist = fs[j] - fs[i];
                for m in 1..=rule.max_harmonic {
                    let d = dist / m as f64;
                    if d > rule.max_spacing_hz {
                        continue;
                    }
                    if d < rule.min_spacing_hz {
                        break;
                    }
                    let c = count(fs[i], d);
                    if c > best {
                        best = c;
                        best_anchor = fs[i];
                        best_d = d;
                    }
                }
            }
        }
        if best == 0 {
            return (0, f64::NAN);
        }
        let member = |f: f64, anchor: f64, d: f64| {
            let k = (f - anchor) / d;
            (k - k.round()).abs() * d <= tol
        };
        let mut members = 0;
        for (l, &f) in fs.iter().enumerate() {
            mask[l] = member(f, best_anchor, best_d);
            members += usize::from(mask[l]);
        }
        let mut d = best_d;
        if members >= 3 {
            // Least squares of f against its grid index over the members.
            let (mut sk, mut sf, mut skk, mut skf, mut cnt) = (0.0, 0.0, 0.0, 0.0, 0.0);
            for (l, &f) in fs.iter().enumerate() {
                if mask[l] {
                    let k = ((f - best_anchor) / best_d).round();
                    sk += k;
                    sf += f;
                    skk += k * k;
                    skf += k * f;
                    cnt += 1.0;
                }
            }
            let den = skk - sk * sk / cnt;
            if den > 0.0 {
                let slope = (skf - sk * sf / cnt) / den;
                if slope.is_finite() && slope > 0.0 {
                    d = slope;
                }
            }
            let anchor = fs[mask.iter().position(|&x| x).expect("members")];
            members = 0;
            for (l, &f) in fs.iter().enumerate() {
                mask[l] = member(f, anchor, d);
                members += usize::from(mask[l]);
            }
        }
        (members, d)
    }

    /// Chance probability that `n` uniform lines over `[lo, hi]` hold a comb of `members` lines.
    pub fn chance(&mut self, n: usize, members: usize, lo_hz: f64, hi_hz: f64) -> f64 {
        let span = hi_hz - lo_hz;
        if self.cache_span_hz.to_bits() != span.to_bits() {
            self.cache.fill(f32::NAN);
            self.cache_span_hz = span;
        }
        let m = self.rule.max_lines;
        let idx = n.min(m) * (m + 1) + members.min(m);
        if !self.cache[idx].is_nan() {
            return f64::from(self.cache[idx]);
        }
        // SplitMix64 seeded by (seed, n, members): deterministic.
        let mut state = self.rule.seed ^ ((n as u64) << 32) ^ members as u64;
        let mut next = move || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            ((z >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        let trials = self.rule.trials.max(1);
        let stop = (self.rule.max_chance * trials as f64).floor() as usize + 1;
        // With no hit in `quiet` trials the 95 % upper bound on the chance is below `max_chance`
        // (1 − 0.05^(1/quiet) < max_chance), so the decision cannot change: stop there.
        let quiet = if self.rule.max_chance > 0.0 && self.rule.max_chance < 1.0 {
            ((0.05f64).ln() / (1.0 - self.rule.max_chance).ln()).ceil() as usize
        } else {
            trials
        };
        let mut hits = 0usize;
        let mut done = 0usize;
        for _ in 0..trials {
            self.trial.clear();
            for _ in 0..n.min(m) {
                self.trial.push(lo_hz + span * next());
            }
            self.trial.sort_unstable_by(f64::total_cmp);
            done += 1;
            if Self::reaches(&self.rule, &self.trial, members) {
                hits += 1;
                if hits >= stop {
                    break;
                }
            } else if hits == 0 && done >= quiet {
                break;
            }
        }
        let p = if hits >= stop {
            (hits as f64 / done as f64).max(self.rule.max_chance)
        } else {
            hits as f64 / done as f64
        };
        self.cache[idx] = p as f32;
        p
    }

    /// Whether some grid holds at least `need` of the sorted lines `fs` (the Monte-Carlo trial
    /// test). Counts forward from each anchor (the grid's first member) and prunes anchors,
    /// partners and scans that can no longer reach `need`.
    fn reaches(rule: &CombRule, fs: &[f64], need: usize) -> bool {
        let n = fs.len();
        if need < 2 || n < need {
            return need <= 1 && n > 0;
        }
        let tol = rule.tolerance_hz;
        for i in 0..=n - need {
            for j in i + 1..=n - need + 1 {
                let dist = fs[j] - fs[i];
                for m in 1..=rule.max_harmonic {
                    let d = dist / m as f64;
                    if d > rule.max_spacing_hz {
                        continue;
                    }
                    if d < rule.min_spacing_hz {
                        break;
                    }
                    let mut count = 2;
                    for (l, &f) in fs.iter().enumerate().skip(j + 1) {
                        if count + (n - l) < need {
                            break;
                        }
                        let k = (f - fs[i]) / d;
                        if (k - k.round()).abs() * d <= tol {
                            count += 1;
                            if count >= need {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Evaluates lines `freqs_hz` (any order, at most `max_lines` used) over the span
    /// `[lo, hi]` into `out`.
    pub fn evaluate(&mut self, freqs_hz: &[f64], lo_hz: f64, hi_hz: f64, out: &mut Comb) {
        out.members = 0;
        out.spacing_hz = f64::NAN;
        out.chance = f64::NAN;
        out.flagged = false;
        out.member_hz.clear();
        let n = freqs_hz.len().min(self.rule.max_lines);
        if n < self.rule.min_members {
            return;
        }
        self.order.clear();
        self.order.extend(0..n);
        self.order
            .sort_unstable_by(|&a, &b| freqs_hz[a].total_cmp(&freqs_hz[b]));
        self.sorted.clear();
        self.sorted.extend(self.order.iter().map(|&i| freqs_hz[i]));
        let (members, d) = Self::find_sorted(&self.rule, &self.sorted, &mut self.mask);
        out.members = members;
        out.spacing_hz = d;
        if members >= self.rule.min_members {
            for (l, &f) in self.sorted.iter().enumerate() {
                if self.mask[l] {
                    out.member_hz.push(f);
                }
            }
            out.chance = self.chance(n, members, lo_hz, hi_hz);
            out.flagged = out.chance < self.rule.max_chance;
        }
    }

    /// Whether `f_hz` is on a flagged comb's member list within `tolerance_hz`.
    pub fn is_member(comb: &Comb, f_hz: f64, tolerance_hz: f64) -> bool {
        comb.flagged
            && comb
                .member_hz
                .iter()
                .any(|&m| (m - f_hz).abs() <= tolerance_hz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comb_lines(n: usize, start: f64, spacing: f64) -> Vec<f64> {
        (0..n).map(|i| start + i as f64 * spacing).collect()
    }

    #[test]
    fn finds_a_14_line_comb_among_random_lines_and_flags_it() {
        let rule = CombRule::default();
        let mut finder = CombFinder::new(rule);
        let mut lines = comb_lines(14, 90.2841e6, 209_478.6);
        // Jitter within tolerance, plus unrelated lines.
        for (i, f) in lines.iter_mut().enumerate() {
            *f += ((i * 7919) % 11) as f64 * 100.0 - 500.0;
        }
        lines.extend([91.1016e6, 93.7498e6, 100.44e6, 102.78e6, 106.1e6]);
        let mut out = Comb::default();
        finder.evaluate(&lines, 90e6, 106e6, &mut out);
        // Like S4's `find_comb` (first maximum count), a subharmonic grid that holds every true
        // line plus a chance coincidence wins over the fundamental: here d/2 picks up 106.1 MHz.
        let m = (209_478.6 / out.spacing_hz).round();
        assert!((1.0..=2.0).contains(&m), "{out:?}");
        assert!(
            (out.spacing_hz * m - 209_478.6).abs() < 50.0,
            "{}",
            out.spacing_hz
        );
        assert!(out.members >= 14, "{out:?}");
        assert!(out.flagged && out.chance < 0.05, "{out:?}");
        for f in &lines[..14] {
            assert!(CombFinder::is_member(&out, *f, 2e3), "{f}");
        }
        assert!(!CombFinder::is_member(&out, 100.44e6, 2e3));
    }

    #[test]
    fn random_lines_are_not_a_comb() {
        let mut finder = CombFinder::new(CombRule::default());
        let mut state = 12345u64;
        let mut lines = Vec::new();
        for _ in 0..12 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            lines.push(90e6 + 16e6 * ((state >> 11) as f64 / (1u64 << 53) as f64));
        }
        let mut out = Comb::default();
        finder.evaluate(&lines, 90e6, 106e6, &mut out);
        assert!(!out.flagged, "{out:?}");
        // Too few lines never search.
        finder.evaluate(&lines[..5], 90e6, 106e6, &mut out);
        assert_eq!(out.members, 0);
    }
}
