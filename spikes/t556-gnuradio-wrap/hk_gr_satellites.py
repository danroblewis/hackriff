#!/usr/bin/env python3
"""T-556 spike: gr-satellites (Daniel Estevez, GPL-3.0) wrapped as a hackriff plugin.

SPIKE CODE. The SECOND wrapper: written after hk_gr_lora.py established the pattern, and timed
(see README §4). Everything decoder-independent is in hk_gr_host.py; this file is the delta.

Decoder-specific facts this wrapper had to find out:
- gr-satellites is ~all Python flowgraph assembled at run time from a per-satellite SatYAML
  (name/NORAD -> demodulator + deframer + transports). `grc_block=True` gives one `out`
  message port of PDUs whose only metadata is `transmitter`.
- NO SAMPLE TIME. PDUs are born in the bit domain after clock recovery (symbol_sync resamples
  by a tracked, not fixed, ratio), across ~60 deframer implementations. There is no single
  frame_info-like tag to patch as there was in gr-lora_sdr. So this wrapper emits decode lines
  WITHOUT sample_index, and the host stamps them with arrival time (§9.3: "Lines without
  sample_index get host arrival time") - the C22 pitfall, accepted and labelled, not hidden:
  metadata.time_source = "arrival". Exact stamping would need input-domain tags propagated
  through every rate-changing block and read in each deframer - a fork of the ecosystem's
  flagship across dozens of files (README §2.3).
- CRC: gr-satellites only forwards frames its deframer accepted (CRC/RS/checksum per
  satellite), but which check applied is not in the PDU. crc_status is therefore "unknown",
  never a claimed "valid".
- Imports: `import satellites` eagerly imports every module, including zmq, requests and
  construct (undeclared in the build; pip-installed into a private target dir).
"""
import json
import os
import sys
import threading
import time

from hk_gr_host import T0, IndexMap, emit, log, reader  # noqa: I001 - FIRST: swaps stdout

import pmt  # noqa: E402
from gnuradio import blocks, gr  # noqa: E402
from satellites.core import gr_satellites_flowgraph  # noqa: E402

T_IMPORT = time.monotonic() - T0


class PduSink(gr.basic_block):
    def __init__(self, sat: str, stats: dict) -> None:
        gr.basic_block.__init__(self, "hk_pdu_sink", in_sig=None, out_sig=None)
        self.sat, self.stats = sat, stats
        self.message_port_register_in(pmt.intern("in"))
        self.set_msg_handler(pmt.intern("in"), self.handle)

    def handle(self, msg) -> None:
        meta = pmt.to_python(pmt.car(msg)) or {}
        payload = bytes(pmt.u8vector_elements(pmt.cdr(msg)))
        emit({"type": "decode", "frame_model": "gr-satellites",
              "crc_status": "unknown",
              "metadata": {"satellite": self.sat, "transmitter": str(meta.get("transmitter")),
                           "length": len(payload), "time_source": "arrival"},
              "content": {"payload_hex": payload.hex()}})
        self.stats["decodes"] += 1
        if self.stats["first_decode_t"] is None:
            self.stats["first_decode_t"] = time.monotonic() - T0
            log(f"first decode at +{self.stats['first_decode_t']:.3f}s")


def main() -> int:
    args = dict(zip(sys.argv[1::2], sys.argv[2::2]))
    samp_rate = float(args.get("--rate", 48000))
    sat = args.get("--satellite", "NuSat 1")
    stats = {"decodes": 0, "markers": 0, "samples_in": 0, "first_sample_t": None,
             "first_decode_t": None, "header": None}
    imap = IndexMap()  # kept for drop accounting; there is nothing to map a PDU back through
    rfd, wfd = os.pipe()

    tb = gr.top_block("hk_gr_satellites")
    src = blocks.file_descriptor_source(gr.sizeof_gr_complex, rfd, False)
    fg = gr_satellites_flowgraph(name=sat, samp_rate=samp_rate, iq=True, grc_block=True)
    sink = PduSink(sat, stats)
    tb.connect(src, fg)
    tb.msg_connect((fg, "out"), (sink, "in"))
    tb.start()
    t_ready = time.monotonic() - T0
    emit({"type": "ready"})
    log(f"ready: import {T_IMPORT:.3f}s, ready {t_ready:.3f}s ({sat} @ {samp_rate:g} S/s)")

    th = threading.Thread(target=reader, args=(sys.stdin.buffer, wfd, imap, stats), daemon=True)
    th.start()
    th.join()
    tb.wait()
    log(f"eof: {stats['samples_in']} samples, {stats['decodes']} decodes")
    emit({"type": "log", "msg": json.dumps({"t556_stats": stats, "import_s": T_IMPORT,
                                            "ready_s": t_ready})})
    return 0


if __name__ == "__main__":
    sys.exit(main())
