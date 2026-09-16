# 02 — SDR Hardware & Compute Landscape for an Exploration-First Signals Device

*Research snapshot: 2026-09-13. Prices are USD list/retail prices seen during research and change often. Several products are crowdfunded, clones, or quote-only. Items marked **(verify)** could not be confirmed from a primary source.*

---

## 0. Executive summary

- **Exploration is limited by the analog front end more than by the ADC.** Whether an unknown signal can be found in a city depends on preselection, linearity (IIP3), LO phase noise and gain control as much as on instantaneous bandwidth (IBW). Wide IBW with few bits and no filtering mostly shows you your own intermodulation.
- **There are two ways to survey a band, and a good device does both:**
  - **Sweep:** retune quickly and stitch FFTs together. HackRF's `hackrf_sweep` does ~8 GHz/s ([rtl-sdr.com](https://www.rtl-sdr.com/scanning-spectrum-8ghz-per-second-new-hackrf-update/)), the Signal Hound BB60D 24 GHz/s ([Signal Hound](https://signalhound.com/products/bb60d-6-ghz-real-time-spectrum-analyzer/)), and the SM200C 1 THz/s ([Signal Hound](https://signalhound.com/products/sm200c-20-ghz-real-time-spectrum-analyzer-with-10gbe/)).
  - **Real-time IBW:** capture gap-free across 20–400+ MHz, so short bursts are not missed.
- **The host link sets the ceiling.** Rough limits: USB 2.0 carries ~20 Msps of 8-bit IQ, USB 3.0 ~60–120 Msps, and PCIe or 10/100 GbE is needed beyond that. The Raspberry Pi 5 has a documented USB 3 streaming regression with bladeRF ([RPi forum](https://forums.raspberrypi.com/viewtopic.php?t=374733)). Its PCIe 2.0 x1 slot is the more promising path for serious SDRs, but it is still immature.
- **Embedded CPU+GPU+FPGA+RF is now a commercial pattern.** Epiq's Matchstiq X40/G-series and Deepwave's AIR-T Embedded all pair an FPGA and RF transceiver with a **Jetson Orin NX**. That validates the Jetson path for on-device classification ([LinuxGizmos](https://linuxgizmos.com/epiq-solutions-matchstiq-x40-and-g-series-for-edge-level-ai-ml-rf-spectrum-analysis/), [Deepwave](https://deepwave.ai/hardware-products/air-t-embedded-series/)).
- **New entrants in 2024–2026 that matter for this project:**
  - HackRF Pro (FPGA, TCXO, 100 kHz–6 GHz, PortaPack-compatible, ~$400)
  - M.2/PCIe SDR cards: LiteX-M2SDR, Wavelet xSDR/sSDR, LimeSDR XTRX, Epiq NV100
  - HydraSDR RFOne
  - Lower-cost RFSoC boards: PZSDR at $8,749, ALINX AXW22 at ~$8.7k, RFSoC 4x2 at $2,499 (academic only)
  - Keysight's 2025 FieldFox with 120 MHz IQ streaming

---

## 1. SDR fundamentals that matter for exploration hardware

### 1.1 Receiver architectures

| Architecture | How it works | Examples | Strengths for exploration | Weaknesses |
|---|---|---|---|---|
| **Direct (RF) sampling** | The ADC digitizes RF directly, either at baseband or in a higher Nyquist zone. Tuning is done digitally with NCO + DDC. | RFSoC boards (ZCU208, RFSoC 4x2, PZSDR, USRP X440), RX888 MkII HF path, KiwiSDR, Red Pitaya SDRlab | No LO, so no LO phase noise, DC spike or IQ imbalance. Multi-GHz of instantaneous spectrum. Simultaneous multi-band capture. | Needs anti-alias and Nyquist-zone filters. The whole band's energy hits one ADC, so blockers set the dynamic range. Huge data rates (5 GSPS × 14-bit). High power. |
| **Zero-IF (direct conversion)** | A quadrature mixer puts the tuned frequency at DC; I and Q are sampled separately. | AD9361/AD9364/AD9363 (bladeRF, USRP B2xx, Pluto, AntSDR), LMS7002M (LimeSDR, xSDR), ADRV9002/9009, HackRF's MAX2837 back end | Cheap, highly integrated 2×2 MIMO. Wide tuning (70 MHz–6 GHz). IBW up to 56–200 MHz. | DC offset/LO-leak spike at center. IQ-imbalance images mirrored around the center. Harmonic mixing responses. Relies on on-chip calibration ([PySDR](https://pysdr.org/content/sampling), [bladeRF wiki](https://github.com/Nuand/bladeRF/wiki/DC-offset-and-IQ-Imbalance-Correction)). |
| **Low-IF** | Mixes to a small nonzero IF (a few MHz) so DC and flicker noise fall outside the wanted band. | Rafael R820T2/R828D (RTL-SDR, Airspy R2/Mini, HydraSDR, KrakenSDR); SDRplay MSi001-based | Avoids the DC spike. Good sensitivity per dollar. | IBW limited to ~2–10 MHz. Image rejection still depends on IQ balance. |
| **Superheterodyne (single/multi-conversion)** | RF preselection, then mixing to a fixed IF with sharp IF filters, then sampling. | Lab/handheld spectrum analyzers (Signal Hound, R&S, Keysight), CRFS RFeye, ThinkRF; HackRF's up/down-conversion front end | Best image rejection and selectivity. Filtering happens before the ADC sees blockers. Enables high sweep rates with narrow RBW. | More parts, cost and power. Multiple LO spurs to manage. IBW limited by IF filter/ADC. |
| **Hybrid** | E.g., HackRF: an RFFC507x mixer shifts RF to a ~2.3–2.7 GHz IF, then a MAX2837 does zero-IF. Airspy HF+ uses a *polyphase harmonic-rejection mixer* with tracking RF filters and sigma-delta ADCs ([Airspy](https://airspy.com/airspy-hf-discovery/)). | HackRF One/Pro, Airspy HF+ Discovery | Wide coverage from a few ICs (HackRF), or exceptional HF blocking dynamic range (HF+: 110 dB) | Mixed spur signatures; each product needs its own calibration. |

**IQ sampling.** A complex sample stream at rate *Fs* represents *Fs* Hz of spectrum centered on the LO: from −Fs/2 to +Fs/2. "Usable" IBW is always somewhat less than *Fs* because of anti-alias filter roll-off. For example, Airspy R2 outputs 10 Msps and documents ~9 MHz alias/image-free ([Airspy R2](https://airspy.com/airspy-r2/)).

**Instantaneous bandwidth vs tuning range.** Tuning range, e.g. 70 MHz–6 GHz, is where the LO can go. IBW is how much you see at once. A 6 GHz sweep at 20 MHz IBW needs ≥300 retunes. With ~1–2 ms of PLL settling plus an FFT dwell per step, that is why `hackrf_sweep` runs at ~8 GHz/s ([HackRF tools docs](https://hackrf.readthedocs.io/en/latest/hackrf_tools.html)). Anything shorter than the revisit time can be missed. That probability-of-intercept (POI) problem is the reason wide real-time IBW is valuable.

### 1.2 ADC bit depth, SNR, ENOB, SFDR

- **Ideal quantization SNR** is 6.02·N + 1.76 dB over Nyquist ([ADI MT-003](https://www.analog.com/media/en/training-seminars/tutorials/MT-003.pdf)): 8-bit ≈ 50 dB, 12-bit ≈ 74 dB, 14-bit ≈ 86 dB, 16-bit ≈ 98 dB.
- **ENOB** is the *effective* bit count derived from measured SINAD, and is always below nominal. Airspy R2's 12-bit ADC delivers 10.4 ENOB and 95 dB SFDR ([Airspy R2](https://airspy.com/airspy-r2/)).
- **SFDR** is the ratio of the carrier to the worst spur. MT-003 notes it "represents the smallest value of signal that can be distinguished from a large interfering signal (blocker)". For an *automatic detector*, SFDR sets the false-alarm floor, because every spur looks like a signal.
- **Processing gain.** An FFT with resolution bandwidth RBW gains 10·log10((Fs/2)/RBW) over the Nyquist-band SNR. For example, 20 Msps with 10 kHz bins gives +30 dB. So an 8-bit HackRF can show weak signals when the band is *quiet*. What it cannot do is show a weak signal next to a strong one, because instantaneous dynamic range is set by the strongest signal in the IBW.
- **Implication.** The wider the IBW, the more likely a strong emitter (broadcast FM, cellular downlink, a nearby Wi-Fi AP) sits inside it and sets gain. Wide real-time capture therefore needs *more bits and better linearity*, not fewer. This is why 14-bit SDRplay units and 16-bit designs such as RX888, Epiq NV100 (ADRV9004) and Per Vices exist.

### 1.3 Noise figure and sensitivity

- The thermal noise floor is −174 dBm/Hz + NF. A 3.5 dB NF receiver (Airspy R2, HydraSDR RFOne between 42–1002 MHz) in a 10 kHz bin sits near −130 dBm.
- Great Scott Gadgets measured the HackRF Pro's noise figure as "solid improvement across almost the whole tuning range, with significant improvement at higher frequencies". In an ADS-B test it decoded roughly double the valid messages of a HackRF One and gained 15–50 km of range ([GSG](https://greatscottgadgets.com/2025/12-03-hackrf-pro-receive-sensitivity-and-noise-figure/)).
- **In urban exploration, low NF is often less useful than high IIP3.** The external noise floor (man-made noise) frequently exceeds the receiver's own noise below ~200 MHz. An LNA then only raises intermodulation.

### 1.4 LO phase noise and reciprocal mixing

- A strong blocker mixes with the LO's phase-noise skirt and deposits noise on nearby channels ([Electronics Notes](https://www.electronics-notes.com/articles/radio/radio-receiver-sensitivity/reciprocal-mixing.php); [ADI article](https://www.analog.com/en/resources/technical-articles/high-dynamic-range-rf-transceiver-solves-the-blocking-challenge.html)).
- Worked example: a −20 dBm blocker with LO phase noise of −110 dBc/Hz at the offset of interest gives −130 dBm/Hz of added noise. That is **~40 dB above** the thermal floor of a 4 dB-NF receiver.
- The fixes are RF filtering *before* conversion, or better synthesizers. Instrument-grade numbers are around −110 dBc/Hz at 10 kHz offset at 1 GHz (Harogic SAN-60 ([Harogic](https://www.harogic.com/product/san-45-usb-spectrum-analyzer/))). RFNM advertises a Si5510 reference with 47 fs jitter ([RFNM](https://rfnm.com/)).
- Direct-sampling RFSoC designs replace LO phase noise with *sample-clock jitter*, which matters equally at multi-GHz input frequencies.

### 1.5 DC offset, IQ imbalance, image rejection, harmonic responses

- **DC spike.** Zero-IF receivers show a spike at 0 Hz from ADC offset, LPF bias and LO leakage. The standard fixes:
  - *Offset tuning:* tune the LO a few hundred kHz away and shift the signal digitally ([PySDR](https://pysdr.org/content/sampling)).
  - *Interleaved sweeps* that discard the center bins, which `hackrf_sweep` does ([rtl-sdr.com](https://www.rtl-sdr.com/scanning-spectrum-8ghz-per-second-new-hackrf-update/)).
  - HackRF Pro lists "DC spike elimination" as a new feature ([GSG](https://greatscottgadgets.com/hackrf/pro/)).
- **IQ imbalance → image.** Image rejection ratio is approximately 10·log10(4/(ε² + θ²)), where ε is the gain error and θ the phase error in radians. For 1% gain and 1° phase error this gives ~40 dB. So a +0 dBm-equivalent signal leaves an image around −40 dBc on the mirrored frequency. An automated detector will report it as a real signal unless QEC (on-chip quadrature-error correction in AD9361/ADRV900x) is tracking, or the software checks for mirror pairs.
- **Harmonic mixing.** Wideband square-wave-driven mixers also respond at 3×LO and 5×LO. Without a low-pass or tracking preselector, a 300 MHz tune can "see" 900 MHz energy.

### 1.6 Preselection filters: why they matter for wideband surveys

- A preselector is a bank of sub-octave or tracking RF filters ahead of the LNA/mixer. It (1) removes out-of-band blockers before they create IMD or reciprocal mixing, (2) suppresses image and harmonic responses, and (3) lets front-end gain be optimized per band.
- Evidence that the market values it:
  - SDRplay RSPdx-R2 has 12 band filters plus MW/FM/DAB notches ([Elektor](https://www.elektor.com/products/sdrplay-rspdx-r2-single-tuner-14-bit-sdr-receiver-1-khz-to-2-ghz)).
  - Signal Hound's BB60D added preselector filters from 130 MHz–6 GHz and gained 10 dB of dynamic range over the BB60C ([Signal Hound](https://signalhound.com/products/bb60d-6-ghz-real-time-spectrum-analyzer/)).
  - Epiq's NV100 integrates Rx preselect filters on an M.2 card ([Epiq](https://militaryembedded.com/comms/sdr/epiq-solutions-announces-new-sidekiq-nv100-an-embeddable-software-defined-radio-sdr-with-extended-rf-tuning-and-high-dynamic-range)).
  - Airspy R2 and HydraSDR use tracking RF filters.
- **Design takeaway:** a sweep-synchronized, switched filter bank is the single highest-leverage addition to a HackRF/AD9361-class front end. Opera Cake already supports frequency-based port switching during `hackrf_sweep` ([GSG Opera Cake](https://greatscottgadgets.com/hackrf/operacake/)).

### 1.7 Overload and intermodulation in urban RF

- Broadcast FM, TV, cellular downlinks and paging transmitters cause overload and IMD even when you are tuned far away. Symptoms are ghost WFM signals appearing at other frequencies as gain rises ([rtl-sdr.com FM filter](https://www.rtl-sdr.com/rtl-sdr-com-broadcast-fm-band-stop-filter-88-108-mhz-reject-now-for-sale/)).
- Third-order IMD grows 3 dB for every 1 dB of input. Several equally strong carriers add roughly 3 dB of composite power per carrier ([rtl-sdr.com overload tag](https://www.rtl-sdr.com/tag/overload/)).
- A commercial 88–108 MHz band-stop provides >50 dB rejection with <0.5 dB insertion loss up to 1 GHz.
- An exploration device should do three things:
  1. **Auto-detect overload.** Check ADC clipping counters, and test for spurs that shift when gain or attenuation changes: real signals don't move, IMD products change level disproportionately.
  2. **Include a step attenuator.** RX888 MkII has 0–31.5 dB on HF ([RX-888](https://www.rx-888.com/rx/)); KiwiSDR 2 added a digital attenuator ([rtl-sdr.com](https://www.rtl-sdr.com/kiwisdr-2-now-available-for-purchase/)).
  3. **Keep switchable notch filters** for local broadcast bands.

### 1.8 Clock accuracy (TCXO/OCXO/GPSDO)

- Frequency error scales with RF. 1 ppm at 2.4 GHz is 2.4 kHz, enough to misplace narrowband channels. The original Pluto's 25 ppm crystal (upgraded to 0.5 ppm on Pluto+ ([Pluto+ listing](https://www.amazon.com/Transceiver-70MHz-6GHz-Compatible-Open-Source-Development/dp/B0FSWWL3M5))) can be 60 kHz off at 2.4 GHz.
- Current references: RTL-SDR Blog V4 has a 1 ppm TCXO, RSPdx-R2 a 0.5 ppm TCXO, HydraSDR 0.5 ppm, and HackRF Pro adds a built-in TCXO.
- A **GPSDO** gives long-term accuracy approaching 1×10⁻¹² (Leo Bodnar LBE-1420 ([Leo Bodnar](https://www.leobodnar.com/shop/index.php?main_page=product_info&products_id=393))), plus 1PPS for timestamping. It matters for TDoA geolocation, coherent multi-device operation and protocol-ID features that key on exact channel rasters.
- Examples with GPSDOs: Ettus sells a board-mounted GPSDO for B210 at $1,502 ([Ettus](https://www.ettus.com/all-products/ub210-kit/)); the X420 has one built in; the Epiq NV100 integrates one.

### 1.9 Coherent multi-channel (for direction finding)

- Angle-of-arrival DF (interferometry, correlative DF, MUSIC) needs channels sharing **one LO and one sample clock**, plus periodic phase calibration.
- Options:
  - **KrakenSDR:** 5 RTL-SDR channels on a common LO with a built-in noise source and switches for automatic coherence calibration ([Crowd Supply](https://www.crowdsupply.com/krakenrf/krakensdr); [CNX](https://www.cnx-software.com/2022/01/31/krakensdr-is-a-5-channel-software-defined-radio-based-on-rtl-sdr/)).
  - **RSPduo:** phase-coherent dual tuner at 2 MHz each ([rtl-sdr.com](https://www.rtl-sdr.com/sdrplay-release-a-dual-tuner-sdr-called-rspduo/)).
  - **AD9361 2×2 boards** (bladeRF, B210, AntSDR) share an LO but have a phase ambiguity at each retune that must be calibrated.
  - **ADRV9009 4-Rx** (Sidekiq X4); **USRP X410** with 4 channels at 400 MHz each.
  - **RFSoC:** multi-tile synchronization across 4–8 ADCs.
- For a handheld, 2–5 coherent channels with a cal source is the realistic target.

---

## 2. SDR hardware comparison (2025–2026)

### 2.1 Master spec table

Legend: **IBW** = usable instantaneous bandwidth (max sample rate in parentheses). **Best for:** S = sweep survey, RT = real-time wide IBW, HF = HF/VLF quality, DF = coherent direction finding, EMB = embeddable (M.2/mPCIe/SoC).

| Device | Freq range | IBW (max Fs) | ADC bits | TX | RX ch | Host interface | Price (USD) | Host needs / notes | Best for |
|---|---|---|---|---|---|---|---|---|---|
| **RTL-SDR Blog V4** | 0.5 kHz(HF upconv)/500 kHz–1.766 GHz | 2.4 MHz stable (3.2 Msps) | 8 | No | 1 | USB 2.0 | $29.95 dongle ([rtl-sdr.com](https://www.rtl-sdr.com/rtl-sdr-blog-v4-dongle-initial-release/)) | Any SBC; R828D tuner, triplexed input, 1 ppm TCXO, bias-tee | Cheap S (slow), spare receiver |
| **Airspy R2** | 24–1700 MHz | ~9 MHz (10 Msps) | 12 (10.4 ENOB) | No | 1 | USB 2.0 | ~$169 **(verify)** | Pi 4 drops samples at 10 Msps (FlightAware reports) | RT narrow, good SFDR |
| **Airspy Mini** | 24–1700 MHz | ~6 MHz (6 Msps) | 12 | No | 1 | USB 2.0 | ~$99–130 **(verify)** | Light CPU | Portable VHF/UHF |
| **Airspy HF+ Discovery** | 0.5 kHz–31 MHz; 64–260 MHz | 660 kHz (768 ksps) | Σ-Δ, 18-bit DDC | No | 1 | USB 2.0 | $169 ([rtl-sdr.com](https://www.rtl-sdr.com/airspy-hf-discovery-collection-of-tests-and-reviews/)) | 110 dB HF blocking DR | HF |
| **HydraSDR RFOne** *(new 2025)* | 24 MHz–1.8 GHz | 10 MHz (10 Msps) | 12 | No | 1 | USB 2.0 (Type-C) | $189 DigiKey ([rtl-sdr.com](https://www.rtl-sdr.com/hydrasdr-rfone-an-new-upcoming-sdr-similar-to-the-airspy-r2/)) | Airspy-R2-like; 3.5 dB NF, 35 dBm IIP3, <2 W | RT narrow |
| **SDRplay RSP1B** | 1 kHz–2 GHz | 10 MHz | 14 | No | 1 | USB 2.0 | ~$143 ([SDRplay](https://sdrplay.com/product/rsp1b/)) | Closed API (SDRplay API), ARM Linux supported | RT narrow, preselected |
| **SDRplay RSPdx-R2** | 1 kHz–2 GHz | 10 MHz | 14 | No | 1 (3 ant ports) | USB 2.0 | $235 MSRP ([rtl-sdr.com](https://www.rtl-sdr.com/sdrplay-rspdx-r2-released/)) | 12 preselect filters, HDR mode <2 MHz, 0.5 ppm TCXO | HF + VHF/UHF, preselected |
| **SDRplay RSPduo** | 1 kHz–2 GHz | 10 MHz single; 2×2 MHz dual | 14 | No | 2 coherent | USB 2.0 | ~£214 ex VAT (~$290) ([SDRplay](https://sdrplay.com/product/rspduo/)) | Dual coherent tuners | Budget DF (2-ch) |
| **RX888 MkII** | 1 kHz–64 MHz direct; 64–1700 MHz via R828D | 64 MHz HF / 10 MHz VHF | 16 (LTC2208) | No | 1 | USB 3.0 | ~$150–250 **(verify; many clones)** | Needs fast USB 3 host + CPU for 64 Msps | HF full-band RT |
| **KiwiSDR 2** | 10 kHz–30 MHz | 30 MHz (web, 4 users) | 14 | No | 1 | Ethernet (BeagleBone) | $395 set ([rtl-sdr.com](https://www.rtl-sdr.com/kiwisdr-2-now-available-for-purchase/)) | Self-hosted web SDR | HF remote |
| **HackRF One** | 1 MHz–6 GHz | ~20 MHz (20 Msps) | 8 | Half-duplex | 1 | USB 2.0 | ~$300–350 **(verify)** | Still listed by GSG; one secondary source claims discontinued **(status unverified)** ([GSG](https://greatscottgadgets.com/hackrf/one/)) | S (8 GHz/s), PortaPack |
| **HackRF Pro** *(new 2025)* | 100 kHz–6 GHz (tunes 0–7.1 GHz) | 20 MHz @ 8-bit; 4-bit up to 40 Msps; 16-bit extended precision | 8 (4/16 modes) | Half-duplex | 1 | USB 2.0 HS, Type-C | ~$400 ([rtl-sdr.com](https://www.rtl-sdr.com/hackrf-pro-pre-order-frequency-range-and-rf-performance-improvements-usb-c-tcxo-added/)) | FPGA (was CPLD), TCXO, clock I/O, trigger connectors, shielding; PortaPack/Opera Cake compatible ([GSG](https://greatscottgadgets.com/hackrf/pro/)) | S + handheld base |
| **LimeSDR Mini 2.0** | 10 MHz–3.5 GHz | 40 MHz (30.72 Msps) | 12 | Full-duplex | 1 | USB 3.0 | $399 ([CNX](https://www.cnx-software.com/2022/06/09/limesdr-mini-2-usb-sdr-lattice-semi-ecp5-fpga/)) | ECP5 FPGA (open toolchain possible) | RT mid |
| **LimeSDR XTRX** | 30 MHz–3.8 GHz | 120 MHz (120 Msps SISO / 90 MIMO) | 12 | Full-duplex | 2 | mini PCIe | $699 CS; ~$608–624 retail ([Lime](https://limemicro.com/story/lime-micro-opens-orders-for-the-limesdr-xtrx-an-open-hardware-mpcie-software-defined-radio/)) | Artix-7 50T; runs on Pi 5 via mPCIe HAT but runs hot (needs fan) ([MyriadRF forum](https://discourse.myriadrf.org/t/limesdr-xtrx-operation-on-raspberry-pi-5-via-mpcie-hat/8850)) | RT wide, EMB |
| **bladeRF 2.0 micro xA4 / xA9** | 47 MHz–6 GHz | 56 MHz (61.44 Msps; 122.88 Msps 8-bit mode) | 12 | Full-duplex | 2 | USB 3.0 | $540 / $860 ([Nuand](https://www.nuand.com/product/bladerf-xa4/)) | Cyclone V 49 kLE / 301 kLE; bus-powered possible | RT mid; FPGA accel (xA9) |
| **USRP B200mini / B205mini-i** | 70 MHz–6 GHz | 56 MHz | 12 | Full-duplex | 1 | USB 3.0 | $1,503 / $1,875 ([Ettus](https://www.ettus.com/all-products/usrp-b200mini/)) | Spartan-6; 10 MHz/PPS in | RT mid, compact |
| **USRP B210** | 70 MHz–6 GHz | 56 MHz (61.44 Msps; ~30.72 in 2-ch) | 12 | Full-duplex | 2 | USB 3.0 | $2,387 (+$1,502 GPSDO) ([Ettus](https://www.ettus.com/all-products/ub210-kit/)) | Spartan-6 150; mature UHD/GNU Radio | RT mid, DF-capable (2ch) |
| **USRP N210** | Daughterboard-dependent | ~25 MHz over GbE | 14 | Yes | 1 | 1 GbE | **EOL** ([Ettus KB](https://kb.ettus.com/index.php?action=pdfbook&format=single&title=N200%2FN210)) | Legacy, used market only | — |
| **USRP X310** | Daughterboard-dependent (to 6 GHz) | 160 MHz/ch (200 Msps) | 14 | Yes | 2 | 10 GbE / PCIe | ~$10,500 base, excl. daughterboards ([Ettus](https://www.ettus.com/all-products/x310-kit/)) | Kintex-7 410T; RFNoC | RT wide, lab |
| **USRP X410** | 1 MHz–7.2 GHz | 400 MHz/ch (500 Msps) | 12 | Yes | 4 | 2×QSFP28 100 GbE, PCIe Gen3 x8 | $33,020 ([Ettus](https://www.ettus.com/all-products/usrp-x410/)) | ZU28DR RFSoC | RT very wide, DF |
| **USRP X440** | Direct sampling to ~4 GHz | ~1.6 GHz/ch (2 GSps) | 14 **(verify)** | Yes | 8 | QSFP28 / PCIe | Quote **(verify)** ([Ettus KB](https://kb.ettus.com/X440)) | No analog tuning front end | RT ultra-wide |
| **USRP X420** | 10 MHz–20 GHz | 1 GHz (1.25 GS/s) | 12 | Yes | 2 | 2×QSFP28, PCIe Gen3 x8 | $52,920 ([Ettus](https://www.ettus.com/all-products/usrp-x420/)) | Built-in GPSDO | RT ultra-wide |
| **ADALM-PLUTO** | 325 MHz–3.8 GHz (70 MHz–6 GHz via AD9364 hack) | 20 MHz (56 MHz hacked) | 12 | Full-duplex | 1 (2×2 on Rev C/D via hack) | USB 2.0 | Launched $149; current ~$150–250 **(verify)** ([rtl-sdr.com](https://www.rtl-sdr.com/adalm-pluto-new-149-tx-capable-sdr-325-3800-mhz-range-12-bit-adc-20-mhz-bandwidth/)) | Zynq-7010; 25 ppm XO; USB 2 caps throughput ~ <10 Msps sustained **(verify)** | Learning |
| **Pluto+ (clone)** | 70 MHz–6 GHz (AD9363, AD9361/4 compatible) | 20–56 MHz | 12 | Full-duplex | 2 | 1 GbE + USB 2.0 | ~$250–350 **(verify)** | Zynq-7010, 0.5 ppm VCTCXO, SD boot | Cheap 2×2 networked |
| **AntSDR E200** | 70 MHz–6 GHz (AD9361) / 325 MHz–3.8 GHz (AD9363) | 56 / 20 MHz | 12 | Full-duplex | 2 | 1 GbE (offload), UHD-compatible | $499 / $299 ([rtl-sdr.com](https://www.rtl-sdr.com/antsdr-e200-set-to-begin-crowdfunding-on-crowdsupply-soon/)) | Zynq-7020 (85k LC, 220 DSP) | Networked RT mid |
| **AntSDR E310** | Same as E200 | 56 / 20 MHz | 12 | Full-duplex | 2 | 1 GbE, USB 2.0 OTG | ~$400–600 **(verify)** | Zynq-7020, 1 GB DDR3 | Networked RT mid |
| **LiteX-M2SDR** *(2024)* | 70 MHz–6 GHz (AD9361) | 56 MHz (61.44; 122.88 oversampled) | 12 | Full-duplex | 2 | M.2 2280, PCIe Gen2 x4 (~14 Gbps) | **(verify)** ([GitHub](https://github.com/enjoy-digital/litex_m2sdr)) | Artix-7 200T, fully open LiteX gateware; Pi 5 host guide exists | EMB, open FPGA |
| **Wavelet uSDR** | 300–3700 MHz (LMS6002D) | 28 MHz (30.72 Msps) | 12 | Yes | 1 | M.2 / USB adapter | $299 ([rtl-sdr.com](https://www.rtl-sdr.com/tag/usdr/)) | WebUSB | EMB low-cost |
| **Wavelet xSDR** *(crowdfunded, est. July 2026)* | 30 MHz–3.8 GHz (LMS7002M) | up to 90 MHz (122.88 Msps) | 12 | Yes | 2 | M.2 2230 A+E | $549 ([rtl-sdr.com](https://www.rtl-sdr.com/xsdr-crowdfunding-campaign-now-live/)) | Shipping status **unverified** | EMB wide |
| **Wavelet sSDR** *(announced)* | 30 MHz–11 GHz | 120 MHz | — | Yes | 2 | M.2 | ~$1,000 ([rtl-sdr.com](https://www.rtl-sdr.com/tag/ssdr/)) | Campaign timing **unverified** | EMB wide, >6 GHz |
| **RFNM** | Lime daughterboard 10–3800 MHz | 122 MHz over 1 USB cable (153 Msps ADC lanes; 612 MHz total RX) | 12 | Yes | up to 8 ADC lanes | USB 3.0 ×2, GbE | **(verify)** ([RFNM](https://rfnm.com/)) | NXP i.MX8M Plus + LA9310 DSP on board | Standalone SDR computer |
| **Epiq Sidekiq NV100** | 30 MHz–6 GHz (ADRV9004) | 40 MHz | 16 | Yes | 2 coherent | M.2 2280 B+M (PCIe) | Quote | On-board preselect filters + GPSDO | EMB, high DR |
| **Epiq Sidekiq X4** | 75 MHz–6 GHz (2×ADRV9009) | 200 MHz/Rx; up to 800 MHz aggregate | 16 | Yes | 4 coherent | FMC (VITA 57.1) | Quote ([PR](https://www.prnewswire.com/news-releases/epiq-solutions-announces-the-sidekiq-x4-rf-transceiver-for-high-bandwidth-multi-channel-applications-300663810.html)) | Needs FPGA carrier | RT wide DF |
| **Epiq Matchstiq X40** | 1 MHz–18 GHz | 450 MHz | — | 2 | 4 | Integrated (Orin NX 16G + ZU7) | Quote | 40–80 W ([LinuxGizmos](https://linuxgizmos.com/epiq-solutions-matchstiq-x40-and-g-series-for-edge-level-ai-ml-rf-spectrum-analysis/)) | Reference "all-in-one" |
| **Epiq Matchstiq G20/G40** | to 6 GHz | 50 MHz | — | 4 | 4 | Integrated (Orin NX 16G + Artix-7) | Quote | 20–50 W | Reference mid-tier |
| **Deepwave AIR-T Embedded (AIR7310/7311)** | 300 MHz–6 GHz | ~100 MHz **(verify)** | 16 **(verify)** | Yes | 2×2 / 4×4 | Integrated (Orin NX 16GB), GPS, PoE++ | Quote ([Deepwave](https://deepwave.ai/hardware-products/air-t-embedded-series/)) | AirStack SDK | GPU-native SDR |
| **KrakenSDR** | 24–1766 MHz | ~2.4 MHz per ch | 8 | No | 5 coherent | USB 2.0 (single) | Launched $399 (2022); ~$780 at a reseller ([Hacker Warehouse](https://hackerwarehouse.com/product/krakensdr/)) **(verify direct price)** | Pi 4/Pi 5 runs DAQ+DSP; replaces KerberosSDR (discontinued) | DF, passive radar |
| **Signal Hound BB60D** | 9 kHz–6 GHz | 27 MHz (40 Msps IQ) | — | No | 1 | USB 3.0 (140 MB/s) | $4,950–5,450 ([Signal Hound](https://signalhound.com/products/bb60d-6-ghz-real-time-spectrum-analyzer/)) | Preselector; 24 GHz/s sweep; Win/Linux x86 SDK | **S (best-in-class per $)** |
| **Signal Hound SM200C** | 100 kHz–20 GHz | 160 MHz IQ (10 GbE) | — | No | 1 | 10 GbE SFP+ / USB | $21,495–23,845 ([Signal Hound](https://signalhound.com/products/sm200c-20-ghz-real-time-spectrum-analyzer-with-10gbe/)) | 1 THz/s sweep, 110 dB DR | S + RT |
| **ThinkRF R5550** | 9 kHz–27 GHz | 100 MHz (160 MHz WBIQ) | — | No | 1 | Ethernet | ~$14,214 (-427) ([Saelig](https://www.saelig.com/product/r5550-427.htm)) | 28 GHz/s, 100 dBc SFDR ([ThinkRF](https://thinkrf.com/real-time-spectrum-analyzers/r5550-real-time-spectrum-analyzer/)) | S + RT, >6 GHz |
| **Harogic SAN-60** | 9 kHz–6.3 GHz | 50 or 100 MHz | — | No | 1 | USB | ~€1,978 ex VAT ([eleshop](https://eleshop.eu/harogic-san-60-usb-spectrum-analyser.html)) | −168 dBm/Hz DANL, >95 dBc image rej. | Low-cost instrument S+RT |
| **Tektronix RSA306B** | 9 kHz–6.2 GHz | 40 MHz | — | No | 1 | USB 3.0 | ~$3,853–7,570 retail ([RS](https://us.rs-online.com/product/tektronix/rsa306b/70816171/)) | SignalVu-PC needs Core i7-class host | RT DPX (100 µs POI) |
| **Per Vices Crimson TNG** | ~DC–6 GHz | 325 MHz/ch | 16 | Yes | 4 | 2×10 GbE (or QSFP) | Quote ([Per Vices](https://www.pervices.com/crimson-tng/)) | Rack/lab | RT wide |
| **Per Vices Cyan** | 100 kHz–20 GHz | 1 GHz (to 3 GHz) | 16 | Yes | up to 16 | 4×40/100 GbE | Quote ([Per Vices](https://support.pervices.com/cyan/specs/)) | Rack/lab | RT ultra-wide |
| **RFSoC 4x2 (Real Digital/AMD)** | ADC input BW ~6 GHz (direct) | up to ~2.5 GHz per ADC (5 GSPS) | 14 | 2 DAC | 4 | QSFP28 100 GbE, 1 GbE | $2,499 **academic only** ([Real Digital](https://www.realdigital.org/hardware/rfsoc-4x2)) | ZU48DR, PYNQ | RT ultra-wide, FPGA dev |
| **AMD ZCU208 / ZCU111** | Direct sampling (ZU48DR / ZU28DR) | 8×14-bit 5 GSPS / 8×12-bit 4.096 GSPS | 14 / 12 | 8 DAC | 8 | QSFP / SFP+ | Five figures **(verify)** ([AMD](https://www.amd.com/en/products/adaptive-socs-and-fpgas/evaluation-boards/zcu111.html)) | Needs RF front-end daughter/baluns; Avnet kits were $9,495–19,995 in 2020 | FPGA R&D |
| **PZSDR** *(crowdfunded)* | 1 MHz–6 GHz direct | ~2.5 GHz/ch (5 GSPS) | 14 | 8 | 8 coherent | 2×100G QSFP28, GbE, USB 3 | $8,749 ([rtl-sdr.com](https://www.rtl-sdr.com/pzsdr-new-amd-zync-ultrascale-based-sdr-crowd-funding-on-crowd-supply/)) | ZU47DR, enclosure + GPS included | RFSoC in a box |
| **ALINX AXW22** | ZU47DR direct | 5 GSPS | 14 | Yes | 8 | — | ~$8,686 ([ALINX](https://www.en.alinx.com/Product/SoC-Development-Boards/Zynq-UltraScale-plus-RFSoC/AXW22.html)) | Dev board | FPGA R&D |
| **Red Pitaya SDRlab 122-16** | 300 kHz–500 MHz analog input (direct, 61 MHz Nyquist) | ~61 MHz | 16 | 2 (14-bit) | 2 | 1 GbE | **(verify)** ([Red Pitaya docs](https://redpitaya.readthedocs.io/en/latest/developerGuide/hardware/ORIG_GEN/122-16/top.html)) | Zynq-7020, open FPGA ecosystem | HF/VHF direct sampling, FPGA learning |
| **Malahit DSP2** (handheld) | 10 kHz–380 MHz, 404 MHz–2 GHz | 192 kHz panorama | — | No | 1 | Standalone (touchscreen) | ~$250–350 **(verify)** | 5000 mAh battery | Portable listening |

**RF transceiver ICs behind many of these boards:**

| RFIC | Tuning | RX BW | ADC | Notes |
|---|---|---|---|---|
| AD9363 | 325 MHz–3.8 GHz | ≤20 MHz | 12-bit | Pluto ([ADI](https://www.analog.com/en/products/ad9363.html)) |
| AD9364 | 70 MHz–6 GHz | ≤56 MHz | 12-bit | 1×1 (B200mini) |
| AD9361 | 70 MHz–6 GHz | ≤56 MHz | 12-bit | 2×2 (bladeRF, B210, AntSDR, LiteX-M2SDR) ([ADI](https://www.analog.com/en/products/ad9361.html)) |
| ADRV9002/9004 | 30 MHz–6 GHz | kHz–40 MHz | 16-bit (9004 per Epiq) | Low-power, portable-oriented; built-in DC/QEC tracking ([ADI](https://www.analog.com/en/products/adrv9002.html)) |
| ADRV9009 | 75 MHz–6 GHz | 200 MHz (245.76 Msps IQ) | — | 450 MHz TX synthesis/ORx ([ADI](https://www.analog.com/en/products/adrv9009.html)) |
| LMS7002M | 100 kHz–3.8 GHz | up to 120 MHz | 12-bit | 2×2 ([Lime](https://limemicro.com/technology/lms7002m/)) |
| RFSoC Gen3 (ZU4xDR) | Direct sampling, ~6 GHz input BW | GHz | 14-bit @ 5 GSPS | Integrated DDC/NCO; multi-tile sync |

### 2.2 Which hardware suits which exploration mode

| Mode | What matters | Good fits | Poor fits |
|---|---|---|---|
| **Sweep-based wideband survey** (0–6 GHz occupancy map, "what's here?") | Fast retune in firmware (no host round-trip), preselection, DC-spike handling, calibrated amplitude | BB60D (24 GHz/s, preselected), SM200C, ThinkRF R5550, **HackRF One/Pro** (`hackrf_sweep` 8 GHz/s; Opera Cake switching), Harogic SAN, tinySA (slow but portable) | RTL-SDR (slow retune, 2.4 MHz steps), Pluto over USB 2 |
| **Real-time wide IBW** (catch short bursts, FHSS, radar, drones) | IBW ≥ 40–100 MHz, bits ≥ 12, gap-free transport (USB 3/PCIe/10 GbE), FPGA offload | LimeSDR XTRX / xSDR (90–120 MHz), bladeRF/B210/AntSDR/LiteX-M2SDR (56 MHz), X410 (400 MHz), Sidekiq X4, RFSoC (GHz), RSA306B (40 MHz DPX) | HackRF (20 MHz, 8-bit), all USB 2.0 receivers |
| **HF/VLF exploration** | Direct sampling, 14–16 bits, excellent blocking DR, attenuator, loop antenna | RX888 MkII (64 MHz, 16-bit), RSPdx-R2 (HDR mode), Airspy HF+ Discovery, KiwiSDR 2, Red Pitaya | HackRF, AD9361-class (no coverage below 47–70 MHz) |
| **Coherent DF** | Shared LO/clock, calibration source, ≥3 channels for unambiguous AoA | KrakenSDR (5 ch, auto-cal), RSPduo (2 ch), B210/bladeRF (2 ch + cal), X410/Sidekiq X4 (4 ch), RFSoC (8 ch), PZSDR | Separate dongles with independent clocks |
| **Embedded / enclosure-friendly** | M.2/mPCIe, low power, thermal path | LiteX-M2SDR, xSDR/uSDR, LimeSDR XTRX, Epiq NV100, B200mini, AntSDR (Ethernet) | X310/X410/Per Vices (rack power) |

### 2.3 Receiver-capability note: 2.4 GHz and 902–928 MHz ISM on the HackRF — detection vs. decode

*This is a capability statement derived from published protocol parameters and the front-end limits above (§1.2, §1.6, §2.1, §3.1), not a device measurement. Marked **unverified** throughout; see what would confirm it at the end.*

- **2.4 GHz decode is mostly out of reach on a HackRF One** — not impossible everywhere in the band, but the two big occupants don't fit the front end:
  - **Wi-Fi (802.11) 20 MHz OFDM channels sit right at the HackRF's 20 Msps ceiling.** IQ sampling gives *Fs* Hz of usable span centered on the LO, and "usable" is already somewhat less than *Fs* after anti-alias roll-off (§1.2's IQ-sampling note); a 20 MHz channel has no guard band to spare against that. Combined with 8-bit quantization and no preselector ahead of the ADC (§1.6, §1.7; §2.2's "Real-time wide IBW" row already lists "HackRF (20 MHz, 8-bit)" as a poor fit), capture of a full Wi-Fi channel is marginal at best — edge subcarriers are the first to go, and a busy 2.4 GHz environment with multiple overlapping APs raises the intermodulation risk the front end already struggles with in cities (§0, §1.7). 40/80/160 MHz Wi-Fi (802.11n/ac/ax) is further out of reach still, and 6 GHz Wi-Fi 6E sits above this device's tuning range.
  - **Classic (BR/EDR) Bluetooth hops too fast to follow blind.** Adaptive frequency hopping cycles at up to 1600 hops/s across 79 × 1 MHz channels spanning the full 2402–2480 MHz range ([Argenox, "Introduction to Bluetooth Classic"](https://argenox.com/library/bluetooth-classic/intro-bluetooth-classic)) — about 4× the HackRF's usable IBW. Following a live link needs either ~80 MHz of real-time capture (a different receiver class, `needs-other-sdr`) or the hop sequence derived from a paired link's clock/address, not blind RF. `docs/capabilities/C10-burst-tracking.md` already records this constraint.
  - So at 2.4 GHz, and for the same front-end reasons at 902–928 MHz ISM (a 26 MHz band that itself slightly exceeds one 20 MHz window — `docs/capabilities/C12-occupancy-baseline.md`, `AWARE-035`), the honest target is **detection and occupancy**: is the channel busy, how often, what shape of burst — not full protocol decode. The individual narrowband ISM devices in 902–928 MHz (rtl_433-style OOK/FSK bursts, tens of kHz wide) are the exception *within* that band, and already decode natively (`SIGNAL-049`, `SIGNAL-050`) because they don't carry Wi-Fi's OFDM bandwidth or classic Bluetooth's hop span.
- **The exception at 2.4 GHz is BLE advertising.** Bluetooth Low Energy's three advertising channels are *fixed*, not hopped: 2402 MHz (ch. 37), 2426 MHz (ch. 38), 2480 MHz (ch. 39), each 1 Msps GFSK ([Argenox, "BLE Advertising Primer"](https://argenox.com/library/bluetooth-low-energy/ble-advertising-primer)), carrying short bursts (roughly hundreds of µs per PDU, repeating on intervals from ~20 ms to several seconds — `docs/capabilities/C10-burst-tracking.md`). A single fixed channel fits comfortably inside a 20 MHz HackRF capture with room to spare, so BLE advertising is both a feasible decode target and, being a genuinely short, non-continuous emission with no stable carrier, a good real-world **burst example for the time-extent model** (ADR-0017 — a signal is a time–frequency region with a time extent, not a persistent carrier). Full BLE *connection* follow (the 37 data channels, which do hop within an established link) is not this exception; that reduces to the same hop-following problem as classic Bluetooth.
- **What would change this.** A narrower channel (e.g. BLE's 1 MHz vs. Wi-Fi's 20 MHz), a different front end (≥12-bit ADC, ≥40–80 MHz real-time IBW, a preselector — see §2.2's "Real-time wide IBW" row and Tier B/C in §7.1), or, for classic Bluetooth, a known hop sequence from a paired sniffer, would each move an item out of "detection only." **What would confirm or contradict this note:** an actual HackRF capture of a live 20 MHz Wi-Fi beacon frame run through OFDM preamble/CRC recovery (does it decode, and how often); and a capture of BLE advertising bursts on 2402/2426/2480 MHz demodulated and CRC-checked against a known device's real advertisements. Neither has been run yet.

---

## 3. Embedded compute platforms for SDR DSP

### 3.1 Throughput math

Bytes per second = Fs × 2 (I and Q) × bytes per sample. Most 12-bit SDRs ship 16-bit containers unless packed.

| Stream | MB/s | Mbit/s | GB per hour | Fits over |
|---|---|---|---|---|
| RTL-SDR: 2.4 Msps × 8-bit | 4.8 | 38 | 17 | USB 2.0 easily |
| Airspy/RSP: 10 Msps × 16-bit | 40 | 320 | 144 | USB 2.0 (near limit) |
| HackRF: 20 Msps × 8-bit | 40 | 320 | 144 | USB 2.0 (at practical limit, ~35–40 MB/s) |
| HackRF Pro: 40 Msps × 4-bit | 40 | 320 | 144 | USB 2.0 |
| AD9361 1-ch: 61.44 Msps × 16-bit | 246 | 1,966 | 885 | USB 3.0, PCIe Gen2 x1 |
| AD9361 2-ch: 61.44 Msps × 16-bit | 492 | 3,932 | 1,770 | USB 3.0 (marginal), PCIe Gen2 x1 (~4 Gbps, matches LiteX note) |
| bladeRF oversample: 122.88 Msps × 8-bit | 246 | 1,966 | 885 | USB 3.0 |
| LMS7002M: 122.88 Msps × 16-bit, 1-ch | 492 | 3,932 | 1,770 | PCIe Gen2 x2+ |
| ADRV9009: 245.76 Msps × 16-bit, 1-ch | 983 | 7,864 | 3,540 | PCIe Gen3 x2+, 10 GbE |
| X410: 500 Msps × 16-bit, 1-ch | 2,000 | 16,000 | 7,200 | PCIe Gen3 x4, 100 GbE |
| RFSoC ADC raw: 5 GSPS × 16-bit container (real, not IQ) | 10,000 | 80,000 | 36,000 | **Only after on-FPGA DDC/channelizer** |

**Approximate usable link payloads** (after encoding and protocol overhead; typical, not guaranteed):

| Link | Usable payload |
|---|---|
| USB 2.0 HS | ~35–40 MB/s |
| USB 3.0 (5 Gbps) | ~350–400 MB/s |
| 1 GbE | ~117 MB/s (~29 Msps of 16-bit IQ) |
| PCIe Gen2 x1 | ~500 MB/s raw |
| PCIe Gen3 x1 | ~985 MB/s |
| PCIe Gen3 x4 | ~3.9 GB/s |
| 10 GbE | ~1.17 GB/s (~290 Msps of 16-bit IQ) |
| 100 GbE | ~11.7 GB/s |

**Recording takeaway.** Continuous raw IQ at 56 MHz fills a 1 TB NVMe in about 70 minutes. An exploration device should record *spectrogram metadata continuously* and *IQ only around detections*, using a pre-trigger ring buffer in RAM or FPGA memory. A 4096-bin float32 spectrogram at 30 lines/s is only ~0.5 MB/s.

### 3.2 DSP compute: order-of-magnitude feasibility

These are engineering estimates for planning. Benchmark on target hardware before committing.

- **FFT for the waterfall.**
  - A 4096-point single-precision FFT costs on the order of 10⁵–10⁶ floating-point operations.
  - A Cortex-A76 core with NEON delivers on the order of 10¹⁰ FLOPS (assumption), i.e. roughly tens of thousands of 4096-FFTs per second per core.
  - At 56 Msps, non-overlapping 4096-sample frames give ~13.7k FFTs/s, so continuous full-rate spectral estimation is **feasible on 1–2 Pi 5 cores**.
  - At 100+ Msps with 50% overlap, CPU cost stays manageable; *I/O and memcpy/format conversion dominate*.
- **Energy/burst detection.**
  - Cheap per bin: noise-floor tracking (median/percentile over time), CFAR thresholds, connected components on the spectrogram.
  - Feasible on CPU up to ~56 MHz. Above that, move detection into FPGA or GPU.
- **Polyphase channelizer (PFB)** with 256–4096 channels: roughly the FFT cost plus an M-tap polyphase filter per frame.
  - CPU is OK at 20 MHz and borderline on a Pi 5 at 56 MHz.
  - A GPU (Orin) is comfortable at 56–100 MHz.
  - At GHz rates an FPGA is mandatory. The RFSoC OPFB example handles 4 GHz with 4096 branches using 24% LUTs / 9% DSP48 on a ZCU111 ([MazinLab/IEEE](https://github.com/MazinLab/RFSoC_OPFB)).
- **Modulation/protocol classification.**
  - Cost scales with *detections per second*, not IBW. Classifiers operate on snippets (e.g., 1024–4096 IQ samples) of each detected burst.
  - A small CNN at INT8/FP16 on an Orin Nano Super (67 TOPS) should handle hundreds to thousands of inferences per second (estimate). A Pi 5 CPU handles tens. An RK3588 NPU (6 TOPS) sits in between, with toolchain friction.

### 3.3 Platform comparison

| Platform | CPU / accel | High-speed I/O | Power | Price | SDR notes |
|---|---|---|---|---|---|
| **Raspberry Pi 5** | 4× Cortex-A76 @ 2.4 GHz, VideoCore VII | 2× USB 3.0 (5 Gbps, simultaneous), **PCIe 2.0 x1** (Gen3 forceable via `dtparam=pciex1_gen=3`, not certified) | 5 V/5 A PSU recommended; ~4–12 W typical board load **(estimate)** | 1–16 GB; 16 GB $305 ([RPi](https://www.raspberrypi.com/products/raspberry-pi-5/), [RPi docs](https://www.raspberrypi.com/documentation/accessories/m2-hat-plus.html)) | **Known issue:** bladeRF 2.0 over USB 3 reached only ~15 Msps discontinuity-free on Pi 5 vs ~40 Msps on Pi 4, unresolved as of Nov 2024. An RPi engineer attributed it to host URB submission patterns ([forum](https://forums.raspberrypi.com/viewtopic.php?t=374733)). PCIe SDRs work: LimeSDR XTRX via mPCIe HAT (needs active cooling, >85 °C → 50–55 °C with a 40 mm fan) ([MyriadRF](https://discourse.myriadrf.org/t/limesdr-xtrx-operation-on-raspberry-pi-5-via-mpcie-hat/8850)); LiteX-M2SDR via M.2 HAT (needs `pcie_aspm=off`, 32-bit DMA; only a 5 Msps smoke test is documented) ([LiteX doc](https://github.com/enjoy-digital/litex_m2sdr/blob/main/doc/hosts/raspberry-pi-5.md)). KrakenSDR officially supports Pi 5. GUI SDR apps (GQRX, SDR++) can lag because of the GL stack ([tech-reader](https://www.tech-reader.blog/2025/06/insight-why-dragonos-runs-best-on-x86.html)). |
| **Jetson Orin Nano Super (8 GB)** | 6× Cortex-A78AE, Ampere GPU 1024 CUDA cores, 67 TOPS | PCIe Gen3 (4 controllers, 7 lanes on module; dev kit exposes M.2 slots), USB 3.2 | 7 / 15 / 25 W / MAXN Super modes | $249 dev kit ([NVIDIA blog](https://developer.nvidia.com/blog/nvidia-jetson-orin-nano-developer-kit-gets-a-super-boost/); [datasheet](https://mm.digikey.com/Volume0/opasdata/d220001/medias/docus/5380/Jetson_Orin_Nano_Series_DS-11105-001_v1.1.pdf)) | Unified CPU/GPU memory avoids PCIe copies. **CuPy** now hosts the former cuSignal (RAPIDS 23.08 was cuSignal's last release) ([cuSignal](https://github.com/rapidsai/cusignal)). The **Holoscan SDK** has an RTL-SDR + SoapySDR FM demo in HoloHub ([NVIDIA blog](https://developer.nvidia.com/blog/developing-streaming-sensor-applications-with-holohub-from-nvidia-holoscan/)). **Holoscan Sensor Bridge** (FPGA → Ethernet → GPU, PTP timing) targets IGX/AGX Orin ([NVIDIA](https://developer.nvidia.com/blog/nvidia-holoscan-sensor-bridge-empowers-developers-with-real-time-data-processing/)). **Aerial** is a 5G-RAN stack, not directly useful here. Orin NX 16GB is the module Epiq and Deepwave chose. |
| **Jetson Orin NX 16 GB** | 8× A78AE, Ampere 1024 cores, up to 100 TOPS (NVIDIA claim) | PCIe Gen4 **(verify)** | 10–25 W (+ MAXN Super) | ~$600–900 module **(verify)** | Used in Matchstiq X40/G20/G40 and AIR-T Embedded (AIR7310/7311) |
| **Rockchip RK3588 (Rock 5B, Orange Pi 5 Plus/Max/Ultra)** | 4× A76 + 4× A55, Mali-G610, **6 TOPS NPU** (RKNN-Toolkit2) | PCIe 3.0 x4 on full RK3588 boards (RK3588S has fewer lanes) **(verify per board)**, USB 3 | ~5–15 W **(estimate)** | ~$100–250 **(verify)** ([Orange Pi](http://www.orangepi.org/html/hardWare/computerAndMicrocontrollers/details/Orange-Pi-5.html)) | More CPU and PCIe than Pi 5. No public SDR throughput benchmarks were found, and the NPU toolchain (RKNN) is less mature than CUDA/TensorRT. |
| **Zynq-7000 SoC** | 2× A9 + Artix-class FPGA | In-fabric to RFIC | 2–5 W | Pluto, AntSDR, Red Pitaya | The ARM is too weak for wideband DSP. The FPGA must do DDC/FFT/detection, sending only results or decimated IQ over GbE. |
| **Zynq UltraScale+ MPSoC / Kria** | 4× A53 + FPGA (+ Mali; EV has VCU) | PCIe (board-dependent), GbE | 5–15 W | KV260 $249, KR260 $349 ([AMD](https://www.amd.com/en/blogs/2025/kria-kv260-vision-ai-starter-kit-refresh.html)) | Viable FPGA co-processor/controller for an AD9361/ADRV9002 front end; no RF ADCs |
| **RFSoC (ZU2x/4xDR)** | 4× A53 + 2× R5 + large FPGA + RF-ADC/DAC | QSFP28 100 GbE, PCIe | ~20–60 W board **(estimate)** | $2.5k (academic 4x2) → $8.7k (PZSDR/AXW22) → five figures | PYNQ/RFSoC-PYNQ ([Xilinx](https://github.com/Xilinx/RFSoC-PYNQ/blob/master/docs/rfsoc_4x2_overview.md)); on-FPGA channelizers/spectrum analyzers ([StrathSDR](https://github.com/strath-sdr/rfsoc_xilinx_adapt)) |
| **x86 mini PC (Intel Core Ultra / Ryzen 7x40–8x40 / N100)** | AVX2 (AVX-512 on some Ryzen), iGPU | Thunderbolt/USB4 (PCIe tunneling), USB 3.2, NVMe Gen4 | 6–65 W | $150–900 | Most mature SDR ecosystem: VOLK/FFTW/liquid-dsp, UHD, all vendor SDKs (Signal Hound, SDRplay x86). Tek's RSA306B real-time DPX reference host is a Core i7 ([Tek](https://www.tek.com/en/products/spectrum-analyzers/rsa306)). LiteX-M2SDR's "RF Adventurer" prototype uses Thunderbolt/USB4 PCIe tunneling for portable 2T2R at 122.88 Msps ([Enjoy Digital](https://x.com/enjoy_digital/status/1892984466509046109)). |
| **Apple Silicon (M-series)** | Strong NEON cores, unified memory, Metal GPU | Thunderbolt/USB4 | 10–40 W | $600+ | SDR++ builds natively ([GitHub](https://github.com/AlexandreRouma/SDRPlusPlus/discussions/994)). No CUDA; UHD and SoapySDR work. OpenGL deprecation is a risk for GL-based UIs. Good for desktop analysis, not for a custom enclosure. |

### 3.4 What's realistic at 20 / 56 / 100+ MHz

| Task | 20 MHz | 56 MHz | 100–400 MHz | GHz |
|---|---|---|---|---|
| Waterfall + persistence display | Pi 5 ✔ | Pi 5 ✔ (I/O permitting) | Orin/x86 with PCIe ✔; USB 3 marginal | FPGA FFT + decimated display only |
| Continuous burst detection (full band) | Pi 5 ✔ | Pi 5 borderline; Orin ✔ | Orin GPU or FPGA | FPGA mandatory |
| Channelizer (1k+ channels) | Pi 5 ✔ | Orin GPU ✔ / FPGA | FPGA (or dGPU) | RFSoC FPGA |
| Per-detection classification (CNN) | Pi 5 (slow) / Orin ✔ | Orin ✔ | Orin ✔ if detections are gated | Orin NX/AGX fed by FPGA detections |
| Continuous raw IQ recording | microSD/USB SSD ✔ | NVMe (Pi 5 Gen2 x1 marginal for 2-ch) | NVMe Gen3 x4 / RAM ring buffer | Triggered snippets only |

---

## 4. FPGA roles in an exploration device

1. **DDC/decimation.** NCO mixing + CIC/FIR to extract narrowband channels at the source, reducing link load by 10–1000×. RFNoC's DDC block is multi-channel, with channel count and decimation set at FPGA build time ([UHD docs](https://files.ettus.com/manual/classuhd_1_1rfnoc_1_1ddc__block__control.html)). RFSoC includes hardened DDC per ADC.
2. **FFT/spectrum acceleration.** The RFNoC FFT block supports multiple FFT pipelines within one block for resource efficiency ([UHD docs](https://files.ettus.com/manual/classuhd_1_1rfnoc_1_1fft__block__control.html)). An FPGA averaging/max-hold spectrum at 10–100k FFTs/s gives gap-free "DPX-like" persistence displays; FieldFox RTSA claims ~120,000 FFTs/s ([Keysight](https://www.keysight.com/us/en/products/spectrum-analyzers-signal-analyzers/fieldfox-handheld-spectrum-analyzers.html)).
3. **Polyphase filter-bank channelizers.** Critically sampled or 2× oversampled PFBs split the IBW into uniform channels with far better stopband than a plain FFT. That is essential for detecting weak narrowband signals next to strong ones. Examples:
   - MazinLab OPFB on RFSoC: 4 GHz / 4096 channels, HLS, PYNQ-controlled, images suppressed below −60 dB ([IEEE](https://ieeexplore.ieee.org/document/9336352/))
   - community RFNoC polyphase channelizer ([GitHub](https://github.com/e33b1711/rfnoc_pp_channelizer))
4. **Triggering/burst detection.** Per-bin energy/CFAR detectors with pre-trigger buffers in BRAM/DDR, so only snippets are sent upstream. HackRF Pro's move from CPLD to FPGA and its new trigger connectors open this door on the budget tier ([GSG](https://greatscottgadgets.com/hackrf/pro/)).
5. **Timestamping and synchronization.** Sample-accurate timestamps (PPS-disciplined counters), VITA-49 packetization (CRFS RFeye streams VITA-49 ([CRFS](https://www.crfs.com/hardware/rf-sensors))), and multi-channel phase alignment for DF/TDoA.
6. **Sweep control.** Firmware-driven retune + settle + capture loops. `hackrf_sweep` retunes without host round-trips, which is why it reaches 8 GHz/s ([rtl-sdr.com](https://www.rtl-sdr.com/scanning-spectrum-8ghz-per-second-new-hackrf-update/)). Opera Cake port switching and preselector-bank control synchronize with the same loop.

**Toolchains and ecosystems:**

| Ecosystem | What it offers |
|---|---|
| **Ettus RFNoC (UHD 4.x)** | Block-based FPGA framework for USRP E/N/X series (B-series with limited fabric); GNU Radio integration ([Ettus KB](https://kb.ettus.com/Getting_Started_with_RFNoC_in_UHD_4.0)) |
| **Analog Devices HDL + Kuiper Linux** | Reference designs for AD9361/ADRV900x on Zynq/ZynqMP/Intel, including Pluto ([ADI HDL](https://analogdevicesinc.github.io/hdl/projects/pluto/index.html)); IIO drivers, libiio |
| **LiteX / LitePCIe** | Python-based SoC builder; fully open gateware for LiteX-M2SDR ([GitHub](https://github.com/enjoy-digital/litex_m2sdr)). Builds on proprietary Vivado or open **openXC7** (yosys + nextpnr-xilinx for Artix-7/Kintex-7/Zynq-7) ([openXC7](https://github.com/openXC7/toolchain-installer)) |
| **Lattice ECP5** | LimeSDR Mini 2.0 uses ECP5, supported by the open yosys/nextpnr flow |
| **Red Pitaya** | Zynq-7020 + 16-bit 122.88 Msps ADCs; open FPGA projects; good for HF direct-sampling FPGA prototyping ([Red Pitaya](https://redpitaya.com/sdrlab-122-16/)) |
| **RFSoC-PYNQ** | Python/Jupyter control of RFSoC overlays; free eBook on SDR with RFSoC ([rfsoc-pynq.io](http://www.rfsoc-pynq.io/)) |
| **bladeRF xA9** | 301 kLE Cyclone V aimed at "hardware accelerators and HDL signal processing chains" ([Nuand](https://www.nuand.com/product/bladerf-xa9/)) |

---

## 5. Antennas and RF front-end accessories

| Item | Examples / specs | Why it matters for exploration |
|---|---|---|
| **Wideband discone** (omni, vertical) | Diamond D130J: RX 25–1300 MHz, ~2 dBi, 5.6 ft ([Diamond](https://www.diamondantenna.net/d130j.html)) | Best fixed-site omni survey antenna for VHF/UHF. Too large for handheld use, but a good "base station" reference. Compact discones and telescopic whips trade gain for portability. |
| **Log-periodic (LPDA)** (directional) | Ettus/Kent LP0965: 850 MHz–6.5 GHz, 5–6 dBi ([Ettus](https://www.ettus.com/all-products/lp0965/)); WA5VJB PCB LPDAs from 400 MHz to 11 GHz ([WA5VJB](https://www.wa5vjb.com/products1.html)) | Gain plus front-to-back ratio. Enables "rotate to peak" DF with a single channel and rejects off-axis strong emitters. A PCB LPDA is light enough for a handheld. |
| **Active magnetic loop** (HF) | MLA-30+ (~$40, 0.5–30 MHz) vs Wellbrook ALA1530LN (~$305): the Wellbrook has a clearly lower noise floor ([rtl-sdr.com](https://www.rtl-sdr.com/reviews-of-the-low-cost-mla-30-wide-band-hf-magnetic-loop-antenna/)) | Responds mainly to the magnetic field. Urban E-field noise from electronics is rejected, and the nulls help locate noise sources ([Electronics Notes](https://www.electronics-notes.com/store-shop/ham-radio-reviews/mla-30plus-hf-loop-antenna-review.php)). |
| **Antenna switch** | Opera Cake: 2 primary ports × 8 secondary, 1 MHz–4 GHz, frequency- or time-based auto-switching during `hackrf_sweep` ([GSG](https://greatscottgadgets.com/hackrf/operacake/)) | Lets one sweep use the right antenna per band. The same switch can drive a filter bank. |
| **Band-stop / notch filters** | FM 88–108 MHz stop: >50 dB, <0.5 dB IL to 1 GHz; AM broadcast high-pass ([rtl-sdr.com](https://www.rtl-sdr.com/rtl-sdr-com-broadcast-fm-band-stop-filter-88-108-mhz-reject-now-for-sale/)) | Cheapest fix for urban overload |
| **Switched preselector / filter bank** | Built into RSPdx-R2 (12 bands), BB60D (130 MHz–6 GHz), Epiq NV100 | Suppresses image, harmonic and blocker energy; the key to trustworthy automatic detection |
| **LNA + bias-tee** | Bias-tees on RTL-SDR V4, RSPdx-R2 (4.7 V), KrakenSDR (4.5 V per port) | A mast-mounted LNA offsets cable loss. On a handheld with short coax it mostly adds IMD, so make it switchable. |
| **Step attenuator / limiter** | RX888 MkII 0–31.5 dB HF attenuator; KiwiSDR 2 digital attenuator | Keeps strong local emitters within ADC range. A limiter protects the LNA near transmitters. |
| **Reference clock** | GPSDO such as Leo Bodnar LBE-1420 (1 Hz–1.1 GHz output, ~1e-12 long-term) | Frequency accuracy, 1PPS timestamps, multi-unit coherence |

---

## 6. Handheld and portable precedents: what they teach about finding signals

| Product | Class | Key signal-finding features | Lessons for our design |
|---|---|---|---|
| **HackRF + PortaPack H4M, Mayhem firmware** | Open handheld SDR | *Looking Glass* wideband overview, *Recon* frequency scanner/signal hunter, *Scanner* (~20 freq/s, 50 ms dwell, 500 ms confirm-lock, color-coded states), *Level*, *Detector*, *Capture/Replay* to microSD ([Mayhem wiki](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Features); [Scanner](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Scanner)); the H4M Pro variant ships with HackRF Pro ([Lab401](https://lab401.com/products/portapack-h4m-pro)) | This is the baseline to beat. The MCU-class UI limits waterfall resolution, and there is no ML identification and no persistent occupancy database. The "confirm before stopping" logic is a good pattern. |
| **Malahit DSP2** | Standalone SDR receiver | 10 kHz–2 GHz, 192 kHz panorama + waterfall, 3.5" touch, 5000 mAh, 82 dB dynamic range ([Radioddity](https://www.radioddity.com/products/raddy-dsp2)) | A small, low-power, battery-friendly UX sells. But its narrow panorama makes it a listening device, not an explorer. |
| **tinySA Ultra+ (ZS407)** | Pocket spectrum analyzer | 100 kHz–7.3 GHz, RBW 200 Hz–850 kHz, 4" touch, 5000 mAh (~10 h), ~$220 ([R&L](https://www.randl.com/index.php?main_page=product_info&products_id=76237)) | Swept superhet with calibrated dBm readings at a very low price. Battery life is excellent. It shows people value amplitude-calibrated readings. |
| **RF Explorer 6G Combo+** | Pocket spectrum analyzer | 50 kHz–6.1 GHz (7.5 GHz with license), PC software; models from $119 ([RF Explorer](https://j3.rf-explorer.com/6gcomboplus)) | Same lesson; a PC link extends the UI |
| **Signal Hound BB60D / SM200C** | USB/10 GbE analyzers with IQ | 24 GHz/s and 1 THz/s sweeps, preselection, 27/160 MHz IQ streaming | The sweep-speed and preselection benchmarks to aim for; tethered, x86 host |
| **Tektronix RSA306B** | USB RTSA | 40 MHz real-time DPX: detects signals ≥100 µs; streaming capture; SignalVu-PC ([Tek](https://www.tek.com/en/products/spectrum-analyzers/rsa306)) | Persistence/density displays with a guaranteed 100% POI duration are the key "find the burst" feature |
| **Harogic SAN / PX series** | USB RTSA + handhelds | 50/100 MHz IBW, drone-detection marketing, 195 g core ([Harogic](https://www.harogic.com/latest-news/real-time-spectrum-analyzer-for-drone-detection-and-spectrum-monitoring-harogic-new-san-series/)) | The price of 100 MHz real-time instruments has fallen below €2k |
| **Keysight FieldFox N99xxD (2025)** | Pro handheld | RTSA to 120 MHz, ~120k FFT/s gap-free, trace record/playback, **120 MHz IQ streaming over SFP+ 10 GbE**, ±0.1 dB amplitude accuracy ([Keysight PR](https://www.keysight.com/us/en/about/newsroom/news-releases/2025/1204_pr26-004-keysight-introduces-new-handheld-analyzer-enabling-120-mhz-iq-streaming-for-gap-free-signal-capture.html)) | Even the top pro handheld now treats *streaming IQ off the device* as a first-class feature |
| **R&S Spectrum Rider FPH** | Pro handheld | Up to 44 GHz, DANL −163 dBm; K15 interference analysis with **spectrogram recording up to 999 h**; K16 **signal-strength mapping on floorplans**; IP54, 2.5 kg, 4.5 h battery ([R&S](https://www.rohde-schwarz.com/us/products/test-and-measurement/handheld/rs-spectrum-rider-fph-handheld-spectrum-analyzer_63493-147712.html)) | Long-duration spectrogram logging and location-tagged signal strength are core "hunting" workflows |
| **CRFS RFeye Node** | Networked sensors | 9 kHz–40 GHz sweeps, 100 MHz IBW (40 MHz entry), occupancy/detection scans, TDoA/AoA/PoA geolocation, I/Q record, VITA-49, IP67 ([CRFS](https://www.crfs.com/hardware/rf-sensors)) | Occupancy statistics + detection + geolocation as scheduled "tasks"; multi-unit synchronization via GPS |
| **Epiq Matchstiq X40 / Deepwave AIR-T** | SDR + GPU edge boxes | Orin NX + FPGA + RFIC for on-device AI/ML | Commercial proof of our intended architecture; 20–80 W power envelopes |

**Features to copy:**
1. Fast calibrated sweep with preselection.
2. Real-time persistence/DPX view with a stated minimum POI duration.
3. Long spectrogram recording with timestamps, GPS and heading.
4. Triggered IQ capture.
5. Automatic scanning with confirm/dwell logic.
6. Signal-strength mapping and DF.
7. IQ streaming off-device to a bigger analyzer.
8. Long battery life.

---

## 7. Takeaways: candidate architectures

### 7.1 Tier summary

| Tier | Core hardware | Approx BOM (RF+compute, excl. enclosure/battery) | Exploration capability | Main bottlenecks |
|---|---|---|---|---|
| **A. Budget** "PortaPack killer" | **HackRF Pro** (or HackRF One) + **Raspberry Pi 5 (8–16 GB)** + 5–7" touch display + switched filter bank / FM notch + optional RTL-SDR V4 or RSPdx-R2 as a second, higher-DR narrowband receiver | ~$700–1,100 | 0–6 GHz sweep at ~8 GHz/s; 20 MHz real-time zoom (8-bit); waterfall, CPU detection, light classification; IQ snippets to NVMe | 8-bit dynamic range (IMD/false detections in cities); USB 2.0 caps 20 Msps; Pi 5 USB quirks; no coherent DF; no GPU |
| **A′. Budget-PCIe** | **Pi 5 + M.2 SDR** (LiteX-M2SDR, xSDR, or LimeSDR XTRX via HAT) | ~$800–1,300 | 56–120 MHz real-time on PCIe; 2×2 (DF experiments) | Pi 5 PCIe Gen2 x1 (~4 Gbps) caps 2-ch at 61 Msps; immature drivers (ASPM/DMA tweaks); thermal; no coverage below 30–70 MHz |
| **B. Mid** | **Jetson Orin Nano Super / Orin NX** + **M.2/USB 3 SDR**: bladeRF 2.0 xA9, USRP B210/B200mini, LiteX-M2SDR, xSDR, or Epiq NV100 for 16-bit + GPSDO + preselect. Add a custom sub-octave preselector bank + step attenuator + LNA, GPSDO, and an HF path (RX888 MkII or RSPdx-R2). | ~$1,500–4,000 (NV100 quote extra) | 56–120 MHz real-time gap-free; GPU channelizer + CNN classifier; 2-ch coherent AoA; continuous spectrogram DB | AD9361 12-bit/56 MHz IBW; USB 3 overflow risk (prefer PCIe); 15–25 W compute power; Jetson software stack (JetPack) lock-in |
| **C. High** | **RFSoC** (RFSoC 4x2 academic / PZSDR / AXW22 / ZCU208) with an analog front end (per-ADC preselect/anti-alias filters, LNA, attenuators; optional block downconverter for >6 GHz) + **Orin NX/AGX** over 10/100 GbE | ~$5,000–20,000 | Direct sampling of ~0–6 GHz in GHz-wide chunks; on-FPGA PFB channelizer + detector + triggered capture; 4–8 coherent channels for DF; GPU classification of detections | Power (40–100 W), heat, cost; FPGA engineering effort (HLS/Vivado); the analog front end is non-trivial; academic-only 4x2 pricing |
| **Buy-in reference** | Signal Hound BB60D or Harogic SAN-60 + x86 mini PC; or Epiq Matchstiq G20/X40 | $3k–$6k; Matchstiq quote | Instrument-grade sweep/RT immediately | Closed firmware, x86/Windows-centric SDKs, not handheld |

### 7.2 Power budget sketches

These are estimates; measure on real hardware.

| Subsystem | Tier A | Tier B | Tier C |
|---|---|---|---|
| RF board | HackRF Pro ~2–3 W (USB-powered) | bladeRF/B210/M.2 SDR ~3–6 W | RFSoC board ~25–60 W |
| Front end (filters, switches, LNA, GPSDO) | ~0.5–1 W | ~1–3 W (GPSDO ~1–2 W) | ~3–10 W |
| Compute | Pi 5 ~4–10 W | Orin Nano Super 7–25 W (or Orin NX 10–25 W) | Orin NX/AGX 15–60 W |
| Display (5–7" IPS) | ~2–4 W | ~2–4 W | ~2–4 W or remote UI |
| **Total** | **~9–18 W** | **~15–38 W** | **~45–130 W** (cf. Matchstiq X40 at 40–80 W) |
| Runtime on 99 Wh battery | ~5–10 h | ~2.5–6 h | ~1–2 h (vehicle/backpack) |

### 7.3 Bottlenecks and design recommendations

1. **Use PCIe, not USB, above 20 MHz.** USB 3 on ARM SBCs is the least predictable link (Pi 5 bladeRF regression). M.2 SDRs (LiteX-M2SDR, xSDR, LimeSDR XTRX, Epiq NV100) on an Orin carrier's PCIe Gen3 slot are the cleanest mid-tier path. For Tier A, the HackRF Pro's USB 2.0 link is actually *predictable* at 20 Msps.
2. **Budget money and board area for the front end.** A switched sub-octave preselector plus FM/cellular notches plus a step attenuator plus overload detection will do more for automatic detection accuracy than doubling IBW.
3. **Build a hybrid sweep + real-time architecture.** Sweep 0–6 GHz for the occupancy map, then park the real-time receiver on active regions. Firmware-controlled retuning, as `hackrf_sweep` does, is essential. An FPGA (HackRF Pro now has one) lets this include filter-bank switching and burst triggers.
4. **Keep detection close to the data.** Run FFT/PFB and CFAR in FPGA or GPU, and send *events + snippets*, not raw IQ, to the classifier and storage. Classification cost scales with the number of detections.
5. **Put a clock reference in from day one.** Use at least a TCXO ≤0.5 ppm, with an optional GPSDO (1PPS) for timestamps, multi-unit TDoA, and accurate channel-raster matching in protocol ID.
6. **Plan for DF.** Two coherent channels (AD9361/LMS7002M) with a switched calibration noise source is a cheap, credible step; KrakenSDR's approach is the template. Five channels, or an RFSoC, gives unambiguous AoA.
7. **Cover HF separately.** None of the AD9361/LMS7002M boards cover HF well. Add an RX888 MkII- or RSPdx-R2-class receiver with an active loop.
8. **Treat these as open risks:**
   - xSDR/sSDR delivery status, LiteX-M2SDR pricing, HackRF One production status
   - Pi 5 PCIe driver maturity for SDRs
   - Jetson thermal limits inside a sealed handheld enclosure
   - Academic-only availability of RFSoC 4x2

---

## Sources

**Fundamentals**
- ADI MT-003, SINAD/ENOB/SNR/THD/SFDR — https://www.analog.com/media/en/training-seminars/tutorials/MT-003.pdf
- PySDR, IQ sampling / DC offset — https://pysdr.org/content/sampling
- Nuand bladeRF wiki, DC offset & IQ imbalance — https://github.com/Nuand/bladeRF/wiki/DC-offset-and-IQ-Imbalance-Correction
- Electronics Notes, reciprocal mixing — https://www.electronics-notes.com/articles/radio/radio-receiver-sensitivity/reciprocal-mixing.php
- ADI, high dynamic range transceiver / blocking — https://www.analog.com/en/resources/technical-articles/high-dynamic-range-rf-transceiver-solves-the-blocking-challenge.html
- rtl-sdr.com, FM band-stop / overload — https://www.rtl-sdr.com/rtl-sdr-com-broadcast-fm-band-stop-filter-88-108-mhz-reject-now-for-sale/ ; https://www.rtl-sdr.com/tag/overload/
- Leo Bodnar LBE-1420 GPSDO — https://www.leobodnar.com/shop/index.php?main_page=product_info&products_id=393

**2.4 GHz ISM (§2.3, unverified capability note)**
- Argenox, "Introduction to Bluetooth Classic" (79 channels, 1600 hops/s AFH) — https://argenox.com/library/bluetooth-classic/intro-bluetooth-classic
- Argenox, "BLE Advertising Primer" (advertising channels 37/38/39 at 2402/2426/2480 MHz, 1 Msps GFSK) — https://argenox.com/library/bluetooth-low-energy/ble-advertising-primer

**SDR hardware**
- HackRF Pro — https://greatscottgadgets.com/hackrf/pro/ ; https://www.greatscottgadgets.com/2025/06-26-meet-hackrf-pro/ ; https://greatscottgadgets.com/2025/12-03-hackrf-pro-receive-sensitivity-and-noise-figure/ ; https://www.rtl-sdr.com/hackrf-pro-pre-order-frequency-range-and-rf-performance-improvements-usb-c-tcxo-added/
- HackRF One — https://greatscottgadgets.com/hackrf/one/ ; https://hackrf.readthedocs.io/en/latest/hackrf_tools.html ; https://www.rtl-sdr.com/scanning-spectrum-8ghz-per-second-new-hackrf-update/
- Opera Cake — https://greatscottgadgets.com/hackrf/operacake/
- RTL-SDR Blog V4 — https://www.rtl-sdr.com/rtl-sdr-blog-v4-dongle-initial-release/ ; https://www.cnx-software.com/2023/08/17/rtl-sdr-blog-v4-dongle-launched-with-rafeal-r828d-tuner-chip/
- Airspy R2 / HF+ Discovery — https://airspy.com/airspy-r2/ ; https://airspy.com/airspy-hf-discovery/ ; https://www.rtl-sdr.com/airspy-hf-discovery-collection-of-tests-and-reviews/
- HydraSDR RFOne — https://www.rtl-sdr.com/hydrasdr-rfone-an-new-upcoming-sdr-similar-to-the-airspy-r2/ ; https://hydrasdr.com/products/
- SDRplay — https://www.sdrplay.com/rspdxr2/ ; https://www.rtl-sdr.com/sdrplay-rspdx-r2-released/ ; https://sdrplay.com/product/rsp1b/ ; https://sdrplay.com/product/rspduo/ ; https://www.elektor.com/products/sdrplay-rspdx-r2-single-tuner-14-bit-sdr-receiver-1-khz-to-2-ghz
- RX888 MkII — https://www.rx-888.com/rx/
- KiwiSDR 2 — https://www.rtl-sdr.com/kiwisdr-2-now-available-for-purchase/
- LimeSDR Mini 2.0 / XTRX / LMS7002M — https://www.cnx-software.com/2022/06/09/limesdr-mini-2-usb-sdr-lattice-semi-ecp5-fpga/ ; https://limemicro.com/story/lime-micro-opens-orders-for-the-limesdr-xtrx-an-open-hardware-mpcie-software-defined-radio/ ; https://www.crowdsupply.com/lime-micro/limesdr-xtrx ; https://limemicro.com/technology/lms7002m/
- bladeRF 2.0 micro — https://www.nuand.com/product/bladerf-xa4/ ; https://www.nuand.com/product/bladerf-xa9/
- Ettus USRP — https://www.ettus.com/all-products/usrp-b200mini/ ; https://www.ettus.com/all-products/usrp-b205mini-i/ ; https://www.ettus.com/all-products/ub210-kit/ ; https://www.ettus.com/all-products/x310-kit/ ; https://www.ettus.com/all-products/usrp-x410/ ; https://www.ettus.com/all-products/usrp-x420/ ; https://kb.ettus.com/X440 ; https://kb.ettus.com/index.php?action=pdfbook&format=single&title=N200%2FN210
- ADALM-PLUTO / AD936x / ADRV900x — https://www.rtl-sdr.com/adalm-pluto-new-149-tx-capable-sdr-325-3800-mhz-range-12-bit-adc-20-mhz-bandwidth/ ; https://wiki.analog.com/university/tools/pluto/devs/specs ; https://www.analog.com/en/products/ad9363.html ; https://www.analog.com/en/products/ad9361.html ; https://www.analog.com/en/products/adrv9002.html ; https://www.analog.com/en/products/adrv9009.html
- Pluto+ — https://www.amazon.com/Transceiver-70MHz-6GHz-Compatible-Open-Source-Development/dp/B0FSWWL3M5
- AntSDR — https://www.crowdsupply.com/microphase-technology/antsdr-e200 ; https://www.rtl-sdr.com/antsdr-e200-set-to-begin-crowdfunding-on-crowdsupply-soon/ ; https://www.cnx-software.com/2023/07/03/antsdr-e200-gigabit-ethernet-connected-sdr-with-xilinx-zynq-soc-fpga-supports-70-mhz-6-ghz-range/
- LiteX-M2SDR — https://github.com/enjoy-digital/litex_m2sdr ; https://github.com/enjoy-digital/litex_m2sdr/blob/main/doc/hosts/raspberry-pi-5.md
- Wavelet Lab — https://www.rtl-sdr.com/tag/usdr/ ; https://www.rtl-sdr.com/xsdr-crowdfunding-campaign-now-live/ ; https://www.rtl-sdr.com/tag/ssdr/ ; https://www.crowdsupply.com/wavelet-lab/usdr
- RFNM — https://rfnm.com/
- Epiq Solutions — https://militaryembedded.com/comms/sdr/epiq-solutions-announces-new-sidekiq-nv100-an-embeddable-software-defined-radio-sdr-with-extended-rf-tuning-and-high-dynamic-range ; https://www.prnewswire.com/news-releases/epiq-solutions-announces-the-sidekiq-x4-rf-transceiver-for-high-bandwidth-multi-channel-applications-300663810.html ; https://epiqsolutions.com/products/sdr/matchstiq-x40 ; https://linuxgizmos.com/epiq-solutions-matchstiq-x40-and-g-series-for-edge-level-ai-ml-rf-spectrum-analysis/
- Deepwave AIR-T — https://deepwave.ai/hardware-products/air-t-embedded-series/ ; https://docs.deepwave.ai/AIR-T/Products/AIR8201/edge_series_product_guide/
- KrakenSDR — https://www.crowdsupply.com/krakenrf/krakensdr ; https://hackerwarehouse.com/product/krakensdr/ ; https://www.crowdsupply.com/krakenrf/krakensdr/updates/june-shipping-update-and-pricing-change ; https://www.cnx-software.com/2022/01/31/krakensdr-is-a-5-channel-software-defined-radio-based-on-rtl-sdr/
- Signal Hound — https://signalhound.com/products/bb60d-6-ghz-real-time-spectrum-analyzer/ ; https://signalhound.com/products/sm200c-20-ghz-real-time-spectrum-analyzer-with-10gbe/
- ThinkRF R5550 — https://thinkrf.com/real-time-spectrum-analyzers/r5550-real-time-spectrum-analyzer/ ; https://www.saelig.com/product/r5550-427.htm
- Harogic — https://www.harogic.com/product/san-45-usb-spectrum-analyzer/ ; https://eleshop.eu/harogic-san-60-usb-spectrum-analyser.html
- Tektronix RSA306B — https://www.tek.com/en/products/spectrum-analyzers/rsa306 ; https://us.rs-online.com/product/tektronix/rsa306b/70816171/
- Per Vices — https://www.pervices.com/crimson-tng/ ; https://support.pervices.com/cyan/specs/
- RFSoC boards — https://www.realdigital.org/hardware/rfsoc-4x2 ; https://github.com/Xilinx/RFSoC-PYNQ/blob/master/docs/rfsoc_4x2_overview.md ; https://www.xilinx.com/publications/product-briefs/xilinx-zcu208-product-brief.pdf ; https://www.amd.com/en/products/adaptive-socs-and-fpgas/evaluation-boards/zcu111.html ; https://www.rtl-sdr.com/pzsdr-new-amd-zync-ultrascale-based-sdr-crowd-funding-on-crowd-supply/ ; https://www.en.alinx.com/Product/SoC-Development-Boards/Zynq-UltraScale-plus-RFSoC/AXW22.html ; https://embeddedcomputing.com/application/networking-5g/avnet-s-new-rfsoc-development-kit-enhances-wireless-design
- Red Pitaya — https://redpitaya.readthedocs.io/en/latest/developerGuide/hardware/ORIG_GEN/122-16/top.html
- Fairwaves XTRX successor — https://limemicro.com/story/lime-micro-opens-orders-for-the-limesdr-xtrx-an-open-hardware-mpcie-software-defined-radio/

**Compute**
- Raspberry Pi 5 — https://www.raspberrypi.com/products/raspberry-pi-5/ ; https://www.raspberrypi.com/documentation/accessories/m2-hat-plus.html ; https://forums.raspberrypi.com/viewtopic.php?t=374733 ; https://discourse.myriadrf.org/t/limesdr-xtrx-operation-on-raspberry-pi-5-via-mpcie-hat/8850 ; https://www.tech-reader.blog/2025/06/insight-why-dragonos-runs-best-on-x86.html ; https://www.phoronix.com/review/raspberry-pi-5-benchmarks/3
- Jetson Orin — https://developer.nvidia.com/blog/nvidia-jetson-orin-nano-developer-kit-gets-a-super-boost/ ; https://mm.digikey.com/Volume0/opasdata/d220001/medias/docus/5380/Jetson_Orin_Nano_Series_DS-11105-001_v1.1.pdf
- NVIDIA Holoscan / cuSignal / Aerial — https://developer.nvidia.com/blog/developing-streaming-sensor-applications-with-holohub-from-nvidia-holoscan/ ; https://developer.nvidia.com/blog/nvidia-holoscan-sensor-bridge-empowers-developers-with-real-time-data-processing/ ; https://github.com/rapidsai/cusignal ; https://developer.nvidia.com/industries/telecommunications/ai-aerial
- RK3588 / Orange Pi 5 — http://www.orangepi.org/html/hardWare/computerAndMicrocontrollers/details/Orange-Pi-5.html
- AMD Kria — https://www.amd.com/en/blogs/2025/kria-kv260-vision-ai-starter-kit-refresh.html
- SDR++ on Apple Silicon — https://github.com/AlexandreRouma/SDRPlusPlus/discussions/994

**FPGA**
- RFNoC — https://kb.ettus.com/Getting_Started_with_RFNoC_in_UHD_4.0 ; https://files.ettus.com/manual/classuhd_1_1rfnoc_1_1ddc__block__control.html ; https://files.ettus.com/manual/classuhd_1_1rfnoc_1_1fft__block__control.html ; https://github.com/e33b1711/rfnoc_pp_channelizer
- RFSoC OPFB channelizer — https://ieeexplore.ieee.org/document/9336352/ ; https://github.com/MazinLab/RFSoC_OPFB
- StrathSDR RFSoC spectrum analyzer — https://github.com/strath-sdr/rfsoc_xilinx_adapt ; http://www.rfsoc-pynq.io/
- ADI HDL — https://analogdevicesinc.github.io/hdl/projects/pluto/index.html
- openXC7 — https://github.com/openXC7/toolchain-installer

**Antennas & accessories**
- Diamond D130J — https://www.diamondantenna.net/d130j.html
- LP0965 LPDA — https://www.ettus.com/all-products/lp0965/ ; https://www.wa5vjb.com/products1.html
- Active loops — https://www.rtl-sdr.com/reviews-of-the-low-cost-mla-30-wide-band-hf-magnetic-loop-antenna/ ; https://www.electronics-notes.com/store-shop/ham-radio-reviews/mla-30plus-hf-loop-antenna-review.php

**Handheld / portable precedents**
- Mayhem firmware — https://github.com/portapack-mayhem/mayhem-firmware/wiki/Features ; https://github.com/portapack-mayhem/mayhem-firmware/wiki/Scanner ; https://lab401.com/products/portapack-h4m-pro
- Malahit DSP2 — https://www.radioddity.com/products/raddy-dsp2
- tinySA Ultra+ — https://www.randl.com/index.php?main_page=product_info&products_id=76237
- RF Explorer — https://j3.rf-explorer.com/6gcomboplus ; https://rfexplorer.com/models/
- Keysight FieldFox — https://www.keysight.com/us/en/about/newsroom/news-releases/2025/1204_pr26-004-keysight-introduces-new-handheld-analyzer-enabling-120-mhz-iq-streaming-for-gap-free-signal-capture.html ; https://www.keysight.com/us/en/products/spectrum-analyzers-signal-analyzers/fieldfox-handheld-spectrum-analyzers.html
- R&S Spectrum Rider FPH — https://www.rohde-schwarz.com/us/products/test-and-measurement/handheld/rs-spectrum-rider-fph-handheld-spectrum-analyzer_63493-147712.html
- CRFS RFeye — https://www.crfs.com/hardware/rf-sensors ; https://www.crfs.com/hardware/rf-sensors/rfeye-node-plus
