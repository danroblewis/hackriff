//! Stage tap `view=sync_search` (stream contract §14.4; T-162): the sync-word match score at
//! every candidate bit position of a `bits` stage output, so the Decode workbench can draw the
//! sync-search plot (ADR-0013 §4.9 gap 6) instead of trusting a single lock/quality number.
//!
//! **Where this runs.** [`SyncSearchTap::push`] is called from
//! [`super::taps::StageTap::publish`], inline on the pipeline thread, at the same point the
//! `view=spectrum` tap ([`super::tap_spectrum`]) computes its PSD. The correlator update per bit
//! is one shift, one XOR-and-mask and one `count_ones` (`hk_estimate::framing::sync::
//! SyncCorrelator`, T-087) — the same O(1)-per-bit cost the `sync_search` block itself already
//! pays while searching — so an attached consumer adds no meaningful CPU cost to the decode
//! chain. As with the spectrum tap, [`super::taps::StageTap::publish`] resets and skips this
//! work entirely while no consumer is attached (`handle.open_consumers() == 0`), so an unopened
//! or abandoned tap costs nothing and the decode chain never stalls on a slow reader.
//!
//! **What a row holds.** Element `i` of a row is the match score at candidate position `i`:
//! `1.0 - errors(i) / sync_bits`, where `errors(i)` is the Hamming distance between the word and
//! the `sync_bits` bits ending at that position. `1.0` is a perfect match; random data centres
//! on `0.5` with a spread that shrinks as `sync_bits` grows (§ below and the block-level test),
//! so a true sync word stands out as a sharp, isolated peak against a flat, noisy floor.
//!
//! **Row length and rate.** Unlike the spectrum tap's Welch averaging, there is nothing to
//! *average* here — a position's score isn't improved by blending it with its neighbours' — so
//! the §14.4 row-rate cap (25 rows/s, the same as `view=spectrum`) is met the other way: the row
//! is *widened* as the bit rate rises ([`SYNC_SEARCH_MIN_ROW_LEN`]..[`SYNC_SEARCH_MAX_ROW_LEN`]
//! candidate positions, always enough to contain several times any legal `SyncCorrelator` word
//! length of 64 bits with room either side), and once the row is already at its memory cap
//! ([`SYNC_SEARCH_MAX_ROW_LEN`], comparable to the spectrum tap's `4 * SPECTRUM_FFT_SIZE`-byte
//! row), by publishing only every `decimate`-th completed row and dropping the skipped ones'
//! scores. [`declared_row_rate_hz`] is exact for both regimes, so the header never promises a
//! rate the tap doesn't keep.
//!
//! **Gating.** Unlike `view=spectrum`, a sync-search row is content
//! (`StreamKind::payload_is_content`, `hk_stream::header`): the caller chooses `word`, so a
//! permitted score would let it probe withheld bits by trying candidates one at a time. It is
//! withheld under a class that forbids content, exactly like the `bits` port it reads (§14.5).

use hk_estimate::framing::sync::SyncCorrelator;
use hk_recipe::Recipe;
use hk_stream::{StreamHeader, StreamKind};

use super::taps::StreamCtx;

/// Row-rate cap, rows/s (§14.4: `view=spectrum` and `view=sync_search` share the cap).
pub const SYNC_SEARCH_MAX_ROWS_PER_S: f64 = 25.0;

/// A row always covers at least this many candidate positions: several times
/// [`SyncCorrelator`]'s maximum word length (64 bits), so a lone sync word is never split
/// outside the window it lands in.
pub const SYNC_SEARCH_MIN_ROW_LEN: usize = 256;

/// A row never exceeds this many candidate positions (32 KiB of `f32`s per row), the same order
/// of magnitude as the spectrum tap's row (`4 * SPECTRUM_FFT_SIZE` = 16 KiB).
pub const SYNC_SEARCH_MAX_ROW_LEN: usize = 8192;

/// Candidate positions per row, and how many completed rows to drop between published ones, so
/// the true row rate never exceeds [`SYNC_SEARCH_MAX_ROWS_PER_S`] regardless of `rate_hz`
/// (bit/s of the tapped port).
fn row_shape(rate_hz: f64) -> (usize, u32) {
    if !(rate_hz.is_finite() && rate_hz > 0.0) {
        return (SYNC_SEARCH_MIN_ROW_LEN, 1);
    }
    let needed = (rate_hz / SYNC_SEARCH_MAX_ROWS_PER_S).ceil() as usize;
    let len = needed.clamp(SYNC_SEARCH_MIN_ROW_LEN, SYNC_SEARCH_MAX_ROW_LEN);
    // `needed` positions/row would hit the cap at row length alone; once the row is capped at
    // SYNC_SEARCH_MAX_ROW_LEN, drop whole rows instead so the rate stays capped.
    let decimate = needed.div_ceil(SYNC_SEARCH_MAX_ROW_LEN).max(1) as u32;
    (len, decimate)
}

/// The declared row rate for a `view=sync_search` header at `rate_hz` (bit/s of the tapped
/// port). Always at most [`SYNC_SEARCH_MAX_ROWS_PER_S`].
pub fn declared_row_rate_hz(rate_hz: f64) -> f64 {
    if !(rate_hz.is_finite() && rate_hz > 0.0) {
        return 0.0;
    }
    let (len, decimate) = row_shape(rate_hz);
    rate_hz / (len as f64 * f64::from(decimate))
}

/// A header for a stage tap's `view=sync_search` (§14.4). `fft_size` is reused for the row
/// length (candidate positions per row), the same convention `tap_spectrum::spectrum_header`
/// uses for its PSD bin count; there is no RF geometry (`center_hz`/`bandwidth_hz` are left
/// unset), and `sample_rate_hz` is the row rate, not the bit rate.
pub fn sync_search_header(
    ctx: &StreamCtx,
    recipe: &Recipe,
    stream_id: String,
    rate_hz: f64,
) -> StreamHeader {
    let mut h = StreamHeader::new(
        stream_id,
        StreamKind::SyncSearch,
        ctx.class,
        format!("hk-pipeline:recipe:{}@{}", recipe.id, recipe.version),
    );
    h.datatype = Some("rf32_le".into());
    h.fft_size = Some(row_shape(rate_hz).0 as u32);
    h.sample_rate_hz = Some(declared_row_rate_hz(rate_hz));
    h.emitter_id = ctx.emitter_id;
    h
}

/// Streaming sync-word match score over a stage tap's bits: runs every bit through a
/// [`SyncCorrelator`] for `word`/`bits`, and yields one row of scores every [`row_shape`]
/// candidate positions (dropping rows past the rate cap, see the [module docs](self)).
pub struct SyncSearchTap {
    corr: SyncCorrelator,
    row: Vec<f32>,
    filled: usize,
    row_len: usize,
    decimate: u32,
    skip: u32,
    /// Set on a reset (no consumer, or a chunk discontinuity/edit); the next completed row
    /// carries `DISCONTINUITY` so a reader knows it isn't contiguous with the last one.
    pending_reset: bool,
}

impl SyncSearchTap {
    /// A tap engine searching for `word` (`bits`-bit, `SyncCorrelator`'s own MSB-first
    /// convention) over a bits port at `rate_hz` bit/s. `None` if `bits` is outside 1..=64 or
    /// `word` doesn't fit in it (the same validity rule the `sync_search` block itself applies).
    pub fn new(rate_hz: f64, word: u64, bits: u32) -> Option<Self> {
        let corr = SyncCorrelator::new(word, bits)?;
        let (row_len, decimate) = row_shape(rate_hz);
        Some(Self {
            corr,
            row: vec![0.0; row_len],
            filled: 0,
            row_len,
            decimate,
            skip: 0,
            pending_reset: true, // the first row after opening is a fresh window too
        })
    }

    /// Discards buffered/partial-row state (no consumer, or a chunk-level reset).
    pub fn reset(&mut self) {
        self.corr.reset();
        self.filled = 0;
        self.pending_reset = true;
    }

    /// Candidate positions per row (the row-length half of [`row_shape`] at this tap's rate),
    /// for callers sizing an encode scratch buffer.
    pub fn row_len(&self) -> usize {
        self.row_len
    }

    /// Feeds bits (one byte per bit, `& 1`, as the `bits` port carries them) and calls
    /// `emit(row, discontinuity)` for every row a window completes and isn't dropped for rate.
    pub fn push(&mut self, bits: &[u8], mut emit: impl FnMut(&[f32], bool)) {
        let sync_bits = self.corr.bits() as f32;
        for &b in bits {
            // `push` returns `None` only while the register is still filling after a reset (at
            // most `sync_bits - 1` bits, once per open/discontinuity): no candidate position
            // exists yet, so those bits contribute no row slot.
            let Some(errors) = self.corr.push(b) else {
                continue;
            };
            self.row[self.filled] = 1.0 - errors as f32 / sync_bits;
            self.filled += 1;
            if self.filled < self.row_len {
                continue;
            }
            self.filled = 0;
            if self.skip == 0 {
                emit(&self.row, self.pending_reset);
                self.pending_reset = false;
                self.skip = self.decimate - 1;
            } else {
                self.skip -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::ContentClass;

    fn recipe() -> Recipe {
        serde_json::from_value(serde_json::json!({
            "schema": "hackriff.recipe", "schema_version": 2, "id": "test", "version": 1,
            "name": "test", "input": {"port": "iq"},
            "nodes": [{"id": "a", "block": "identity"}],
            "outputs": [],
            "output_policy": {"content_class": "unrestricted"}
        }))
        .unwrap()
    }

    fn ctx() -> StreamCtx {
        StreamCtx {
            pipeline_id: "p1".into(),
            class: ContentClass::Unrestricted,
            center_hz: 0.0,
            bandwidth_hz: 1200.0,
            emitter_id: None,
            channels: Vec::new(),
            measured: None,
        }
    }

    /// xorshift64*, deterministic and dependency-free.
    struct Rng(u64);
    impl Rng {
        fn bit(&mut self) -> u8 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 & 1) as u8
        }
    }

    /// A 48-bit sync word planted once, at a known bit offset, inside a long run of random bits
    /// (a single burst, not a periodic frame — the strongest case a matched filter must still
    /// find). The row containing it must show a sharp peak at exactly that offset, and nothing
    /// else in the row should come close.
    ///
    /// **The margin, stated precisely.** A random `sync_bits`-bit window's Hamming distance to
    /// the word is Binomial(`sync_bits`, 0.5), so its score `1 - errors/sync_bits` has mean 0.5
    /// and sd `0.5/√sync_bits`; at `sync_bits = 48` that's ≈ 0.072. The row has
    /// [`SYNC_SEARCH_MIN_ROW_LEN`] = 256 other candidate positions competing to be the highest by
    /// chance, so the fair bar isn't "one sd above the mean" but the max of ~256 draws: even a
    /// generous 4-sd allowance (`P(Z > 4) ≈ 3×10⁻⁵` per position) puts at most a ~1% chance
    /// *any* of them tops `0.5 + 4·0.072 ≈ 0.79`, and the assertion below checks the actual
    /// runner-up against exactly that threshold — not a number picked to fit one run — while the
    /// true position scores a clean 1.0 (zero errors).
    #[test]
    fn known_sync_word_shows_a_clear_peak_and_nothing_comparable_elsewhere() {
        const WORD: u64 = 0x9E37_79B9_7F4A; // 48 bits (a fixed, unremarkable constant)
        const BITS: u32 = 48;
        const RATE_HZ: f64 = 1000.0; // small enough that one row spans the whole burst
        let mut tap = SyncSearchTap::new(RATE_HZ, WORD, BITS).expect("valid word/bits");
        assert_eq!(
            tap.row_len, SYNC_SEARCH_MIN_ROW_LEN,
            "1000 bit/s clamps to the row-length floor"
        );
        let chance_sd = 0.5 / f64::from(BITS).sqrt();
        let chance_ceiling = (0.5 + 4.0 * chance_sd) as f32;

        let mut rng = Rng(0xC0FF_EE15_C0FF_EE15);
        let plant_at = tap.row_len / 2; // candidate position the word will land on
        let mut bits = Vec::with_capacity(tap.row_len + BITS as usize);
        for _ in 0..plant_at {
            bits.push(rng.bit());
        }
        for k in (0..BITS).rev() {
            bits.push(((WORD >> k) & 1) as u8);
        }
        while bits.len() < tap.row_len + BITS as usize {
            bits.push(rng.bit());
        }

        let mut rows: Vec<Vec<f32>> = Vec::new();
        tap.push(&bits, |row, _disc| rows.push(row.to_vec()));
        assert_eq!(rows.len(), 1, "one full row from one burst");
        let row = &rows[0];

        let (peak_i, peak_v) = row
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert!(
            (*peak_v - 1.0).abs() < 1e-6,
            "the true position should score a perfect match: {peak_v}"
        );
        // Row slot 0 is the score once the first `BITS` bits (absolute index `BITS - 1`) have
        // been seen, so absolute index `k` lands in slot `k - (BITS - 1)`. The word's last bit
        // is at absolute index `plant_at + BITS - 1`, i.e. slot `plant_at`.
        assert_eq!(peak_i, plant_at, "peak at the true offset");

        let runner_up = row
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != peak_i)
            .map(|(_, &v)| v)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            runner_up < chance_ceiling,
            "runner-up {runner_up} should stay under the ~4-sd chance ceiling {chance_ceiling} \
             (peak {peak_v})"
        );
        assert!(
            peak_v - runner_up > 0.2,
            "peak {peak_v} should clear the next-best score {runner_up} by a wide, visible margin"
        );
    }

    #[test]
    fn declared_row_rate_is_bounded_at_low_and_extreme_rates() {
        for rate in [100.0, 1_200.0, 48_000.0, 2_000_000.0, 20_000_000.0] {
            let r = declared_row_rate_hz(rate);
            assert!(
                r > 0.0 && r <= SYNC_SEARCH_MAX_ROWS_PER_S + 1e-9,
                "rate_hz={rate}: declared row rate {r}"
            );
        }
    }

    #[test]
    fn header_shape() {
        let h = sync_search_header(&ctx(), &recipe(), "s1".into(), 1200.0);
        assert_eq!(h.kind, StreamKind::SyncSearch);
        assert_eq!(h.datatype.as_deref(), Some("rf32_le"));
        assert_eq!(h.fft_size, Some(SYNC_SEARCH_MIN_ROW_LEN as u32));
        assert!(h.sample_rate_hz.unwrap() <= SYNC_SEARCH_MAX_ROWS_PER_S);
        assert!(h.center_hz.is_none(), "no RF geometry for a bit-domain row");
    }

    #[test]
    fn reset_marks_the_next_row_as_a_discontinuity() {
        let mut tap = SyncSearchTap::new(1200.0, 0xABCD, 16).unwrap();
        let mut rng = Rng(1);
        let bits: Vec<u8> = (0..tap.row_len * 2 + 64).map(|_| rng.bit()).collect();
        let mut flags = Vec::new();
        tap.push(&bits, |_row, disc| flags.push(disc));
        assert_eq!(flags.first(), Some(&true), "fresh tap starts as a reset");
        tap.reset();
        let mut flags2 = Vec::new();
        tap.push(&bits, |_row, disc| flags2.push(disc));
        assert_eq!(flags2.first(), Some(&true), "reset() marks the next row");
        assert!(
            flags2.iter().skip(1).all(|d| !d),
            "only the first row after a reset is marked"
        );
    }

    #[test]
    fn invalid_word_or_bits_refused() {
        assert!(SyncSearchTap::new(1200.0, 0, 0).is_none(), "0 bits");
        assert!(SyncSearchTap::new(1200.0, 0, 65).is_none(), "> 64 bits");
        assert!(
            SyncSearchTap::new(1200.0, 0xFFFF, 8).is_none(),
            "word wider than bits"
        );
    }
}
