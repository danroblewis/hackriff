//! Baseline store (T-119, ADR-0012 §3.6, §9): one file per `BaselineKey` holding the frozen
//! reference and adaptive slot statistics, rewritten temp → fsync → rename at slot close, with a
//! byte quota evicting the least recently visited sites.
//!
//! This module owns the **persisted state** ([`BaselineState`]) and its codec; the logic that
//! fills it (pooling, maturity, winsorising, forgetting, CUSUM) lives in
//! `hk_context::occupancy::baseline`, which depends on this crate.
//!
//! # Layout and format
//! `<root>/<site>/<cal>/<scheme>-<factor>.bin`, where `<cal>` is `uncalibrated` or the
//! CalibrationState id. A file is:
//! - an uncompressed header: magic `HKBL`, format version (u16), the key, `last_visit` (i64 ns,
//!   sample clock), so eviction reads only the header;
//! - a zstd frame of the little-endian body, sparse by slot (empty slots are not written);
//! - an FNV-1a 64 checksum of the uncompressed body. A mismatch reads as corrupt, never as data.
//!
//! In memory the slots are sparse too ([`SlotSeries`], T-134): a series holds only the
//! hour-of-week slots it has observed. The file format is unchanged by that (version 2).
//!
//! All times are the device/sample clock (ADR-0012 §0).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use hk_model::attention::baseline::{BaselineKey, CalKey, HourOfWeek, SlotStats};
use hk_model::attention::occupancy::ChannelKey;
use hk_model::ids::{CalibrationStateId, SiteId};
use hk_model::time::Timestamp;
use serde::{Deserialize, Serialize};

/// File format version written (T-132: 2 adds the level class per series, the sequential
/// accumulators and the latched hours per subject). Version 1 is still read.
pub const BASELINE_FORMAT_VERSION: u16 = 2;
/// Default store quota (ADR-0012 §3.6).
pub const BASELINE_QUOTA_BYTES: u64 = 1 << 30;
/// Gain states kept per subject before it reports `mixed` (ADR-0012 §3.1).
pub const MAX_GAIN_STATES: usize = 4;

const MAGIC: &[u8; 4] = b"HKBL";

/// Store errors.
#[derive(Debug, thiserror::Error)]
pub enum BaselineStoreError {
    /// Filesystem.
    #[error("baseline store I/O: {0}")]
    Io(#[from] io::Error),
    /// The file is not a readable baseline (bad magic, version, checksum or truncation).
    #[error("baseline file {path}: {reason}")]
    Corrupt {
        /// File.
        path: PathBuf,
        /// What is wrong.
        reason: &'static str,
    },
}

/// What one baseline row describes within its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum BaselineSubject {
    /// A baseline cell: `cell_factor` level-0 cells starting at `index × cell_factor`.
    Cell {
        /// Baseline cell index.
        index: i64,
    },
    /// A learned channel (ADR-0012 §2.7).
    Channel {
        /// Key.
        key: ChannelKey,
    },
}

/// Slot statistics with exponential forgetting (the adaptive copy, §3.4): the same moments as
/// [`SlotStats`] with a real-valued visit count, so scaling by the forgetting factor keeps
/// mean = Σ/n exact.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecayedStats {
    /// Σ decayed visit weight.
    pub n: f64,
    /// Decayed observed (represented) seconds.
    pub observed_s: f64,
    /// Σ level, dB.
    pub sum_db: f64,
    /// Σ level², dB².
    pub sum_sq_db: f64,
    /// Σ weight·occupied, s.
    pub occupied_weight_s: f64,
    /// Σ weight, s.
    pub weight_s: f64,
    /// Max level (not decayed), dB; `-inf` when empty.
    pub max_db: f64,
}

impl Default for DecayedStats {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl DecayedStats {
    /// Empty.
    pub const EMPTY: DecayedStats = DecayedStats {
        n: 0.0,
        observed_s: 0.0,
        sum_db: 0.0,
        sum_sq_db: 0.0,
        occupied_weight_s: 0.0,
        weight_s: 0.0,
        max_db: f64::NEG_INFINITY,
    };

    /// Folds one visit (as [`SlotStats::add`]).
    pub fn add(
        &mut self,
        level_db: f64,
        max_db: f64,
        occupied_weight_s: f64,
        weight_s: f64,
        observed_s: f64,
    ) {
        self.n += 1.0;
        self.observed_s += observed_s;
        self.sum_db += level_db;
        self.sum_sq_db += level_db * level_db;
        self.occupied_weight_s += occupied_weight_s;
        self.weight_s += weight_s;
        self.max_db = self.max_db.max(max_db);
    }

    /// Multiplies every additive moment by `factor` (forgetting).
    pub fn scale(&mut self, factor: f64) {
        self.n *= factor;
        self.observed_s *= factor;
        self.sum_db *= factor;
        self.sum_sq_db *= factor;
        self.occupied_weight_s *= factor;
        self.weight_s *= factor;
    }

    /// Nothing folded (or forgotten to nothing).
    pub fn is_empty(&self) -> bool {
        self.n <= 0.0
    }
}

impl From<&SlotStats> for DecayedStats {
    fn from(s: &SlotStats) -> Self {
        Self {
            n: s.n_visits as f64,
            observed_s: s.observed_s,
            sum_db: s.sum_db,
            sum_sq_db: s.sum_sq_db,
            occupied_weight_s: s.occupied_weight_s,
            weight_s: s.weight_s,
            max_db: if s.n_visits == 0 {
                f64::NEG_INFINITY
            } else {
                s.max_db
            },
        }
    }
}

/// Which visits a series' level moments come from (T-132): a low-FCO channel's occupied and idle
/// levels are two modes, so they are pooled apart.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum LevelClass {
    /// Intervals with no occupied weight (level = idle level above the floor). Version-1 files
    /// load as this.
    #[default]
    Idle,
    /// Intervals with any occupied weight (level = occupied level above the floor; with too few
    /// occupied visits for a level, occupancy only).
    Occupied,
}

/// A per-slot statistic with an empty value (what an untouched slot reads as).
pub trait SlotValue: Copy {
    /// The untouched slot.
    const EMPTY: Self;
}

impl SlotValue for SlotStats {
    const EMPTY: Self = SlotStats::EMPTY;
}

impl SlotValue for DecayedStats {
    const EMPTY: Self = DecayedStats::EMPTY;
}

/// Minimum and maximum slots a full [`SlotSeries`] grows by (a parked day adds 24).
const SLOT_GROWTH: (usize, usize) = (4, 12);

/// Hour-of-week slots of one statistic, **sparse** (T-134): only touched slots are stored, in slot
/// order, behind a 168-bit presence mask. An untouched slot reads as [`SlotValue::EMPTY`]
/// (`series[i]`), and indexing mutably (`&mut series[i]`) stores it. So the memory of a series
/// follows the slots observed (48 after a parked 48 h run), not all [`HourOfWeek::SLOTS`], and
/// every read is the dense value: this is a representation, not a behaviour.
#[derive(Clone, Debug)]
pub struct SlotSeries<T> {
    mask: [u64; 3],
    values: Vec<T>,
}

impl<T: SlotValue> Default for SlotSeries<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: SlotValue> SlotSeries<T> {
    /// No slot touched.
    pub const fn new() -> Self {
        Self {
            mask: [0; 3],
            values: Vec::new(),
        }
    }

    fn present(&self, i: usize) -> bool {
        (self.mask[i / 64] >> (i % 64)) & 1 == 1
    }

    /// Stored slots below `i`.
    fn rank(&self, i: usize) -> usize {
        let w = i / 64;
        let below: u32 = self.mask[..w].iter().map(|m| m.count_ones()).sum();
        (below + (self.mask[w] & ((1_u64 << (i % 64)) - 1)).count_ones()) as usize
    }

    /// Slot `i` when stored.
    pub fn get(&self, i: usize) -> Option<&T> {
        (i < HourOfWeek::SLOTS && self.present(i)).then(|| &self.values[self.rank(i)])
    }

    /// Slot `i`'s value (`EMPTY` when untouched).
    pub fn value(&self, i: usize) -> T {
        self.get(i).copied().unwrap_or(T::EMPTY)
    }

    /// Slot `i`, stored (as `EMPTY`) if untouched. Panics when `i` ≥ [`HourOfWeek::SLOTS`].
    pub fn slot_mut(&mut self, i: usize) -> &mut T {
        assert!(i < HourOfWeek::SLOTS, "slot {i} out of range");
        let r = self.rank(i);
        if !self.present(i) {
            if self.values.len() == self.values.capacity() {
                let grow = self.values.len().clamp(SLOT_GROWTH.0, SLOT_GROWTH.1);
                self.values.reserve_exact(grow);
            }
            self.values.insert(r, T::EMPTY);
            self.mask[i / 64] |= 1 << (i % 64);
        }
        &mut self.values[r]
    }

    /// Heap bytes [`Self::slot_mut`]`(i)` would add (0 when stored or with spare capacity).
    pub fn growth_of(&self, i: usize) -> usize {
        if (i < HourOfWeek::SLOTS && self.present(i)) || self.values.len() < self.values.capacity()
        {
            0
        } else {
            self.values.len().clamp(SLOT_GROWTH.0, SLOT_GROWTH.1) * std::mem::size_of::<T>()
        }
    }

    /// Stored slots with their index, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &T)> + '_ {
        let mask = self.mask;
        (0..3)
            .flat_map(move |w| {
                let mut m = mask[w];
                std::iter::from_fn(move || {
                    (m != 0).then(|| {
                        let b = m.trailing_zeros() as usize;
                        m &= m - 1;
                        w * 64 + b
                    })
                })
            })
            .zip(&self.values)
    }

    /// Stored slots.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// No slot stored.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.values.capacity() * std::mem::size_of::<T>()
    }

    /// Drops spare capacity (after a load).
    pub fn shrink_to_fit(&mut self) {
        self.values.shrink_to_fit();
    }

    /// The same slots mapped by `f` (untouched slots stay untouched).
    pub fn map<U: SlotValue>(&self, f: impl FnMut(&T) -> U) -> SlotSeries<U> {
        SlotSeries {
            mask: self.mask,
            values: self.values.iter().map(f).collect(),
        }
    }
}

/// Equal when every slot reads equal (stored-as-`EMPTY` equals untouched).
impl<T: SlotValue + PartialEq> PartialEq for SlotSeries<T> {
    fn eq(&self, other: &Self) -> bool {
        (0..HourOfWeek::SLOTS).all(|i| self.value(i) == other.value(i))
    }
}

impl std::ops::Index<usize> for SlotSeries<SlotStats> {
    type Output = SlotStats;
    fn index(&self, i: usize) -> &SlotStats {
        assert!(i < HourOfWeek::SLOTS, "slot {i} out of range");
        self.get(i).unwrap_or(&SlotStats::EMPTY)
    }
}

impl std::ops::Index<usize> for SlotSeries<DecayedStats> {
    type Output = DecayedStats;
    fn index(&self, i: usize) -> &DecayedStats {
        assert!(i < HourOfWeek::SLOTS, "slot {i} out of range");
        self.get(i).unwrap_or(&DecayedStats::EMPTY)
    }
}

impl<T: SlotValue> std::ops::IndexMut<usize> for SlotSeries<T>
where
    Self: std::ops::Index<usize, Output = T>,
{
    fn index_mut(&mut self, i: usize) -> &mut T {
        self.slot_mut(i)
    }
}

/// The stored slots' values, in slot order (untouched slots contribute nothing to a sum).
impl<'a, T> IntoIterator for &'a SlotSeries<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.values.iter()
    }
}

/// The stored slots' values, mutably (forgetting leaves untouched slots empty).
impl<'a, T> IntoIterator for &'a mut SlotSeries<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.values.iter_mut()
    }
}

/// Reference and adaptive slots of one subject under one gain state and level class.
#[derive(Clone, Debug, PartialEq)]
pub struct GainSeries {
    /// Front-end gain-state key (hash of the gain settings; 0 = unknown/single).
    pub gain: u32,
    /// Level class (T-132).
    pub class: LevelClass,
    /// Frozen reference over [`HourOfWeek::SLOTS`] slots, sparse (T-134).
    pub reference: SlotSeries<SlotStats>,
    /// Adaptive copy over [`HourOfWeek::SLOTS`] slots, sparse (T-134).
    pub adaptive: SlotSeries<DecayedStats>,
}

impl GainSeries {
    /// Empty series for `gain`.
    pub fn new(gain: u32) -> Self {
        Self::with_class(gain, LevelClass::Idle)
    }

    /// Empty series for `gain` and `class`.
    pub fn with_class(gain: u32, class: LevelClass) -> Self {
        Self {
            gain,
            class,
            reference: SlotSeries::new(),
            adaptive: SlotSeries::new(),
        }
    }

    /// Heap bytes of both copies' stored slots.
    pub fn heap_bytes(&self) -> usize {
        self.reference.heap_bytes() + self.adaptive.heap_bytes()
    }

    /// Heap bytes storing slot `i` in both copies would add.
    pub fn growth_of(&self, i: usize) -> usize {
        self.reference.growth_of(i) + self.adaptive.growth_of(i)
    }
}

/// Two-sided CUSUM accumulators of (adaptive − reference)/σ_ref (§3.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CusumState {
    /// Level, upward.
    pub level_pos: f64,
    /// Level, downward.
    pub level_neg: f64,
    /// Occupancy, upward.
    pub occ_pos: f64,
    /// Occupancy, downward.
    pub occ_neg: f64,
}

/// Which statistic crossed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeStatistic {
    /// Mean level.
    Level,
    /// Occupancy (FCO).
    Occupancy,
}

/// A latched change point: the adaptive copy diverged from the frozen reference. Stays until the
/// user re-freezes (§3.4).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangePoint {
    /// Crossing time (sample clock).
    pub t: Timestamp,
    /// Statistic.
    pub statistic: ChangeStatistic,
    /// +1 up, −1 down.
    pub direction: i8,
    /// CUSUM value at the crossing, in reference σ.
    pub cusum: f64,
}

/// Baseline of one cell or channel.
#[derive(Clone, Debug, PartialEq)]
pub struct SubjectBaseline {
    /// Per gain state, at most [`MAX_GAIN_STATES`].
    pub gains: Vec<GainSeries>,
    /// More gain states were seen than are kept: levels under the extra states are not folded.
    pub mixed: bool,
    /// CUSUM accumulators.
    pub cusum: CusumState,
    /// Open change point.
    pub change_point: Option<ChangePoint>,
    /// Sequential test of fold z-scores against the reference, over all folds (T-132): gates
    /// reference learning subject-wide while it builds, so a weak persistent interferer (every
    /// fold below the novelty z) cannot drain into the reference.
    pub seq: CusumState,
    /// The same test per hour of day (empty until first used, then 24 entries): a crossing latches
    /// that hour only.
    pub seq_hod: Vec<CusumState>,
    /// Hours of day (bit i = hour i) whose reference learning is latched off by a change point or
    /// a sequential crossing, until re-freeze (T-132 per-slot latch).
    pub latched_hours: u32,
    /// When the reference first became mature (any resolution).
    pub mature_at: Option<Timestamp>,
    /// Last user re-freeze.
    pub refrozen_at: Option<Timestamp>,
    /// Last fold.
    pub last_seen: Timestamp,
}

impl SubjectBaseline {
    /// Empty.
    pub fn new(t: Timestamp) -> Self {
        Self {
            gains: Vec::new(),
            mixed: false,
            cusum: CusumState::default(),
            change_point: None,
            seq: CusumState::default(),
            seq_hod: Vec::new(),
            latched_hours: 0,
            mature_at: None,
            refrozen_at: None,
            last_seen: t,
        }
    }
}

/// Everything stored under one [`BaselineKey`].
#[derive(Clone, Debug, PartialEq)]
pub struct BaselineState {
    /// Key.
    pub key: BaselineKey,
    /// Last fold into any subject (eviction order).
    pub last_visit: Timestamp,
    /// Subjects, sparse (only observed ones).
    pub subjects: BTreeMap<BaselineSubject, SubjectBaseline>,
}

/// Approximate heap bytes of one stored slot pair (reference + adaptive).
pub const SLOT_PAIR_BYTES: usize =
    std::mem::size_of::<SlotStats>() + std::mem::size_of::<DecayedStats>();

impl SubjectBaseline {
    /// Approximate heap bytes (the subject, its map node and its series' stored slots; T-132
    /// memory cap).
    pub fn approx_bytes(&self) -> usize {
        let node = std::mem::size_of::<BaselineSubject>() + std::mem::size_of::<Self>() + 16;
        let series = self.gains.capacity() * std::mem::size_of::<GainSeries>()
            + self.gains.iter().map(GainSeries::heap_bytes).sum::<usize>();
        node + series + self.seq_hod.capacity() * std::mem::size_of::<CusumState>()
    }
}

impl BaselineState {
    /// Approximate heap bytes of the whole state (T-132 memory cap).
    pub fn approx_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .subjects
                .values()
                .map(SubjectBaseline::approx_bytes)
                .sum::<usize>()
    }

    /// Empty state for `key`.
    pub fn new(key: BaselineKey, t: Timestamp) -> Self {
        Self {
            key,
            last_visit: t,
            subjects: BTreeMap::new(),
        }
    }
}

// ---- codec ----

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn opt_t(&mut self, t: Option<Timestamp>) {
        match t {
            Some(t) => {
                self.u8(1);
                self.i64(t.as_unix_nanos());
            }
            None => self.u8(0),
        }
    }
    fn key(&mut self, k: &BaselineKey) {
        // Ids as their fixed 36-byte hyphenated text (no uuid dependency here).
        self.0.extend_from_slice(k.site.to_string().as_bytes());
        match k.cal {
            CalKey::Uncalibrated => {
                self.u8(0);
                self.0.extend_from_slice(&[b'0'; 36]);
            }
            CalKey::Calibrated(id) => {
                self.u8(1);
                self.0.extend_from_slice(id.to_string().as_bytes());
            }
        }
        self.u16(k.scheme);
        self.u16(k.cell_factor);
    }
}

struct R<'a> {
    b: &'a [u8],
    at: usize,
}

type Rd<T> = Result<T, &'static str>;

impl R<'_> {
    fn take(&mut self, n: usize) -> Rd<&[u8]> {
        let s = self.b.get(self.at..self.at + n).ok_or("truncated")?;
        self.at += n;
        Ok(s)
    }
    fn arr<const N: usize>(&mut self) -> Rd<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }
    fn u8(&mut self) -> Rd<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Rd<u16> {
        Ok(u16::from_le_bytes(self.arr()?))
    }
    fn u32(&mut self) -> Rd<u32> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    fn u64(&mut self) -> Rd<u64> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    fn i64(&mut self) -> Rd<i64> {
        Ok(i64::from_le_bytes(self.arr()?))
    }
    fn f64(&mut self) -> Rd<f64> {
        Ok(f64::from_le_bytes(self.arr()?))
    }
    fn opt_t(&mut self) -> Rd<Option<Timestamp>> {
        Ok(match self.u8()? {
            0 => None,
            1 => Some(Timestamp::from_unix_nanos(self.i64()?)),
            _ => return Err("bad option tag"),
        })
    }
    fn key(&mut self) -> Rd<BaselineKey> {
        let site = id_text::<SiteId>(self.take(36)?)?;
        let tag = self.u8()?;
        let cal_text = self.take(36)?;
        let cal = match tag {
            0 => CalKey::Uncalibrated,
            1 => CalKey::Calibrated(id_text::<CalibrationStateId>(cal_text)?),
            _ => return Err("bad calibration tag"),
        };
        Ok(BaselineKey {
            site,
            cal,
            scheme: self.u16()?,
            cell_factor: self.u16()?,
        })
    }
}

fn id_text<T: std::str::FromStr>(b: &[u8]) -> Rd<T> {
    std::str::from_utf8(b)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or("bad id")
}

fn header(key: &BaselineKey, last_visit: Timestamp, version: u16) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(64));
    w.0.extend_from_slice(MAGIC);
    w.u16(version);
    w.key(key);
    w.i64(last_visit.as_unix_nanos());
    w.0
}

const HEADER_LEN: usize = 4 + 2 + 36 + 1 + 36 + 2 + 2 + 8;

fn encode_body(state: &BaselineState, version: u16) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(state.subjects.len() as u32);
    for (subject, s) in &state.subjects {
        match subject {
            BaselineSubject::Cell { index } => {
                w.u8(0);
                w.i64(*index);
            }
            BaselineSubject::Channel { key } => {
                w.u8(1);
                w.u16(key.scheme);
                w.i64(key.lo_cell);
                w.i64(key.hi_cell);
            }
        }
        w.u8(u8::from(s.mixed));
        for v in [
            s.cusum.level_pos,
            s.cusum.level_neg,
            s.cusum.occ_pos,
            s.cusum.occ_neg,
        ] {
            w.f64(v);
        }
        match s.change_point {
            Some(cp) => {
                w.u8(1);
                w.i64(cp.t.as_unix_nanos());
                w.u8(match cp.statistic {
                    ChangeStatistic::Level => 0,
                    ChangeStatistic::Occupancy => 1,
                });
                w.u8(cp.direction as u8);
                w.f64(cp.cusum);
            }
            None => w.u8(0),
        }
        w.opt_t(s.mature_at);
        w.opt_t(s.refrozen_at);
        w.i64(s.last_seen.as_unix_nanos());
        if version >= 2 {
            w.u8(s.seq_hod.len() as u8);
            for c in std::iter::once(&s.seq).chain(&s.seq_hod) {
                for v in [c.level_pos, c.level_neg, c.occ_pos, c.occ_neg] {
                    w.f64(v);
                }
            }
            w.u32(s.latched_hours);
        }
        w.u8(s.gains.len() as u8);
        for g in &s.gains {
            w.u32(g.gain);
            if version >= 2 {
                w.u8(match g.class {
                    LevelClass::Idle => 0,
                    LevelClass::Occupied => 1,
                });
            }
            let refs: Vec<_> = g
                .reference
                .iter()
                // T-132: occupancy-only slots (no level visit) are data too.
                .filter(|(_, x)| x.n_visits > 0 || x.observed_s > 0.0 || x.weight_s > 0.0)
                .collect();
            w.u8(refs.len() as u8);
            for (slot, x) in refs {
                w.u8(slot as u8);
                w.u64(x.n_visits);
                for v in [
                    x.observed_s,
                    x.sum_db,
                    x.sum_sq_db,
                    x.occupied_weight_s,
                    x.weight_s,
                    x.max_db,
                ] {
                    w.f64(v);
                }
            }
            let ads: Vec<_> = g
                .adaptive
                .iter()
                .filter(|(_, x)| !x.is_empty() || x.observed_s > 0.0 || x.weight_s > 0.0)
                .collect();
            w.u8(ads.len() as u8);
            for (slot, x) in ads {
                w.u8(slot as u8);
                for v in [
                    x.n,
                    x.observed_s,
                    x.sum_db,
                    x.sum_sq_db,
                    x.occupied_weight_s,
                    x.weight_s,
                    x.max_db,
                ] {
                    w.f64(v);
                }
            }
        }
    }
    w.0
}

fn slot_index(v: u8) -> Rd<usize> {
    let i = usize::from(v);
    if i < HourOfWeek::SLOTS {
        Ok(i)
    } else {
        Err("slot out of range")
    }
}

fn read_cusum(r: &mut R<'_>) -> Rd<CusumState> {
    Ok(CusumState {
        level_pos: r.f64()?,
        level_neg: r.f64()?,
        occ_pos: r.f64()?,
        occ_neg: r.f64()?,
    })
}

fn decode_body(
    version: u16,
    key: BaselineKey,
    last_visit: Timestamp,
    b: &[u8],
) -> Rd<BaselineState> {
    let mut r = R { b, at: 0 };
    let n = r.u32()?;
    let mut state = BaselineState::new(key, last_visit);
    for _ in 0..n {
        let subject = match r.u8()? {
            0 => BaselineSubject::Cell { index: r.i64()? },
            1 => BaselineSubject::Channel {
                key: ChannelKey {
                    scheme: r.u16()?,
                    lo_cell: r.i64()?,
                    hi_cell: r.i64()?,
                },
            },
            _ => return Err("bad subject tag"),
        };
        let mixed = r.u8()? != 0;
        let cusum = read_cusum(&mut r)?;
        let change_point = match r.u8()? {
            0 => None,
            1 => Some(ChangePoint {
                t: Timestamp::from_unix_nanos(r.i64()?),
                statistic: match r.u8()? {
                    0 => ChangeStatistic::Level,
                    1 => ChangeStatistic::Occupancy,
                    _ => return Err("bad statistic tag"),
                },
                direction: r.u8()? as i8,
                cusum: r.f64()?,
            }),
            _ => return Err("bad option tag"),
        };
        let mature_at = r.opt_t()?;
        let refrozen_at = r.opt_t()?;
        let last_seen = Timestamp::from_unix_nanos(r.i64()?);
        let (mut seq, mut seq_hod, mut latched_hours) = (CusumState::default(), Vec::new(), 0);
        if version >= 2 {
            let n_hod = usize::from(r.u8()?);
            if n_hod != 0 && n_hod != 24 {
                return Err("bad hour-of-day accumulators");
            }
            seq = read_cusum(&mut r)?;
            for _ in 0..n_hod {
                seq_hod.push(read_cusum(&mut r)?);
            }
            latched_hours = r.u32()?;
        }
        let n_gains = usize::from(r.u8()?);
        if n_gains > 2 * MAX_GAIN_STATES {
            return Err("too many gain states");
        }
        let mut gains = Vec::with_capacity(n_gains);
        for _ in 0..n_gains {
            let gain = r.u32()?;
            let class = if version >= 2 {
                match r.u8()? {
                    0 => LevelClass::Idle,
                    1 => LevelClass::Occupied,
                    _ => return Err("bad level class"),
                }
            } else {
                LevelClass::Idle
            };
            let mut g = GainSeries::with_class(gain, class);
            for _ in 0..r.u8()? {
                let slot = slot_index(r.u8()?)?;
                g.reference[slot] = SlotStats {
                    n_visits: r.u64()?,
                    observed_s: r.f64()?,
                    sum_db: r.f64()?,
                    sum_sq_db: r.f64()?,
                    occupied_weight_s: r.f64()?,
                    weight_s: r.f64()?,
                    max_db: r.f64()?,
                };
            }
            for _ in 0..r.u8()? {
                let slot = slot_index(r.u8()?)?;
                g.adaptive[slot] = DecayedStats {
                    n: r.f64()?,
                    observed_s: r.f64()?,
                    sum_db: r.f64()?,
                    sum_sq_db: r.f64()?,
                    occupied_weight_s: r.f64()?,
                    weight_s: r.f64()?,
                    max_db: r.f64()?,
                };
            }
            g.reference.shrink_to_fit();
            g.adaptive.shrink_to_fit();
            gains.push(g);
        }
        state.subjects.insert(
            subject,
            SubjectBaseline {
                gains,
                mixed,
                cusum,
                change_point,
                seq,
                seq_hod,
                latched_hours,
                mature_at,
                refrozen_at,
                last_seen,
            },
        );
    }
    if r.at != b.len() {
        return Err("trailing bytes");
    }
    Ok(state)
}

/// Encodes a state as a complete file image.
pub fn encode(state: &BaselineState) -> io::Result<Vec<u8>> {
    encode_version(state, BASELINE_FORMAT_VERSION)
}

/// Encodes as `version` (1 drops the T-132 fields; kept for the backward-read test).
fn encode_version(state: &BaselineState, version: u16) -> io::Result<Vec<u8>> {
    let body = encode_body(state, version);
    let mut out = header(&state.key, state.last_visit, version);
    out.extend_from_slice(&zstd::encode_all(body.as_slice(), 3)?);
    out.extend_from_slice(&fnv1a(&body).to_le_bytes());
    Ok(out)
}

fn read_header(bytes: &[u8]) -> Rd<(BaselineKey, Timestamp, u16)> {
    let mut r = R { b: bytes, at: 0 };
    if r.take(4)? != MAGIC {
        return Err("bad magic");
    }
    let version = r.u16()?;
    if !(1..=BASELINE_FORMAT_VERSION).contains(&version) {
        return Err("unsupported format version");
    }
    let key = r.key()?;
    Ok((key, Timestamp::from_unix_nanos(r.i64()?), version))
}

/// Decodes a file image.
pub fn decode(bytes: &[u8]) -> Result<BaselineState, &'static str> {
    let (key, last_visit, version) = read_header(bytes)?;
    if bytes.len() < HEADER_LEN + 8 {
        return Err("truncated");
    }
    let (frame, sum) = bytes[HEADER_LEN..].split_at(bytes.len() - HEADER_LEN - 8);
    let body = zstd::decode_all(frame).map_err(|_| "bad compressed body")?;
    if fnv1a(&body).to_le_bytes() != sum {
        return Err("checksum mismatch");
    }
    decode_body(version, key, last_visit, &body)
}

// ---- store ----

fn cal_dir(cal: CalKey) -> String {
    match cal {
        CalKey::Uncalibrated => "uncalibrated".into(),
        CalKey::Calibrated(id) => id.to_string(),
    }
}

/// The baseline directory tree with its quota.
#[derive(Clone, Debug)]
pub struct BaselineStore {
    root: PathBuf,
    quota_bytes: u64,
}

impl BaselineStore {
    /// Opens (creating) `root` with the default quota.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            quota_bytes: BASELINE_QUOTA_BYTES,
        })
    }

    /// Sets the byte quota.
    pub fn with_quota(mut self, bytes: u64) -> Self {
        self.quota_bytes = bytes;
        self
    }

    /// Root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// File of `key`.
    pub fn path_of(&self, key: &BaselineKey) -> PathBuf {
        self.root
            .join(key.site.to_string())
            .join(cal_dir(key.cal))
            .join(format!("{}-{}.bin", key.scheme, key.cell_factor))
    }

    /// Writes `state` atomically (temp → fsync → rename); returns the file size.
    pub fn save(&self, state: &BaselineState) -> Result<u64, BaselineStoreError> {
        let path = self.path_of(&state.key);
        let dir = path.parent().expect("key paths have a parent");
        fs::create_dir_all(dir)?;
        let bytes = encode(state)?;
        let tmp = path.with_extension("bin.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(bytes.len() as u64)
    }

    /// Reads the state of `key`, `None` when never saved.
    pub fn load(&self, key: &BaselineKey) -> Result<Option<BaselineState>, BaselineStoreError> {
        let path = self.path_of(key);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let state = decode(&bytes).map_err(|reason| BaselineStoreError::Corrupt {
            path: path.clone(),
            reason,
        })?;
        if state.key != *key {
            return Err(BaselineStoreError::Corrupt {
                path,
                reason: "key does not match its path",
            });
        }
        Ok(Some(state))
    }

    /// Every stored file: key, last visit and size (header reads only). Unreadable files are
    /// skipped.
    pub fn entries(&self) -> Result<Vec<(BaselineKey, Timestamp, u64)>, BaselineStoreError> {
        let mut out = Vec::new();
        for site in read_dir_or_empty(&self.root)? {
            for cal in read_dir_or_empty(&site)? {
                for file in read_dir_or_empty(&cal)? {
                    if file.extension().and_then(|e| e.to_str()) != Some("bin") {
                        continue;
                    }
                    let Ok(bytes) = fs::read(&file) else { continue };
                    if let Ok((key, t, _)) = read_header(&bytes) {
                        out.push((key, t, bytes.len() as u64));
                    }
                }
            }
        }
        out.sort_by_key(|e| e.0);
        Ok(out)
    }

    /// Bytes used.
    pub fn usage(&self) -> Result<u64, BaselineStoreError> {
        Ok(self.entries()?.iter().map(|e| e.2).sum())
    }

    /// Enforces the quota: while over it, removes whole sites, least recently visited first
    /// (a site's visit time is its newest file's), never `protect` (the current site). Returns the
    /// evicted sites.
    pub fn enforce_quota(
        &self,
        protect: Option<SiteId>,
    ) -> Result<Vec<SiteId>, BaselineStoreError> {
        let entries = self.entries()?;
        let mut used: u64 = entries.iter().map(|e| e.2).sum();
        let mut sites: BTreeMap<SiteId, (Timestamp, u64)> = BTreeMap::new();
        for (k, t, size) in &entries {
            let e = sites.entry(k.site).or_insert((*t, 0));
            e.0 = e.0.max(*t);
            e.1 += size;
        }
        let mut order: Vec<_> = sites
            .into_iter()
            .filter(|(s, _)| Some(*s) != protect)
            .collect();
        order.sort_by_key(|(s, (t, _))| (*t, *s));
        let mut evicted = Vec::new();
        for (site, (_, size)) in order {
            if used <= self.quota_bytes {
                break;
            }
            fs::remove_dir_all(self.root.join(site.to_string()))?;
            used = used.saturating_sub(size);
            evicted.push(site);
        }
        Ok(evicted)
    }
}

fn read_dir_or_empty(dir: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::read_dir(dir) {
        Ok(rd) => rd.map(|e| e.map(|e| e.path())).collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "hk-baseline-{tag}-{}-{}",
            std::process::id(),
            SiteId::new()
        ));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn state(site: SiteId, t: i64) -> BaselineState {
        let key = BaselineKey {
            site,
            cal: CalKey::Calibrated(CalibrationStateId::new()),
            scheme: 1,
            cell_factor: 16,
        };
        let mut s = BaselineState::new(key, Timestamp::from_unix_nanos(t));
        let mut sub = SubjectBaseline::new(Timestamp::from_unix_nanos(t));
        let mut g = GainSeries::new(7);
        g.reference[3].add(-90.0, -80.0, true, 60.0, 60.0);
        g.reference[167].add(-95.0, -95.0, false, 30.0, 30.0);
        g.adaptive[3].add(-91.0, -85.0, 12.0, 60.0, 60.0);
        g.adaptive[3].scale(0.5);
        sub.gains.push(g);
        sub.mixed = true;
        sub.cusum.occ_pos = 2.5;
        sub.change_point = Some(ChangePoint {
            t: Timestamp::from_unix_nanos(t),
            statistic: ChangeStatistic::Occupancy,
            direction: -1,
            cusum: 9.0,
        });
        sub.mature_at = Some(Timestamp::from_unix_nanos(5));
        s.subjects
            .insert(BaselineSubject::Cell { index: -4 }, sub.clone());
        sub.gains.clear();
        s.subjects.insert(
            BaselineSubject::Channel {
                key: ChannelKey {
                    scheme: 1,
                    lo_cell: 10,
                    hi_cell: 14,
                },
            },
            sub,
        );
        s
    }

    #[test]
    fn baseline_codec_round_trips_and_rejects_corruption() {
        let s = state(SiteId::new(), 42);
        let bytes = encode(&s).unwrap();
        assert_eq!(decode(&bytes).unwrap(), s);
        // T-132: version 1 files still read (the new fields default).
        let mut v1 = s.clone();
        let mut v2 = s.clone();
        for sub in v2.subjects.values_mut() {
            sub.seq.level_pos = 1.5;
            sub.seq_hod = vec![CusumState::default(); 24];
            sub.seq_hod[3].occ_neg = 2.0;
            sub.latched_hours = 1 << 3;
            for g in &mut sub.gains {
                g.class = LevelClass::Occupied;
            }
        }
        assert_eq!(decode(&encode(&v2).unwrap()).unwrap(), v2);
        for sub in v1.subjects.values_mut() {
            sub.seq = CusumState::default();
        }
        assert_eq!(decode(&encode_version(&v2, 1).unwrap()).unwrap(), v1);
        let mut bad = bytes.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert_eq!(decode(&bad).unwrap_err(), "checksum mismatch");
        assert_eq!(decode(&bytes[..10]).unwrap_err(), "truncated");
        let mut magic = bytes;
        magic[0] = b'X';
        assert_eq!(decode(&magic).unwrap_err(), "bad magic");
    }

    /// A deterministic state touching every v2 field: two subjects, idle and occupied series,
    /// occupancy-only and level slots, sequential accumulators, latched hours.
    fn golden_state() -> BaselineState {
        let key = BaselineKey {
            site: "6f1c2a9e-3b4d-4e5f-8a6b-7c8d9e0f1a2b".parse().unwrap(),
            cal: CalKey::Calibrated("0a1b2c3d-4e5f-4a6b-9c7d-8e9f0a1b2c3d".parse().unwrap()),
            scheme: 1,
            cell_factor: 16,
        };
        let t = |s: i64| Timestamp::from_unix_nanos(s * 1_000_000_000);
        let mut s = BaselineState::new(key, t(1_000_000));
        let mut sub = SubjectBaseline::new(t(999_000));
        let mut idle = GainSeries::new(0xA);
        idle.reference[0].add(2.0, 3.5, false, 1800.0, 1800.0);
        idle.reference[25].add(2.5, 2.5, false, 900.0, 1800.0);
        idle.reference[167].observed_s = 600.0;
        idle.reference[167].weight_s = 600.0;
        idle.adaptive[0].add(2.1, 3.5, 0.0, 1800.0, 1800.0);
        idle.adaptive[0].scale(0.75);
        idle.adaptive[100].observed_s = 42.0;
        let mut occ = GainSeries::with_class(0xB, LevelClass::Occupied);
        occ.reference[12].add(15.0, 18.0, true, 1800.0, 1800.0);
        occ.adaptive[12].add(15.5, 18.0, 900.0, 1800.0, 1800.0);
        occ.adaptive[13].add(14.5, 16.0, 450.0, 1800.0, 1800.0);
        sub.gains = vec![idle, occ];
        sub.cusum.level_pos = 1.25;
        sub.seq.occ_neg = 0.5;
        sub.seq_hod = vec![CusumState::default(); 24];
        sub.seq_hod[12].level_pos = 3.0;
        sub.latched_hours = 1 << 12;
        sub.change_point = Some(ChangePoint {
            t: t(999_500),
            statistic: ChangeStatistic::Level,
            direction: 1,
            cusum: 8.5,
        });
        sub.mature_at = Some(t(990_000));
        s.subjects
            .insert(BaselineSubject::Cell { index: 7 }, sub.clone());
        sub.gains.truncate(1);
        sub.seq_hod.clear();
        sub.change_point = None;
        s.subjects.insert(
            BaselineSubject::Channel {
                key: ChannelKey {
                    scheme: 1,
                    lo_cell: 69_000,
                    hi_cell: 69_004,
                },
            },
            sub,
        );
        s
    }

    /// [`golden_state`] as written by the dense-slot encoder before T-134 (version 2).
    const GOLDEN_V2_DENSE_HEX: &str = concat!(
        "484b424c020036663163326139652d336234642d346535662d386136622d376338643965306631613262013061316232",
        "6333642d346535662d346136622d396337642d386539663061316232633364010010000080c6a47e8d030028b52ffd00",
        "583d08007209243070c7690c3c86a2285c5421402050702f410941a9d0fdef062f2116c03b0f5f6767b6faff444b5446",
        "51dbe560766f9902517f7a299302565d87072359488b69a0c6e43e7a8b9db11891ea9fe8b8c85b346a63e760a0a25898",
        "0d524a291fab5133e175dcc7a020b3aa83561bedb40416fc8fefa717daa6869986706d7e0870a14cb675aff5a1408470",
        "18d6a7c288ff1739203003199136071c4628b86a199951c913acd206bb3e787630fd66cc8139b7b233a200480266e0ea",
        "e02efb08595d913048c0813b307745e0e5b6e4eda5f46c9f705b661f0a1348284bfce2e20e5d10392edf6a662e960205",
        "1a0a709007bb4c7a0ebb1481eaaab35654082fd7db0a693d8e60045c2c5ae2f4bf7309",
    );

    /// T-134: a version-2 file written with dense in-memory slots loads into the sparse series
    /// unchanged, stores only its touched slots, and re-encodes to the same body (the format did
    /// not change, so the version stays 2).
    #[test]
    fn baseline_codec_reads_v2_files_written_by_the_dense_layout() {
        let bytes: Vec<u8> = (0..GOLDEN_V2_DENSE_HEX.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&GOLDEN_V2_DENSE_HEX[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(BASELINE_FORMAT_VERSION, 2);
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded, golden_state());
        let sub = &decoded.subjects[&BaselineSubject::Cell { index: 7 }];
        let stored: Vec<(Vec<usize>, Vec<usize>)> = sub
            .gains
            .iter()
            .map(|g| {
                (
                    g.reference.iter().map(|(i, _)| i).collect(),
                    g.adaptive.iter().map(|(i, _)| i).collect(),
                )
            })
            .collect();
        assert_eq!(
            stored,
            vec![(vec![0, 25, 167], vec![0, 100]), (vec![12], vec![12, 13])]
        );
        let body = |b: &[u8]| zstd::decode_all(&b[HEADER_LEN..b.len() - 8]).unwrap();
        assert_eq!(body(&encode(&decoded).unwrap()), body(&bytes));
    }

    /// T-134: sparse slots read as the dense values, store only what is touched, and grow in
    /// bounded steps.
    #[test]
    fn baseline_slot_series_reads_dense_and_stores_sparse() {
        let mut s: SlotSeries<DecayedStats> = SlotSeries::new();
        assert_eq!(s[5], DecayedStats::EMPTY);
        assert!(s.is_empty() && s.heap_bytes() == 0);
        let one = std::mem::size_of::<DecayedStats>();
        assert_eq!(s.growth_of(100), 4 * one);
        for i in [100, 3, 167, 64, 63] {
            s[i].add(f64::from(i as u32), 0.0, 0.0, 1.0, 1.0);
        }
        assert_eq!(s.growth_of(3), 0, "stored");
        assert_eq!(s.growth_of(4), 0, "full at 4, grew by 4: spare capacity");
        s[4].n = 0.0;
        assert_eq!((s.len(), s.heap_bytes()), (6, 8 * one));
        let order: Vec<usize> = s.iter().map(|(i, _)| i).collect();
        assert_eq!(order, vec![3, 4, 63, 64, 100, 167]);
        for i in [3, 63, 64, 100, 167] {
            assert_eq!(s[i].sum_db, f64::from(i as u32));
        }
        s[5].n = 0.0;
        s[6].n = 0.0;
        assert_eq!(s.growth_of(7), 8 * one, "full at 8, grows by 8");
        s[5] = DecayedStats::EMPTY;
        s[6] = DecayedStats::EMPTY;
        // Stored-as-empty equals untouched.
        let mut t = s.clone();
        let _ = &mut t[20];
        t[4] = DecayedStats::EMPTY;
        s[4] = DecayedStats::EMPTY;
        assert_eq!(s, t);
        let sum: f64 = (&s).into_iter().map(|d| d.sum_db).sum();
        assert_eq!(sum, 3.0 + 63.0 + 64.0 + 100.0 + 167.0);
        let mapped = s.map(|d| SlotStats {
            n_visits: d.n as u64,
            ..SlotStats::EMPTY
        });
        assert_eq!(mapped.len(), s.len());
    }

    #[test]
    fn baseline_store_saves_atomically_and_loads_by_key() {
        let root = temp_root("save");
        let store = BaselineStore::open(&root).unwrap();
        let s = state(SiteId::new(), 1);
        assert_eq!(store.load(&s.key).unwrap(), None);
        store.save(&s).unwrap();
        assert!(!store.path_of(&s.key).with_extension("bin.tmp").exists());
        assert_eq!(store.load(&s.key).unwrap().unwrap(), s);
        let entries = store.entries().unwrap();
        assert_eq!((entries.len(), entries[0].0), (1, s.key));
        fs::write(store.path_of(&s.key), b"HKBLgarbage").unwrap();
        assert!(matches!(
            store.load(&s.key),
            Err(BaselineStoreError::Corrupt { .. })
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn baseline_quota_evicts_least_recently_visited_sites_first() {
        let root = temp_root("quota");
        let store = BaselineStore::open(&root).unwrap();
        let (old, mid, new) = (SiteId::new(), SiteId::new(), SiteId::new());
        let mut size = 0;
        for (site, t) in [(old, 10), (mid, 20), (new, 30)] {
            size = store.save(&state(site, t)).unwrap();
        }
        // Room for two files: the oldest site goes.
        let store = store.with_quota(2 * size);
        assert_eq!(store.enforce_quota(None).unwrap(), vec![old]);
        // Room for one, the least recent (mid) is protected: `new` goes instead.
        let store = store.with_quota(size);
        assert_eq!(store.enforce_quota(Some(mid)).unwrap(), vec![new]);
        assert_eq!(store.entries().unwrap()[0].0.site, mid);
        let _ = fs::remove_dir_all(root);
    }
}
