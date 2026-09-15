//! Error-control blocks (T-087). Reuse hk-estimate `framing::crc::CrcCore` (RevEng model)
//! rather than a second CRC engine (ADR-0011 §1.6).

use hk_recipe::PortType::Frames;
use hk_recipe::{BlockDescriptor, PortSpec};

use crate::Registry;
use crate::schema::{ParamExt, boolean, descriptor, hex, int, list, object, param};

/// Pinned descriptors of this group.
pub fn planned() -> Vec<BlockDescriptor> {
    let frames = || {
        (
            vec![PortSpec::new("in", Frames)],
            vec![PortSpec::new("out", Frames)],
        )
    };
    let unpinned = |name: &str, doc: &str| {
        let (i, o) = frames();
        descriptor(name, "fec", doc, i, o, vec![], false)
    };
    let (crc_in, crc_out) = frames();
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
                param(
                    "drop_invalid",
                    boolean(),
                    "Drop frames that fail the check.",
                )
                .default_value(false)
                .hot(),
                param(
                    "correct_burst_bits",
                    int(0, 5),
                    "Correct error bursts up to this length per block/frame.",
                )
                .default_value(0),
            ],
            true,
        ),
        unpinned("bch", "BCH decode/correct (e.g. POCSAG BCH(31,21))."),
        unpinned("parity", "Parity check."),
        unpinned("checksum", "Additive / XOR checksums."),
    ]
}

/// Registers this group's implemented blocks (none yet).
pub fn register(_r: &mut Registry) {}
