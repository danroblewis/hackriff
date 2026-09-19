#!/usr/bin/env python3
"""Broadcast FM receiver: drives hackrf_transfer, writes 48 kHz s16le mono to stdout.

  python3 tools/fm_rx.py 101.3 | ffplay -f s16le -ar 48000 -ch_layout mono -nodisp -loglevel quiet -
  python3 tools/fm_rx.py 101.3 --seconds 10 | ffmpeg -f s16le -ar 48000 -ch_layout mono -i - fm.wav
"""
import argparse
import subprocess
import sys

import numpy as np
from scipy import signal

FS = 2_400_000
OFFSET = 250_000   # tune beside the station so the HackRF's DC spike isn't on top of it
CHUNK = 240_000    # 0.1 s; a multiple of 48 (the mixer's period) and of both decimation factors


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("mhz", type=float, help="station frequency in MHz, e.g. 101.3")
    p.add_argument("-l", "--lna", type=int, default=32, help="LNA gain 0-40 dB, 8 dB steps")
    p.add_argument("-g", "--vga", type=int, default=30, help="VGA gain 0-62 dB, 2 dB steps")
    p.add_argument("-a", "--amp", type=int, default=0, help="RF amp 0/1")
    p.add_argument("--deemph", type=float, default=75e-6, help="de-emphasis seconds (75e-6 Americas, 50e-6 elsewhere)")
    p.add_argument("--seconds", type=float, help="stop after this long")
    args = p.parse_args()
    if sys.stdout.isatty():
        p.error("stdout is a terminal; pipe into ffplay (see usage above)")

    cmd = ["hackrf_transfer", "-r", "-", "-f", str(int(args.mhz * 1e6) + OFFSET), "-s", str(FS),
           "-l", str(args.lna), "-g", str(args.vga), "-a", str(args.amp)]
    if args.seconds:
        cmd += ["-n", str(int(args.seconds * FS))]
    hackrf = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)

    # CHUNK is a whole number of mixer periods, so one precomputed mixer stays phase-continuous.
    mixer = np.exp(-2j * np.pi * OFFSET * np.arange(CHUNK) / FS).astype(np.complex64)
    b1 = signal.firwin(129, 110e3, fs=FS)           # FM channel, 2.4 Msps -> 240 ksps
    z1 = np.zeros(128, complex)
    b2 = signal.firwin(129, 15e3, fs=FS // 10)      # mono audio, 240 ksps -> 48 ksps
    z2 = np.zeros(128)
    a = np.exp(-1 / (48_000 * args.deemph))
    z3 = np.zeros(1)
    prev = 0j

    try:
        while True:
            raw = hackrf.stdout.read(2 * CHUNK)
            if len(raw) < 2 * CHUNK:
                break
            r = np.frombuffer(raw, np.int8).astype(np.float32)
            x = (r[0::2] + 1j * r[1::2]) * mixer
            y, z1 = signal.lfilter(b1, 1, x, zi=z1)
            y = y[::10]
            d = np.angle(y * np.conj(np.concatenate(([prev], y[:-1]))))
            prev = y[-1]
            au, z2 = signal.lfilter(b2, 1, d, zi=z2)
            au, z3 = signal.lfilter([1 - a], [1, -a], au[::5], zi=z3)
            pcm = np.clip(au * 16000, -32768, 32767).astype("<i2")
            sys.stdout.buffer.write(pcm.tobytes())
    except (BrokenPipeError, KeyboardInterrupt):
        pass
    finally:
        hackrf.terminate()
        hackrf.wait()


if __name__ == "__main__":
    main()
