//! NXDN Type-C CAC decode, and the frequency the air interface never carries (C23, T-345).
//!
//! T-271 left NXDN **identified but not decoded**: its 20-bit frame sync word `0xCDF59` was
//! verified by arithmetic — it reproduces the published symbol sequence exactly — and then nothing
//! was read from the air, because the CAC's error-correction and CRC layout could not be
//! corroborated. This module corroborates it, from the specification itself, and decodes it.
//!
//! It ends the way T-271's DMR module ends, for a reason that is *stronger* here than there: an
//! NXDN Type-C channel number resolves to **no frequency at all**, and the specification is what
//! says so. See *The frequency this module will not produce*.
//!
//! # What is verified, and how
//!
//! The reference is the NXDN Forum's own published air-interface specification, **NXDN TS 1-A
//! "Common Air Interface" Version 1.3 (November 2011)**, with **NXDN TS 1-C "Trunking Procedures
//! (Type-C)" Version 1.3** for the trunking half, cross-checked against an independent
//! implementation (`dsd-fme`'s NXDN decoder) wherever the two describe the same quantity. Where an
//! arithmetic identity could settle a number, that is what settles it, in T-268's and T-271's
//! discipline: *check a property the value must satisfy, not a table someone typed.*
//!
//! - **The frame sync word is 10 symbols (20 bits), `0xCDF59`** (TS 1-A Table 4.4-2). *Verified* —
//!   the hex word decodes, under the specification's own dibit map (TS 1-A Table 3.3-1: `01`→+3,
//!   `00`→+1, `10`→−1, `11`→−3), to `-3,+1,-3,+3,-3,-3,+3,+3,-1,+3`, which is the symbol sequence
//!   the same table publishes beside it. This is T-271's check, re-run here as
//!   [`tests::the_published_hex_words_and_symbol_sequences_reproduce_each_other`].
//! - **The dibit map itself**, and the Post field, *verified by two tables reproducing each other*:
//!   the Preamble is published as **HEX `5775FD`** (Table 4.4-1) and the Post field is published
//!   **only as symbols** — `+3,+3,+3` then `-3,+3,-3,+3,+3,-3,-3,-3,+3` (Table 4.4-3). Under the
//!   dibit map, `5775FD`'s twelve symbols are exactly that sequence. Two independently printed
//!   tables, in different notations, agreeing bit for bit; a transcription error survives neither.
//! - **The outbound RCCH frame is 384 bits: FSW(20) + LICH(16) + CAC(300) + E(24) + Post(24)**
//!   (TS 1-A Figure 4.4-1 and Figure 4.6-2). *Verified by the sum*, which is exactly 384 = 192
//!   symbols, and independently by `dsd-fme`, which describes the same frame as "192 dibits: 10
//!   dibits frame sync, 8 dibits LICH, 174 dibits payload" — and 300 + 24 + 24 = 348 = 174 dibits.
//!   Two descriptions of the same frame that only agree if both are right.
//! - **The CAC coding chain** (TS 1-A §4.5.1.1 and Figures 4.5-1, 4.5-10, 4.5-11, 4.5-12):
//!   SR(8) plus layer-3 message(144) plus 3 null bits is **155 information bits**; with the
//!   CRC(16) 171; with 4 zero tail bits **175**; rate-1/2 convolutional **350**; punctured
//!   **300**; interleaved 25 × 12 into the 300-bit CAC. *Verified by arithmetic at every step*,
//!   and two of those steps are checks a wrong reading cannot pass:
//!   - the puncturing matrix `[1111111; 1011101]` keeps 7 + 5 = **12 of every 14** coded bits, and
//!     350 × 12/14 = **300** exactly;
//!   - the interleaver is published as depth **25** with rows of **12**, and 300 / 12 = **25**.
//!
//!   Both are in [`tests::the_cac_coding_chain_closes_by_arithmetic_at_every_step`].
//! - **The convolutional code is K = 5, rate 1/2, `G1(D) = 1 + D³ + D⁴`, `G2(D) = 1 + D + D² + D⁴`**
//!   (TS 1-A §4.5.1.1(4)). *Verified* by a property the numbers must satisfy: written as tap
//!   patterns those are `0b10011` and `0b11101` — **(23, 35) octal**, which is the standard optimum
//!   rate-1/2 constraint-length-5 convolutional code (free distance 7). A mistyped generator does
//!   not land on the textbook pair.
//! - **The CAC CRC is 16 bits, `X¹⁶ + X¹² + X⁵ + 1`, shift register initialised to all ones**
//!   (TS 1-A §4.5.1.1(2) and §4.5.5) — i.e. CRC-16/CCITT-FALSE, the **same** CRC T-267 already
//!   uses for P25 and the same one `hk_estimate` catalogues. Unlike DMR's, this CRC's initial value
//!   is stated by the specification rather than chosen by this repo, and there is no mask.
//! - **The scrambler** (TS 1-A §4.6): a 9-bit PN generator, `X⁹ + X⁴ + 1`, register `S8..S0`
//!   initialised to `011100100` **per frame**, applied per symbol as a sign inversion over the 182
//!   symbols that follow the frame sync (LICH + CAC + E + Post = 8 + 150 + 12 + 12 = 182 — the sum
//!   again). *Corroborated*: `dsd-fme` descrambles "all 182 dibits (8 LICH + 174 payload)" with a
//!   9-bit LFSR seeded **`0xE4`**, and `0b0_1110_0100` is `0xE4`. The one place the two differ is
//!   the tap notation (`X⁹+X⁴+1` against `x⁹+x⁵+1`), which is the same sequence written for the
//!   opposite shift direction — see [`SCRAMBLER_DIRECTION`], which is this repo's choice between
//!   two consistent readings and is invisible to every claim.
//! - **The LICH is 7 control bits plus one even-parity bit over its 4 most significant bits, each
//!   of the 8 bits sent as an outer symbol** — `1`→`11`(−3), `0`→`01`(+3) (TS 1-A §4.5.3 and
//!   §5.2.1). *Verified by the sum* — 8 bits × 1 symbol = 8 symbols = 16 bits, the field's stated
//!   width — and used here exactly as the specification says a receiver uses it: "the RF channel
//!   type is also used to judge a status of frame synchronization… subject to when the parity bit
//!   of LICH does not indicate errors" (§5.3.1.1).
//! - **`VCALL_ASSGN` is message type `00 0100` and `VCALL_ASSGN_DUP` is `00 0101`** (TS 1-A §6.4.5,
//!   "List of Message Type"). *Corroborated* by `dsd-fme`, which names `0x04` VCALL_ASSGN, `0x05`
//!   VCALL_ASSGN_DUP and `0x18` SITE_INFO — three values agreeing with three rows of the table.
//! - **The assignment's field layout** (TS 1-A Figure 6.4-31, with the element widths from §6.5):
//!   message type(6) behind two spare flags(2), CC Option(8), Call Type(3) + Voice Call Option(5),
//!   Source Unit ID(16), Destination Group or Unit ID(16), **Call Timer(6) + Channel(10)**.
//!   *Verified by arithmetic*: the figure draws Call Timer and Channel as one octet each, but
//!   §6.5.32 defines Call Timer as **6 bits** and §6.5.31 defines Channel as **10 bits**, and
//!   6 + 10 = 16 fills those two octets exactly with nothing over — and that placement is
//!   independently confirmed by `dsd-fme` reading the channel number at **bits 62..71** of the
//!   message, which is precisely the last two bits of octet 7 plus all of octet 8. The mandatory
//!   part sums to 8 + 8 + 8 + 16 + 16 + 16 = **72 bits = 9 octets**.
//!
//! # What is NOT verified, and therefore decides nothing
//!
//! - **The two spare flags** `F1`/`F2` in octet 0 and the **CC Option** octet. Their field-by-field
//!   meanings are not read here: the flags are documented as spare and the CC Option's bits select
//!   *optional* elements this decoder does not parse. Both are carried verbatim into the grant row
//!   ([`NxdnAssignment::flags`], [`NxdnAssignment::cc_option`]) and **nothing is decided by them** —
//!   in particular no encryption state, for the reason below.
//! - **The `E` (collision control) and `Post` fields.** Descrambled with the rest of the frame and
//!   then ignored; the outbound collision-control field is about *inbound* access and carries
//!   nothing this milestone needs.
//! - **Everything that would turn a Channel into a frequency.** See below — and note that here
//!   "unverified" is the wrong word: the specification is clear, and what it says is that the air
//!   interface does not carry it.
//!
//! # The frequency this module will not produce
//!
//! P25 announces its band plan, so T-268 resolves a grant from the air alone. DMR Tier III does
//! not, so T-271 refuses. NXDN Type-C refuses for the same reason, and the reason is *stated* here
//! rather than merely unfound:
//!
//! > **§6.5.31 Channel.** "10-bit Channel is a value to determine the carrier frequency used for
//! > control channels or traffic channels of a TRS." `000` Null, `001` Channel No. 1, … `3FF`
//! > Channel No. 1023.
//!
//! A channel *number*, one to 1023, and the specification defines **no mapping from it to hertz**.
//! Nor is the mapping hiding in another message: §6.5's list of information elements is the
//! complete set the air interface can carry, and **not one of its forty-one entries is a
//! frequency** — the closest, `CCH_INFO` (§6.4.3.3), tells a radio about its site's control
//! channels by giving their *channel numbers* too. The number is resolved against a table
//! configured in the radio, which is why every reference decoder asks the listener for one:
//! `dsd-fme` looks a channel up in a configured channel map and, failing that, computes
//! `base + channel × step` from a **configured** base and step.
//!
//! So a decoded assignment resolves to [`NxdnResolved::NoChannelMap`] — no frequency, and the row
//! says why. The tempting move is the same one DMR offers: assume the step is the 12.5 kHz LMR
//! raster and the base is wherever the radio happens to be tuned. That produces a number for every
//! grant, and it is C23's stale-band-plan pitfall committed on purpose. The synthetic scene for
//! this task parks **real voice keyings exactly where that assumption points**, so that refusing
//! costs something and the refusal is therefore worth testing.
//!
//! # Encryption: the control channel says nothing, and that is corroborated rather than assumed
//!
//! NXDN does have an encryption indication — §6.5.27 **Cipher Type** (2 bits: `00` non-ciphered,
//! `01` scramble, `10` DES, `11` AES) with a 6-bit **Key ID** beside it. It appears in `VCALL`,
//! `VCALL_IV` and the data-call header, and §6.4.5 places every one of those on the **RTCH/RDCH** —
//! the *traffic* channel — never in the RCCH outbound column. `VCALL_ASSGN` has no Cipher Type
//! field, and that is not an argument from absence: its nine mandatory octets are accounted for to
//! the bit (72 = 8 + 8 + 8 + 16 + 16 + 16), so there is no room for one.
//!
//! This build demodulates no traffic channel, so **nothing an NXDN Type-C control channel says
//! bears on encryption at all**, and [`NxdnAssignment::encryption`] returns
//! [`hk_model::Encryption::Unknown`] unconditionally. That is structurally the same position P25's
//! ALGID (T-270) and DMR's PI header (T-271) are in, and it reaches the **same**
//! [`super::VoicePermit`] gate — there is no second gate, and `Unknown` fails closed there exactly
//! as hard as `Encrypted` does. No input to this module produces `Clear`.
//!
//! Nothing here decrypts; there is no cipher in this crate, and no Cipher Type value is read.
//!
//! # NOT a standards-compliant NXDN receiver
//!
//! The channel coding above **is** implemented, faithfully — descramble, deinterleave, depuncture,
//! Viterbi, CRC — because every part of it is specified exactly and each part is checkable. What is
//! still missing, stated so nobody mistakes this for an NXDN stack:
//!
//! 1. **Only the 9600 bps (12.5 kHz) variant is reachable.** NXDN's 4800 bps variant is 2400 Bd in
//!    6.25 kHz and this milestone's symbol path produces 4800 Bd dibits only — the same structural
//!    limit that keeps SmartNet and EDACS out (`super::support`). The framing is identical; the
//!    symbol rate is not.
//! 2. **Only the outbound CAC.** Long and Short inbound CAC, SACCH, FACCH1/2, UDCH and the voice
//!    channel are not decoded.
//! 3. **No superframe or paging-frame structure** (TS 1-C §5.1.4): frames are read independently,
//!    so a message split across a superframe's paging allocation is not reassembled.
//! 4. **Optional information elements are not parsed** — the Location ID and Temporary Unit ID an
//!    individual call may append past octet 8 are left in the message bytes and read by nothing.
//! 5. **No soft decisions.** The Viterbi decoder takes hard bits, so it corrects what a hard-decision
//!    decoder corrects and no more.

use hk_estimate::framing::crc::BitCrc;
use hk_model::{Encryption, TrunkProtocol};

// ---------------------------------------------------------------------------------------------
// Framing (verified — see the module docs)
// ---------------------------------------------------------------------------------------------

/// NXDN frame sync word, `0xCDF59`, as 10 dibits (20 bits, MSB first).
///
/// *Verified* by arithmetic: these dibits are the hex word, and under the specification's dibit map
/// they are the symbol sequence the specification prints beside it. See
/// [`tests::the_published_hex_words_and_symbol_sequences_reproduce_each_other`].
pub const NXDN_FSW_DIBITS: [u8; 10] = [3, 0, 3, 1, 3, 3, 1, 1, 2, 1];

/// The frame sync word as published, for the test that checks the dibits against it.
pub const NXDN_FSW_HEX: u32 = 0x0C_DF59;

/// The Preamble, `0x5775FD`, as 12 dibits — and, symbol for symbol, the Post field.
///
/// Kept because it is half of an arithmetic verification, not because anything correlates against
/// it: the Preamble is published as hex and the Post field as symbols, and the two are the same
/// twelve symbols under the dibit map.
pub const NXDN_PREAMBLE_DIBITS: [u8; 12] = [1, 1, 1, 3, 1, 3, 1, 1, 3, 3, 3, 1];

/// The Preamble as published.
pub const NXDN_PREAMBLE_HEX: u32 = 0x57_75FD;

/// Symbols in one outbound RCCH frame: 192 = 384 bits at 2 bits per symbol.
pub const NXDN_FRAME_DIBITS: usize = 192;

/// Symbols the scrambler covers: everything after the frame sync word.
///
/// 8 (LICH) + 150 (CAC) + 12 (E) + 12 (Post) = **182**, and 10 + 182 = 192 — the sum that has to
/// close for the frame layout to be right.
pub const NXDN_SCRAMBLED_DIBITS: usize = 182;

/// Symbols in the LICH: one per coded bit, because each LICH bit is sent as a whole outer symbol.
pub const NXDN_LICH_DIBITS: usize = 8;
/// Symbols in the CAC.
pub const NXDN_CAC_DIBITS: usize = 150;
/// Bits in the CAC, after coding.
pub const NXDN_CAC_BITS: usize = 300;
/// Symbols in the E (collision control) field. Descrambled and then read by nothing.
pub const NXDN_E_DIBITS: usize = 12;
/// Symbols in the Post field. Descrambled and then read by nothing.
pub const NXDN_POST_DIBITS: usize = 12;

/// Bits of layer-3 information a CAC carries: SR(8) + layer-3 message(144).
pub const NXDN_L3_BITS: usize = 152;
/// The same, as whole bytes — 152 / 8 = 19, which is why the decoder can hand back plain bytes.
pub const NXDN_L3_BYTES: usize = 19;
/// Octets in the layer-3 message itself (`NXDN_L3_BYTES` less the one SR octet).
pub const NXDN_MESSAGE_BYTES: usize = 18;
/// Null bits appended after the layer-3 information and covered by the CRC. Fixed at zero by the
/// specification, which makes them a structural check worth a factor of eight.
pub const NXDN_NULL_BITS: usize = 3;
/// Information bits the CRC covers: `NXDN_L3_BITS + NXDN_NULL_BITS`.
pub const NXDN_INFO_BITS: usize = NXDN_L3_BITS + NXDN_NULL_BITS;
/// Width of the CAC's CRC.
pub const NXDN_CRC_BITS: usize = 16;
/// Zero tail bits that terminate the convolutional code.
pub const NXDN_TAIL_BITS: usize = 4;
/// Bits entering the convolutional encoder: 155 + 16 + 4 = 175.
pub const NXDN_CONV_INPUT_BITS: usize = NXDN_INFO_BITS + NXDN_CRC_BITS + NXDN_TAIL_BITS;
/// Bits leaving it, before puncturing: 175 × 2.
pub const NXDN_CONV_OUTPUT_BITS: usize = NXDN_CONV_INPUT_BITS * 2;

/// Rows of the block interleaver.
pub const NXDN_INTERLEAVE_DEPTH: usize = 25;
/// Bits per interleaver row. 25 × 12 = 300, the CAC.
pub const NXDN_INTERLEAVE_WIDTH: usize = 12;

/// `G1(D) = 1 + D³ + D⁴` as a tap pattern, `D⁰` in the most significant position: `0o23`.
pub const NXDN_CONV_G1: u8 = 0b1_0011;
/// `G2(D) = 1 + D + D² + D⁴` as a tap pattern: `0o35`.
pub const NXDN_CONV_G2: u8 = 0b1_1101;

/// The scrambler's 9-bit register as the specification initialises it: `S8..S0 = 011100100`.
pub const NXDN_SCRAMBLER_SEED: u16 = 0b0_1110_0100;

/// Which way the scrambler's register shifts — **this repo's choice between two readings that
/// describe the same sequence**, in the spirit of T-271's DMR CRC initial value.
///
/// TS 1-A §4.6 gives the generator as `X⁹ + X⁴ + 1` and draws the register `S8 … S0`; `dsd-fme`
/// implements the same scrambler, with the same `0xE4` seed, as `x⁹ + x⁵ + 1`. Those are reciprocal
/// polynomials: they are the same recurrence read in opposite shift directions, and a figure that
/// shows taps without an arrow does not settle which. This module shifts **right**, taking the
/// output from `S0` and feeding `S0 ⊕ S4` back into `S8`.
///
/// The choice is invisible to every claim made here. It changes which symbols carry a given frame
/// and nothing else: no decoded field depends on it, and the false-alarm arithmetic below is
/// identical either way, because the LICH's admissible-symbol set is *closed* under sign inversion
/// (`01` ↔ `11`, `00` ↔ `10`), so even the LICH gate measures the same thing under either reading.
/// Anyone feeding this a real off-air capture must settle it against the specification's figure.
pub const SCRAMBLER_DIRECTION: &str = "shift right, output S0, feedback S0^S4 into S8";

/// NXDN's symbol rate in the 12.5 kHz (9600 bps) variant this build can reach.
pub const NXDN_SYMBOL_RATE_BD: f64 = 4800.0;

// ---------------------------------------------------------------------------------------------
// Message types (verified — TS 1-A §6.4.5, corroborated by dsd-fme for three of them)
// ---------------------------------------------------------------------------------------------

/// `VCALL_ASSGN` — a traffic channel assigned to a voice call.
pub const MSG_VCALL_ASSGN: u8 = 0x04;
/// `VCALL_ASSGN_DUP` — the periodic repeat that lets a radio join a call already in progress.
pub const MSG_VCALL_ASSGN_DUP: u8 = 0x05;
/// `DCALL_ASSGN_DUP` — the data-call equivalent of the above.
pub const MSG_DCALL_ASSGN_DUP: u8 = 0x0D;
/// `DCALL_ASSGN` — a traffic channel assigned to a data call.
pub const MSG_DCALL_ASSGN: u8 = 0x0E;
/// `SITE_INFO` — the site's own broadcast identity.
pub const MSG_SITE_INFO: u8 = 0x18;
/// `SRV_INFO` — which services the system offers.
pub const MSG_SRV_INFO: u8 = 0x19;
/// `CCH_INFO` — which **channel numbers** this site's control channels use. Named here because it
/// is Type-C evidence; read for nothing, because what it carries is channel numbers.
pub const MSG_CCH_INFO: u8 = 0x1A;
/// `ADJ_SITE_INFO` — neighbouring sites.
pub const MSG_ADJ_SITE_INFO: u8 = 0x1B;

/// The assignment messages this decoder reads as grants.
pub const NXDN_ASSIGNMENTS: [u8; 4] = [
    MSG_VCALL_ASSGN,
    MSG_VCALL_ASSGN_DUP,
    MSG_DCALL_ASSGN,
    MSG_DCALL_ASSGN_DUP,
];

/// Message types whose presence on an **outbound RCCH** is evidence the system is a Type-C trunked
/// one rather than a conventional NXDN repeater emitting the same air interface.
///
/// Deliberately the corroborated ones only, and deliberately only messages TS 1-A §6.4.5 places in
/// the RCCH-outbound column: a conventional repeater has no control channel and therefore emits
/// none of these. A message type this decoder cannot name counts as unhandled and contributes to
/// nothing.
pub const NXDN_TYPE_C: [u8; 8] = [
    MSG_VCALL_ASSGN,
    MSG_VCALL_ASSGN_DUP,
    MSG_DCALL_ASSGN,
    MSG_DCALL_ASSGN_DUP,
    MSG_SITE_INFO,
    MSG_SRV_INFO,
    MSG_CCH_INFO,
    MSG_ADJ_SITE_INFO,
];

/// Whether a message type assigns a **voice** channel — the only kind a follower would ever open a
/// voice path on. Data assignments are still grants, still traffic, and still recorded.
pub const fn is_voice_assignment(message_type: u8) -> bool {
    matches!(message_type, MSG_VCALL_ASSGN | MSG_VCALL_ASSGN_DUP)
}

/// A short name for a message type, for a row a person reads. `None` for one this decoder does not
/// name — which is a statement, not a gap to fill with a guess.
pub fn message_type_name(message_type: u8) -> Option<&'static str> {
    Some(match message_type {
        MSG_VCALL_ASSGN => "vcall-assgn",
        MSG_VCALL_ASSGN_DUP => "vcall-assgn-dup",
        MSG_DCALL_ASSGN => "dcall-assgn",
        MSG_DCALL_ASSGN_DUP => "dcall-assgn-dup",
        MSG_SITE_INFO => "site-info",
        MSG_SRV_INFO => "srv-info",
        MSG_CCH_INFO => "cch-info",
        MSG_ADJ_SITE_INFO => "adj-site-info",
        _ => return None,
    })
}

/// The Channel value the specification reserves as "Null" — a filler, never a channel.
pub const NXDN_CHANNEL_NULL: u16 = 0;
/// The largest Channel value, `0x3FF` (§6.5.31: Channel No. 1023).
pub const NXDN_CHANNEL_MAX: u16 = 1023;

// ---------------------------------------------------------------------------------------------
// A-priori thresholds.
//
// Fixed, with the arithmetic below, BEFORE the decoder was run on anything. Never edit one of these
// to make a run pass (ADR-0016 §7; the rule `confirm.rs`, `tsbk.rs` and `dmr.rs` state for their
// own floors).
// ---------------------------------------------------------------------------------------------

/// Symbol mismatches tolerated across NXDN's frame sync word: **none**.
///
/// A priori, and different from [`super::SYNC_TOLERANCE_DIBITS`] for a reason that is arithmetic
/// rather than taste. P25's and DMR's syncs are 24 symbols; NXDN's is **10**. On noise a symbol
/// matches by chance with probability 1/4, so the exact binomial chance of a correlation hit at one
/// trial position is `Σ_{k≤T} C(10,k)·3^k / 4¹⁰`: **9.5e-7 at T = 0**, 2.96e-5 at T = 1, 4.16e-4 at
/// T = 2. The shared tolerance of 2 would therefore put NXDN's sync gate **4.6e7 times weaker** than
/// the one the same constant buys on a 24-symbol pattern — at 4800 Bd that is roughly one chance
/// sync hit per second of noise per channel, where P25's is one per 40 000 years.
///
/// T = 0 restores the floor, and the frame's own structure pays for the strictness twice over: the
/// LICH check below is a second, independent 6.1e-5, so the combined frame gate (1.1e-16 per trial;
/// see [`MIN_NXDN_CACS`]) sits alongside T-267's 1.4e-16.
///
/// **Error direction.** Tightening to zero costs *robustness*, not correctness: a frame whose sync
/// carries a single symbol error is missed. A 9600 bps NXDN control channel emits 25 frames a
/// second, so at a 2 % symbol error rate 81 % of its frames still sync and a half-second window
/// still holds ten of them — where [`super::MIN_CRC_VALID`] needs two. Loosening it would admit
/// chance syncs at a rate the CRC alone cannot carry.
pub const NXDN_SYNC_TOLERANCE_DIBITS: u32 = 0;

/// CRC-valid CACs carrying a **named Type-C RCCH message** that must be seen before this build will
/// name a system `nxdn-type-c`.
///
/// A priori, in the style of [`super::MIN_DMR_CSBKS`], with the arithmetic stated so it cannot
/// drift to whatever makes a scene pass. A random 192-symbol position becomes a decoded, CRC-valid
/// CAC only by clearing four independent gates:
///
/// - the 10-symbol frame sync at tolerance 0: 4⁻¹⁰ = **9.5e-7**;
/// - the LICH: all eight of its symbols must be **outer** (2⁻⁸), and six of its eight bits are then
///   fixed by "outbound RCCH carrying a CAC" — RF channel type `00`, CAC type `00`, direction `1`,
///   and the even-parity bit those force to `0` (2⁻⁶). Together **2⁻¹⁴ = 6.1e-5**;
/// - the 16-bit CRC over the Viterbi-decoded block: 2⁻¹⁶ = **1.53e-5**;
/// - the three null bits the specification fixes at zero: 2⁻³ = **0.125**.
///
/// So one random frame position yields a CRC-valid CAC with probability
/// 9.5e-7 × 6.1e-5 × 1.53e-5 × 0.125 = **1.1e-16** — which is where T-267's 1.4e-16 per confirmable
/// P25 frame sits, and it is this number, not the short sync word, that the confirmer's two-block
/// requirement then squares.
///
/// Naming a *protocol* costs one gate more: the 6-bit message type must be one of the
/// [`NXDN_TYPE_C`] eight, 8/64 = **0.125**. Over raw random blocks that is
/// 1.53e-5 × 0.125 × 0.125 = 2.4e-7 each, and `N = 2` squares it to **5.7e-14** — before the frame
/// gate above, which no random block reaches twice in any run this device will make.
///
/// **Error direction.** Raising this delays naming a real system by one more control-channel
/// message; a Type-C control channel emits 25 frames a second, so the cost is milliseconds.
/// Lowering it toward 1 lets a single lucky block claim what the emitter *is*, which is exactly the
/// claim T-268 refused to make from one unrepeated message.
pub const MIN_NXDN_CACS: u32 = 2;

/// Most CACs decoded from one window, so the cost of a pass stays bounded a priori.
///
/// Matches [`super::MAX_TSBK_PER_WINDOW`]. At 4800 Bd a 192-symbol frame is 40 ms, so a second of
/// control channel holds 25 frames and 4096 is two orders of magnitude of headroom over any window
/// the chain collects.
pub const MAX_CAC_PER_WINDOW: usize = 4096;

// ---------------------------------------------------------------------------------------------
// The scrambler
// ---------------------------------------------------------------------------------------------

/// Scrambles or descrambles the 182 symbols that follow a frame sync — the operation is its own
/// inverse, because it is a sign inversion.
///
/// Sign inversion of a 4-level symbol is exactly "flip the dibit's most significant bit": `+3` (`01`)
/// becomes `-3` (`11`), `+1` (`00`) becomes `-1` (`10`). The register is reinitialised per frame, as
/// §4.6 requires, so a lost frame costs nothing.
pub fn descramble(dibits: &[u8]) -> Vec<u8> {
    let mut reg = NXDN_SCRAMBLER_SEED;
    dibits
        .iter()
        .map(|&d| {
            let out = reg & 1;
            let feedback = (reg ^ (reg >> 4)) & 1;
            reg = (reg >> 1) | (feedback << 8);
            if out == 1 { d ^ 0b10 } else { d }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The LICH
// ---------------------------------------------------------------------------------------------

/// The Link Information Channel: seven control bits that say what the frame is.
///
/// Constructing one asserts that the eight coded symbols were all outer symbols and that the parity
/// bit checked out — [`NxdnLich::parse`] is the only way to make one, and it returns `None`
/// otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NxdnLich {
    /// The seven control bits, MSB first in the low seven bits: RF channel type(2), functional
    /// channel type(2), option(2), direction(1).
    control: u8,
}

impl NxdnLich {
    /// Decodes the eight descrambled LICH symbols, or `None` if they are not a LICH.
    ///
    /// Two independent checks, both from the specification: every coded bit is sent as an **outer**
    /// symbol (`1`→`11`, `0`→`01`), and the last of the eight bits is **even parity over the four
    /// most significant** control bits.
    pub fn parse(dibits: &[u8]) -> Option<Self> {
        if dibits.len() < NXDN_LICH_DIBITS {
            return None;
        }
        let mut bits = 0u8;
        for &d in &dibits[..NXDN_LICH_DIBITS] {
            let bit = match d {
                0b01 => 0,
                0b11 => 1,
                // An inner symbol is not a LICH bit at all: the coding uses only +3 and -3.
                _ => return None,
            };
            bits = (bits << 1) | bit;
        }
        let control = bits >> 1;
        let parity = bits & 1;
        // Even parity over the four most significant control bits (§4.5.3(2)).
        if (control >> 3).count_ones() % 2 != u32::from(parity) {
            return None;
        }
        Some(Self { control })
    }

    /// RF channel type, 2 bits. `00` is RCCH — a control channel.
    pub const fn rf_channel_type(self) -> u8 {
        (self.control >> 5) & 0b11
    }

    /// Functional channel type, 2 bits. On an outbound RCCH, `00` is a CAC.
    pub const fn functional_channel_type(self) -> u8 {
        (self.control >> 3) & 0b11
    }

    /// Option field, 2 bits. On an outbound CAC this is the Data Flag: `00` normal, `01` idle data,
    /// `10` common data. Recorded; nothing here is decided by it.
    pub const fn option(self) -> u8 {
        (self.control >> 1) & 0b11
    }

    /// Direction. `1` is outbound — from the trunking controller to the radios.
    pub const fn is_outbound(self) -> bool {
        self.control & 1 == 1
    }

    /// Whether this frame is an **outbound RCCH carrying a CAC**: the thing a control-channel
    /// decoder is looking for, and the judgement §5.3.1.1 says a receiver makes from the RF channel
    /// type once the parity bit is good.
    pub const fn is_outbound_cac(self) -> bool {
        self.rf_channel_type() == 0b00
            && self.functional_channel_type() == 0b00
            && self.is_outbound()
    }

    /// The seven control bits, for a row that wants to record what was read.
    pub const fn control(self) -> u8 {
        self.control
    }
}

// ---------------------------------------------------------------------------------------------
// The CAC coding chain
// ---------------------------------------------------------------------------------------------

/// The CAC's CRC: CRC-16/CCITT-FALSE, `X¹⁶ + X¹² + X⁵ + 1` with the shift register initialised to
/// all ones (§4.5.1.1(2), §4.5.5).
///
/// The **same** CRC T-267 built for P25, which is why this returns the catalogued variant rather
/// than a second hand-rolled one.
pub fn cac_crc() -> BitCrc {
    BitCrc::new(16, 0x1021, 0xFFFF, false, false, 0).expect("CRC-16/CCITT-FALSE is a valid BitCrc")
}

/// Encodes 155 information bits into the 300-bit CAC: CRC, tail, convolutional code, puncture,
/// interleave.
///
/// Present because the decoder's correctness is only checkable against an encoder — the
/// single-bit-error property in the tests is what the whole chain has to satisfy, and it cannot be
/// stated without one. Not used on any run path.
pub fn encode_cac(info: &[u8]) -> Option<Vec<u8>> {
    if info.len() != NXDN_INFO_BITS {
        return None;
    }
    // ---- CRC over the 155 information bits, MSB first.
    let mut bits: Vec<u8> = info.to_vec();
    let crc = cac_crc().compute(&pack_bits(info), 0, NXDN_INFO_BITS) as u16;
    for i in (0..NXDN_CRC_BITS).rev() {
        bits.push(((crc >> i) & 1) as u8);
    }
    bits.extend(std::iter::repeat_n(0u8, NXDN_TAIL_BITS));
    debug_assert_eq!(bits.len(), NXDN_CONV_INPUT_BITS);

    // ---- Rate-1/2 convolutional code, output read G1 then G2.
    let mut coded = Vec::with_capacity(NXDN_CONV_OUTPUT_BITS);
    let mut state = 0u8;
    for &u in &bits {
        let (g1, g2) = conv_outputs(state, u);
        coded.push(g1);
        coded.push(g2);
        state = ((state << 1) | u) & 0xF;
    }

    // ---- Puncture: of every seven codeword pairs, the G2 bits of pairs 1 and 5 are erased.
    let mut punctured = Vec::with_capacity(NXDN_CAC_BITS);
    for i in 0..NXDN_CONV_INPUT_BITS {
        punctured.push(coded[2 * i]);
        if !is_punctured(i) {
            punctured.push(coded[2 * i + 1]);
        }
    }
    debug_assert_eq!(punctured.len(), NXDN_CAC_BITS);

    // ---- Interleave: write 25 rows of 12, read down the columns.
    let mut out = vec![0u8; NXDN_CAC_BITS];
    for (k, o) in out.iter_mut().enumerate() {
        *o = punctured
            [(k % NXDN_INTERLEAVE_DEPTH) * NXDN_INTERLEAVE_WIDTH + k / NXDN_INTERLEAVE_DEPTH];
    }
    Some(out)
}

/// Decodes a 300-bit CAC into its 19 bytes of layer-3 information, or `None`.
///
/// `None` means one of the structural checks failed: the three null bits the specification fixes at
/// zero, or the CRC. The Viterbi decoder itself always produces *something* — a decoder that always
/// answers is why those two checks carry the whole gate.
pub fn decode_cac(cac: &[u8]) -> Option<[u8; NXDN_L3_BYTES]> {
    if cac.len() != NXDN_CAC_BITS {
        return None;
    }
    // ---- Deinterleave, undoing the column read.
    let mut y = vec![0u8; NXDN_CAC_BITS];
    for (k, &b) in cac.iter().enumerate() {
        y[(k % NXDN_INTERLEAVE_DEPTH) * NXDN_INTERLEAVE_WIDTH + k / NXDN_INTERLEAVE_DEPTH] = b;
    }

    // ---- Depuncture: 2 marks an erased position, which costs the Viterbi decoder nothing rather
    // than costing it a wrong bit.
    const ERASURE: u8 = 2;
    let mut x = vec![ERASURE; NXDN_CONV_OUTPUT_BITS];
    let mut j = 0usize;
    for i in 0..NXDN_CONV_INPUT_BITS {
        x[2 * i] = y[j];
        j += 1;
        if !is_punctured(i) {
            x[2 * i + 1] = y[j];
            j += 1;
        }
    }
    debug_assert_eq!(j, NXDN_CAC_BITS);

    let bits = viterbi(&x);

    // ---- The three null bits are fixed at zero by §4.5.4's Figure 4.5-10.
    if bits[NXDN_L3_BITS..NXDN_L3_BITS + NXDN_NULL_BITS]
        .iter()
        .any(|&b| b != 0)
    {
        return None;
    }
    // ---- The CRC over the 155 information bits.
    let packed = pack_bits(&bits[..NXDN_INFO_BITS]);
    let want = bits[NXDN_INFO_BITS..NXDN_INFO_BITS + NXDN_CRC_BITS]
        .iter()
        .fold(0u16, |acc, &b| (acc << 1) | u16::from(b));
    if cac_crc().compute(&packed, 0, NXDN_INFO_BITS) as u16 != want {
        return None;
    }
    let mut out = [0u8; NXDN_L3_BYTES];
    out.copy_from_slice(&pack_bits(&bits[..NXDN_L3_BITS]));
    Some(out)
}

/// Whether codeword pair `i` (zero-based) has its G2 bit erased by the puncturing matrix.
///
/// The matrix `[1111111; 1011101]` erases the second row's columns 1 and 5 of every seven, which is
/// the specification's own worked example: "X4 and X12 are erased in this case".
const fn is_punctured(i: usize) -> bool {
    matches!(i % 7, 1 | 5)
}

/// The two coded bits `G1`, `G2` for input `u` from encoder state `state`.
///
/// `state` holds the previous four input bits, most recent in bit 0. `G1(D) = 1 + D³ + D⁴` and
/// `G2(D) = 1 + D + D² + D⁴`.
const fn conv_outputs(state: u8, u: u8) -> (u8, u8) {
    let g1 = u ^ ((state >> 2) & 1) ^ ((state >> 3) & 1);
    let g2 = u ^ (state & 1) ^ ((state >> 1) & 1) ^ ((state >> 3) & 1);
    (g1, g2)
}

/// Hard-decision Viterbi over the K = 5, rate-1/2 code, with erasures.
///
/// The code is **terminated**: four zero tail bits drive the encoder back to state 0, so the
/// traceback starts from state 0 rather than from the best final state. That is a real constraint
/// and not a convenience — it is four bits of the decoder's own gate.
fn viterbi(x: &[u8]) -> Vec<u8> {
    const STATES: usize = 16;
    const INF: u32 = u32::MAX / 4;
    let steps = NXDN_CONV_INPUT_BITS;
    let mut cost = [INF; STATES];
    cost[0] = 0;
    let mut back = vec![0u8; steps * STATES];
    for t in 0..steps {
        let (a, b) = (x[2 * t], x[2 * t + 1]);
        let mut next = [INF; STATES];
        for (s, &here) in cost.iter().enumerate() {
            if here >= INF {
                continue;
            }
            for u in 0u8..2 {
                let (g1, g2) = conv_outputs(s as u8, u);
                // An erasure contributes nothing: it is a bit nobody transmitted.
                let m = u32::from(a != 2 && a != g1) + u32::from(b != 2 && b != g2);
                let ns = ((s << 1) | usize::from(u)) & 0xF;
                let c = here + m;
                if c < next[ns] {
                    next[ns] = c;
                    back[t * STATES + ns] = ((s as u8) << 1) | u;
                }
            }
        }
        cost = next;
    }
    // Terminated: the path must end in state 0.
    let mut out = vec![0u8; steps];
    let mut s = 0usize;
    for t in (0..steps).rev() {
        let entry = back[t * STATES + s];
        out[t] = entry & 1;
        s = usize::from(entry >> 1);
    }
    out
}

/// Packs MSB-first bits into bytes, zero-padding the last one.
fn pack_bits(bits: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, &b) in bits.iter().enumerate() {
        out[i / 8] |= (b & 1) << (7 - i % 8);
    }
    out
}

/// Unpacks `n` MSB-first bits from `bytes`.
fn unpack_bits(bytes: &[u8], n: usize) -> Vec<u8> {
    (0..n).map(|i| (bytes[i / 8] >> (7 - i % 8)) & 1).collect()
}

/// The 2-bit dibits of `n` bits.
fn bits_to_dibits(bits: &[u8]) -> Vec<u8> {
    bits.chunks(2).map(|c| (c[0] << 1) | c[1]).collect()
}

/// The bits of a dibit run, MSB first.
fn dibits_to_bits(dibits: &[u8]) -> Vec<u8> {
    dibits.iter().flat_map(|&d| [(d >> 1) & 1, d & 1]).collect()
}

// ---------------------------------------------------------------------------------------------
// A frame
// ---------------------------------------------------------------------------------------------

/// One decoded outbound RCCH frame: what its LICH said, and the layer-3 information its CAC
/// carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NxdnFrame {
    /// The LICH, already parity-checked.
    pub lich: NxdnLich,
    /// SR(8) + the 18-octet layer-3 message.
    pub info: [u8; NXDN_L3_BYTES],
}

/// Decodes the 182 symbols that follow a frame sync into a frame, or `None`.
///
/// The gates, in the order they are cheapest: the LICH's outer symbols and parity, that the LICH
/// says **outbound RCCH carrying a CAC**, then the CAC's own null bits and CRC. See
/// [`MIN_NXDN_CACS`] for what each is worth.
pub fn decode_frame(after_sync: &[u8]) -> Option<NxdnFrame> {
    if after_sync.len() < NXDN_SCRAMBLED_DIBITS {
        return None;
    }
    let d = descramble(&after_sync[..NXDN_SCRAMBLED_DIBITS]);
    let lich = NxdnLich::parse(&d[..NXDN_LICH_DIBITS])?;
    if !lich.is_outbound_cac() {
        return None;
    }
    let cac = dibits_to_bits(&d[NXDN_LICH_DIBITS..NXDN_LICH_DIBITS + NXDN_CAC_DIBITS]);
    let info = decode_cac(&cac)?;
    Some(NxdnFrame { lich, info })
}

/// Builds the 182 symbols that follow a frame sync from a LICH's control bits and a layer-3
/// information block. The inverse of [`decode_frame`], for tests and for the generator's reference.
pub fn encode_frame(lich_control: u8, info: &[u8; NXDN_L3_BYTES]) -> Option<Vec<u8>> {
    // LICH: seven control bits plus even parity over the top four, each bit an outer symbol.
    let control = lich_control & 0x7F;
    let parity = (control >> 3).count_ones() as u8 % 2;
    let mut out: Vec<u8> = (0..7)
        .map(|i| (control >> (6 - i)) & 1)
        .chain(std::iter::once(parity))
        .map(|b| if b == 1 { 0b11 } else { 0b01 })
        .collect();

    // CAC: 152 information bits plus the three null bits, through the coding chain.
    let mut bits = unpack_bits(info, NXDN_L3_BITS);
    bits.extend(std::iter::repeat_n(0u8, NXDN_NULL_BITS));
    out.extend(bits_to_dibits(&encode_cac(&bits)?));

    // E and Post. The Post field is the published fixed pattern; the collision-control field is
    // not modelled and is sent as the same pattern, which nothing reads either way.
    out.extend_from_slice(&NXDN_PREAMBLE_DIBITS);
    out.extend_from_slice(&NXDN_PREAMBLE_DIBITS);
    debug_assert_eq!(out.len(), NXDN_SCRAMBLED_DIBITS);
    Some(descramble(&out))
}

// ---------------------------------------------------------------------------------------------
// The layer-3 message
// ---------------------------------------------------------------------------------------------

/// One decoded CAC: the SR header and the layer-3 message behind it.
///
/// Constructing one asserts only that 19 bytes parsed. The CRC was checked by whatever produced
/// them, exactly as [`super::Tsbk`] and [`super::Csbk`] inherit their CRC checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cac {
    /// Structure field, 2 bits: whether this frame heads a superframe, and single/dual message.
    /// Recorded, never acted on here — superframes are not reassembled (see the module docs).
    pub structure: u8,
    /// Radio Access Number, 6 bits. On a trunked system this is the site's colour code.
    pub ran: u8,
    /// The two spare flags in the message's octet 0. **UNVERIFIED** as to meaning (the
    /// specification calls them spare); carried verbatim and read by nothing.
    pub flags: u8,
    /// Message type, 6 bits.
    pub message_type: u8,
    /// The 18-octet layer-3 message, verbatim.
    pub message: [u8; NXDN_MESSAGE_BYTES],
}

impl Cac {
    /// Parses the 19 bytes a CAC carries.
    pub fn parse(info: &[u8]) -> Option<Self> {
        if info.len() < NXDN_L3_BYTES {
            return None;
        }
        let mut message = [0u8; NXDN_MESSAGE_BYTES];
        message.copy_from_slice(&info[1..NXDN_L3_BYTES]);
        Some(Self {
            structure: (info[0] >> 6) & 0b11,
            ran: info[0] & 0x3F,
            flags: (message[0] >> 6) & 0b11,
            message_type: message[0] & 0x3F,
            message,
        })
    }

    /// Whether this message is one of the [`NXDN_TYPE_C`] eight — the evidence [`MIN_NXDN_CACS`]
    /// counts.
    pub fn is_type_c(&self) -> bool {
        NXDN_TYPE_C.contains(&self.message_type)
    }

    /// The channel assignment this message carries, if it is one.
    ///
    /// Layout of the nine mandatory octets, which sum to exactly 72 bits: flags(2) + message
    /// type(6), CC Option(8), Call Type(3) + Voice Call Option(5), Source Unit ID(16), Destination
    /// Group or Unit ID(16), Call Timer(6) + Channel(10). See the module docs for how the last
    /// pair's placement is pinned by arithmetic rather than by a figure.
    pub fn assignment(&self) -> Option<NxdnAssignment> {
        if !NXDN_ASSIGNMENTS.contains(&self.message_type) {
            return None;
        }
        let m = &self.message;
        Some(NxdnAssignment {
            message_type: self.message_type,
            flags: self.flags,
            cc_option: m[1],
            call_type: (m[2] >> 5) & 0b111,
            call_option: m[2] & 0x1F,
            source: (u16::from(m[3]) << 8) | u16::from(m[4]),
            destination: (u16::from(m[5]) << 8) | u16::from(m[6]),
            call_timer: (m[7] >> 2) & 0x3F,
            channel: (u16::from(m[7] & 0b11) << 8) | u16::from(m[8]),
            ran: self.ran,
        })
    }
}

/// An NXDN Type-C channel assignment.
///
/// It names a **channel number**, which is the whole difficulty: see [`NxdnAssignment::resolve`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NxdnAssignment {
    /// The message type that assigned it, so a row can say which kind of assignment this was.
    pub message_type: u8,
    /// The two spare flags from octet 0. **UNVERIFIED**, carried verbatim, mapped through nothing.
    pub flags: u8,
    /// The CC Option octet. It selects optional elements this decoder does not parse, so it too is
    /// carried verbatim and read by nothing.
    pub cc_option: u8,
    /// Call Type, 3 bits: `000` broadcast (group), `001` conference, `100` individual,
    /// `110` interconnect, `111` speed dial (§6.5.12).
    pub call_type: u8,
    /// Voice (or data) Call Option, 5 bits: duplex flag and transmission mode (§6.5.13).
    pub call_option: u8,
    /// Source Unit ID, 16 bits. `0x0000` is the specification's "Null Unit ID" filler.
    pub source: u16,
    /// Destination — a Group ID for a group call, a Unit ID for an individual one (§6.4 Table 6.4-6).
    pub destination: u16,
    /// Call Timer, 6 bits: how long the radio may hold the traffic channel (§6.5.32).
    pub call_timer: u8,
    /// **Channel number**, 10 bits, 1..=1023. *Not* a frequency, and not resolvable to one here.
    pub channel: u16,
    /// The site's Radio Access Number (colour code), from the frame's SR header.
    pub ran: u8,
}

impl NxdnAssignment {
    /// Whether this assignment is for voice (as opposed to data).
    pub const fn is_voice(&self) -> bool {
        is_voice_assignment(self.message_type)
    }

    /// Whether the destination names a **group** rather than an individual radio (§6.5.12): only a
    /// broadcast or conference call's destination is a talkgroup.
    pub const fn is_group_call(&self) -> bool {
        matches!(self.call_type, 0b000 | 0b001)
    }

    /// Whether this is the periodic repeat a radio joins a call in progress with — NXDN's late
    /// entry, and structurally P25's `GRP_VCH_GRANT_UPDATE`.
    pub const fn is_late_entry(&self) -> bool {
        matches!(self.message_type, MSG_VCALL_ASSGN_DUP | MSG_DCALL_ASSGN_DUP)
    }

    /// The message type's name, when this decoder names it.
    pub fn message_name(&self) -> Option<&'static str> {
        message_type_name(self.message_type)
    }

    /// What this assignment is entitled to say about encryption: **nothing**.
    ///
    /// NXDN's encryption indication is the Cipher Type element (§6.5.27), and §6.4.5 places every
    /// message carrying one on the *traffic* channel, which nothing in this milestone demodulates.
    /// `VCALL_ASSGN` has no Cipher Type field and no room for one — its nine mandatory octets are
    /// accounted for to the bit. So every NXDN call reaches [`super::VoicePermit::open`] as
    /// [`hk_model::Encryption::Unknown`] and is refused a voice path, exactly as hard as an
    /// encrypted one, through the **same** gate a P25 or DMR call goes through.
    pub const fn encryption(&self) -> Encryption {
        Encryption::Unknown
    }

    /// The frequency this assignment's channel number resolves to: **none, and why**.
    ///
    /// Kept as a method returning a refusal rather than simply having no method, so the refusal is
    /// something a caller receives and records rather than something it has to remember to say —
    /// T-271's shape, one protocol along.
    pub const fn resolve(&self) -> NxdnResolved {
        NxdnResolved::NoChannelMap
    }
}

/// What an NXDN Type-C channel number resolved to.
///
/// One variant today, and the type exists precisely so that adding a `Mapped` variant — which would
/// need a channel map the *user* configured, since the air carries none — is a change the compiler
/// forces every caller to consider, rather than a frequency quietly appearing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NxdnResolved {
    /// The air interface carries no channel map and no base/step, and none was configured, so
    /// there is nothing to resolve the channel number through and **no frequency is produced**.
    NoChannelMap,
}

impl NxdnResolved {
    /// A short machine reason for a `grant_event` detail.
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::NoChannelMap => "no-channel-map",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Reading a window
// ---------------------------------------------------------------------------------------------

/// What one window of CRC-valid CACs contained.
#[derive(Clone, Debug, Default)]
pub struct CacScan {
    /// Blocks that parsed.
    pub blocks: usize,
    /// Blocks carrying a named Type-C RCCH message type.
    pub type_c: usize,
    /// Channel assignments seen, in order.
    pub assignments: Vec<NxdnAssignment>,
    /// Blocks whose message type this decoder does not name.
    pub unhandled: usize,
}

/// Decodes CRC-valid CACs into assignments and Type-C evidence.
///
/// Input is blocks a confirmer already validated, so nothing here re-decides whether a control
/// channel is present: [`super::CcConfirmer::confirm_framing`] remains the only source of a
/// [`super::ConfirmedCc`], and this function cannot produce one.
pub fn scan_cacs<'a>(blocks: impl IntoIterator<Item = &'a [u8; NXDN_L3_BYTES]>) -> CacScan {
    let mut out = CacScan::default();
    for block in blocks.into_iter().take(MAX_CAC_PER_WINDOW) {
        let Some(c) = Cac::parse(block) else {
            continue;
        };
        out.blocks += 1;
        if c.is_type_c() {
            out.type_c += 1;
        } else {
            out.unhandled += 1;
        }
        if let Some(a) = c.assignment() {
            out.assignments.push(a);
        }
    }
    out
}

/// The protocol a decoded window names, or `Unknown`.
///
/// Naming is gated on [`MIN_NXDN_CACS`] CRC-valid CACs carrying a named Type-C RCCH message. An
/// NXDN *frame sync* alone is deliberately not enough: a conventional NXDN repeater transmits the
/// same air interface, so the sync says "NXDN", and only RCCH-outbound trunking messages say
/// "Type-C trunked".
pub fn nxdn_protocol_of(scan: &CacScan) -> TrunkProtocol {
    if scan.type_c >= MIN_NXDN_CACS as usize {
        TrunkProtocol::NxdnTypeC
    } else {
        TrunkProtocol::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 4FSK dibit→symbol map NXDN publishes (TS 1-A Table 3.3-1) — the same one P25 and DMR use.
    fn level(dibit: u8) -> i8 {
        match dibit {
            0b01 => 3,
            0b00 => 1,
            0b10 => -1,
            0b11 => -3,
            _ => unreachable!(),
        }
    }

    fn hex_to_dibits(hex: u32, bits: u32) -> Vec<u8> {
        (0..bits / 2)
            .map(|i| ((hex >> (bits - 2 - 2 * i)) & 3) as u8)
            .collect()
    }

    /// The constants, verified the way T-271 verified DMR's: by making two independently published
    /// statements reproduce each other, rather than by transcribing one of them.
    ///
    /// The specification prints the frame sync word **both** as hex and as symbols, and prints the
    /// Preamble as hex while printing the Post field as symbols only. So there are two pairs, and
    /// both have to close under the same dibit map. A transcription error survives neither.
    #[test]
    fn the_published_hex_words_and_symbol_sequences_reproduce_each_other() {
        // Table 4.4-2: HEX CDF59, symbols -3,+1,-3,+3,-3,-3,+3,+3,-1,+3.
        let fsw = hex_to_dibits(NXDN_FSW_HEX, 20);
        assert_eq!(fsw, NXDN_FSW_DIBITS, "0xCDF59 is not the constant");
        assert_eq!(
            fsw.iter().map(|&d| level(d)).collect::<Vec<_>>(),
            vec![-3, 1, -3, 3, -3, -3, 3, 3, -1, 3],
            "0xCDF59 does not decode to the published frame sync symbols"
        );

        // Table 4.4-1: Preamble HEX 5775FD, symbols +3,+3,+3 then -3,+3,-3,+3,+3,-3,-3,-3,+3.
        // Table 4.4-3: the Post field is published ONLY as those same twelve symbols.
        let pre = hex_to_dibits(NXDN_PREAMBLE_HEX, 24);
        assert_eq!(pre, NXDN_PREAMBLE_DIBITS, "0x5775FD is not the constant");
        let post_symbols = [3, 3, 3, -3, 3, -3, 3, 3, -3, -3, -3, 3];
        assert_eq!(
            pre.iter().map(|&d| level(d)).collect::<Vec<_>>(),
            post_symbols,
            "the Preamble hex and the Post field symbol table disagree, so one of them is mistyped"
        );
        // And the Post field's documented split is 3 symbols then 9.
        assert_eq!(&post_symbols[..3], &[3, 3, 3]);
        assert_eq!(&post_symbols[3..], &[-3, 3, -3, 3, 3, -3, -3, -3, 3]);
    }

    /// The CAC coding chain, as arithmetic. Every step of the published flow has to close, and two
    /// of them are checks a wrong reading cannot pass.
    #[test]
    fn the_cac_coding_chain_closes_by_arithmetic_at_every_step() {
        // The frame: FSW(20) + LICH(16) + CAC(300) + E(24) + Post(24) = 384 bits = 192 symbols.
        assert_eq!(20 + 16 + NXDN_CAC_BITS + 24 + 24, NXDN_FRAME_DIBITS * 2);
        // Everything after the sync is scrambled, and it is 182 symbols.
        assert_eq!(
            NXDN_LICH_DIBITS + NXDN_CAC_DIBITS + NXDN_E_DIBITS + NXDN_POST_DIBITS,
            NXDN_SCRAMBLED_DIBITS
        );
        assert_eq!(
            NXDN_FSW_DIBITS.len() + NXDN_SCRAMBLED_DIBITS,
            NXDN_FRAME_DIBITS
        );

        // The layer-3 information: SR(2 + 6 = 8) + message(18 octets = 144) = 152, a whole 19 bytes.
        assert_eq!(2 + 6, 8);
        assert_eq!(8 + NXDN_MESSAGE_BYTES * 8, NXDN_L3_BITS);
        assert_eq!(NXDN_L3_BITS, NXDN_L3_BYTES * 8);
        // + 3 null = 155 information bits, + CRC(16) = 171, + tail(4) = 175.
        assert_eq!(NXDN_INFO_BITS, 155);
        assert_eq!(NXDN_INFO_BITS + NXDN_CRC_BITS, 171);
        assert_eq!(NXDN_CONV_INPUT_BITS, 175);
        // Rate 1/2 → 350.
        assert_eq!(NXDN_CONV_OUTPUT_BITS, 350);

        // THE PUNCTURE CHECK. The matrix [1111111; 1011101] keeps 7 + 5 = 12 of every 14 bits, so
        // 350 x 12/14 = 300 exactly. Counted from `is_punctured` rather than asserted, so the
        // constant and the code cannot drift apart.
        let kept = 2 * NXDN_CONV_INPUT_BITS
            - (0..NXDN_CONV_INPUT_BITS)
                .filter(|&i| is_punctured(i))
                .count();
        assert_eq!(
            kept, NXDN_CAC_BITS,
            "the puncturing matrix does not yield 300 bits"
        );
        assert_eq!(NXDN_CONV_OUTPUT_BITS * 12 / 14, NXDN_CAC_BITS);
        // The specification's own worked example: X4 and X12 are the erased ones.
        assert!(
            is_punctured(1) && is_punctured(5),
            "X4 and X12 must be the erasures"
        );
        assert_eq!((0..7).filter(|&i| is_punctured(i)).count(), 2);

        // THE INTERLEAVE CHECK. Published as depth 25 with rows of 12, and 300 / 12 = 25.
        assert_eq!(NXDN_INTERLEAVE_DEPTH * NXDN_INTERLEAVE_WIDTH, NXDN_CAC_BITS);
        assert_eq!(NXDN_CAC_BITS / NXDN_INTERLEAVE_WIDTH, NXDN_INTERLEAVE_DEPTH);

        // THE GENERATOR CHECK. G1 = 1 + D^3 + D^4 and G2 = 1 + D + D^2 + D^4 written as taps are
        // (23, 35) octal - the standard optimum rate-1/2 constraint-length-5 code. A mistyped
        // generator does not land on the textbook pair.
        assert_eq!(
            NXDN_CONV_G1, 0o23,
            "G1 is not the (23,35) octal pair's first half"
        );
        assert_eq!(
            NXDN_CONV_G2, 0o35,
            "G2 is not the (23,35) octal pair's second half"
        );
        // And the tap patterns are the polynomials: bit 4 is D^0.
        assert_eq!(NXDN_CONV_G1, (1 << 4) | (1 << 1) | 1, "1 + D^3 + D^4");
        assert_eq!(
            NXDN_CONV_G2,
            (1 << 4) | (1 << 3) | (1 << 2) | 1,
            "1 + D + D^2 + D^4"
        );

        // The scrambler's published register value is the seed an independent decoder uses.
        assert_eq!(NXDN_SCRAMBLER_SEED, 0xE4, "S8..S0 = 011100100 is 0xE4");
    }

    /// The assignment's field placement, pinned by arithmetic rather than by a figure.
    ///
    /// The figure draws Call Timer and Channel as one octet each, which cannot be right: the
    /// element definitions give them 6 and 10 bits. 6 + 10 = 16 fills exactly those two octets, and
    /// the mandatory part then sums to 72 bits — nine octets with nothing over, which is also why
    /// there is no room for a Cipher Type field.
    #[test]
    fn the_assignment_field_widths_sum_to_its_nine_mandatory_octets() {
        // flags(2) + message type(6) | CC Option(8) | Call Type(3) + Call Option(5)
        // | Source(16) | Destination(16) | Call Timer(6) + Channel(10)
        assert_eq!(2 + 6, 8);
        assert_eq!(3 + 5, 8);
        assert_eq!(6 + 10, 16, "Call Timer and Channel fill two octets exactly");
        assert_eq!(2 + 6 + 8 + 3 + 5 + 16 + 16 + 6 + 10, 72);
        assert_eq!(72 / 8, 9, "nine mandatory octets");
        // dsd-fme reads the channel number at bits 62..71 of the message, which is where a 10-bit
        // field sitting under a 6-bit Call Timer in octets 7 and 8 has to be.
        assert_eq!(7 * 8 + 6, 62);
        assert_eq!(8 * 8 + 8, 72);
        // The Channel element's range, §6.5.31.
        assert_eq!(NXDN_CHANNEL_MAX, 0x3FF);
        assert_eq!(NXDN_CHANNEL_NULL, 0);
    }

    /// The scrambler is its own inverse, which is what makes one function serve both directions.
    #[test]
    fn the_scrambler_is_its_own_inverse_and_moves_every_symbol_it_touches() {
        let src: Vec<u8> = (0..NXDN_SCRAMBLED_DIBITS).map(|i| (i % 4) as u8).collect();
        let once = descramble(&src);
        assert_eq!(
            descramble(&once),
            src,
            "scrambling twice is not the identity"
        );
        assert_ne!(once, src, "the scrambler changed nothing at all");
        // Sign inversion only ever flips a dibit's MSB, so the inner/outer distinction survives it —
        // which is why the LICH gate is invariant to the shift-direction choice.
        for (a, b) in src.iter().zip(&once) {
            assert_eq!(a & 1, b & 1, "the scrambler altered a dibit's low bit");
            assert_eq!(
                level(*a).abs(),
                level(*b).abs(),
                "a symbol changed magnitude"
            );
        }
    }

    /// A LICH round trip, and the two independent checks that make one unconstructible from noise.
    #[test]
    fn a_lich_needs_outer_symbols_and_a_parity_bit_that_checks() {
        // Outbound RCCH carrying a CAC, normal data: 00 00 00 1.
        let control = 0b000_0001u8;
        let frame = encode_frame(control, &[0u8; NXDN_L3_BYTES]).expect("a frame");
        let d = descramble(&frame);
        let lich = NxdnLich::parse(&d[..NXDN_LICH_DIBITS]).expect("a LICH");
        assert_eq!(lich.control(), control);
        assert_eq!(lich.rf_channel_type(), 0b00);
        assert_eq!(lich.functional_channel_type(), 0b00);
        assert!(lich.is_outbound());
        assert!(lich.is_outbound_cac());

        // An inner symbol is not a LICH bit. The coding uses +3 and -3 only.
        let mut inner = d[..NXDN_LICH_DIBITS].to_vec();
        inner[3] = 0b00;
        assert!(
            NxdnLich::parse(&inner).is_none(),
            "an inner symbol parsed as a LICH bit"
        );

        // A flipped control bit breaks the parity it is covered by.
        let mut flipped = d[..NXDN_LICH_DIBITS].to_vec();
        flipped[0] ^= 0b10;
        assert!(
            NxdnLich::parse(&flipped).is_none(),
            "the parity bit checked nothing"
        );

        // An inbound frame is not what a control-channel decoder is looking for.
        let inbound = NxdnLich::parse(
            &descramble(&encode_frame(0b000_0000, &[0u8; NXDN_L3_BYTES]).unwrap())
                [..NXDN_LICH_DIBITS],
        )
        .expect("a valid inbound LICH");
        assert!(!inbound.is_outbound());
        assert!(!inbound.is_outbound_cac());
    }

    /// An assignment as it comes off the air: nineteen bytes through the whole coding chain.
    fn assignment_info(
        message_type: u8,
        call_type: u8,
        source: u16,
        destination: u16,
        channel: u16,
        ran: u8,
    ) -> [u8; NXDN_L3_BYTES] {
        let mut info = [0u8; NXDN_L3_BYTES];
        info[0] = ran & 0x3F;
        info[1] = message_type & 0x3F;
        info[2] = 0x00; // CC Option: no optional elements.
        info[3] = ((call_type & 0b111) << 5) | 0b00010; // 9600 bps EHR, half duplex.
        info[4..6].copy_from_slice(&source.to_be_bytes());
        info[6..8].copy_from_slice(&destination.to_be_bytes());
        // Call Timer(6) in the top six bits of octet 7, Channel(10) straddling octets 7 and 8.
        info[8] = (0b000010 << 2) | ((channel >> 8) & 0b11) as u8;
        info[9] = (channel & 0xFF) as u8;
        info
    }

    /// The whole chain, end to end: encode a real assignment, decode it back, read its fields.
    #[test]
    fn an_assignment_survives_the_coding_chain_and_decodes_to_what_was_encoded() {
        let info = assignment_info(MSG_VCALL_ASSGN, 0b000, 1357, 2468, 0x2A5, 0x1B);
        let frame = encode_frame(0b000_0001, &info).expect("a frame");
        let got = decode_frame(&frame).expect("a decodable frame");
        assert_eq!(got.info, info, "the coding chain did not round-trip");

        let cac = Cac::parse(&got.info).expect("nineteen bytes");
        assert_eq!(cac.ran, 0x1B);
        assert_eq!(cac.message_type, MSG_VCALL_ASSGN);
        assert!(cac.is_type_c());
        let a = cac.assignment().expect("an assignment");
        assert_eq!(a.channel, 0x2A5);
        assert_eq!(a.source, 1357);
        assert_eq!(a.destination, 2468);
        assert_eq!(a.call_type, 0b000);
        assert_eq!(a.call_timer, 0b000010);
        assert_eq!(a.ran, 0x1B);
        assert!(a.is_voice());
        assert!(a.is_group_call());
        assert!(!a.is_late_entry());
        assert_eq!(a.message_name(), Some("vcall-assgn"));

        // A data assignment is still a grant, still traffic, and is not voice.
        let d = Cac::parse(
            &decode_frame(
                &encode_frame(
                    0b000_0001,
                    &assignment_info(MSG_DCALL_ASSGN, 0b100, 1, 2, 7, 0),
                )
                .unwrap(),
            )
            .unwrap()
            .info,
        )
        .unwrap()
        .assignment()
        .expect("a data assignment");
        assert!(!d.is_voice());
        assert!(
            !d.is_group_call(),
            "an individual call's destination is a radio"
        );
        assert_eq!(d.message_name(), Some("dcall-assgn"));

        // The duplicate is NXDN's late entry.
        let dup = Cac::parse(
            &decode_frame(
                &encode_frame(
                    0b000_0001,
                    &assignment_info(MSG_VCALL_ASSGN_DUP, 0b000, 0, 2468, 5, 0),
                )
                .unwrap(),
            )
            .unwrap()
            .info,
        )
        .unwrap()
        .assignment()
        .expect("a duplicate assignment");
        assert!(dup.is_late_entry());
        assert_eq!(dup.source, 0, "the specification's Null Unit ID filler");
    }

    /// **The property that corroborates the coding chain rather than merely exercising it.**
    ///
    /// A rate-1/2, constraint-length-5 convolutional code with free distance 7 corrects any single
    /// bit error — but only if the deinterleaver, the depuncturer and the Viterbi decoder all agree
    /// with the encoder about *where every bit went*. Transpose the interleaver, erase the wrong
    /// puncture positions, or mistype a generator, and a single flipped bit in the 300-bit CAC
    /// lands somewhere else entirely and the CRC fails. So this walks all 300 positions.
    #[test]
    fn every_single_bit_error_in_the_cac_is_corrected() {
        let info = assignment_info(
            MSG_VCALL_ASSGN,
            0b000,
            40_000,
            65_535,
            NXDN_CHANNEL_MAX,
            0x2F,
        );
        let mut bits = unpack_bits(&info, NXDN_L3_BITS);
        bits.extend(std::iter::repeat_n(0u8, NXDN_NULL_BITS));
        let cac = encode_cac(&bits).expect("155 information bits");
        assert_eq!(cac.len(), NXDN_CAC_BITS);
        assert_eq!(
            decode_cac(&cac),
            Some(info),
            "the clean block did not decode"
        );

        let mut corrected = 0usize;
        for i in 0..NXDN_CAC_BITS {
            let mut damaged = cac.clone();
            damaged[i] ^= 1;
            assert_eq!(
                decode_cac(&damaged),
                Some(info),
                "a single bit error at CAC bit {i} was not corrected, which means the \
                 deinterleave, the depuncture or the generators do not match the encoder"
            );
            corrected += 1;
        }
        assert_eq!(corrected, NXDN_CAC_BITS);
    }

    /// Damage the decoder must **refuse** rather than guess through. A Viterbi decoder always
    /// produces an answer, so the CRC is what stands between an answer and a claim.
    #[test]
    fn a_heavily_damaged_cac_is_refused_rather_than_decoded_to_something_plausible() {
        let info = assignment_info(MSG_VCALL_ASSGN, 0b000, 1, 2, 3, 0);
        let mut bits = unpack_bits(&info, NXDN_L3_BITS);
        bits.extend(std::iter::repeat_n(0u8, NXDN_NULL_BITS));
        let cac = encode_cac(&bits).unwrap();

        // Every other bit flipped: 150 errors, far past anything the code can carry. (The code is
        // stronger than one might guess — twenty errors spread every fifteen bits are still
        // *corrected*, which the single-bit walk above only hints at — so the damage here has to be
        // wholesale for the test to be about the CRC rather than about the Viterbi decoder.)
        let mut damaged = cac.clone();
        for i in (0..NXDN_CAC_BITS).step_by(2) {
            damaged[i] ^= 1;
        }
        assert_eq!(
            decode_cac(&damaged),
            None,
            "a block with 150 bit errors decoded to something the CRC accepted"
        );
        // And a wrong-length block is refused rather than panicking.
        assert_eq!(decode_cac(&cac[..299]), None);
        assert_eq!(encode_cac(&bits[..154]), None);
    }

    /// The naming gate, and the negative control that matters: random blocks must never name a
    /// protocol. The NXDN twin of T-268's and T-271's random-block tests.
    #[test]
    fn random_blocks_name_no_protocol() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let (mut valid, mut type_c, mut assignments) = (0usize, 0usize, 0usize);
        // Ten thousand blocks, not a million: every one costs a full 175-step Viterbi, and the
        // expected number that clear the null-bit and CRC gates is 10 000 x 1.53e-5 x 0.125 = 0.02,
        // so a larger run would measure the same zero more slowly.
        for _ in 0..10_000 {
            let cac: Vec<u8> = (0..NXDN_CAC_BITS).map(|_| (next() & 1) as u8).collect();
            let Some(info) = decode_cac(&cac) else {
                continue;
            };
            valid += 1;
            let scan = scan_cacs(std::iter::once(&info));
            type_c += scan.type_c;
            assignments += scan.assignments.len();
        }
        eprintln!(
            "[T-345] 10,000 random 300-bit CACs: {valid} decoded (null bits + CRC), \
             {type_c} Type-C-shaped, {assignments} assignment-shaped"
        );

        // A single stray block must not be enough, which is what MIN_NXDN_CACS is for.
        let one = CacScan {
            type_c: 1,
            ..CacScan::default()
        };
        assert_eq!(
            nxdn_protocol_of(&one),
            TrunkProtocol::Unknown,
            "one Type-C-shaped block named a protocol"
        );
        let two = CacScan {
            type_c: MIN_NXDN_CACS as usize,
            ..CacScan::default()
        };
        assert_eq!(nxdn_protocol_of(&two), TrunkProtocol::NxdnTypeC);
    }

    /// The assertion this module's whole design turns on: an NXDN assignment produces **no
    /// frequency**, and says why, rather than being resolved through a step nobody announced.
    #[test]
    fn an_assignment_never_produces_a_frequency_and_says_why() {
        for channel in [1u16, 6, 100, NXDN_CHANNEL_MAX] {
            let a = Cac::parse(
                &decode_frame(
                    &encode_frame(
                        0b000_0001,
                        &assignment_info(MSG_VCALL_ASSGN, 0b000, 11, 22, channel, 0),
                    )
                    .unwrap(),
                )
                .unwrap()
                .info,
            )
            .unwrap()
            .assignment()
            .unwrap();
            // The tempting wrong answer, computed here only so the test can name what must NOT
            // appear: a 12.5 kHz step off wherever the radio happens to be tuned.
            let plausible_but_unsupported_hz = 851.0125e6 + 12_500.0 * f64::from(channel);
            assert_eq!(
                a.resolve(),
                NxdnResolved::NoChannelMap,
                "channel {channel} resolved to a frequency; the nearest plausible guess would have \
                 been {plausible_but_unsupported_hz} Hz, which the air interface does not support — \
                 it carries a channel NUMBER and defines no mapping from one to hertz"
            );
            assert_eq!(a.resolve().reason(), "no-channel-map");
            // The assignment itself is not in doubt: everything that WAS said is decoded.
            assert_eq!(a.channel, channel);
            assert_eq!(a.source, 11);
            assert_eq!(a.destination, 22);
        }
    }

    /// Nothing an NXDN Type-C control channel carries claims an encryption state, and no
    /// combination of the octets it *does* carry reaches `clear`.
    ///
    /// Exhaustive over the two fields that could plausibly be misread as one — the CC Option octet
    /// and the Call Type / Call Option octet — which is 2¹⁶ combinations.
    #[test]
    fn no_field_of_an_assignment_ever_claims_an_encryption_state() {
        let mut checked = 0u32;
        for cc_option in 0..=u16::from(u8::MAX) {
            for call in 0..=u16::from(u8::MAX) {
                let a = NxdnAssignment {
                    message_type: MSG_VCALL_ASSGN,
                    flags: (call >> 6) as u8 & 0b11,
                    cc_option: cc_option as u8,
                    call_type: (call >> 5) as u8 & 0b111,
                    call_option: call as u8 & 0x1F,
                    source: 1,
                    destination: 2,
                    call_timer: 0,
                    channel: 3,
                    ran: 0,
                };
                assert_eq!(
                    a.encryption(),
                    Encryption::Unknown,
                    "cc_option {cc_option:#04x} / call {call:#04x} claimed an encryption state \
                     from a control-channel message that carries no Cipher Type field"
                );
                assert!(!a.encryption().is_clear());
                checked += 1;
            }
        }
        assert_eq!(checked, 1 << 16);
        // And the refusal is what T-270's gate sees — the same gate, not an NXDN-specific one.
        let a = NxdnAssignment {
            message_type: MSG_VCALL_ASSGN,
            flags: 0,
            cc_option: 0,
            call_type: 0,
            call_option: 0,
            source: 1,
            destination: 2,
            call_timer: 0,
            channel: 3,
            ran: 0,
        };
        assert!(super::super::VoicePermit::open(a.encryption()).is_err());
    }

    #[test]
    fn a_window_separates_assignments_from_the_messages_this_decoder_does_not_name() {
        let block = |mt: u8, ch: u16| {
            decode_frame(
                &encode_frame(0b000_0001, &assignment_info(mt, 0b000, 1, 2, ch, 0)).unwrap(),
            )
            .unwrap()
            .info
        };
        let blocks = [
            block(MSG_SITE_INFO, 0),
            block(MSG_VCALL_ASSGN, 7),
            block(0x07, 0), // a message type this decoder does not name
        ];
        let scan = scan_cacs(blocks.iter());
        assert_eq!(scan.blocks, 3);
        assert_eq!(scan.type_c, 2, "site-info and the assignment");
        assert_eq!(scan.assignments.len(), 1);
        assert_eq!(scan.unhandled, 1);
        assert_eq!(nxdn_protocol_of(&scan), TrunkProtocol::NxdnTypeC);
    }

    /// **A frame produced by the other implementation.**
    ///
    /// Every other test here rides the same scrambler, the same interleaver and the same generators
    /// twice, so a consistent misreading of the specification would cancel out and pass. This vector
    /// came out of `py/hkpy/synth/trunking.py`, which was written from the same tables but not from
    /// this code, and it is what a fixture actually puts on the air — so a divergence between the
    /// two shows up here rather than as an acceptance run that mysteriously decodes nothing.
    #[test]
    fn a_frame_from_the_synthetic_generator_decodes_to_the_assignment_it_encoded() {
        // hkpy.synth.trunking: VCALL_ASSGN, broadcast call, source 1357, destination 2468,
        // channel 6, RAN 0x1B, on an outbound RCCH carrying a CAC.
        #[rustfmt::skip]
        const FRAME: [u8; NXDN_FRAME_DIBITS] = [
            3, 0, 3, 1, 3, 3, 1, 1, 2, 1, 1, 1, 3, 1, 1, 3, 1, 3, 1, 0, 2, 3, 2, 1, 2, 0, 2, 2,
            0, 0, 2, 3, 2, 3, 1, 1, 0, 2, 2, 0, 2, 0, 3, 3, 2, 0, 1, 3, 1, 0, 0, 2, 0, 0, 3, 2,
            3, 3, 2, 2, 3, 0, 2, 0, 2, 0, 3, 1, 1, 2, 3, 3, 3, 0, 2, 2, 2, 2, 0, 2, 3, 1, 0, 2,
            2, 0, 0, 0, 0, 2, 0, 2, 0, 3, 0, 1, 1, 2, 3, 2, 0, 0, 2, 0, 1, 1, 0, 0, 0, 1, 2, 0,
            0, 2, 2, 0, 0, 0, 3, 0, 2, 1, 0, 1, 0, 0, 2, 2, 0, 0, 0, 2, 3, 2, 0, 0, 2, 2, 0, 0,
            0, 2, 2, 1, 2, 2, 0, 0, 0, 2, 2, 2, 2, 2, 0, 0, 2, 3, 2, 2, 2, 2, 0, 2, 2, 0, 0, 3,
            3, 0, 2, 1, 0, 3, 2, 2, 0, 3, 1, 3, 3, 3, 1, 3, 3, 1, 3, 3, 3, 3, 3, 3,
        ];
        // It begins with the frame sync word, which is the first thing a scan looks for.
        assert_eq!(&FRAME[..NXDN_FSW_DIBITS.len()], &NXDN_FSW_DIBITS);

        let frame = decode_frame(&FRAME[NXDN_FSW_DIBITS.len()..]).unwrap_or_else(|| {
            panic!(
                "a frame from the synthetic generator did not decode: the two implementations \
                 disagree about the scrambler, the LICH, the coding chain or the CRC"
            )
        });
        assert!(frame.lich.is_outbound_cac());
        let cac = Cac::parse(&frame.info).expect("nineteen bytes");
        assert_eq!(cac.ran, 0x1B);
        assert_eq!(cac.message_type, MSG_VCALL_ASSGN);
        let a = cac.assignment().expect("an assignment");
        assert_eq!(a.channel, 6);
        assert_eq!(a.source, 1357);
        assert_eq!(a.destination, 2468);
        assert_eq!(a.call_type, 0b000);
        assert!(a.is_group_call() && a.is_voice());
        // And it still resolves to nothing, which is the whole point of the fixture it comes from.
        assert_eq!(a.resolve(), NxdnResolved::NoChannelMap);
    }

    /// A short run is no run, rather than a panic.
    #[test]
    fn a_short_frame_yields_nothing_rather_than_a_panic() {
        assert!(decode_frame(&[]).is_none());
        assert!(decode_frame(&[1u8; NXDN_SCRAMBLED_DIBITS - 1]).is_none());
        assert!(NxdnLich::parse(&[1, 1, 1]).is_none());
        assert!(Cac::parse(&[0u8; NXDN_L3_BYTES - 1]).is_none());
    }
}
