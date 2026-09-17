//! The coverage map (T-368): what a front end **actually sampled**, so a view can grey only the
//! cells nothing ever looked at.
//!
//! # The rule this serves
//!
//! The user's invariant (CLAUDE.md, "Time, the waterfall, and the live view"):
//!
//! > **The waterfall shows the data that exists for the selected (time, frequency); grey means
//! > genuinely unobserved.** … This requires the backend to keep a **coverage map derived from the
//! > SDR configuration/tune history** — for each interval, which centre/span/rate (and which
//! > device) was active — so observed-vs-unobserved is computed from what was actually sampled.
//!
//! # Three states, and the third is not the second
//!
//! | State | Meaning | Type |
//! |---|---|---|
//! | 1 | observed, and there was energy | [`Coverage::Observed`], with the measurement beside it |
//! | 2 | observed, and it was quiet — **a finding** | [`Coverage::Observed`], measurement low |
//! | 3 | **never observed** — no claim either way | [`Coverage::Unobserved`] |
//!
//! States 2 and 3 are the pair that gets collapsed, and collapsing them invents an
//! absence-of-signal finding out of an absence of measurement. So this module makes state 3
//! **unrepresentable as state 2**: the only way to reach [`Coverage::Observed`] is
//! [`Coverage::of`], which refuses a span count of zero and a sampled duration of zero and hands
//! back [`Coverage::Unobserved`] instead. There is no value of [`Sampled`] that means "nothing was
//! sampled", so no amount of zeroing a field turns an un-looked-at cell into a quiet one. It is the
//! same rule as `BiasTee::Unknown` ≠ `Off`: **nothing said is never permissive.**
//!
//! # Device-local, never unioned
//!
//! Coverage is a fact about **one front end**. Two radios covering different ranges must not have
//! their coverage silently merged into a claim that either one saw both (T-259/T-305: device-local
//! physics reads the device). So every span carries its [`Device`], [`by_device`] returns one grid
//! per distinct device and never a merged one, and [`Device::Unknown`] — a span whose record did
//! not say which radio produced it — is its **own** device: it never matches a query for a named
//! front end and never folds into one. A union across devices is available only by asking for it
//! ([`union_grid`]), and the grid it returns says [`Device::Any`] so nothing can mistake it for one
//! radio's coverage.
//!
//! # Derived, not invented
//!
//! A [`CoverageSpan`] is not a new ledger to maintain. It is the shape that already-written
//! provenance takes when read as coverage: the IQ ring journal's segments (`t0`/`t1`/`center_hz`/
//! `sample_rate_hz`/`device_id`, one per provenance change — so one per retune, by construction)
//! and the observation log's dwell and sweep windows (`ObservedWindow::covered()` over
//! `DwellRecord::observed`). Both already record "for each interval, which centre/span/rate was
//! active"; the first also records which device. The caller collects the spans from whichever of
//! those it has and this module folds them.

use std::collections::BTreeMap;

use hk_model::{FreqRange, TimeRange, Timestamp};

/// Most frequency cells one [`CoverageGrid`] may hold. Each cell is a measurement, and the fold is
/// linear in `spans × cells_touched`, so the grid is bounded like every other served picture.
pub const MAX_COVERAGE_CELLS: usize = 4096;

/// Which front end a coverage claim belongs to.
///
/// [`Device::Unknown`] is **not** a wildcard and **not** a device: a span whose record did not name
/// the radio is evidence that *something* sampled there, and nothing more. It never satisfies a
/// query for a named front end, and it never merges into one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Device {
    /// A named front end (the `device_id` of the provenance that produced the samples).
    Id(String),
    /// The record did not say which front end sampled here.
    Unknown,
    /// Every front end at once — the label of a deliberately requested union ([`union_grid`]),
    /// never the label of one radio's coverage.
    Any,
}

impl Device {
    /// The wire value: the device id, `"unknown"`, or `"any"`.
    pub fn as_str(&self) -> &str {
        match self {
            Device::Id(s) => s.as_str(),
            Device::Unknown => "unknown",
            Device::Any => "any",
        }
    }

    /// Whether this is a named front end (so a caller can tell a real identity from the two
    /// labels that are not one).
    pub fn is_named(&self) -> bool {
        matches!(self, Device::Id(_))
    }
}

/// One interval a front end actually sampled, at one tune.
///
/// This is the coverage-shaped read of provenance that already exists — an IQ-ring segment, or an
/// observation-log dwell/sweep window — not a record anything has to start keeping.
#[derive(Clone, Debug, PartialEq)]
pub struct CoverageSpan {
    /// Which front end sampled it.
    pub device: Device,
    /// The interval sampled.
    pub time: TimeRange,
    /// The frequency extent this tune actually covered (the usable band, DC notch already removed
    /// by the producer — a `CoverageSpan` is a positive claim about what was sampled, so a notched
    /// window contributes two spans, not one wide one).
    pub freq: FreqRange,
    /// Tuned RF centre, Hz — the configuration in force, carried so a view can say *why* a cell is
    /// covered and at what resolution.
    pub center_hz: f64,
    /// Sample rate, Hz: the instantaneous bandwidth this interval was sampled at.
    pub sample_rate_hz: f64,
}

impl CoverageSpan {
    /// Whether the span is usable: a positive interval over a positive, finite frequency extent.
    /// A degenerate span is dropped rather than folded, because a zero-width claim of coverage is
    /// not coverage.
    pub fn is_valid(&self) -> bool {
        self.time.duration_ns() > 0
            && self.freq.lo_hz.is_finite()
            && self.freq.hi_hz > self.freq.lo_hz
    }
}

/// What was actually sampled in one cell. Every field is a **positive** fact about sampling that
/// happened; there is no value of this type that means "nothing was sampled".
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct Sampled {
    /// Distinct sampling intervals that covered the cell (after merging overlaps).
    pub spans: u32,
    /// Nanoseconds of the window actually sampled. Always `> 0`.
    pub observed_ns: i64,
    /// `observed_ns` as a fraction of the window, `0 < duty ≤ 1`. A cell sampled for part of the
    /// window is observed for that part and unobserved for the rest — reported, not rounded away.
    pub duty: f64,
    /// End of the newest sampling interval: "when this cell was last looked at".
    pub last: Timestamp,
    /// Tuned centre of the newest interval, Hz.
    pub center_hz: f64,
    /// Widest sample rate any covering interval used, Hz.
    pub sample_rate_hz: f64,
}

/// What is known about one cell of the coverage map.
///
/// The two variants are the whole contract: an absence of measurement is a **different value** from
/// a measurement of nothing, at every layer that carries this type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Coverage {
    /// Never sampled here, by this device, in this window. **No claim** about what is on the air —
    /// this is the cell a view greys.
    Unobserved,
    /// Sampled: the front end was tuned here, for this long. Whether anything was *on* the air is a
    /// separate measurement, and a quiet answer to it is a finding.
    Observed(Sampled),
}

impl Coverage {
    /// The only way to build a [`Coverage::Observed`], and the reason state 3 cannot be spelled as
    /// state 2.
    ///
    /// Returns [`Coverage::Unobserved`] — not a zeroed [`Sampled`] — whenever nothing was sampled:
    /// no spans, a non-positive sampled duration, or a non-positive window. A caller that wanted to
    /// say "observed, and quiet" therefore *cannot* say it by accident; it has to have a real
    /// sampled interval to say it with.
    ///
    /// [`Sampled`] is `#[non_exhaustive]`, so no crate outside `hk-store` can spell one with a
    /// struct literal — this function is the only door in. Its fields stay public because reading
    /// them is the point; it is *writing* one that has to go through the check above. Inside this
    /// crate the literal is reachable, and this is its one construction site: a second one here
    /// would be a way to say "observed" without having observed anything.
    pub fn of(
        spans: u32,
        observed_ns: i64,
        window_ns: i64,
        last: Timestamp,
        center_hz: f64,
        sample_rate_hz: f64,
    ) -> Coverage {
        if spans == 0 || observed_ns <= 0 || window_ns <= 0 {
            return Coverage::Unobserved;
        }
        Coverage::Observed(Sampled {
            spans,
            observed_ns,
            duty: (observed_ns as f64 / window_ns as f64).clamp(f64::MIN_POSITIVE, 1.0),
            last,
            center_hz,
            sample_rate_hz,
        })
    }

    /// Whether anything ever looked here.
    pub fn is_observed(&self) -> bool {
        matches!(self, Coverage::Observed(_))
    }

    /// The sampling behind an observed cell; `None` when nothing looked.
    pub fn sampled(&self) -> Option<&Sampled> {
        match self {
            Coverage::Observed(s) => Some(s),
            Coverage::Unobserved => None,
        }
    }

    /// The wire value: `"observed"` or `"unobserved"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Coverage::Observed(_) => "observed",
            Coverage::Unobserved => "unobserved",
        }
    }
}

/// One device's coverage of a band over a window, in `cells` frequency cells.
#[derive(Clone, Debug, PartialEq)]
pub struct CoverageGrid {
    /// Whose coverage this is. Never a merge of two named devices (see [`union_grid`]).
    pub device: Device,
    /// The window the duties are fractions of.
    pub window: TimeRange,
    /// Low edge of cell 0, Hz.
    pub f_lo_hz: f64,
    /// Cell width, Hz.
    pub f_cell_hz: f64,
    /// The cells, low frequency first.
    pub cells: Vec<Coverage>,
}

impl CoverageGrid {
    /// Cells something sampled.
    pub fn observed_cells(&self) -> usize {
        self.cells.iter().filter(|c| c.is_observed()).count()
    }

    /// Cells nothing ever sampled — the grey ones.
    pub fn unobserved_cells(&self) -> usize {
        self.cells.len() - self.observed_cells()
    }

    /// The fraction of the band this device observed; `0.0` for an empty grid.
    pub fn observed_fraction(&self) -> f64 {
        if self.cells.is_empty() {
            return 0.0;
        }
        self.observed_cells() as f64 / self.cells.len() as f64
    }

    /// The cell containing `f_hz`, or `None` outside the grid.
    pub fn at(&self, f_hz: f64) -> Option<&Coverage> {
        if !(f_hz.is_finite() && self.f_cell_hz > 0.0) {
            return None;
        }
        let i = ((f_hz - self.f_lo_hz) / self.f_cell_hz).floor();
        if i < 0.0 {
            return None;
        }
        self.cells.get(i as usize)
    }
}

/// Merged sampled time per cell while folding.
#[derive(Default)]
struct Acc {
    intervals: Vec<(i64, i64)>,
    center_hz: f64,
    sample_rate_hz: f64,
    last_ns: i64,
}

impl Acc {
    fn push(&mut self, t0: i64, t1: i64, s: &CoverageSpan) {
        self.intervals.push((t0, t1));
        if t1 >= self.last_ns {
            self.last_ns = t1;
            self.center_hz = s.center_hz;
        }
        if s.sample_rate_hz > self.sample_rate_hz {
            self.sample_rate_hz = s.sample_rate_hz;
        }
    }

    /// Merged duration and merged-interval count. Overlapping dwells on the same cell are one
    /// observation of it, not two — double-counting them would report a duty above 1.
    fn merged(&mut self) -> (u32, i64) {
        self.intervals.sort_unstable();
        let (mut n, mut total) = (0u32, 0i64);
        let mut cur: Option<(i64, i64)> = None;
        for &(a, b) in &self.intervals {
            match cur {
                Some((s, e)) if a <= e => cur = Some((s, e.max(b))),
                Some((s, e)) => {
                    total += e - s;
                    n += 1;
                    cur = Some((a, b));
                }
                None => cur = Some((a, b)),
            }
        }
        if let Some((s, e)) = cur {
            total += e - s;
            n += 1;
        }
        (n, total)
    }
}

/// Folds `spans` into one device's coverage grid over `freq` × `window`.
///
/// Only spans whose [`CoverageSpan::device`] equals `device` contribute — including
/// [`Device::Unknown`], which matches only itself. `cells` is clamped to `1..=`[`MAX_COVERAGE_CELLS`].
pub fn grid(
    spans: &[CoverageSpan],
    device: &Device,
    freq: FreqRange,
    window: TimeRange,
    cells: usize,
) -> CoverageGrid {
    fold(spans, device.clone(), freq, window, cells, |s| {
        s.device == *device
    })
}

/// Folds **every** span into one grid, whatever device produced it, and labels the result
/// [`Device::Any`].
///
/// This is the union, and it exists only because a caller sometimes genuinely wants "did *anything*
/// look here". It is deliberately a separate function with a label of its own: a union that came
/// back wearing one radio's `device_id` would be the exact claim T-259/T-305 forbid.
pub fn union_grid(
    spans: &[CoverageSpan],
    freq: FreqRange,
    window: TimeRange,
    cells: usize,
) -> CoverageGrid {
    fold(spans, Device::Any, freq, window, cells, |_| true)
}

fn fold(
    spans: &[CoverageSpan],
    label: Device,
    freq: FreqRange,
    window: TimeRange,
    cells: usize,
    keep: impl Fn(&CoverageSpan) -> bool,
) -> CoverageGrid {
    let n = cells.clamp(1, MAX_COVERAGE_CELLS);
    let width = freq.hi_hz - freq.lo_hz;
    let f_cell_hz = if width.is_finite() && width > 0.0 {
        width / n as f64
    } else {
        0.0
    };
    let window_ns = window.duration_ns();
    let mut acc: Vec<Acc> = (0..n).map(|_| Acc::default()).collect();
    if f_cell_hz > 0.0 && window_ns > 0 {
        for s in spans.iter().filter(|s| s.is_valid() && keep(s)) {
            let t0 = s
                .time
                .start
                .as_unix_nanos()
                .max(window.start.as_unix_nanos());
            let t1 = s.time.end.as_unix_nanos().min(window.end.as_unix_nanos());
            if t1 <= t0 {
                continue;
            }
            // Cells the span's extent touches at all: a tune that covered any part of a cell
            // observed that part of it, and reporting the cell unobserved would grey a band the
            // radio was demonstrably sitting on.
            let lo = ((s.freq.lo_hz - freq.lo_hz) / f_cell_hz).floor();
            let hi = ((s.freq.hi_hz - freq.lo_hz) / f_cell_hz).ceil();
            let lo = lo.max(0.0) as usize;
            let hi = (hi.max(0.0) as usize).min(n);
            for a in acc.iter_mut().take(hi).skip(lo) {
                a.push(t0, t1, s);
            }
        }
    }
    let cells = acc
        .iter_mut()
        .map(|a| {
            let (n_merged, observed_ns) = a.merged();
            Coverage::of(
                n_merged,
                observed_ns,
                window_ns,
                Timestamp::from_unix_nanos(a.last_ns),
                a.center_hz,
                a.sample_rate_hz,
            )
        })
        .collect();
    CoverageGrid {
        device: label,
        window,
        f_lo_hz: freq.lo_hz,
        f_cell_hz,
        cells,
    }
}

/// One grid **per distinct device** in `spans`, ordered by device, never merged.
///
/// Two front ends covering disjoint ranges come back as two grids, each unobserved where the other
/// looked. There is no code path here that produces a single grid from two named devices.
pub fn by_device(
    spans: &[CoverageSpan],
    freq: FreqRange,
    window: TimeRange,
    cells: usize,
) -> Vec<CoverageGrid> {
    let mut devices: BTreeMap<Device, ()> = BTreeMap::new();
    for s in spans.iter().filter(|s| s.is_valid()) {
        devices.insert(s.device.clone(), ());
    }
    devices
        .keys()
        .map(|d| grid(spans, d, freq, window, cells))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    fn window() -> TimeRange {
        TimeRange::new(t(1000), t(1060))
    }

    fn band() -> FreqRange {
        // 100 MHz wide, so a 10-cell grid has 10 MHz cells.
        FreqRange::new(100e6, 200e6)
    }

    fn span(device: &str, lo: f64, hi: f64, t0: i64, t1: i64) -> CoverageSpan {
        CoverageSpan {
            device: Device::Id(device.into()),
            time: TimeRange::new(t(t0), t(t1)),
            freq: FreqRange::new(lo, hi),
            center_hz: (lo + hi) / 2.0,
            sample_rate_hz: hi - lo,
        }
    }

    /// The mutation the control uses: **assume coverage everywhere.** One span over the whole band
    /// for the whole window, claiming the radio was always sitting on all of it.
    fn assume_everywhere(freq: FreqRange, w: TimeRange) -> Vec<CoverageSpan> {
        vec![CoverageSpan {
            device: Device::Id("dev-a".into()),
            time: w,
            freq,
            center_hz: (freq.lo_hz + freq.hi_hz) / 2.0,
            sample_rate_hz: freq.hi_hz - freq.lo_hz,
        }]
    }

    /// **The property.** A band the device demonstrably never tuned reads unobserved; a band it
    /// tuned reads observed — and the two are *different values*, not the same null.
    #[test]
    fn a_band_never_tuned_is_unobserved_and_a_band_tuned_is_observed() {
        // The radio sat on 100–110 MHz for 30 of the 60 s, and never went anywhere else.
        let spans = vec![span("dev-a", 100e6, 110e6, 1000, 1030)];
        let g = grid(&spans, &Device::Id("dev-a".into()), band(), window(), 10);

        // Cell 0 (100–110 MHz): observed, with the sampling that proves it.
        let looked = g.at(105e6).expect("cell 0");
        let s = looked.sampled().expect("cell 0 was sampled");
        assert_eq!(s.observed_ns, 30_000_000_000, "30 s of the 60 s window");
        assert_eq!(s.duty, 0.5);
        assert_eq!(s.last, t(1030));
        assert_eq!(s.center_hz, 105e6);
        assert_eq!(s.sample_rate_hz, 10e6);

        // Cell 5 (150–160 MHz): never tuned. A *different value*, carrying no measurement at all —
        // not `Observed` with zeros, which is what "quiet" would look like.
        assert_eq!(*g.at(155e6).expect("cell 5"), Coverage::Unobserved);
        assert_ne!(*g.at(155e6).unwrap(), *looked);
        assert!(g.at(155e6).unwrap().sampled().is_none());

        assert_eq!(g.observed_cells(), 1);
        assert_eq!(g.unobserved_cells(), 9);
        assert_eq!(g.observed_fraction(), 0.1);
    }

    /// **The control that matters.** A never-observed region must not be servable as quiet. Mutate
    /// the map so coverage is assumed everywhere, and the assertion above has to fail — otherwise
    /// it was never testing anything.
    #[test]
    fn assuming_coverage_everywhere_breaks_the_property() {
        let honest = vec![span("dev-a", 100e6, 110e6, 1000, 1030)];
        let dev = Device::Id("dev-a".into());
        let g = grid(&honest, &dev, band(), window(), 10);
        assert_eq!(*g.at(155e6).unwrap(), Coverage::Unobserved, "honest map");

        let assumed = assume_everywhere(band(), window());
        let m = grid(&assumed, &dev, band(), window(), 10);
        // The mutant claims the never-tuned cell was looked at — which is exactly the failure the
        // honest assertion forbids, so the honest assertion is load-bearing.
        assert!(
            m.at(155e6).unwrap().is_observed(),
            "the assumed-everywhere mutant must claim the never-tuned cell"
        );
        assert_eq!(m.unobserved_cells(), 0);
        assert_ne!(g.cells, m.cells);
    }

    /// **Device-local.** Two devices covering disjoint ranges do not union into one device's
    /// coverage: each is unobserved exactly where the other looked.
    #[test]
    fn two_devices_on_disjoint_ranges_do_not_union() {
        let spans = vec![
            span("hackrf:aaa", 100e6, 110e6, 1000, 1060),
            span("hackrf:bbb", 190e6, 200e6, 1000, 1060),
        ];
        let a = grid(
            &spans,
            &Device::Id("hackrf:aaa".into()),
            band(),
            window(),
            10,
        );
        let b = grid(
            &spans,
            &Device::Id("hackrf:bbb".into()),
            band(),
            window(),
            10,
        );

        assert!(a.at(105e6).unwrap().is_observed(), "a looked at 105 MHz");
        assert_eq!(
            *a.at(195e6).unwrap(),
            Coverage::Unobserved,
            "a never did 195"
        );
        assert!(b.at(195e6).unwrap().is_observed(), "b looked at 195 MHz");
        assert_eq!(
            *b.at(105e6).unwrap(),
            Coverage::Unobserved,
            "b never did 105"
        );
        assert_eq!((a.observed_cells(), b.observed_cells()), (1, 1));

        // `by_device` keeps them apart, and the union is only what someone asked for by name.
        let per = by_device(&spans, band(), window(), 10);
        assert_eq!(per.len(), 2);
        assert_eq!(
            per.iter().map(|g| g.device.clone()).collect::<Vec<_>>(),
            vec![
                Device::Id("hackrf:aaa".into()),
                Device::Id("hackrf:bbb".into())
            ]
        );
        assert!(per.iter().all(|g| g.observed_cells() == 1));
        let u = union_grid(&spans, band(), window(), 10);
        assert_eq!(u.device, Device::Any);
        assert_eq!(u.observed_cells(), 2);
    }

    /// An unnamed span is evidence that *something* looked, and nothing more: it never answers for
    /// a named front end. (`BiasTee::Unknown` ≠ `Off`, restated for identity.)
    #[test]
    fn unknown_device_coverage_never_answers_for_a_named_one() {
        let spans = vec![CoverageSpan {
            device: Device::Unknown,
            ..span("ignored", 100e6, 110e6, 1000, 1060)
        }];
        let named = grid(
            &spans,
            &Device::Id("hackrf:aaa".into()),
            band(),
            window(),
            10,
        );
        assert_eq!(named.observed_cells(), 0, "unknown is not hackrf:aaa");
        let unknown = grid(&spans, &Device::Unknown, band(), window(), 10);
        assert_eq!(unknown.observed_cells(), 1);
        assert!(!Device::Unknown.is_named());
        assert!(!Device::Any.is_named());
        assert!(Device::Id("x".into()).is_named());
    }

    /// State 3 is unrepresentable as state 2: the only constructor refuses to mint a `Sampled` that
    /// means "nothing was sampled", so no zeroed measurement can pass for a quiet one.
    #[test]
    fn nothing_sampled_cannot_be_built_as_an_observation() {
        let z = Timestamp::from_unix_nanos(0);
        assert_eq!(
            Coverage::of(0, 60_000_000_000, 60_000_000_000, z, 1e8, 2e6),
            Coverage::Unobserved,
            "no spans"
        );
        assert_eq!(
            Coverage::of(1, 0, 60_000_000_000, z, 1e8, 2e6),
            Coverage::Unobserved,
            "no sampled duration"
        );
        assert_eq!(
            Coverage::of(1, 60_000_000_000, 0, z, 1e8, 2e6),
            Coverage::Unobserved,
            "no window"
        );
        // A real, brief look is observed — with a duty that says how brief, never rounded to zero.
        let c = Coverage::of(1, 1, 60_000_000_000, z, 1e8, 2e6);
        assert!(c.is_observed());
        assert!(c.sampled().unwrap().duty > 0.0);
        assert_eq!(c.as_str(), "observed");
        assert_eq!(Coverage::Unobserved.as_str(), "unobserved");
    }

    /// Overlapping dwells on one cell are one observation of it, so the duty never exceeds 1, and a
    /// gap between two looks is not counted as observed.
    #[test]
    fn overlapping_looks_merge_and_gaps_do_not_count_as_observed() {
        let spans = vec![
            span("dev-a", 100e6, 110e6, 1000, 1020),
            span("dev-a", 100e6, 110e6, 1010, 1030),
            span("dev-a", 100e6, 110e6, 1050, 1060),
        ];
        let g = grid(&spans, &Device::Id("dev-a".into()), band(), window(), 10);
        let s = g.at(105e6).unwrap().sampled().unwrap();
        assert_eq!(s.spans, 2, "two merged runs, not three records");
        assert_eq!(s.observed_ns, 40_000_000_000, "30 s + 10 s, not 50 s");
        assert!((s.duty - 40.0 / 60.0).abs() < 1e-12);
        assert_eq!(s.last, t(1060));
    }

    /// A span is clipped to the window: coverage outside the window is not coverage inside it.
    #[test]
    fn coverage_outside_the_window_is_not_coverage_inside_it() {
        let before = vec![span("dev-a", 100e6, 110e6, 900, 990)];
        let g = grid(&before, &Device::Id("dev-a".into()), band(), window(), 10);
        assert_eq!(*g.at(105e6).unwrap(), Coverage::Unobserved);
        let straddling = vec![span("dev-a", 100e6, 110e6, 990, 1015)];
        let g = grid(
            &straddling,
            &Device::Id("dev-a".into()),
            band(),
            window(),
            10,
        );
        assert_eq!(
            g.at(105e6).unwrap().sampled().unwrap().observed_ns,
            15_000_000_000,
            "only the part inside the window"
        );
    }
}
