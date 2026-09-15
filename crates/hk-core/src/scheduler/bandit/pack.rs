//! Window packing (ADR-0012 §5.1): candidates by score, each top unpacked candidate placed off DC
//! inside the usable span, the centre slid (on the arm-quantum grid, so arm keys are stable) to
//! maximise the summed `score_norm` of other unpacked candidates that also fit clear of DC.
//! Runs only when a new candidate snapshot is seen; allocation is allowed here.

use super::super::plan::rf_path;
use crate::source::SourceCapabilities;

/// One candidate as the packer sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PackItem {
    pub key: u64,
    pub lo_hz: f64,
    pub hi_hz: f64,
    pub score_norm: f64,
    pub suspect_fraction: f64,
    pub expected_interval_s: Option<f64>,
    pub min_on_off_s: Option<f64>,
    pub next_eta_ns: Option<i64>,
}

/// Window geometry shared by every arm of one packing.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Geometry<'a> {
    pub usable_hz: f64,
    pub quantum_hz: f64,
    pub dc_guard_hz: f64,
    pub bounds: &'a [f64],
    pub caps: &'a SourceCapabilities,
}

/// One packed window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Packed {
    pub center_hz: f64,
    pub rf_path: u8,
    pub lead: u64,
    pub prior: f64,
    pub suspect_fraction: f64,
    pub expected_interval_s: Option<f64>,
    pub required_revisit_s: Option<f64>,
    pub next_eta_ns: Option<i64>,
    pub members: u16,
    /// The lead candidate could not be placed clear of DC (wider than about half the span, or no
    /// in-range centre on its RF path).
    pub on_dc: bool,
}

fn fits(it: &PackItem, c: f64, half: f64, guard: f64) -> bool {
    it.lo_hz >= c - half && it.hi_hz <= c + half && (it.lo_hz >= c + guard || it.hi_hz <= c - guard)
}

/// Most grid points tried per side of DC.
const MAX_POINTS: i64 = 64;

/// Packs `items` (sorted by score, descending) into windows.
pub(crate) fn pack(items: &[PackItem], g: &Geometry<'_>) -> Vec<Packed> {
    let n = items.len();
    let half = g.usable_hz / 2.0;
    let guard = g.dc_guard_hz;
    let q = g.quantum_hz.max(1.0);
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| items[a].lo_hz.total_cmp(&items[b].lo_hz).then(a.cmp(&b)));
    let mut packed = vec![false; n];
    let mut out = Vec::new();
    for t in 0..n {
        if packed[t] {
            continue;
        }
        let it = items[t];
        let ct = 0.5 * (it.lo_hz + it.hi_hz);
        let path = rf_path(g.bounds, ct);
        let first = order.partition_point(|&j| items[j].lo_hz < ct - 2.0 * half - guard);
        let near: Vec<usize> = order[first..]
            .iter()
            .copied()
            .take_while(|&j| items[j].lo_hz <= ct + 2.0 * half)
            .filter(|&j| !packed[j])
            .collect();
        let objective = |c: f64| -> f64 {
            near.iter()
                .filter(|&&j| {
                    let o = &items[j];
                    fits(o, c, half, guard) && rf_path(g.bounds, 0.5 * (o.lo_hz + o.hi_hz)) == path
                })
                .map(|&j| items[j].score_norm.max(1e-9))
                .sum()
        };
        let preferred = [ct - half / 2.0, ct + half / 2.0];
        let mut best: Option<(f64, f64, f64)> = None;
        let mut consider = |c: f64| {
            if !(g.caps.supports_frequency(c)
                && rf_path(g.bounds, c) == path
                && fits(&it, c, half, guard))
            {
                return;
            }
            let obj = objective(c);
            let dist = preferred
                .iter()
                .map(|p| (p - c).abs())
                .fold(f64::INFINITY, f64::min);
            let better = match best {
                None => true,
                Some((bo, bd, bc)) => {
                    obj > bo + 1e-12
                        || ((obj - bo).abs() <= 1e-12 && (dist < bd || (dist == bd && c < bc)))
                }
            };
            if better {
                best = Some((obj, dist, c));
            }
        };
        for (a, b) in [
            (it.hi_hz - half, it.lo_hz - guard),
            (it.hi_hz + guard, it.lo_hz + half),
        ] {
            if a > b {
                continue;
            }
            let (k0, k1) = ((a / q).ceil() as i64, (b / q).floor() as i64);
            if k0 > k1 {
                consider(0.5 * (a + b));
            } else {
                let stride = ((k1 - k0) / MAX_POINTS).max(1);
                let mut k = k0;
                while k <= k1 {
                    consider(k as f64 * q);
                    k += stride;
                }
            }
        }
        let (center_hz, on_dc) = match best {
            Some((_, _, c)) => (c, false),
            None => (ct, true),
        };
        let mut p = Packed {
            center_hz,
            rf_path: path,
            lead: it.key,
            prior: it.score_norm,
            suspect_fraction: 0.0,
            expected_interval_s: None,
            required_revisit_s: None,
            next_eta_ns: None,
            members: 0,
            on_dc,
        };
        let (mut w_sum, mut sf_sum) = (0.0, 0.0);
        let members = near.iter().copied().filter(|&j| {
            j == t
                || (!on_dc
                    && fits(&items[j], center_hz, half, guard)
                    && rf_path(g.bounds, 0.5 * (items[j].lo_hz + items[j].hi_hz)) == path)
        });
        let members: Vec<usize> = if near.contains(&t) {
            members.collect()
        } else {
            std::iter::once(t).chain(members).collect()
        };
        for j in members {
            packed[j] = true;
            let o = &items[j];
            let w = o.score_norm.max(1e-9);
            w_sum += w;
            sf_sum += w * o.suspect_fraction;
            p.members = p.members.saturating_add(1);
            p.prior = p.prior.max(o.score_norm);
            p.expected_interval_s = max_opt(p.expected_interval_s, o.expected_interval_s);
            p.required_revisit_s = min_opt(p.required_revisit_s, o.min_on_off_s.map(|m| m / 2.0));
            p.next_eta_ns = match (p.next_eta_ns, o.next_eta_ns) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        p.suspect_fraction = if w_sum > 0.0 {
            (sf_sum / w_sum).clamp(0.0, 1.0)
        } else {
            0.0
        };
        out.push(p);
    }
    out
}

fn max_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, y) => x.or(y),
    }
}

fn min_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: u64, center_mhz: f64, bw_khz: f64, score_norm: f64) -> PackItem {
        let (c, h) = (center_mhz * 1e6, bw_khz * 500.0);
        PackItem {
            key,
            lo_hz: c - h,
            hi_hz: c + h,
            score_norm,
            suspect_fraction: 0.0,
            expected_interval_s: None,
            min_on_off_s: None,
            next_eta_ns: None,
        }
    }

    #[test]
    fn packs_neighbours_into_one_window_clear_of_dc_on_the_grid() {
        let caps = SourceCapabilities::hackrf_one();
        let g = Geometry {
            usable_hz: 15e6,
            quantum_hz: 1e6,
            dc_guard_hz: 10e3,
            bounds: &caps.rf_path_boundaries_hz,
            caps: &caps,
        };
        // Top at 433.9 MHz; neighbours at 434.5, 438 (fit together) and 470 (does not).
        let items = [
            item(1, 433.92, 50.0, 0.9),
            item(2, 434.5, 50.0, 0.5),
            item(3, 438.0, 200.0, 0.4),
            item(4, 470.0, 12.5, 0.3),
        ];
        let out = pack(&items, &g);
        assert_eq!(out.len(), 2, "{out:?}");
        let a = out[0];
        assert_eq!((a.lead, a.members, a.on_dc), (1, 3, false));
        assert_eq!(a.center_hz % 1e6, 0.0, "grid centre {}", a.center_hz);
        for it in &items[..3] {
            assert!(
                fits(it, a.center_hz, 7.5e6, 10e3),
                "{it:?} in {}",
                a.center_hz
            );
        }
        assert_eq!((out[1].lead, out[1].members), (4, 1));
        assert_eq!(a.prior, 0.9);
    }
}
