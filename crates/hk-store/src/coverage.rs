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
//! active", and since T-378 both record **which device** — the same `device_id` the baseline chain
//! key and the history source key are hashed from — so the long-horizon spans are device-local too.
//! The caller collects the spans from whichever of those it has and this module folds them.
//!
//! # The time axis (T-421, `docs/16` §6.3 / §7 step 1)
//!
//! T-368 folded those spans onto the **frequency** axis and collapsed time, so one grid answered
//! *"was this band sampled anywhere in this window"* — a **column**, where a view drawing a
//! waterfall needs a **cell**. The consequence was not cosmetic: a cell in a band the radio
//! demonstrably watched, at an instant it was tuned somewhere else, came back `Observed` and read as
//! *"sampled, level not retained"* rather than grey.
//!
//! This is not new data and not a new type. A [`CoverageSpan`] is already
//! `(t_start, t_end, f_lo, f_hi, device)`; **per-(t, f) coverage is the same rasterisation with the
//! time axis kept.** [`grid_over`] lays `nt × nf` cells on the window instead of `1 × nf`, and each
//! cell goes through the same [`Coverage::of`] with its **own** extent as the window — so the
//! refusal that makes state 3 unrepresentable as state 2 applies per cell, unchanged, and the
//! one-row case *is* [`grid`], bit for bit.
//!
//! # Coarsening is a SUM over the children, never a best-of (T-419)
//!
//! [`CoverageGrid::coarsen`] is the fold, and it exists because the obvious alternative is wrong:
//! rasterising **directly** onto a coarse grid answers *"did anything look anywhere in this cell"*
//! and therefore reports a cell **fully covered** on the evidence of one sixteenth of its frequency
//! extent — the exact shape T-419 removed from `Tile::fold_child`, which had been taking the max
//! across child frequency cells. `observed_ns` is foldable; `duty` is not. So a parent's
//! `observed_ns` is the **sum** over its children divided by the number of frequency children (the
//! parent's own extent), and its `duty` is recomputed from that against the parent's own duration —
//! never averaged from the children's duties. This module's tests prove every level against
//! **level 0**, independently, because a parent-vs-child assertion is satisfied by the rounded-up
//! answer at every level and so can only ever prove that the fold is *a* fold.
//!
//! # The fourth state: `Unobserved` here is *"no record"*, which past the horizon is not *"never"*
//!
//! `docs/16` §5.4. The spectrum-history pyramid has **no age limit** (a rolling byte budget) while
//! the observation log expires at [`crate::observation::DEFAULT_MAX_AGE_NS`] (**180 days** since
//! T-406, up from 30), so the two can still cross: a cell older than the record
//! horizon can hold a measurement whose coverage record has been discarded. A cell this module
//! calls [`Coverage::Unobserved`] means *no surviving record covers it* — inside the horizon that is
//! "nothing looked", and **beyond it that is "we no longer know whether we looked"**, which must not
//! be painted grey. This module cannot decide which, because it is handed spans and not the horizon
//! that produced them; [`CoverageGrid::unknown_rows_before`] is how a caller names the boundary, and
//! it is deliberately a *question about rows* rather than a third [`Coverage`] variant — adding one
//! is a `core_interface` change `docs/16` §5.4 says to make deliberately, if at all.

use std::collections::BTreeMap;

use hk_model::attention::observation::{ObservationRecord, ObservedWindow, SweepGeometry};
use hk_model::{FreqRange, TimeRange, Timestamp};

/// Most frequency cells one [`CoverageGrid`] may hold. Each cell is a measurement, and the fold is
/// linear in `spans × cells_touched`, so the grid is bounded like every other served picture.
pub const MAX_COVERAGE_CELLS: usize = 4096;

/// Most time rows one [`CoverageGrid`] may hold — the same bound on the axis T-421 added.
pub const MAX_COVERAGE_ROWS: usize = 4096;

/// Most cells in total, `nt × nf`. Each axis is bounded on its own, but the fold's cost is the
/// **product**, so the product is bounded too: 256 × 256, one tile of `docs/16` §6.2's view scheme.
/// A request past it keeps every frequency cell asked for and reduces the time rows, and the grid
/// reports the [`CoverageGrid::nt`] it actually built — the realised resolution is never implied.
pub const MAX_COVERAGE_GRID_CELLS: usize = 65_536;

/// Which front end a coverage claim belongs to.
///
/// [`Device::Unknown`] is **not** a wildcard and **not** a device: a span whose record did not name
/// the radio is evidence that *something* sampled there, and nothing more. It never satisfies a
/// query for a named front end, and it never merges into one. A record written before its producer
/// logged a device reads back here, never as the front end that happens to be running now.
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
    /// The frequency extent this span covers. A `CoverageSpan` is a positive claim about what was
    /// sampled, so a notched window contributes **three** spans — the two analysed sides and the
    /// notch itself as [`Analysis::Excluded`] (T-595) — never one wide one, and never a hole.
    pub freq: FreqRange,
    /// Whether the analysis ran on these samples, or the producer declared them excluded from it
    /// (T-595). [`Analysis::Analysed`] is the ordinary span; [`Analysis::Excluded`] is the DC notch.
    pub analysis: Analysis,
    /// Tuned RF centre, Hz — the configuration in force, carried so a view can say *why* a cell is
    /// covered and at what resolution.
    pub center_hz: f64,
    /// Sample rate, Hz: the instantaneous bandwidth this interval was sampled at.
    pub sample_rate_hz: f64,
}

/// Whether a span's samples reached the analysis, or were deliberately left out of it (T-595).
///
/// A [`CoverageSpan`] is a positive claim that a front end sampled a band over an interval. It is a
/// **separate** question whether the analysis then looked at those samples: a receiver excludes its
/// own DC/LO-leakage notch from detection, and the observation log records that exclusion
/// ([`ObservedWindow::dc_excluded`]). Before T-595 the notch simply did not become a span, so a
/// 25 kHz stripe down the middle of every band the radio ever sat on rasterised as
/// [`Coverage::Unobserved`] — *"nothing ever looked"* — painted over spectrum rows the history
/// pyramid holds. Both halves of the invariant broke at once: data that exists was not shown, and
/// grey stopped meaning genuinely unobserved.
///
/// So the notch is a span like any other, carrying the one thing that makes it different. The
/// alternative — declaring the notch analysed — would have made the observation log lie in the
/// other direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Analysis {
    /// Sampled, and the analysis ran on it. The ordinary span.
    #[default]
    Analysed,
    /// Sampled, and **deliberately excluded** from analysis — today only the receiver's own DC/LO
    /// notch. The samples exist and the spectrum history keeps rows here; no detection claim is
    /// made about them.
    Excluded,
}

impl Analysis {
    /// Whether the analysis ran here.
    pub fn is_analysed(self) -> bool {
        matches!(self, Analysis::Analysed)
    }
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
    /// Nanoseconds of the cell's own time extent actually sampled. Always `> 0`.
    ///
    /// On a one-row grid ([`grid`]) the cell's extent is the whole window; on an `nt`-row grid
    /// ([`grid_over`]) it is that row's slice of it. After [`CoverageGrid::coarsen`] it is the
    /// children's summed seconds over the parent's **frequency** extent, so a parent pooling one
    /// observed child with `f_factor − 1` never-observed ones holds `1/f_factor` of a child's
    /// seconds rather than a child's seconds (T-419).
    pub observed_ns: i64,
    /// Of [`Self::observed_ns`], the nanoseconds the **analysis** actually ran on (T-595).
    ///
    /// `0 < analysed_ns <= observed_ns` for an ordinary cell and exactly `0` for a cell whose every
    /// covering interval declared it excluded — the DC notch. It is a count, not a flag, so a cell
    /// covered by one excluded span and one analysed one is reported as what it is (analysed for
    /// part of its extent) instead of being rounded to either claim; [`Self::excluded`] is the
    /// `== 0` test, named once so no consumer re-spells it.
    ///
    /// Folded exactly like `observed_ns` in [`CoverageGrid::coarsen`], and for the same reason: a
    /// coarse cell pooling one analysed child with fifteen excluded ones was analysed, in part.
    pub analysed_ns: i64,
    /// `observed_ns` as a fraction of the cell's own time extent, `0 < duty ≤ 1`. A cell sampled for
    /// part of its extent is observed for that part and unobserved for the rest — reported, not
    /// rounded away.
    ///
    /// **Derived against `observed_ns`, so it is re-derived whenever `observed_ns` changes.** It is
    /// never averaged from children's duties in a fold; T-419's near-miss was the same shape one
    /// field over (`occ_s` against `obs_s`), and a fold that carried a stale ratio would double it.
    pub duty: f64,
    /// End of the newest sampling interval: "when this cell was last looked at".
    pub last: Timestamp,
    /// Tuned centre of the newest interval, Hz.
    pub center_hz: f64,
    /// Widest sample rate any covering interval used, Hz.
    pub sample_rate_hz: f64,
}

impl Sampled {
    /// Sampled, and no part of it analysed — the DC notch (T-595).
    pub fn excluded(&self) -> bool {
        self.analysed_ns == 0
    }
}

/// What is known about one cell of the coverage map.
///
/// The two variants are the whole contract: an absence of measurement is a **different value** from
/// a measurement of nothing, at every layer that carries this type.
///
/// **T-595 did not add a third variant.** *Excluded from analysis* is a property of an observation
/// — [`Sampled::analysed_ns`] — not an absence of one: the radio sampled the notch, the history
/// keeps rows there, and the only thing that did not happen is the analysis. Spelling it as a
/// variant beside `Unobserved` would have put a cell we hold data for on the same footing as one
/// nothing ever looked at, which is the collapse this type exists to refuse. It is
/// [`Coverage::as_str`]'s third word and the wire's fourth state; it is not a fourth kind of
/// ignorance.
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
        analysed_ns: i64,
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
            // Clamped into `0..=observed_ns`: the analysis cannot have run on more of the cell than
            // was sampled, and a caller that thought otherwise is saying something no span supports.
            analysed_ns: analysed_ns.clamp(0, observed_ns),
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

    /// Sampled, and **no** part of it analysed: the DC notch (T-595). `false` for an unobserved
    /// cell, which is a different claim — nothing looked there at all.
    pub fn is_excluded(&self) -> bool {
        matches!(self, Coverage::Observed(s) if s.analysed_ns == 0)
    }

    /// The sampling behind an observed cell; `None` when nothing looked.
    pub fn sampled(&self) -> Option<&Sampled> {
        match self {
            Coverage::Observed(s) => Some(s),
            Coverage::Unobserved => None,
        }
    }

    /// The wire value: `"observed"`, `"excluded"` (T-595) or `"unobserved"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Coverage::Observed(s) if s.analysed_ns == 0 => "excluded",
            Coverage::Observed(_) => "observed",
            Coverage::Unobserved => "unobserved",
        }
    }
}

/// One device's coverage of a band over a window, on an `nt × nf` time–frequency grid.
///
/// `nt == 1` is T-368's answer unchanged — one row over the whole window, which is what [`grid`],
/// [`union_grid`] and [`by_device`] build. `nt > 1` is T-421's: the same rasterisation with the time
/// axis kept, so a cell can say *"this front end was not tuned here, **then**"*.
#[derive(Clone, Debug, PartialEq)]
pub struct CoverageGrid {
    /// Whose coverage this is. Never a merge of two named devices (see [`union_grid`]).
    pub device: Device,
    /// The window the rows partition.
    pub window: TimeRange,
    /// Time rows, earliest first. Always `>= 1`.
    pub nt: usize,
    /// Frequency cells per row, low frequency first. Always `>= 1`.
    pub nf: usize,
    /// Nominal row duration, ns. Rows partition `window` exactly, so a row may differ from this by
    /// the division's remainder; [`CoverageGrid::row_time`] is the row's true extent.
    pub t_cell_ns: i64,
    /// Low edge of frequency cell 0, Hz.
    pub f_lo_hz: f64,
    /// Cell width, Hz.
    pub f_cell_hz: f64,
    /// The cells, **row-major**: row `t`, cell `f` is `cells[t * nf + f]`. Earliest row first, low
    /// frequency first — so a one-row grid is exactly T-368's `Vec<Coverage>`.
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

    /// Cells sampled but wholly excluded from analysis — the DC notch (T-595). These are a
    /// **subset** of [`Self::observed_cells`]: they were observed, and the count says how many of
    /// them carry the exclusion, so no arithmetic here changes what grey means.
    pub fn excluded_cells(&self) -> usize {
        self.cells.iter().filter(|c| c.is_excluded()).count()
    }

    /// The fraction of the grid this device observed; `0.0` for an empty grid.
    pub fn observed_fraction(&self) -> f64 {
        if self.cells.is_empty() {
            return 0.0;
        }
        self.observed_cells() as f64 / self.cells.len() as f64
    }

    /// Cell `(t, f)`, or `None` outside the grid.
    pub fn cell(&self, t: usize, f: usize) -> Option<&Coverage> {
        if t >= self.nt || f >= self.nf {
            return None;
        }
        self.cells.get(t * self.nf + f)
    }

    /// Row `t`'s true extent. Rows partition [`Self::window`] exactly and none is dropped to
    /// rounding, so the last row absorbs the division's remainder rather than the window losing it.
    pub fn row_time(&self, t: usize) -> Option<TimeRange> {
        if t >= self.nt {
            return None;
        }
        let w0 = self.window.start.as_unix_nanos();
        let span = i128::from(self.window.duration_ns());
        let edge = |i: usize| -> i64 {
            (i128::from(w0) + span * i as i128 / self.nt as i128)
                .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
        };
        Some(TimeRange::new(
            Timestamp::from_unix_nanos(edge(t)),
            Timestamp::from_unix_nanos(edge(t + 1)),
        ))
    }

    /// Frequency cell `f`'s extent.
    pub fn freq_of(&self, f: usize) -> Option<FreqRange> {
        if f >= self.nf {
            return None;
        }
        let lo = self.f_lo_hz + self.f_cell_hz * f as f64;
        Some(FreqRange::new(lo, lo + self.f_cell_hz))
    }

    /// The cell containing `f_hz` in **row 0**, or `None` outside the grid.
    ///
    /// On a one-row grid — every caller T-368 shipped — row 0 is the whole window and this is the
    /// original meaning unchanged. On a multi-row grid say which row you mean: [`Self::at_tf`].
    pub fn at(&self, f_hz: f64) -> Option<&Coverage> {
        self.f_index(f_hz).and_then(|f| self.cell(0, f))
    }

    /// The cell containing `(t, f_hz)`, or `None` outside the grid.
    pub fn at_tf(&self, t: Timestamp, f_hz: f64) -> Option<&Coverage> {
        let f = self.f_index(f_hz)?;
        let span = self.window.duration_ns();
        if span <= 0 {
            return None;
        }
        let d = i128::from(t.as_unix_nanos() - self.window.start.as_unix_nanos());
        if d < 0 {
            return None;
        }
        let row = (d * self.nt as i128 / i128::from(span)) as usize;
        self.cell(row.min(self.nt.saturating_sub(1)), f)
    }

    fn f_index(&self, f_hz: f64) -> Option<usize> {
        if !(f_hz.is_finite() && self.f_cell_hz > 0.0) {
            return None;
        }
        let i = ((f_hz - self.f_lo_hz) / self.f_cell_hz).floor();
        if i < 0.0 || i >= self.nf as f64 {
            return None;
        }
        Some(i as usize)
    }

    /// Rows lying **wholly before** `oldest_record` — the boundary past which this grid's
    /// [`Coverage::Unobserved`] stops meaning "nothing looked".
    ///
    /// `docs/16` §5.4's fourth state, made askable without a third [`Coverage`] variant. This module
    /// is handed spans, not the horizon that produced them, so it cannot tell *no record* from *no
    /// surviving record*; the caller that read the observation log knows when its oldest record
    /// starts and passes it here. Rows `0..n` are **"we no longer know whether we looked"** and must
    /// be drawn as a fourth thing, not as grey — greying them spells "never looked" for spectrum
    /// whose records were merely discarded, which is [`Coverage::of`]'s sin one horizon out.
    ///
    /// Only rows are returned because the horizon is a time, not a band: a discarded record takes
    /// every frequency with it.
    pub fn unknown_rows_before(&self, oldest_record: Timestamp) -> usize {
        (0..self.nt)
            .take_while(|&t| {
                self.row_time(t)
                    .is_some_and(|r| r.end <= oldest_record && r.duration_ns() > 0)
            })
            .count()
    }

    /// Coarsens the grid by `t_factor × f_factor`, **summing** observed seconds over the children
    /// and re-deriving `duty` against the parent's own extent.
    ///
    /// This is `docs/16` §6.3's rule and T-419's finding: `observed_ns` is foldable, `duty` is not.
    /// A parent's `observed_ns` is `Σ children / f_factor` — the summed seconds spread over the
    /// parent's own **frequency** extent — so one observed child among `f_factor` siblings gives the
    /// parent `1/f_factor`, not the child's own value. Taking the best child instead is what let a
    /// level-4 cell claim full coverage on one sixteenth of its band.
    ///
    /// Returns `None` unless both factors are `>= 1` and divide [`Self::nt`] and [`Self::nf`]
    /// exactly: **a fold whose children do not partition its parent is not a fold**, and the sum
    /// would then be over a different extent than the divisor claims. `docs/16` §6.2's ladder is ×2
    /// on both axes from uniform tiles, so it always divides exactly.
    ///
    /// Nothing observed becomes unobserved: a parent with any observed child keeps at least one
    /// nanosecond, so integer division can never manufacture grey out of arithmetic.
    pub fn coarsen(&self, t_factor: usize, f_factor: usize) -> Option<CoverageGrid> {
        if t_factor == 0 || f_factor == 0 || self.nt % t_factor != 0 || self.nf % f_factor != 0 {
            return None;
        }
        if t_factor == 1 && f_factor == 1 {
            return Some(self.clone());
        }
        let (nt, nf) = (self.nt / t_factor, self.nf / f_factor);
        let out = CoverageGrid {
            device: self.device.clone(),
            window: self.window,
            nt,
            nf,
            t_cell_ns: self.window.duration_ns() / nt.max(1) as i64,
            f_lo_hz: self.f_lo_hz,
            f_cell_hz: self.f_cell_hz * f_factor as f64,
            cells: Vec::new(),
        };
        let mut cells = Vec::with_capacity(nt * nf);
        for tp in 0..nt {
            let parent_ns = out.row_time(tp).map_or(0, |r| r.duration_ns());
            for fp in 0..nf {
                let mut sum_ns = 0i128;
                // T-595: the analysed seconds fold exactly like the observed ones, so a parent
                // pooling one analysed child with `f_factor - 1` excluded ones is analysed in
                // part — never excluded outright, and never analysed outright either.
                let mut sum_analysed_ns = 0i128;
                // `spans` is a merged-run count, and a run crossing two children is one run in each.
                // Summing would double it and the fold no longer holds the intervals to re-merge, so
                // take the **most any child saw**: a lower bound, which is the safe direction — it is
                // `> 0` exactly when some child observed, which is all `Coverage::of` reads it for.
                let mut spans = 0u32;
                let (mut last_ns, mut center_hz, mut rate_hz) = (i64::MIN, 0.0f64, 0.0f64);
                let mut any = false;
                for t in tp * t_factor..(tp + 1) * t_factor {
                    for f in fp * f_factor..(fp + 1) * f_factor {
                        let Some(s) = self.cell(t, f).and_then(Coverage::sampled) else {
                            continue;
                        };
                        any = true;
                        sum_ns += i128::from(s.observed_ns);
                        sum_analysed_ns += i128::from(s.analysed_ns);
                        spans = spans.max(s.spans);
                        let t_last = s.last.as_unix_nanos();
                        if t_last >= last_ns {
                            last_ns = t_last;
                            center_hz = s.center_hz;
                        }
                        rate_hz = rate_hz.max(s.sample_rate_hz);
                    }
                }
                // Over the parent's own frequency extent — the sum, not the best.
                let observed_ns = (sum_ns / f_factor as i128).min(i128::from(i64::MAX)) as i64;
                let observed_ns = if any { observed_ns.max(1) } else { 0 };
                // Same integer division, and the same floor against it: a parent with any analysed
                // child keeps at least one nanosecond, so arithmetic can never turn "analysed
                // somewhere inside" into "excluded" — the coarse twin of the grey rule above.
                let analysed_ns =
                    (sum_analysed_ns / f_factor as i128).min(i128::from(i64::MAX)) as i64;
                let analysed_ns = if sum_analysed_ns > 0 {
                    analysed_ns.max(1)
                } else {
                    0
                };
                cells.push(Coverage::of(
                    spans,
                    observed_ns,
                    analysed_ns,
                    parent_ns,
                    Timestamp::from_unix_nanos(if any { last_ns } else { 0 }),
                    center_hz,
                    rate_hz,
                ));
            }
        }
        Some(CoverageGrid { cells, ..out })
    }
}

/// Merged sampled time per cell while folding.
#[derive(Default)]
struct Acc {
    intervals: Vec<(i64, i64)>,
    /// The subset of `intervals` contributed by [`Analysis::Analysed`] spans (T-595). Kept as its
    /// own list, and merged by the same routine, because "how much of this cell did the analysis
    /// run on" is a merged duration exactly like "how much of it was sampled" — a flag would round
    /// a part-analysed cell to one claim or the other.
    analysed: Vec<(i64, i64)>,
    center_hz: f64,
    sample_rate_hz: f64,
    last_ns: i64,
}

impl Acc {
    fn push(&mut self, t0: i64, t1: i64, s: &CoverageSpan) {
        self.intervals.push((t0, t1));
        if s.analysis.is_analysed() {
            self.analysed.push((t0, t1));
        }
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
        Self::merge(&mut self.intervals)
    }

    /// The same merge over the analysed subset: nanoseconds of this cell the analysis ran on.
    fn merged_analysed(&mut self) -> i64 {
        Self::merge(&mut self.analysed).1
    }

    fn merge(intervals: &mut [(i64, i64)]) -> (u32, i64) {
        intervals.sort_unstable();
        let (mut n, mut total) = (0u32, 0i64);
        let mut cur: Option<(i64, i64)> = None;
        for &(a, b) in intervals.iter() {
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

/// Which front end an observation record says looked (T-378).
///
/// A record that names one is device-local evidence, exactly like an IQ-ring segment. A record that
/// names none — every record written before T-378, and every record of a source that states no
/// identity — is [`Device::Unknown`]: evidence that *something* looked, and nothing more. It is
/// never read as the device that happens to be running now, which would invent provenance for data
/// that has none (`BiasTee::Unknown` ≠ `off`).
pub fn record_device(device_id: Option<&str>) -> Device {
    device_id
        .filter(|d| !d.is_empty())
        .map_or(Device::Unknown, |d| Device::Id(d.to_string()))
}

/// Coverage spans read out of observation records, and how many of them named a device.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordSpans {
    /// The spans, device-local and unmerged, in record order.
    pub spans: Vec<CoverageSpan>,
    /// How many of `spans` carry a named [`Device::Id`] — so a caller can report `device_known`
    /// as something **measured**, never declared (T-378).
    pub named: usize,
}

/// The tune history an observation log holds, as coverage spans: every dwell's analysed extent over
/// the interval it was analysed in, and every sweep hop visit's (ADR-0012 §1).
///
/// # One span per step, which is the whole of `docs/16` §7 step 6
///
/// A [`DwellRecord`](hk_model::attention::observation::DwellRecord) describes **one scheduler step** — one tuned band, one settled interval — so it
/// becomes one span (two when a DC notch splits it) with that step's own shape. A [`SweepRecord`]
/// aggregates a pass, but it keeps its hops: each [`HopVisit`](hk_model::attention::observation::HopVisit) names a geometry hop and an offset
/// inside the record's span, so it too becomes a span with its **hop's** band and that visit's own
/// interval. Neither kind ever contributes one coarse span over a whole pass, and that is exactly
/// what [`grid_over`] needs — a rasteriser can only tell the truth about a dwell pattern if the
/// records it reads carry the dwell's own shape. A producer that folded several steps into one
/// record with a union band would rasterise as *the whole band, the whole time*: the overclaim this
/// module exists to prevent, arriving through the input rather than the fold.
///
/// [`ObservedWindow::covered`] has already removed the DC notch, so a notched window contributes two
/// analysed spans — **plus the notch itself as an [`Analysis::Excluded`] span** (T-595). The notch
/// is not unobserved: the radio sampled it and the spectrum history keeps rows there; it is
/// *excluded from analysis*, which is a different claim and gets a different mark. A visit of zero
/// observed length contributes nothing: a hop cut before it settled sampled nothing, and [`Coverage::of`] would refuse it anyway.
///
/// `freq` filters: a covered extent that misses the band asked about is dropped rather than folded,
/// because coverage of somewhere else is not coverage of here. Pass
/// [`FreqRange::new(f64::NEG_INFINITY, f64::INFINITY)`](FreqRange::new) to keep everything.
///
/// `geometries` are the [`SweepGeometry`] records the page's sweep records reference; a sweep record
/// whose geometry is not among them contributes nothing, because without the geometry its hops name
/// no band and guessing one would be inventing coverage.
pub fn spans_from_records<'a>(
    records: impl IntoIterator<Item = &'a ObservationRecord>,
    geometries: &[SweepGeometry],
    freq: FreqRange,
) -> RecordSpans {
    let mut out = RecordSpans::default();
    let mut push = |device: &Device, w: &ObservedWindow, t: TimeRange| {
        if t.duration_ns() <= 0 {
            return;
        }
        // Every sub-range the tune SAMPLED, each carrying whether the analysis ran on it: the two
        // analysed sides of the notch, and the notch itself as `Excluded` (T-595). The notch used
        // to contribute no span at all, which rasterised as "nothing ever looked" over rows the
        // history pyramid holds.
        let ranges = w
            .covered()
            .into_iter()
            .map(|c| (c, Analysis::Analysed))
            .chain(w.dc_excluded.map(|dc| (dc, Analysis::Excluded)));
        for (c, analysis) in ranges {
            if c.overlaps(&freq) {
                out.named += usize::from(device.is_named());
                out.spans.push(CoverageSpan {
                    device: device.clone(),
                    time: t,
                    freq: c,
                    analysis,
                    center_hz: w.center_hz,
                    sample_rate_hz: w.sample_rate_hz,
                });
            }
        }
    };
    for r in records {
        match r {
            ObservationRecord::Dwell(d) => push(
                &record_device(d.device_id.as_deref()),
                &d.window,
                d.observed,
            ),
            ObservationRecord::Sweep(s) => {
                let device = record_device(s.device_id.as_deref());
                let Some(g) = geometries.iter().find(|g| g.id == s.geometry) else {
                    continue;
                };
                for v in &s.visits {
                    let Some(w) = g.hops.get(v.hop as usize) else {
                        continue;
                    };
                    let start = s
                        .span
                        .start
                        .saturating_add_nanos(i64::from(v.start_ms) * 1_000_000);
                    let end = start.saturating_add_nanos(i64::from(v.observed_ms) * 1_000_000);
                    if end > start {
                        push(&device, w, TimeRange::new(start, end));
                    }
                }
            }
            ObservationRecord::Geometry(_) => {}
        }
    }
    out
}

/// Folds `spans` into one device's coverage grid over `freq` × `window`, collapsed on time.
///
/// Only spans whose [`CoverageSpan::device`] equals `device` contribute — including
/// [`Device::Unknown`], which matches only itself. `cells` is clamped to `1..=`[`MAX_COVERAGE_CELLS`].
///
/// This is [`grid_over`] with one time row, and it answers a **column**: *was this band sampled
/// anywhere in this window*. For a cell — *was it sampled **then*** — ask [`grid_over`].
pub fn grid(
    spans: &[CoverageSpan],
    device: &Device,
    freq: FreqRange,
    window: TimeRange,
    cells: usize,
) -> CoverageGrid {
    grid_over(spans, device, freq, window, 1, cells)
}

/// Folds `spans` into one device's coverage on an `nt × nf` time–frequency grid (T-421).
///
/// The same rasterisation as [`grid`] with the time axis kept: each row is its own slice of
/// `window`, a span contributes to a row only where it actually overlaps that row, and every cell
/// goes through [`Coverage::of`] with the **row's own** duration as the window. So a band the front
/// end watched for one second of a minute is observed in the row holding that second and
/// [`Coverage::Unobserved`] — genuinely grey — in the other rows, where the column answer said
/// "sampled" for all of them.
///
/// `nf` is clamped to `1..=`[`MAX_COVERAGE_CELLS`] and `nt` to `1..=`[`MAX_COVERAGE_ROWS`]; if the
/// product still exceeds [`MAX_COVERAGE_GRID_CELLS`] the **time** rows are reduced to fit, and the
/// grid says which [`CoverageGrid::nt`] it built.
///
/// **Coarsening a grid is [`CoverageGrid::coarsen`], not a coarser call to this function.** Asking
/// here for a wide cell answers *"did anything look anywhere inside it"*, which reports a cell fully
/// covered on a sliver of its frequency extent — the rounded-up shape T-419 removed from the tile
/// fold. Rasterise at the finest resolution you mean and fold down.
pub fn grid_over(
    spans: &[CoverageSpan],
    device: &Device,
    freq: FreqRange,
    window: TimeRange,
    nt: usize,
    nf: usize,
) -> CoverageGrid {
    fold(spans, device.clone(), freq, window, nt, nf, |s| {
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
    union_grid_over(spans, freq, window, 1, cells)
}

/// [`union_grid`] with the time axis kept — every span, whatever device produced it, on an
/// `nt × nf` grid labelled [`Device::Any`].
pub fn union_grid_over(
    spans: &[CoverageSpan],
    freq: FreqRange,
    window: TimeRange,
    nt: usize,
    nf: usize,
) -> CoverageGrid {
    fold(spans, Device::Any, freq, window, nt, nf, |_| true)
}

#[allow(clippy::too_many_arguments)]
fn fold(
    spans: &[CoverageSpan],
    label: Device,
    freq: FreqRange,
    window: TimeRange,
    nt: usize,
    nf: usize,
    keep: impl Fn(&CoverageSpan) -> bool,
) -> CoverageGrid {
    let nf = nf.clamp(1, MAX_COVERAGE_CELLS);
    let nt = nt
        .clamp(1, MAX_COVERAGE_ROWS)
        .min((MAX_COVERAGE_GRID_CELLS / nf).max(1));
    let width = freq.hi_hz - freq.lo_hz;
    let f_cell_hz = if width.is_finite() && width > 0.0 {
        width / nf as f64
    } else {
        0.0
    };
    let window_ns = window.duration_ns();
    let w0 = window.start.as_unix_nanos();
    // Row edges, computed from the window so the rows partition it exactly rather than drifting by
    // the remainder of a division — and so a coarser grid's edges are always a subset of a finer
    // one's, which is what makes `coarsen` an exact partition.
    let edge = |i: usize| -> i64 {
        (i128::from(w0) + i128::from(window_ns) * i as i128 / nt as i128) as i64
    };
    let mut acc: Vec<Acc> = (0..nt * nf).map(|_| Acc::default()).collect();
    if f_cell_hz > 0.0 && window_ns > 0 {
        for s in spans.iter().filter(|s| s.is_valid() && keep(s)) {
            let t0 = s.time.start.as_unix_nanos().max(w0);
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
            let hi = (hi.max(0.0) as usize).min(nf);
            if lo >= hi {
                continue;
            }
            // Rows the span's interval touches, and within each row only the part that falls in it:
            // coverage of another instant is no more coverage of this one than coverage of another
            // band is coverage of this band.
            let wn = i128::from(window_ns);
            let row_lo = ((i128::from(t0 - w0) * nt as i128).div_euclid(wn) as usize).min(nt - 1);
            let row_hi = (((i128::from(t1 - w0) * nt as i128 + wn - 1).div_euclid(wn)) as usize)
                .clamp(row_lo + 1, nt);
            for r in row_lo..row_hi {
                let (r0, r1) = (edge(r).max(t0), edge(r + 1).min(t1));
                if r1 <= r0 {
                    continue;
                }
                for a in acc[r * nf..(r + 1) * nf].iter_mut().take(hi).skip(lo) {
                    a.push(r0, r1, s);
                }
            }
        }
    }
    let cells = acc
        .iter_mut()
        .enumerate()
        .map(|(i, a)| {
            let (n_merged, observed_ns) = a.merged();
            let analysed_ns = a.merged_analysed();
            // The window each cell's duty is a fraction of is the **row's own** extent, so
            // `Coverage::of`'s refusal — and its `0 < duty <= 1` — apply per cell unchanged.
            let row_ns = edge(i / nf + 1) - edge(i / nf);
            Coverage::of(
                n_merged,
                observed_ns,
                analysed_ns,
                row_ns,
                Timestamp::from_unix_nanos(a.last_ns),
                a.center_hz,
                a.sample_rate_hz,
            )
        })
        .collect();
    CoverageGrid {
        device: label,
        window,
        nt,
        nf,
        t_cell_ns: if nt > 0 { window_ns / nt as i64 } else { 0 },
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
    by_device_over(spans, freq, window, 1, cells)
}

/// [`by_device`] with the time axis kept: one `nt × nf` grid per distinct device, never merged.
///
/// This is where *"fills in as more SDRs are added"* needs no new concepts — another front end is
/// another grid under the same key, and [`union_grid_over`] is still the only way to get one answer
/// out of several radios.
pub fn by_device_over(
    spans: &[CoverageSpan],
    freq: FreqRange,
    window: TimeRange,
    nt: usize,
    nf: usize,
) -> Vec<CoverageGrid> {
    let mut devices: BTreeMap<Device, ()> = BTreeMap::new();
    for s in spans.iter().filter(|s| s.is_valid()) {
        devices.insert(s.device.clone(), ());
    }
    devices
        .keys()
        .map(|d| grid_over(spans, d, freq, window, nt, nf))
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
            analysis: Analysis::Analysed,
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
            analysis: Analysis::Analysed,
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

    // ---- T-595: the DC notch is EXCLUDED, never unobserved ------------------------------------

    /// **T-595.** A dwell record whose window declares a DC notch must rasterise the notch as
    /// *sampled, excluded from analysis* — never as [`Coverage::Unobserved`].
    ///
    /// The notch is 30 kHz of a 2 MHz tune, and the history pyramid keeps spectrum rows right
    /// across it. Before this, [`spans_from_records`] emitted only the two analysed sides, so the
    /// fold answered "nothing ever looked" for the middle: grey painted over rows we hold. The
    /// assertion counts, and the counts are stated on both sides so it cannot pass by observing
    /// everything or by excluding everything.
    ///
    /// RED without the fix: drop the `Analysis::Excluded` span from [`spans_from_records`] and the
    /// notch cells go back to `Unobserved`, failing the first two assertions.
    #[test]
    fn the_declared_dc_notch_rasterises_as_excluded_and_never_as_unobserved() {
        use hk_model::attention::baseline::SiteKey;
        use hk_model::attention::observation::{
            DwellRecord, ObservationRecord, ObservedWindow, Reason, Tier,
        };
        let centre = 100e6;
        let w = TimeRange::new(t(1000), t(1060));
        let rec = ObservationRecord::Dwell(DwellRecord {
            schema: hk_model::attention::ATTENTION_SCHEMA_VERSION,
            survey_id: None,
            seq: 1,
            plan_version: 1,
            site: SiteKey::Unassigned,
            device_id: Some("dev-a".into()),
            reason: Reason::RegionDwell { hop: 0 },
            tier: Tier::ScheduledPlan,
            window: ObservedWindow {
                center_hz: centre,
                sample_rate_hz: 2e6,
                usable: FreqRange::new(centre - 1e6, centre + 1e6),
                dc_excluded: Some(FreqRange::new(centre - 15e3, centre + 15e3)),
                rbw_hz: 1e3,
            },
            rf_path: 0,
            planned: w,
            observed: w,
            preempted: false,
            dropped_samples: 0,
            overload: false,
            provenance_ref: None,
        });
        let all = FreqRange::new(f64::NEG_INFINITY, f64::INFINITY);
        let rs = spans_from_records([&rec], &[], all);
        assert_eq!(
            rs.spans.len(),
            3,
            "two analysed sides and the notch itself: {:?}",
            rs.spans
        );
        assert_eq!(
            rs.spans
                .iter()
                .filter(|s| !s.analysis.is_analysed())
                .count(),
            1,
            "exactly one excluded span, and it is the notch: {:?}",
            rs.spans
        );

        // 2 MHz of band in 10 kHz cells, so the 30 kHz notch is its own handful of cells.
        let f = FreqRange::new(centre - 1e6, centre + 1e6);
        let g = grid(&rs.spans, &Device::Id("dev-a".into()), f, w, 200);
        let notch: Vec<usize> = (0..g.nf)
            .filter(|&i| {
                let r = g.freq_of(i).unwrap();
                r.lo_hz >= centre - 15e3 && r.hi_hz <= centre + 15e3
            })
            .collect();
        assert!(
            notch.len() >= 2,
            "the fixture must judge notch cells: {notch:?}"
        );
        let greyed = notch.iter().filter(|&&i| !g.cells[i].is_observed()).count();
        assert_eq!(
            greyed,
            0,
            "{greyed} of {} notch cells read unobserved - grey over spectrum the radio sampled",
            notch.len()
        );
        for &i in &notch {
            assert!(
                g.cells[i].is_excluded(),
                "notch cell {i} ({:?}) must carry the exclusion, not pass as ordinary coverage",
                g.freq_of(i)
            );
            assert_eq!(g.cells[i].as_str(), "excluded");
        }
        assert_eq!(
            g.excluded_cells(),
            notch.len(),
            "only the notch is excluded"
        );

        // And the analysed sides are analysed: an exclusion that spread would be the same defect
        // pointed the other way.
        let side = g.at(centre - 500e3).unwrap();
        assert!(side.is_observed() && !side.is_excluded(), "{side:?}");
        assert_eq!(side.as_str(), "observed");
        assert!(
            g.observed_cells() > notch.len() * 4,
            "{}",
            g.observed_cells()
        );

        // A coarse fold pools the notch into analysed neighbours and reads analysed: a 30 kHz
        // exclusion is not a claim about a 200 kHz cell.
        let coarse = g.coarsen(1, 20).expect("fold");
        assert_eq!(
            coarse.excluded_cells(),
            0,
            "the notch must not colour the cell that swallows it: {coarse:?}"
        );
    }

    /// State 3 is unrepresentable as state 2: the only constructor refuses to mint a `Sampled` that
    /// means "nothing was sampled", so no zeroed measurement can pass for a quiet one.
    #[test]
    fn nothing_sampled_cannot_be_built_as_an_observation() {
        let z = Timestamp::from_unix_nanos(0);
        assert_eq!(
            Coverage::of(
                0,
                60_000_000_000,
                60_000_000_000,
                60_000_000_000,
                z,
                1e8,
                2e6
            ),
            Coverage::Unobserved,
            "no spans"
        );
        assert_eq!(
            Coverage::of(1, 0, 0, 60_000_000_000, z, 1e8, 2e6),
            Coverage::Unobserved,
            "no sampled duration"
        );
        assert_eq!(
            Coverage::of(1, 60_000_000_000, 60_000_000_000, 0, z, 1e8, 2e6),
            Coverage::Unobserved,
            "no window"
        );
        // A real, brief look is observed — with a duty that says how brief, never rounded to zero.
        let c = Coverage::of(1, 1, 1, 60_000_000_000, z, 1e8, 2e6);
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

    // ---------------------------------------------------------------------------------------
    // T-421: the record answer gains a time axis, and the fold that coarsens it is a SUM.
    // ---------------------------------------------------------------------------------------

    /// The ladder these use: 16 MHz of band in 1 MHz cells, 64 s of window in 4 s rows, so four ×2
    /// frequency folds (1 → 2 → 4 → 8 → 16 MHz) and four ×2 time folds run over the same grid.
    const NF0: usize = 16;
    const NT0: usize = 16;

    fn ladder_band() -> FreqRange {
        FreqRange::new(0.0, 16e6)
    }

    fn ladder_window() -> TimeRange {
        TimeRange::new(t(1000), t(1064))
    }

    fn dev_a() -> Device {
        Device::Id("dev-a".into())
    }

    fn level0(spans: &[CoverageSpan]) -> CoverageGrid {
        grid_over(spans, &dev_a(), ladder_band(), ladder_window(), NT0, NF0)
    }

    /// **The control, copied from T-419's `truth_from_level_0`.** The observed seconds of coarse
    /// cell `(tp, fp)`, computed from the **level-0** grid alone: every level-0 cell inside the box
    /// contributes its own observed seconds, the divisor is the number of level-0 frequency cells
    /// the box spans, and the result is a fraction of the coarse cell's own duration. Nothing here
    /// reads the cell it is checking, and nothing reads an intermediate level — which is the whole
    /// point: a parent-vs-child assertion is satisfied by an answer `f_factor` times too large at
    /// every level, so only a value pinned against the finest cells can catch a fold that rounds up.
    fn truth_from_level_0(h0: &CoverageGrid, coarse: &CoverageGrid, tp: usize, fp: usize) -> f64 {
        let box_t = coarse.row_time(tp).expect("row");
        let box_f = coarse.freq_of(fp).expect("cell");
        let f_children = (coarse.f_cell_hz / h0.f_cell_hz).round();
        let mut observed_ns = 0f64;
        for t in 0..h0.nt {
            let r = h0.row_time(t).expect("row");
            if r.start < box_t.start || r.end > box_t.end {
                continue;
            }
            for f in 0..h0.nf {
                let c = h0.freq_of(f).expect("cell");
                if c.lo_hz < box_f.lo_hz || c.hi_hz > box_f.hi_hz {
                    continue;
                }
                if let Some(s) = h0.cell(t, f).and_then(Coverage::sampled) {
                    observed_ns += s.observed_ns as f64;
                }
            }
        }
        observed_ns / (f_children * box_t.duration_ns() as f64)
    }

    /// **The case that motivated T-421.** The radio watched 100–110 MHz for the first 10 s of a 60 s
    /// window and was tuned elsewhere for the other 50. The column answer — one row over the whole
    /// window, which is all T-368 could say — calls the band `Observed`, so a view drawing a cell at
    /// t = 40 s reads "sampled, level not retained" and paints it as data it merely failed to keep.
    /// With the time axis kept, the row holding the dwell is observed and **every other row is
    /// genuinely grey.**
    #[test]
    fn a_band_the_radio_was_tuned_away_from_at_that_instant_is_grey_in_those_rows() {
        let spans = vec![span("dev-a", 100e6, 110e6, 1000, 1010)];

        // What the column answer says, and it is the whole column: observed, for all 60 s of it.
        let column = grid(&spans, &dev_a(), band(), window(), 10);
        assert!(column.at(105e6).unwrap().is_observed());
        assert_eq!(column.nt, 1, "T-368's grid is the one-row case");

        // With six 10 s rows, only the row the radio was actually here in.
        let g = grid_over(&spans, &dev_a(), band(), window(), 6, 10);
        assert_eq!((g.nt, g.nf), (6, 10));
        let first = g.cell(0, 0).expect("row 0");
        let s = first.sampled().expect("the dwell");
        assert_eq!(s.observed_ns, 10_000_000_000, "the whole row");
        assert_eq!(s.duty, 1.0, "duty is against the ROW, not the window");

        for row in 1..6 {
            assert_eq!(
                *g.cell(row, 0).expect("row"),
                Coverage::Unobserved,
                "row {row}: the radio was tuned away, so the cell is grey — not 'sampled, level \
                 not retained'"
            );
            assert!(
                g.cell(row, 0).unwrap().sampled().is_none(),
                "row {row}: and it carries no measurement to read as a zero"
            );
        }
        // The band it never visited at all stays grey in every row, as before.
        assert!((0..6).all(|r| !g.cell(r, 5).unwrap().is_observed()));
        // And `at_tf` places an instant on the same rows.
        assert!(g.at_tf(t(1005), 105e6).unwrap().is_observed());
        assert_eq!(*g.at_tf(t(1045), 105e6).unwrap(), Coverage::Unobserved);
    }

    /// The one-row grid is T-368's answer bit for bit — same cells, same duties, same device
    /// labelling — so the time axis is an addition and not a change of meaning.
    #[test]
    fn one_row_is_the_column_answer_unchanged() {
        let spans = vec![
            span("dev-a", 100e6, 110e6, 1000, 1030),
            span("dev-b", 190e6, 200e6, 1010, 1060),
        ];
        for d in [dev_a(), Device::Id("dev-b".into())] {
            let old = grid(&spans, &d, band(), window(), 10);
            let new = grid_over(&spans, &d, band(), window(), 1, 10);
            assert_eq!(old, new);
            assert_eq!(old.nt, 1);
            assert_eq!(old.cells.len(), 10);
        }
        assert_eq!(
            union_grid(&spans, band(), window(), 10),
            union_grid_over(&spans, band(), window(), 1, 10)
        );
        assert_eq!(
            by_device(&spans, band(), window(), 10),
            by_device_over(&spans, band(), window(), 1, 10)
        );
    }

    /// **The defect T-419 found, measured on the new axis before it could be written.** One megahertz
    /// of a sixteen-megahertz band is watched, continuously, for the whole window. Level 0 sees one
    /// covered frequency cell beside fifteen unobserved ones; every ×2 frequency fold pools that cell
    /// with a never-observed sibling, so coverage must **halve at every fold**: 1 → ½ → ¼ → ⅛ → 1/16.
    ///
    /// Every assertion is against level 0, computed independently.
    #[test]
    fn coarsening_in_frequency_halves_coverage_and_never_rounds_it_up() {
        let spans = vec![span("dev-a", 0.0, 1e6, 1000, 1064)];
        let h0 = level0(&spans);
        assert_eq!(h0.cell(0, 0).unwrap().sampled().unwrap().duty, 1.0);
        assert_eq!(*h0.cell(0, 1).unwrap(), Coverage::Unobserved);

        let mut g = h0.clone();
        for (fold_n, want) in [(1usize, 0.5f64), (2, 0.25), (3, 0.125), (4, 0.0625)] {
            g = g.coarsen(1, 2).expect("×2 divides exactly");
            assert_eq!(g.nf, NF0 >> fold_n);
            let c = g.cell(0, 0).expect("the covered cell");
            let s = c
                .sampled()
                .expect("still observed — the fold never greys what was seen");
            let truth = truth_from_level_0(&h0, &g, 0, 0);
            assert!(
                (truth - want).abs() < 1e-9,
                "fold {fold_n}: level-0 truth {truth} should be {want}"
            );
            assert!(
                (s.duty - want).abs() < 1e-9,
                "fold {fold_n}: duty {} should be {want} — the max-of-children rule said 1.0",
                s.duty
            );
            // And the far end of the band, which nothing ever tuned, stays grey at every fold that
            // still has one: a coverage fold that rounds up is one step from a fold that invents
            // observation.
            if g.nf > 1 {
                assert_eq!(*g.cell(0, g.nf - 1).unwrap(), Coverage::Unobserved);
            }
        }
    }

    /// **The guard, as a test rather than a comment** — T-419's blind-spot control, on the record
    /// answer. Ask the question a parent-vs-child assertion asks (*is the parent no more than its
    /// best child?*) and it is satisfied at every level **by the answer twice too large**, because
    /// the correct parent and the rounded-up parent differ by exactly `f_factor` and both are
    /// consistent with some child. So that form can only ever prove the fold is *a* fold. Only
    /// `truth_from_level_0` pins the value.
    #[test]
    fn a_parent_vs_child_assertion_would_not_have_caught_this() {
        let spans = vec![span("dev-a", 0.0, 1e6, 1000, 1064)];
        let h0 = level0(&spans);
        let mut child = h0.clone();
        for level in 1..=4 {
            let parent = child.coarsen(1, 2).expect("×2");
            let p = f64::from(parent.cell(0, 0).unwrap().sampled().unwrap().duty as f32);

            // The OLD rule recomputed: the best-observed child frequency cell in the parent's box,
            // as a fraction of the parent's own duration.
            let box_f = parent.freq_of(0).unwrap();
            let best = (0..child.nf)
                .filter(|&f| {
                    let c = child.freq_of(f).unwrap();
                    c.lo_hz >= box_f.lo_hz && c.hi_hz <= box_f.hi_hz
                })
                .map(|f| {
                    child
                        .cell(0, f)
                        .and_then(Coverage::sampled)
                        .map_or(0.0, |s| s.duty)
                })
                .fold(0.0f64, f64::max);

            // The blind assertion: true of the fixed value AND of the value twice as large.
            assert!(
                best >= p - 1e-9,
                "level {level}: parent {p} > best child {best}"
            );
            assert!(
                (best - 2.0 * p).abs() < 1e-9,
                "level {level}: the max-of-children answer {best} is exactly f_factor × the truth \
                 {p} — which is why that assertion proves nothing"
            );
            // The only assertion that would have failed under the old rule.
            let truth = truth_from_level_0(&h0, &parent, 0, 0);
            assert!(
                (p - truth).abs() < 1e-9,
                "level {level}: {p} vs level-0 truth {truth}"
            );
            child = parent;
        }
    }

    /// The other axis, so the sum rule is not accidentally a frequency-only fix: the whole band is
    /// watched, but only on every other row. Every time fold must read 0.5, not the best row's 1.0.
    #[test]
    fn coarsening_in_time_reports_the_summed_seconds_not_the_best_row() {
        let spans: Vec<CoverageSpan> = (0..NT0 as i64)
            .filter(|r| r % 2 == 0)
            .map(|r| span("dev-a", 0.0, 16e6, 1000 + r * 4, 1000 + r * 4 + 4))
            .collect();
        let h0 = level0(&spans);
        assert!(h0.cell(0, 0).unwrap().is_observed());
        assert_eq!(*h0.cell(1, 0).unwrap(), Coverage::Unobserved);

        let mut g = h0.clone();
        for fold_n in 1..=4 {
            g = g.coarsen(2, 1).expect("×2");
            assert_eq!(g.nt, NT0 >> fold_n);
            for f in 0..g.nf {
                let s = g.cell(0, f).unwrap().sampled().expect("observed");
                let truth = truth_from_level_0(&h0, &g, 0, f);
                assert!(
                    (truth - 0.5).abs() < 1e-9,
                    "fold {fold_n} cell {f}: truth {truth}"
                );
                assert!(
                    (s.duty - 0.5).abs() < 1e-9,
                    "fold {fold_n} cell {f}: duty {} should be 0.5",
                    s.duty
                );
            }
        }
    }

    /// The fix must not cost the honest case anything: watched everywhere, always, reads 1.0 at
    /// every fold on both axes. A coverage fold that under-reports is a different lie, same shape.
    #[test]
    fn a_fully_watched_band_still_reads_fully_covered_at_every_fold() {
        let spans = vec![span("dev-a", 0.0, 16e6, 1000, 1064)];
        let h0 = level0(&spans);
        let mut g = h0.clone();
        for fold_n in 1..=4 {
            g = g.coarsen(2, 2).expect("×2 on both axes");
            for t in 0..g.nt {
                for f in 0..g.nf {
                    let s = g.cell(t, f).unwrap().sampled().expect("observed");
                    assert!(
                        (s.duty - 1.0).abs() < 1e-9,
                        "fold {fold_n} cell ({t}, {f}): duty {}",
                        s.duty
                    );
                    assert!((truth_from_level_0(&h0, &g, t, f) - 1.0).abs() < 1e-9);
                }
            }
        }
    }

    /// **Rasterising onto a coarse grid is not the fold, and the difference is the whole bug.**
    /// Asked directly for one wide cell, the rasteriser answers *"did anything look anywhere in
    /// here"* and says fully covered on one sixteenth of the band — the shape T-419 removed. The
    /// fold answers *"how much of this cell's extent was looked at"* and says 1/16. Measured here so
    /// the distinction is pinned rather than assumed, and so `coarsen` can never be quietly replaced
    /// by a coarser call to `grid_over`.
    #[test]
    fn rasterising_directly_onto_a_coarse_grid_is_not_the_fold() {
        let spans = vec![span("dev-a", 0.0, 1e6, 1000, 1064)];
        let direct = grid_over(&spans, &dev_a(), ladder_band(), ladder_window(), NT0, 1);
        assert_eq!(
            direct.cell(0, 0).unwrap().sampled().unwrap().duty,
            1.0,
            "the direct answer rounds the sliver up to the whole cell"
        );
        let folded = level0(&spans).coarsen(1, NF0).expect("16 divides 16");
        assert_eq!(folded.nf, 1);
        assert!(
            (folded.cell(0, 0).unwrap().sampled().unwrap().duty - 1.0 / NF0 as f64).abs() < 1e-9,
            "the fold reports the sixteenth that was actually watched"
        );
    }

    /// `duty` is derived against `observed_ns`, so the fold re-derives it against the **parent's**
    /// own numbers and never averages the children's. T-419's near-miss was this shape one field
    /// over: `occ_s` against `obs_s`, where carrying the stale ratio would have doubled occupancy.
    /// Here, averaging the children's duties would report ⅝ where the truth is 5/16.
    #[test]
    fn a_folds_duty_is_re_derived_and_never_averaged_from_the_children() {
        // Five of sixteen 1 MHz cells watched, each for the whole window.
        let spans = vec![span("dev-a", 0.0, 5e6, 1000, 1064)];
        let h0 = level0(&spans);
        let g = h0.coarsen(1, NF0).expect("one cell");
        let s = g.cell(0, 0).unwrap().sampled().unwrap();

        let mean_of_child_duties = (0..h0.nf)
            .filter_map(|f| h0.cell(0, f).and_then(Coverage::sampled))
            .map(|c| c.duty)
            .sum::<f64>()
            / 5.0;
        assert_eq!(
            mean_of_child_duties, 1.0,
            "the observed children all read 1.0"
        );
        assert!(
            (s.duty - 5.0 / 16.0).abs() < 1e-9,
            "duty {} should be 5/16 — the observed seconds over the PARENT's extent",
            s.duty
        );
        assert!((truth_from_level_0(&h0, &g, 0, 0) - 5.0 / 16.0).abs() < 1e-9);
        // And the same statement in the field it is derived from: the row is still a level-0 row
        // (only the frequency axis was folded), so it is five sixteenths of a 4 s row.
        let row_ns = ladder_window().duration_ns() / NT0 as i64;
        assert_eq!(s.observed_ns, 5 * row_ns / 16);
    }

    /// The fold moves in one direction only: it never greys a cell some child observed (integer
    /// division cannot manufacture "nothing looked"), and it never observes a cell no child did.
    #[test]
    fn coarsening_neither_invents_observation_nor_manufactures_grey() {
        // A single nanosecond of a single cell, folded sixteen ways: 1/16 ns rounds to zero, and a
        // zero would read as `Unobserved` — grey produced by arithmetic, not by the radio.
        let one_ns = vec![CoverageSpan {
            device: dev_a(),
            time: TimeRange::new(
                t(1000),
                Timestamp::from_unix_nanos(t(1000).as_unix_nanos() + 1),
            ),
            freq: FreqRange::new(0.0, 1e6),
            analysis: Analysis::Analysed,
            center_hz: 0.5e6,
            sample_rate_hz: 1e6,
        }];
        let g = level0(&one_ns).coarsen(1, NF0).expect("fold");
        let s = g
            .cell(0, 0)
            .unwrap()
            .sampled()
            .expect("a nanosecond is still a look");
        assert_eq!(
            s.observed_ns, 1,
            "floored to the smallest claim that is still a claim"
        );
        assert!(s.duty > 0.0);

        // Nothing anywhere stays nothing everywhere, at every fold.
        let empty = level0(&[]);
        let folded = empty.coarsen(2, 2).expect("fold");
        assert_eq!(folded.observed_cells(), 0);
        assert!(folded.cells.iter().all(|c| *c == Coverage::Unobserved));
    }

    /// A fold whose children do not partition its parent is not a fold, and is refused rather than
    /// answered over a different extent than its divisor claims.
    #[test]
    fn a_fold_that_does_not_partition_its_parent_is_refused() {
        let g = level0(&[span("dev-a", 0.0, 16e6, 1000, 1064)]);
        assert!(g.coarsen(1, 3).is_none(), "3 does not divide 16");
        assert!(g.coarsen(5, 1).is_none(), "5 does not divide 16");
        assert!(g.coarsen(0, 2).is_none(), "a zero factor is not a fold");
        assert_eq!(
            g.coarsen(1, 1).as_ref(),
            Some(&g),
            "the identity fold is the grid"
        );
        assert!(g.coarsen(NT0, NF0).is_some(), "the whole grid in one cell");
    }

    /// **`docs/16` §5.4's fourth state, named rather than painted grey.** The pyramid has no age
    /// limit and the observation log expires at 30 days, so a cell older than the oldest surviving
    /// record is not "nothing looked" — it is *we no longer know whether we looked*. This module is
    /// handed spans, not the horizon that produced them, so it cannot decide; the caller that read
    /// the log passes the horizon and gets the rows back, and must draw them as a fourth thing.
    #[test]
    fn rows_before_the_record_horizon_are_not_the_same_grey_as_rows_inside_it() {
        // The radio watched the band throughout, but the records covering the first half have been
        // discarded — so those rows hold no span and read `Unobserved` exactly like a never-visited
        // band, which is the confusion this method exists to prevent.
        let spans = vec![span("dev-a", 0.0, 16e6, 1032, 1064)];
        let g = level0(&spans);
        let horizon = t(1032);

        assert_eq!(g.unknown_rows_before(horizon), NT0 / 2);
        for r in 0..NT0 / 2 {
            assert_eq!(
                *g.cell(r, 0).unwrap(),
                Coverage::Unobserved,
                "row {r} looks identical to never-observed…"
            );
        }
        assert!(
            (0..NT0 / 2).all(|r| r < g.unknown_rows_before(horizon)),
            "…and only the horizon tells the caller it is 'we no longer know', not 'never'"
        );
        for r in NT0 / 2..NT0 {
            assert!(
                g.cell(r, 0).unwrap().is_observed(),
                "row {r} is inside the record"
            );
        }
        // A horizon at or before the window's start leaves every row knowable; one past its end
        // leaves none of them.
        assert_eq!(g.unknown_rows_before(t(1000)), 0);
        assert_eq!(g.unknown_rows_before(t(900)), 0);
        assert_eq!(g.unknown_rows_before(t(2000)), NT0);
    }

    /// The grid is bounded on both axes and on their product, and it reports the resolution it
    /// actually built — a caller never has to assume it got what it asked for.
    #[test]
    fn the_grid_is_bounded_on_both_axes_and_says_what_it_built() {
        let spans = vec![span("dev-a", 0.0, 16e6, 1000, 1064)];
        let g = grid_over(&spans, &dev_a(), ladder_band(), ladder_window(), 0, 0);
        assert_eq!((g.nt, g.nf), (1, 1), "zero is not a grid");

        let g = grid_over(
            &spans,
            &dev_a(),
            ladder_band(),
            ladder_window(),
            MAX_COVERAGE_ROWS * 4,
            MAX_COVERAGE_CELLS * 4,
        );
        assert_eq!(
            g.nf, MAX_COVERAGE_CELLS,
            "frequency cells keep their own bound"
        );
        assert_eq!(
            g.nt,
            MAX_COVERAGE_GRID_CELLS / MAX_COVERAGE_CELLS,
            "and the time rows give way so the product stays bounded"
        );
        assert_eq!(g.cells.len(), g.nt * g.nf);
        assert!(g.cells.len() <= MAX_COVERAGE_GRID_CELLS);
    }

    /// The rows partition the window exactly — no instant belongs to two rows and none to none —
    /// which is what makes a coarser grid's edges a subset of a finer one's, and therefore what
    /// makes `coarsen` an exact partition rather than an approximation.
    #[test]
    fn rows_partition_the_window_exactly_and_nest_under_coarsening() {
        // 7 rows over 64 s: the division has a remainder, which is where drift would show.
        let g = grid_over(&[], &dev_a(), ladder_band(), ladder_window(), 7, 4);
        assert_eq!(g.row_time(0).unwrap().start, g.window.start);
        assert_eq!(g.row_time(6).unwrap().end, g.window.end);
        for r in 1..7 {
            assert_eq!(
                g.row_time(r - 1).unwrap().end,
                g.row_time(r).unwrap().start,
                "row {r} starts where row {} ended",
                r - 1
            );
        }
        assert!(g.row_time(7).is_none());

        // And the 16-row ladder's ×2 folds land on edges the finer grid already had.
        let fine = grid_over(&[], &dev_a(), ladder_band(), ladder_window(), NT0, NF0);
        let coarse = fine.coarsen(2, 2).expect("×2");
        for r in 0..coarse.nt {
            assert_eq!(
                coarse.row_time(r).unwrap().start,
                fine.row_time(r * 2).unwrap().start
            );
            assert_eq!(
                coarse.row_time(r).unwrap().end,
                fine.row_time(r * 2 + 1).unwrap().end
            );
        }
    }

    /// Two front ends, kept apart on the time axis too: each is grey exactly where — and **when** —
    /// the other was looking. This is what makes *"fills in as more SDRs are added"* need no new
    /// concepts: another radio is another grid under the same key.
    #[test]
    fn the_time_axis_stays_device_local() {
        let spans = vec![
            span("hackrf:aaa", 100e6, 110e6, 1000, 1030),
            span("hackrf:bbb", 100e6, 110e6, 1030, 1060),
        ];
        let per = by_device_over(&spans, band(), window(), 6, 10);
        assert_eq!(per.len(), 2);
        let (a, b) = (&per[0], &per[1]);
        assert_eq!(a.device, Device::Id("hackrf:aaa".into()));
        assert!(a.cell(0, 0).unwrap().is_observed());
        assert_eq!(
            *a.cell(5, 0).unwrap(),
            Coverage::Unobserved,
            "a had stopped by then"
        );
        assert_eq!(
            *b.cell(0, 0).unwrap(),
            Coverage::Unobserved,
            "b had not started"
        );
        assert!(b.cell(5, 0).unwrap().is_observed());

        // The union is the only thing that sees both, and it wears neither radio's name.
        let u = union_grid_over(&spans, band(), window(), 6, 10);
        assert_eq!(u.device, Device::Any);
        assert!(!u.device.is_named());
        assert!((0..6).all(|r| u.cell(r, 0).unwrap().is_observed()));
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
