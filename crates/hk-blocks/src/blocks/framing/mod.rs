//! Framing blocks (T-087): turn a bit stream into frames. Reuse hk-estimate `framing::sync`
//! and hk-demod `rds::block` syndromes (ADR-0011 §1.6).

use hk_recipe::PortType::{Bits, Frames};
use hk_recipe::{BlockDescriptor, PortSpec};

use crate::Registry;
use crate::schema::{ParamExt, boolean, descriptor, hex, int, list, object, one_of, param, string};

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
            "Finds frame boundaries in a bit stream by a sync word, or by block-code syndromes with offset words (RDS), and emits fixed-length frames.",
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
                    "sync-word: frame length after the sync word, bits.",
                ),
                param(
                    "include_sync",
                    boolean(),
                    "sync-word: keep the sync word in the frame.",
                )
                .default_value(false),
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
