"""C4FM/4FSK trunking control-channel framing and modulation (T-267, C23).

A land-mobile-radio control channel transmits a **continuous** outbound data stream at 100 %
duty cycle on the 12.5 kHz LMR raster (docs/04 §8.1). That description alone does not identify
one: any continuous data emitter parked on the raster looks identical to an occupancy measure.
What separates a control channel from a continuous data emitter is *framing* — a known frame
sync pattern and payloads whose CRC checks out (docs/04 §8.4, C23 "Pitfalls").

This module therefore builds both, so a fixture can carry a real control channel **and** a
decoy that is continuous, on-raster and 4FSK but carries no recoverable framing.

**Scope.** Real P25 Phase 1 protects the TSBK with a rate-1/2 trellis code and interleaves it;
that coding is deliberately **not** implemented here. T-267 confirms *that* a control channel
exists (sync + CRC); T-268 decodes TSBKs and names the protocol. The frame below is the honest
minimum for that split: a real 48-bit P25 frame sync followed by a 12-byte block whose last two
bytes are a real CRC-16/CCITT-FALSE over the first ten. Nothing here should be read as a
standards-compliant P25 encoder.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth.fsk import crc16_ccitt_false

#: P25 Phase 1 frame sync, 48 bits (docs/04 §7.3). MSB first.
P25_FRAME_SYNC_HEX = "5575f5ff77ff"
P25_FRAME_SYNC_BITS = 48
#: Dibits per frame sync (48 bits / 2 bits per C4FM symbol).
P25_FRAME_SYNC_DIBITS = P25_FRAME_SYNC_BITS // 2

#: P25 Phase 1 C4FM: 4800 symbols/s = 9600 bit/s (docs/04 §7.2).
C4FM_SYMBOL_RATE_BD = 4800.0
#: C4FM dibit -> frequency deviation, Hz (docs/04 §7.2: peaks at +/-600 and +/-1800 Hz).
C4FM_DEVIATIONS_HZ = {0b01: 1800.0, 0b00: 600.0, 0b10: -600.0, 0b11: -1800.0}

#: TSBK-shaped block: 10 data bytes + 2 CRC bytes.
TSBK_DATA_BYTES = 10
TSBK_CRC_BYTES = 2
TSBK_BYTES = TSBK_DATA_BYTES + TSBK_CRC_BYTES
#: Dibits in one frame: sync + block.
FRAME_DIBITS = P25_FRAME_SYNC_DIBITS + TSBK_BYTES * 4

#: The 12.5 kHz narrowband LMR raster (docs/04 §4: narrowbanding mandated 2013).
LMR_RASTER_HZ = 12_500.0

CC_FRAME_SPEC: dict[str, Any] = {
    "sync": "P25 Phase 1 frame sync, 48 bits, MSB first",
    "sync_hex": P25_FRAME_SYNC_HEX,
    "block": "12 bytes: 10 data + CRC-16/CCITT-FALSE over those 10",
    "symbol_rate_bd": C4FM_SYMBOL_RATE_BD,
    "modulation": "c4fm",
    "dibit_map": {f"{k:02b}": v for k, v in C4FM_DEVIATIONS_HZ.items()},
    "frame_dibits": FRAME_DIBITS,
    "coding": "none (T-267 confirms sync+CRC; the P25 1/2-rate trellis is T-268)",
}


def bytes_to_dibits(data: bytes) -> np.ndarray:
    """MSB-first dibits (values 0..3) of ``data``."""
    bits = np.unpackbits(np.frombuffer(data, dtype=np.uint8))
    return (bits[0::2] * 2 + bits[1::2]).astype(np.uint8)


def sync_dibits() -> np.ndarray:
    """The 24 dibits of the P25 frame sync."""
    return bytes_to_dibits(bytes.fromhex(P25_FRAME_SYNC_HEX))


def tsbk_block(rng: np.random.Generator) -> tuple[bytes, int]:
    """A 12-byte TSBK-shaped block with a valid CRC. Returns ``(block, crc)``."""
    data = bytes(int(v) for v in rng.integers(0, 256, TSBK_DATA_BYTES))
    crc = crc16_ccitt_false(data)
    return data + crc.to_bytes(TSBK_CRC_BYTES, "big"), crc


def control_channel_dibits(rng: np.random.Generator, n_frames: int) -> tuple[np.ndarray, list[dict[str, Any]]]:
    """``n_frames`` back-to-back frames of (frame sync + CRC-valid block) as dibits."""
    sync = sync_dibits()
    out: list[np.ndarray] = []
    frames: list[dict[str, Any]] = []
    for i in range(n_frames):
        block, crc = tsbk_block(rng)
        out.append(sync)
        out.append(bytes_to_dibits(block))
        frames.append({
            "index": i,
            "block_hex": block.hex(),
            "crc_hex": f"{crc:04x}",
            "crc_valid": True,
        })
    return np.concatenate(out).astype(np.uint8), frames


# ---------------------------------------------------------------------------------------------
# TSBK content (T-268)
# ---------------------------------------------------------------------------------------------
#
# The block above is TSBK-*shaped*; these builders fill it with real P25 Phase 1 message fields so
# a decoder has something to read. Field layout and units, all verified against an independent
# reference before use (see crates/hk-detect/src/trunk/tsbk.rs for the verification notes):
#
#   TSBK, 12 bytes: LB(1) P(1) opcode(6) | MFID(8) | 8 argument bytes | CRC-16
#   IDEN_UP args, 64 bits: iden(4) bandwidth(9) offset-sign(1) offset-magnitude(8) spacing(10)
#                          base(32); base in units of 5 Hz, spacing in units of 125 Hz
#   GRP_VCH_GRANT args, 64 bits: service options(8) channel(16) group(16) source(24)
#   IDEN_UP_TDMA args, 64 bits: iden(4) channel-type(4) offset-sign(1) offset-magnitude(13)
#                               spacing(10) base(32); same units, plus a channel type whose slot
#                               count divides the channel number (T-272)
#   A 16-bit channel number is iden(4) then channel(12); f = base + spacing x (channel / slots),
#   and slot = channel % slots -- so on a two-slot TDMA plan, channels 2n and 2n+1 are ONE
#   frequency on two slots, which is C23's TDMA slot mix-up pitfall
#
# STILL NOT STANDARDS-COMPLIANT, deliberately and in the same four ways T-267 recorded: no
# rate-1/2 trellis code, no interleaving, no status symbols, and CRC-16/CCITT-FALSE where real P25
# uses the augmented CRC-CCITT (init 0, final XOR 0xFFFF). A real off-air capture needs all four.

#: TSBK opcodes (verified).
TSBK_OP_GRP_VCH_GRANT = 0x00
TSBK_OP_GRP_VCH_GRANT_UPDATE = 0x02
TSBK_OP_IDEN_UP = 0x3D
TSBK_OP_IDEN_UP_TDMA = 0x33

#: Slots per carrier for each 4-bit IDEN_UP_TDMA channel type (op25's ``slots_per_carrier``, for
#: types 0-4; the reserved types are not generated here because the decoder refuses them).
TDMA_SLOTS_PER_CHANNEL_TYPE = {0: 1, 1: 1, 2: 1, 3: 2, 4: 4}

# --- Service options (T-270) ---------------------------------------------------------------
#
# The encryption ("protected") bit of a grant's service-options octet, argument byte 0. VERIFIED
# three independent ways: SDRTrunk's ServiceOptions.java names ENCRYPTION_FLAG = 0x40 behind
# isEncrypted(); dsd-fme documents the same test as `svc & 0x40`; and a TIA-102.AABC-B-referenced
# field description gives bit 6 as "protected".
#
# The remaining bits (emergency 0x80, duplex 0x20, mode 0x10, priority 0x07) are UNVERIFIED for
# this project's purposes and the decoder maps nothing through them, so the generator sets none of
# them: a fixture must not depend on a field nothing is entitled to read.
#
# NOTE: a real GRP_VCH_GRANT_UPDATE (0x02) carries no service-options octet at all -- its standard
# argument layout is four 16-bit channel/group fields. This generator keeps T-268's simplified
# layout for 0x02 (see the module docstring's list of deliberate deviations); what matters for
# T-270 is that the DECODER reads a service-options octet only from opcode 0x00, so a channel
# announced only by an update can never have an encryption state, which is the late-entry case.
SVC_ENCRYPTED = 0x40

#: Base-frequency unit, Hz (verified: field 0x09157562 x 5 Hz = 762,006,250 Hz).
IDEN_BASE_UNIT_HZ = 5.0
#: Channel-spacing unit, Hz (verified: spacing field x 125 Hz).
IDEN_SPACING_UNIT_HZ = 125.0
#: Transmit-offset unit, Hz. UNVERIFIED encoding; nothing maps a downlink through it.
IDEN_OFFSET_UNIT_HZ = 250_000.0
#: Bandwidth unit, Hz. UNVERIFIED units.
IDEN_BANDWIDTH_UNIT_HZ = 125.0


def tsbk(opcode: int, args: bytes, *, last_block: bool = False, protected: bool = False,
         mfid: int = 0) -> bytes:
    """One 12-byte TSBK: header, 8 argument bytes, and a CRC over the 10 that precede it."""
    if len(args) != 8:
        raise ValueError("TSBK arguments are exactly 8 bytes")
    head = (0x80 if last_block else 0) | (0x40 if protected else 0) | (opcode & 0x3F)
    data = bytes([head, mfid & 0xFF]) + args
    return data + crc16_ccitt_false(data).to_bytes(TSBK_CRC_BYTES, "big")


def iden_up_args(iden: int, base_hz: float, spacing_hz: float, *, tx_offset_hz: float = 0.0,
                 bandwidth_hz: float = 0.0) -> bytes:
    """Pack an IDEN_UP argument field. Raises if a value does not encode exactly."""
    base = round(base_hz / IDEN_BASE_UNIT_HZ)
    spacing = round(spacing_hz / IDEN_SPACING_UNIT_HZ)
    bw = round(bandwidth_hz / IDEN_BANDWIDTH_UNIT_HZ)
    mag = round(abs(tx_offset_hz) / IDEN_OFFSET_UNIT_HZ)
    if base * IDEN_BASE_UNIT_HZ != base_hz:
        raise ValueError(f"base {base_hz} Hz is not a whole number of {IDEN_BASE_UNIT_HZ} Hz steps")
    if spacing * IDEN_SPACING_UNIT_HZ != spacing_hz:
        raise ValueError(f"spacing {spacing_hz} Hz is not a whole number of "
                         f"{IDEN_SPACING_UNIT_HZ} Hz steps")
    if not 0 <= iden <= 0xF or not 0 <= spacing <= 0x3FF or not 0 <= base <= 0xFFFF_FFFF:
        raise ValueError("an IDEN_UP field is out of range")
    if not 0 <= bw <= 0x1FF or not 0 <= mag <= 0xFF:
        raise ValueError("an IDEN_UP field is out of range")
    v = ((iden & 0xF) << 60) | (bw << 51) | ((1 if tx_offset_hz >= 0 else 0) << 50) \
        | (mag << 42) | (spacing << 32) | base
    return v.to_bytes(8, "big")


def iden_up_tdma_args(iden: int, channel_type: int, base_hz: float, spacing_hz: float,
                      *, tx_offset_steps: int = 0) -> bytes:
    """Pack an IDEN_UP_TDMA argument field (T-272). Raises if a value does not encode exactly.

    iden(4) channel-type(4) offset-sign(1) offset-magnitude(13) spacing(10) base(32) = 64 bits.
    The transmit offset is in units of the channel spacing here, not 250 kHz.
    """
    base = round(base_hz / IDEN_BASE_UNIT_HZ)
    spacing = round(spacing_hz / IDEN_SPACING_UNIT_HZ)
    if base * IDEN_BASE_UNIT_HZ != base_hz:
        raise ValueError(f"base {base_hz} Hz is not a whole number of {IDEN_BASE_UNIT_HZ} Hz steps")
    if spacing * IDEN_SPACING_UNIT_HZ != spacing_hz:
        raise ValueError(f"spacing {spacing_hz} Hz is not a whole number of "
                         f"{IDEN_SPACING_UNIT_HZ} Hz steps")
    if not 0 <= iden <= 0xF or not 0 <= channel_type <= 0xF:
        raise ValueError("an IDEN_UP_TDMA field is out of range")
    if not 0 <= spacing <= 0x3FF or not 0 <= base <= 0xFFFF_FFFF or abs(tx_offset_steps) > 0x1FFF:
        raise ValueError("an IDEN_UP_TDMA field is out of range")
    v = ((iden & 0xF) << 60) | ((channel_type & 0xF) << 56) \
        | ((1 if tx_offset_steps >= 0 else 0) << 55) | ((abs(tx_offset_steps) & 0x1FFF) << 42) \
        | (spacing << 32) | base
    return v.to_bytes(8, "big")


def tdma_channel_number(iden: int, channel: int, slot: int, slots: int) -> int:
    """The 16-bit channel number naming ``channel`` on ``slot`` of a ``slots``-slot TDMA plan.

    The FREQUENCY and the slot are the truth; this derives the number a grant has to carry to name
    them, never the other way round -- so a decoder still has to divide by the slot count it read
    off the air to get back to either.
    """
    if slots not in TDMA_SLOTS_PER_CHANNEL_TYPE.values():
        raise ValueError(f"{slots} is not a slot count any channel type names")
    if not 0 <= slot < slots:
        raise ValueError(f"slot {slot} is out of range for {slots} slots")
    return channel_number(iden, channel * slots + slot)


def grant_args(channel: int, talkgroup: int, *, source: int = 0, service_options: int = 0) -> bytes:
    """Pack a group voice channel grant argument field."""
    if not 0 <= channel <= 0xFFFF or not 0 <= talkgroup <= 0xFFFF:
        raise ValueError("a grant field is out of range")
    v = ((service_options & 0xFF) << 56) | (channel << 40) | (talkgroup << 24) \
        | (source & 0xFF_FFFF)
    return v.to_bytes(8, "big")


def channel_number(iden: int, channel: int) -> int:
    """The 16-bit channel number a grant carries: iden in the top 4 bits, channel in the low 12."""
    if not 0 <= iden <= 0xF or not 0 <= channel <= 0xFFF:
        raise ValueError("a channel number is iden(4) + channel(12)")
    return (iden << 12) | channel


def frames_from_blocks(blocks: list[bytes], n_frames: int) -> np.ndarray:
    """``n_frames`` frames of (frame sync + block), cycling through ``blocks``."""
    sync = sync_dibits()
    out: list[np.ndarray] = []
    for i in range(n_frames):
        block = blocks[i % len(blocks)]
        if len(block) != TSBK_BYTES:
            raise ValueError(f"a frame block is {TSBK_BYTES} bytes, got {len(block)}")
        out.append(sync)
        out.append(bytes_to_dibits(block))
    return np.concatenate(out).astype(np.uint8)


# ---------------------------------------------------------------------------------------------
# DMR Tier III (T-271)
# ---------------------------------------------------------------------------------------------
#
# A second trunking air interface, so a blind hunt has to find the right one rather than assuming
# the only framing it knows. Field layout and constants, all verified against an independent
# reference before use (see crates/hk-detect/src/trunk/dmr.rs for the verification notes and for
# what could NOT be corroborated):
#
#   Frame sync, 48 bits: BS-sourced DATA 0xDFF57D75DF5D (what a Tier III control channel sends),
#                        BS-sourced VOICE 0x755FD7DF75F7. Verified two ways beyond being stated:
#                        every dibit is an OUTER symbol, and the two words are exact dibit
#                        complements of each other.
#   CSBK, 96 bits: LB(1) PF(1) CSBKO(6) | FID(8) | 64 payload bits | CRC-16. The widths sum to
#                  exactly 96, which is itself the check.
#   CSBK CRC: CRC-CCITT over the first 80 bits, XORed with the mask 0xA5A5 (verified).
#   Grant payload, 64 bits: LPCN(12) timeslot(1) three UNVERIFIED flag bits target(24) source(24).
#                           Sums to exactly 64; the three flag bits are the residue and their
#                           meanings could not be corroborated, so nothing reads them.
#
# THE BAND PLAN IS THE POINT OF DIFFERENCE. P25 announces one (IDEN_UP), so a grant's channel
# number resolves to a frequency from the air alone. No DMR Tier III channel-parameter announcement
# could be corroborated, so a Logical Physical Channel Number resolves to NOTHING -- and a scene
# built on that has to contain the trap, or it proves nothing. See ``trunk_dmr_control_channel``.
#
# STILL NOT STANDARDS-COMPLIANT, deliberately and in the same spirit as the P25 frames above: no
# BPTC(196,96), no interleaving, the burst is flattened (a real DMR burst is 264 bits with the sync
# in the MIDDLE between two 98-bit halves), no TDMA burst timing or CACH, and the CRC's INITIAL
# VALUE is this repo's choice (0x0000) because it could not be corroborated -- the MASK is the
# verified part, and the initial value is invisible to every claim, since it changes only which
# 16-bit value a block carries and not the 2^-16 chance rate of the gate.

#: DMR base-station DATA frame sync, 48 bits (what a Tier III TSCC transmits). MSB first.
DMR_BS_DATA_SYNC_HEX = "dff57d75df5d"
#: DMR base-station VOICE frame sync, 48 bits. Present only as the arithmetic that verifies the
#: data sync: the two are exact dibit complements.
DMR_BS_VOICE_SYNC_HEX = "755fd7df75f7"

#: DMR 4FSK deviations, Hz (ETSI: outer +/-1.944 kHz, inner +/-648 Hz) -- narrower than P25 C4FM's
#: +/-1800/+/-600, which is deliberate: the demodulator's slicer must find the levels rather than
#: being handed the ones it already knows.
DMR_DEVIATIONS_HZ = {0b01: 1944.0, 0b00: 648.0, 0b10: -648.0, 0b11: -1944.0}

#: DMR symbol rate: 4800 Bd = 9600 bit/s, the same symbol rate as P25 C4FM.
DMR_SYMBOL_RATE_BD = 4800.0

#: CSBK size, bytes: 10 data + 2 CRC.
CSBK_DATA_BYTES = 10
CSBK_BYTES = 12
#: The mask XORed into a CSBK's CRC-CCITT before transmission (verified).
CSBK_CRC_MASK = 0xA5A5

#: CSBK opcodes (verified).
CSBKO_C_ALOHA = 0x19
CSBKO_P_CLEAR = 0x2E
CSBKO_P_GRANT = 0x30
CSBKO_BTV_GRANT = 0x32
CSBKO_PD_GRANT = 0x33
CSBKO_TD_GRANT = 0x34

#: Dibits in one DMR frame as this generator lays it out: sync + CSBK.
DMR_FRAME_DIBITS = 24 + CSBK_BYTES * 4

DMR_FRAME_SPEC: dict[str, Any] = {
    "sync": "DMR base-station data frame sync, 48 bits, MSB first",
    "sync_hex": DMR_BS_DATA_SYNC_HEX,
    "block": "12 bytes: LB/PF/CSBKO(8) FID(8) 8 payload bytes, CRC-CCITT masked with 0xA5A5",
    "symbol_rate_bd": DMR_SYMBOL_RATE_BD,
    "modulation": "4fsk",
    "dibit_map": {f"{k:02b}": v for k, v in DMR_DEVIATIONS_HZ.items()},
    "frame_dibits": DMR_FRAME_DIBITS,
    "coding": "none (no BPTC(196,96), no interleaving, burst flattened, no TDMA burst timing)",
}


def crc16_ccitt_zero(data: bytes) -> int:
    """CRC-CCITT, poly 0x1021, **initial value 0x0000**, no reflection, no final XOR.

    The DMR CSBK mask is applied by the caller. See the module notes: the initial value could not
    be corroborated and is this repo's choice, which no claim depends on.
    """
    crc = 0x0000
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def dmr_sync_dibits(voice: bool = False) -> np.ndarray:
    """The 24 dibits of a DMR base-station frame sync (data by default)."""
    return bytes_to_dibits(bytes.fromhex(DMR_BS_VOICE_SYNC_HEX if voice else DMR_BS_DATA_SYNC_HEX))


def csbk(csbko: int, payload: bytes, *, last_block: bool = False, protect: bool = False,
         fid: int = 0) -> bytes:
    """One 12-byte CSBK: header, 8 payload bytes, and the masked CRC over the 10 that precede it."""
    if len(payload) != 8:
        raise ValueError("a CSBK payload is exactly 8 bytes")
    head = (0x80 if last_block else 0) | (0x40 if protect else 0) | (csbko & 0x3F)
    data = bytes([head, fid & 0xFF]) + payload
    crc = crc16_ccitt_zero(data) ^ CSBK_CRC_MASK
    return data + crc.to_bytes(2, "big")


def dmr_grant_payload(lpcn: int, timeslot: int, target: int, source: int, *, flags: int = 0
                      ) -> bytes:
    """Pack a Tier III channel-grant payload: LPCN(12) TS(1) flags(3) target(24) source(24)."""
    if not 0 <= lpcn <= 0xFFF or timeslot not in (0, 1) or not 0 <= flags <= 7:
        raise ValueError("a DMR grant field is out of range")
    if not 0 <= target <= 0xFF_FFFF or not 0 <= source <= 0xFF_FFFF:
        raise ValueError("a DMR address is 24 bits")
    v = ((lpcn & 0xFFF) << 52) | ((timeslot & 1) << 51) | ((flags & 7) << 48) \
        | ((target & 0xFF_FFFF) << 24) | (source & 0xFF_FFFF)
    return v.to_bytes(8, "big")


def dmr_frames_from_blocks(blocks: list[bytes], n_frames: int) -> np.ndarray:
    """``n_frames`` frames of (DMR BS-data sync + CSBK), cycling through ``blocks``."""
    sync = dmr_sync_dibits()
    out: list[np.ndarray] = []
    for i in range(n_frames):
        block = blocks[i % len(blocks)]
        if len(block) != CSBK_BYTES:
            raise ValueError(f"a CSBK is {CSBK_BYTES} bytes, got {len(block)}")
        out.append(sync)
        out.append(bytes_to_dibits(block))
    return np.concatenate(out).astype(np.uint8)


# ---------------------------------------------------------------------------------------------
# NXDN Type-C (T-345)
# ---------------------------------------------------------------------------------------------
#
# A THIRD trunking air interface, and the first whose channel coding is implemented faithfully
# rather than flattened. Everything below is from the NXDN Forum's own published air interface,
# **NXDN TS 1-A "Common Air Interface" Ver. 1.3**, with TS 1-C for the trunking half, cross-checked
# against dsd-fme's independent decoder; see crates/hk-detect/src/trunk/nxdn.rs for the full
# verification notes, including which identities close by arithmetic.
#
#   Frame sync, 20 bits: 0xCDF59 (Table 4.4-2), published beside its symbol sequence
#                        -3,+1,-3,+3,-3,-3,+3,+3,-1,+3, which the hex reproduces exactly.
#   Outbound RCCH frame, 384 bits: FSW(20) LICH(16) CAC(300) E(24) Post(24). The sum is the check,
#                        and it is 192 symbols.
#   LICH: 7 control bits + 1 even-parity bit over the top four, each sent as an OUTER symbol
#         (1 -> 11 = -3, 0 -> 01 = +3). For an outbound RCCH carrying a CAC the control bits are
#         00 (RCCH) 00 (CAC) xx (data flag) 1 (outbound).
#   CAC coding flow (Figure 4.5-1): SR(8) + L3 message(144) + 3 null bits = 155 information bits,
#         + CRC-16 (X^16+X^12+X^5+1, register preset to all ones) = 171, + 4 zero tail bits = 175,
#         rate-1/2 K=5 convolutional (G1 = 1+D^3+D^4, G2 = 1+D+D^2+D^4, i.e. the textbook (23,35)
#         octal pair) = 350, punctured 12-of-14 = 300, interleaved 25 rows x 12 = the 300-bit CAC.
#         350 x 12/14 = 300 and 300 / 12 = 25: both identities have to close.
#   Scrambler (Sec 4.6): 9-bit PN, register preset 011100100 = 0xE4, reinitialised per frame,
#         applied as a SIGN INVERSION to all 182 symbols after the frame sync.
#   VCALL_ASSGN = message type 00 0100; its nine mandatory octets are msgtype(6 behind 2 spare
#         flags), CC Option(8), Call Type(3)+Voice Call Option(5), Source Unit ID(16), Destination
#         Group or Unit ID(16), Call Timer(6)+Channel(10) - 72 bits with nothing over.
#
# THE CHANNEL IS THE POINT OF DIFFERENCE, and it is sharper than DMR's. Sec 6.5.31 defines Channel
# as a 10-bit NUMBER, 1 to 1023, "a value to determine the carrier frequency" -- and the air
# interface defines no mapping from one to hertz anywhere; not one of its forty-one information
# elements is a frequency. The map is configured in the radio. So a decoded assignment resolves to
# NOTHING, and a scene built on that has to contain the trap or it proves nothing. See
# ``trunk_nxdn_control_channel``.
#
# NOT STANDARDS-COMPLIANT in these ways, stated so nobody mistakes it for an NXDN encoder: only the
# outbound CAC is generated (no inbound Long/Short CAC, no SACCH/FACCH/UDCH/voice), there is no
# superframe or paging-frame structure, optional information elements past octet 8 are not emitted,
# and the E (collision control) field is filled with random bits because nothing reads it.

#: NXDN frame sync word, 20 bits (Table 4.4-2). MSB first.
NXDN_FSW_HEX = "cdf59"
NXDN_FSW_BITS = 20
#: The Preamble, published as hex (Table 4.4-1) -- and, symbol for symbol, the Post field, which is
#: published only as symbols (Table 4.4-3). The two agreeing is what verifies the dibit map.
NXDN_PREAMBLE_HEX = "5775fd"

#: NXDN 4FSK deviations for the 9600 bps (12.5 kHz) variant (Table 3.3-1): outer +/-2400 Hz, inner
#: +/-800 Hz. Wider than DMR's and P25's, which is deliberate: the demodulator must find the levels.
NXDN_DEVIATIONS_HZ = {0b01: 2400.0, 0b00: 800.0, 0b10: -800.0, 0b11: -2400.0}

#: 9600 bps over 4-level FSK is 4800 Bd -- the same symbol rate P25 and DMR use, which is the only
#: reason this variant is reachable at all. The 6.25 kHz variant is 2400 Bd and is not generated.
NXDN_SYMBOL_RATE_BD = 4800.0

#: Symbols in one outbound RCCH frame.
NXDN_FRAME_DIBITS = 192
#: Symbols the scrambler covers: LICH(8) + CAC(150) + E(12) + Post(12).
NXDN_SCRAMBLED_DIBITS = 182
#: Bits of layer-3 information a CAC carries: SR(8) + message(144).
NXDN_L3_BITS = 152
NXDN_L3_BYTES = 19
NXDN_MESSAGE_BYTES = 18
#: Bits of the coded CAC.
NXDN_CAC_BITS = 300
#: Interleaver geometry.
NXDN_INTERLEAVE_DEPTH = 25
NXDN_INTERLEAVE_WIDTH = 12
#: The scrambler's published register preset, S8..S0 = 011100100.
NXDN_SCRAMBLER_SEED = 0b0_1110_0100

#: NXDN message types (Sec 6.4.5). The RCCH-outbound ones this project names.
NXDN_MSG_VCALL_ASSGN = 0x04
NXDN_MSG_VCALL_ASSGN_DUP = 0x05
NXDN_MSG_DCALL_ASSGN_DUP = 0x0D
NXDN_MSG_DCALL_ASSGN = 0x0E
NXDN_MSG_SITE_INFO = 0x18
NXDN_MSG_SRV_INFO = 0x19
NXDN_MSG_CCH_INFO = 0x1A
NXDN_MSG_ADJ_SITE_INFO = 0x1B

#: Call Type values (Sec 6.5.12).
NXDN_CALL_BROADCAST = 0b000
NXDN_CALL_CONFERENCE = 0b001
NXDN_CALL_INDIVIDUAL = 0b100

#: Channel values (Sec 6.5.31): 0 is the Null filler, 1..1023 are channels.
NXDN_CHANNEL_MAX = 1023

NXDN_FRAME_SPEC: dict[str, Any] = {
    "sync": "NXDN frame sync word, 20 bits, MSB first",
    "sync_hex": NXDN_FSW_HEX,
    "frame": "FSW(20) LICH(16) CAC(300) E(24) Post(24) = 384 bits",
    "block": "CAC: SR(8) + L3(144) + null(3) + CRC-16/CCITT-FALSE(16) + tail(4), "
             "rate-1/2 K=5 convolutional (23,35) octal, punctured 12-of-14, interleaved 25x12",
    "symbol_rate_bd": NXDN_SYMBOL_RATE_BD,
    "modulation": "4fsk",
    "dibit_map": {f"{k:02b}": v for k, v in NXDN_DEVIATIONS_HZ.items()},
    "frame_dibits": NXDN_FRAME_DIBITS,
    "scrambler": "PN9, register preset 0xE4, sign inversion over the 182 symbols after the FSW",
    "coding": "faithful (the CAC's FEC IS implemented); outbound CAC only, no superframe "
              "structure, no optional information elements, E field not modelled",
}


def nxdn_sync_dibits() -> np.ndarray:
    """The 10 dibits of the NXDN frame sync word."""
    v = int(NXDN_FSW_HEX, 16)
    return np.array([(v >> (NXDN_FSW_BITS - 2 - 2 * i)) & 3 for i in range(NXDN_FSW_BITS // 2)],
                    dtype=np.uint8)


def nxdn_post_dibits() -> np.ndarray:
    """The 12 dibits of the Post field -- the Preamble's pattern, from Table 4.4-1."""
    return bytes_to_dibits(bytes.fromhex(NXDN_PREAMBLE_HEX))


def nxdn_scramble(dibits: np.ndarray) -> np.ndarray:
    """Scrambles (or descrambles: it is its own inverse) the 182 symbols after a frame sync.

    Sign inversion of a 4-level symbol is exactly "flip the dibit's most significant bit", because
    the map sends 01/11 to +3/-3 and 00/10 to +1/-1.
    """
    reg = NXDN_SCRAMBLER_SEED
    out = np.array(dibits, dtype=np.uint8, copy=True)
    for i in range(len(out)):
        bit = reg & 1
        feedback = (reg ^ (reg >> 4)) & 1
        reg = (reg >> 1) | (feedback << 8)
        if bit:
            out[i] ^= 0b10
    return out


def nxdn_lich_dibits(control: int) -> np.ndarray:
    """The 8 outer symbols of a LICH: 7 control bits plus even parity over the top four."""
    control &= 0x7F
    parity = bin(control >> 3).count("1") & 1
    bits = [(control >> (6 - i)) & 1 for i in range(7)] + [parity]
    return np.array([0b11 if b else 0b01 for b in bits], dtype=np.uint8)


def _nxdn_conv_outputs(state: int, u: int) -> tuple[int, int]:
    """G1 = 1 + D^3 + D^4 and G2 = 1 + D + D^2 + D^4, with ``state`` holding the last four inputs."""
    g1 = u ^ ((state >> 2) & 1) ^ ((state >> 3) & 1)
    g2 = u ^ (state & 1) ^ ((state >> 1) & 1) ^ ((state >> 3) & 1)
    return g1, g2


def nxdn_encode_cac(info_bits: list[int]) -> list[int]:
    """Encodes the 155 information bits into the 300-bit CAC."""
    if len(info_bits) != 155:
        raise ValueError(f"a CAC carries 155 information bits, got {len(info_bits)}")
    crc = crc16_ccitt_false_bits(info_bits)
    bits = list(info_bits) + [(crc >> (15 - i)) & 1 for i in range(16)] + [0, 0, 0, 0]
    if len(bits) != 175:
        raise AssertionError(f"the convolutional encoder takes 175 bits, got {len(bits)}")
    coded: list[int] = []
    state = 0
    for u in bits:
        g1, g2 = _nxdn_conv_outputs(state, u)
        coded += [g1, g2]
        state = ((state << 1) | u) & 0xF
    # Puncture: of every seven codeword pairs, the G2 bits of pairs 1 and 5 are erased (the
    # specification's own worked example: "X4 and X12 are erased").
    punctured: list[int] = []
    for i in range(175):
        punctured.append(coded[2 * i])
        if i % 7 not in (1, 5):
            punctured.append(coded[2 * i + 1])
    if len(punctured) != NXDN_CAC_BITS:
        raise AssertionError(f"puncturing yielded {len(punctured)} bits, not {NXDN_CAC_BITS}")
    # Interleave: write 25 rows of 12, read down the columns.
    return [punctured[(k % NXDN_INTERLEAVE_DEPTH) * NXDN_INTERLEAVE_WIDTH
                      + k // NXDN_INTERLEAVE_DEPTH] for k in range(NXDN_CAC_BITS)]


def crc16_ccitt_false_bits(bits: list[int]) -> int:
    """CRC-16/CCITT-FALSE over a bit sequence of any length (the CAC's 155 bits are not a whole
    number of octets, which is why this exists beside the byte-wise one)."""
    crc = 0xFFFF
    for b in bits:
        crc ^= (b & 1) << 15
        crc = ((crc << 1) ^ 0x1021) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def nxdn_cac_dibits(info: bytes) -> np.ndarray:
    """The 150 symbols of a CAC carrying ``info`` (19 bytes: SR + an 18-octet message)."""
    if len(info) != NXDN_L3_BYTES:
        raise ValueError(f"a CAC carries {NXDN_L3_BYTES} bytes of layer-3 information")
    bits = [(info[i // 8] >> (7 - i % 8)) & 1 for i in range(NXDN_L3_BITS)] + [0, 0, 0]
    cac = nxdn_encode_cac(bits)
    return np.array([(cac[2 * i] << 1) | cac[2 * i + 1] for i in range(NXDN_CAC_BITS // 2)],
                    dtype=np.uint8)


def nxdn_frame_dibits(info: bytes, rng: np.random.Generator, *, control: int = 0b000_0001
                      ) -> np.ndarray:
    """One outbound RCCH frame: FSW, then the scrambled LICH, CAC, E and Post fields."""
    body = np.concatenate([
        nxdn_lich_dibits(control),
        nxdn_cac_dibits(info),
        # E, the collision control field: real coded data about inbound access, which nothing here
        # decodes, so it is filled with random symbols rather than a pattern that would bias the
        # demodulator's level estimate.
        rng.integers(0, 4, 12).astype(np.uint8),
        nxdn_post_dibits(),
    ])
    if len(body) != NXDN_SCRAMBLED_DIBITS:
        raise AssertionError(f"a frame body is {NXDN_SCRAMBLED_DIBITS} symbols, got {len(body)}")
    return np.concatenate([nxdn_sync_dibits(), nxdn_scramble(body)]).astype(np.uint8)


def nxdn_message(message_type: int, octets: bytes, ran: int = 0) -> bytes:
    """The 19 bytes of layer-3 information: SR(structure + RAN) then an 18-octet message."""
    if len(octets) > NXDN_MESSAGE_BYTES - 1:
        raise ValueError("a layer-3 message is at most 18 octets including its type")
    body = bytes([message_type & 0x3F]) + octets
    return bytes([ran & 0x3F]) + body.ljust(NXDN_MESSAGE_BYTES, b"\x00")


def nxdn_assignment_octets(call_type: int, source: int, destination: int, channel: int,
                           *, call_timer: int = 2, call_option: int = 0b00010,
                           cc_option: int = 0) -> bytes:
    """Octets 1..8 of an assignment message (octet 0 is the message type).

    Layout, summing to the nine mandatory octets: CC Option(8), Call Type(3) + Call Option(5),
    Source Unit ID(16), Destination Group or Unit ID(16), Call Timer(6) + Channel(10).
    """
    if not 0 <= channel <= NXDN_CHANNEL_MAX:
        raise ValueError("an NXDN channel number is 10 bits, 0 (Null) to 1023")
    if not 0 <= source <= 0xFFFF or not 0 <= destination <= 0xFFFF:
        raise ValueError("an NXDN unit or group ID is 16 bits")
    if not 0 <= call_timer <= 0x3F or not 0 <= call_type <= 0b111:
        raise ValueError("an NXDN assignment field is out of range")
    return bytes([
        cc_option & 0xFF,
        ((call_type & 0b111) << 5) | (call_option & 0x1F),
        (source >> 8) & 0xFF, source & 0xFF,
        (destination >> 8) & 0xFF, destination & 0xFF,
        ((call_timer & 0x3F) << 2) | ((channel >> 8) & 0b11),
        channel & 0xFF,
    ])


def nxdn_frames_from_messages(messages: list[bytes], n_frames: int,
                              rng: np.random.Generator) -> np.ndarray:
    """``n_frames`` outbound RCCH frames, cycling through ``messages``."""
    out = [nxdn_frame_dibits(messages[i % len(messages)], rng) for i in range(n_frames)]
    return np.concatenate(out).astype(np.uint8)


def continuous_data_dibits(rng: np.random.Generator, n_dibits: int) -> np.ndarray:
    """Unframed continuous 4FSK traffic: the decoy a pure FCO test cannot tell from a CC.

    Uniform random dibits carry no frame sync and no CRC-valid block, so this emitter is
    continuous, on-raster and 4FSK — it passes every candidacy test — and must still be
    rejected at confirmation.
    """
    return rng.integers(0, 4, int(n_dibits)).astype(np.uint8)


def c4fm(dibits: np.ndarray, sample_rate: float, symbol_rate: float = C4FM_SYMBOL_RATE_BD,
         *, deviations: dict[int, float] | None = None, phase0: float = 0.0,
         shape: bool = True) -> np.ndarray:
    """Continuous-phase 4FSK (C4FM) for ``dibits``, one complex sample per output sample.

    ``shape`` applies the raised-cosine-ish symbol smoothing a real C4FM modulator uses, which
    keeps the emission inside 12.5 kHz instead of splattering across neighbouring channels.
    """
    dev = deviations or C4FM_DEVIATIONS_HZ
    d = np.asarray(dibits, dtype=np.uint8)
    n = int(math.ceil(len(d) * sample_rate / symbol_rate))
    idx = np.minimum((np.arange(n) * symbol_rate / sample_rate).astype(np.int64), len(d) - 1)
    levels = np.array([dev[int(v)] for v in d], dtype=np.float64)
    freq = levels[idx]
    if shape:
        sps = sample_rate / symbol_rate
        # A **one-symbol-wide** raised cosine. The width is not cosmetic: a two-symbol-wide pulse
        # is not a Nyquist pulse, so it closes the eye at the symbol centres and no receiver can
        # read it — measured here at ~38 % symbol error rate against 0 % at this width. A fixture
        # that cannot be demodulated tests nothing, so the pulse has to be one a real C4FM
        # modulator would emit: narrow enough to keep the centres clean, wide enough to
        # band-limit the deviation steps into 12.5 kHz.
        w = max(int(sps / 2), 1)
        k = np.arange(-w, w + 1)
        h = 0.5 * (1.0 + np.cos(math.pi * k / w))
        freq = np.convolve(freq, h / h.sum(), mode="same")
    phase = phase0 + 2 * math.pi * np.cumsum(freq) / sample_rate
    return np.exp(1j * phase)


# ---------------------------------------------------------------------------------------------
# P25 Phase 1 voice frames: LDU1 link control, LDU2 encryption sync (T-849)
# ---------------------------------------------------------------------------------------------
#
# Unlike the TSBK block above, this IS the P25 coding as recalled -- status symbols, the BCH NID,
# Hamming(10,6,3) hexbits and the two GF(64) Reed-Solomon codes -- because the decoder it feeds
# (crates/hk-detect/src/trunk/ldu.rs) reads the real layout. Recalled, NOT verified against the
# standard or a real capture: see that module's docs for what each piece rests on. This encoder is
# written separately from the Rust one and represents each code differently (the BCH generator as
# the published octal literal where Rust derives it from GF(64); the Hamming parity from six
# generator columns where Rust carries the 64-entry table; RS by an LFSR where Rust does long
# division), so a slip in either implementation does not silently agree with itself.
#
#   LDU = sync 48 | NID 64 | IMBE1 | IMBE2 | LC 40 | IMBE3 | LC 40 | IMBE4 | LC 40 | IMBE5 |
#         LC 40 | IMBE6 | LC 40 | IMBE7 | LC 40 | IMBE8 | LSD 32 | IMBE9     (1680 bits)
#   then one status dibit after every 35 dibits, counted from the first sync dibit -> 864 dibits.
#
# The IMBE and LSD bits are random: nothing reads them, and real IMBE frames would add nothing a
# decoder of LC/ES could use. No voice exists in this fixture and none can be recovered from it.

#: The published BCH(63,16,23) generator, octal (bit i = coefficient of x^i).
P25_NID_BCH_GENERATOR = 0o6331_1413_6723_5453
#: The default network access code (the conventional factory NAC).
P25_DEFAULT_NAC = 0x293
P25_DUID_LDU1 = 0x5
P25_DUID_LDU2 = 0xA
#: Dibits in one LDU on the air, status symbols included.
P25_LDU_DIBITS = 864
#: Seconds one LDU occupies (1728 bits at 9600 bit/s).
P25_LDU_S = 0.18
#: First bit of each 40-bit LC/ES block in the de-statused frame.
P25_LC_BLOCK_STARTS = (400, 584, 768, 952, 1136, 1320)
#: P25 ALGIDs this scene uses.
P25_ALGID_CLEAR = 0x80
P25_ALGID_AES256 = 0x84

#: Hamming(10,6,3) parity of each single data bit, MSB (32) first.
_HAMMING_COLUMNS = {32: 14, 16: 13, 8: 11, 4: 7, 2: 3, 1: 12}


def _gf64_tables() -> tuple[list[int], list[int]]:
    exp: list[int] = []
    log = [0] * 64
    x = 1
    for i in range(63):
        exp.append(x)
        log[x] = i
        x <<= 1
        if x & 0x40:
            x ^= 0x43  # x^6 + x + 1
    return exp, log


_GF_EXP, _GF_LOG = _gf64_tables()


def _gf_mul(a: int, b: int) -> int:
    if a == 0 or b == 0:
        return 0
    return _GF_EXP[(_GF_LOG[a] + _GF_LOG[b]) % 63]


def hamming_10_6(data: int) -> int:
    """The 10-bit codeword of a hexbit: six data bits then four parity bits."""
    parity = 0
    for bit, col in _HAMMING_COLUMNS.items():
        if data & bit:
            parity ^= col
    return ((data & 0x3F) << 4) | parity


def rs64_parity(data: list[int], n: int) -> list[int]:
    """The ``n - len(data)`` parity hexbits of a shortened RS code over GF(64), roots a^1..a^(n-k).

    Systematic, data first on the air, via the usual LFSR: the register holds the running
    remainder of d(x) x^(n-k) divided by g(x).
    """
    p = n - len(data)
    g = [1]  # coefficient i of x^i
    for i in range(1, p + 1):
        root = _GF_EXP[i % 63]
        nxt = [0] * (len(g) + 1)
        for j, c in enumerate(g):
            nxt[j + 1] ^= c
            nxt[j] ^= _gf_mul(c, root)
        g = nxt
    reg = [0] * p  # reg[0] is the highest-order remainder coefficient
    for d in data:
        fb = d ^ reg[0]
        reg = reg[1:] + [0]
        if fb:
            for j in range(p):
                reg[j] ^= _gf_mul(fb, g[p - 1 - j])
    return reg


def p25_nid(nac: int, duid: int) -> int:
    """The 64-bit NID: BCH(63,16) over NAC and DUID, then an even-parity bit (unverified, unread)."""
    data = ((nac & 0xFFF) << 4) | (duid & 0xF)
    rem = data << 47
    for bit in range(62, 46, -1):
        if rem >> bit & 1:
            rem ^= P25_NID_BCH_GENERATOR << (bit - 47)
    cw = (data << 47) | rem
    return (cw << 1) | (bin(cw).count("1") & 1)


def p25_lc_group_voice(talkgroup: int, source: int, service_options: int = 0) -> bytes:
    """A group-voice-channel-user link control: LCF 0x00, MFID 0, svc, reserved, TGID, source."""
    return (bytes([0x00, 0x00, service_options & 0xFF, 0x00])
            + int(talkgroup).to_bytes(2, "big") + int(source).to_bytes(3, "big"))


def p25_es(mi: bytes, algid: int, key_id: int) -> bytes:
    """An encryption sync: 72-bit MI, ALGID, 16-bit key id."""
    if len(mi) != 9:
        raise ValueError("the message indicator is 72 bits")
    return bytes(mi) + bytes([algid & 0xFF]) + int(key_id).to_bytes(2, "big")


def _hexbits(payload: bytes) -> list[int]:
    value = int.from_bytes(payload, "big")
    n = len(payload) * 8 // 6
    return [(value >> (6 * (n - 1 - i))) & 0x3F for i in range(n)]


def p25_ldu_dibits(duid: int, payload: bytes, rng: np.random.Generator, *,
                   nac: int = P25_DEFAULT_NAC, status: int = 0b10) -> np.ndarray:
    """The 864 on-air dibits of one LDU1 (``payload`` = 9-octet LC) or LDU2 (12-octet ES)."""
    if duid == P25_DUID_LDU1 and len(payload) == 9:
        n_data = 12
    elif duid == P25_DUID_LDU2 and len(payload) == 12:
        n_data = 16
    else:
        raise ValueError("an LDU1 carries a 9-octet LC and an LDU2 a 12-octet ES")
    hexbits = _hexbits(payload)
    if len(hexbits) != n_data:
        raise AssertionError("payload does not fill its hexbits")
    hexbits = hexbits + rs64_parity(hexbits, 24)
    bits = rng.integers(0, 2, 1680).astype(np.uint8)  # IMBE + LSD filler, overwritten below
    bits[:48] = np.unpackbits(np.frombuffer(bytes.fromhex(P25_FRAME_SYNC_HEX), dtype=np.uint8))
    nid = p25_nid(nac, duid)
    bits[48:112] = [(nid >> (63 - i)) & 1 for i in range(64)]
    for b, start in enumerate(P25_LC_BLOCK_STARTS):
        for w in range(4):
            cw = hamming_10_6(hexbits[b * 4 + w])
            at = start + w * 10
            bits[at:at + 10] = [(cw >> (9 - i)) & 1 for i in range(10)]
    data = (bits[0::2] * 2 + bits[1::2]).astype(np.uint8)
    out: list[int] = []
    for d in data:
        out.append(int(d))
        if len(out) % 36 == 35:
            out.append(status & 3)
    if len(out) != P25_LDU_DIBITS:
        raise AssertionError("an LDU is 864 dibits on the air")
    return np.array(out, dtype=np.uint8)
