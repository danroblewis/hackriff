"""Minimal reader for the hackriff stream contract (docs/stream-contract.md), standard library only.

Use it as a starting point for your own consumer; it is not on any real-time path.

Framing: every frame is a ``u32`` little-endian length followed by the payload. The first frame is
the JSON stream header (``"schema": "hackriff.stream"``), or, on the TCP stream server, a refusal
object (``"type": "refused"``). Messages streams then carry one NDJSON record per frame; binary
streams (bits, symbols, iq, audio, spectrum) carry a 32-byte record header plus payload.

TCP handshake (``hk serve`` prints the address; discovery is ``GET /api/streams``)::

    <stream_id>?token=<token>\\n            always-on stream, e.g. spectrum/live
    open/<name>?token=<token>[&k=v]\\n       on-demand stream: open/bits, open/symbols, open/listen
"""

from __future__ import annotations

import json
import socket
import struct
from dataclasses import dataclass
from typing import BinaryIO, Iterator
from urllib.parse import quote

RECORD_DATA = 1
RECORD_DROPPED = 2
RECORD_STATUS = 3

FLAG_GATED = 1 << 0
FLAG_DISCONTINUITY = 1 << 1
FLAG_OVERLOAD = 1 << 2
FLAG_BURST_START = 1 << 3
FLAG_BURST_END = 1 << 4

HEADER_MAX_LEN = 64 * 1024
PROTOCOL_MAX_FRAME_LEN = 4 * 1024 * 1024
RECORD_HEADER = struct.Struct("<BBHIQqQ")  # type, flags, reserved, len, seq, t_ns, sample_index
BINARY_KINDS = {"bits", "symbols", "iq", "audio", "spectrum"}


class Refused(Exception):
    """The server refused the request (the first frame was a refusal object)."""

    def __init__(self, obj: dict):
        self.status = obj.get("status")
        self.code = obj.get("code")
        self.reason = obj.get("reason")
        self.content_class = obj.get("content_class")
        super().__init__(f"refused {self.status} {self.code}: {self.reason}")


@dataclass
class BinaryRecord:
    """One binary record (data, dropped marker or status)."""

    type: int
    flags: int
    payload_len: int
    seq: int
    t_ns: int
    sample_index: int
    payload: bytes

    @property
    def gated(self) -> bool:
        return bool(self.flags & FLAG_GATED)

    @property
    def discontinuity(self) -> bool:
        return bool(self.flags & FLAG_DISCONTINUITY)

    def status(self) -> dict | None:
        """The JSON object of a status record."""
        return json.loads(self.payload) if self.type == RECORD_STATUS else None

    def dropped_count(self) -> int:
        """How many records a dropped marker reports (``seq`` is the first one)."""
        return struct.unpack_from("<Q", self.payload)[0] if self.type == RECORD_DROPPED else 0


def _read_exact(f: BinaryIO, n: int) -> bytes | None:
    """``n`` bytes, ``None`` at a clean end of stream; raises ``EOFError`` inside a frame."""
    chunks, got = [], 0
    while got < n:
        b = f.read(n - got)
        if not b:
            if got == 0:
                return None
            raise EOFError("stream truncated inside a frame")
        chunks.append(b)
        got += len(b)
    return b"".join(chunks)


def read_frame(f: BinaryIO, max_len: int) -> bytes | None:
    """One frame payload, ``None`` at the end of the stream."""
    prefix = _read_exact(f, 4)
    if prefix is None:
        return None
    (length,) = struct.unpack("<I", prefix)
    if length > max_len:
        raise ValueError(f"frame of {length} bytes exceeds the limit {max_len}: disconnect")
    payload = _read_exact(f, length)
    if payload is None:
        raise EOFError("stream truncated after a length prefix")
    return payload


def parse_binary_record(frame: bytes) -> BinaryRecord:
    if len(frame) < RECORD_HEADER.size:
        raise ValueError("binary record shorter than its 32-byte header")
    rtype, flags, _, plen, seq, t_ns, index = RECORD_HEADER.unpack_from(frame)
    return BinaryRecord(rtype, flags, plen, seq, t_ns, index, frame[RECORD_HEADER.size:])


class StreamReader:
    """Reads the header, then yields records (``BinaryRecord`` or message ``dict``)."""

    def __init__(self, f: BinaryIO):
        self.f = f
        self.header: dict | None = None

    def read_header(self) -> dict:
        if self.header is None:
            frame = read_frame(self.f, HEADER_MAX_LEN)
            if frame is None:
                raise EOFError("stream closed before its header")
            obj = json.loads(frame)
            if obj.get("type") == "refused":
                raise Refused(obj)
            if obj.get("schema") != "hackriff.stream":
                raise ValueError(f"not a hackriff stream header: {obj}")
            if str(obj.get("version", "")).split(".")[0] != "1":
                raise ValueError(f"unsupported major version {obj.get('version')}")
            self.header = obj
        return self.header

    def records(self) -> Iterator[BinaryRecord | dict]:
        header = self.read_header()
        limit = min(int(header.get("max_frame_len", PROTOCOL_MAX_FRAME_LEN)) + 64,
                    PROTOCOL_MAX_FRAME_LEN + 64)
        binary = header.get("kind") in BINARY_KINDS
        while True:
            frame = read_frame(self.f, limit)
            if frame is None:
                return
            yield parse_binary_record(frame) if binary else json.loads(frame)


def handshake_line(target: str, token: str, **params) -> bytes:
    query = "".join(f"&{quote(str(k))}={quote(str(v))}" for k, v in params.items() if v is not None)
    return f"{target}?token={quote(token, safe='')}{query}\n".encode()


def connect(host: str, port: int, target: str, token: str, timeout: float | None = None,
            **params) -> BinaryIO:
    """Connects to the TCP stream server and returns a binary file for ``StreamReader``.

    Keep the returned file open while reading: closing it (or sending anything) ends the stream.
    """
    sock = socket.create_connection((host, port), timeout=timeout)
    sock.sendall(handshake_line(target, token, **params))
    return sock.makefile("rb")


def pack_bits(bits: bytes, msb_first: bool = True) -> bytes:
    """Packs one-byte-per-bit values (0/1) into bytes; a trailing partial byte is dropped."""
    out = bytearray()
    for i in range(0, len(bits) - len(bits) % 8, 8):
        byte = 0
        for k, b in enumerate(bits[i:i + 8]):
            byte |= (b & 1) << (7 - k if msb_first else k)
        out.append(byte)
    return bytes(out)
