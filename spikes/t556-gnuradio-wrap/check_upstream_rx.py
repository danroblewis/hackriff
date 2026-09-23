#!/usr/bin/env python3
"""T-556 spike: run a fixture through gr-lora_sdr's OWN unmodified-API hier block (lora_rx), no
wrapper, and list what it decodes - shows the hk04 miss is upstream, not the adapter.
Usage: check_upstream_rx.py <fixture-stem>"""
import sys
from gnuradio import blocks, gr
import gnuradio.lora_sdr as lora_sdr
tb = gr.top_block()
src = blocks.file_source(gr.sizeof_gr_complex, sys.argv[1] + ".cf32", False)
rx = lora_sdr.lora_sdr_lora_rx(center_freq=868100000, bw=125000, cr=1, has_crc=True,
                               impl_head=False, pay_len=255, samp_rate=250000, sf=7,
                               sync_word=[0x12], soft_decoding=False, ldro_mode=2,
                               print_rx=[False, True])  # prints "rx msg: <payload>" per frame
tb.connect(src, rx, blocks.null_sink(1))
tb.run()
