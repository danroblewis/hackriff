#!/usr/bin/env python3
"""T-556 spike, question 5: what does ONE process hosting several GNU Radio flowgraphs save?

`shared_host_probe.py bare|lora|sat|both` builds and STARTS the named flowgraph(s) on idle pipe
sources (the same graphs the wrappers build), then reports time-to-started and maxrss. The
difference between `both` and `lora`+`sat` is the most a shared "GNU Radio host" plugin could
amortise; `bare` is the runtime floor every per-decoder process pays.
"""
import json, os, resource, sys, time
t0 = time.monotonic()
import numpy  # noqa: F401,E402
from gnuradio import blocks, gr  # noqa: E402
what = sys.argv[1]
tb = gr.top_block()
def fd_src():
    r, _w = os.pipe()
    return blocks.file_descriptor_source(gr.sizeof_gr_complex, r, False)
if what == "bare":
    tb.connect(fd_src(), blocks.null_sink(gr.sizeof_gr_complex))
if what in ("lora", "both"):
    import gnuradio.lora_sdr as l
    fs = l.frame_sync(868100000, 125000, 7, False, [0x12], 2, 8)
    chain = [fs, l.fft_demod(False, True), l.gray_mapping(False), l.deinterleaver(False),
             l.hamming_dec(False), l.header_decoder(False, 1, 255, True, 2, False),
             l.dewhitening(), l.crc_verif(0, False), blocks.null_sink(1)]
    tb.connect(fd_src(), *chain)
    tb.msg_connect((chain[5], "frame_info"), (fs, "frame_info"))
if what in ("sat", "both"):
    from satellites.core import gr_satellites_flowgraph
    tb.connect(fd_src(), gr_satellites_flowgraph(name="NuSat 1", samp_rate=192000, iq=True,
                                                 grc_block=True))
tb.start()
t = time.monotonic() - t0
time.sleep(0.5)
ru = resource.getrusage(resource.RUSAGE_SELF)
print(json.dumps({"what": what, "started_s": round(t, 3), "maxrss_mib": round(ru.ru_maxrss / 2**20, 1),
                  "threads": len(os.popen(f"ps -M -p {os.getpid()}").read().splitlines()) - 1}), flush=True)
os._exit(0)
