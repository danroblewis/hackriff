#!/usr/bin/env python3
"""T-556 spike: the decoder-independent half of a wrapped GNU Radio decoder.

Shared by hk_gr_lora.py and hk_gr_satellites.py - it is what "the pattern" is, and its size is
the part of the wrapping cost that the SECOND decoder does not pay again. IMPORT THIS FIRST:
importing it performs the stdout swap (see hk_gr_lora.py's docstring, point 1) before any
GNU Radio C++ is loaded.
"""
import json
import os
import struct
import sys
import threading
import time

T0 = time.monotonic()
TAG = os.path.basename(sys.argv[0]).removesuffix(".py")

# (1) stdout discipline - before anything that could load C++ that prints.
_NDJSON_FD = os.dup(1)
os.dup2(2, 1)
_out = os.fdopen(_NDJSON_FD, "w", buffering=1)
_out_lock = threading.Lock()


def emit(obj: dict) -> None:
    line = json.dumps(obj, separators=(",", ":"))
    with _out_lock:
        _out.write(line + "\n")
        _out.flush()


def log(msg: str) -> None:
    print(f"[{TAG} +{time.monotonic() - T0:.3f}s] {msg}", file=sys.stderr, flush=True)


REC_HDR = struct.Struct("<BBHIQqQ")  # §5.2: type, flags, rsvd, len, seq, t_ns, sample_index
ITEM = 8  # cf32_le == gr_complex


class IndexMap:
    """Flowgraph item offset -> host stream sample_index, piecewise per record."""

    def __init__(self) -> None:
        self._segs: list[tuple[int, int]] = []  # (fg_offset, host_index), fg_offset ascending
        self._lock = threading.Lock()
        self.discontinuities = 0

    def add(self, fg_offset: int, host_index: int) -> None:
        with self._lock:
            if self._segs:
                last_fg, last_host = self._segs[-1]
                if host_index - last_host == fg_offset - last_fg:
                    return  # contiguous: the previous segment already covers it
                self.discontinuities += 1
            self._segs.append((fg_offset, host_index))

    def host(self, fg_offset: int) -> int | None:
        with self._lock:
            lo, hi = 0, len(self._segs) - 1
            if hi < 0 or fg_offset < self._segs[0][0]:
                return None
            while lo < hi:
                mid = (lo + hi + 1) // 2
                if self._segs[mid][0] <= fg_offset:
                    lo = mid
                else:
                    hi = mid - 1
            fg, host = self._segs[lo]
            return host + (fg_offset - fg)


def read_exact(f, n: int) -> bytes | None:
    buf = bytearray()
    while len(buf) < n:
        chunk = f.read(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return bytes(buf)


def reader(stdin, wfd: int, imap: IndexMap, stats: dict) -> None:
    """(2) Strip hackriff-v1 framing, feed payload to the flowgraph pipe, record the index map."""
    fg_offset = 0
    try:
        n = read_exact(stdin, 4)
        if n is None:
            return
        header = json.loads(read_exact(stdin, struct.unpack("<I", n)[0]))
        if header.get("datatype") != "cf32_le":
            log(f"refusing datatype {header.get('datatype')!r}; manifest declares cf32_le")
            return
        stats["header"] = {k: header.get(k) for k in ("sample_rate_hz", "center_hz", "datatype")}
        while True:
            n = read_exact(stdin, 4)
            if n is None:
                return
            frame = read_exact(stdin, struct.unpack("<I", n)[0])
            if frame is None or len(frame) < REC_HDR.size:
                return
            rtype, flags, _, plen, _seq, _t_ns, sidx = REC_HDR.unpack_from(frame)
            if rtype != 1:  # dropped marker / status: the next data record's index says where
                stats["markers"] += 1
                continue
            payload = memoryview(frame)[REC_HDR.size:REC_HDR.size + plen]
            imap.add(fg_offset, sidx)
            os.write(wfd, payload)  # blocking write == backpressure onto the host's bounded queue
            fg_offset += plen // ITEM
            stats["samples_in"] = fg_offset
            if stats["first_sample_t"] is None:
                stats["first_sample_t"] = time.monotonic() - T0
    except BrokenPipeError:
        pass
    finally:
        os.close(wfd)  # EOF to the flowgraph -> it drains and exits


