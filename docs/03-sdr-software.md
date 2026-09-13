# 03 — SDR Software Landscape for Exploration-First Signals Analysis

*Research snapshot: 2026-09-13. Maintenance status is taken from GitHub (last push, latest release) on or near that date unless noted. Where I could not verify something from a primary source, it is flagged **(unverified)**.*

## 0. TL;DR

- **No open-source tool today does the full loop** we want: wideband survey → automatic channel detection → automatic modulation/parameter estimation → automatic protocol ID/decode → persistent signal inventory → SigMF recording → select-and-extract from a waterfall. The pieces exist, spread across roughly a dozen projects.
- **Closest open-source building blocks:**
  - **SDRangel**: the broadest engineering feature set, including a Frequency Scanner, a squelch-triggered SigMF sink, Channel Power, maps, radio astronomy and a REST API.
  - **SigDigger/Suscan**: the best interactive "inspector" for unknown signals, with blind parameter estimation.
  - **URH**: automatic demod-parameter estimation and protocol field inference, but the repo was archived in 2026.
  - **rtl_433**: automatic ISM device decoding plus a pulse analyzer.
  - **Trunk Recorder**: records everything a trunked system carries from wideband captures.
  - **OpenWebRX+**: server-side background decoding.
  - **SatDump**: fully automatic satellite pipelines.
  - **IQEngine**: web SigMF browser with a backend plugin model for detectors and classifiers.
  - **Maia SDR**: FPGA waterfall plus SigMF recording on a PlutoSDR, served to a phone's web browser.
- **ML signal classification is real but mostly not productized in open source.**
  - TorchSig 2.x is actively maintained and has good synthetic data generators with realistic impairments, but it ships no GUI and no maintained real-time receiver.
  - DeepSig itself says its RadioML datasets are not used in its products.
  - Commercial systems do ship this: DeepSig OmniSIG, CRFS DeepView, R&S CA100 (AMMOS classifier), Aaronia's IQ Signal Classifier and ThinkRF SXM.
- **Frameworks are in flux:**
  - GNU Radio 4.0 reached RC1 in March 2026. It has no Python bindings or GUI yet.
  - Its governance split from GSI/FAIR in May 2026; the core is MIT-licensed.
  - Rust options (FutureSDR, RustRadio) are active and both target WebAssembly.
- **The UX problem is structural.** Every mainstream receiver is tuning-first: pick a frequency, pick a mode, set squelch, listen. None keeps a persistent, queryable inventory of "what's been seen where and when." Commercial monitoring suites (signal lists, occupancy, DPX persistence) are the best UX references.

---

## 1. Frameworks and Libraries

### 1.1 GNU Radio 3.10 (production line)

- **Status:** active. Latest release is v3.10.12.0 (2025-02-20); the repo was pushed 2026-08-27 ([releases](https://github.com/gnuradio/gnuradio)).
- **What it is:** the de facto open-source SDR DSP framework. It has C++ blocks with Python bindings, a thread-per-block scheduler, PMT message passing, and a large out-of-tree (OOT) module ecosystem.
- **Hardware access:**
  - `gr-soapy` has been in-tree since 3.10 and wraps SoapySDR.
  - `gr-uhd` covers Ettus radios.
  - `gr-osmosdr` (osmocom, pushed 2026-08) is the legacy multi-device source used by Gqrx and many OOTs ([osmocom/gr-osmosdr](https://github.com/osmocom/gr-osmosdr)).
- **Relevant OOTs:**
  - [gr-satellites](https://github.com/daniestevez/gr-satellites): active, pushed 2026-09.
  - [gr-inspector](https://github.com/gnuradio/gr-inspector): stale. Its energy detector, signal separator, OFDM estimator and TensorFlow AMC target GNU Radio 3.8; last push 2025-03.
  - [gr-fosphor](https://github.com/osmocom/gr-fosphor): OpenCL/GPU "phosphor" spectrum display; last push 2024-06.
  - gr-iqbal: IQ imbalance correction.
- **GNU Radio Companion (GRC) limitations for exploration:**
  1. GRC is a *program editor*, not an instrument. Changing what you're looking at often means editing a flowgraph and re-running it.
  2. Dynamic topology is awkward. You can't spawn N demodulators for N detected signals at runtime without custom Python or message-passing hacks.
  3. The Qt GUI sinks are debugging widgets, not a coherent analysis UI.
  4. No session, recording, or signal-inventory model.
  5. The HN community describes GNU Radio as sitting in "the uncanny valley — too GUI to scratch my programming itch but too low-level to be useful." Another user found it "so opaque as to be unusable" without a DSP background ([HN thread on GNU Radio World](https://news.ycombinator.com/item?id=49628576)).
- **GNU Radio World** (by Marc Lichtman, author of PySDR) compiles GNU Radio 3.x and the Qt GUI sinks to WebAssembly.
  - It runs a GRC-compatible editor in the browser and reads/writes `.grc` files.
  - It talks to RTL-SDR, PlutoSDR or HackRF over WebUSB ([gnuradioworld.com](https://gnuradioworld.com/), [rtl-sdr.com](https://www.rtl-sdr.com/gnu-radio-world-browser-based-gnu-radio-flowgraphs/), [777arc/gnuradio-world](https://github.com/777arc/gnuradio-world)).
  - It is a notable proof that browser SDR UIs are viable, but it keeps the flowgraph-centric model.

### 1.2 GNU Radio 4.0 (rewrite)

- **Origin:** developed largely by GSI/FAIR (the accelerator facility in Darmstadt) as `graph-prototype`, later `fair-acc/gnuradio4`.
- **Milestones:**
  - **RC1, March 2026** ([GR news](https://www.gnuradio.org/news/2026-03-22-gr4-release-candidate-1/); [fair-acc release](https://github.com/fair-acc/gnuradio4/releases/tag/4.0.0-RC1)):
    - **Block API:** modern C++23 with three processing styles — `processOne` (per sample), `processBulk` (per chunk) and manual `work()`. It adds compile-time reflection and first-class SIMD.
    - **Scheduler:** pluggable; single- and multi-threaded; "µs-scale scheduling latency"; no thread-per-block requirement.
    - **Buffers and graphs:** lock-free circular buffers; feedback loops in all graph modes; runtime graph reconfiguration.
    - **Compile-time block merging:** `BlockMerging.hpp` gives claimed 2–10× gains by removing inter-block buffers.
    - **Blocks:** a new polymorphic type system replaces PMT; SimdFFT (claimed faster than FFTW3); FIR/IIR filters; file I/O; SoapySDR source/sink.
    - **Explicitly missing:** Python bindings, a graphical design tool, most of the block library, and GR3 migration tools.
    - **Toolchain:** GCC ≥ 13.3, Clang ≥ 20, Emscripten 5.0.2; Linux stable, macOS ARM64 in progress, Windows needs maintainers.
  - **Governance split, May 21 2026** ([announcement](https://www.gnuradio.org/news/2026-05-21-gr4-community-stewardship/)):
    - GSI/FAIR had proposed a GSI-centered governance, licensing and release-cadence model. The GNU Radio board declined.
    - GR4 continues under GNU Radio's community governance from the MIT-licensed core. Blocks ported from GR3 stay GPLv3.
  - **Official super-repo, Aug 16 2026** ([announcement](https://www.gnuradio.org/news/2026-08-16-gr4-easy-to-build/)):
    - `gnuradio/gnuradio4` is a super-repo builder over `gnuradio4-core`, `gnuradio4-blocks` and `gnuradio4-library`.
    - Planned additions are `gnuradio4-blocks-gpl`, modtool, Python bindings, and a GPLv3 desktop "Studio" app.
    - It is not yet formally released as 4.0.0.
  - **Uncertainty:** `fair-acc/gnuradio4` is also still being pushed (2026-09-09). The long-term relationship between the FAIR and community lines is not clear from public sources.
- **UI work — OpenDigitizer** ([fair-acc/opendigitizer](https://github.com/fair-acc/opendigitizer), v1.0.0 2026-06-02):
  - GSI/FAIR's digitizer UI, built on GR4 plus OpenCMW, with Dear ImGui compiled natively and to WASM via Emscripten ([IPAC'23 paper](https://indico.jacow.org/event/41/contributions/2575/)).
  - It is aimed at accelerator diagnostics, not radio exploration. It shows that GR4 flowgraphs can drive a GPU-rendered, browser-deployable UI.
- **Assessment for this project:**
  - GR4's architecture is well-suited to embedded, high-rate work: compile-time merging, SIMD, and no thread per block.
  - But the ecosystem (blocks, Python, GUI, device support) is 1–2 years behind GR3.
  - Its governance churn in 2026 is a risk. Building a product on it today means doing a lot of block porting yourself.

### 1.3 Hardware abstraction

| Library | Notes | Status |
|---|---|---|
| [SoapySDR](https://github.com/pothosware/SoapySDR) | Vendor-neutral C/C++ device API with Python bindings; modules for RTL-SDR, HackRF, Airspy, SDRplay, LimeSDR, Pluto, UHD, and more; SoapyRemote for network streaming | Slow-moving but alive (push 2026-01) |
| [UHD](https://github.com/EttusResearch/uhd) | Ettus/NI USRP driver | Very active; v4.11.0.0 released 2026-09-09 |
| gr-osmosdr | Legacy multi-device GR source | Maintenance mode (osmocom) |
| libhackrf / hackrf tools | HackRF; includes `hackrf_sweep` | Active; v2026.01.3 ([greatscottgadgets/hackrf](https://github.com/greatscottgadgets/hackrf)) |

### 1.4 DSP libraries

- **[liquid-dsp](https://github.com/jgaeddert/liquid-dsp):** portable C DSP library (filters, modems, framing, AGC, NCO/PLL, spectral periodogram). No dependencies, so it's ideal for embedded use. Very active: v1.8.0 (2026-06), v1.8.1, v1.8.2 (2026-08-07).
- **[VOLK](https://github.com/gnuradio/volk):** SIMD kernels used by GNU Radio. v3.3.0 (2026-02); active.
- **FFTW:** the standard CPU FFT. Stable, with no significant new development in years **(unverified: latest version believed 3.3.10)**. On ARM, GR4's SimdFFT, KissFFT, or vendor libraries (ARM Performance Libraries, cuFFT on Jetson) are alternatives.
- **[csdr](https://github.com/jketterl/csdr):** command-line pipe DSP used by OpenWebRX.
  - jketterl's upstream was last pushed 2024-07.
  - The [luarvique/csdr](https://github.com/luarvique/csdr) fork used by OpenWebRX+ is active (2026-09).

### 1.5 Alternative frameworks

| Framework | Language | Model | Status (2026) | Relevance |
|---|---|---|---|---|
| [FutureSDR](https://github.com/FutureSDR/FutureSDR) | Rust | Async runtime; runs on Linux/Win/macOS/Android/WASM; custom buffers for Zynq DMA, Vulkan, Burn ML | Active; v0.6–v0.8 in Jul–Aug 2026 | Strong candidate for an embedded + web UI stack with ML hooks ([docs](https://www.futuresdr.org/learn/)) |
| [RustRadio](https://github.com/ThomasHabets/rustradio) | Rust | GNU Radio-like blocks and streams | Active (push 2026-09); browser/WASM UI support added April 2026 ([blog](https://blog.habets.se/2026/04/Rustradio-SDR-framework-now-also-in-the-browser-with-wasm.html)) | Small, single-maintainer |
| [Pothos](https://github.com/pothosware/PothosCore) | C++ | Dataflow + GUI designer | Dormant (last push 2023-06) | Avoid |
| [LuaRadio](https://github.com/vsergeev/luaradio) | LuaJIT | Lightweight, embeddable | Low activity (v0.11.0 2022; push 2025-11) | Nice embeddable design; small ecosystem |
| [REDHAWK](https://github.com/RedhawkSDR/redhawk) | C++/Java/Python | Component framework (CORBA heritage) | Dormant (push 2023-05) | Avoid |

### 1.6 Metadata: SigMF

- **What it is:** [SigMF](https://github.com/sigmf/SigMF) pairs a `.sigmf-data` file (raw samples) with a `.sigmf-meta` JSON file.
  - The JSON has `global`, `captures` (segments with frequency/time) and `annotations` (time/frequency boxes with labels and comments). Extensions and collections add more.
- **Status:** v1.2.6 (2025-12-21); [sigmf-python](https://github.com/sigmf/sigmf-python) is active (2026-08).
- **Supported by:** SDRangel (SigMF File Sink), Maia SDR, inspectrum, IQEngine, Trunk Recorder (`conventionalSIGMF` input), SatDump, and DeepSig OmniSIG output.
- **Recommendation:** SigMF should be the native recording *and* annotation format for this project. Detections and classifications become annotations.

### 1.7 GPU / ML stacks

- **cuSignal is archived.** RAPIDS 23.08 was its last release, and it was folded into CuPy (`cupyx.scipy.signal`, 140+ routines in CuPy v13) ([rapidsai/cusignal](https://github.com/rapidsai/cusignal)).
- **NVIDIA Holoscan SDK** is very active: v4.4 → v4.6.0 in July–Sept 2026 ([repo](https://github.com/nvidia-holoscan/holoscan-sdk)).
  - It is a streaming sensor/AI pipeline framework for Jetson/IGX/x86+GPU.
  - HoloHub includes an SDR FM demodulation reference app ([NVIDIA blog](https://developer.nvidia.com/blog/developing-streaming-sensor-applications-with-holohub-from-nvidia-holoscan/)).
  - It suits a Jetson build (zero-copy GPU pipelines, TensorRT inference), but it's general sensor middleware, not an SDR app.
- **NVIDIA Sionna** ([NVlabs/sionna](https://github.com/NVlabs/sionna)) is link-level PHY/system simulation plus a differentiable ray tracer, not a receiver.
  - Sionna 2.0 (2026-03) moved PHY/SYS from TensorFlow to PyTorch with the same API; v2.1.0 came 2026-09-09 ([forum](https://forums.developer.nvidia.com/t/sionna-2-0-pytorch-native-same-api/364087)).
  - Useful for generating training data and channel models, not for live exploration.
- **DeepSig/commercial engines** are covered in §4.

### 1.8 MATLAB / Simulink (brief)

- **What it offers:** Communications Toolbox, DSP System Toolbox and Deep Learning Toolbox provide polished AMC and SDR examples (Pluto/USRP/RTL-SDR support packages).
- **Canonical example:** the "Modulation Classification with Deep Learning" example ([MathWorks](https://www.mathworks.com/help/comm/ug/modulation-classification-with-deep-learning.html)).
  - A 5-conv-layer CNN over 11 modulations, trained on synthetic frames with AWGN, Rician fading and clock offset.
  - It reports ~97.7% synthetic test accuracy and "99%" over-the-air with two stationary ADALM-Pluto radios two feet apart.
  - That OTA setup is benign — a good illustration of how much easier lab AMC is than the field.
- **Verdict:** great for prototyping algorithms. Licensing and deployment make it a non-starter on a handheld.

### 1.9 Python ecosystem

- **Device access:** [pyrtlsdr](https://github.com/pyrtlsdr/pyrtlsdr) (active, 2026-09) and SoapySDR's Python bindings.
- **Array and DSP libraries:** `numpy`/`scipy.signal` (and CuPy on GPU) are the workhorses.
- **Learning:** [PySDR](https://pysdr.org/) ([repo](https://github.com/777arc/PySDR), active 2026-09) is the best free textbook. Its chapters on detection, sync and SigMF map directly to this project.
- **Rapid prototyping:** Python is ideal for the "analysis brain" (detection, parameter estimation, classification, database). Real-time front-end channelization belongs in C/C++/Rust/FPGA.

---

## 2. General Receiver Applications (baseline)

### 2.1 SDR++ (and forks)

- **Upstream** ([AlexandreRouma/SDRPlusPlus](https://github.com/AlexandreRouma/SDRPlusPlus); ~6.3k stars; push 2026-07-05):
  - **Releases:** the last tagged stable is 1.0.4 (2021). Everyone runs the rolling "nightly" build, which the release page now labels 1.3.0.
  - **Tech:** C++ with an ImGui UI; fast and lightweight.
  - **Module system:** source, sink, decoder and misc modules. Misc modules in the tree: `frequency_manager`, `scanner`, `recorder`, `rigctl_client`, `rigctl_server`, `scheduler`, `iq_exporter`, `discord_integration` ([misc_modules](https://github.com/AlexandreRouma/SDRPlusPlus/tree/master/misc_modules)).
  - **Frequency Manager:** bookmark lists with name, frequency, mode and bandwidth; can be shown on the waterfall.
  - **Scanner module** (from reading [the source](https://github.com/AlexandreRouma/SDRPlusPlus/blob/master/misc_modules/scanner/src/main.cpp)):
    - It steps a *single* start/stop range at a fixed interval.
    - It checks the max FFT level within a passband ratio of the VFO against one trigger level, with tuning and linger times.
    - It does **not** change demodulator mode, has no blacklist in the fetched source, and keeps no log of hits.
    - In short, a classic hardware-scanner imitation.
- **SDR++ Brown** ([sannysanoff/SDRPlusPlusBrown](https://github.com/sannysanoff/SDRPlusPlusBrown); push 2026-08; [site](https://sdrpp-brown.san.systems/)):
  - Adds a bundled DSD decoder (DMR/P25/NXDN), FT8/FT4 decoding with PSKReporter, remote KiwiSDR access, Hermes-Lite 2 TX, noise reduction, and Android small-screen UI work ([rtl-sdr.com](https://www.rtl-sdr.com/techminds-testing-the-sdr-brown-fork-with-built-in-dsd-and-remote-kiwisdr-support/)).
  - Unofficial; upstream asks users not to send it Brown-fork support requests.
- **HydraSDR fork** ([hydrasdr/SDRPlusPlus](https://github.com/hydrasdr/SDRPlusPlus)): vendor fork for the HydraSDR RFOne.
- **Takeaway:** best-in-class *responsiveness* and a clean module API, but it has no signal-analysis or inventory concepts.

### 2.2 SDRangel — the most engineering-grade open-source receiver

- **Status:** [f4exb/sdrangel](https://github.com/f4exb/sdrangel), ~4k stars. Very active: v7.27.0/7.27.1 (2026-07-04), v7.27.2 (2026-08-19), push 2026-09-11. The 2026 releases added MeshCore and Fobos SDR support ([Mac Ham Radio](https://machamradio.com/blog/2026/07/14/sdrangel-v7-27-1-released/)).
- **Platforms and devices** ([sdrangel.org](https://www.sdrangel.org/)):
  - Windows/Linux/macOS/Android; GUI or headless server with a full REST API.
  - Rx and Tx, MIMO, multiple devices at once; OpenGL/Vulkan/CUDA acceleration.
- **Architecture:**
  - *Device sets* (a sampling source/sink) host *channel plugins* (demods, sinks, analyzers) at offsets within the device's bandwidth.
  - *Feature plugins* sit above devices and coordinate across channels, devices and external systems.
- **Channel Rx plugins** ([channelrx](https://github.com/f4exb/sdrangel/tree/master/plugins/channelrx)), 44 in total:
  - **Analog:** AM, NFM, WFM, BFM (with RDS), SSB, ATV, WDSP Rx.
  - **Digital voice and data:** DSD (DMR/dPMR/D-Star/YSF/NXDN via DSDcc), M17, FreeDV, FT8, RTTY, NAVTEX, DSC, packet/AX.25, pager (POCSAG/FLEX), ADS-B, AIS, APT, DAB, DATV, Inmarsat, ILS/VOR localizer, radiosonde, end-of-train, ChirpChat (LoRa), Meshtastic, MeshCore.
  - **Measurement:**
    - `chanalyzer` — scope, constellation and PLL/FLL lock.
    - `channelpower` — average, max/min peak and pulse-average power over a set bandwidth ([readme](https://github.com/f4exb/sdrangel/blob/master/plugins/channelrx/channelpower/readme.md)).
    - `freqtracker` and `noisefigure`.
    - `radioastronomy` — total power, spectrometry, calibration.
    - `heatmap` — GPS-tagged power mapping with CSV/image export ([readme](https://github.com/f4exb/sdrangel/blob/master/plugins/channelrx/heatmap/readme.md)).
    - `radioclock`.
  - **Sinks:** `filesink`, `sigmffilesink`, `localsink`, `remotesink`, `remotetcpsink`, `udpsink`.
  - **Scanning:** `freqscanner`.
- **Feature plugins** ([feature](https://github.com/f4exb/sdrangel/tree/master/plugins/feature)): Map (2D/3D), Satellite Tracker (Doppler correction, preset loading at AOS), Star Tracker (Stellarium link, galactic line-of-sight, drift scans), Sky Map, Radiosonde, AIS, APRS, VOR localizer, GS-232 rotator controller, AFC, Demod Analyzer, Denoiser, Morse decoder, PER tester, SID (sudden ionospheric disturbance), rigctl server, remote control, and more ([Star Tracker readme](https://github.com/f4exb/sdrangel/blob/master/plugins/feature/startracker/readme.md), [Satellite Tracker readme](https://github.com/f4exb/sdrangel/blob/master/plugins/feature/satellitetracker/readme.md)).
- **Frequency Scanner** ([readme](https://github.com/f4exb/sdrangel/blob/master/plugins/channelrx/freqscanner/readme.md)):
  - **Input:** a list of frequencies plus one channel bandwidth.
  - **Tuning:** it places the device center so that as many listed frequencies as possible fall within the instantaneous bandwidth, avoiding DC.
  - **Detection:** FFT power (Peak or Total) against a threshold.
  - **Modes:** Single, Continuous, Scan-Only (just counts activity) and Multiplex. On a hit it retunes a *chosen existing* demod channel.
  - **Limitation:** it's still list-based. It doesn't discover unknown channels, estimate bandwidth, or pick a demod.
- **SigMF File Sink** ([readme](https://github.com/f4exb/sdrangel/blob/master/plugins/channelrx/sigmffilesink/readme.md)):
  - Records a decimated channel as SigMF at 8/16/32-bit.
  - A spectrum squelch triggers recording, with pre-trigger (prepended seconds) and post-trigger hold. New captures are appended to the same SigMF file.
  - Several instances can record different slices of one baseband. This is the closest open-source analog to "capture-on-detect."
- **Why it matters:** SDRangel already has most *mechanisms* we need (multi-channel within a wideband capture, triggered SigMF recording, power statistics, maps, REST automation, headless mode).
- **What it lacks:** the *automation layer* (detect → classify → spawn the right channel → log to an inventory) and the *UX* (it's a dense, desktop-docking Qt UI with a manual "add channel, choose plugin" workflow).
- **Practical option:** drive SDRangel headless via REST from an external "brain" — a viable prototype path.

### 2.3 Other desktop receivers (brief)

| App | Notes | Status |
|---|---|---|
| **SDR#** (Airspy) | Windows-only, closed source; large plugin ecosystem; frequency scanner plugins | Maintained by Airspy; closed |
| **[Gqrx](https://github.com/gqrx-sdr/gqrx)** | GNU Radio + Qt; simple, Linux/macOS; bookmarks; audio/IQ recording | Active; v2.17.7 (2025-05); push 2026-08 |
| **[CubicSDR](https://github.com/cjcliffe/CubicSDR)** | liquid-dsp + SoapySDR; OpenGL UI | Last release 0.2.7 (2022); occasional commits in 2026 — effectively low-maintenance |
| **SDRuno / SDRconnect** (SDRplay) | Vendor apps for RSP devices; SDRconnect is the newer cross-platform app. A Dec 2024 RadioReference user called it "nowhere near ready for prime time" ([thread](https://forums.radioreference.com/threads/im-getting-sick-of-this.482612/)) | Closed, vendor-maintained |
| **SDR Console** (Simon Brown) | Windows, closed; very feature-rich, including trunk voice following ([sdr-radio.com](https://www.sdr-radio.com/trunk-voice-following)) | Maintained; closed |
| **[AetherSDR](https://github.com/aethersdr/AetherSDR)** | FlexRadio-client workstation (Qt6/C++20); [Aether-gate](https://github.com/nigelfenton/Aether-gate) bridges other radios. *Not* a monitoring/exploration tool — likely a name collision with the "Aether" in the brief | Active, ham-operating focus |

### 2.4 Web-based and embedded receivers

- **OpenWebRX / OpenWebRX+:**
  - Upstream [jketterl/openwebrx](https://github.com/jketterl/openwebrx) was last pushed 2024-12 (stalled).
  - The community fork **OpenWebRX+** ([luarvique/openwebrx](https://github.com/luarvique/openwebrx)) is very active: 1.2.117 (2026-06), push 2026-09. It has Raspberry Pi images and a PPA.
  - **Decoders:** AIS, SSTV, FAX, FLEX, POCSAG, HFDL, VDL2, ADS-B, ACARS, ISM, RDS, SAM, SITOR-B, RTTY, CW, DTMF/EEA/EIA/CCIR/ZVEY selcall. The DMR/YSF/D-Star/NXDN and WSJT modes are inherited.
  - **Extras:** automatic bookmarks (shortwave broadcasters, nearby repeaters), a bookmark scanner, recorder, and a map of decoded positions.
  - **Background decoding** ([wiki](https://github.com/jketterl/openwebrx/wiki/Background-decoding)):
    - When no user is connected, a *Static* or *Daylight* (sunrise/sunset/greyline) scheduler selects SDR profiles.
    - Decoders for WSJT-X modes, JS8, packet, and SSTV/FAX in OWRX+ run at low CPU priority.
    - Spots go to PSKReporter, APRS-IS and WSPRnet; interactive users preempt the schedule.
  - **Assessment:** this "receiver as an always-on decoding appliance" is a key UX idea. It's still frequency-plan-driven (it decodes where you *told* it modes live), not discovery-driven.
- **KiwiSDR:**
  - The original `jks-prv/Beagle_SDR_GPS` repo was **archived** (2024-12). Activity continues in [jks-prv/KiwiSDR](https://github.com/jks-prv/KiwiSDR) (push 2026-09) — I infer this is the successor **(relationship inferred, unverified)**.
  - An HF-only (0–30 MHz) networked receiver with many extensions (FT8, WSPR, FAX, DRM, TDoA).
- **WebSDR** (PA3FWM): closed source, multi-user HF/VHF web receivers; baseline only.
- **Maia SDR — important** ([maia-sdr.org](https://maia-sdr.org/), [repo](https://github.com/maia-sdr/maia-sdr); v0.12.0 2025-11-09; push 2026-04):
  - **What it is:** firmware for the ADALM-Pluto/Pluto+ by Daniel Estévez.
  - **FPGA (Amaranth HDL):** a pipelined, low-resource FFT core for a real-time waterfall at up to 61.44 Msps, plus a digital down-converter (DDC) to select a slice of the input spectrum (since Maia 0.8.0 / firmware v0.6.0).
  - **Software (Rust):** an async HTTP server on the Zynq ARM with a REST API controlling the FPGA IP and AD936x.
  - **UI:** Rust→WASM with WebGL2 waterfall rendering, reachable from a phone browser.
  - **Recording:** IQ to SigMF, limited to 400 MiB by Pluto RAM.
  - **Maturity:** self-described "minimum viable product" ([Estévez blog](https://destevez.net/2023/02/maia-sdr/)).
  - **Relevance:** architecturally this is the best reference for a handheld/embedded exploration device: FPGA does the FFT and DDC, a small Rust server exposes REST, a WASM/WebGL UI runs on any phone. It has no demodulators, detection or classification.
- **PortaPack Mayhem** ([portapack-mayhem/mayhem-firmware](https://github.com/portapack-mayhem/mayhem-firmware), ~5.4k stars):
  - Latest stable v2.3.2 (Dec 2025); nightlies through 2026-09-03.
  - HackRF+PortaPack firmware with dozens of apps: receivers, ADS-B/AIS/POCSAG/APRS/weather-sensor decoders, a "Looking Glass" wideband spectrum view, recon/scanner, capture/replay, external SD-card apps.
  - **Limits:** the Cortex-M4 is heavily constrained — very narrow processing bandwidth, one app at a time. This is the device our project wants to replace.

---

## 3. Scanning, Survey, Monitoring and Decoding Tools

### 3.1 Wideband sweep / survey

| Tool | What it does | Status |
|---|---|---|
| **`hackrf_sweep`** | Retunes the HackRF in 20 MHz steps and emits CSV power bins (frequency, bin width, dB). Very fast sweeps (derived tools quote ~8 GHz/s). A library reimplementation exists: [subreption/hackrf_sweeper](https://github.com/subreption/hackrf_sweeper) | Active with HackRF releases |
| **rtl_power** + **heatmap.py** | rtl-sdr's hopping power integrator; `heatmap.py` in [keenerd/rtl-sdr-misc](https://github.com/keenerd/rtl-sdr-misc) renders CSV to waterfall PNGs | Stable/old (push 2024-07) |
| **[soapy_power](https://github.com/xmikos/soapy_power)** | rtl_power-like for any SoapySDR device | Unmaintained (release 2017; push 2024-06) |
| **[QSpectrumAnalyzer](https://github.com/xmikos/qspectrumanalyzer)** | PyQtGraph GUI over hackrf_sweep/rtl_power/soapy_power; fork [SpectroScope](https://github.com/konung-yaropolk/SpectroScope) | Low maintenance (push 2024-04) |
| **[spektrum](https://github.com/pavels/spektrum)** | Processing-based sweep GUI | Dormant (2022) |
| **[hackrf-spectrum-analyzer](https://github.com/pavsa/hackrf-spectrum-analyzer)** | Java GUI for hackrf_sweep with persistence display | Older (unverified recency) |
| **[Spectre (HB9TF)](https://github.com/hb9tf/spectre)** | Go tool for long-term, distributed survey: collects from rtl_power/hackrf_sweep into SQLite/MySQL/central server; renders waterfall images; filter by time/frequency/source ([write-up](https://medium.com/hb9tf/rf-spectrum-analysis-open-sourced-7ec5911fc2)) | Active (push 2026-07) — rare example of *persistent survey history* |
| **[NTIA SCOS Sensor](https://github.com/NTIA/scos-sensor)** + [scos-actions](https://github.com/NTIA/scos-actions) | US government reference for networked spectrum sensors: hardware-agnostic, web tasking, "actions" (FFT at frequencies, multi-frequency IQ acquisition), SigMF output; part of an IEEE standardization effort ([ITS](https://its.ntia.gov/research/rfm/spectrum-monitoring/standardization)) | Research-grade; model worth studying for task scheduling + metadata |
| **ElectroSense** | Crowdsourced RTL-SDR spectrum monitoring network with an open API ([paper](https://arxiv.org/abs/1703.09989)) | **Likely dormant**: sensor repo last push 2022-10; electrosense.org served a mismatched TLS certificate when checked (2026-09) |

### 3.2 Real-time scanning inside receivers

- **SDR++ scanner** (§2.1) and **SDRangel Frequency Scanner** (§2.2) are both threshold-over-list (or range) scanners.
- **[RTLSDR-Airband](https://github.com/szpajder/RTLSDR-Airband)** (active 2026-08):
  - Multichannel AM/NFM airband recording with *automatic* noise-floor-tracking squelch (opens at ~10 dB over the estimated noise floor).
  - Its own wiki still tells users with continuous carriers to "observe signal levels in a textual waterfall and guess the value" for manual squelch ([wiki](https://github.com/szpajder/RTLSDR-Airband/wiki/Manual-squelch-setting)).
- **Trunk Recorder** also has a signal detector with an automatic noise floor for conventional channels (§3.5).

### 3.3 Automatic device and protocol decoders

- **[rtl_433](https://github.com/merbanan/rtl_433)** (~7.8k stars; release 25.12 on 2025-12-12; nightly 2026-09):
  - **Coverage:** ~380 device-protocol entries in the README (my count of the list). Weather stations, TPMS, remotes, energy meters and other 315/345/433/868/915 MHz OOK/FSK devices, all decoded simultaneously with no user mode selection.
  - **Pulse analyzer (`-A`)** ([ANALYZE.md](https://github.com/merbanan/rtl_433/blob/master/docs/ANALYZE.md)): prints pulse/gap (OOK) or mark/space (FSK) timing histograms and guesses the coding (PWM/PPM/Manchester). The *flex decoder* (`-X`) then lets you define a new decoder from the command line.
  - **Why it's the model:** this is the best existing open-source example of "detect burst → characterize → auto-decode or suggest a decoder."
- **[multimon-ng](https://github.com/EliasOenal/multimon-ng)** (active 2026-07): POCSAG/FLEX/EAS/DTMF/AFSK/ZVEI and more from an audio pipe.
- **[Dire Wolf](https://github.com/wb2osz/direwolf)** (active 2026-09): AX.25/APRS soft TNC.
- **ADS-B:** [dump1090 (FlightAware)](https://github.com/flightaware/dump1090) and [readsb](https://github.com/wiedehopf/readsb) — both active.
- **[AIS-catcher](https://github.com/jvde-github/AIS-catcher)** (v0.70, 2026-06): multi-SDR AIS with a web UI.
- **[radiosonde_auto_rx](https://github.com/projecthorus/radiosonde_auto_rx)** (active 2026-09): scans a band, detects sonde transmissions, identifies sonde type, and decodes automatically. A small but complete detect → identify → decode pipeline for one signal family.
- **WSJT-X / fldigi:** WSJT-X decodes every FT8/FT4/WSPR signal in its passband with no tuning. fldigi supports RSID (a transmitted mode identifier) for automatic mode switching, but only when the sender transmits RSID **(based on long-standing fldigi docs; not re-verified this session)**.
- **[gr-satellites](https://github.com/daniestevez/gr-satellites):** decoders for 100+ amateur/cubesat telemetry formats; active.

### 3.4 Protocol reverse engineering and signal inspection

**Universal Radio Hacker (URH) — important, but now archived**

- **Status:** [jopohl/urh](https://github.com/jopohl/urh), ~12.6k stars. v2.10.0 was released 2025-12-17, then the repository was **archived (read-only)**, reportedly on 2026-03-29.
- **Capabilities** (per the README):
  - Record or load IQ.
  - **Automatic detection of modulation parameters**: modulation type (ASK/FSK/PSK), samples per symbol/bit length, center/threshold, noise level, tolerance ([ACM paper](https://dl.acm.org/doi/10.1145/3375894.3375896)).
  - Configurable decodings, e.g. CC1101 whitening.
  - **Rule-based automatic protocol field inference** ([WOOT'19](https://www.usenix.org/conference/woot19/presentation/pohl)).
  - Fuzzing and a stateful simulator ([WOOT'18](https://www.usenix.org/conference/woot18/presentation/pohl)).
- **UX strength:** a unified signal → bits → protocol → message-type view.
- **Limits:** burst-oriented (ISM-style OOK/FSK/PSK) and Python/Qt. No wideband discovery.
- **Relevance:** its parameter-estimation heuristics and protocol-inference ideas are directly reusable (GPLv3) but need a maintained home.

**inspectrum** ([miek/inspectrum](https://github.com/miek/inspectrum))

- **Status:** v0.4.0 (2025-12-06) after a two-year gap.
- **Features:** offline spectrogram viewer for 100 GB+ files (SigMF, cf32/cs16/cu8...).
  - Amplitude/frequency/phase/IQ derived plots.
  - Cursors to measure period and symbol rate and extract symbols.
  - Export of filtered samples.
- Beloved for its minimalism; purely manual.

**SigDigger / Suscan / Sigutils — important: the closest thing to an open signals-analysis GUI**

- **Repos:** [BatchDrake/SigDigger](https://github.com/BatchDrake/SigDigger) (~2.9k stars; push 2026-02) and [Suscan](https://github.com/BatchDrake/suscan) (push 2026-08).
- **Release cadence:** the last *tagged* release is v0.3.0 (2022). A rolling "Development Build" was last refreshed 2025-04-08. All four repos (SigDigger, SuWidgets, Suscan, Sigutils) now develop on `master`.
- **Maintenance:** single primary maintainer (Gonzalo Carracedo); 72 open issues. Linux AppImage and macOS builds; Windows is acknowledged as problematic.
- **Features** ([site](https://batchdrake.github.io/SigDigger/)):
  - **Views:** OpenGL spectrum/waterfall and an interactive **Panoramic Spectrum** (sweeping wider than the device bandwidth).
  - **Inspectors:** select a channel on the waterfall → open an ASK/FSK/PSK inspector with **blind parameter estimation** (baud rate via fast symbol autocorrelation, gradient-descent SNR estimation), carrier recovery, and symbol recording/visualization.
  - **Other analysis:** subcarrier inspection, a waveform window with burst detection and limited offline demodulation, analog TV demodulation, and Doppler analysis with TLE-based correction.
  - **Audio and recording:** AM/FM/USB/LSB audio; full-band and per-channel baseband recording.
  - **Networking and plugins:** UDP broadcast of samples/symbols; plugins (APT, AmateurDSN, ZeroMQ, AntSDR).
  - **Library layer:** Suscan provides multicore worker threads, generic ASK/FSK/PSK/audio demodulators, a codec interface and SoapySDR sources ([suscan README](https://github.com/BatchDrake/suscan)). It has a remote-analyzer mode (SigDigger connecting to a headless suscan server) **(present in 0.3.x release notes; not re-verified this session)**.
- **Why it matters:** the **"inspector" model** — click a signal, get a dedicated analysis workspace with estimators, a constellation/symbol view and recording — is exactly the interaction pattern this project should adopt. What SigDigger lacks is *automation* (it never proposes signals or parameters unprompted), an inventory, and polish/docs ("documentation is work in progress").

**baudline**

- Closed-source, freeware time-frequency browser, famous for fast drill-down and many analysis views.
- No updates in many years **(exact last version unverified)**. Reference for analysis-UI density only.

**IQEngine — important** ([IQEngine/IQEngine](https://github.com/IQEngine/IQEngine); [iqengine.org](https://iqengine.org/))

- **What it is:** a web app (React + FastAPI backend) to browse, search and share SigMF recordings.
  - Spectrogram with zoom, time/frequency/IQ plots, FIR filtering, annotation editing.
  - Metadata search across large corpora; private deployments on Azure/blob storage.
  - Backed by Microsoft, Qoherent and Airbus ([rtl-sdr.com](https://www.rtl-sdr.com/iqengine-a-web-based-toolkit-for-sharing-and-analyzing-rf-iq-recordings/)).
  - The team spun out [WebFFT](https://github.com/IQEngine/WebFFT).
- **Plugin model** ([plugins doc](https://github.com/IQEngine/IQEngine/blob/main/client/src/pages/docs/plugins.mdx)):
  - Plugins are REST services: IQ in; IQ, audio, bytes or **SigMF annotations** out. A Python plugin server and template are provided, and the API was reworked for async processing.
  - Plugins in the tree: `fm_receiver`, `fm_receiver_gnuradio`, `fm_signal_detector`, `simple_detector`, `markos_detector`, `satdump`, `lowpass_filter` (+ GNU Radio version), `gain_stage`, `template_plugin`.
  - Envisioned categories: detectors, modulation classifiers, modems, TDOA/DOA, MIMO, generators.
- **Status:** pre-release builds through 2025-12-20; repo push 2026-07-31; 327 stars. Maintained but lower velocity than in 2023–24.
- **Takeaway:** the plugin → annotation pattern is the right abstraction for "run a detector/classifier over a selection and draw boxes." It is offline/recording-centric, not live.

### 3.5 Trunking and digital voice (the "CB/trunk complaint")

- **[SDRTrunk](https://github.com/DSheirer/sdrtrunk)** (Java; ~2.2k stars):
  - **Protocols:** P25 Phase 1/2 trunking, DMR (Tier III, Capacity Plus/Connect Plus), LTR, MPT1327, NBFM/AM, and more.
  - **Features:** multi-tuner, Radio Reference import, streaming to Broadcastify/Rdio Scanner/OpenMHz.
  - **Status:** latest *final* release v0.6.1 (2024-12-03); the master branch was still pushed 2026-08-01. No new release in ~21 months **(treat release cadence as slow)**. A community fork [sdrtrunk-vce](https://github.com/tylerwatt12/sdrtrunk-vce) publishes 0.6.2 alphas (2026).
  - **Configuration:** requires building "playlists" of systems/channels by hand or from Radio Reference.
- **[OP25 (boatbod fork)](https://github.com/boatbod/op25):** GNU Radio-based P25/DMR/SmartNet decoder with a web terminal; active (2026-08).
- **Trunk Recorder — important** ([TrunkRecorder/trunk-recorder](https://github.com/TrunkRecorder/trunk-recorder); v5.2.0/5.2.1 2026-04; push 2026-09):
  - **Capture-everything model** ([intro](https://trunkrecorder.com/docs/intro)):
    1. One or more SDRs are tuned to cover the system's whole frequency span.
    2. The control channel is decoded continuously.
    3. For *every* voice grant, a recorder is attached to the matching slice of the already-captured wideband stream, so **all simultaneous calls are recorded**, not just the one you'd be listening to.
    4. Calls end after a silence timeout.
    5. Plugins upload to OpenMHz, Broadcastify and Rdio Scanner.
  - **System types** ([CONFIGURE.md](https://github.com/TrunkRecorder/trunk-recorder/blob/master/docs/CONFIGURE.md)): `smartnet`, `p25`, **trunked `dmr`** (Capacity Plus/Capacity Max/Connect Plus/Tier III, with automatic LCN→frequency mapping from a candidate channel list), `conventional`, `conventionalP25`, `conventionalDMR`, `conventionalSIGMF`.
  - **Tuning:** a signal detector with automatic noise-floor thresholding and `autoTune` frequency-error correction.
  - **Setup burden:** the configuration guide still tells you to research the system on Radio Reference, then "try and receive the control channel... using GQRX... Type in the different frequencies to look for the active control channel" (CONFIGURE.md). Discovery is entirely manual even in the best capture-everything tool.
- **DSD+ / DSDPlus Fastlane:** Windows, closed. Fastlane is a paid early-access subscription. It does P25 Phase 1/2, DMR and NXDN voice following, including a single-dongle control/voice hopping mode, plus scan lists ([RR wiki](https://wiki.radioreference.com/index.php/DSDPlus), [rtl-sdr.com](https://www.rtl-sdr.com/signalseverywhere-using-dsdplus-fastlane-for-listening-to-phase-1-p25-trunking/)).
- **[dsd-fme](https://github.com/lwvmobile/dsd-fme):** open-source DSD fork with broad digital-voice support and trunking; active (release 2026-07-15).
- **Unitrunker:** long-standing Windows closed-source trunking control-channel tracker; baseline only **(status not re-verified)**.

### 3.6 Satellites

- **SatDump — important** ([SatDump/SatDump](https://github.com/SatDump/SatDump); docs at [docs.satdump.org](https://docs.satdump.org/index.html)):
  - **Status:** latest stable 1.2.2 (2024-11-29). Development is on **2.0.0-alpha** — the docs report `2.0.0-alpha-b65975556` — with a new Explorer UI, product and calibration system, and AngelScript scripting ([rtl-sdr.com](https://www.rtl-sdr.com/moving-satdump-towards-v2-0-0/)). Very active (push 2026-09-12).
  - **Pipelines:** declarative definitions of baseband → demod → deframe → decode → products (images, calibrated composites) for dozens of weather/EO satellites.
  - **Automation:** a full auto scheduler selects satellites by pass, starts the SDR, picks the pipeline, drives a rotator and processes each pass ([tracker docs](https://docs.satdump.org/sat_tracker.html)).
  - **Platforms:** GUI, CLI/headless, Android, Raspberry Pi; OpenCL acceleration.
  - **Takeaway:** its *pipeline registry* (signal type → processing chain → products) is the pattern to copy for "auto protocol decode" once a signal has been identified.

### 3.7 Signal identification references

- **[Signal Identification Wiki](https://www.sigidwiki.com/wiki/Signal_Identification_Guide):** 592 identified signals, 406 unidentified, 54 requested at time of fetch. Organized by band and category, with audio samples and waterfall images. Human-oriented, with no machine-readable feature vectors.
- **[Artemis](https://github.com/AresValley/Artemis)** (v4.2.0, 2026-07-18; active):
  - An offline, filterable sigidwiki database (frequency, bandwidth, mode, modulation, ACF) with a PySide6 GUI.
  - v4 was a full rewrite "paving the way for... machine learning based identification" ([rtl-sdr.com](https://www.rtl-sdr.com/artemis-4-released-offline-signal-identification-database/)).
  - Could seed a *prior* for a classifier: given a detected center frequency and bandwidth, rank candidate signal types.

### 3.8 Other

- **[TempestSDR](https://github.com/martinmarinov/TempestSDR):** video-emanation eavesdropping; dormant (2023).
- **Kestrel TSCM Professional** ([kestreltscm.com](https://kestreltscm.com/)): commercial Canadian SDR-based TSCM/SIGINT-support software.
  - Supports many SDRs and analyzers, with up to 165 MHz real-time bandwidth claimed.
  - Workflow-oriented: emission identification, TEMPEST detection, spectrum surveys.
  - Good reference for "survey → compare against baseline" workflows.
- **RFNM** ([rfnm.com](https://rfnm.com/)): hardware, not software. An NXP LA9310-based SDR with daughterboards (Lime 10–3800 MHz; Granita up to 7.2 GHz) and up to ~153 MSPS ADCs ([rtl-sdr.com review](https://www.rtl-sdr.com/an-initial-review-of-the-rfnm-software-defined-radio/)). Relevant as a high-bandwidth front end for a Pi 5/Jetson build; no new 2025–26 news found.

### 3.9 Commercial analysis and monitoring software (UX references)

| Product | What's relevant | Source |
|---|---|---|
| **DeepSig OmniSIG** | ML detection and classification of known *and unknown* signals from VITA-49 streams (Epiq, USRP, Signal Hound), up to 500 MHz IBW; JSON/SigMF/syslog/Elastic output; **OmniSIG Studio** trains custom models from labeled recordings; runs on x86/ARM/GPU; shown on NVIDIA Jetson Thor at GTC DC 2025 | [OmniSIG](https://www.deepsig.ai/omnisig/), [BusinessWire](https://www.businesswire.com/news/home/20251028742262/en/DeepSig-Demonstrates-AI-Native-Wireless-Intelligence-at-NVIDIA-GTC-DC-2025) |
| **CRFS RFeye DeepView** | Forensic analysis of recordings with "Signal Discovery" (anomaly detection and statistics across large datasets), occupancy, classification, TDoA geolocation; finds hoppers and low-power signals near strong ones | [DeepView](https://www.crfs.com/software/rfeye-deepview) |
| **R&S CA100 / CA120 / CA210, ARGUS** | CA100 includes the **AMMOS classifier** for automatic modulation/transmission-system recognition on unknown HF/VHF/UHF signals, plus demod and decode; CA120 is multichannel; ARGUS is ITU-style monitoring | [CA100](https://www.rohde-schwarz.com/us/products/aerospace-defense-security/online-signal-analysis/rs-ca100-pc-based-signal-analysis-and-signal-processing-software_63493-52992.html), [ARGUS](https://www.rohde-schwarz.com/us/products/aerospace-defense-security/radiomonitoring-software/rs-argus_63493-10783.html) |
| **Aaronia RTSA-Suite PRO** | Block-graph UI; **IQ Signal Classifier** block; **IQ Pulse Inspector** does automatic burst classification and decoding (Wi-Fi, BT, GSM, DECT, QPSK, QAM...) and can run over a whole recording to produce **a table of all found signals** | [Aaronia IQ Pulse Inspector](https://aaronia.com/en/iq-pulse-inspector), [RTSA blocks PDF](https://downloads.aaronia.com/manuals/RTSA-Suite_PRO_Software-Blocks.pdf) |
| **Tektronix SignalVu-PC (DPX)** | DPX: a 3D histogram (frequency × amplitude × hit density) of up to ~292k spectra/s rendered as a color-graded persistence bitmap; DPX density triggers | [DPX primer](https://www.tek.com/en/documents/primer/dpx-acquisition-technology-spectrum-analyzers-fundamentals) |
| **Keysight PathWave 89600 VSA** | Deep standards-based demod and measurements (EVM, constellations, 5G/Wi-Fi); assumes you know what you're measuring | [89600 VSA](https://www.keysight.com/us/en/products/software/pathwave-test-software/89600-vsa-software.html) |
| **Signal Hound Spike** | Spectrum/spectrogram and real-time views; **Pulse Analysis** option deinterleaves and classifies up to five emitters; multichannel phase-coherent IQ with PCR4200 (2025) | [Spike](https://signalhound.com/spike/), [pulse analysis](https://signalhound.com/products/pulse-analysis/) |
| **ThinkRF SXM** | Networked RTSAs plus "autonomous, AI-driven spectrum intelligence": blind detection/classification, multi-sensor fusion, automatic 2G–5G identification, 24/7 monitoring, REST API | [thinkrf.com](https://thinkrf.com/) |
| **Distributed Spectrum** | Startup (founded 2020; $25M Series A March 2025): networks of small edge-AI RF sensors for tactical spectrum awareness | [Medium](https://medium.com/@distributedspectrum/why-we-built-distributed-spectrum-ff9ebd026da2), [StartupHub](https://www.startuphub.ai/startups/distributed-spectrum) |
| **Pentek** | High-rate recording/FPGA hardware (e.g., Talon recorders); not an exploration UI **(not researched in depth)** | — |

---

## 4. Automatic Modulation Classification and Signal Detection in Practice

### 4.1 Datasets and toolkits

- **RadioML (DeepSig, 2016–2018)** ([datasets page](https://www.deepsig.ai/datasets/)):
  - **Versions:** 2016.04C and 2016.10A (11 modulations, GNU Radio synthetic); 2018.01A (24 modulations, ~2M × 1024-sample frames, SNR −20…+30 dB, synthetic channel effects).
  - **License:** CC BY-NC-SA 4.0 — non-commercial.
  - **Caveats:** DeepSig notes "known errata," that the datasets are "not currently used in their products," and recommends real data instead.
  - **Practical implication:** thousands of papers report 90%+ accuracy on RadioML, which says little about real OTA exploration. Frames are pre-channelized, single-signal, and cover only a fixed set of classes.
- **TorchSig** ([TorchDSP/torchsig](https://github.com/TorchDSP/torchsig); MIT; active):
  - **Release history:**
    - v1.0 (March 2025): ground-up rewrite.
    - v2.0 (2025-09): Sig53 → "Narrowband" and WidebandSig53 → "Wideband" renamed and then unified into one configurable dataset system; HDF5 storage; metadata transforms; 61 signal classes including analog AM/FM variants and chirp spread spectrum.
    - v2.1.0 (2026-02): models and image datasets moved out to [torchsig-models](https://github.com/TorchDSP/torchsig-models); NumPy 2.
    - v2.1.1 (2026-04): now on PyPI.
    - v2.2.0 (2026-08-31): LoRa, GSM, 802.11a, Zigbee and BLE signal families; geolocation support; multi-label classification; Numba acceleration ([releases](https://github.com/TorchDSP/torchsig/releases)).
  - **Impairment modeling** (GRCon 2025 paper, [PDF](https://events.gnuradio.org/event/26/contributions/752/attachments/220/586/TorchSig_GRCon2025.pdf)):
    - Probabilistic receive-side impairments: intermodulation (50%), nonlinear amplifier (75%), coarse gain change (25%), spurs (75%), IQ imbalance (50%), phase noise, frequency drift, passband ripple, sample-clock drift/jitter, quantization (75%), digital AGC (25%). A matching transmit list exists.
    - "Bring your own data" to mix real IQ into training.
  - **Recommended hardware:** Ubuntu ≥ 22.04, ≥ 4 cores, ≥ 1 TB storage, and a **≥ 16 GB GPU** — for *training/generation*, not inference.
- **WidebandSig53 paper** ([arXiv 2211.10335](https://arxiv.org/abs/2211.10335)):
  - 550k synthetic wideband examples with ~2M signals across 53 classes.
  - Benchmarks segmentation and object-detection CNNs/transformers on complex spectrograms for detection (time/frequency boxes) and recognition (plus modulation family).
  - This is the academic basis for "YOLO/DETR-on-spectrogram" detectors.
- **RF fingerprinting (Northeastern Genesys lab):** the [ORACLE dataset](https://www.genesys-lab.org/oracle) has 16 USRP X310 transmitters captured OTA at 2–62 ft.
  - The CNN identifies *individual radios* from hardware impairments. Published error rates are ~1.4%, improving to 0.24–0.5% with transmitter-side cooperation (DARPA RFMLS) ([large-scale study](https://ece.northeastern.edu/fac-ece/ioannidis/static/pdf/2020/J_Jian_RFDeepLearning_IoT_2020.pdf)).
  - Relevant for "is this the same emitter I saw last week?" in an inventory. Channel robustness remains the known weak point.

### 4.2 How well AMC works on real OTA data

- **Sim-to-real studies stay optimistic only in narrow settings.**
  - A 2026 *Sensors* study ([PMC](https://pmc.ncbi.nlm.nih.gov/articles/PMC13211014/)) trained CNNs for OFDM subcarrier modulation (BPSK/QPSK/16QAM/64QAM) and tested OTA indoors with bladeRF x40s.
  - Large, sync-impaired synthetic training reached **93.4%** OTA accuracy. Small conducted-hardware training reached 89.4%. 16-QAM was the weakest class at ~81%.
  - Takeaway: scale and diversity of impairments beat data provenance — but this is 4 classes, known band, cooperative link.
- **The MATLAB example's "99% OTA"** used Plutos 2 ft apart (§1.8).
- **The real exploration problem is harder in every dimension:**
  - Unknown number of signals per band, unknown bandwidth and center.
  - Bursty and hopping emitters; co-channel overlap.
  - Classes outside the training set (open-set) and front-end artifacts (images, IMD, spurs from cheap SDRs).
  - A detected "signal" might be a birdie or a harmonic.
- **Classical detection is still a strong baseline.** A Dec 2025 paper ([arXiv 2512.13542](https://arxiv.org/abs/2512.13542), Virginia Tech) compares DNN detectors on raw IQ against statistical detectors and matched filters for signals with *unknown* parameters.
  - The takeaway for us: pair ML with CFAR/energy detection and calibrated false-alarm control rather than replacing it.
- **Practical ML that works today on cheap edge hardware:**
  - **RTL-ML** ([TrevTron/rtl-ml](https://github.com/TrevTron/rtl-ml); [rtl-sdr.com](https://www.rtl-sdr.com/automatic-signal-recognition-with-ai-machine-learning-and-rtl-sdr/), March 2026):
    - A Random Forest over 17 hand-crafted spectral/IQ features, trained on real captures from one location.
    - Classes: FM broadcast, NOAA weather radio/satellite, APRS, ISM, FRS/GMRS, pager, noise.
    - The README reports 96.9% on 160 test samples; the rtl-sdr.com write-up quotes 87.5% over 8 classes (the two sources disagree).
    - A 186 KB model at ~120 ms/inference on a Pi 5.
    - Shows that *service-level* classification (what service is this?) with priors is cheap and useful, even if not generalizable.
  - **Drone detectors** (e.g., [Scientific Reports 2026](https://www.nature.com/articles/s41598-026-48925-1)) use YOLOv5/Faster R-CNN on SDR spectrograms to separate drone control links from Wi-Fi/Bluetooth. Detection-as-object-detection is mature enough for well-defined target sets.
- **Commercial proof points:** OmniSIG (and CRFS/R&S/Aaronia/ThinkRF) show the *product* is feasible, with their own curated real-world training data. DeepSig's own guidance to use real data instead of RadioML is telling.

### 4.3 Open-source implementations and integration status

| Project | What | Integrated in a usable GUI? | Status |
|---|---|---|---|
| TorchSig | Synthetic data, transforms, (external) models | No | Active (v2.2.0, 2026-08) |
| torchsig-models | Reference models (e.g., XCiT narrowband classifier checkpoint) | No | Active, tiny (push 2026-07) |
| gr-inspector | Energy detection, OFDM parameter estimation, TF AMC block, Qt spectrum GUI | GRC widgets only | Stale (GR 3.8; 2025-03) |
| TorchSig GNU Radio block (GRCon 2024 paper) | GR block for inference plus spectrogram tools ([paper](https://pubs.gnuradio.org/index.php/grcon/article/view/147)) | Flowgraph only | Research artifact |
| IQEngine plugins | Detectors (`simple_detector`, `fm_signal_detector`, `markos_detector`) returning SigMF annotations | **Yes — web, offline recordings** | Maintained |
| Qoherent RIA | RF ML tooling | — | **Archived** (2025-09) ([qoherent/ria](https://github.com/qoherent/ria)) |
| Artemis | Database; ML ID "planned" | Database GUI only | Active |
| RTL-ML | Random Forest service classifier on a Pi | CLI | New (2026) |
| Countless "AMC" GitHub repos (RadioML CNN/LSTM/Transformer) | Paper reproductions | No | Mostly abandoned after publication |

**Bottom line:** no maintained open-source *live* receiver GUI integrates ML detection or classification. SDR++, SDRangel, SigDigger, OpenWebRX+ and Gqrx have none. IQEngine does it offline via plugins; commercial suites do it live.

### 4.4 Compute requirements (engineering estimates)

- **Detection on the waterfall** (CFAR/energy on averaged FFT frames): trivial on a Pi 5 CPU at tens of MHz. FPGA FFT (Maia-style) moves it off the CPU entirely.
- **Narrowband classification** (1–4k IQ samples per detected channel, small CNN/ResNet/XCiT-tiny):
  - Milliseconds per inference on a Jetson Orin-class GPU with TensorRT; tens of ms on a Pi 5 CPU.
  - Feasible per detection event, not per sample.
  - **(Estimate, based on typical model sizes; not benchmarked here.)**
- **Wideband spectrogram object detection** (YOLO/DETR on e.g. 512×512 spectrograms): real-time at a few frames/s on Jetson Orin; marginal on a Pi 5 without an accelerator (Hailo/Coral-class NPU) **(estimate)**.
- **The real bottleneck** on a Pi 5 is channelizing and demodulating several simultaneous signals from a 20–60 MS/s stream (USB throughput plus CPU), not the ML itself. The polyphase channelizer should be in FPGA or GPU, or the stream decimated early.

---

## 5. UX Analysis: Why Current Tools Are Bad at Exploration

### 5.1 Concrete problems

1. **Tuning-first mental model.**
   - Every mainstream receiver (SDR#, SDR++, Gqrx, SDRangel, SDRuno, CubicSDR) centers on a VFO. You drag to a frequency, then choose a demodulator.
   - The waterfall is a tuning aid, not an object model. Signals aren't *things* you can list, name, tag or revisit.
2. **Manual mode, bandwidth and squelch selection.**
   - Users must know that 162.55 MHz is NFM at 12.5 kHz, that airband is AM, and so on.
   - Squelch is a raw dB number. Even RTLSDR-Airband, which has good auto-squelch, tells users to "guess the value" in edge cases ([wiki](https://github.com/szpajder/RTLSDR-Airband/wiki/Manual-squelch-setting)).
   - SDR++'s scanner never changes the mode (source review). SDRangel's scanner retunes a demod channel you pre-created with a fixed mode.
3. **Discovery requires outside research.**
   - Trunk Recorder, the best capture-everything tool, still tells users to look up the system on Radio Reference and hunt for the control channel by typing frequencies into GQRX (CONFIGURE.md).
   - SDRTrunk requires hand-built playlists. Nothing *finds* a control channel and proposes "this looks like P25 Phase 1, NAC 0x293, want me to follow it?"
4. **No signal inventory or database.**
   - No open-source receiver keeps a persistent table of detected emitters: first/last seen, center, bandwidth, duty cycle, modulation guess, decoded IDs, recordings.
   - Bookmarks (SDR++ Frequency Manager, OpenWebRX bookmarks, Gqrx) are manual and static. Artemis and sigidwiki are reference databases disconnected from live data.
5. **No persistent survey history.**
   - hackrf_sweep/rtl_power produce CSV files you graph later. QSpectrumAnalyzer shows live sweeps but doesn't keep long-term history.
   - Spectre (HB9TF) and NTIA SCOS are the rare exceptions; neither is tied to a receiver UI.
6. **Poor recording management.**
   - Recordings are loose files named by timestamp and frequency, often WAV/raw without SigMF.
   - SDRangel's SigMF sink with squelch pre-trigger is the best open option, but it isn't linked to detections or an index. inspectrum and IQEngine are separate apps.
7. **Flowgraph-centric GNU Radio UIs.** GRC is a program editor, as users note in the HN thread above. Exploration needs dynamic, data-driven spawning of processing chains, which static flowgraphs make awkward.
8. **Fragmentation.** A realistic "what is this?" workflow today looks like:
   - SDR++ to spot the signal;
   - SigDigger to estimate baud rate;
   - URH (now archived) or inspectrum for bits;
   - rtl_433 `-A` for ISM timing;
   - sigidwiki/Artemis to guess the identity;
   - dsd-fme or SDRTrunk to decode;
   - a text file to remember it.
9. **Installation and polish friction.** A Dec 2024 RadioReference thread complains that SDR++ "didn't want to install cleanly," that vendor software is "nowhere near ready for prime time," and about the lack of written docs ([thread](https://forums.radioreference.com/threads/im-getting-sick-of-this.482612/)). SigDigger's own site says documentation is a work in progress.
10. **Embedded devices are single-purpose.** Mayhem runs one app at a time on a narrow band. Maia SDR has a great waterfall and recorder but no analysis.

*Note: direct Reddit/HN searches were limited in this session. The complaints above are from sources actually retrieved; broader sentiment mining is a follow-up.*

### 5.2 Best UX ideas that already exist (steal these)

| Idea | Where it exists | Why it matters for us |
|---|---|---|
| **Inspector workspace per signal** (click a channel → dedicated tab with estimators, constellation, symbol stream, recording) | SigDigger | The core "select signal on waterfall → analyze/extract" interaction |
| **Automatic demod-parameter estimation** (modulation, symbol length, center/threshold, noise) | URH; SigDigger blind estimation; rtl_433 `-A` pulse analyzer | Removes FM/AM/squelch guesswork for digital bursts |
| **Plugin → annotation model** (detector/classifier returns SigMF annotations drawn on the spectrogram) | IQEngine | Uniform API for adding detectors, classifiers and decoders; everything becomes metadata |
| **Feature plugins that orchestrate channels and devices** (Satellite Tracker loads presets at AOS, Frequency Scanner retunes demods, Map aggregates decodes) | SDRangel | Automation layer above DSP; REST-driven |
| **Capture everything, decide later** (wideband capture, spawn a recorder for every event) | Trunk Recorder; SDRangel SigMF sink with pre-trigger | Never miss the first seconds; don't make the user choose one signal |
| **Background decoding appliance with schedules** | OpenWebRX/OpenWebRX+ | Always-on value; decodes flow into a map/log without a user present |
| **Pipeline registry** (signal type → processing chain → products) and **auto scheduler** | SatDump | "Auto protocol decode" is a lookup from classification result to pipeline |
| **Decode-everything-in-band with no mode selection** | rtl_433; WSJT-X; dump1090/readsb; AIS-catcher | Proves protocol-aware auto-decode works when the signal family is known |
| **Signal lists / tables of all found signals** | Aaronia IQ Pulse Inspector; CRFS DeepView Signal Discovery; OmniSIG JSON output | The inventory view: sortable, filterable, linked to recordings |
| **Occupancy and long-term statistics** | CRFS, R&S ARGUS, Spectre (HB9TF), NTIA SCOS | Survey history: when is this band busy, what changed since the baseline |
| **DPX / persistence / density display** | Tektronix DPX; hackrf-spectrum-analyzer persistence; gr-fosphor | Reveals intermittent and buried signals a normal waterfall averages away |
| **Web UI from an embedded box** (FPGA FFT + REST + WASM/WebGL) | Maia SDR; OpenWebRX; FutureSDR/RustRadio WASM | Phone/tablet as the display for a Pi/Jetson/Pluto device |
| **Offline identification database with filters** (frequency, bandwidth, modulation, ACF) | Artemis / sigidwiki | Bayesian prior for classification and human-readable explanation |
| **Trainable classifier from your own labeled recordings** | DeepSig OmniSIG Studio; RTL-ML (simple version) | Local environments differ; users need to teach the system new signals |

---

## 6. Gap Analysis

Legend: ● = yes / strong; ◐ = partial / manual / limited; ○ = no. "Active" means a push or release within ~12 months of 2026-09 plus visible development.

| Tool | Wideband sweep survey | Real-time channel detection | Auto modulation ID | Auto demod param estimation | Auto protocol decode | Signal DB / inventory | SigMF recording | Embedded / handheld | Open source | Active |
|---|---|---|---|---|---|---|---|---|---|---|
| GNU Radio 3.10 (+OOTs) | ◐ (build it) | ◐ (gr-inspector, stale) | ◐ (gr-inspector TF, stale) | ○ | ◐ (per-OOT) | ○ | ◐ (blocks exist) | ◐ | ● | ● |
| GNU Radio 4.0 RC | ○ | ○ | ○ | ○ | ○ | ○ | ○ | ● (design goal) | ● (MIT core) | ● |
| SDR++ (+Brown) | ○ | ◐ (scanner, threshold) | ○ | ○ | ◐ (Brown: DSD, FT8) | ◐ (bookmarks) | ○ | ◐ (Android) | ● | ● |
| SDRangel | ◐ (scanner over list) | ◐ (Freq Scanner, Channel Power) | ○ | ○ | ◐ (many demods; manual pick) | ○ | ● (SigMF sink, triggered) | ◐ (Android, server mode) | ● | ● |
| SigDigger / Suscan | ◐ (panoramic) | ○ | ◐ (manual inspector type) | ● (blind baud/SNR) | ○ | ○ | ◐ (baseband rec) | ○ | ● | ◐ (single maintainer, no tagged release since 2022) |
| URH | ○ | ◐ (burst detection in captures) | ● (ASK/FSK/PSK) | ● | ◐ (field inference) | ○ | ○ | ○ | ● | ○ (archived 2026) |
| inspectrum | ○ | ○ | ○ | ◐ (manual cursors) | ○ | ○ | ● (reads) | ○ | ● | ◐ |
| rtl_433 | ○ | ● (bursts in band) | ◐ (OOK/FSK) | ● (pulse analyzer) | ● (~380 devices) | ○ | ◐ (can dump IQ; SigMF unverified) | ● (Pi) | ● | ● |
| IQEngine | ○ | ◐ (detector plugins, offline) | ◐ (plugins) | ○ | ◐ (plugins, e.g. SatDump) | ◐ (recording metadata search) | ● | ○ | ● | ◐ |
| OpenWebRX+ | ○ | ○ | ○ | ○ | ● (background, by plan) | ◐ (auto bookmarks, map) | ○ | ● (Pi images) | ● | ● |
| Trunk Recorder | ○ | ◐ (conventional signal detector) | ○ | ◐ (autotune) | ● (P25/SmartNet/DMR) | ◐ (call logs, uploads) | ◐ (SigMF input) | ● (Pi) | ● | ● |
| SDRTrunk | ○ | ○ | ○ | ○ | ● (P25/DMR/LTR/MPT1327) | ◐ (event logs) | ○ | ◐ | ● | ◐ (last final release 2024-12) |
| SatDump | ○ | ○ | ○ | ○ | ● (pipelines + scheduler) | ◐ (products) | ◐ | ● (Pi/Android) | ● | ● |
| hackrf_sweep / rtl_power / QSpectrumAnalyzer | ● | ○ | ○ | ○ | ○ | ○ | ○ | ● | ● | ◐ |
| Spectre (HB9TF) | ● | ○ | ○ | ○ | ○ | ◐ (SQL power history) | ○ | ● | ● | ● |
| Maia SDR | ◐ (61 MHz live) | ○ | ○ | ○ | ○ | ○ | ● | ● (Pluto + phone) | ● | ◐ |
| Mayhem (PortaPack) | ◐ (Looking Glass) | ◐ (recon) | ○ | ○ | ◐ (per-app) | ○ | ○ | ● | ● | ● |
| Artemis / sigidwiki | ○ | ○ | ○ | ○ | ○ | ● (reference DB) | ○ | ○ | ● | ● |
| TorchSig | ○ | ◐ (models, offline) | ● (offline) | ○ | ○ | ○ | ○ | ○ | ● | ● |
| DeepSig OmniSIG | ● (via sensor) | ● | ● | ◐ | ◐ (classification; decode unclear) | ● (JSON/Elastic) | ● | ● (Jetson) | ○ | ● |
| CRFS DeepView / RFeye | ● | ● | ● | ◐ | ◐ | ● (occupancy, discovery) | ◐ (proprietary + export) | ● (nodes) | ○ | ● |
| R&S CA100 / ARGUS | ● | ● | ● (AMMOS) | ● | ● (decoders) | ● | ◐ | ◐ | ○ | ● |
| Aaronia RTSA-Suite PRO | ● | ● | ● (IQ classifier) | ◐ | ◐ | ● (signal table) | ◐ | ◐ | ○ | ● |

**Gaps no open-source tool fills:**

1. **Discovery → decode automation.** Nothing goes wideband energy detection → bandwidth/center estimate → modulation class → parameters → pick pipeline (rtl_433 / DSD / SatDump / multimon-ng / AIS / ADS-B) → decode, without user selection.
2. **A persistent signal inventory** joining detections, classifications, decoded identifiers, recordings (SigMF) and survey history, queryable over time and location.
3. **Live ML in a usable receiver UI.** TorchSig models exist; no live GUI hosts them.
4. **A handheld/embedded exploration instrument** combining Maia-style FPGA/WASM display, SDRangel-class decoders, and the above automation. Mayhem is too constrained; Maia has no analysis.
5. **Maintained protocol reverse-engineering.** URH is archived, so its estimators and inference need a new home.

---

## 7. Implications for This Project (short)

- **Data model first:** make SigMF annotations and a local SQLite/DuckDB "signal inventory" the backbone. Each detection is a row plus an annotation, linked to clips.
- **Build around a pluggable detector → classifier → decoder chain** with an IQEngine-like contract (IQ in → annotations/decoded data out). Wrap existing decoders as subprocesses: rtl_433, dsd-fme, multimon-ng, readsb, AIS-catcher, direwolf, SatDump CLI.
- **Borrow UX patterns:** SigDigger's inspector, Trunk Recorder's capture-everything, OpenWebRX+'s background decoding and SatDump's pipeline registry. Offer DPX-style persistence and CRFS/Aaronia-style signal tables and occupancy.
- **DSP stack candidates:**
  - C/C++: liquid-dsp + SoapySDR (+ VOLK).
  - Rust: FutureSDR (WASM UI, Burn ML hooks).
  - Headless SDRangel via REST as a stop-gap engine.
  - Treat GNU Radio 4 as promising but pre-ecosystem.
- **Classification strategy:**
  1. Classical CFAR detection plus bandwidth/burst statistics.
  2. Service priors (frequency allocation plus Artemis DB).
  3. A small TorchSig-trained classifier fine-tuned on locally captured, user-labeled SigMF.
  4. Protocol-decoder "trial decoding" as the final arbiter: if rtl_433 or DSD locks, that's the ground truth.
- **Embedded:**
  - Pi 5 handles detection plus a few decoders.
  - Jetson Orin handles GPU channelization and live spectrogram object detection.
  - Maia SDR's FPGA-FFT + Rust REST + WASM UI is the reference architecture for a phone-as-display device.

---

## Sources

**Frameworks / libraries**
- GNU Radio repo & releases: https://github.com/gnuradio/gnuradio
- GR4 RC1 news (2026-03-22): https://www.gnuradio.org/news/2026-03-22-gr4-release-candidate-1/
- GR4 RC1 release notes (fair-acc): https://github.com/fair-acc/gnuradio4/releases/tag/4.0.0-RC1
- GR4 community stewardship (2026-05-21): https://www.gnuradio.org/news/2026-05-21-gr4-community-stewardship/
- GR4 official super-repo (2026-08-16): https://www.gnuradio.org/news/2026-08-16-gr4-easy-to-build/
- OpenDigitizer: https://github.com/fair-acc/opendigitizer ; IPAC'23: https://indico.jacow.org/event/41/contributions/2575/
- GNU Radio World: https://gnuradioworld.com/ ; https://www.rtl-sdr.com/gnu-radio-world-browser-based-gnu-radio-flowgraphs/ ; HN: https://news.ycombinator.com/item?id=49628576
- gr-osmosdr: https://github.com/osmocom/gr-osmosdr ; gr-inspector: https://github.com/gnuradio/gr-inspector ; gr-fosphor: https://github.com/osmocom/gr-fosphor
- SoapySDR: https://github.com/pothosware/SoapySDR ; UHD: https://github.com/EttusResearch/uhd
- liquid-dsp: https://github.com/jgaeddert/liquid-dsp ; VOLK: https://github.com/gnuradio/volk
- csdr: https://github.com/jketterl/csdr ; https://github.com/luarvique/csdr
- FutureSDR: https://github.com/FutureSDR/FutureSDR ; https://www.futuresdr.org/learn/
- RustRadio: https://github.com/ThomasHabets/rustradio ; https://blog.habets.se/2026/04/Rustradio-SDR-framework-now-also-in-the-browser-with-wasm.html
- Pothos: https://github.com/pothosware/PothosCore ; LuaRadio: https://github.com/vsergeev/luaradio ; REDHAWK: https://github.com/RedhawkSDR/redhawk
- SigMF: https://github.com/sigmf/SigMF ; sigmf-python: https://github.com/sigmf/sigmf-python
- cuSignal (archived): https://github.com/rapidsai/cusignal ; Holoscan SDK: https://github.com/nvidia-holoscan/holoscan-sdk ; HoloHub blog: https://developer.nvidia.com/blog/developing-streaming-sensor-applications-with-holohub-from-nvidia-holoscan/
- Sionna: https://github.com/NVlabs/sionna ; Sionna 2.0: https://forums.developer.nvidia.com/t/sionna-2-0-pytorch-native-same-api/364087
- MATLAB AMC example: https://www.mathworks.com/help/comm/ug/modulation-classification-with-deep-learning.html
- pyrtlsdr: https://github.com/pyrtlsdr/pyrtlsdr ; PySDR: https://pysdr.org/ , https://github.com/777arc/PySDR

**Receivers**
- SDR++: https://github.com/AlexandreRouma/SDRPlusPlus ; misc modules: https://github.com/AlexandreRouma/SDRPlusPlus/tree/master/misc_modules ; scanner source: https://github.com/AlexandreRouma/SDRPlusPlus/blob/master/misc_modules/scanner/src/main.cpp
- SDR++ Brown: https://github.com/sannysanoff/SDRPlusPlusBrown ; https://sdrpp-brown.san.systems/ ; https://www.rtl-sdr.com/techminds-testing-the-sdr-brown-fork-with-built-in-dsd-and-remote-kiwisdr-support/
- HydraSDR SDR++ fork: https://github.com/hydrasdr/SDRPlusPlus
- SDRangel: https://github.com/f4exb/sdrangel ; https://www.sdrangel.org/ ; channel Rx plugins: https://github.com/f4exb/sdrangel/tree/master/plugins/channelrx ; feature plugins: https://github.com/f4exb/sdrangel/tree/master/plugins/feature
- SDRangel Frequency Scanner: https://github.com/f4exb/sdrangel/blob/master/plugins/channelrx/freqscanner/readme.md
- SDRangel SigMF File Sink: https://github.com/f4exb/sdrangel/blob/master/plugins/channelrx/sigmffilesink/readme.md
- SDRangel Channel Power: https://github.com/f4exb/sdrangel/blob/master/plugins/channelrx/channelpower/readme.md ; Heat Map: https://github.com/f4exb/sdrangel/blob/master/plugins/channelrx/heatmap/readme.md
- SDRangel Star Tracker: https://github.com/f4exb/sdrangel/blob/master/plugins/feature/startracker/readme.md ; Satellite Tracker: https://github.com/f4exb/sdrangel/blob/master/plugins/feature/satellitetracker/readme.md
- SDRangel 7.27.1: https://machamradio.com/blog/2026/07/14/sdrangel-v7-27-1-released/
- Gqrx: https://github.com/gqrx-sdr/gqrx ; CubicSDR: https://github.com/cjcliffe/CubicSDR
- SDR Console trunk following: https://www.sdr-radio.com/trunk-voice-following
- AetherSDR: https://github.com/aethersdr/AetherSDR ; Aether-gate: https://github.com/nigelfenton/Aether-gate
- OpenWebRX: https://github.com/jketterl/openwebrx ; background decoding: https://github.com/jketterl/openwebrx/wiki/Background-decoding ; OpenWebRX+: https://github.com/luarvique/openwebrx
- KiwiSDR: https://github.com/jks-prv/KiwiSDR ; https://github.com/jks-prv/Beagle_SDR_GPS
- Maia SDR: https://maia-sdr.org/ ; https://github.com/maia-sdr/maia-sdr ; https://destevez.net/2023/02/maia-sdr/ ; firmware: https://github.com/maia-sdr/plutosdr-fw/releases
- Mayhem: https://github.com/portapack-mayhem/mayhem-firmware ; releases: https://github.com/portapack-mayhem/mayhem-firmware/releases

**Scanning / survey / decoding**
- HackRF: https://github.com/greatscottgadgets/hackrf ; hackrf_sweeper: https://github.com/subreption/hackrf_sweeper
- rtl-sdr-misc (heatmap.py): https://github.com/keenerd/rtl-sdr-misc ; soapy_power: https://github.com/xmikos/soapy_power ; QSpectrumAnalyzer: https://github.com/xmikos/qspectrumanalyzer ; SpectroScope: https://github.com/konung-yaropolk/SpectroScope ; spektrum: https://github.com/pavels/spektrum ; hackrf-spectrum-analyzer: https://github.com/pavsa/hackrf-spectrum-analyzer
- Spectre (HB9TF): https://github.com/hb9tf/spectre ; https://medium.com/hb9tf/rf-spectrum-analysis-open-sourced-7ec5911fc2
- NTIA SCOS: https://github.com/NTIA/scos-sensor ; https://github.com/NTIA/scos-actions ; https://its.ntia.gov/research/rfm/spectrum-monitoring/standardization
- ElectroSense paper: https://arxiv.org/abs/1703.09989
- RTLSDR-Airband squelch: https://github.com/szpajder/RTLSDR-Airband/wiki/Manual-squelch-setting
- rtl_433: https://github.com/merbanan/rtl_433 ; ANALYZE.md: https://github.com/merbanan/rtl_433/blob/master/docs/ANALYZE.md
- URH: https://github.com/jopohl/urh ; WOOT'18: https://www.usenix.org/conference/woot18/presentation/pohl ; WOOT'19: https://www.usenix.org/conference/woot19/presentation/pohl ; ACM: https://dl.acm.org/doi/10.1145/3375894.3375896
- inspectrum: https://github.com/miek/inspectrum
- SigDigger: https://github.com/BatchDrake/SigDigger ; https://batchdrake.github.io/SigDigger/ ; dev build: https://github.com/BatchDrake/SigDigger/releases/tag/latest ; Suscan: https://github.com/BatchDrake/suscan
- IQEngine: https://github.com/IQEngine/IQEngine ; plugin docs: https://github.com/IQEngine/IQEngine/blob/main/client/src/pages/docs/plugins.mdx ; https://www.rtl-sdr.com/iqengine-a-web-based-toolkit-for-sharing-and-analyzing-rf-iq-recordings/
- SDRTrunk: https://github.com/DSheirer/sdrtrunk ; sdrtrunk-vce: https://github.com/tylerwatt12/sdrtrunk-vce
- OP25: https://github.com/boatbod/op25
- Trunk Recorder: https://github.com/TrunkRecorder/trunk-recorder ; https://trunkrecorder.com/docs/intro ; CONFIGURE.md: https://github.com/TrunkRecorder/trunk-recorder/blob/master/docs/CONFIGURE.md
- DSDPlus: https://wiki.radioreference.com/index.php/DSDPlus ; https://www.rtl-sdr.com/signalseverywhere-using-dsdplus-fastlane-for-listening-to-phase-1-p25-trunking/ ; dsd-fme: https://github.com/lwvmobile/dsd-fme
- dump1090: https://github.com/flightaware/dump1090 ; readsb: https://github.com/wiedehopf/readsb ; AIS-catcher: https://github.com/jvde-github/AIS-catcher ; multimon-ng: https://github.com/EliasOenal/multimon-ng ; direwolf: https://github.com/wb2osz/direwolf ; radiosonde_auto_rx: https://github.com/projecthorus/radiosonde_auto_rx
- SatDump: https://github.com/SatDump/SatDump ; https://docs.satdump.org/index.html ; https://docs.satdump.org/sat_tracker.html ; https://www.rtl-sdr.com/moving-satdump-towards-v2-0-0/
- gr-satellites: https://github.com/daniestevez/gr-satellites
- Signal Identification Wiki: https://www.sigidwiki.com/wiki/Signal_Identification_Guide ; Artemis: https://github.com/AresValley/Artemis ; https://www.rtl-sdr.com/artemis-4-released-offline-signal-identification-database/
- TempestSDR: https://github.com/martinmarinov/TempestSDR
- Kestrel TSCM: https://kestreltscm.com/
- RFNM: https://rfnm.com/ ; https://www.rtl-sdr.com/an-initial-review-of-the-rfnm-software-defined-radio/

**Commercial**
- DeepSig OmniSIG: https://www.deepsig.ai/omnisig/ ; GTC DC 2025: https://www.businesswire.com/news/home/20251028742262/en/DeepSig-Demonstrates-AI-Native-Wireless-Intelligence-at-NVIDIA-GTC-DC-2025
- CRFS DeepView: https://www.crfs.com/software/rfeye-deepview
- R&S CA100: https://www.rohde-schwarz.com/us/products/aerospace-defense-security/online-signal-analysis/rs-ca100-pc-based-signal-analysis-and-signal-processing-software_63493-52992.html ; ARGUS: https://www.rohde-schwarz.com/us/products/aerospace-defense-security/radiomonitoring-software/rs-argus_63493-10783.html
- Aaronia IQ Pulse Inspector: https://aaronia.com/en/iq-pulse-inspector ; RTSA blocks: https://downloads.aaronia.com/manuals/RTSA-Suite_PRO_Software-Blocks.pdf
- Tektronix DPX primer: https://www.tek.com/en/documents/primer/dpx-acquisition-technology-spectrum-analyzers-fundamentals
- Keysight 89600 VSA: https://www.keysight.com/us/en/products/software/pathwave-test-software/89600-vsa-software.html
- Signal Hound Spike: https://signalhound.com/spike/ ; pulse analysis: https://signalhound.com/products/pulse-analysis/
- ThinkRF: https://thinkrf.com/
- Distributed Spectrum: https://medium.com/@distributedspectrum/why-we-built-distributed-spectrum-ff9ebd026da2 ; https://www.startuphub.ai/startups/distributed-spectrum

**ML / AMC**
- DeepSig datasets (RadioML): https://www.deepsig.ai/datasets/
- TorchSig: https://github.com/TorchDSP/torchsig ; releases: https://github.com/TorchDSP/torchsig/releases ; torchsig-models: https://github.com/TorchDSP/torchsig-models ; GRCon 2025 paper: https://events.gnuradio.org/event/26/contributions/752/attachments/220/586/TorchSig_GRCon2025.pdf ; GRCon 2024 GR block paper: https://pubs.gnuradio.org/index.php/grcon/article/view/147
- WidebandSig53: https://arxiv.org/abs/2211.10335
- DL detection with unknown parameters (2025): https://arxiv.org/abs/2512.13542
- OTA AMC sim-to-real (Sensors 2026): https://pmc.ncbi.nlm.nih.gov/articles/PMC13211014/
- ORACLE RF fingerprinting: https://www.genesys-lab.org/oracle ; https://ece.northeastern.edu/fac-ece/ioannidis/static/pdf/2020/J_Jian_RFDeepLearning_IoT_2020.pdf
- RTL-ML: https://github.com/TrevTron/rtl-ml ; https://www.rtl-sdr.com/automatic-signal-recognition-with-ai-machine-learning-and-rtl-sdr/
- SDR drone detection with YOLO (Sci Rep 2026): https://www.nature.com/articles/s41598-026-48925-1
- Qoherent RIA (archived): https://github.com/qoherent/ria

**UX / community**
- RadioReference "I'm getting sick of this..." (Dec 2024): https://forums.radioreference.com/threads/im-getting-sick-of-this.482612/
- HN, GNU Radio World discussion: https://news.ycombinator.com/item?id=49628576
