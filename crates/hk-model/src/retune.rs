//! Retune diversity (T-586, AWARE-011): deciding whether a measured line is **on the air** or
//! **in the receiver**, by looking at it from more than one local oscillator.
//!
//! # The physics, which is what makes this a measurement and not an opinion
//!
//! A real emission exists at an absolute radio frequency. Retuning the front end changes which
//! baseband bin it lands in, and nothing else: its **absolute** frequency is invariant.
//!
//! A receiver artefact is manufactured inside the receive chain, at a frequency the LO defines:
//!
//! | mechanism | where it lands | slope `d f / d f_LO` |
//! |---|---|---|
//! | real emission | `f` | 0 |
//! | DC / LO leakage | `f_LO` | 1 |
//! | internal spur at a fixed IF offset, baseband product | `f_LO + k` | 1 |
//! | IQ image of an emission at `f` | `2·f_LO − f` | 2 |
//!
//! So the **slope of a line's absolute frequency against the LO** separates the classes, and one
//! retune measures it. This is the generalisation, over N centres and over the *stored* record, of
//! the pairwise live-spectrum test in `hk_detect::trust::retune`: that one compares two
//! `CaptureResult` spectra inside the scheduler's verification group, while this one reads
//! [`Detection::f_center_hz`](crate::Detection::f_center_hz) and the LO from the detection's own
//! [`Provenance`](crate::Provenance) — the record every detection already carries — so a survey
//! that visited a region at several centres can be judged after the fact, with no extra dwell.
//!
//! # What a verdict does and does not claim
//!
//! [`RetuneSlope::LoLocked`] and [`RetuneSlope::Image`] are **positive** artefact findings: nothing
//! on the air tracks a receiver's LO. [`RetuneSlope::Absolute`] is the weaker statement "not
//! LO-relative" — a reference-clock harmonic sits at a fixed absolute frequency too, and is still
//! an artefact, which is why the single-capture flaggers (`SpurReason::RefHarmonic`, `Dc`, `Comb`,
//! `ClockHarmonic`) are not replaced by this test but joined to it. Absolute-invariance is
//! necessary for a real emission, not sufficient.
//!
//! A single centre can therefore never produce a verdict: [`classify`] needs
//! [`RetuneTolerance::min_centres`] distinct LOs before it will say anything, and reports the
//! LOs and members it actually compared so a caller can tell "agreed across three centres" from
//! "nothing to compare", which otherwise look identical from the outside.

use serde::{Deserialize, Serialize};

use crate::detection::{DetectionFlags, SpurReason};

/// How a line's absolute frequency moves when the LO moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetuneSlope {
    /// Slope 0: the same absolute frequency from every centre. Consistent with a real emission
    /// (and with an absolute-frequency artefact such as a reference harmonic — see the module
    /// docs).
    Absolute,
    /// Slope 1: a fixed offset from the LO. DC/LO leakage (offset 0), an internal spur at a fixed
    /// IF offset, a baseband product. Always a receiver artefact.
    LoLocked,
    /// Slope 2: mirrored about the LO (`2·f_LO − f`). The IQ image of an emission. Always a
    /// receiver artefact.
    Image,
}

impl RetuneSlope {
    /// `d f / d f_LO` for this class.
    pub const fn factor(self) -> f64 {
        match self {
            RetuneSlope::Absolute => 0.0,
            RetuneSlope::LoLocked => 1.0,
            RetuneSlope::Image => 2.0,
        }
    }

    /// Whether the slope is by itself proof of a receiver artefact.
    pub const fn is_receiver_artefact(self) -> bool {
        !matches!(self, RetuneSlope::Absolute)
    }

    /// Storage/label name.
    pub const fn as_str(self) -> &'static str {
        match self {
            RetuneSlope::Absolute => "absolute",
            RetuneSlope::LoLocked => "lo-locked",
            RetuneSlope::Image => "image",
        }
    }

    /// Records the verdict on a detection's suspect flags, with the same meaning
    /// `hk_detect::trust::RetuneLabel::apply` gives the pairwise live test: an LO-locked line is a
    /// `lo-relative` spur candidate, a mirrored one a **retune-confirmed** image. An absolute line
    /// clears nothing — another mechanism may still have flagged it.
    pub fn apply(self, flags: &mut DetectionFlags) {
        match self {
            RetuneSlope::Absolute => {}
            RetuneSlope::LoLocked => {
                flags.spur_candidate = true;
                if flags.spur_reason.is_none() {
                    flags.spur_reason = Some(SpurReason::LoRelative);
                }
            }
            RetuneSlope::Image => {
                flags.image_candidate = true;
                flags.image_retune_confirmed = true;
            }
        }
    }
}

/// The slopes tested, in the order a tie is broken: a line that fits both the null hypothesis and
/// a mechanism is left alone.
pub const SLOPES: [RetuneSlope; 3] = [
    RetuneSlope::Absolute,
    RetuneSlope::LoLocked,
    RetuneSlope::Image,
];

/// One measured line and the LO it was measured under.
///
/// Built from a stored [`Detection`](crate::Detection): `f_center_hz` is the detection's own,
/// `lo_hz` is `provenance.tune.center_hz` read through its `provenance_ref`. Nothing else is
/// needed, and deliberately nothing else is read — the verdict must not depend on what the line
/// looks like, only on where it goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetuneObservation {
    /// Tuned centre (local oscillator), Hz.
    pub lo_hz: f64,
    /// Measured centre frequency, absolute Hz.
    pub f_center_hz: f64,
    /// Measured occupied bandwidth, Hz. Widens the grouping tolerance only; `0.0` is fine.
    pub bandwidth_hz: f64,
    /// Width of the instantaneous window this line was measured in, Hz — the complex sample rate
    /// (`provenance.tune.sample_rate_hz`), so the window is `lo_hz ± span_hz / 2`. `None` when
    /// the record does not say, which is "coverage unknown" (T-879: [`classify`]).
    pub span_hz: Option<f64>,
}

impl RetuneObservation {
    /// Whether this observation's window held absolute frequency `f_hz`: had a line been standing
    /// at `f_hz` while this one was measured, it was in view. Unknown coverage holds nothing.
    pub fn window_holds(&self, f_hz: f64) -> bool {
        self.span_hz
            .is_some_and(|span| span > 0.0 && (f_hz - self.lo_hz).abs() <= span / 2.0)
    }
}

/// Whether the absolute hypothesis was **testable** for these observations (T-879): some line,
/// measured under one LO, sits at an absolute frequency another member's window — under a
/// different LO — also covered.
///
/// An artefact verdict says "this line moved with the LO", and that is a measurement only when
/// the place it would have stayed, had it been on the air, was looked at from another centre and
/// the line was not there. Three windows that share no spectrum — a survey stepping a whole
/// window at a time — see three lines at one LO offset whether that is one spur or three
/// emitters on three channels on a common raster, and the physics cannot tell them apart. Before
/// T-879 that geometry was called LO-locked, and three units of one sensor type on three channels
/// 1.5 MHz apart (500 kHz windows) became one hidden "artefact".
fn absolute_hypothesis_tested(members: &[RetuneObservation], lo_tol_hz: f64) -> bool {
    members.iter().any(|a| {
        members
            .iter()
            .any(|b| (a.lo_hz - b.lo_hz).abs() > lo_tol_hz && b.window_holds(a.f_center_hz))
    })
}

// ---------------------------------------------------------------------------------------------
// Thresholds. All a priori — set from the measurement geometry, never fitted to a fixture.
// ---------------------------------------------------------------------------------------------

/// Smallest frequency tolerance, Hz, for calling two measurements of a line "the same frequency".
///
/// A priori, from the detector's own resolution rather than from any recording: a dwell FFT at the
/// rates this project uses puts a bin somewhere in the low kHz, and a centroid over a few bins
/// wanders by a bin or two, so 10 kHz is a couple of bins of slack. It is the same 10 kHz the
/// pairwise live test uses (`hk_detect::trust::RetuneConfig::frequency_tolerance_hz`), and the
/// same order as `crate::relate::ARTIFACT_CENTER_MIN_HZ`.
pub const RETUNE_MIN_TOLERANCE_HZ: f64 = 10e3;

/// Extra tolerance as a fraction of the **measured** bandwidth: a wide emission's centroid moves
/// more than a CW line's. Quarter-bandwidth, as `crate::relate::ARTIFACT_CENTER_BW_FRACTION`.
pub const RETUNE_TOLERANCE_BW_FRACTION: f64 = 0.25;

/// Distinct LOs a group needs before any verdict is given. Two is the minimum that measures
/// anything at all; below it there is no diversity and the honest answer is silence.
pub const RETUNE_MIN_CENTRES: usize = 2;

/// Grouping and verdict tolerances.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetuneTolerance {
    /// Smallest frequency tolerance, Hz.
    pub min_tolerance_hz: f64,
    /// Bandwidth fraction added to it.
    pub bandwidth_fraction: f64,
    /// Distinct LOs needed for a verdict.
    pub min_centres: usize,
}

impl Default for RetuneTolerance {
    fn default() -> Self {
        Self {
            min_tolerance_hz: RETUNE_MIN_TOLERANCE_HZ,
            bandwidth_fraction: RETUNE_TOLERANCE_BW_FRACTION,
            min_centres: RETUNE_MIN_CENTRES,
        }
    }
}

impl RetuneTolerance {
    /// Tolerance for one observation, Hz.
    fn for_obs(&self, o: &RetuneObservation) -> f64 {
        self.min_tolerance_hz
            .max(self.bandwidth_fraction * o.bandwidth_hz.max(0.0))
    }
}

/// A set of observations that behave as one line under one slope.
#[derive(Clone, Debug, PartialEq)]
pub struct RetuneGroup {
    /// The verdict.
    pub slope: RetuneSlope,
    /// The invariant coordinate `f − slope·f_LO`, Hz: the absolute frequency for
    /// [`RetuneSlope::Absolute`], the LO offset for [`RetuneSlope::LoLocked`], and `−f_source` for
    /// [`RetuneSlope::Image`] (so the mirrored emission sits at `−invariant_hz`).
    pub invariant_hz: f64,
    /// Indices into the observation slice, ascending.
    pub members: Vec<usize>,
    /// Distinct LOs the members were measured under, ascending.
    pub los_hz: Vec<f64>,
    /// Spread of the invariant coordinate over the members, Hz (max − min).
    pub spread_hz: f64,
}

impl RetuneGroup {
    /// Distinct LOs compared. Never below [`RetuneTolerance::min_centres`] in a returned group.
    pub fn centres(&self) -> usize {
        self.los_hz.len()
    }
}

/// What [`classify`] compared, so a caller can tell a real agreement from an empty one.
///
/// A cross-centre check over zero lines, or over one centre, passes every assertion while testing
/// nothing. These counts exist to be asserted on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RetuneSummary {
    /// Observations offered.
    pub observations: usize,
    /// Distinct LOs among them, ascending.
    pub los_hz: Vec<f64>,
    /// Groups returned.
    pub groups: Vec<RetuneGroup>,
    /// Observations in no group: seen under too few centres, or matching no slope.
    pub unexplained: Vec<usize>,
}

impl RetuneSummary {
    /// Distinct LOs seen across every observation.
    pub fn centres(&self) -> usize {
        self.los_hz.len()
    }

    /// Groups of one slope.
    pub fn with_slope(&self, slope: RetuneSlope) -> impl Iterator<Item = &RetuneGroup> {
        self.groups.iter().filter(move |g| g.slope == slope)
    }

    /// The group whose invariant coordinate is within `tol_hz` of `invariant_hz` under `slope`.
    pub fn find(&self, slope: RetuneSlope, invariant_hz: f64, tol_hz: f64) -> Option<&RetuneGroup> {
        self.with_slope(slope)
            .filter(|g| (g.invariant_hz - invariant_hz).abs() <= tol_hz)
            .min_by(|a, b| {
                (a.invariant_hz - invariant_hz)
                    .abs()
                    .total_cmp(&(b.invariant_hz - invariant_hz).abs())
            })
    }
}

/// Distinct LOs in `obs`, ascending, merged within `tol_hz` (a commanded retune moves the centre
/// far further than the tolerance, so this only collapses float noise).
fn distinct_los(obs: &[RetuneObservation], tol_hz: f64) -> Vec<f64> {
    let mut los: Vec<f64> = obs.iter().map(|o| o.lo_hz).collect();
    los.sort_by(f64::total_cmp);
    los.dedup_by(|a, b| (*a - *b).abs() <= tol_hz);
    los
}

/// Candidate groups under one slope: single-linkage clusters of the invariant coordinate.
fn clusters(
    obs: &[RetuneObservation],
    slope: RetuneSlope,
    tol: &RetuneTolerance,
) -> Vec<RetuneGroup> {
    let factor = slope.factor();
    let mut points: Vec<(f64, usize, f64)> = obs
        .iter()
        .enumerate()
        .map(|(i, o)| (o.f_center_hz - factor * o.lo_hz, i, tol.for_obs(o)))
        .collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut out = Vec::new();
    let mut run: Vec<(f64, usize, f64)> = Vec::new();
    for p in points {
        // The link tolerance grows with the slope: the invariant coordinate of an image is
        // `f − 2·f_LO`, so a centre error of `e` in `f` is still `e`, but a *mis-grouped* pair is
        // separated by `|slope|·Δf_LO`. Only the measurement error matters for linking.
        let link = match run.last() {
            Some(&(prev, _, prev_tol)) => (p.0 - prev).abs() <= prev_tol.max(p.2),
            None => true,
        };
        if !link {
            out.extend(finish(&mut run, slope, obs, tol));
        }
        run.push(p);
    }
    out.extend(finish(&mut run, slope, obs, tol));
    out
}

/// Turns a finished cluster into a group, if it spans enough distinct LOs.
fn finish(
    run: &mut Vec<(f64, usize, f64)>,
    slope: RetuneSlope,
    obs: &[RetuneObservation],
    tol: &RetuneTolerance,
) -> Option<RetuneGroup> {
    let points = std::mem::take(run);
    if points.is_empty() {
        return None;
    }
    let mut members: Vec<usize> = points.iter().map(|p| p.1).collect();
    members.sort_unstable();
    let los = distinct_los(
        &members.iter().map(|&i| obs[i]).collect::<Vec<_>>(),
        tol.min_tolerance_hz,
    );
    if los.len() < tol.min_centres {
        return None;
    }
    if slope.is_receiver_artefact()
        && !absolute_hypothesis_tested(
            &members.iter().map(|&i| obs[i]).collect::<Vec<_>>(),
            tol.min_tolerance_hz,
        )
    {
        return None;
    }
    let lo = points.first().map(|p| p.0).unwrap_or(f64::NAN);
    let hi = points.last().map(|p| p.0).unwrap_or(f64::NAN);
    let sum: f64 = points.iter().map(|p| p.0).sum();
    Some(RetuneGroup {
        slope,
        invariant_hz: sum / points.len() as f64,
        members,
        los_hz: los,
        spread_hz: hi - lo,
    })
}

/// Classifies every observation by how its absolute frequency tracks the LO.
///
/// Each slope in [`SLOPES`] is tried: the observations are projected into that slope's invariant
/// coordinate `f − slope·f_LO`, single-linkage clustered within tolerance, and a cluster spanning
/// at least [`RetuneTolerance::min_centres`] distinct LOs becomes a candidate group — and an
/// artefact slope only where the absolute hypothesis was actually tested (T-879: some member's
/// absolute frequency lay inside another centre's window; see `absolute_hypothesis_tested`). Candidates
/// then claim observations greedily, best first, and an observation already claimed cannot be
/// claimed again — so one line gets one verdict.
///
/// "Best" is, in order: more distinct centres (more diversity is more evidence), then a tighter
/// spread, then the earlier slope in [`SLOPES`] — which makes [`RetuneSlope::Absolute`] the
/// tie-break winner, so a line is only called a receiver artefact when the artefact geometry fits
/// it *better* than standing still does. Claiming an observation can drop a candidate below
/// `min_centres`, in which case the candidate is dropped rather than shrunk: a verdict is only
/// ever given on the diversity that actually supported it.
pub fn classify(obs: &[RetuneObservation], tol: &RetuneTolerance) -> RetuneSummary {
    let los_hz = distinct_los(obs, tol.min_tolerance_hz);
    let mut summary = RetuneSummary {
        observations: obs.len(),
        los_hz,
        ..RetuneSummary::default()
    };
    if summary.centres() < tol.min_centres {
        summary.unexplained = (0..obs.len()).collect();
        return summary;
    }

    let mut candidates: Vec<RetuneGroup> =
        SLOPES.iter().flat_map(|&s| clusters(obs, s, tol)).collect();
    candidates.sort_by(|a, b| {
        b.centres()
            .cmp(&a.centres())
            .then(a.spread_hz.total_cmp(&b.spread_hz))
            .then_with(|| slope_rank(a.slope).cmp(&slope_rank(b.slope)))
    });

    let mut taken = vec![false; obs.len()];
    for mut g in candidates {
        g.members.retain(|&i| !taken[i]);
        if g.members.is_empty() {
            continue;
        }
        let kept: Vec<RetuneObservation> = g.members.iter().map(|&i| obs[i]).collect();
        g.los_hz = distinct_los(&kept, tol.min_tolerance_hz);
        if g.los_hz.len() < tol.min_centres
            || (g.slope.is_receiver_artefact()
                && !absolute_hypothesis_tested(&kept, tol.min_tolerance_hz))
        {
            continue;
        }
        let factor = g.slope.factor();
        let mut coords: Vec<f64> = kept
            .iter()
            .map(|o| o.f_center_hz - factor * o.lo_hz)
            .collect();
        coords.sort_by(f64::total_cmp);
        g.invariant_hz = coords.iter().sum::<f64>() / coords.len() as f64;
        g.spread_hz = coords[coords.len() - 1] - coords[0];
        for &i in &g.members {
            taken[i] = true;
        }
        summary.groups.push(g);
    }
    summary.groups.sort_by(|a, b| {
        slope_rank(a.slope)
            .cmp(&slope_rank(b.slope))
            .then(a.invariant_hz.total_cmp(&b.invariant_hz))
    });
    summary.unexplained = (0..obs.len()).filter(|&i| !taken[i]).collect();
    summary
}

fn slope_rank(s: RetuneSlope) -> usize {
    SLOPES.iter().position(|&x| x == s).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOS: [f64; 3] = [100.0e6, 100.5e6, 101.0e6];

    /// Window width of every observation built by [`obs`]: wide enough that each centre in
    /// [`LOS`] sees the others' spectrum, as a real retune-diversity survey is laid out.
    const SPAN: f64 = 2.0e6;

    fn obs(lo: f64, f: f64) -> RetuneObservation {
        RetuneObservation {
            lo_hz: lo,
            f_center_hz: f,
            bandwidth_hz: 0.0,
            span_hz: Some(SPAN),
        }
    }

    /// **T-879: an artefact verdict needs windows that could have refuted it.** Three emitters on
    /// three channels, each seen only from its own centre at the same offset from it (500 kHz
    /// windows 1.5 MHz apart), fit "LO-locked" exactly — and so does one spur. Nothing measured
    /// separates them, so nothing is claimed. The same lines under windows that overlap are the
    /// spur, and are called one.
    #[test]
    fn a_common_lo_offset_in_windows_that_share_no_spectrum_is_no_verdict() {
        let los = [433.92e6, 435.42e6, 436.92e6];
        let at = |span: Option<f64>| -> Vec<RetuneObservation> {
            los.iter()
                .map(|&lo| RetuneObservation {
                    span_hz: span,
                    ..obs(lo, lo + 53e3)
                })
                .collect()
        };
        let tol = RetuneTolerance::default();

        for span in [Some(500e3), None] {
            let s = classify(&at(span), &tol);
            assert_eq!(s.centres(), 3, "three centres were compared");
            assert_eq!(
                s.with_slope(RetuneSlope::LoLocked).count(),
                0,
                "span {span:?}: no window covered another's line, so moving with the LO was never                  tested: {s:?}"
            );
            assert_eq!(s.unexplained.len(), 3, "{s:?}");
        }

        // Windows wide enough to see each other's line: the same geometry is now a measurement.
        let s = classify(&at(Some(4e6)), &tol);
        let g = s
            .find(RetuneSlope::LoLocked, 53e3, 1e3)
            .expect("overlapping windows measure the slope");
        assert_eq!(g.centres(), 3);
    }

    /// A line at a fixed absolute frequency, seen from each LO.
    fn emission(f: f64) -> Vec<RetuneObservation> {
        LOS.iter().map(|&lo| obs(lo, f)).collect()
    }

    /// A line at a fixed offset from each LO.
    fn lo_locked(offset: f64) -> Vec<RetuneObservation> {
        LOS.iter().map(|&lo| obs(lo, lo + offset)).collect()
    }

    /// The IQ image of an emission at `f`, seen from each LO.
    fn image(f: f64) -> Vec<RetuneObservation> {
        LOS.iter().map(|&lo| obs(lo, 2.0 * lo - f)).collect()
    }

    #[test]
    fn a_fixed_frequency_line_is_absolute_and_a_fixed_offset_one_is_lo_locked() {
        let mut o = emission(99.78e6);
        o.extend(lo_locked(0.0)); // DC / LO leakage
        o.extend(lo_locked(370e3)); // internal spur at a fixed IF offset
        let s = classify(&o, &RetuneTolerance::default());
        assert_eq!(s.centres(), 3, "three centres were offered");
        assert_eq!(s.unexplained, Vec::<usize>::new());

        let real = s
            .find(RetuneSlope::Absolute, 99.78e6, 1e3)
            .expect("emitter");
        assert_eq!(real.centres(), 3);
        let dc = s.find(RetuneSlope::LoLocked, 0.0, 1e3).expect("dc");
        assert_eq!(dc.centres(), 3);
        let spur = s.find(RetuneSlope::LoLocked, 370e3, 1e3).expect("spur");
        assert_eq!(spur.centres(), 3);
        assert_eq!(s.with_slope(RetuneSlope::Absolute).count(), 1);
        assert_eq!(s.with_slope(RetuneSlope::LoLocked).count(), 2);
    }

    #[test]
    fn an_iq_image_is_found_at_twice_the_lo() {
        let mut o = emission(99.78e6);
        o.extend(image(99.78e6));
        let s = classify(&o, &RetuneTolerance::default());
        let img = s
            .find(RetuneSlope::Image, -99.78e6, 1e3)
            .expect("image group");
        assert_eq!(img.centres(), 3);
        assert!(img.slope.is_receiver_artefact());
        assert_eq!(s.with_slope(RetuneSlope::Absolute).count(), 1);
    }

    #[test]
    fn one_centre_yields_no_verdict_at_all() {
        let o: Vec<_> = [99.78e6, 100.0e6, 100.37e6]
            .iter()
            .map(|&f| obs(100.0e6, f))
            .collect();
        let s = classify(&o, &RetuneTolerance::default());
        assert_eq!(s.centres(), 1);
        assert!(s.groups.is_empty(), "no diversity, so no claim: {s:?}");
        assert_eq!(s.unexplained.len(), 3);
    }

    #[test]
    fn a_line_seen_from_only_one_centre_is_unexplained_not_absolute() {
        let mut o = emission(99.78e6);
        o.push(obs(100.5e6, 100.94e6)); // a burst, present for one dwell only
        let s = classify(&o, &RetuneTolerance::default());
        assert_eq!(s.unexplained, vec![3]);
        assert_eq!(s.with_slope(RetuneSlope::Absolute).count(), 1);
    }

    #[test]
    fn a_standing_line_beats_the_artefact_geometry_that_also_fits_it() {
        // Standing still is the null hypothesis: a line is only called a receiver artefact when
        // an artefact geometry fits it better, never merely as well.
        let o = emission(100.5e6);
        let s = classify(&o, &RetuneTolerance::default());
        assert_eq!(s.groups.len(), 1);
        assert_eq!(s.groups[0].slope, RetuneSlope::Absolute);
    }

    #[test]
    fn measurement_scatter_inside_tolerance_does_not_break_a_group() {
        let jitter = [-4e3, 0.0, 4e3];
        let o: Vec<_> = LOS
            .iter()
            .zip(jitter)
            .map(|(&lo, j)| obs(lo, 99.78e6 + j))
            .collect();
        let s = classify(&o, &RetuneTolerance::default());
        let g = s.find(RetuneSlope::Absolute, 99.78e6, 5e3).expect("group");
        assert_eq!(g.centres(), 3);
        assert!(g.spread_hz <= 8e3 + 1.0, "spread {}", g.spread_hz);
    }

    #[test]
    fn the_verdict_sets_the_documented_flags() {
        let mut f = DetectionFlags::default();
        RetuneSlope::Absolute.apply(&mut f);
        assert_eq!(f, DetectionFlags::default());

        let mut f = DetectionFlags::default();
        RetuneSlope::LoLocked.apply(&mut f);
        assert!(f.spur_candidate);
        assert_eq!(f.spur_reason, Some(SpurReason::LoRelative));
        assert!(f.inconsistency().is_none());

        let mut f = DetectionFlags::default();
        RetuneSlope::Image.apply(&mut f);
        assert!(f.image_candidate && f.image_retune_confirmed);
        assert!(f.inconsistency().is_none());
    }

    #[test]
    fn an_existing_spur_reason_is_not_overwritten_by_the_retune_verdict() {
        let mut f = DetectionFlags {
            spur_candidate: true,
            spur_reason: Some(SpurReason::Dc),
            ..DetectionFlags::default()
        };
        RetuneSlope::LoLocked.apply(&mut f);
        assert_eq!(f.spur_reason, Some(SpurReason::Dc));
    }
}
