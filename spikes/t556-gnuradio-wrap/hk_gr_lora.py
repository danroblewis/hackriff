#!/usr/bin/env python3
"""T-556 spike: gr-lora_sdr wrapped as a hackriff plugin (docs/stream-contract.md §9).

SPIKE CODE - not product, not in plugins/. Runs under GNU Radio's own Python (GPLv3 stays in
this subprocess, ADR-0010).

What this adapter has to do that hk-plugin-readsb does not (the point of the spike):

1. STDOUT DISCIPLINE. gr-lora_sdr's C++ blocks print to std::cout unconditionally (header
   dumps, CRC diagnostics, "netid" warnings). stdout is the §9.3 message plane, so any of those
   lines is a malformed-message count at best and a corrupted NDJSON line at worst. The adapter
   dup()s the real stdout to a private fd for NDJSON and dup2()s stderr over fd 1 BEFORE
   importing GNU Radio, so every C++ print lands in the host's stderr log ring.
2. FRAMING + SAMPLE FORMAT. Input is hackriff-v1 framed cf32_le (the channel DDC's native
   output, §12.3), which GNU Radio's gr_complex takes byte-for-byte - no conversion. A reader
   thread strips the §3/§5.2 framing and writes the payload into an os.pipe() that a C++
   blocks.file_descriptor_source reads, so no sample goes through a Python work() call.
3. HOST SAMPLE INDICES. GNU Radio numbers items from 0 at flowgraph start and has no idea of the
   host's stream index, drops or discontinuities. The reader keeps a piecewise map
   flowgraph-item-offset -> host sample_index, one segment per record, so a decode at flowgraph
   offset k is stamped with the HOST index even across dropped-record gaps. gr-lora_sdr itself
   drops the input offset on the floor (frame_sync tags frames in its OUTPUT domain and
   header_decoder builds a fresh dict), so this needs an 8-line PATCH to three GPL source files
   (patches/lora-sample-offset.patch) - i.e. a maintained fork. Without it the only available
   stamp is "samples written when the message arrived", which is exactly the wall-clock-style
   pitfall C22 warns about (it is off by the flowgraph's buffering, tens of ms and variable).
4. OUTPUT. The hier block's `msg` port carries only the payload string - no CRC flag, no time.
   So the adapter does not use the hier block's message port: it taps crc_verif's output byte
   stream with a small Python sink that reads the per-frame `frame_info` tag (pay_len,
   crc_valid, sample_offset) and emits one §9.3 `decode` line per frame.
5. READY + EOF. `{"type":"ready"}` once tb.start() has returned (interpreter up, OOT imported,
   scheduler threads running). On stdin EOF the reader closes the pipe, the fd source hits EOF,
   the flowgraph drains, and the adapter exits 0 (§9.5 end-of-input).
"""
import json
import os
import sys
import threading
import time

from hk_gr_host import T0, IndexMap, emit, log, reader  # noqa: I001 - FIRST: swaps stdout

import numpy as np  # noqa: E402
import pmt  # noqa: E402
from gnuradio import blocks, gr  # noqa: E402
import gnuradio.lora_sdr as lora_sdr  # noqa: E402

T_IMPORT = time.monotonic() - T0


class FrameSink(gr.sync_block):
    """(4) Reads crc_verif's payload bytes + frame_info tags; emits one decode line per frame."""

    def __init__(self, imap: IndexMap, preamble_symbols: float, sps: int, stats: dict,
                 sf: int) -> None:
        gr.sync_block.__init__(self, "hk_frame_sink", in_sig=[np.uint8], out_sig=None)
        self.imap, self.stats = imap, stats
        self.preamble_samples = int(round(preamble_symbols * sps))
        self.cur: dict | None = None
        self.sf = sf  # header_decoder's fresh dict drops sf; it is a configuration, not a measurement

    def _finish(self) -> None:
        cur, self.cur = self.cur, None
        info = cur["info"]
        off = info.get("sample_offset")
        # sample_offset is the header start in input items; the frame starts a preamble earlier.
        start_fg = None if off is None else max(0, off - self.preamble_samples)
        sidx = None if start_fg is None else self.imap.host(start_fg)
        crc = info.get("crc_valid")
        line = {"type": "decode", "frame_model": "lora-phy-explicit",
                "crc_status": "no-crc" if crc is None else ("valid" if crc else "invalid"),
                "metadata": {"sf": self.sf, "cr": info.get("cr"), "pay_len": info.get("pay_len"),
                             "ldro_mode": info.get("ldro_mode")},
                "content": {"payload_hex": bytes(cur["bytes"]).hex(),
                            "payload_text": bytes(cur["bytes"]).decode("latin-1")}}
        if sidx is not None:
            line["sample_index"] = sidx
        emit(line)
        self.stats["decodes"] += 1
        if self.stats["first_decode_t"] is None:
            self.stats["first_decode_t"] = time.monotonic() - T0
            log(f"first decode at +{self.stats['first_decode_t']:.3f}s")

    def work(self, input_items, output_items):
        data = input_items[0]
        base = self.nitems_read(0)
        tags = self.get_tags_in_window(0, 0, len(data), pmt.intern("frame_info"))
        tag_at = {t.offset - base: pmt.to_python(t.value) for t in tags}
        for i, b in enumerate(data):
            if i in tag_at:
                if self.cur is not None:
                    self._finish()
                self.cur = {"info": tag_at[i], "bytes": bytearray()}
            if self.cur is not None:
                self.cur["bytes"].append(int(b))
                if len(self.cur["bytes"]) >= int(self.cur["info"].get("pay_len", 0)):
                    self._finish()
        return len(data)


def main() -> int:
    args = dict(zip(sys.argv[1::2], sys.argv[2::2]))
    samp_rate = int(float(args.get("--rate", 250000)))
    bw = int(float(args.get("--bw", 125000)))
    sf = int(args.get("--sf", 7))
    center = int(float(args.get("--center", 868100000)))
    os_factor = samp_rate // bw
    sps = (1 << sf) * os_factor

    stats = {"decodes": 0, "markers": 0, "samples_in": 0, "first_sample_t": None,
             "first_decode_t": None, "header": None}
    imap = IndexMap()
    rfd, wfd = os.pipe()

    tb = gr.top_block("hk_gr_lora")
    src = blocks.file_descriptor_source(gr.sizeof_gr_complex, rfd, False)
    # The hier block's internals, not the hier block: we need crc_verif's output stream + tags.
    frame_sync = lora_sdr.frame_sync(center, bw, sf, False, [0x12], os_factor, 8)
    fft_demod = lora_sdr.fft_demod(False, True)
    gray = lora_sdr.gray_mapping(False)
    deint = lora_sdr.deinterleaver(False)
    hamming = lora_sdr.hamming_dec(False)
    header = lora_sdr.header_decoder(False, 1, 255, True, 2, False)
    dewhite = lora_sdr.dewhitening()
    crc = lora_sdr.crc_verif(0, False)  # 0 = print nothing
    sink = FrameSink(imap, preamble_symbols=8 + 2 + 2.25, sps=sps, stats=stats, sf=sf)
    tb.connect(src, frame_sync, fft_demod, gray, deint, hamming, header, dewhite, crc, sink)
    tb.msg_connect((header, "frame_info"), (frame_sync, "frame_info"))
    tb.start()

    t_ready = time.monotonic() - T0
    emit({"type": "ready"})
    log(f"ready: import {T_IMPORT:.3f}s, ready {t_ready:.3f}s (sf={sf} bw={bw} rate={samp_rate})")

    th = threading.Thread(target=reader, args=(sys.stdin.buffer, wfd, imap, stats), daemon=True)
    th.start()
    th.join()
    tb.wait()  # (5) flowgraph drains after pipe EOF
    log(f"eof: {stats['samples_in']} samples, {stats['decodes']} decodes, "
        f"{imap.discontinuities} discontinuities, {stats['markers']} markers")
    emit({"type": "log", "msg": json.dumps({"t556_stats": stats, "import_s": T_IMPORT,
                                            "ready_s": t_ready})})
    return 0


if __name__ == "__main__":
    sys.exit(main())
