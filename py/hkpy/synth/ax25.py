"""AX.25 UI frames over Bell 202 AFSK 1200 (APRS, T-952): address/control/PID/info framing, HDLC
zero-bit stuffing, NRZI line coding, and the Bell 202 tone-pair audio waveform an NBFM voice
channel carries.

**Conventions (AX.25 v2.2 §2, ISO/IEC 13239; not a captured recording — no APRS burst reached the
explorer's antenna on 2026-09-25, see T-952's notes).**

- Every AX.25 octet, including the flag, is transmitted **least-significant-bit first**. Address
  octets hold a 6-character callsign, each character **ASCII shifted left one bit** (bit 0 always
  0 for a non-terminal character), followed by one SSID octet: bit 7 command/response, bits 6-5
  reserved (sent 1, matching convention), bits 4-1 the SSID number, bit 0 the "last address"
  extension bit (1 = no further addresses/digipeaters follow).
- This module builds the simplest valid UI frame: destination + source only, no digipeater path
  (`docs/07`'s Detection/Emitter model needs no path parsing to prove the decode chain blind).
- Control 0x03 (UI, no poll/final), PID 0xF0 (no layer-3 protocol) — standard APRS.
- FCS: CRC-16/X-25 (poly 0x1021, init 0xFFFF, refin/refout, xorout 0xFFFF) over every octet from
  the destination address through the info field, sent low byte first, computed with
  ``hkpy.synth.fsk.crc_generic`` (the same RevEng core the Rust `crc` block implements).
- Zero-bit stuffing: after five consecutive 1 bits, insert a stuffed 0 (flags are exempt, and are
  not part of the stuffed span). NRZI: a 0 data bit toggles the line level, a 1 leaves it unchanged
  (`transition-is-0`, ISO 13239 / USB / AX.25 convention); the whole flag-delimited transmission
  (preamble flags, frame, trailing flags) runs through one continuous NRZI encoder.
- Bell 202: mark 1200 Hz, space 2200 Hz, 1200 Bd. Which NRZI line level maps to which tone is an
  arbitrary, self-consistent choice here (mark = level 1): the receive chain (`nrzi` block,
  `decode` direction) recovers data from *transitions*, which are invariant to a constant
  inversion of the sliced line, so the choice cannot desync a compliant receiver.
"""

from __future__ import annotations

import math
from dataclasses import dataclass

import numpy as np

from hkpy.synth.fsk import crc_generic

BAUD = 1200.0
MARK_HZ = 1200.0
SPACE_HZ = 2200.0
FLAG = 0x7E
CONTROL_UI = 0x03
PID_NONE = 0xF0
#: AX.25 FCS: CRC-16/X-25 (RevEng name; ISO/IEC 13239's FCS).
FCS_SPEC = {
    "algorithm": "CRC-16/X-25",
    "poly": "0x1021",
    "init": "0xFFFF",
    "refin": True,
    "refout": True,
    "xorout": "0xFFFF",
    "check_123456789": "0x906E",
    "covers": "destination through info field octets (addresses, control, pid, info)",
    "byte_order": "little-endian (low byte first on air)",
}
FRAMING_NOTE = (
    "flags (0x7E) delimit a zero-bit-stuffed HDLC frame; every octet incl. the FCS sent LSB "
    "first; address octets are ASCII characters shifted left 1 bit + an SSID/extension octet; "
    "control 0x03 (UI), PID 0xF0 (no layer 3); FCS = CRC-16/X-25 over dest..info, low byte first; "
    "NRZI (transition-is-0) over the whole flag-delimited transmission"
)


def callsign_octets(call: str, ssid: int, *, last: bool, command_or_response: bool = False) -> bytes:
    """7 address octets: 6 shifted-ASCII characters (space-padded/truncated) then the SSID octet."""
    call6 = call.upper().strip().ljust(6)[:6]
    chars = bytes((ord(c) & 0x7F) << 1 for c in call6)
    ssid_octet = (
        (0x80 if command_or_response else 0x00)
        | 0x60  # reserved bits, sent 1 (common convention)
        | ((ssid & 0x0F) << 1)
        | (0x01 if last else 0x00)
    )
    return chars + bytes([ssid_octet])


def crc_x25(data: bytes) -> int:
    """AX.25 FCS: CRC-16/X-25 over `data` (see :data:`FCS_SPEC`)."""
    return crc_generic(data, 16, 0x1021, 0xFFFF, True, True, 0xFFFF)


@dataclass
class Frame:
    dest_call: str
    dest_ssid: int
    src_call: str
    src_ssid: int
    info: str
    dest_bytes: bytes  # 7 raw address octets (shifted ASCII + SSID), on-air content order
    src_bytes: bytes
    content: bytes  # dest + src + control + pid + info, the FCS's span
    fcs: int
    tx_bytes: bytes  # content + FCS (low byte first) — the frame between flags, pre-stuffing
    fcs_spec: dict


def build_ui_frame(dest_call: str, src_call: str, src_ssid: int, info: str,
                    *, dest_ssid: int = 0) -> Frame:
    dest_bytes = callsign_octets(dest_call, dest_ssid, last=False)
    src_bytes = callsign_octets(src_call, src_ssid, last=True)
    body = dest_bytes + src_bytes + bytes([CONTROL_UI, PID_NONE]) + info.encode("ascii")
    fcs = crc_x25(body)
    tx = body + bytes([fcs & 0xFF, (fcs >> 8) & 0xFF])
    return Frame(dest_call=dest_call, dest_ssid=dest_ssid, src_call=src_call, src_ssid=src_ssid,
                 info=info, dest_bytes=dest_bytes, src_bytes=src_bytes, content=body, fcs=fcs,
                 tx_bytes=tx, fcs_spec=FCS_SPEC)


def lsb_first_bits(data: bytes) -> np.ndarray:
    """On-air bit order: each octet least-significant-bit first."""
    return np.unpackbits(np.frombuffer(data, dtype=np.uint8), bitorder="little")


def bit_stuff(bits: np.ndarray) -> np.ndarray:
    """HDLC zero-bit stuffing: a 0 inserted after every run of five consecutive 1 bits."""
    out = []
    ones = 0
    for b in np.asarray(bits, dtype=np.uint8):
        out.append(int(b))
        if b == 1:
            ones += 1
            if ones == 5:
                out.append(0)
                ones = 0
        else:
            ones = 0
    return np.array(out, dtype=np.uint8)


def nrzi_encode(bits: np.ndarray) -> np.ndarray:
    """NRZI line levels: level toggles on a 0 data bit, holds on a 1 (`transition-is-0`). Starts
    at level 1 (the `nrzi` block's `encode` direction convention)."""
    levels = np.empty(len(bits), dtype=np.uint8)
    level = 1
    for i, b in enumerate(np.asarray(bits, dtype=np.uint8)):
        if b == 0:
            level ^= 1
        levels[i] = level
    return levels


def build_transmission(frame: Frame, *, n_preamble_flags: int = 20,
                        n_trailer_flags: int = 2) -> tuple[np.ndarray, dict]:
    """Preamble flags + opening flag + stuffed frame + closing flag + trailer flags, LSB-first,
    NRZI-encoded as one continuous line. Returns (NRZI levels, info dict with bit counts)."""
    flag_bits = lsb_first_bits(bytes([FLAG]))
    data_bits = lsb_first_bits(frame.tx_bytes)
    stuffed = bit_stuff(data_bits)
    all_bits = np.concatenate(
        [np.tile(flag_bits, n_preamble_flags), flag_bits, stuffed, flag_bits,
         np.tile(flag_bits, n_trailer_flags)]
    )
    levels = nrzi_encode(all_bits)
    info = {
        "n_data_bits": int(len(data_bits)),
        "n_stuffed_bits": int(len(stuffed)),
        "n_line_bits": int(len(levels)),
        "stuff_overhead_bits": int(len(stuffed) - len(data_bits)),
    }
    return levels, info


def afsk1200_audio(levels: np.ndarray, sample_rate: float, *, prekey_s: float = 0.0) -> np.ndarray:
    """Real audio-domain waveform in [-1, 1]: `prekey_s` of silence, then continuous-phase Bell
    202 tones (mark :data:`MARK_HZ` at line level 1, space :data:`SPACE_HZ` at level 0)."""
    freq = np.where(np.asarray(levels, dtype=np.uint8) == 1, MARK_HZ, SPACE_HZ)
    n = int(round(len(freq) * sample_rate / BAUD))
    sym = np.minimum((np.arange(n) * BAUD / sample_rate).astype(np.int64), len(freq) - 1)
    phase = 2 * math.pi * np.cumsum(freq[sym]) / sample_rate
    tone = np.sin(phase)
    n_prekey = int(round(prekey_s * sample_rate))
    return np.concatenate([np.zeros(n_prekey), tone])
