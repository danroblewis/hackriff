//! ADR-0011 §9 (T-606) catalogue rows for group `fec`: `viterbi`, `viterbi_frames`,
//! `reed_solomon`. **Ports are pinned; parameters are placeholders** (`params_pinned: false`)
//! that T-610 (`viterbi`, `viterbi_frames`) and T-611 (`reed_solomon`) pin. None is implemented
//! here.
//!
//! `fec` has **two shapes** from here on (ADR-0011 §9.3): the hard-decision per-frame codes
//! (`frames → frames`: `crc`, `bch`, `parity`, `checksum`, `reed_solomon`, `viterbi_frames`) and
//! the streaming soft-decision decoder (`soft|bits → bits`: `viterbi`).

use hk_recipe::PortType::{Bits, Frames, Soft};
use hk_recipe::{BlockDescriptor, ParamSchema, PortSpec};

use crate::schema::{ParamExt, boolean, descriptor, hex, int, list, object, one_of, param};

/// The convolutional code, shared by both Viterbi shapes (one trellis engine, T-610).
fn code() -> Vec<ParamSchema> {
    vec![
        param("constraint_length", int(2, 16), "K (polys form)."),
        param(
            "polys",
            list(hex(16), 2),
            "Binary feedforward code: generator polynomials, one per coded bit (rate 1/n).",
        ),
        param(
            "trellis",
            object(vec![
                param("input_bits", int(1, 4), "Bits per trellis step in.").required(),
                param("output_bits", int(2, 16), "Coded bits per step out.").required(),
                param(
                    "next_state",
                    list(int(0, 65_535), 2),
                    "Row-major [state][input] → state.",
                )
                .required(),
                param(
                    "output",
                    list(int(0, 65_535), 2),
                    "Row-major [state][input] → coded word.",
                )
                .required(),
            ]),
            "Explicit finite-state code instead of polys (P25 1/2- and 3/4-rate trellis).",
        ),
        param(
            "invert",
            list(boolean(), 2),
            "Per generator: output inverted (CCSDS inverts G2).",
        ),
        param(
            "puncture",
            list(hex(64), 1),
            "Per generator: keep-mask over one puncturing period; absent: unpunctured.",
        ),
    ]
}

/// The group-`fec` rows ADR-0011 §9.1 adds.
pub fn planned() -> Vec<BlockDescriptor> {
    let mut viterbi = code();
    viterbi.extend([
        param(
            "traceback_bits",
            int(8, 4_096),
            "Decision depth (≥ 5 K is the textbook floor).",
        )
        .default_value(64),
        param(
            "align",
            one_of(&["auto", "fixed"]),
            "Branch (n-tuple) and puncturing phase: searched by path-metric growth, or from \
             the first item.",
        )
        .default_value("auto"),
    ]);
    let mut per_frame = code();
    per_frame.extend([
        param(
            "termination",
            one_of(&["terminated", "tail-biting", "truncated"]),
            "How each frame's trellis starts and ends.",
        )
        .required(),
        param(
            "span",
            object(vec![
                param("start_bit", int(0, 1_000_000), "First coded bit.").default_value(0),
                param("end_trim_bits", int(0, 1_000_000), "Uncoded trailing bits.")
                    .default_value(0),
            ]),
            "The coded part of the frame.",
        ),
    ]);
    vec![
        descriptor(
            "viterbi",
            "fec",
            "Streaming convolutional (Viterbi) decoder, soft-decision on `soft` input.",
            vec![PortSpec::any_of("in", &[Soft, Bits])],
            vec![PortSpec::new("out", Bits)],
            viterbi,
            false,
        ),
        descriptor(
            "viterbi_frames",
            "fec",
            "Per-frame convolutional decoder (hard decision): one code block per frame.",
            vec![PortSpec::new("in", Frames)],
            vec![PortSpec::new("out", Frames)],
            per_frame,
            false,
        ),
        descriptor(
            "reed_solomon",
            "fec",
            "Reed–Solomon decoder over a frame, interleaved codewords and dual basis included.",
            vec![PortSpec::new("in", Frames)],
            vec![PortSpec::new("out", Frames)],
            vec![
                param(
                    "n",
                    int(3, 65_535),
                    "Codeword symbols (shortened when < 2^m - 1).",
                )
                .required(),
                param("k", int(1, 65_535), "Data symbols.").required(),
                param("symbol_bits", int(2, 16), "m.").default_value(8),
                param("poly", hex(17), "Field generator polynomial.").required(),
                param("fcr", int(0, 65_535), "First consecutive root.").required(),
                param("prim", int(1, 65_535), "Primitive element exponent.").default_value(1),
                param(
                    "dual_basis",
                    boolean(),
                    "Berlekamp (dual) basis symbols, the CCSDS convention.",
                )
                .default_value(false),
                param("depth", int(1, 16), "Interleaved codewords (CCSDS I).").default_value(1),
                param("strip", boolean(), "Remove the check symbols.").default_value(true),
                param(
                    "drop_invalid",
                    boolean(),
                    "Drop frames that fail the check.",
                )
                .default_value(false)
                .hot(),
            ],
            false,
        ),
    ]
}
