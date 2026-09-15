//! Framing blocks (T-087): turn a bit stream into frames. Reuse hk-estimate `framing::sync`
//! and hk-demod `rds::block` syndromes (ADR-0011 §1.6).

use hk_recipe::PortType::{Bits, Frames};
use hk_recipe::{BlockDescriptor, PortSpec};

use crate::Registry;
use crate::schema::{
    ParamExt, boolean, descriptor, frame_length, hex, int, list, object, one_of, param, string,
};

/// Pinned descriptors of this group.
pub fn planned() -> Vec<BlockDescriptor> {
    let frames = |name: &str, doc: &str, input: PortSpec| {
        descriptor(
            name,
            "framing",
            doc,
            vec![input],
            vec![PortSpec::new("out", Frames)],
            vec![],
            false,
        )
    };
    vec![
        descriptor(
            "sync_search",
            "framing",
            "Finds frame boundaries in a bit stream by a sync word, or by block-code syndromes with offset words (RDS), and emits frames: fixed-length, or (sync-word) variable by length_from/terminator with frame_bits as the maximum.",
            vec![PortSpec::new("in", Bits)],
            vec![PortSpec::new("out", Frames)],
            vec![
                param(
                    "mode",
                    one_of(&["sync-word", "offset-words"]),
                    "Search method.",
                )
                .required(),
                // sync-word
                param(
                    "sync_word",
                    hex(64),
                    "sync-word: the word, first bit on air = MSB.",
                ),
                param("sync_bits", int(1, 64), "sync-word: word length, bits."),
                param("max_errors", int(0, 16), "sync-word: tolerated bit errors.")
                    .default_value(0)
                    .hot(),
                param(
                    "frame_bits",
                    int(1, 1_000_000),
                    "sync-word: frame length after the sync word, bits; with length_from or terminator, the maximum.",
                ),
                param(
                    "include_sync",
                    boolean(),
                    "sync-word: keep the sync word in the frame.",
                )
                .default_value(false),
                param(
                    "bit_order",
                    one_of(&["msb", "lsb"]),
                    "sync-word: lsb = the protocol sends 8-bit characters LSB first (ACARS); the frame body is bit-reversed per 8 bits from its first bit before packing, and length_from/terminator read the packed bits. The sync word stays in air order.",
                )
                .default_value("msb"),
                // offset-words
                param(
                    "block_bits",
                    int(2, 64),
                    "offset-words: block length incl. check bits.",
                ),
                param(
                    "check_bits",
                    int(1, 32),
                    "offset-words: check bits per block.",
                ),
                param(
                    "poly",
                    hex(33),
                    "offset-words: generator polynomial, normal form (without the x^check_bits term) or full form with it (RDS 0x5B9); the top term is implied either way.",
                ),
                param(
                    "offsets",
                    list(
                        object(vec![
                            param("name", string(8), "Offset word name.").required(),
                            param("word", hex(32), "Offset word added to the check bits.")
                                .required(),
                        ]),
                        1,
                    ),
                    "offset-words: named offset words.",
                ),
                param(
                    "sequence",
                    list(list(string(8), 1), 1),
                    "offset-words: allowed offset names per block position; a frame is one full sequence starting at position 0.",
                ),
                param(
                    "lock_blocks",
                    int(1, 64),
                    "offset-words: consecutive valid blocks to lock.",
                )
                .default_value(2),
                param(
                    "unlock_errors",
                    int(1, 1_000),
                    "offset-words: invalid blocks within unlock_window that drop lock.",
                )
                .default_value(20),
                param(
                    "unlock_window",
                    int(1, 1_000),
                    "offset-words: window for unlock_errors, blocks.",
                )
                .default_value(50),
            ]
            .into_iter()
            .chain(frame_length())
            .collect(),
            true,
        ),
        descriptor(
            "assemble",
            "framing",
            "Joins codewords into messages (POCSAG): splits each frame into word_bits words; a start word opens a message with its header bits (and slot index), continuation words append payload bits, and an idle word, the next start word, max_words or a gap/DISCONTINUITY closes it. Messages continue across contiguous frames (batches). One output frame per message: header, slot, then payload bits; check is the worst of its words, corrected_bits their sum.",
            vec![PortSpec::new("in", Frames)],
            vec![PortSpec::new("out", Frames)],
            vec![
                param("word_bits", int(1, 64), "Codeword length (POCSAG 32); a trailing partial word is ignored.")
                    .required(),
                param(
                    "start",
                    object(vec![
                        param("bit", int(0, 63), "Bit of the word, 0 = first.").required(),
                        param("value", int(0, 1), "Value marking a start word.").required(),
                    ]),
                    "Start-word test (POCSAG address codeword: bit 0 = 0).",
                )
                .required(),
                param(
                    "idle_words",
                    list(hex(64), 1),
                    "Words that close a message and carry nothing (POCSAG idle 0x7A89C197).",
                ),
                param(
                    "header",
                    object(vec![
                        param("offset_bits", int(0, 63), "First bit.").required(),
                        param("bits", int(1, 64), "Bits.").required(),
                    ]),
                    "Bits of the start word copied first (POCSAG: 1, 20 = address 18 + function 2).",
                )
                .required(),
                param(
                    "slot",
                    object(vec![
                        param("words_per_slot", int(1, 64), "Words per slot.").required(),
                        param("bits", int(1, 16), "Slot index width.").required(),
                    ]),
                    "Append the start word's slot index within its frame after the header (POCSAG: 2 words, 3 bits = the RIC's low bits).",
                ),
                param(
                    "payload",
                    object(vec![
                        param("offset_bits", int(0, 63), "First bit.").required(),
                        param("bits", int(1, 64), "Bits.").required(),
                    ]),
                    "Bits of each continuation word appended (POCSAG: 1, 20).",
                )
                .required(),
                param("max_words", int(1, 4_096), "Longest message, words.").default_value(256),
                param(
                    "span_frames",
                    boolean(),
                    "Continue a message into the next contiguous frame.",
                )
                .default_value(true),
            ],
            true,
        ),
        frames(
            "deframe",
            "Fixed or length-field variable frames from bits or frames.",
            PortSpec::any_of("in", &[Bits, Frames]),
        ),
        frames(
            "interleave",
            "Interleave frame bits.",
            PortSpec::new("in", Frames),
        ),
        frames(
            "deinterleave",
            "Deinterleave frame bits.",
            PortSpec::new("in", Frames),
        ),
    ]
}

/// Registers this group's implemented blocks (none yet).
pub fn register(_r: &mut Registry) {}
