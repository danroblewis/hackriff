//! Parser-authoring assist (T-091, ADR-0011 §7, docs/13 layer 4): classical helpers that look at
//! recorded bits or frames and **suggest** structure for a parser. Every result is a scored
//! suggestion the user accepts or edits into a recipe; nothing here is applied automatically or
//! treated as truth, and no known-protocol table is consulted to pick an answer (catalogues only
//! *name* what the search measured).
//!
//! - [`sync`]: sync-word hunting (repeated patterns with inversion folding and an error
//!   tolerance, extended to their maximal consensus, preamble stripped), frame-period hunting
//!   (masked autocorrelation) and **linear-block period** hunting: stacking `N`-bit blocks at the
//!   right alignment gives a GF(2) matrix of deficient rank when the blocks are codewords of a
//!   linear (or affine) code (RDS 26-bit blocks: rank ≤ 20; POCSAG 32-bit codewords: rank 21).
//! - [`codes`]: CRC / BCH / parity search over frames. For a CRC with generator `G`, the XOR of
//!   two equal-length frames (init and xorout cancel) is divisible by `G`, so `G` divides the GCD
//!   of the frame differences: the search is exhaustive over every polynomial of every width
//!   (3–32) without enumerating 2^w candidates. Catalogue generators are tried first when the
//!   GCD is too loose to factor. Frames may be grouped into classes (block index mod m) whose
//!   constants differ, which recovers RDS offset words as per-class xorouts.
//! - [`fields`]: per-bit entropy, constancy and transition rate over aligned frames; counter,
//!   length-field and CRC detectors; constant / counter / high-entropy regions emitted as a
//!   draft [`hk_recipe::FieldMap`].
//!
//! **Bounded compute.** Every entry point takes a [`Budget`] of word operations; when it runs
//! out the search stops, returns what it found so far and reports `partial: true` in its
//! [`WorkReport`].

pub mod codes;
pub mod fields;
pub(crate) mod gf2;
pub mod sync;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

pub use codes::{
    CodeKind, CodeReport, CodeSearchConfig, CodeSuggestion, CyclicInfo, ParityScope,
    ParitySuggestion, RevEngMatch, search_codes,
};
pub use fields::{
    BitStat, FieldKind, FieldSuggestion, FieldsConfig, FieldsReport, MAX_FIELD_BITS, SyncAlign,
    suggest_fields,
};
pub use sync::{
    PatternKind, PeriodMethod, PeriodSuggestion, StreamReport, SyncConfig, SyncFramesReport,
    SyncSuggestion, align_on_sync, analyze_stream, hunt_sync_frames,
};

/// Default work cap, in operations (charged at about 1 ns of release time each on the dev Mac
/// in the most expensive stages; ≤ about 1 s).
pub const DEFAULT_MAX_OPS: u64 = 500_000_000;

/// Largest sensible work cap (≤ about 2–3 s of release time); the API clamps `max_ops` to it.
pub const MAX_OPS: u64 = 1_500_000_000;

/// A work cap for one assist call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Maximum word operations (XORs/popcounts over 64-bit words and similar steps).
    pub max_ops: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_ops: DEFAULT_MAX_OPS,
        }
    }
}

/// How much work a call did and whether it stopped at the cap.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkReport {
    /// Word operations spent.
    pub ops: u64,
    /// The cap.
    pub max_ops: u64,
    /// The cap was hit: results are what was found before stopping.
    pub partial: bool,
    /// Hypotheses (search cells) evaluated.
    pub hypotheses: u64,
    /// What was skipped because of the cap or the input size.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

/// Work meter shared by the stages of one call.
#[derive(Debug)]
pub(crate) struct Meter {
    ops: u64,
    max: u64,
    hypotheses: u64,
    skipped: Vec<String>,
}

impl Meter {
    pub fn new(budget: Budget) -> Self {
        Self {
            ops: 0,
            max: budget.max_ops,
            hypotheses: 0,
            skipped: Vec::new(),
        }
    }

    /// Adds `n` operations; `false` once the cap is exceeded.
    pub fn charge(&mut self, n: u64) -> bool {
        self.ops = self.ops.saturating_add(n);
        self.ok()
    }

    pub fn ok(&self) -> bool {
        self.ops <= self.max
    }

    pub fn hypothesis(&mut self) {
        self.hypotheses += 1;
    }

    pub fn skip(&mut self, what: impl Into<String>) {
        let what = what.into();
        if !self.skipped.contains(&what) {
            self.skipped.push(what);
        }
    }

    pub fn report(&self) -> WorkReport {
        WorkReport {
            ops: self.ops,
            max_ops: self.max,
            partial: !self.ok(),
            hypotheses: self.hypotheses,
            skipped: self.skipped.clone(),
        }
    }
}

/// A recipe block fragment: `{"block": …, "params": {…}}` in the shape of the pinned
/// `hk_blocks` catalogue (ADR-0011 §1.5), for the user to paste into a recipe and validate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlockFragment {
    /// Block kind (`sync_search`, `crc`, `bch`, `parity`).
    pub block: String,
    /// Parameters.
    pub params: FragmentParams,
}

/// Block parameters of a [`BlockFragment`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FragmentParams {
    /// `sync_search` in `sync-word` mode.
    SyncWord(SyncWordParams),
    /// `sync_search` in `offset-words` mode.
    OffsetWords(OffsetWordsParams),
    /// `crc`.
    Crc(CrcFragment),
    /// `bch`.
    Bch(BchFragment),
    /// `parity`.
    Parity(ParityFragment),
}

/// `sync_search` `sync-word` parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncWordParams {
    /// `sync-word`.
    pub mode: String,
    /// The word as observed on air, MSB-first hex (`0x…`); for widths not a multiple of 4 see
    /// `sync_bits`.
    pub sync_word: String,
    /// Width, bits.
    pub sync_bits: usize,
    /// Tolerated bit errors.
    pub max_errors: usize,
    /// Frame bits after the sync (the modal spacing minus the sync), when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_bits: Option<usize>,
    /// Frames exclude the sync.
    pub include_sync: bool,
}

/// One offset word.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OffsetWord {
    /// A placeholder name (`o0`…) in stream order; the user renames them.
    pub name: String,
    /// The word (hex).
    pub word: String,
}

/// `sync_search` `offset-words` parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OffsetWordsParams {
    /// `offset-words`.
    pub mode: String,
    /// Block width, bits.
    pub block_bits: usize,
    /// Check bits per block.
    pub check_bits: usize,
    /// Generator, full form (hex, with the `x^w` term).
    pub poly: String,
    /// Offset words in observed block order.
    pub offsets: Vec<OffsetWord>,
    /// The block sequence (one name per position).
    pub sequence: Vec<Vec<String>>,
}

/// `crc` span.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrcSpan {
    /// First covered bit.
    pub start_bit: usize,
    /// Bits after the check field that it does not cover.
    pub end_trim_bits: usize,
}

/// `crc` block layout for per-position offsets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrcBlocks {
    /// Data bits per block.
    pub data_bits: usize,
    /// Check bits per block.
    pub check_bits: usize,
    /// Offset word per block position (hex).
    pub offsets: Vec<Vec<String>>,
}

/// `crc` parameters (RevEng model).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CrcFragment {
    /// Width, bits.
    pub width: u8,
    /// Generator, normal form without the `x^w` term (hex).
    pub poly: String,
    /// Register init, normal form (hex).
    pub init: String,
    /// Input reflected.
    pub refin: bool,
    /// Output reflected.
    pub refout: bool,
    /// Final XOR (hex).
    pub xorout: String,
    /// Covered span (frame mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<CrcSpan>,
    /// Block layout (per-position offsets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocks: Option<CrcBlocks>,
    /// Strip the check field.
    pub strip: bool,
}

/// `bch` parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BchFragment {
    /// Word width including any parity bit.
    pub word_bits: usize,
    /// Code length.
    pub n: usize,
    /// Data bits.
    pub k: usize,
    /// Generator, full form (hex).
    pub poly: String,
    /// Trailing overall parity (`even`/`odd`), when found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parity: Option<String>,
}

/// `parity` parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParityFragment {
    /// `even` or `odd`.
    pub parity: String,
    /// Bits per character (character parity) or absent (one parity bit over the span).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub char_bits: Option<usize>,
    /// Covered span.
    pub span: CrcSpan,
}

/// `0x…` hex of `v`, zero-padded to `bits`.
pub(crate) fn hex_bits(v: u64, bits: usize) -> String {
    format!("0x{v:0width$X}", width = bits.div_ceil(4).max(1))
}

/// MSB-first value of up to 64 bits.
pub(crate) fn bits_value(bits: &[u8]) -> u64 {
    bits.iter()
        .fold(0u64, |acc, &b| (acc << 1) | u64::from(b & 1))
}
