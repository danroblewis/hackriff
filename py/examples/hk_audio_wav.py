#!/usr/bin/env python3
"""Write a hackriff Listen audio stream to a WAV file (standard library only).

Listen picks the mode and parameters from the signal; you only say what to listen to::

    HK_TOKEN=... python3 py/examples/hk_audio_wav.py --emitter <id> --seconds 10 --out station.wav
    HK_TOKEN=... python3 py/examples/hk_audio_wav.py --f-lo 101.2e6 --f-hi 101.4e6 --out fm.wav

Audio records are 20 ms of mono ``int16`` at 48 kHz. A jump in ``sample_index`` (squelch closed,
samples skipped to stay live) is filled with silence, up to one second per gap, so the file keeps
real time. Status records (level, SNR, squelch) are printed to stderr.
"""

from __future__ import annotations

import argparse
import os
import sys
import wave
from pathlib import Path
from typing import BinaryIO

sys.path.insert(0, str(Path(__file__).resolve().parent))

import hkstream  # noqa: E402

MAX_GAP_S = 1.0


def write_wav(reader: hkstream.StreamReader, out: BinaryIO | str, seconds: float,
              log=sys.stderr) -> int:
    """Writes up to ``seconds`` of audio; returns the samples written."""
    h = reader.read_header()
    if h.get("kind") != "audio" or h.get("datatype") != "ri16_le":
        raise ValueError(f"not an ri16_le audio stream: {h.get('kind')} {h.get('datatype')}")
    rate = int(h.get("sample_rate_hz", 48000))
    channels = int((h.get("audio") or {}).get("channels", 1))
    want = int(seconds * rate)
    written, next_index = 0, None
    with wave.open(out, "wb") as w:
        w.setnchannels(channels)
        w.setsampwidth(2)
        w.setframerate(rate)
        for rec in reader.records():
            if rec.type == hkstream.RECORD_STATUS:
                print(f"# status {rec.status()}", file=log)
                continue
            if rec.type == hkstream.RECORD_DROPPED:
                print(f"# dropped {rec.dropped_count()} records", file=log)
                continue
            if rec.type != hkstream.RECORD_DATA or rec.gated:
                continue
            if next_index is not None and rec.sample_index > next_index:
                gap = min(rec.sample_index - next_index, int(MAX_GAP_S * rate))
                w.writeframes(b"\x00\x00" * channels * gap)
                written += gap
            frames = len(rec.payload) // (2 * channels)
            w.writeframes(rec.payload[: frames * 2 * channels])
            written += frames
            next_index = rec.sample_index + frames
            if written >= want:
                break
    return written


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=8788)
    p.add_argument("--token", default=os.environ.get("HK_TOKEN"))
    p.add_argument("--emitter")
    p.add_argument("--detection")
    p.add_argument("--f-lo", type=float)
    p.add_argument("--f-hi", type=float)
    p.add_argument("--seconds", type=float, default=10.0)
    p.add_argument("--out", default="listen.wav")
    p.add_argument("--file", help="read a dumped stream instead of connecting ('-' for stdin)")
    a = p.parse_args(argv)
    if a.file:
        f = sys.stdin.buffer if a.file == "-" else open(a.file, "rb")
    else:
        if not a.token:
            p.error("--token or HK_TOKEN is required")
        f = hkstream.connect(a.host, a.port, "open/listen", a.token, emitter=a.emitter,
                             detection=a.detection, f_lo=a.f_lo, f_hi=a.f_hi)
    try:
        n = write_wav(hkstream.StreamReader(f), a.out, a.seconds)
    except hkstream.Refused as e:
        print(e, file=sys.stderr)
        return 1
    print(f"wrote {n} samples to {a.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
