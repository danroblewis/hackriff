# T-556 spike: what a wrapped GNU Radio OOT decoder actually costs

**Throwaway spike code. Not product, not in `plugins/`, not in the workspace.** It answers
docs/18 §2.4's open question with measurements. Use cases: **SIGNAL-053** (LoRa, gr-lora_sdr) and
**SIGNAL-034** (cubesat telemetry, gr-satellites). Measured 2026-09-22 on the dev Mac (M3 Ultra,
28 cores, macOS 15, Homebrew GNU Radio 3.10.12.0), under **load average 11–67** from other agents.
The raw numbers are in `results/`.

## Verdict

**Runtime cost is not the problem. A wrapped decoder is cheap to run and cheap to get decoding.**

| | gr-lora_sdr | gr-satellites |
|---|---|---|
| Private memory (`phys_footprint`) | 27 MB | 55 MB |
| RSS / peak RSS | 43 / 45 MiB | 80 / 81 MiB |
| CPU at real time | 2.4–3.0 % of one M3 core at 250 kS/s | 11.5 % at 192 kS/s |
| Warm start-up to ready | median 0.31 s | median 1.6 s |

What is expensive is everything *around* the decode: exact time stamps, the build matrix, and the
toolchain. Worst of all is what a wrapped decoder cannot tell the rest of the system.

**Recommendation: use the GNU Radio reference set as a *specification source* for native recipes.
Keep wrapped GNU Radio decoders as a narrow, opt-in tier, not as the coverage roadmap's backbone.**
A wrapped GR decoder earns a place only if all three hold:
1. no native recipe is planned;
2. the OOT has a single framing point where host sample indices can be carried through (gr-lora_sdr
   does; gr-satellites does not);
3. its parity against the stock tool is under test.

Reasons, in order of weight:
1. **Time stamping needs a fork, or is not feasible.** The invariant is that every time-varying
   record carries absolute capture time (CLAUDE.md: signal boxes on one time axis).
   - gr-lora_sdr needed an **8-line patch to three GPL source files** to get there.
   - gr-satellites has **no patchable point**: frames are born in the bit domain after
     variable-ratio clock recovery, across ~60 deframers. Its decodes can only be
     **arrival-stamped**. During a faster-than-real-time replay that stamp is meaningless (all
     11 NuSat frames arrived within 0.2 s of wall time).
2. **The Jetson install is a separate ~1.1 GB, 375-package closure with its own version skew.**
   It ships GNU Radio 3.10.1 and pybind11 2.9 against the Mac's 3.10.12 and pybind11 3.1. That
   means two build matrices for every OOT. The Mac build alone failed twice on ABI skew before it
   imported (§1).
3. **Opaque to synthesis (docs/18 point 3), and upstream bugs cannot be fixed.** gr-lora_sdr
   loses one of 12 synthetic frames (`hk04`, deterministically, whatever the SNR, and also in its
   own unmodified `lora_rx` hier block) whenever certain frames precede it. Fixing that is more
   fork work.
4. **A shared "GNU Radio host" does not change the slope** (§5). It saves about one runtime floor
   (~38 MiB RSS, ~0.2–0.5 s) per extra decoder and costs fault isolation plus a contract change.

## 1. Build: what it takes to get it running at all

**Mac (run).** Homebrew GNU Radio 3.10.12 was already installed. Both OOTs build against it, but
not by following the READMEs. It took **three attempts for gr-lora_sdr** before it imported.
`build.sh` encodes every workaround:

| Attempt | Failure | Cause |
|---|---|---|
| 1 | `Library not loaded: @rpath/libfmt.11.dylib` | The user's Anaconda on `PATH` supplied `fmt_DIR` (fmt 11); GNU Radio links Homebrew fmt 12 |
| 2 | `ImportError: generic_type: type "add_crc" referenced unknown base type "gr::block"` | The gnuradio bottle was built with pybind11 3.1 (internals `v12`); Homebrew's installed pybind11 is 3.0.2 (`v11`), so they are two type registries |
| 3 | OK | Clean `PATH` + private pybind11 3.1.0 |

gr-satellites then needed:
- `construct`, `requests` and `websocket-client` (documented);
- **`pyzmq` (not documented)**: `import satellites` eagerly imports every module, including
  `zmq`.

These are installed into a private `--target` directory; the Homebrew venv is untouched.

| Step | gr-lora_sdr | gr-satellites |
|---|---|---|
| clone | 2.1 MB | 15 MB |
| configure | 12 s | 105 s |
| build (6–8 jobs, load ~20–60) | 83–159 s | 224 s |
| installed OOT | 0.9 MB | (with Python) ~5 MB |
| extra Python deps | none | 10 MB (`pydeps/`) |

**Dependency closure, Mac.** The Homebrew `gnuradio` closure is **1.89 GiB across 90 packages**
(`results/brew-closure-kib.txt`). The largest are gcc 481 MiB, boost 377, qt@5 208, python 86 and
icu4c 84, most of it GUI and build-time. What the running adapter **actually maps** is **60
non-system images, 45.7 MiB** (`results/runtime-images.txt`). OpenBLAS is 15 MiB of that
(numpy), and `gr-blocks` drags in the whole audio-codec stack (sndfile, lame, mpg123, vorbis,
opus, flac) for a LoRa decoder.

**aarch64 / JetPack 6 (assessed, not run).** Docker was not running, and starting it on a shared
machine under load 60 was not justified. So the assessment is computed from the real Ubuntu 22.04
(Jammy, JetPack 6's userland) `arm64` package indices from `ports.ubuntu.com`
(`results/jammy-arm64-apt-closure.txt`):
- GNU Radio **3.10.1.1** and pybind11 **2.9.1**. The OOTs require 3.10, so they should build,
  but against a different pybind11 major and GR point release than the Mac, which means a second
  build matrix and CI job.
- `apt install gnuradio` is **the only apt route to the Python bindings**. Its Depends-only
  closure beyond a python3 base is **375 packages, 1,085 MiB installed**, including **60
  GUI/GL packages** (mesa, GTK, Qt5, pyqtgraph, OpenGL), because the bindings ship in the same
  package as GRC.
- Building an OOT on-device adds `gnuradio-dev`, `pybind11-dev`, `cmake` and `g++`: **425
  packages, 1,230 MiB**.
- The C++ runtime libraries alone (runtime, pmt, blocks, analog, digital, fft, filter) are 42
  packages and **79 MiB**, but they cannot run a Python flowgraph.
- A headless, minimal image means building GNU Radio from source for aarch64 (or conda-forge
  `linux-aarch64`, **unverified**). That is its own day of work, repeated at every GR upgrade.
- **Unverified:** CPU cost on the Orin's Cortex-A78AE. The M3 numbers below are a lower bound;
  expect several times the per-core cost.

**"Not practical to ship on a handheld?"** It is practical, as one ~1 GB, version-pinned,
separately-built runtime. It is not cheap: it is the largest single component the image would
carry, and it is paid once for any number of wrapped GR decoders (§5).

## 2. The wire contract: what the adapter does that `hk-plugin-readsb` does not

`manifest.json` is valid per plugins/README.md: it loads through `PluginManifest::load` and ran
under the real host (§3).
- Input: `kind: iq`, `datatype: cf32_le`, `hackriff-v1` framing, `ready_signal: true`.
- Output: NDJSON `decode` lines, `schema_id: hackriff.lora/1`, `content_class: unrestricted`.

The adapter is `hk_gr_host.py` (shared, 118 lines) plus `hk_gr_lora.py` (147 lines) or
`hk_gr_satellites.py` (92 lines). What it has to do:

1. **Stdout discipline (new).** gr-lora_sdr's C++ blocks write `std::cout` unconditionally
   (header dumps, CRC diagnostics, `netid` warnings), and stdout is the §9.3 message plane. The
   adapter `dup()`s stdout to a private fd and `dup2()`s stderr over fd 1 **before importing
   GNU Radio**. Every C++ print then lands in the host's stderr log ring: 0 malformed lines in
   every run. readsb needed no such step; it has its own output channel.
2. **Sample format: no conversion.** The host's channel datatype `cf32_le` is exactly
   `gr_complex`. readsb needs ci8→UC8; GNU Radio needs nothing.
   - The payload goes straight into an `os.pipe()` read by the C++ `file_descriptor_source`, so
     no sample passes through a Python `work()`.
   - The blocking pipe write is the backpressure. A **Python** block on the sample path is avoided
     deliberately. The only Python block is the sink on the decoded-byte stream.
3. **Ready signal: simpler than readsb.** Ready is emitted once `tb.start()` has returned.
   There is no Beast socket and no pre-roll (readsb's T-223 machinery).
   - Warm start: 0.29–0.35 s (LoRa), 1.3–2.6 s (gr-satellites: the `import satellites`
     eager-import is 1.3 s of it).
   - **Cold start (first launch after the OOT is relinked): 4.3–11.6 s** at load 63–67,
     reproduced twice (`results/startup-after-relink.json`). That covers 1.4 s of exec and
     interpreter start plus 2.9 s of imports on one run, 11.6 s total on the other.
   - This works with T-629's rule only because the adapter writes **no byte before ready**. The
     cold import is then charged to `startup_timeout` (60 s), not `ready_timeout` (5 s).
     **An early "starting…" log line would start the 5 s clock and fail the cold start.** Every
     Python wrapper must follow that rule.
4. **Re-stamping from host sample indices (the C22 pitfall): the hard part.** GNU Radio counts
   items from 0 at flowgraph start and knows nothing of the host's index, drops or
   discontinuities.
   - The adapter keeps a **piecewise map from flowgraph item offset to host `sample_index`**, one
     segment per record (§5.2 header). It is exact across dropped records: 7/7 and 21/21 frames
     with dropped records in between, error ≤ 2 samples.
   - **But the decoder has to tell the adapter the input offset of each frame, and neither OOT
     does:**
     - **gr-lora_sdr:** `frame_sync` tags frames in its *output* (symbol) domain, and
       `header_decoder` rebuilds the tag dict, dropping everything. The fix is
       `patches/lora-sample-offset.patch`: 3 files, +8 lines, carrying
       `nitems_read(0)` + the consumed offset through as `sample_offset`. Result: **host
       `t_ns` within −16 µs…0 of truth** (−4…0 samples at 250 kS/s; tolerance was ±512 µs).
       That is a maintained fork of a GPL project, rebased at every upstream release.
     - **gr-satellites:** PDUs leave ~60 deframers in the bit domain after `symbol_sync`, whose
       resampling ratio is tracked, not fixed. There is no single place to patch. The wrapper
       omits `sample_index`, the host stamps **arrival time** (§9.3), and the line says so
       (`metadata.time_source: "arrival"`). Exact stamping would need input-domain tags
       propagated through every rate-changing block, read in each deframer: a fork of the
       ecosystem's flagship project. **Not feasible for a spike, and not attractive for a
       product.**
   - Under a restricted `content_class` an arrival-stamped line is *allowed*, because §9.3 bounds
     only lines that carry a `sample_index`. But it places the decode wherever the flowgraph's
     buffering put it.
5. **Output: tag-reading sink instead of the message port.** The hier block's `msg` port carries
   only the payload string: no CRC flag, no length, no time. The LoRa adapter builds the RX chain
   itself and taps `crc_verif`'s byte stream plus its `frame_info` tags, which give `crc_valid`,
   `pay_len`, `cr` and `sample_offset`.
   - gr-satellites forwards only frames its deframer accepted, but which check applied (CRC,
     Reed-Solomon, checksum) is not in the PDU. `crc_status` is therefore `unknown`, never a
     claimed `valid`.
6. **Discontinuities.** A frame straddling a dropped record is decoded from spliced samples.
   gr-lora_sdr's CRC catches it (3 `crc_status: invalid` lines out of 24 with every 7th record
   dropped). A product adapter should zero-fill or restart the flowgraph at a `DISCONTINUITY`.
   GNU Radio 3.10 has no clean way to reset a running graph.
7. **EOF.** Closing the pipe drains the graph and exits 0 (§9.5 end-of-input). It works under the
   real host: `clean_exits: 1`, `last_exit: exit code 0`.

## 3. Runtime footprint

LoRa: SF7, BW 125 kHz, 250 kS/s (2× oversampling), the representative channel rate. gr-satellites:
NuSat 1, 40k FSK, 192 kS/s IQ, from the upstream `satellite-recordings` file.

| Measure | gr-lora_sdr | gr-satellites | Source |
|---|---|---|---|
| CPU at real time | **2.4–3.0 %** of one core (0.96 CPU-s over 32 s) | **11.5 %** (3.4 CPU-s over 29.5 s) | `lora-realtime-x10.json`, `sat-realtime-x5.json` |
| Throughput, as fast as possible | 32 s of signal in 0.64 s wall, 0.96 CPU-s (~33× real time per core) | — | `throughput-x10.json` |
| Peak RSS (`ru_maxrss`) | 45 MiB | 81 MiB | same |
| Private footprint (`phys_footprint`) | 27 MB | 55 MB | `footprint` on a live adapter |
| Threads | 11 (one per block + Python) | 12 | `shared-host-probe.jsonl` |
| Ready, warm | 0.29–0.35 s (n=10, load 38) | 1.30–2.59 s, median 1.60 (n=10, load 17) | `startup-warm.json`, `sat-startup-warm.json` |
| Ready, first launch after relink | 4.3–11.6 s (load 63–67) | not separately measured | `startup-after-relink.json` |
| First decode after spawn | 0.29–0.89 s (the first frame starts 0.14 s into the fixture) | 1.78 s | drive runs |
| Decode quality | 11/12 frames per pass; 110/120 over 10 passes; 0 false; `hk04` missed by the upstream decoder too | **11/11 parity** with the stock `gr_satellites` CLI, 55/55 over 5 passes, 0 false | same |

**Under the real host** (`host-check/`, `hk_plugins::PluginInstance` + `Ingest` + a republished
messages stream; `results/host-check.json`):
- manifest loads;
- ready in 0.34–0.86 s with **0 records offered before ready**;
- 22 host-stamped decodes republished (11 per pass × 2 passes, the `hk04` miss each pass);
- `malformed: 0`, `sample_index_out_of_range: 0`, `t_ns` error −16 µs…0.

**Kill and restart:** SIGKILL of the child mid-run, then a supervised restart back to ready in
**0.91–0.93 s**. That includes the 200 ms `backoff_initial_ms`, which leaves ~0.7 s for the
restart itself. A second pass then decodes everything again, continuing the host index.

## 4. The per-decoder cost, as a number

The spike's own clock, stated as what it is: an **agent's wall time on a warm machine**, not a
developer's.

| | Agent wall time | What it covered |
|---|---|---|
| Decoder 1: gr-lora_sdr + the shared pattern | ~16 min to the first real-host run | Three builds, the patch, adapter, fixture generator, driver |
| **Decoder 2: gr-satellites** | **14 min, 13:53→14:07**, of which **5.5 min** was compiling | Configure + build, finding the undeclared `pyzmq`, the adapter, a parity fixture from the stock CLI, 11/11 on the first run |

**The number for docs/18, in developer hours for the SECOND and later wrapped GR decoders:**

| Grade | Estimate | Basis |
|---|---|---|
| Decoding on stdout, arrival-stamped, parity-checked on one recording | **2–4 h** | This spike's second wrapper, scaled from agent to human time; mostly learning the OOT's I/O shape and output metadata |
| Product grade with exact host time stamps, when the OOT has one framing point (gr-lora_sdr shape) | **1.5–3 days** | Find the input-offset path, write and maintain a fork patch, contract test in `hk-plugins` with a fixture, manifest review, ledger, skip-if-absent CI, a second build on the Jetson matrix |
| Product grade when the OOT has no framing point (gr-satellites shape) | **not feasible without forking dozens of files**; realistically **"arrival-stamped forever"** | §2 point 4 |
| For comparison, readsb (not GR) | ~2,450 lines (1,358 wrapper + 1,094 tests) over **6 tickets**: T-015, T-037b, T-072, T-103, T-223, T-224 | `git log`. Most of the cost was the timing and readiness tail, not the first wrap |

**The first one's intercept:** ~0.5–1 day on the Mac (the toolchain traps in §1). Assessed at
**1–2 days on the Jetson** (**unverified**: a from-source or conda-forge headless GR runtime
plus the second build matrix), then again at every GNU Radio upgrade.

## 5. The honest negative: does a shared "GNU Radio host" amortise the runtime?

`shared_host_probe.py` starts the same graphs on idle inputs in one process versus separately
(`results/shared-host-probe.jsonl`, two runs each):

| Process | Started in | Peak RSS | Threads |
|---|---|---|---|
| bare runtime (Python + numpy + `gr` + `blocks`, one trivial graph) | 0.23–0.56 s | 38.5 MiB | 3 |
| lora alone | 0.26–0.49 s | 41.8 MiB | 11 |
| satellites alone | 1.51–1.55 s | 78.2 MiB | 12 |
| **both in one process** | 1.66–1.75 s | **78.8 MiB** | 22 |
| lora + satellites as two processes | — | 120 MiB (RSS double-counts shared dylib text; private is 27 + 55 MB) | 23 |

So a shared host saves **about one runtime floor per extra decoder: ~38 MiB RSS, somewhat less
private memory, and 0.2–0.5 s of start-up.** The decoder-specific part (gr-satellites' 40 MiB of
eagerly imported Python) is not shared. What it costs:
- **Fault isolation.** A segfault in any OOT block kills every decoder, and hk-plugins' restart
  model is per process.
- **One GIL.** gr-satellites' deframers and every adapter sink are Python, so they contend.
- **ABI coupling.** Every OOT loaded together must match one GR build and one pybind11 internals
  version. That is exactly the trap in §1, now between decoders.
- **A contract change.** One manifest = one process = one input stream today. A shared host
  needs N channels multiplexed on one stdin, or a sub-process protocol.

**Verdict: per-decoder processes are affordable and preferable.** On the Jetson's 8 GB, five
decoders' worth of duplicated runtime is ~190 MiB. A shared host lowers the *intercept* slightly
and leaves the *slope* unchanged. The slope is per-decoder engineering (§4): time stamping,
parity fixtures and CRC semantics, which no host can amortise. The one thing that amortises on
its own is the ~1 GB on-disk closure, and it does so whether or not the host is shared.

## 6. Observed, not chased

- **gr-lora_sdr upstream decode miss.** Frame `hk04` (25-byte payload, −480.7 Hz CFO) is
  decoded alone and after `hk03`, but **missed after `hk01`/`hk02`**. It is identical in the
  upstream `lora_sdr_lora_rx` hier block and at 10 and 20 dB SNR, so the cause is frame_sync
  state carried between frames. It is not the wrapper. Reproduce: `gen_fixture.py <stem> 12 556 20`, then
  `check_upstream_rx.py <stem>`, which prints 11 `rx msg` lines with no `hk04`.
- **The ADR-0010 ledger said VOLK is GPLv3.** VOLK ≥ 3.0 is **LGPL-3.0-or-later** (Homebrew
  `volk` 3.3.0 metadata). It still reaches hackriff only through GPLv3 GNU Radio, so its
  placement does not change; the ledger row is corrected in this branch.
- **Homebrew's gnuradio bottle and its pybind11 formula are out of step** (3.1 vs 3.0.2). A
  `brew upgrade` of either can break every built OOT without a rebuild. It is a trap for any
  Mac-side wrapped-GR CI, not only for this spike.

## Reproduce

```sh
./build.sh                                   # ~5-8 min; outputs to ~/.hackriff-ops/work/T-556
. ./env.sh
$GR_PY gen_fixture.py $T556_WORK/fx/lora12s20 12 556 20      # LoRa fixture + hidden truth
$GR_PY drive.py $T556_WORK/fx/lora12s20 --realtime --repeat 10 -- ./hk-gr-lora
$GR_PY drive.py $T556_WORK/fx/lora12s20 --drop-every 7 -- ./hk-gr-lora
curl -LO https://raw.githubusercontent.com/daniestevez/satellite-recordings/master/nusat.wav
$GR_PY sat_fixture.py nusat.wav $T556_WORK/fx/nusat "NuSat 1" 192000
$GR_PY drive.py $T556_WORK/fx/nusat --realtime --repeat 5 -- ./hk-gr-satellites --rate 192000
$GR_PY startup.py 10 ./hk-gr-lora
$GR_PY shared_host_probe.py both
(cd host-check && cargo run --release -- $T556_WORK/fx/lora12s20)   # the REAL plugin host
```

| File | What it is |
|---|---|
| `hk_gr_host.py` | Decoder-independent half: stdout swap, `hackriff-v1` reader, host index map |
| `hk_gr_lora.py`, `hk_gr_satellites.py` | The two adapters |
| `hk-gr-lora`, `hk-gr-satellites` | Launchers (`exec`, no daemons) |
| `manifest.json` | The LoRa plugin manifest |
| `patches/` | The gr-lora_sdr fork patch |
| `drive.py` | Host emulator: frames, drops, real-time pacing, `wait4` rusage, blind scoring |
| `host-check/` | Real `PluginInstance` driver; outside the workspace, path deps |
| `gen_fixture.py`, `sat_fixture.py` | Fixtures with hidden truth / stock-CLI parity truth |
| `startup.py`, `shared_host_probe.py` | Start-up sampling; question 5 |
