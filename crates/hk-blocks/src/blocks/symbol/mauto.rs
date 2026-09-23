//! ADR-0011 §9 (T-606) catalogue rows for group `symbol`: `mlevel_slicer`, `descramble`,
//! `bitstuff`, `codeword_map`, `despread`, `equalise`. **Ports are pinned; parameters are
//! placeholders** (`params_pinned: false`) that each implementing ticket pins (T-613 `bitstuff`;
//! the rest are unfiled, docs/18 §9). `descramble` (T-608) and `mlevel_slicer` (T-612) are
//! implemented with their parameters pinned: their rows are `super::descramble::descriptor` and
//! `super::mlevel::descriptor`.

use hk_recipe::PortType::{Bits, Frames, Iq, Soft};
use hk_recipe::{BlockDescriptor, PortSpec};

use crate::schema::{ParamExt, descriptor, float, hex, int, list, one_of, param};

/// `bits|frames → same`: a streaming mode, and a per-frame mode that restarts at each frame.
fn bits_or_frames() -> (Vec<PortSpec>, Vec<PortSpec>) {
    (
        vec![PortSpec::any_of("in", &[Bits, Frames])],
        vec![PortSpec::any_of("out", &[Bits, Frames])],
    )
}

/// The group-`symbol` rows ADR-0011 §9.1 adds.
pub fn planned() -> Vec<BlockDescriptor> {
    let (dsc_in, dsc_out) = bits_or_frames();
    let (stf_in, stf_out) = bits_or_frames();
    let (cwm_in, cwm_out) = bits_or_frames();
    vec![
        super::mlevel::descriptor(),
        super::descramble::descriptor(dsc_in, dsc_out),
        descriptor(
            "bitstuff",
            "symbol",
            "HDLC zero-bit (de)stuffing; flags pass through in bits mode so sync_search still \
             frames on them.",
            stf_in,
            stf_out,
            vec![
                param("flag", hex(8), "Flag octet.").default_value("0x7E"),
                param("stuff_after", int(1, 16), "Ones before a stuffed zero.").default_value(5),
                param(
                    "direction",
                    one_of(&["destuff", "stuff"]),
                    "Remove stuffed zeros (receive) or insert them.",
                )
                .default_value("destuff"),
                param("abort_ones", int(2, 32), "Ones that mean abort/idle.").default_value(7),
            ],
            false,
        ),
        descriptor(
            "codeword_map",
            "symbol",
            "Fixed n-bit codeword → k-bit value table (constant-weight / m-of-n codes).",
            cwm_in,
            cwm_out,
            vec![
                param("word_bits", int(2, 32), "n: bits per codeword.").required(),
                param("value_bits", int(1, 32), "k: bits per value.").required(),
                param(
                    "table",
                    list(hex(32), 2),
                    "Codewords; the index is the value.",
                )
                .required(),
                param(
                    "align",
                    one_of(&["auto", "fixed"]),
                    "bits: find codeword boundaries by table validity, or start at bit 0.",
                )
                .default_value("auto"),
                param(
                    "on_invalid",
                    one_of(&["drop", "substitute"]),
                    "A word not in the table (counted in error_rate).",
                )
                .default_value("substitute"),
            ],
            false,
        ),
        descriptor(
            "despread",
            "symbol",
            "DSSS chip-sequence correlator: chips per symbol → k bits per symbol.",
            vec![PortSpec::any_of("in", &[Soft, Bits])],
            vec![PortSpec::new("out", Bits)],
            vec![
                param("chips_per_symbol", int(2, 1_024), "Chips per symbol.").required(),
                param(
                    "sequences",
                    list(hex(1_024), 2),
                    "Chip sequence per symbol value; the index is the value.",
                )
                .required(),
                param(
                    "bit_order",
                    one_of(&["msb", "lsb"]),
                    "Order the value's bits are emitted (802.15.4: lsb).",
                )
                .default_value("msb"),
                param(
                    "max_chip_errors",
                    int(0, 1_024),
                    "Worst accepted correlation, in chip errors.",
                )
                .hot(),
            ],
            false,
        ),
        descriptor(
            "equalise",
            "symbol",
            "Blind adaptive equaliser on complex samples, ahead of the demodulator.",
            vec![PortSpec::new("in", Iq)],
            vec![PortSpec::new("out", Iq)],
            vec![
                param(
                    "algorithm",
                    one_of(&["cma", "lms-dd"]),
                    "Constant-modulus, or decision-directed LMS on a declared constellation.",
                )
                .default_value("cma"),
                param("taps", int(1, 1_024), "Equaliser length.").default_value(16),
                param(
                    "samples_per_symbol",
                    int(1, 8),
                    "Fractional spacing (1: symbol-spaced).",
                )
                .default_value(2),
                param("step", float(0.0, 1.0, ""), "Adaptation step.").hot(),
                param(
                    "constellation",
                    one_of(&["bpsk", "qpsk", "8psk"]),
                    "Decision set for lms-dd.",
                ),
            ],
            false,
        ),
    ]
}
