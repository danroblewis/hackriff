//! FLEX frame structure above the codeword: the sync-1 mode word, the frame information word, the
//! block interleave and the block/address/vector/message fields that make a page.
//!
//! # Sources, and what is unverified
//!
//! The public descriptions (sigidwiki "FLEX"; the structure multimon-ng's `flex.c` decodes,
//! read as a description of the format — no code is taken from it, ADR-0010) give: a 1600 bit/s
//! 2-level sync-1 of bit sync, an `A` word carrying a 16-bit **mode code** and the 32-bit marker
//! `0xA6C6AAAA`, the inverse of the `A` word, then the frame information word (FIW); a 25 ms
//! sync-2 at the frame's own rate; then 1760 ms of data in eleven 256-bit blocks per **phase**,
//! each block eight codewords sent column-wise.
//!
//! **Checked on the air** (the explorer's `flex-pagers-930p8`, T-950): the sync-1 layout
//! (`bit sync · code · marker · ~code · ~marker-high · FIW`), the codeword bit order
//! ([`super::bch`]), the FIW checksum and field positions (six FIWs with consecutive frame
//! numbers at 1.875 s spacing), the block interleave and phase order (block-information words
//! check in every phase), the BIW checksum and offsets, the vector-word checksum and fields,
//! and the mode codes `0xDEA0` = 3200 Bd 4-level and `0xB068` = 1600 Bd 4-level (the data
//! codewords only check at that clock and alphabet).
//!
//! **Unverified** (no truth in any fixture reaches them): the other mode codes' rates, the long
//! address capcode arithmetic, and the alphanumeric/numeric header semantics (fragment and
//! continuation bits, the numeric header skip). They only shape page *content*, which the
//! restricted-paging class withholds in every shipped configuration.

use super::bch::{self, Checked};

/// The 32-bit sync-1 marker, in the polarity where the marker's 1 bits are the **lower**
/// frequency.
pub const SYNC_MARKER: u32 = 0xA6C6_AAAA;
/// Most bit errors allowed in the marker, and between the mode code and its inverse.
pub const SYNC_MAX_ERRORS: u32 = 3;
/// Header (sync-1 and FIW) symbol rate, Bd: always 2-level at this rate.
pub const HEADER_BAUD: f64 = 1600.0;
/// Header bits from the first mode-code bit to the end of the FIW.
pub const HEADER_BITS: usize = 16 + 32 + 16 + 16 + 32;
/// Bits from the marker's first bit to the FIW's first bit.
pub const MARKER_TO_FIW_BITS: usize = 64;
/// Sync-2 duration, s.
pub const SYNC2_S: f64 = 0.025;
/// Data section duration, s.
pub const DATA_S: f64 = 1.760;
/// Frame period, s.
pub const FRAME_S: f64 = 1.875;
/// Blocks per phase per frame.
pub const BLOCKS: usize = 11;
/// Codewords per block.
pub const WORDS_PER_BLOCK: usize = 8;
/// Bits per block.
pub const BLOCK_BITS: usize = 32 * WORDS_PER_BLOCK;

/// A frame's transmission mode, as the sync-1 mode code declares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    /// The 16-bit code.
    pub code: u16,
    /// Data symbol rate, Bd.
    pub baud: u32,
    /// FSK levels in the data section.
    pub levels: u8,
}

impl Mode {
    /// Data bit rate, bit/s.
    pub fn bps(&self) -> u32 {
        self.baud * if self.levels == 4 { 2 } else { 1 }
    }
}

/// The mode codes (see the module docs for which are checked on the air).
pub const MODES: [Mode; 5] = [
    Mode {
        code: 0x870C,
        baud: 1600,
        levels: 2,
    },
    Mode {
        code: 0xB068,
        baud: 1600,
        levels: 4,
    },
    Mode {
        code: 0x7B18,
        baud: 3200,
        levels: 2,
    },
    Mode {
        code: 0xDEA0,
        baud: 3200,
        levels: 4,
    },
    Mode {
        code: 0x4C7C,
        baud: 3200,
        levels: 4,
    },
];

/// The mode a received code declares: the nearest table entry within [`SYNC_MAX_ERRORS`] bits.
pub fn mode_of(code: u16) -> Option<Mode> {
    MODES
        .iter()
        .map(|m| (u32::from(m.code ^ code).count_ones(), *m))
        .filter(|(d, _)| *d <= SYNC_MAX_ERRORS)
        .min_by_key(|(d, _)| *d)
        .map(|(_, m)| m)
}

/// Whether the 64 bits `code(16) · marker(32) · ~code(16)` (first bit = MSB, marker polarity)
/// are a sync-1: the marker within [`SYNC_MAX_ERRORS`] and the code agreeing with its inverse.
/// Returns the received code.
pub fn sync_code(window: u64) -> Option<u16> {
    let marker = (window >> 16) as u32;
    let hi = (window >> 48) as u16;
    let lo = !(window as u16);
    ((marker ^ SYNC_MARKER).count_ones() <= SYNC_MAX_ERRORS
        && u32::from(hi ^ lo).count_ones() <= SYNC_MAX_ERRORS)
        .then_some(hi)
}

/// The FLEX field checksum: the 4-bit groups of the 21 information bits (the last group is one
/// bit) sum to `0xF`. Frame, block and vector information words carry one.
pub fn checksum_ok(data: u32) -> bool {
    let d = data & bch::DATA_MASK;
    let sum = (0..5).map(|i| (d >> (4 * i)) & 0xF).sum::<u32>() + (d >> 20 & 1);
    sum & 0xF == 0xF
}

/// The frame information word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fiw {
    /// Cycle number, 0–14.
    pub cycle: u8,
    /// Frame number in the cycle, 0–127.
    pub frame: u8,
    /// The checksum held.
    pub checksum_ok: bool,
    /// Code-bit errors corrected.
    pub corrected: u8,
}

impl Fiw {
    /// Parses a checked FIW (data polarity).
    pub fn parse(c: &Checked) -> Self {
        let d = c.data();
        Fiw {
            cycle: (d >> 4 & 0xF) as u8,
            frame: (d >> 8 & 0x7F) as u8,
            checksum_ok: checksum_ok(d),
            corrected: c.corrected,
        }
    }
}

/// Deinterleaves one phase: bit `j` of a 256-bit block is bit `j / 8` of word `j % 8`. Returns
/// the words (FLEX bit order), eight per complete block.
pub fn deinterleave(bits: &[u8]) -> Vec<u32> {
    let blocks = bits.len() / BLOCK_BITS;
    let mut words = vec![0u32; blocks * WORDS_PER_BLOCK];
    for (j, &b) in bits[..blocks * BLOCK_BITS].iter().enumerate() {
        let block = j / BLOCK_BITS;
        let within = j % BLOCK_BITS;
        words[block * WORDS_PER_BLOCK + within % 8] |= u32::from(b & 1) << (within / 8);
    }
    words
}

/// The inverse of [`deinterleave`] (for synthesising frames in tests).
pub fn interleave(words: &[u32]) -> Vec<u8> {
    let blocks = words.len() / WORDS_PER_BLOCK;
    let mut bits = vec![0u8; blocks * BLOCK_BITS];
    for (j, b) in bits.iter_mut().enumerate() {
        let block = j / BLOCK_BITS;
        let within = j % BLOCK_BITS;
        *b = (words[block * WORDS_PER_BLOCK + within % 8] >> (within / 8) & 1) as u8;
    }
    bits
}

/// What a vector word says a page is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    /// Secure message.
    Secure,
    /// Short instruction (no message field).
    ShortInstruction,
    /// Tone only (no message field).
    Tone,
    /// Standard numeric.
    Numeric,
    /// Special-format numeric.
    SpecialNumeric,
    /// Alphanumeric text.
    Alphanumeric,
    /// Binary / transparent data.
    Binary,
    /// Numbered numeric.
    NumberedNumeric,
}

impl PageKind {
    fn from_bits(v: u32) -> Self {
        match v & 7 {
            0 => PageKind::Secure,
            1 => PageKind::ShortInstruction,
            2 => PageKind::Tone,
            3 => PageKind::Numeric,
            4 => PageKind::SpecialNumeric,
            5 => PageKind::Alphanumeric,
            6 => PageKind::Binary,
            _ => PageKind::NumberedNumeric,
        }
    }

    /// Stable lower-case name (decode metadata).
    pub fn as_str(self) -> &'static str {
        match self {
            PageKind::Secure => "secure",
            PageKind::ShortInstruction => "short-instruction",
            PageKind::Tone => "tone",
            PageKind::Numeric => "numeric",
            PageKind::SpecialNumeric => "special-numeric",
            PageKind::Alphanumeric => "alphanumeric",
            PageKind::Binary => "binary",
            PageKind::NumberedNumeric => "numbered-numeric",
        }
    }

    fn numeric(self) -> bool {
        matches!(
            self,
            PageKind::Numeric | PageKind::SpecialNumeric | PageKind::NumberedNumeric
        )
    }
}

/// One page: an address with its vector and, when it has one, its message.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    /// Phase letter, `A`–`D`.
    pub phase: char,
    /// Capcode (the receiving pager's address — metadata, not the transmitter's identity).
    pub capcode: u64,
    /// A two-word (long) address.
    pub long_address: bool,
    /// Page type from the vector word.
    pub kind: PageKind,
    /// Message words the vector points at.
    pub message_words: usize,
    /// Every address, vector and message word this page used passed its BCH check (and the
    /// vector its checksum). A page with any failed word carries no text.
    pub complete: bool,
    /// Message text (alphanumeric / numeric pages, complete ones only). **Content**: the caller
    /// stores it only under a class that permits content.
    pub text: Option<String>,
}

/// One phase of one frame, decoded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Phase {
    /// Phase letter.
    pub letter: char,
    /// Words checked (on air).
    pub words: usize,
    /// Words that passed the BCH check (at most two corrections) with their parity holding
    /// ([`Checked::valid`]).
    pub valid: usize,
    /// Of those, words that needed a correction.
    pub corrected: usize,
    /// Words passing the evidence rule ([`Checked::clean`]: at most one correction, parity
    /// holding).
    pub clean: usize,
    /// The block information word checked and its checksum held: the phase carries a frame.
    pub biw_ok: bool,
    /// Pages found.
    pub pages: Vec<Page>,
}

fn data_at(words: &[Option<Checked>], i: usize) -> Option<u32> {
    words
        .get(i)
        .copied()
        .flatten()
        .filter(Checked::valid)
        .map(|c| c.data())
}

/// Parses one phase's checked words (`None` = BCH failure, or a word never received).
pub fn parse_phase(letter: char, words: &[Option<Checked>]) -> Phase {
    let valid: Vec<&Checked> = words.iter().flatten().filter(|c| c.valid()).collect();
    let mut phase = Phase {
        letter,
        words: words.len(),
        valid: valid.len(),
        corrected: valid.iter().filter(|c| c.corrected > 0).count(),
        clean: valid.iter().filter(|c| c.clean()).count(),
        biw_ok: false,
        pages: Vec::new(),
    };
    // The BIW decides whether the phase carries a frame at all, so it must pass the evidence
    // rule, not merely be correctable.
    let Some(biw) = words
        .first()
        .copied()
        .flatten()
        .filter(Checked::clean)
        .map(|c| c.data())
    else {
        return phase;
    };
    phase.biw_ok = checksum_ok(biw);
    if !phase.biw_ok {
        return phase;
    }
    let voffset = (biw >> 10 & 0x3F) as usize;
    let aoffset = (biw >> 8 & 0x3) as usize + 1;
    let mut i = aoffset;
    while i < voffset.min(words.len()) {
        let j = voffset + i - aoffset;
        let Some(aw1) = data_at(words, i) else {
            i += 1;
            continue;
        };
        if aw1 == 0 || aw1 == bch::DATA_MASK {
            i += 1;
            continue;
        }
        let long_address = aw1 < 0x8001 || (aw1 > 0x1E_0000 && aw1 < 0x1F_0001) || aw1 > 0x1F_7FFE;
        let aw2 = if long_address {
            data_at(words, i + 1)
        } else {
            None
        };
        let capcode = if long_address {
            aw2.map(|a2| (u64::from(a2 ^ bch::DATA_MASK) << 15) + 0x1F_9000 + u64::from(aw1))
        } else {
            Some(u64::from(aw1) - 0x8000)
        };
        let viw = data_at(words, j);
        if let (Some(capcode), Some(viw)) = (capcode, viw.filter(|v| checksum_ok(*v))) {
            let kind = PageKind::from_bits(viw >> 4);
            let mw1 = (viw >> 7 & 0x7F) as usize;
            let len = if kind.numeric() {
                (viw >> 14 & 0x7) as usize
            } else {
                (viw >> 14 & 0x7F) as usize
            };
            let (text, complete, message_words) =
                message(words, kind, long_address, j, mw1, len);
            phase.pages.push(Page {
                phase: letter,
                capcode,
                long_address,
                kind,
                message_words,
                complete,
                text,
            });
        }
        i += if long_address { 2 } else { 1 };
    }
    phase
}

/// The message a vector points at: `(text, complete, words)`.
fn message(
    words: &[Option<Checked>],
    kind: PageKind,
    long_address: bool,
    j: usize,
    mw1: usize,
    len: usize,
) -> (Option<String>, bool, usize) {
    match kind {
        PageKind::Alphanumeric | PageKind::Secure => {
            // The first message word is a header (fragment/continuation), in the second vector
            // word for a long address (unverified, module docs).
            let (hdr, start, n) = if long_address {
                (data_at(words, j + 1), mw1, len.saturating_sub(1))
            } else {
                (data_at(words, mw1), mw1 + 1, len.saturating_sub(1))
            };
            let used: Vec<Option<u32>> = (start..start + n).map(|i| data_at(words, i)).collect();
            if hdr.is_none() || used.iter().any(Option::is_none) || start + n > words.len() {
                return (None, false, len);
            }
            let frag = hdr.unwrap() >> 11 & 0x3;
            let mut s = String::new();
            for (k, dw) in used.iter().flatten().enumerate() {
                let chars = [dw & 0x7F, dw >> 7 & 0x7F, dw >> 14 & 0x7F];
                let skip_first = k == 0 && frag == 0x3;
                for (c_i, c) in chars.iter().enumerate() {
                    if (skip_first && c_i == 0) || *c == 0x03 || *c == 0 {
                        continue;
                    }
                    s.push(char::from(*c as u8));
                }
            }
            (Some(s), true, len)
        }
        k if k.numeric() => {
            const BCD: &[u8; 16] = b"0123456789 U -][";
            let (first, start, end) = if long_address {
                (data_at(words, j + 1), mw1, mw1 + len)
            } else {
                (data_at(words, mw1), mw1 + 1, mw1 + len + 1)
            };
            let rest: Vec<Option<u32>> = (start..end).map(|i| data_at(words, i)).collect();
            if first.is_none() || rest.iter().any(Option::is_none) {
                return (None, false, len + 1);
            }
            let mut count = if k == PageKind::NumberedNumeric {
                14
            } else {
                6
            };
            let mut digit = 0u8;
            let mut s = String::new();
            for dw in std::iter::once(first).chain(rest).flatten() {
                let mut dw = dw;
                for _ in 0..21 {
                    digit = (digit >> 1) & 0x0F;
                    if dw & 1 == 1 {
                        digit ^= 0x08;
                    }
                    dw >>= 1;
                    count -= 1;
                    if count == 0 {
                        if digit != 0x0C {
                            s.push(char::from(BCD[digit as usize]));
                        }
                        count = 4;
                    }
                }
            }
            (Some(s.trim_end().to_owned()), true, len + 1)
        }
        PageKind::Binary => {
            let ok = (mw1..mw1 + len).all(|i| data_at(words, i).is_some());
            (None, ok, len)
        }
        _ => (None, true, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flex::bch::{check, encode};

    /// A field word with a valid checksum: `payload` in bits 4..21, the checksum nibble chosen.
    pub(crate) fn with_checksum(payload: u32) -> u32 {
        let p = payload & 0x1F_FFF0;
        let partial = (1..5).map(|i| (p >> (4 * i)) & 0xF).sum::<u32>() + (p >> 20 & 1);
        let ck = (0xF + 16 * 4 - partial) & 0xF;
        let d = p | ck;
        assert!(checksum_ok(d));
        d
    }

    #[test]
    fn the_sync_window_needs_marker_and_code_to_agree() {
        let w = |code: u16| u64::from(code) << 48 | u64::from(SYNC_MARKER) << 16 | u64::from(!code);
        assert_eq!(sync_code(w(0xDEA0)), Some(0xDEA0));
        assert_eq!(mode_of(0xDEA0).map(|m| (m.baud, m.levels)), Some((3200, 4)));
        assert_eq!(mode_of(0xB068).map(|m| (m.baud, m.levels)), Some((1600, 4)));
        assert_eq!(mode_of(0x870C).map(|m| m.bps()), Some(1600));
        assert_eq!(mode_of(0xDEA0).map(|m| m.bps()), Some(6400));
        // Three marker errors pass, four do not.
        assert!(sync_code(w(0x870C) ^ 0b111 << 20).is_some());
        assert!(sync_code(w(0x870C) ^ 0b1111 << 20).is_none());
        // A code that disagrees with its inverse is not a sync.
        assert!(sync_code(w(0x870C) ^ 0xFF).is_none());
    }

    #[test]
    fn the_air_words_checksums_hold() {
        // BIW and a vector word read off flex-pagers-930p8 (data polarity).
        assert!(checksum_ok(0x00_040B));
        assert!(checksum_ok(0x00_0C03));
        assert!(checksum_ok(0x02_02DE));
        assert!(!checksum_ok(0));
    }

    #[test]
    fn deinterleave_inverts_interleave_and_sends_columns() {
        let words: Vec<u32> = (0..16).map(|i| encode(0x1_0000 + i * 977)).collect();
        let bits = interleave(&words);
        assert_eq!(deinterleave(&bits), words);
        // The first eight bits on air are bit 0 of the block's eight words.
        for (w, bit) in bits[..8].iter().enumerate() {
            assert_eq!(u32::from(*bit), words[w] & 1);
        }
    }

    fn phase_of(data: &[u32]) -> Vec<Option<Checked>> {
        data.iter().map(|d| check(encode(*d))).collect()
    }

    #[test]
    fn a_short_address_alphanumeric_page_parses() {
        // BIW: aoffset = 1 (bits 8-9 = 0), voffset = 2 (bits 10-15).
        let biw = with_checksum(2 << 10);
        let capcode = 1_234_567u32;
        let aw = capcode + 0x8000;
        // Vector: type 5, message at word 3, two words (header + one text word).
        let viw = with_checksum(5 << 4 | 3 << 7 | 2 << 14);
        let hdr = 0u32; // fragment 0
        let text = u32::from(b'H') | u32::from(b'I') << 7 | u32::from(b'!') << 14;
        let mut data = vec![biw, aw, viw, hdr, text];
        data.resize(8, 0);
        let p = parse_phase('A', &phase_of(&data));
        assert!(p.biw_ok);
        assert_eq!(p.valid, 8);
        assert_eq!(p.pages.len(), 1);
        let page = &p.pages[0];
        assert_eq!(page.capcode, u64::from(capcode));
        assert!(!page.long_address);
        assert_eq!(page.kind, PageKind::Alphanumeric);
        assert!(page.complete);
        assert_eq!(page.text.as_deref(), Some("HI!"));
    }

    #[test]
    fn a_page_with_a_failed_message_word_carries_no_text() {
        let biw = with_checksum(2 << 10);
        let viw = with_checksum(5 << 4 | 3 << 7 | 2 << 14);
        let data = [biw, 1_000_000 + 0x8000, viw, 0, 0x1234];
        let mut words = phase_of(&data);
        words[4] = None;
        let p = parse_phase('B', &words);
        assert_eq!(p.pages.len(), 1);
        assert!(!p.pages[0].complete);
        assert_eq!(p.pages[0].text, None);
    }

    #[test]
    fn a_numeric_page_reads_bcd_digits() {
        let biw = with_checksum(2 << 10);
        // Standard numeric: message at word 3, one word beyond the first (bits 14-16 = 1).
        let viw = with_checksum(3 << 4 | 3 << 7 | 1 << 14);
        // 2 header bits, digits 1-9 (4 bits each, LSB first), then fill (0xC): 42 bits.
        let mut bits: Vec<u32> = vec![0, 0];
        for d in 1u32..=9 {
            bits.extend((0..4).map(|k| d >> k & 1));
        }
        bits.extend((0..4).map(|k| 0xCu32 >> k & 1));
        let word = |b: &[u32]| b.iter().enumerate().fold(0u32, |a, (k, v)| a | v << k);
        let data = [biw, 555_555 + 0x8000, viw, word(&bits[..21]), word(&bits[21..42])];
        let p = parse_phase('A', &phase_of(&data));
        assert_eq!(p.pages[0].kind, PageKind::Numeric);
        assert_eq!(p.pages[0].capcode, 555_555);
        assert_eq!(p.pages[0].text.as_deref(), Some("123456789"));
    }

    #[test]
    fn a_phase_without_a_valid_biw_carries_nothing() {
        let p = parse_phase('C', &phase_of(&[0, 0, 0, 0]));
        assert!(!p.biw_ok);
        assert_eq!(p.valid, 4);
        assert!(p.pages.is_empty());
    }
}
