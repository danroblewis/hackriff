//! In-memory tile accumulators: level-0 frame folding and level-to-level rollup.

use hk_model::attention::baseline::SiteKey;
use hk_model::{BiasTee, CalibrationStateId, SpurMaskId, TileKey, Timestamp};

use super::config::{HistogramConfig, LevelGeometry};
use super::frame::{FrameInput, FrontEnd, GainState, PortTag};
use super::stats::{exact_percentile, hist_percentile, undb};

/// Most distinct gain states listed per tile; further states are counted in
/// [`ProvenanceSummary::other_gain_frames`].
pub const MAX_GAIN_STATES: usize = 8;

/// Most distinct origins (source × site) listed per tile (T-133); frames of further origins are
/// counted in [`ProvenanceSummary::other_origin_frames`] and read as unknown origin.
pub const MAX_ORIGINS: usize = 8;

/// Where a tile's frames came from (T-133): the [`FrameInput::source`] key and the site
/// ([`FrameInput::site`]). `None` is **unknown**: frames of tiles written before format 3, frames
/// whose caller stated no site, and origins beyond [`MAX_ORIGINS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Origin {
    /// Source key, `None` when unknown.
    pub source: Option<u64>,
    /// Site, `None` when unknown.
    pub site: Option<SiteKey>,
}

impl Origin {
    /// Unknown source and site.
    pub const UNKNOWN: Origin = Origin {
        source: None,
        site: None,
    };
}

/// One field of an [`OriginFilter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginField<T> {
    /// Any value, including unknown (no filter).
    Any,
    /// Exactly this known value.
    Is(T),
    /// Only frames whose value is unknown.
    Unknown,
}

impl<T: PartialEq + Copy> OriginField<T> {
    /// Whether a frame whose value is `v` (`None` = unknown) passes.
    pub fn matches(&self, v: Option<T>) -> bool {
        match self {
            OriginField::Any => true,
            OriginField::Is(x) => v == Some(*x),
            OriginField::Unknown => v.is_none(),
        }
    }
}

/// A source/site filter on history queries (T-133). See [`super::Pyramid::query_filtered`] for how
/// tiles holding several origins are treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OriginFilter {
    /// Source key.
    pub source: OriginField<u64>,
    /// Site.
    pub site: OriginField<SiteKey>,
}

impl Default for OriginFilter {
    fn default() -> Self {
        Self::ANY
    }
}

impl OriginFilter {
    /// No filter.
    pub const ANY: OriginFilter = OriginFilter {
        source: OriginField::Any,
        site: OriginField::Any,
    };

    /// Neither field filters.
    pub fn is_any(&self) -> bool {
        matches!(
            (self.source, self.site),
            (OriginField::Any, OriginField::Any)
        )
    }

    /// Whether frames of origin `o` pass.
    pub fn matches(&self, o: &Origin) -> bool {
        self.source.matches(o.source) && self.site.matches(o.site)
    }
}

/// How a tile's frames relate to an [`OriginFilter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginMatch {
    /// Every frame passes (or the tile has no frames).
    All,
    /// Some frames pass, others do not.
    Mixed,
    /// No frame passes.
    None,
}

/// Most provenance steps listed per tile or query result; further steps are counted in
/// [`ProvenanceSummary::steps_dropped`].
pub const MAX_PROVENANCE_STEPS: usize = 32;

/// Relative difference within which two cell shapes count as the same.
pub const SHAPE_TOLERANCE: f32 = 0.05;

/// Most distinct cell shapes (within [`SHAPE_TOLERANCE`]) listed per tile or query result (T-141);
/// values of further shapes are counted in [`ProvenanceSummary::other_shape_values`].
pub const MAX_CELL_SHAPES: usize = 32;

/// The front-end state a frame was taken under: what a [`ProvenanceStep`] compares.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct FrontEndState {
    /// Gain state.
    pub gain: Option<GainState>,
    /// Calibration in force.
    pub calibration: Option<CalibrationStateId>,
    /// Antenna-port bias-tee state (T-332): the DC powers an external LNA, so it is part of the
    /// receive chain exactly as the gain state is. Compared by **equality** and three-valued —
    /// [`BiasTee::Unknown`] is its own state, never [`BiasTee::Off`] (T-325).
    pub bias_tee: BiasTee,
    /// Gain table, filter, spur mask.
    pub front_end: FrontEnd,
}

impl FrontEndState {
    /// The state of a frame.
    pub fn of(f: &FrameInput<'_>) -> Self {
        Self {
            gain: f.gain,
            calibration: f.calibration,
            bias_tee: f.bias_tee,
            front_end: f.front_end,
        }
    }

    /// [`ProvenanceStep`] change flags from `self` to `to` (0 when equal).
    pub fn changes(&self, to: &Self) -> u8 {
        let mut c = 0;
        if self.gain != to.gain {
            c |= ProvenanceStep::GAIN;
        }
        if self.calibration != to.calibration {
            c |= ProvenanceStep::CALIBRATION;
        }
        if self.front_end.gain_table != to.front_end.gain_table {
            c |= ProvenanceStep::GAIN_TABLE;
        }
        if self.front_end.filter != to.front_end.filter {
            c |= ProvenanceStep::FILTER;
        }
        if self.front_end.spur_mask != to.front_end.spur_mask {
            c |= ProvenanceStep::SPUR_MASK;
        }
        if self.bias_tee != to.bias_tee {
            c |= ProvenanceStep::BIAS_TEE;
        }
        c
    }
}

/// A change of front-end state between consecutive folded frames (T-116). Tiles and cells are
/// not split at a step (the grid is fixed); the step is recorded instead, so a level change it
/// causes can be explained as provenance, not as an event (C26/C30).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProvenanceStep {
    /// Start of the first frame under the new state.
    pub t: Timestamp,
    /// What changed ([`ProvenanceStep::GAIN`] | …).
    pub changed: u8,
    /// State before.
    pub from: FrontEndState,
    /// State after.
    pub to: FrontEndState,
}

impl ProvenanceStep {
    /// Gain state changed.
    pub const GAIN: u8 = 1;
    /// Calibration changed.
    pub const CALIBRATION: u8 = 2;
    /// Gain table changed.
    pub const GAIN_TABLE: u8 = 4;
    /// Filter / antenna port changed.
    pub const FILTER: u8 = 8;
    /// Spur-mask version changed.
    pub const SPUR_MASK: u8 = 16;
    /// Antenna-port bias-tee state changed (T-332), including to or from
    /// [`BiasTee::Unknown`].
    pub const BIAS_TEE: u8 = 32;

    /// Names of the changed fields.
    pub fn change_names(&self) -> Vec<&'static str> {
        [
            (Self::GAIN, "gain"),
            (Self::CALIBRATION, "calibration"),
            (Self::GAIN_TABLE, "gain_table"),
            (Self::FILTER, "filter"),
            (Self::SPUR_MASK, "spur_mask"),
            (Self::BIAS_TEE, "bias_tee"),
        ]
        .into_iter()
        .filter(|(f, _)| self.changed & f != 0)
        .map(|(_, n)| n)
        .collect()
    }
}

/// Provenance and gain-state digest of the frames behind a tile or query result (C26: "store
/// provenance per tile so C30 can rule front-end changes out first").
///
/// `frames` counts frame contributions per level-0 tile (a frame spanning two tiles counts once in
/// each); ratios such as [`ProvenanceSummary::suspect_fraction`] are unaffected. Single-valued
/// tags (calibration, gain table, filter, spur mask, cell shape) keep the first value and a
/// `*_mixed` flag once another value contributed, and every front-end change is listed in
/// [`ProvenanceSummary::steps`]: different provenance never merges silently.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProvenanceSummary {
    /// Frame contributions.
    pub frames: u64,
    /// Contributions flagged overloaded/clipped.
    pub suspect_frames: u64,
    /// Samples reported dropped before contributing frames.
    pub dropped_samples: u64,
    /// Gain steps seen while folding (summed over tiles: a step in a frame spanning several
    /// frequency tiles counts in each).
    pub gain_changes: u64,
    /// Distinct gain states and their frame counts (at most [`MAX_GAIN_STATES`]).
    pub gain_states: Vec<(GainState, u64)>,
    /// Frames under gain states beyond the listed ones.
    pub other_gain_frames: u64,
    /// Frames without a gain state.
    pub unknown_gain_frames: u64,
    /// Calibration of the first frame.
    pub calibration: Option<CalibrationStateId>,
    /// More than one calibration contributed.
    pub calibration_mixed: bool,
    /// Gain table of the first frame (T-116).
    pub gain_table: Option<u32>,
    /// More than one gain table contributed.
    pub gain_table_mixed: bool,
    /// Filter / antenna port of the first frame (T-116).
    pub filter: Option<PortTag>,
    /// More than one filter contributed.
    pub filter_mixed: bool,
    /// Spur-mask version of the first frame (T-116).
    pub spur_mask: Option<SpurMaskId>,
    /// More than one spur mask contributed.
    pub spur_mask_mixed: bool,
    /// Antenna-port bias-tee state of the first frame (T-332). [`BiasTee::Unknown`] is a value, not
    /// an absence: it says the frames' source could not report the state, never that the DC was
    /// off. Tiles written before T-332 read as `Unknown`, which is what they are.
    pub bias_tee: BiasTee,
    /// More than one bias-tee state contributed, so this tile pools two receive chains and a level
    /// step inside it may be the DC rather than the air. The [`Self::steps`] say when.
    pub bias_tee_mixed: bool,
    /// Gamma shape `n_c` of the level-0 cell values (T-116; drives the floor bias correction).
    pub cell_shape: Option<f32>,
    /// Shapes differing by more than [`SHAPE_TOLERANCE`] contributed (no corrected floor).
    pub cell_shape_mixed: bool,
    /// T-141: level-0 cell values and frames folded per cell shape, `(shape, values, frames)` in
    /// first-seen order (shapes within [`SHAPE_TOLERANCE`] of a listed one count under it; at
    /// most [`MAX_CELL_SHAPES`]). The pooled values' Gamma mixture gives a mixed-shape tile its
    /// floor bias when every shape's frames covered the same number of cells
    /// ([`Self::cell_shape_mixture`]).
    pub cell_shapes: Vec<(f32, u64, u64)>,
    /// Values of shapes beyond the listed ones, of frames without a shape, or of tiles written
    /// before format 4 (which recorded no shape histogram): while nonzero there is no mixture.
    pub other_shape_values: u64,
    /// Front-end steps, in time order, deduplicated (at most [`MAX_PROVENANCE_STEPS`]).
    pub steps: Vec<ProvenanceStep>,
    /// Steps beyond the listed ones.
    pub steps_dropped: u64,
    /// Earliest frame start.
    pub first_frame: Option<Timestamp>,
    /// Latest frame start.
    pub last_frame: Option<Timestamp>,
    /// Frame contributions per origin (source × site, T-133), in first-seen order (at most
    /// [`MAX_ORIGINS`]). Tiles written before format 3 list all their frames under
    /// [`Origin::UNKNOWN`].
    pub origins: Vec<(Origin, u64)>,
    /// Frames of origins beyond the listed ones (read as unknown origin).
    pub other_origin_frames: u64,
}

fn fold_tag<T: PartialEq>(
    cur: &mut Option<T>,
    mixed: &mut bool,
    v: Option<T>,
    v_mixed: bool,
    first: bool,
) {
    if first {
        *cur = v;
        *mixed = v_mixed;
    } else if v_mixed || *cur != v {
        *mixed = true;
    }
}

fn same_shape(a: Option<f32>, b: Option<f32>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => (a - b).abs() <= SHAPE_TOLERANCE * a.max(b),
        _ => false,
    }
}

impl ProvenanceSummary {
    /// Key ([`super::GainState::key`]) of the gain state with the most frames, 0 when none is
    /// known (T-132 baseline gain-state key).
    pub fn dominant_gain_key(&self) -> u32 {
        self.gain_states
            .iter()
            .max_by_key(|(_, n)| *n)
            .map_or(0, |(g, _)| g.key())
    }
    /// Fraction of contributions flagged suspect (the hk-model `SpectrumTile::suspect_fraction`).
    pub fn suspect_fraction(&self) -> f64 {
        if self.frames == 0 {
            0.0
        } else {
            self.suspect_frames as f64 / self.frames as f64
        }
    }

    /// The cell shape, when one shape (within tolerance) contributed.
    pub fn uniform_cell_shape(&self) -> Option<f32> {
        self.cell_shape.filter(|_| !self.cell_shape_mixed)
    }

    /// T-141: the `(shape, values, frames)` mixture of a mixed-shape summary, when every folded
    /// value's shape is recorded ([`Self::other_shape_values`] is 0) and every shape's frames
    /// folded the same number of values each (values/frames equal across shapes). A cell's
    /// low percentile pools only its own frames, so the tile-wide weights are that cell's only
    /// when every frame covered as many cells; a shape whose frames covered fewer or other cells
    /// (short hops beside full-span dwells) could give a cell the wrong weights, so such a tile
    /// has no mixture and no floor. `None` also for a uniform summary (use
    /// [`Self::uniform_cell_shape`]), for unrecorded shapes (tiles before format 4, frames with
    /// no shape, more than [`MAX_CELL_SHAPES`] shapes) and when nothing was folded.
    pub fn cell_shape_mixture(&self) -> Option<&[(f32, u64, u64)]> {
        let mut per_frame = None;
        let same_coverage = self.cell_shapes.iter().all(|&(_, v, f)| {
            if f == 0 {
                return false;
            }
            let (v0, f0) = *per_frame.get_or_insert((v, f));
            u128::from(v) * u128::from(f0) == u128::from(v0) * u128::from(f)
        });
        let known = self.cell_shape_mixed
            && self.other_shape_values == 0
            && same_coverage
            && self.cell_shapes.iter().any(|&(_, v, _)| v > 0);
        known.then_some(&self.cell_shapes[..])
    }

    /// Counts `values` level-0 cell values of `frames` frames under `shape`. A shape within
    /// [`SHAPE_TOLERANCE`] of an already listed one counts under the first such shape seen (its
    /// listed shape is not updated), so the list's order and shapes depend on fold order.
    fn add_shape_values(&mut self, shape: Option<f32>, values: u64, frames: u64) {
        if values == 0 {
            return;
        }
        let Some(k) = shape else {
            self.other_shape_values = self.other_shape_values.saturating_add(values);
            return;
        };
        if let Some(s) = self
            .cell_shapes
            .iter_mut()
            .find(|(s, _, _)| same_shape(Some(*s), Some(k)))
        {
            s.1 = s.1.saturating_add(values);
            s.2 = s.2.saturating_add(frames);
        } else if self.cell_shapes.len() < MAX_CELL_SHAPES {
            self.cell_shapes.push((k, values, frames));
        } else {
            self.other_shape_values = self.other_shape_values.saturating_add(values);
        }
    }

    /// `(passing, other)` frame contributions under `filter` (frames beyond the listed origins
    /// count as unknown origin).
    pub fn origin_frames(&self, filter: &OriginFilter) -> (u64, u64) {
        let (mut pass, mut other) = (0, 0);
        for (o, n) in &self.origins {
            if filter.matches(o) {
                pass += n;
            } else {
                other += n;
            }
        }
        if filter.matches(&Origin::UNKNOWN) {
            pass += self.other_origin_frames;
        } else {
            other += self.other_origin_frames;
        }
        (pass, other)
    }

    /// How these frames relate to `filter`.
    pub fn origin_match(&self, filter: &OriginFilter) -> OriginMatch {
        match self.origin_frames(filter) {
            (_, 0) => OriginMatch::All,
            (0, _) => OriginMatch::None,
            _ => OriginMatch::Mixed,
        }
    }

    pub(crate) fn clear(&mut self) {
        let mut states = std::mem::take(&mut self.gain_states);
        let mut steps = std::mem::take(&mut self.steps);
        let mut origins = std::mem::take(&mut self.origins);
        let mut shapes = std::mem::take(&mut self.cell_shapes);
        states.clear();
        steps.clear();
        origins.clear();
        shapes.clear();
        *self = Self {
            gain_states: states,
            steps,
            origins,
            cell_shapes: shapes,
            ..Self::default()
        };
    }

    fn add_origin(&mut self, o: Origin, frames: u64) {
        if let Some(s) = self.origins.iter_mut().find(|(s, _)| *s == o) {
            s.1 += frames;
        } else if self.origins.len() < MAX_ORIGINS {
            self.origins.push((o, frames));
        } else {
            self.other_origin_frames += frames;
        }
    }

    fn add_gain(&mut self, g: GainState, frames: u64) {
        if let Some(s) = self.gain_states.iter_mut().find(|(s, _)| *s == g) {
            s.1 += frames;
        } else if self.gain_states.len() < MAX_GAIN_STATES {
            self.gain_states.push((g, frames));
        } else {
            self.other_gain_frames += frames;
        }
    }

    fn add_step(&mut self, s: &ProvenanceStep) {
        if self.steps.contains(s) {
            return;
        }
        if self.steps.len() >= MAX_PROVENANCE_STEPS {
            self.steps_dropped += 1;
            return;
        }
        let at = self.steps.partition_point(|x| x.t <= s.t);
        self.steps.insert(at, *s);
    }

    /// Folds one frame: `state` is its front-end state, `step` the change from the previous
    /// folded frame (if any), `cell_shape` its resolved level-0 cell shape and `values` the cell
    /// values it folded into this tile.
    pub(crate) fn add_frame(
        &mut self,
        f: &FrameInput<'_>,
        state: &FrontEndState,
        step: Option<&ProvenanceStep>,
        cell_shape: Option<f32>,
        values: u64,
    ) {
        self.add_shape_values(cell_shape, values, 1);
        if self.frames == 0 {
            self.calibration = state.calibration;
            self.gain_table = state.front_end.gain_table;
            self.filter = state.front_end.filter;
            self.spur_mask = state.front_end.spur_mask;
            self.bias_tee = state.bias_tee;
            self.cell_shape = cell_shape;
        } else {
            self.calibration_mixed |= self.calibration != state.calibration;
            self.gain_table_mixed |= self.gain_table != state.front_end.gain_table;
            self.filter_mixed |= self.filter != state.front_end.filter;
            self.spur_mask_mixed |= self.spur_mask != state.front_end.spur_mask;
            self.bias_tee_mixed |= self.bias_tee != state.bias_tee;
            self.cell_shape_mixed |= !same_shape(self.cell_shape, cell_shape);
        }
        self.frames += 1;
        self.suspect_frames += u64::from(f.suspect);
        self.dropped_samples += f.dropped_samples;
        self.add_origin(
            Origin {
                source: Some(f.source),
                site: f.site,
            },
            1,
        );
        match f.gain {
            Some(g) => self.add_gain(g, 1),
            None => self.unknown_gain_frames += 1,
        }
        if let Some(s) = step {
            if s.changed & ProvenanceStep::GAIN != 0 {
                self.gain_changes += 1;
            }
            self.add_step(s);
        }
        self.first_frame = Some(self.first_frame.map_or(f.t, |t| t.min(f.t)));
        self.last_frame = Some(self.last_frame.map_or(f.t, |t| t.max(f.t)));
    }

    /// Folds another summary into this one.
    pub fn merge(&mut self, o: &ProvenanceSummary) {
        if o.frames == 0 {
            return;
        }
        let first = self.frames == 0;
        fold_tag(
            &mut self.calibration,
            &mut self.calibration_mixed,
            o.calibration,
            o.calibration_mixed,
            first,
        );
        fold_tag(
            &mut self.gain_table,
            &mut self.gain_table_mixed,
            o.gain_table,
            o.gain_table_mixed,
            first,
        );
        fold_tag(
            &mut self.filter,
            &mut self.filter_mixed,
            o.filter,
            o.filter_mixed,
            first,
        );
        fold_tag(
            &mut self.spur_mask,
            &mut self.spur_mask_mixed,
            o.spur_mask,
            o.spur_mask_mixed,
            first,
        );
        // Not `fold_tag`: the bias tee is a plain three-valued state, and `Unknown` is one of the
        // three rather than an absence (T-325), so it has no `Option` to wrap.
        if first {
            self.bias_tee = o.bias_tee;
            self.bias_tee_mixed = o.bias_tee_mixed;
        } else if o.bias_tee_mixed || self.bias_tee != o.bias_tee {
            self.bias_tee_mixed = true;
        }
        if first {
            self.cell_shape = o.cell_shape;
            self.cell_shape_mixed = o.cell_shape_mixed;
        } else if o.cell_shape_mixed || !same_shape(self.cell_shape, o.cell_shape) {
            self.cell_shape_mixed = true;
        }
        for &(s, n, f) in &o.cell_shapes {
            self.add_shape_values(Some(s), n, f);
        }
        self.other_shape_values = self.other_shape_values.saturating_add(o.other_shape_values);
        self.frames += o.frames;
        self.suspect_frames += o.suspect_frames;
        self.dropped_samples += o.dropped_samples;
        self.gain_changes += o.gain_changes;
        for &(g, n) in &o.gain_states {
            self.add_gain(g, n);
        }
        self.other_gain_frames += o.other_gain_frames;
        self.unknown_gain_frames += o.unknown_gain_frames;
        for &(origin, n) in &o.origins {
            self.add_origin(origin, n);
        }
        self.other_origin_frames += o.other_origin_frames;
        for s in &o.steps {
            self.add_step(s);
        }
        self.steps_dropped += o.steps_dropped;
        for t in [o.first_frame, o.last_frame].into_iter().flatten() {
            self.first_frame = Some(self.first_frame.map_or(t, |x| x.min(t)));
            self.last_frame = Some(self.last_frame.map_or(t, |x| x.max(t)));
        }
    }
}

/// An open column's would-be stats: `(t, per-f (p_lo, p_hi, occupied seconds))`.
pub(crate) type ColumnPreview = (usize, Vec<(f32, f32, f64)>);

/// One frame value waiting for its level-0 time column to close.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ColEntry {
    pub f: u32,
    pub v: f32,
    /// Caller threshold, dB; NaN = use the default floor at close.
    pub thr: f32,
    pub dur_s: f32,
}

/// A tile accumulator. Per-cell arrays are row-major `t · nf + f`; `hist` is `f · bins + bin` and
/// holds the value histogram of each frequency cell over the **whole tile**, which is exactly the
/// histogram of the parent cell this tile rolls up into.
#[derive(Clone, Debug)]
pub(crate) struct Tile {
    pub key: TileKey,
    pub nf: usize,
    pub nt: usize,
    pub bins: usize,
    /// Global cell index of frequency cell 0 / time cell 0.
    pub f_cell0: i64,
    pub t_cell0: i64,
    pub t_cell_s: f64,
    pub count: Vec<u32>,
    /// dB; −∞ when unobserved.
    pub max: Vec<f32>,
    pub sum_lin: Vec<f64>,
    /// Observed seconds (unclipped sum of frame durations).
    pub obs_s: Vec<f64>,
    /// Occupied seconds.
    pub occ_s: Vec<f64>,
    pub occ_max: Vec<f32>,
    pub p_lo: Vec<f32>,
    pub p_hi: Vec<f32>,
    pub hist: Vec<u32>,
    pub prov: ProvenanceSummary,
    /// Level 0: the time column currently buffering values.
    pub col_t: Option<usize>,
    /// Level 0: the highest column already closed (later values for it take the late path).
    pub col_done: Option<usize>,
    pub col: Vec<ColEntry>,
}

impl Tile {
    pub fn new(key: TileKey, nf: usize, g: &LevelGeometry, bins: usize) -> Self {
        let n = nf * g.nt;
        let mut t = Self {
            key,
            nf,
            nt: g.nt,
            bins,
            f_cell0: 0,
            t_cell0: 0,
            t_cell_s: 0.0,
            count: vec![0; n],
            max: vec![0.0; n],
            sum_lin: vec![0.0; n],
            obs_s: vec![0.0; n],
            occ_s: vec![0.0; n],
            occ_max: vec![0.0; n],
            p_lo: vec![0.0; n],
            p_hi: vec![0.0; n],
            hist: vec![0; nf * bins],
            prov: ProvenanceSummary {
                gain_states: Vec::with_capacity(MAX_GAIN_STATES),
                steps: Vec::with_capacity(MAX_PROVENANCE_STEPS),
                origins: Vec::with_capacity(MAX_ORIGINS),
                cell_shapes: Vec::with_capacity(MAX_CELL_SHAPES),
                ..Default::default()
            },
            col_t: None,
            col_done: None,
            col: Vec::new(),
        };
        t.reset(key, g);
        t
    }

    /// Clears the accumulator for reuse under `key` (same dimensions). Allocation-free.
    pub fn reset(&mut self, key: TileKey, g: &LevelGeometry) {
        debug_assert_eq!(self.nt, g.nt);
        self.key = key;
        self.f_cell0 = key.f_block * self.nf as i64;
        self.t_cell0 = key.t_block * self.nt as i64;
        self.t_cell_s = g.t_cell_ns as f64 * 1e-9;
        self.count.fill(0);
        self.max.fill(f32::NEG_INFINITY);
        self.sum_lin.fill(0.0);
        self.obs_s.fill(0.0);
        self.occ_s.fill(0.0);
        self.occ_max.fill(0.0);
        self.p_lo.fill(f32::NAN);
        self.p_hi.fill(f32::NAN);
        self.hist.fill(0);
        self.prov.clear();
        self.col_t = None;
        self.col_done = None;
        self.col.clear();
    }

    /// Clears frequency cell `f` over the whole tile (retention trim, T-126): every time cell of
    /// the column reads unobserved and its histogram row empties. Returns whether it held data.
    pub fn clear_freq(&mut self, f: usize) -> bool {
        let row = f * self.bins..(f + 1) * self.bins;
        let had = (0..self.nt).any(|t| self.count[t * self.nf + f] > 0)
            || self.hist[row.clone()].iter().any(|&h| h > 0);
        for t in 0..self.nt {
            let i = t * self.nf + f;
            self.count[i] = 0;
            self.max[i] = f32::NEG_INFINITY;
            self.sum_lin[i] = 0.0;
            self.obs_s[i] = 0.0;
            self.occ_s[i] = 0.0;
            self.occ_max[i] = 0.0;
            self.p_lo[i] = f32::NAN;
            self.p_hi[i] = f32::NAN;
        }
        self.hist[row].fill(0);
        had
    }

    #[inline]
    pub fn hist_row(&self, f: usize) -> &[u32] {
        &self.hist[f * self.bins..(f + 1) * self.bins]
    }

    /// Observed and occupied seconds of cell `i`, with observation clipped to the cell duration
    /// (overlapping frames) and occupancy scaled with it.
    #[inline]
    pub fn cell_obs(&self, i: usize) -> (f64, f64) {
        let raw = self.obs_s[i];
        if raw <= 0.0 {
            return (0.0, 0.0);
        }
        let o = raw.min(self.t_cell_s);
        (o, self.occ_s[i].min(raw) * (o / raw))
    }

    /// Folds one resampled frame value into level-0 cell `(t, f)`.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn add_value(
        &mut self,
        t: usize,
        f: usize,
        v_db: f32,
        pk_db: f32,
        v_lin: f64,
        dur_s: f64,
        hist: &HistogramConfig,
    ) {
        let i = t * self.nf + f;
        self.count[i] += 1;
        self.sum_lin[i] += v_lin;
        self.obs_s[i] += dur_s;
        if pk_db > self.max[i] {
            self.max[i] = pk_db;
        }
        self.hist[f * self.bins + hist.bin(v_db)] += 1;
    }

    /// Occupancy for a value whose column has already closed (percentiles are not revisited).
    pub fn add_late_occupancy(&mut self, t: usize, f: usize, v_db: f32, thr: f32, dur_s: f64) {
        let i = t * self.nf + f;
        if v_db > thr {
            self.occ_s[i] += dur_s;
        }
        if self.obs_s[i] > 0.0 {
            self.occ_max[i] = (self.occ_s[i] / self.obs_s[i]).min(1.0) as f32;
        }
    }

    /// Closes the buffered level-0 column: exact percentiles and occupancy decisions.
    pub fn close_column(
        &mut self,
        floor: Option<&[f32]>,
        margin: f32,
        pct: (f32, f32),
        scratch: &mut Vec<f32>,
    ) {
        let Some(t) = self.col_t.take() else {
            return;
        };
        self.col_done = Some(self.col_done.map_or(t, |d| d.max(t)));
        let mut col = std::mem::take(&mut self.col);
        col.sort_unstable_by_key(|e| e.f);
        let nf = self.nf;
        column_stats(&col, floor, margin, pct, scratch, |f, plo, phi, occ| {
            let i = t * nf + f;
            self.p_lo[i] = plo;
            self.p_hi[i] = phi;
            self.occ_s[i] += occ;
            if self.obs_s[i] > 0.0 {
                self.occ_max[i] = (self.occ_s[i] / self.obs_s[i]).min(1.0) as f32;
            }
        });
        col.clear();
        self.col = col;
    }

    /// The stats the open column would get if closed now: `(t, per-f (p_lo, p_hi, occ_s))`.
    pub fn column_preview(
        &self,
        floor: Option<&[f32]>,
        margin: f32,
        pct: (f32, f32),
    ) -> Option<ColumnPreview> {
        let t = self.col_t?;
        let mut col = self.col.clone();
        col.sort_unstable_by_key(|e| e.f);
        let mut out = vec![(f32::NAN, f32::NAN, 0.0); self.nf];
        let mut scratch = Vec::new();
        column_stats(&col, floor, margin, pct, &mut scratch, |f, a, b, o| {
            out[f] = (a, b, o);
        });
        Some((t, out))
    }

    /// Rolls a sealed child tile (one level finer) into this tile. The child becomes time column
    /// `child.t_block − t_cell0`; each parent frequency cell aggregates `f_factor` child cells.
    ///
    /// Rules: max-of-max; power mean (Σ linear mean × frames); histogram sum → percentiles;
    /// occupancy = time-weighted mean over the child's time cells, then the **maximum** over the
    /// child frequency cells (a lower bound on "any child occupied", exact for one emitter per
    /// cell); max-occupancy = max over all child cells, so a short busy period survives; coverage =
    /// the best-observed child frequency cell. The parent cell is overwritten, so a fold is
    /// idempotent.
    pub fn fold_child(
        &mut self,
        child: &Tile,
        f_factor: u32,
        hist_cfg: &HistogramConfig,
        pct: (f32, f32),
        group: &mut Vec<u32>,
    ) {
        let tp = child.key.t_block - self.t_cell0;
        if tp < 0 || tp >= self.nt as i64 {
            debug_assert!(false, "child outside parent tile");
            return;
        }
        let tp = tp as usize;
        group.clear();
        group.resize(self.bins, 0);
        let factor = i64::from(f_factor);
        let mut fc = 0usize;
        while fc < child.nf {
            let gc = child.f_cell0 + fc as i64;
            let fp = gc.div_euclid(factor) - self.f_cell0;
            let group_end = child.nf.min(fc + (factor - gc.rem_euclid(factor)) as usize);
            let (mut count, mut max, mut sum_lin, mut occ_max) =
                (0u64, f32::NEG_INFINITY, 0.0, 0f32);
            let (mut best_obs, mut best_ratio) = (0.0f64, 0.0f64);
            let mut any_hist = false;
            for f in fc..group_end {
                let (mut obs_f, mut occ_f) = (0.0, 0.0);
                for t in 0..child.nt {
                    let i = t * child.nf + f;
                    let n = child.count[i];
                    if n == 0 {
                        continue;
                    }
                    count += u64::from(n);
                    max = max.max(child.max[i]);
                    sum_lin += child.sum_lin[i];
                    occ_max = occ_max.max(child.occ_max[i]);
                    let (o, c) = child.cell_obs(i);
                    obs_f += o;
                    occ_f += c;
                }
                if obs_f > 0.0 {
                    best_obs = best_obs.max(obs_f);
                    best_ratio = best_ratio.max(occ_f / obs_f);
                }
                let row = child.hist_row(f);
                if row.iter().any(|&c| c > 0) {
                    any_hist = true;
                    for (g, &c) in group.iter_mut().zip(row) {
                        *g += c;
                    }
                }
            }
            if count > 0 && fp >= 0 && (fp as usize) < self.nf {
                let fp = fp as usize;
                let i = tp * self.nf + fp;
                self.count[i] = count.min(u64::from(u32::MAX)) as u32;
                self.max[i] = max;
                self.sum_lin[i] = sum_lin;
                self.obs_s[i] = best_obs;
                self.occ_s[i] = best_ratio * best_obs;
                // A time-weighted mean never exceeds its maximum, so max-of-max alone suffices
                // (and keeps quantisation monotone: stored max = max of stored child maxima).
                self.occ_max[i] = occ_max;
                if any_hist {
                    self.p_lo[i] = hist_percentile(group, hist_cfg, pct.0);
                    self.p_hi[i] = hist_percentile(group, hist_cfg, pct.1);
                    let row = &mut self.hist[fp * self.bins..(fp + 1) * self.bins];
                    for (h, &g) in row.iter_mut().zip(group.iter()) {
                        *h = h.saturating_add(g);
                    }
                }
            }
            if any_hist {
                group.fill(0);
            }
            fc = group_end;
        }
        self.prov.merge(&child.prov);
    }

    /// Mean of cell `i` in dB (NaN when unobserved).
    pub fn mean_db(&self, i: usize) -> f32 {
        let n = self.count[i];
        if n == 0 {
            f32::NAN
        } else {
            super::stats::db(self.sum_lin[i] / f64::from(n))
        }
    }

    /// Sets cell `i` from decoded (stored) values.
    #[allow(clippy::too_many_arguments)]
    pub fn set_decoded(
        &mut self,
        i: usize,
        count: u32,
        max: f32,
        mean_db: f32,
        p_lo: f32,
        p_hi: f32,
        occupancy: f32,
        occ_max: f32,
        coverage: f32,
    ) {
        self.count[i] = count;
        self.max[i] = max;
        self.sum_lin[i] = undb(mean_db) * f64::from(count);
        self.p_lo[i] = p_lo;
        self.p_hi[i] = p_hi;
        let obs = f64::from(coverage) * self.t_cell_s;
        self.obs_s[i] = obs;
        self.occ_s[i] = f64::from(occupancy) * obs;
        self.occ_max[i] = occ_max;
    }
}

/// Percentiles and occupancy for a column's entries (sorted by `f`). Values without a caller
/// threshold use `floor[f] + margin`; when the floor is unknown (cold start), the 20th percentile
/// of every value in the column (across frequency) stands in for it.
pub(crate) fn column_stats(
    col: &[ColEntry],
    floor: Option<&[f32]>,
    margin: f32,
    pct: (f32, f32),
    scratch: &mut Vec<f32>,
    mut out: impl FnMut(usize, f32, f32, f64),
) {
    if col.is_empty() {
        return;
    }
    let known = |f: u32| floor.is_some_and(|fl| fl[f as usize].is_finite());
    let cold = if col.iter().any(|e| e.thr.is_nan() && !known(e.f)) {
        scratch.clear();
        scratch.extend(col.iter().map(|e| e.v));
        exact_percentile(scratch, 20.0)
    } else {
        f32::NAN
    };
    let mut i = 0;
    while i < col.len() {
        let f = col[i].f;
        let mut j = i + 1;
        while j < col.len() && col[j].f == f {
            j += 1;
        }
        scratch.clear();
        scratch.extend(col[i..j].iter().map(|e| e.v));
        let plo = exact_percentile(scratch, pct.0);
        let phi = exact_percentile(scratch, pct.1);
        let base = if known(f) {
            floor.map_or(cold, |fl| fl[f as usize])
        } else {
            cold
        };
        let thr_default = base + margin;
        let mut occ = 0.0;
        for e in &col[i..j] {
            let thr = if e.thr.is_nan() { thr_default } else { e.thr };
            if e.v > thr {
                occ += f64::from(e.dur_s);
            }
        }
        out(f as usize, plo, phi, occ);
        i = j;
    }
}
