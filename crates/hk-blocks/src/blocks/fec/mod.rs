//! Error-control blocks (T-087). `crc` runs on hk-estimate `framing::crc::BitCrc` (the RevEng
//! model over bit ranges; its whole-byte path is `CrcCore`) rather than a second CRC engine
//! (ADR-0011 §1.6).

use std::sync::Arc;

use hk_recipe::PortType::Frames;
use hk_recipe::{BlockDescriptor, ParamSchema, PortSpec};

use crate::Registry;
use crate::schema::{ParamExt, boolean, descriptor, hex, int, list, object, one_of, param};

mod bch;
mod checksum;
mod crc;
mod parity;
#[cfg(test)]
pub(crate) mod tests;

pub use bch::Bch;
pub use checksum::Checksum;
pub use crc::Crc;
pub use parity::Parity;

fn drop_invalid() -> ParamSchema {
    param(
        "drop_invalid",
        boolean(),
        "Drop frames that fail the check.",
    )
    .default_value(false)
    .hot()
}

fn span(doc: &str, trim_doc: &str) -> ParamSchema {
    param(
        "span",
        object(vec![
            param("start_bit", int(0, 1_000_000), "First covered bit.").default_value(0),
            param("end_trim_bits", int(0, 1_000_000), trim_doc).default_value(0),
        ]),
        doc,
    )
}

/// Pinned descriptors of this group.
pub fn planned() -> Vec<BlockDescriptor> {
    let frames = || {
        (
            vec![PortSpec::new("in", Frames)],
            vec![PortSpec::new("out", Frames)],
        )
    };
    let (crc_in, crc_out) = frames();
    let (bch_in, bch_out) = frames();
    let (par_in, par_out) = frames();
    let (sum_in, sum_out) = frames();
    vec![
        descriptor(
            "crc",
            "fec",
            "Checks (and optionally strips) a CRC over the frame, or per block with offset words (RDS), setting each frame's check status.",
            crc_in,
            crc_out,
            vec![
                param("width", int(1, 32), "CRC width, bits.").required(),
                param(
                    "poly",
                    hex(33),
                    "Polynomial, RevEng normal form (without the x^width term) or full form with it (RDS 0x5B9); the top term is implied either way.",
                )
                .required(),
                param("init", hex(32), "Register initial value.").default_value("0x0"),
                param("refin", boolean(), "Reflect input bytes.").default_value(false),
                param("refout", boolean(), "Reflect output.").default_value(false),
                param("xorout", hex(32), "Final XOR.").default_value("0x0"),
                param(
                    "span",
                    object(vec![
                        param("start_bit", int(0, 1_000_000), "First covered bit.")
                            .default_value(0),
                        param(
                            "end_trim_bits",
                            int(0, 1_000_000),
                            "Bits after the check word not covered.",
                        )
                        .default_value(0),
                    ]),
                    "Whole-frame mode: the check word ends end_trim_bits before the frame end.",
                ),
                param(
                    "blocks",
                    object(vec![
                        param("data_bits", int(1, 64), "Data bits per block.").required(),
                        param("check_bits", int(1, 32), "Check bits per block (= width).")
                            .required(),
                        param(
                            "offsets",
                            list(list(hex(32), 1), 1),
                            "Allowed offset words per block position (XORed onto the check word).",
                        )
                        .required(),
                    ]),
                    "Block mode: the frame is a sequence of data+check blocks (RDS: 4 × 16+10).",
                ),
                param(
                    "strip",
                    boolean(),
                    "Remove check bits from the output frame.",
                )
                .default_value(true),
                drop_invalid(),
                param(
                    "correct_burst_bits",
                    int(0, 5),
                    "Correct error bursts up to this length per block/frame. Refused where it would turn more than 1e-3 of random blocks valid (burst patterns × allowed offsets / 2^width): at build for blocks and the shortest span frame, per frame length at run time (status correction_skipped). RDS's 10-bit check allows none; CRC-24 over 112-bit Mode S allows up to 5.",
                )
                .default_value(0),
            ],
            true,
        ),
        descriptor(
            "bch",
            "fec",
            "Binary BCH/cyclic codeword decoding per word of the frame with bounded-distance correction and an optional overall parity bit (POCSAG BCH(31,21) + even parity); sets the frame's check status (valid only if every word is) and corrected bits.",
            bch_in,
            bch_out,
            vec![
                param(
                    "word_bits",
                    int(1, 64),
                    "Word length: the n code bits (k data then n − k check, MSB first), the parity bit if any, then ignored bits (POCSAG 32); a trailing partial word is passed through.",
                )
                .default_value(32),
                param("n", int(3, 63), "Code length, bits (POCSAG 31).").required(),
                param("k", int(1, 62), "Data bits (POCSAG 21); n − k ≤ 16.").required(),
                param(
                    "poly",
                    hex(64),
                    "Generator polynomial of degree n − k, full form (POCSAG 0x769) or without the top term.",
                )
                .required(),
                param(
                    "parity",
                    one_of(&["none", "even", "odd"]),
                    "Overall parity bit after the n code bits, over all n + 1 bits (POCSAG even). A decode that leaves it wrong is refused unless one more correction fits in correct_bits.",
                )
                .default_value("none"),
                param(
                    "correct_bits",
                    int(0, 3),
                    "Bit errors corrected per word (≤ the code's capacity: BCH(31,21) corrects 2); syndromes shared by two patterns of the lowest weight are refused. With parity none, errors beyond the code's capacity (3 bits on BCH(31,21)) can miscorrect to a wrong codeword that is marked valid; the parity bit catches most of them.",
                )
                .default_value(1),
                drop_invalid(),
            ],
            true,
        ),
        descriptor(
            "parity",
            "fec",
            "Per-unit parity over a span of the frame (e.g. 7-bit characters + parity bit); sets the frame's check status and optionally strips the parity bits, or replaces each with a constant 0 in place (unit width unchanged) for a check field computed over the pre-parity data padded back to the unit width (ACARS's block check).",
            par_in,
            par_out,
            vec![
                param("unit_bits", int(2, 64), "Unit length incl. its parity bit (8 for 7+1 characters).")
                    .required(),
                param("parity", one_of(&["even", "odd"]), "Parity of each unit's ones.").required(),
                param(
                    "position",
                    one_of(&["first", "last"]),
                    "Where the parity bit sits in a unit, in frame order (LSB-first characters reversed by sync_search: first).",
                )
                .default_value("last"),
                span(
                    "Checked units run from start_bit; a trailing partial unit is not checked.",
                    "Bits at the frame end not checked (e.g. a trailing CRC).",
                ),
                param("strip", boolean(), "Remove the parity bits of checked units.")
                    .default_value(false),
                param(
                    "zero",
                    boolean(),
                    "Replace the parity bit of each checked unit with 0 in place instead of removing it (unit width unchanged); exclusive with strip.",
                )
                .default_value(false),
                drop_invalid(),
            ],
            true,
        ),
        descriptor(
            "checksum",
            "fec",
            "Additive, XOR or ones'-complement checksum over units of the frame, compared with the check field at the end of the span; sets the frame's check status.",
            sum_in,
            sum_out,
            vec![
                param(
                    "algorithm",
                    one_of(&["sum", "xor", "ones-complement"]),
                    "sum: modular sum; xor: XOR (NMEA); ones-complement: end-around-carry sum (IP, with complement).",
                )
                .required(),
                param("unit_bits", int(8, 32), "Unit width: 8, 16 or 32; a trailing partial unit is zero-padded.")
                    .default_value(8),
                param("width", int(1, 32), "Check field width, bits (default unit_bits)."),
                param(
                    "endianness",
                    one_of(&["big", "little"]),
                    "Byte order of multi-byte units and the check field.",
                )
                .default_value("big"),
                param("init", hex(32), "Initial value.").default_value("0x0"),
                param("complement", boolean(), "Invert the result (IP checksum).")
                    .default_value(false),
                span(
                    "Covered data from start_bit up to the check field.",
                    "Bits after the check field not covered.",
                ),
                param("strip", boolean(), "Remove the check field.").default_value(true),
                drop_invalid(),
            ],
            true,
        ),
    ]
}

/// Registers this group's blocks.
pub fn register(r: &mut Registry) {
    use crate::blocks::framing::common::{BuildFn, FnFactory};
    let pinned = planned();
    let blocks: [(&str, BuildFn); 4] = [
        ("crc", crc::build),
        ("bch", bch::build),
        ("parity", parity::build),
        ("checksum", checksum::build),
    ];
    for (name, build) in blocks {
        r.register(Arc::new(FnFactory::new(&pinned, name, build)))
            .expect("fec block names are unique");
    }
}
