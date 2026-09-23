#!/usr/bin/env python3
"""T-556 spike: stereo int16 IQ WAV (satellite-recordings) -> cf32 + a PARITY truth list.

The truth is what the stock, unwrapped `gr_satellites` CLI decodes from the same file
(--hexdump), so a wrapped run is scored for parity with the upstream tool: same frames, no more,
no fewer. (A parity check, not a blind detection test - the wrapper adds no detection.)
Usage: sat_fixture.py <in.wav> <out-stem> <satellite> <rate>
"""
import json, os, re, subprocess, sys, wave
import numpy as np
wav, stem, sat, rate = sys.argv[1], sys.argv[2], sys.argv[3], float(sys.argv[4])
w = wave.open(wav)
assert w.getnchannels() == 2 and w.getsampwidth() == 2 and w.getframerate() == rate
x = np.frombuffer(w.readframes(w.getnframes()), "<i2").reshape(-1, 2).astype(np.float32) / 32768
(x[:, 0] + 1j * x[:, 1]).astype("<c8").tofile(stem + ".cf32")
cli = os.path.join(os.environ["T556_WORK"], "prefix/bin/gr_satellites")
out = subprocess.run([sys.executable, cli, sat, "--wavfile", wav, "--iq", "--samp_rate", str(rate),
                      "--hexdump"], capture_output=True, text=True).stdout
pdus = []
for blk in out.split("pdu vector contents = ")[1:]:
    rows = re.findall(r"^[0-9a-f]{4}: ((?:[0-9a-f]{2} ?)+)", blk, re.M)
    pdus.append("".join(r.replace(" ", "") for r in rows))
json.dump({"sample_rate_hz": rate, "satellite": sat, "samples": len(x),
           "duration_s": len(x) / rate, "payloads_hex": pdus, "source": os.path.basename(wav)},
          open(stem + ".truth.json", "w"), indent=1)
print(f"{stem}: {len(x)} samples ({len(x)/rate:.1f} s), stock CLI decoded {len(pdus)} PDUs")
