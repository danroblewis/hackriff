//! Assertions against truth, with use-case IDs in every failure message.

use std::fmt;

use crate::fixture::TruthItem;
use crate::pipeline::DetectionBox;

/// A parameter tolerance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tolerance {
    /// `|measured - expected| <= value`.
    Abs(f64),
    /// `|measured - expected| <= fraction * |expected|`.
    Rel(f64),
}

impl Tolerance {
    /// Whether `measured` is within tolerance of `expected`.
    pub fn allows(self, measured: f64, expected: f64) -> bool {
        let err = (measured - expected).abs();
        match self {
            Tolerance::Abs(t) => err <= t,
            Tolerance::Rel(f) => err <= f * expected.abs(),
        }
    }
}

impl fmt::Display for Tolerance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tolerance::Abs(t) => write!(f, "±{t}"),
            Tolerance::Rel(r) => write!(f, "±{}%", r * 100.0),
        }
    }
}

fn ids(use_cases: &[&str]) -> String {
    if use_cases.is_empty() {
        "[no use case]".into()
    } else {
        format!("[{}]", use_cases.join(", "))
    }
}

/// Checks one estimated parameter; the error message names the use cases.
pub fn check_param(
    use_cases: &[&str],
    name: &str,
    measured: f64,
    expected: f64,
    tolerance: Tolerance,
) -> Result<(), String> {
    if measured.is_finite() && tolerance.allows(measured, expected) {
        Ok(())
    } else {
        Err(format!(
            "{} {name}: measured {measured} vs truth {expected} (error {}, tolerance {tolerance})",
            ids(use_cases),
            measured - expected
        ))
    }
}

/// Panics unless `measured` is within `tolerance` of `expected`.
#[track_caller]
pub fn assert_param(
    use_cases: &[&str],
    name: &str,
    measured: f64,
    expected: f64,
    tolerance: Tolerance,
) {
    if let Err(msg) = check_param(use_cases, name, measured, expected, tolerance) {
        panic!("{msg}");
    }
}

/// How far a detection may sit from a truth box and still match it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxTolerance {
    /// Slack on each time edge, seconds.
    pub time_s: f64,
    /// Slack on each frequency edge, Hz.
    pub freq_hz: f64,
}

/// Result of [`match_detections`]. Indices refer to the slices passed in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MatchReport {
    /// `(truth index, detection index)` pairs, one-to-one.
    pub matches: Vec<(usize, usize)>,
    /// Truth items with no detection.
    pub missed: Vec<usize>,
    /// Detections that match nothing but overlap a truth or artefact box (duplicates, fragments,
    /// or detected artefacts).
    pub explained: Vec<usize>,
    /// Detections outside every truth and artefact box.
    pub false_alarms: Vec<usize>,
}

impl MatchReport {
    /// Fraction of truth items matched.
    pub fn recall(&self) -> f64 {
        let n = self.matches.len() + self.missed.len();
        if n == 0 {
            1.0
        } else {
            self.matches.len() as f64 / n as f64
        }
    }

    /// The detection matched to truth item `truth_index`.
    pub fn detection_for(&self, truth_index: usize) -> Option<usize> {
        self.matches
            .iter()
            .find(|(t, _)| *t == truth_index)
            .map(|(_, d)| *d)
    }

    /// Panics listing every missed truth item.
    #[track_caller]
    pub fn assert_all_found(&self, use_cases: &[&str], truth: &[&TruthItem]) {
        self.assert_recall_at_least(use_cases, truth, 1.0);
    }

    /// Panics if recall is below `min_recall`, listing the missed items.
    #[track_caller]
    pub fn assert_recall_at_least(
        &self,
        use_cases: &[&str],
        truth: &[&TruthItem],
        min_recall: f64,
    ) {
        if self.recall() + 1e-12 < min_recall {
            let missed: Vec<String> = self.missed.iter().map(|&i| describe(truth[i])).collect();
            panic!(
                "{} recall {:.3} < {min_recall}: missed {} of {} truth boxes:\n  {}",
                ids(use_cases),
                self.recall(),
                self.missed.len(),
                truth.len(),
                missed.join("\n  ")
            );
        }
    }

    /// Panics if there are more than `max` false alarms, listing them.
    #[track_caller]
    pub fn assert_false_alarms_at_most(
        &self,
        use_cases: &[&str],
        detections: &[DetectionBox],
        max: usize,
    ) {
        if self.false_alarms.len() > max {
            let list: Vec<String> = self
                .false_alarms
                .iter()
                .map(|&i| format!("{:?}", detections[i]))
                .collect();
            panic!(
                "{} {} false alarms outside truth boxes (max {max}):\n  {}",
                ids(use_cases),
                self.false_alarms.len(),
                list.join("\n  ")
            );
        }
    }
}

fn describe(t: &TruthItem) -> String {
    format!(
        "{} #{} t=[{:.6}, {:.6}] s f=[{:.1}, {:.1}] Hz",
        t.kind, t.annotation_index, t.t_start_s, t.t_end_s, t.f_lo_hz, t.f_hi_hz
    )
}

fn overlaps(d: &DetectionBox, t: &TruthItem, tol: BoxTolerance) -> bool {
    d.t_start_s <= t.t_end_s + tol.time_s
        && d.t_end_s >= t.t_start_s - tol.time_s
        && d.f_lo_hz <= t.f_hi_hz + tol.freq_hz
        && d.f_hi_hz >= t.f_lo_hz - tol.freq_hz
}

fn candidate(d: &DetectionBox, t: &TruthItem, tol: BoxTolerance) -> Option<f64> {
    let fc = d.center_hz();
    let tc = d.t_center_s();
    let freq_ok = fc >= t.f_lo_hz - tol.freq_hz && fc <= t.f_hi_hz + tol.freq_hz;
    let time_ok = tc >= t.t_start_s - tol.time_s && tc <= t.t_end_s + tol.time_s;
    (freq_ok && time_ok && overlaps(d, t, tol)).then(|| {
        let tt = (t.t_start_s + t.t_end_s) / 2.0;
        (fc - t.center_hz()).abs() / tol.freq_hz.max(1.0) + (tc - tt).abs() / tol.time_s.max(1e-9)
    })
}

/// Matches detections to truth boxes one-to-one.
///
/// A detection is a candidate for a truth box when its centre frequency and centre time fall
/// inside the box widened by `tol` on every edge. Candidates are assigned greedily, closest
/// (normalised centre distance) first. Unmatched detections that still overlap any widened truth
/// or `artefacts` box are `explained`; the rest are `false_alarms`.
pub fn match_detections(
    detections: &[DetectionBox],
    truth: &[&TruthItem],
    artefacts: &[&TruthItem],
    tol: BoxTolerance,
) -> MatchReport {
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (ti, t) in truth.iter().enumerate() {
        for (di, d) in detections.iter().enumerate() {
            if let Some(cost) = candidate(d, t, tol) {
                pairs.push((cost, ti, di));
            }
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut truth_used = vec![false; truth.len()];
    let mut det_used = vec![false; detections.len()];
    let mut report = MatchReport::default();
    for (_, ti, di) in pairs {
        if !truth_used[ti] && !det_used[di] {
            truth_used[ti] = true;
            det_used[di] = true;
            report.matches.push((ti, di));
        }
    }
    report.matches.sort_unstable();
    report.missed = (0..truth.len()).filter(|&i| !truth_used[i]).collect();
    for (di, d) in detections.iter().enumerate() {
        if det_used[di] {
            continue;
        }
        if truth.iter().chain(artefacts).any(|t| overlaps(d, t, tol)) {
            report.explained.push(di);
        } else {
            report.false_alarms.push(di);
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::fixture::Role;

    fn item(i: usize, t: (f64, f64), f: (f64, f64), role: Role) -> TruthItem {
        TruthItem {
            annotation_index: i,
            role,
            kind: "burst".into(),
            label: None,
            sample_start: 0,
            sample_count: 0,
            t_start_s: t.0,
            t_end_s: t.1,
            f_lo_hz: f.0,
            f_hi_hz: f.1,
            value: json!({}),
        }
    }

    fn det(t: (f64, f64), f: (f64, f64)) -> DetectionBox {
        DetectionBox {
            t_start_s: t.0,
            t_end_s: t.1,
            f_lo_hz: f.0,
            f_hi_hz: f.1,
            ..Default::default()
        }
    }

    const TOL: BoxTolerance = BoxTolerance {
        time_s: 0.01,
        freq_hz: 1e3,
    };

    #[test]
    fn matches_within_tolerance_and_classifies_the_rest() {
        let a = item(0, (0.0, 0.1), (100e3, 120e3), Role::Emission);
        let b = item(1, (0.5, 0.6), (100e3, 120e3), Role::Emission);
        let spur = item(2, (0.0, 1.0), (300e3, 300e3), Role::Artefact);
        let dets = vec![
            det((0.505, 0.598), (100.5e3, 120.5e3)), // b, shifted inside tolerance
            det((0.0, 0.1), (100e3, 120e3)),         // a
            det((0.01, 0.05), (101e3, 119e3)),       // fragment of a
            det((0.2, 0.3), (300e3, 300e3)),         // the spur
            det((0.8, 0.9), (500e3, 510e3)),         // nothing there
        ];
        let r = match_detections(&dets, &[&a, &b], &[&spur], TOL);
        assert_eq!(r.matches, vec![(0, 1), (1, 0)]);
        assert!(r.missed.is_empty());
        assert_eq!(r.explained, vec![2, 3]);
        assert_eq!(r.false_alarms, vec![4]);
        assert_eq!(r.recall(), 1.0);
        assert_eq!(r.detection_for(1), Some(0));
    }

    #[test]
    fn misses_outside_tolerance() {
        let a = item(0, (0.0, 0.1), (100e3, 120e3), Role::Emission);
        let dets = vec![det((0.0, 0.1), (125e3, 135e3))];
        let r = match_detections(&dets, &[&a], &[], TOL);
        assert_eq!(r.missed, vec![0]);
        assert_eq!(r.false_alarms, vec![0]);
        let msg = std::panic::catch_unwind(|| r.assert_all_found(&["AWARE-036"], &[&a]))
            .unwrap_err()
            .downcast::<String>()
            .unwrap();
        assert!(
            msg.contains("[AWARE-036]") && msg.contains("burst #0"),
            "{msg}"
        );
    }

    #[test]
    fn parameter_tolerances_name_the_use_case() {
        assert!(!Tolerance::Rel(0.01).allows(4850.0, 4800.0));
        assert!(Tolerance::Rel(0.01).allows(4847.0, 4800.0));
        assert!(Tolerance::Abs(1.0).allows(-99.5, -100.0));
        assert!(check_param(&[], "x", f64::NAN, 1.0, Tolerance::Abs(10.0)).is_err());
        let err = check_param(
            &["SPACE-050"],
            "floor_dbm",
            -120.0,
            -122.0,
            Tolerance::Abs(1.0),
        )
        .unwrap_err();
        assert!(err.starts_with("[SPACE-050] floor_dbm"), "{err}");
    }
}
