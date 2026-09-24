//! ADR-0011 §9 (T-606) catalogue rows for group `fec`: `viterbi`, `viterbi_frames`,
//! `reed_solomon`, pinned and implemented: T-610 both Viterbi shapes (`viterbi.rs` over
//! `trellis.rs`), T-611 `reed_solomon` (`reed_solomon.rs` over `rs.rs`).
//!
//! `fec` has **two shapes** from here on (ADR-0011 §9.3): the hard-decision per-frame codes
//! (`frames → frames`: `crc`, `bch`, `parity`, `checksum`, `reed_solomon`, `viterbi_frames`) and
//! the streaming soft-decision decoder (`soft|bits → bits`: `viterbi`).

use hk_recipe::PortType::{Bits, Frames, Soft};
use hk_recipe::{BlockDescriptor, ParamSchema, PortSpec};

use crate::schema::{ParamExt, boolean, descriptor, hex, int, list, object, one_of, param, string};

/// The convolutional code, shared by both Viterbi shapes (one trellis engine, T-610).
fn code() -> Vec<ParamSchema> {
    vec![
        param("constraint_length", int(2, 16), "K (polys form)."),
        param(
            "polys",
            list(hex(16), 2),
            "Binary feedforward code: generator polynomials, one per coded bit (rate 1/n), in \
             transmission order. CCSDS 131.0-B: [\"0x4F\", \"0x6D\"] newest-lsb (= octal 171, \
             133 newest-msb) with invert [false, true].",
        ),
        param(
            "poly_order",
            one_of(&["newest-lsb", "newest-msb"]),
            "Which polynomial bit taps the input bit just shifted in: bit 0 (libfec, GNU \
             Radio) or bit K-1 (the textbook octal form).",
        )
        .default_value("newest-lsb"),
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
            list(string(64), 1),
            "Per generator: a string of 0/1 over one puncturing period, character t = whether \
             that coded bit of step t is sent (CCSDS rate 3/4: [\"101\", \"110\"]); absent: \
             unpunctured.",
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
            "Decision depth: every decided bit has at least this many trellis bits after it \
             (≥ 5 K is the textbook floor; punctured codes want more).",
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
            "Streaming convolutional (Viterbi) decoder: soft decision on `soft` input, hard \
             decision on `bits` (reported as status hard_decision = 1, ~2 dB worse).",
            vec![PortSpec::any_of("in", &[Soft, Bits])],
            vec![PortSpec::new("out", Bits)],
            viterbi,
            true,
        ),
        descriptor(
            "viterbi_frames",
            "fec",
            "Per-frame convolutional decoder (hard decision): one code block per frame.",
            vec![PortSpec::new("in", Frames)],
            vec![PortSpec::new("out", Frames)],
            per_frame,
            true,
        ),
        descriptor(
            "reed_solomon",
            "fec",
            "Reed–Solomon decoding per code block of the frame (errors only, up to (n − k)/2 \
             symbols per codeword), interleaved codewords and the CCSDS dual basis included; \
             sets the frame's check status (valid only if every codeword decodes) and adds the \
             channel bits it changed to corrected_bits.",
            vec![PortSpec::new("in", Frames)],
            vec![PortSpec::new("out", Frames)],
            vec![
                param(
                    "n",
                    int(3, 65_535),
                    "Codeword symbols; below 2^symbol_bits − 1 the code is shortened (leading \
                     data symbols fixed at zero and not sent: DVB RS(204,188), P25 RS(24,12)).",
                )
                .required(),
                param(
                    "k",
                    int(1, 65_534),
                    "Data symbols (first on air; then n − k check symbols).",
                )
                .required(),
                param(
                    "symbol_bits",
                    int(2, 16),
                    "m: bits per symbol, MSB first (8; P25 hexbits 6).",
                )
                .default_value(8),
                param(
                    "poly",
                    hex(17),
                    "Primitive field polynomial of degree symbol_bits, full form (CCSDS 0x187, \
                     DVB 0x11D, P25 0x43) or without the top term.",
                )
                .required(),
                param(
                    "fcr",
                    int(0, 65_535),
                    "First consecutive root: g(x) = Π (x − α^(prim·(fcr+i))), i < n − k \
                     (CCSDS 112, DVB 0, P25 1).",
                )
                .required(),
                param(
                    "prim",
                    int(1, 65_535),
                    "Primitive element exponent of the roots, coprime with 2^symbol_bits − 1 \
                     (CCSDS 11).",
                )
                .default_value(1),
                param(
                    "dual_basis",
                    boolean(),
                    "Symbols are in Berlekamp's dual basis on air (CCSDS 131.0-B; field 0x187 \
                     only). Wrong here, every codeword fails.",
                )
                .default_value(false),
                param(
                    "depth",
                    int(1, 255),
                    "Interleaved codewords per code block (CCSDS I = 1–5 or 8; DAB+ s, the \
                     subchannel's bitrate / 8): on-air symbol q belongs to codeword q mod depth.",
                )
                .default_value(1),
                param(
                    "strip",
                    boolean(),
                    "Remove each code block's depth × (n − k) check symbols.",
                )
                .default_value(true),
                param(
                    "span",
                    object(vec![
                        param("start_bit", int(0, 1_000_000), "First coded bit.").default_value(0),
                        param("end_trim_bits", int(0, 1_000_000), "Uncoded trailing bits.")
                            .default_value(0),
                    ]),
                    "The coded part of the frame: consecutive code blocks from start_bit; a \
                     trailing partial block passes through.",
                ),
                param(
                    "drop_invalid",
                    boolean(),
                    "Drop frames that fail the check.",
                )
                .default_value(false)
                .hot(),
            ],
            true,
        ),
    ]
}
