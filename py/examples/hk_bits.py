#!/usr/bin/env python3
"""Print decoded bursts from a hackriff bits or symbols stream (standard library only).

Connect to ``hk serve``'s TCP stream server::

    HK_TOKEN=... python3 py/examples/hk_bits.py --port 8788              # every burst
    HK_TOKEN=... python3 py/examples/hk_bits.py --emitter <id> --symbols  # one emitter, soft symbols

or read a stream dumped by netcat (``-`` is stdin)::

    printf 'open/bits?token=%s\\n' "$HK_TOKEN" | nc 127.0.0.1 8788 > bursts.hkstream
    python3 py/examples/hk_bits.py --file bursts.hkstream

Each burst is a status record (framing metadata: sync and payload offsets, bit order, CRC)
followed by a data record (one byte per bit, or one float32 soft value per symbol).
"""

from __future__ import annotations

import argparse
import datetime as dt
import os
import struct
import sys
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import hkstream  # noqa: E402


@dataclass
class Burst:
    status: dict
    t_ns: int
    sample_index: int
    bits: bytes | None  # None when the content was withheld or gated

    @property
    def payload(self) -> bytes | None:
        s = self.status
        if self.bits is None or "payload_bit" not in s or "payload_bits" not in s:
            return None
        at, n = int(s["payload_bit"]), int(s["payload_bits"])
        return hkstream.pack_bits(self.bits[at:at + n], s.get("bit_order") != "lsb-first")


def bursts(reader: hkstream.StreamReader):
    """Pairs each status record with the data record that follows it."""
    header = reader.read_header()
    symbols = header.get("kind") == "symbols"
    status = None
    for rec in reader.records():
        if isinstance(rec, dict):
            continue
        if rec.type == hkstream.RECORD_STATUS:
            status = rec.status()
            if status.get("content_withheld"):
                yield Burst(status, rec.t_ns, rec.sample_index, None)
                status = None
        elif rec.type == hkstream.RECORD_DROPPED:
            print(f"# dropped {rec.dropped_count()} records from seq {rec.seq}", file=sys.stderr)
            status = None
        elif rec.type == hkstream.RECORD_DATA and status is not None:
            if rec.gated:
                bits = None
            elif symbols:
                soft = struct.unpack(f"<{len(rec.payload) // 4}f", rec.payload)
                bits = bytes(1 if v > 0 else 0 for v in soft)
            else:
                bits = rec.payload
            yield Burst(status, rec.t_ns, rec.sample_index, bits)
            status = None


def describe(b: Burst) -> str:
    s = b.status
    t = dt.datetime.fromtimestamp(b.t_ns / 1e9, dt.timezone.utc).isoformat(timespec="milliseconds")
    parts = [
        f"burst {s.get('burst')}",
        t,
        f"{s.get('symbol_rate_bd', 0):.1f} Bd",
        f"{s.get('f_center_hz', 0) / 1e6:.4f} MHz",
        f"{s.get('symbols')} symbols",
        f"crc={s.get('crc', 'none')}",
    ]
    if b.bits is None:
        parts.append("content withheld")
    elif b.payload is not None:
        parts.append(f"payload={b.payload.hex()}")
    else:
        parts.append(f"bits={hkstream.pack_bits(b.bits).hex()}")
    return "  ".join(parts)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=8788)
    p.add_argument("--token", default=os.environ.get("HK_TOKEN"))
    p.add_argument("--symbols", action="store_true", help="open/symbols instead of open/bits")
    p.add_argument("--emitter")
    p.add_argument("--f-lo", type=float)
    p.add_argument("--f-hi", type=float)
    p.add_argument("--count", type=int, default=0, help="stop after N bursts (0: run until the end)")
    p.add_argument("--file", help="read a dumped stream instead of connecting ('-' for stdin)")
    a = p.parse_args(argv)
    if a.file:
        f = sys.stdin.buffer if a.file == "-" else open(a.file, "rb")
    else:
        if not a.token:
            p.error("--token or HK_TOKEN is required")
        f = hkstream.connect(a.host, a.port, "open/symbols" if a.symbols else "open/bits", a.token,
                             emitter=a.emitter, f_lo=a.f_lo, f_hi=a.f_hi)
    reader = hkstream.StreamReader(f)
    try:
        h = reader.read_header()
    except hkstream.Refused as e:
        print(e, file=sys.stderr)
        return 1
    print(f"# {h['stream_id']} {h['kind']} {h.get('datatype')} class={h['content_class']}", file=sys.stderr)
    try:
        for n, b in enumerate(bursts(reader), 1):
            print(describe(b), flush=True)
            if a.count and n >= a.count:
                break
    except EOFError:
        print("# stream truncated", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
