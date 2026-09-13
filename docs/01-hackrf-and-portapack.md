# 01 — HackRF, PortaPack, and Mayhem: State of the Art (as of 2026-09-13)

> Research for an "exploration-first" handheld signals-analysis device that could replace a HackRF + PortaPack.
> Method: primary sources first (Great Scott Gadgets docs/blog, GitHub repos, releases, and commit history via the GitHub API, the Mayhem wiki, and vendor product pages). Where I couldn't verify something, or sources disagree, I say so inline with **[unverified]** or **[conflict]**.

---

## 0. Executive summary: is HackRF/PortaPack stagnant?

**Short answer: the projects aren't stagnant, but the core architecture is.** Development activity is healthy and there's new hardware in 2025–2026. The fundamental RF/compute envelope (8-bit samples, 20 Msps, USB 2.0, an LPC43xx microcontroller doing the handheld DSP) hasn't changed since 2014. The new HackRF Pro deliberately keeps the same USB-facing envelope for backward compatibility.

Evidence (GitHub API queries run 2026-09-13 unless noted):

| Indicator | HackRF ([greatscottgadgets/hackrf](https://github.com/greatscottgadgets/hackrf)) | Mayhem ([portapack-mayhem/mayhem-firmware](https://github.com/portapack-mayhem/mayhem-firmware)) |
|---|---|---|
| Stars / forks | 8,102 / 1,722 | 5,407 / 917 |
| Contributors (incl. anonymous) | ~99 | ~142 |
| Last push | 2026-09-10 | 2026-09-03 |
| Commits by calendar year (default branch) | 2022: 315 · 2023: 104 · 2024: 88 · 2025: 136 · **2026 (to Sep): 396** | 2020: 414 · 2021: 225 · 2022: 339 · **2023: 757** · 2024: 442 · 2025: 237 · 2026 (to Sep): 240 |
| Tagged releases | 2021.03.1, 2022.09.1, 2023.01.1, 2024.02.1, **then a ~23-month gap**, 2026.01.1 / .2 / .3 (Jan 2026) ([releases](https://github.com/greatscottgadgets/hackrf/releases)) | Stable: v2.0.0 (2024-02-16), v2.0.1, v2.0.2, v2.1.0 (2024-12-20), v2.2.0 (2025-07-11), v2.3.1 (2025-11-07), v2.3.2 (2025-12-21), **v2.4.0 (2026-03-24)**; plus 738 total releases including nightlies (226 nightlies in 2023, 180 in 2024, 128 in 2025, 110 in 2026 so far) ([releases](https://github.com/portapack-mayhem/mayhem-firmware/releases)) |
| New hardware | **HackRF Pro** announced 2025-06-26, shipped to resellers Jan 2026 | Supports H4M, H4M Pro, PortaRF, HackRF Pro (WIP) |

How to read this:

- **The user's impression is partly right for HackRF One host/firmware releases.** There was no tagged release between Feb 2024 and Jan 2026. GSG's engineering went into HackRF Pro during that time. In 2026 the hackrf repo had its busiest year since at least 2022. The work was mostly FPGA gateware DSP, clock correction (USB API bumped to 1.13), a transceiver temperature readout, and driver refactoring (commit titles, Apr–Sep 2026).
- **Mayhem commit volume peaked in 2023 and has roughly halved since**, but it's still steady. There are near-daily nightlies, a stable release every ~4–8 months, and new apps in every release (see §3.8).
- **What really is stagnant is the platform ceiling.** The HackRF One (2014) and HackRF Pro (2026) both deliver 8-bit I/Q at up to 20 Msps over USB 2.0. The PortaPack still runs all DSP on the HackRF's own dual-core Cortex-M4/M0 MCU with ~200 kB of RAM. Mayhem's maintainers explicitly refuse feature requests like DMR/P25 decoding because "the hardware is too low" ([issue #2516](https://github.com/portapack-mayhem/mayhem-firmware/issues/2516)).

---

## 1. HackRF One hardware

### 1.1 Signal chain and major components

HackRF One is a half-duplex SDR transceiver peripheral covering 1 MHz–6 GHz ([HackRF One docs](https://hackrf.readthedocs.io/en/latest/hackrf_one.html)). Major parts ([hardware components](https://hackrf.readthedocs.io/en/latest/hardware_components.html)):

| Block | Part (HackRF One) | Role |
|---|---|---|
| RF front end | RF switches (SKY13350 / SKY13453, varies by revision), optional ~14 dB RX/TX amp | Path selection, antenna port |
| Mixer / synthesizer | **RFFC5072** | Up/down-converts between the tuned RF and the transceiver's 2.3–2.7 GHz IF, for everything outside the IF band |
| Transceiver | **MAX2837** (r1–r8, r10) / **MAX2839** (r9) | 2.3–2.7 GHz zero-IF transceiver; LNA gain 0–40 dB in 8 dB steps, VGA 0–62 dB in 2 dB steps (see `hackrf_sweep` usage), programmable baseband low-pass filter |
| ADC/DAC | **MAX5864** | 8-bit I/Q ADC, 10-bit DAC. The datasheet rates it for ~22 Msps **[datasheet not re-fetched this session]** |
| Glue logic | Xilinx **CoolRunner-II CPLD** | Sample bus between the ADC/DAC and the MCU |
| MCU | NXP **LPC4320** (Cortex-M4F + Cortex-M0, flashless) | USB High-Speed, sample streaming via SGPIO, radio control; runs PortaPack firmware in standalone mode |
| Clock | **Si5351C** (r9: Si5351A plus extra clock distribution) | Clock generation; CLKIN/CLKOUT |
| Flash | W25Q80BV, 8 Mbit (1 MB) | Firmware |

Revision history, per GSG ([hardware revisions](https://hackrf.readthedocs.io/en/latest/list_of_hardware_revisions.html)). "Hardware revisions exist mainly to deal with changes in component availability", and every revision meets the same factory specs:

| Rev | Change | Years |
|---|---|---|
| r1–r4 | Same design | 2014–2020 |
| r5 | Experimental, never made | — |
| r6 | SKY13350 → SKY13453 switches; revision-detect pin straps added | 2020 |
| r7 | Back to SKY13350; VBUS resistor change | 2021 |
| r8 | SKY13453 again | 2021–2022 |
| r9 | **MAX2837 → MAX2839**; Si5351C → Si5351A; antenna-power series diode | 2023 |
| r10 | Based on r8 (reverts most r9 changes), keeps the diode | 2024 |

The 2022–2023 HackRF shortage (the discontinued MAX2837) drove r9 ([GSG blog "HackRF One Shortage", Dec 2022](https://greatscottgadgets.com/tags/hackrf/)). Most clones sold today are labeled "R10C" or similar ([rtl-sdr.com H4M review](https://www.rtl-sdr.com/a-review-of-the-new-hackrf-portapack-h4m/)). They're not GSG products.

### 1.2 Specifications

| Parameter | HackRF One | Source |
|---|---|---|
| Frequency | 1 MHz–6 GHz (operating); `hackrf_sweep` allows up to 7250 MHz | [docs](https://hackrf.readthedocs.io/en/latest/hackrf_one.html), [`hackrf_sweep.c`](https://github.com/greatscottgadgets/hackrf/blob/main/host/hackrf-tools/src/hackrf_sweep.c) |
| Sample rate | 2–20 Msps quadrature; **below 8 Msps not recommended** (MAX5864 isn't specified below 8 Msps; the minimum 1.75 MHz baseband filter gives only ~4 dB rejection at ±1 MHz) | [sampling rate doc](https://github.com/greatscottgadgets/hackrf/blob/main/docs/source/sampling_rate.rst) |
| Resolution | 8-bit I and Q | docs |
| Duplex | Half-duplex | docs |
| Max RX input | **−5 dBm** (damage risk); ~+10 dBm theoretically OK with the amp off | docs |
| TX power | 5–15 dBm below 2.17 GHz; 13–15 dBm at 2.17–2.74 GHz; 0–5 dBm at 2.74–4 GHz; −10 to 0 dBm at 4–6 GHz | docs |
| Antenna port power | Software-controlled, max 50 mA at 3.0–3.3 V (`hackrf_biast` since 2024.02.1) | docs, releases |
| Interface | USB 2.0 High-Speed, Micro-B | docs |
| Clock | CLKOUT: 10 MHz 3.3 V square wave. CLKIN: expects 10 MHz 3.3 V; auto-selected when present, switched only at the start of RX/TX | [external clock doc](https://hackrf.readthedocs.io/en/latest/external_clock_interface.html) |
| Frequency reference | Plain crystal, **no TCXO** on HackRF One. GSG's product page gives no ppm figure. **[unverified: the commonly quoted ±20 ppm]** | [GSG HackRF One page](https://greatscottgadgets.com/hackrf/one/) |

### 1.3 Noise figure, dynamic range, and overload

- **GSG declines to publish a sensitivity number.** Its docs say minimum detectable power "isn't a question that can be answered for a general purpose SDR platform" ([docs](https://hackrf.readthedocs.io/en/latest/hackrf_one.html)). I found **no GSG-published numeric noise figure for HackRF One**. GSG's Dec 2025 post shows an NF-vs-frequency plot comparing One and Pro. The text describes "solid improvement across almost the whole tuning range, with significant improvement at higher frequencies" but gives no numbers ([HackRF Pro Receive Sensitivity and Noise Figure](https://greatscottgadgets.com/2025/12-03-hackrf-pro-receive-sensitivity-and-noise-figure/)). **[unverified: specific NF values]**
- **8-bit ADC.** The ideal quantization SNR is ~50 dB (6.02·8 + 1.76). Reviewers note usable dynamic range in practice is "closer to 6 bits", and any strong in-band signal can overload the ADC and block weak ones ([rtl-sdr.com HackRF review](https://www.rtl-sdr.com/hackrf-initial-review/); [Airspy vs SDRplay vs HackRF](https://www.rtl-sdr.com/review-airspy-vs-sdrplay-rsp-vs-hackrf/)).
- **No band preselection.** The Mayhem wiki states "HackRF has no built-in RF filtering, making it susceptible to out-of-band interference". It recommends FM-band notch filters against broadcast and pager overload ([Receivers](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Receivers); [Receive Quality Issues](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Help!-Im-not-receiving-anything!---Receive-Quality-Issues)). *Nuance:* the mixer path does have image-reject LPF/HPF switching. What it lacks is tunable preselection.
- **DC spike** at the center of the zero-IF band (the HackRF Pro claims "elimination of the DC spike", which implies it's a known One artifact) ([HackRF Pro docs](https://hackrf.readthedocs.io/en/latest/hackrf_pro.html)).

### 1.4 USB 2.0 throughput ceiling

At 20 Msps × 2 bytes per I/Q pair, that's **40 MB/s, or about 320 Mbit/s**. Real-world USB 2.0 High-Speed bulk throughput tops out around 35–40 MB/s, so the 20 Msps cap is set by the bus as much as the ADC. *(My own arithmetic, not a GSG statement.)* GSG's own evidence: HackRF Pro's FPGA can sample at 40 Msps internally, but it only exposes 40 Msps over USB in a **4-bit** "half-precision" mode "within the constraints of the USB interface" ([gateware docs](https://github.com/greatscottgadgets/hackrf/blob/main/docs/source/gateware.rst)). Hosts with weak USB controllers or CPUs drop samples at 20 Msps. Commenters raised this for Raspberry Pi use back in 2017 ([rtl-sdr.com](https://www.rtl-sdr.com/scanning-spectrum-8ghz-per-second-new-hackrf-update/)).

### 1.5 Opera Cake antenna switch

Opera Cake was released Oct 2022 ([blog](https://greatscottgadgets.com/tags/hackrf/)), with firmware support in 2022.09.1. It's a stackable add-on board with **two 1×4 switches**, usable as a 1×8 selector. Up to 8 boards can stack on one HackRF ([Opera Cake docs](https://hackrf.readthedocs.io/en/latest/opera_cake.html)). Modes ([modes of operation](https://github.com/greatscottgadgets/hackrf/blob/main/docs/source/opera_cake_modes_of_operation.rst)):

- **manual**: fixed port assignment.
- **frequency**: switches port automatically on retune, using a priority-ordered band plan (e.g. `-f A1:100:600 -f A3:600:1200 -f B2:0:4000`). It persists across other host software, so a **filter bank or per-band antenna set works transparently with `hackrf_sweep`**. That's directly relevant to a sweep-based survey instrument.
- **time**: cycles ports every N samples, intended for pseudo-Doppler direction finding.

### 1.6 Firmware, `hackrf_sweep`, and host tools

Host tools ([HackRF tools doc](https://github.com/greatscottgadgets/hackrf/blob/main/docs/source/hackrf_tools.rst)):

| Tool | Purpose |
|---|---|
| `hackrf_info` | Device and firmware info; hardware revision detection from r6 |
| `hackrf_transfer` | Raw RX/TX to and from 8-bit signed I/Q files |
| `hackrf_sweep` | Command-line spectrum analyzer that sweeps in firmware |
| `hackrf_clock` | CLKIN/CLKOUT configuration (added 2021.03.1) |
| `hackrf_operacake` | Opera Cake configuration |
| `hackrf_spiflash` | Firmware flashing |
| `hackrf_debug` | Register access; since Jul 2026 also transceiver temperature |
| `hackrf_biast` | Antenna port power (2024.02.1) |

`libhackrf` got comprehensive API documentation in 2024.02.1 ([releases](https://github.com/greatscottgadgets/hackrf/releases)). It supports all HackRF variants (One, Pro, Jawbreaker, rad1o).

**How `hackrf_sweep` works** (from [`hackrf_sweep.c`](https://github.com/greatscottgadgets/hackrf/blob/main/host/hackrf-tools/src/hackrf_sweep.c) and the [2017 announcement coverage](https://www.rtl-sdr.com/scanning-spectrum-8ghz-per-second-new-hackrf-update/)):

- Introduced in release 2017.02.1. The **firmware retunes autonomously** through a list of frequencies, so the host doesn't send a USB control request per step. That's what makes it fast.
- Fixed at **20 Msps with a 15 MHz baseband filter**. It steps in 20 MHz increments (`TUNE_STEP`) with a 7.5 MHz offset (`OFFSET`). Each tuning produces two 5 MHz output slices, keeping clear of the DC spike and the filter skirts (the doc's sample output shows 5 MHz rows arriving out of order: 2400–2405, 2410–2415, 2405–2410…).
- FFT bin width is 2,445 Hz to 5 MHz (`-w`). Output is CSV `date, time, hz_low, hz_high, bin_width, num_samples, dB…` or binary. Each full sweep gets a single timestamp ([doc](https://github.com/greatscottgadgets/hackrf/blob/main/docs/source/hackrf_tools.rst)).
- **Speed:** advertised **~8 GHz/s**, which is 0–6 GHz in about 0.75 s. rtl-sdr.com compared that to ~1 GHz/s for an Airspy ([rtl-sdr.com](https://www.rtl-sdr.com/scanning-spectrum-8ghz-per-second-new-hackrf-update/)). Derived: 8 GHz/s ÷ 20 MHz/step ≈ **400 retunes/s, or ~2.5 ms per step** including PLL settling. *(My derivation; actual dwell depends on bin width and host.)*

What `hackrf_sweep` **can't** do:
- No I/Q output, no demodulation, no triggered capture. It only produces power spectra.
- **Low probability of intercept for bursty signals.** Each 20 MHz chunk is observed for only a few ms per sweep, so a 20 ms key-fob burst or a 100 ms PTT on a frequency is easily missed.
- No amplitude calibration (dBFS-like values), 8-bit dynamic range, and residual spurs.
- It's a host tool. The PortaPack's "Looking Glass" is a separate on-device implementation (§3.4).

**Firmware release status 2025–2026:** 2026.01.1 (Jan 5 2026) added HackRF Pro support, support for newer PortaPacks with **AGM Microelectronics CPLDs**, and Windows USB performance improvements. 2026.01.2 (Jan 16) fixed rad1o, intermittent tuning failures with SDR++, and Pro RX spectrum inversion. 2026.01.3 (Jan 30) fixed mixer lock failures and added access to the Pro's larger SPI flash ([releases](https://github.com/greatscottgadgets/hackrf/releases)). No tagged release since, but `main` is very active: an FPGA complex mixer using SB_MAC16 blocks, CIC improvements, a `RADIO_CLOCK_CORRECTION` register (USB API 1.13, Jun 2026), temperature readout, Si5351 driver rework, and HIL CI (commit log, Apr–Sep 2026).

### 1.7 HackRF Pro (codename "Praline")

Announced **2025-06-26** ([Meet HackRF Pro](https://www.greatscottgadgets.com/2025/06-26-meet-hackrf-pro/); [Q&A](https://greatscottgadgets.com/2025/06-27-hackrf-pro-q-a/); [product page](https://greatscottgadgets.com/hackrf/pro/); [docs](https://hackrf.readthedocs.io/en/latest/hackrf_pro.html)).

| Parameter | HackRF Pro | Change vs One |
|---|---|---|
| Frequency | 100 kHz–6 GHz operating; **tunable 0 Hz–7.1 GHz** (One is "quite lossy above 6.1 or 6.2 GHz") | Wider |
| USB samples | Up to 20 Msps, 8-bit I/Q (default, compatible) | Same |
| Extended precision | **16-bit samples, ENOB 9–11**, minimum FPGA decimation 16× | New |
| Half precision | **4-bit samples up to 40 Msps** | New |
| Logic | **Lattice iCE40UP5K FPGA** (5,280 LUT4s, 8 DSP blocks), gateware in Amaranth HDL, built with the open icestorm toolchain; bitstreams switchable at runtime | CPLD → FPGA |
| Standard gateware DSP | DC blocker, fs/4 shifter, decimation 1–32× (RX), interpolation 1–32× (TX) | New |
| Transceiver | **MAX2831** | Was MAX2837/2839 |
| ADC/DAC | **MAX5865** per GSG's block diagram. **[conflict]** GSG's component list still names MAX5864 without a Pro note. | — |
| MCU | **LPC4330** per GSG's block diagram and OpenSourceSDRLab. **[conflict]** The hackrf CMake sets `MCU_PARTNO LPC4320` for `PRALINE`, possibly a conservative memory map. | More SRAM (if LPC4330) |
| Reference | Built-in **TCXO** (block diagram and OpenSourceSDRLab: 25 MHz, 0.5 ppm) | New |
| Flash | W25Q32, 32 Mbit (4 MB) | 4× |
| Clock ports | Two configurable SMA ports (default CLKIN/CLKOUT); trigger in/out; a second independent CLKIN on header P22 ([clock doc](https://hackrf.readthedocs.io/en/latest/external_clock_interface.html)) | More flexible |
| Other | USB-C (still USB 2.0 HS), internal shielding, DC spike eliminated, improved power management, RF port protection, **hardware TX-disable**, PCB cutout for future add-ons | — |
| Duplex | Half-duplex | Same |

**Timeline:** production files mid-July 2025 → target Sept 2025 → slipped to end of Oct because of a crystal oscillator lead time ([Sept 2025 update](https://www.greatscottgadgets.com/2025/09-05-hackrf-pro-production-timeline-update/)) → Dec 2025 for macOS USB fixes → "all prepaid reseller preorders have shipped" by 2026-01-16 ([shipping update](https://greatscottgadgets.com/2026/01-16-hackrf-pro-shipping-update/)). Initial production revision is r1.2.1. Some r1.2.1-p1 units had a flaky USB-C connector, and GSG published a self-repair guide in March 2026 ([blog index](https://greatscottgadgets.com/tags/hackrf/)). **The HackRF One is still sold** alongside the Pro ([GSG One page](https://greatscottgadgets.com/hackrf/one/)).

**Performance claims:** better NF across the range (no numbers published). ADS-B tests with simple dipoles showed "15 to 50 km extra maximum range" and "double the number of valid messages" versus HackRF One, and positions out to ~400 km in good conditions ([Dec 2025 post](https://greatscottgadgets.com/2025/12-03-hackrf-pro-receive-sensitivity-and-noise-figure/)). GSG notes these were prototypes and results vary by unit.

**Price:** GSG's pages don't list MSRP. **[unverified]** OpenSourceSDRLab sells a "Pro r1.2.1 + PortaPack H4M Pro" bundle for **$266** without battery ([listing](https://opensourcesdrlab.com/products/hackrf-pro-h4m-pro)). I couldn't confirm whether that board is GSG-manufactured or a third-party build of the open design.

**Implication for us:** the Pro's FPGA is a genuine but small step toward on-radio DSP. At 5,280 LUTs it can do DC removal, fs/4 shifts, and CIC/FIR decimation, but not a multi-channel channelizer or FFT engine. The Pro still hands **≤20 Msps of 8-bit (or ≤40 Msps of 4-bit)** samples to whatever compute sits behind it.

---

## 2. PortaPack hardware

### 2.1 Concept

The PortaPack is a daughterboard that mounts on the HackRF One's expansion headers. It adds a display, controls, audio, a microSD card, a better clock, and (on later clones) a battery. It **adds no processor**: all firmware, UI and DSP, runs on the HackRF's LPC4320. Designed by Jared Boone (ShareBrained Technology), shown at DEF CON 23 in Aug 2015 ([GSG blog](https://greatscottgadgets.com/tags/hackrf/); [H1 hardware in Mayhem repo](https://github.com/portapack-mayhem/mayhem-firmware/tree/master/hardware/portapack_h1)).

### 2.2 Variants

| Model | Origin | Display | Audio | Power | Notes |
|---|---|---|---|---|---|
| **H1** | ShareBrained, open design (KiCad) | 2.4" 240×320 TFT | **AK4951** codec (some WM8731); 1 W speaker amp built into the AK4951 | USB only; no battery or charger | 2.5 ppm TCXO, coin-cell RTC backup, own CPLD (QFP64) for LCD/IO |
| **H2** | Unknown origin; **no public design** | 3.2" touch | AK4951 + CS8122S 3 W class-D amp | Internal Li-ion, IP5306-type charger, power switch | Some units inject charger noise when USB is connected |
| **H2M** | Mayhem-branded H2 | 3.2" | AK4951 | Battery | Contributor names silkscreened |
| **"H2+" variants** | Various clones | 3.2" | **WM8731L** (no speaker amp) + INS8002E/LTK8002D; low volume on some | Battery | QFP100 CPLD; component swaps without notice |
| **H3** | Clone | — | — | — | Closed-source firmware; Mayhem wiki: "Do not buy or support as it's a scam" |
| **H4M** | OpenSourceSDRLab, Mayhem "Signature Edition" | 3.2" 320×240 resistive touch, matte | Built-in speaker **and microphone** | 2500 mAh pouch cell, battery-management IC with %, V, and current readout | **GPIO/I²C port**, USB-C, flat "iPod-style" body, low-profile wheel, proper power button; gerbers contributed to Mayhem in v2.2.0 |
| **H4M Pro** | OpenSourceSDRLab | Touch | Speaker/mic | **User-replaceable 18650** (2500 mAh) | For HackRF Pro; hardware files added to Mayhem repo Aug 2026 ([nightly 2026-08-25](https://github.com/portapack-mayhem/mayhem-firmware/releases)) |
| **PortaRF** | OpenSourceSDRLab, May 2026 | **4" IPS** touch | — | 3000 mAh | **HackRF One + H4M on a single PCB**; optional ESP32-S3 "AI voice control" add-on (beta); from $220 ([CNX Software](https://www.cnx-software.com/2026/05/14/portarf-single-board-sdr-mixes-hackrf-one-and-portapack-h4m-hardware-adds-ai-voice-control/)); Mayhem added dynamic screen layouts for it in v2.3.1 |

Sources: [PortaPack Versions](https://github.com/portapack-mayhem/mayhem-firmware/wiki/PortaPack-Versions), [Differences H1/H2](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Differences-Between-H1-and-H2-models), [Hardware overview](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Hardware-overview), [rtl-sdr.com H4M review](https://www.rtl-sdr.com/a-review-of-the-new-hackrf-portapack-h4m/) (H4M + R10C clone bundle $152), [Lab401 H4M](https://lab401.com/collections/all-products-no-flipper/products/portapack-h4m), [OpenSourceSDRLab H4M Pro](https://opensourcesdrlab.com/products/hackrf-pro-h4m-pro).

**[conflict]** The Mayhem versions table (as I read it via fetch) shows H4M with an IP5306 battery IC and none on H2, while the H1/H2 differences page says H2 uses an IP5306 "or similar". Treat charger ICs as vendor-variable. **[unverified]** CNX says the PortaRF uses a MAX2837, which is odd because that part is discontinued. It could be a clone-sourced part or a reporting error.

### 2.3 Common hardware elements

- **TCXO:** the PortaPack supplies a TCXO to the HackRF clock input. The H1 spec is 2.5 ppm, and clone listings commonly claim 0.5 ppm. Some units leave it unpopulated ([Hardware overview](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Hardware-overview)).
- **CPLD on the PortaPack** drives the parallel LCD bus. Newer units use AGM CPLDs (AG256) instead of Altera 5M40Z, which needed HackRF firmware 2026.01.1 and Mayhem support ([Mayhem structure diagram](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Description-of-the-Structure)).
- **Controls:** a click wheel/encoder plus a 5-way pad. H2 and later add resistive touch.
- **microSD:** needed for external apps, maps, freqman lists, captures, and settings.
- **Enclosures:** 3D-printed ([wiki](https://github.com/portapack-mayhem/mayhem-firmware/wiki/3d-printed-enclosure)), aluminum clone cases, and vendor plastic shells.
- **Power draw:** one user's 63.7 h continuous Recon run used 120.4 Wh at ~4.87 V × 0.42 A, i.e. **≈2 W average** with the screen on ([Recon wiki](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Recon)).
- **Mixed-vendor warning:** "Most manufacturers change components frequently without notifying anyone" ([PortaPack Versions](https://github.com/portapack-mayhem/mayhem-firmware/wiki/PortaPack-Versions)).

---

## 3. Firmware

### 3.1 Lineage

1. **ShareBrained PortaPack firmware** ([sharebrained/portapack-hackrf](https://github.com/sharebrained/portapack-hackrf)): the original, by Jared Boone. 1,096 stars; last push Jan 2024; effectively dormant. It established the M4/M0 split and memory map.
2. **Havoc** ([furrtek/portapack-havoc](https://github.com/furrtek/portapack-havoc)): Furrtek's fork, which added many TX/RX apps (mic transceiver 2017, capture/replay). 876 stars; last push Jul 2020; abandoned.
3. **Mayhem**: started as eried's fork in 2020 (`v1.0.0` on 2020-05-12). It moved to the **portapack-mayhem** GitHub org around v2.0.0 (Feb 2024) ([v2.0.0 notes](https://github.com/portapack-mayhem/mayhem-firmware/releases/tag/v2.0.0)). It's now the de facto firmware. The default branch is `next`, and nightlies are built by GitHub Actions.

### 3.2 Architecture (M4 baseband vs M0 application)

From the Mayhem wiki's [Firmware Architecture](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Firmware-Architecture) page (inherited from ShareBrained):

- The LPC4320 has "a total of 200Kbytes of RAM and 1Mbyte of SPI flash" on HackRF One.
- **Cortex-M4F does baseband DSP** under ChibiOS with three threads: *Baseband* (processes sample buffers into audio, packets, and so on), *RSSI*, and *Default* (message handling).
- **Cortex-M0 runs the whole UI** plus light packet post-processing in a single event-driven thread.
- Memory map:
  - M4 **code in 32 kB** of local SRAM, M4 **data in 96 kB**
  - **8 kB shared SRAM for M4↔M0 messages**
  - M0 data in 64 kB AHB SRAM
  - M0 code executes in place from quad-SPI flash (SPIFI at 100 MHz SCK, ~50 MB/s)
- **Baseband images** are per-app DSP modules, compressed in flash and loaded into M4 RAM when an app starts. The app (M0) and baseband (M4) exchange typed messages (`MessageHandlerRegistration`, `Message::ID::…`) ([Create a Simple App](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Create-a-Simple-App)).
- **HackRF mode** chain-loads a slightly modified stock HackRF firmware, so the device works as a normal USB SDR ([structure diagram](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Description-of-the-Structure)). The DFU bootloader in LPC ROM can't be overwritten, so the device is effectively unbrickable.

**Concrete DSP chain examples** (source on `next`):

- **NFM receiver** (`proc_nfm_audio.hpp`): `baseband_fs = 3,072,000` Hz, 8-bit complex input buffers.
  - First stage: `FIRC8xR16x24FS4Decim4`, a 24-tap FIR that shifts by fs/4 and decimates by 4, going from 8-bit to 16-bit.
  - Then a complex channel filter/decimator, an FM demodulator, audio decimation, and a CTCSS filter.
  - It uses **fixed-point, multiplier-light, fs/4 tricks** throughout.
- **Wideband spectrum** (`proc_wideband_spectrum.cpp`, used by Looking Glass). The source comments are revealing:
  - `// 2048 complex8_t samples per buffer. 102.4us per buffer. 20480 instruction cycles per buffer.` That's **~10 CPU instruction cycles per sample at 20 Msps.**
  - `// TODO: Removed window-presum windowing, due to lack of available code RAM.`

  These two comments summarize §4 better than any benchmark.

### 3.3 Apps, external apps (.ppma), and the catalog

- **External apps** live on the SD card in `APPS/*.ppma` and **must match the running firmware version** ([Applications](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Applications)). How they're built ([External apps](https://github.com/portapack-mayhem/mayhem-firmware/wiki/External-apps-informations)):
  - They're linked at a fake `0xADxxxxxx` address range, then relocated by `export_external_apps.py`.
  - Their baseband image is appended, plus a 32-bit sum checksum.
  - They're loaded into RAM at launch, and LTO is disabled for them.
  - Motivation: flash ran out. Since v1.8.0 apps have been migrating to SD, formalized in v2.0.0. The [roadmap](https://github.com/portapack-mayhem/mayhem-firmware/wiki/External-App-Roadmap) notes that external apps get *less* free RAM, so memory-hungry apps stay internal.
- **Scale on `next` (2026-09-13, repo tree):** **96 external app directories**, **59 M4 baseband processor modules**, and ~39 internal app source files. v2.3.1 introduced a "standalone app API v3". v2.1.0 added an I²C device manager and an external module API used by the [ESP32 companion module](https://github.com/portapack-mayhem/mayhem-firmware/releases/tag/v2.2.0). **[unverified]** The ESP32 repo URL I tried returned 404; it appears to be a companion sensor/GPS/Wi-Fi module, not a port of Mayhem.
- **Firmware packaging:** a `.ppfw.tar` holds firmware plus all external apps. It can be flashed offline with the on-device [Flash Utility](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Flash-Utility), via `hackrf_spiflash` in HackRF mode, or through the web ([Update firmware](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Update-firmware)).

**App catalog** (wiki [Receivers](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Receivers), [Transmitters](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Transmitters), [Transceivers](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Transceivers), [Utilities](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Utilities)):

| Category | Apps |
|---|---|
| **Receivers / decoders** | Audio (AM/NFM/WFM/DSB/LSB/USB), Radio (broadcast), **ADS-B** (1090 MHz), **ACARS**, **AIS**, **APRS**, AFSK (Bell 202), **POCSAG**, **FLEX** pager, **ERT** utility meters, **TPMS**, **Weather** stations (26 protocols), **Radiosonde** (RS41, M10, M20, M2K2; map), **BLE RX**, NRF24L01, **SubGhzD** (45 OOK remote protocols incl. KeeLoq display), SubCar (car keyless entry), **ProtoView** (generic OOK visualizer), **EPIRB 406 MHz**, **SSTV RX**, **WeFax**, **NOAA APT**, Morse RX, RTTY RX, 2-Tone RX, **TETRA RX** (unencrypted control channel metadata only, no audio), **VOR** RX, Analog TV, FPV Detect (5.8 GHz), Fox Hunt, Level, Detector, Signal Hunter, Time Sink, gfxEQ |
| **Spectrum / search** | Looking Glass, Search, Scanner, Recon |
| **Capture / replay** | Capture (C8/C16), Replay, Remote (button panels of captures), IQ Trim, Playlist Editor |
| **Transmitters** | Mic TX / transceiver, APRS TX, POCSAG TX, FLEX TX, ADS-B TX, BLE TX / BLESpam, GPS Sim, Jammer, OOK / OOK Editor / OOK Brute, KeeLoq TX, Security+ TX, TPMS TX, SSTV TX, RDS, Morse/RTTY TX, SAME/EAS TX, EPIRB TX, MDC-1200 TX, **P25 TX** (Phase 1 frames; TX only), Spectrum Painter, Soundboard, FlipperTX (plays `.sub` files), TouchTunes, TEDI/LCR, Hopper, Signal Generator |
| **Transceivers** | Microphone transceiver (half-duplex, CTCSS, VOX), KISS TNC (AX.25 over USB) |
| **Utilities / games** | File manager, Freq manager, Notepad, Calculator, Antenna length, SD-over-USB, WardriveMap, Waterfall Designer, WAV viewer, Tetris, Doom, Pac-Man, etc. |

Colors in the menu mark maturity: green (complete), yellow (partial), orange (beta), red (destructive).

**[unverified]** Which of these are in stable v2.4.0 versus nightly only: TETRA RX, VOR, and Signal Hunter appear in the `next` tree and wiki, but I didn't confirm their stable release.

### 3.4 Spectrum and "exploration" apps and their limits

| App | How it works | Hard limits (from wiki) |
|---|---|---|
| **[Looking Glass](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Looking-Glass)** | Steps through a range in slices; each completed sweep adds one waterfall row. Modes: waterfall, live bars, peak bars. The marker jumps to the Audio app. | "For wide scan ranges the sweep time is long and the display may appear frozen — this is normal." Fast/slow mode trades accuracy. RES 2–128. No I/Q, no detection list. Jumping to Audio and back **resets Looking Glass settings.** |
| **[Search](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Search)** | FFT sweep in **2.5 MHz slices, 256 bins (~9.8 kHz/bin)**; locks when a bin exceeds mean + threshold; logs frequency, time, and duration to CSV; snap-to-grid option | **Max span 80 MHz (32 slices)**. Needs 500 ms above threshold to lock, releases after 600 ms. No demod while searching. RF amp always off. |
| **[Scanner](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Scanner)** | Freqman list or start/stop/step; squelch-based stop; AM/NFM/WFM | **~20 frequencies/s** (50 ms settle per frequency); only one range per file |
| **[Recon](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Recon)** | Reworked scanner driven by stats messages; continuous or sparse matching; auto-save hits; **auto-record on lock** (WAV, or C16 in SPEC mode); experimental store-and-forward "repeater" | Stats update every 100 ms, so **max ~10 frequencies/s** in AM/NFM/WFM; SPEC mode 5/s at 1.75–3 MHz and 3/s at 3–5 MHz; **max 115 list entries**; many options must be set in CONFIG first |
| **[Detector](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Detector)** | Cycles preset lists (TETRA uplink, LoRa, remotes) in 750 kHz windows, ~16 ms per step; beeps on threshold | Fixed presets |
| **[Signal Hunter](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Signal-Hunter)** | 2 Msps ÷8 → 250 kHz; 64-sample sliding energy window; **8 ms pre-trigger ring buffer**; writes C16 on trigger; single-frequency or hop mode | Energy-only trigger; no classification |

The pattern: **everything is energy/squelch-triggered on one narrow window at a time**, with retune/dwell measured in tens of milliseconds, and nothing identifies *what* a signal is.

### 3.5 Recording and replay formats

- **[Capture](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Capture)** writes **C16** (interleaved int16 little-endian I/Q) or **C8** (int8), plus a same-name `.TXT` with `center_frequency=`, `sample_rate=`, and optional GPS `latitude=`/`longitude=`/`satinuse=` ([C16 format](https://github.com/portapack-mayhem/mayhem-firmware/wiki/C16-format)).
- **Bandwidth is limited by SD write speed and M4 throughput.** Reliable capture for replay is **≤1 MHz** (500 kHz recommended, needing >2 MB/s writes for C16). 1.25 MHz is "at the limit" with M4 sample drops. 1.25–5.5 MHz records with **periodic sample drops**, fine for viewing a spectrum but not for replay.
- Captures are usable in GNU Radio (`IShort To Complex`, scale by 1/32768), Audacity, and inspectrum.

### 3.6 Mayhem Hub, web flasher, and USB shell

- **[Mayhem Hub](https://github.com/portapack-mayhem/MayhemHub) at [hackrf.app](https://hackrf.app/)** is a Chromium-only WebSerial PWA, introduced with v2.0.0. It offers:
  - live screen streaming and remote control
  - SD file manager
  - one-click stable/nightly/custom firmware update (stable ≥ v2.0.1)
  - serial console
  - `.ppsc` scripting
- **USB serial ChibiOS shell** ([USB Serial Console](https://github.com/portapack-mayhem/mayhem-firmware/wiki/USB-Serial-Console)). Available whenever the device isn't in HackRF mode. Commands include:
  - screen: `screenshot`, `screenframe`
  - input injection: `button`, `touch`, `keyboard`
  - accessibility tree: `accessibility_readall`
  - app control: `applist`, `appstart`
  - radio and sensors: `setfreq` (select RX apps), `radioinfo`, `gotgps`, `gotorientation`, `gotenv`
  - files: `ls`, `fopen`/`fread`/`fwrite`/`frb`/`fwb`
  - CPLD: `cpld_read`, `cpld_write`
  - pager: `sendpocsag`
  - maintenance: `sysinfo`, `reboot`, `dfu`, `hackrf`, `sd_over_usb`, `flash`

  This is a de facto **remote API and test harness**.

### 3.7 Community size

- GitHub: 5.4k stars, ~142 contributors. v2.4.0 alone credited **17 contributors** ([release](https://github.com/portapack-mayhem/mayhem-firmware/releases/tag/v2.4.0)). Most frequent recent authors in release notes: htotoo, zxkmm, gullradriel, RocketGod-git, NotherNgineer, iNetro.
- Discord: the Mayhem Hub page showed **10,731 members** when fetched (count may be stale). There's also an official Facebook group ([README](https://github.com/portapack-mayhem/mayhem-firmware)).
- There's commercial pull as well: OpenSourceSDRLab co-brands "Mayhem Signature Edition" hardware, contributed H4 gerbers, and ships a modified `Pro_v2.4.0` build ([OSSDRLab blog](https://blog.opensourcesdrlab.com/archives/hackrf-pro-h4m-pro)).

### 3.8 Recent developments 2024–2026

| Release | Highlights |
|---|---|
| **v2.0.0** (2024-02-16) | Apps moved to SD card; `.ppfw.tar` bundle; USB serial in PortaPack mode; hackrf.app; BLE apps; Recon raw auto-record/replay; new GitHub org |
| v2.0.1 / v2.0.2 (2024) | Fixes; web-update support |
| **v2.1.0** (2024-12-20) | I²C device manager, external module API, ProtoView pause, random-password app, geotagging |
| **v2.2.0** (2025-07-11) | App manager (hide apps), H4 gerbers from OpenSourceSDRLab, encoder options, signal-gen modulations, Flipper TX, more apps moved external |
| **v2.3.1** (2025-11-07) | **EPIRB 406 RX**, OpenStreetMap support, BLE RX/TX rework, RS41 improvements, standalone app API v3, multi-screen layouts (PortaRF), Search logging |
| **v2.3.2** (2025-12-21) | FLEX pager RX, **SSTV RX**, SubCar, sonde plus map |
| **v2.4.0** (2026-03-24) | **HackRF Pro ("PRALINE") support, flagged "work in progress"** (iCE40 bitstream loading, MAX2831 driver, software RSSI); Morse RX/TX, RTTY RX/TX, **FPV Detect**, Time Sink, EPIRB TX, SAME TX, MDC-1200 TX, **P25 TX**, TPMS TX, KeeLoq TX, KISS TNC, Waterfall Designer, ADS-B trails, ACARS improvements |
| Nightlies Aug–Sep 2026 | SD-over-USB for HackRF Pro, Superrollo (HCS361) decode, H4M Pro hardware files, AM spectrum zoom, BigFrequency widget, jammer fix |

---

## 4. Why the PortaPack is compute-constrained

### 4.1 The resource budget

| Resource | Budget (HackRF One + PortaPack) | Consequence |
|---|---|---|
| CPU | LPC4320: Cortex-M4F (up to 204 MHz) + Cortex-M0 ([NXP LPC43xx](https://www.nxp.com/products/LPC4320FBD144)) | At 20 Msps the M4 gets **~10 instruction cycles per sample** (Mayhem source comment), so real DSP must run at ≤3 Msps after aggressive decimation |
| RAM | ~200 kB total: 96 kB M4 data, 32 kB M4 code, 64 kB M0 data, 8 kB shared | FFTs are 256-bin in Search; **windowing removed "due to lack of available code RAM"**; Recon capped at 115 list entries; external apps must fit in RAM |
| Flash | 1 MB SPI (4 MB on HackRF Pro) | Apps pushed to SD; firmware/app version lock-step |
| Storage I/O | microSD over SDIO from the MCU | Reliable I/Q capture ≤1 MHz BW |
| ADC | 8-bit, ~6 effective bits in practice | ~40–50 dB instantaneous dynamic range; strong signals block weak ones |
| Front end | One 20 MHz (max) zero-IF window; no preselector; half-duplex | Can observe only one slice at a time; overload from broadcast/pagers |
| Display / UI | 240×320 or 320×240, resistive touch, M0-rendered | Dense config screens; UI thread coupled to radio events |

### 4.2 Why wideband scanning is slow on-device

- Every app retunes the synthesizer and waits for statistics from the M4. Scanner uses 50 ms per frequency and Recon ≥100 ms. Looking Glass accumulates many retunes per waterfall row. For a 1 GHz span at 20 MHz per slice, that's ≥50 retunes per row, and the display "may appear frozen".
- Unlike `hackrf_sweep` on a PC, there's no host with FFTW and GBs of RAM. The M4 must FFT, integrate, and message the M0 at each step within its tiny RAM, and the M0 must render.
- **Probability of intercept is poor.** A 1 GHz Looking Glass sweep revisits each frequency only every few seconds at best, so bursty emitters (key fobs, TPMS, telemetry, PTT voice) are mostly missed unless you already know where to park.

### 4.3 Why automatic classification is infeasible on it

Automatic modulation recognition, even classical feature-based rather than neural, needs:
- a spectrum per candidate
- occupied bandwidth and symbol-rate estimates (cyclostationary or envelope features)
- often several seconds of I/Q per candidate

That's millions of multiply-adds per second and megabytes of working memory. The LPC4320 has neither. Digital voice needs a 4FSK/C4FM or TDMA demodulator, an FEC/framing decoder, a vocoder (AMBE+2 / IMBE), and a trunking state machine following a control channel while voice hops. Mayhem maintainers have closed every such request (#225, #360, #2507, #2516). Their answer in 2025: *"it's not possible using the portapack. The hardware is too low. Use a PC… and portapack in hackrf mode"* ([#2516](https://github.com/portapack-mayhem/mayhem-firmware/issues/2516)).

*Caveat:* the longer maintainer reply in [#2507](https://github.com/portapack-mayhem/mayhem-firmware/issues/2507) wrongly calls the MCU an "STM32F4", so don't cite it for specifics. The conclusion is still consistent with the budget above. Vocoder licensing (AMBE/IMBE are proprietary DVSI codecs) is a further, non-compute barrier. *(My note.)*

### 4.4 HackRF Pro doesn't fix this for PortaPack users

Mayhem's Pro support is WIP. The Pro's FPGA can offload DC removal and decimation, and the LPC4330 (if confirmed) adds SRAM. But the M4-plus-SD-card handheld architecture is unchanged. The Pro mainly improves the **RF head**: TCXO, NF, no DC spike, 16-bit narrowband mode, and 7.1 GHz tuning.

---

## 5. Known pain points reported by users

| Pain point | Evidence |
|---|---|
| **Scanner/Recon UX and stability** | UI lockups when changing modulation while scanning ([#1432](https://github.com/portapack-mayhem/mayhem-firmware/issues/1432)); long-running enhancement requests ([#986](https://github.com/portapack-mayhem/mayhem-firmware/issues/986)); audio only on first hit ([#462](https://github.com/eried/portapack-mayhem/issues/462), older); truncated frequency display ([#1360](https://github.com/eried/portapack-mayhem/issues/1360)); the Recon wiki notes "A lot are complaining that the search is not starting by itself" because of CONFIG defaults |
| **Slow wideband views** | Looking Glass "may appear frozen" on wide spans; Search capped at 80 MHz ([wiki](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Looking-Glass)) |
| **No automatic modulation detection / identification** | No app does this; all detection is squelch/energy based (§3.4) |
| **No DMR/P25/NXDN/TETRA voice; no trunk tracking** | Issues [#225](https://github.com/portapack-mayhem/mayhem-firmware/issues/225), [#2507](https://github.com/portapack-mayhem/mayhem-firmware/issues/2507), [#2516](https://github.com/portapack-mayhem/mayhem-firmware/issues/2516) closed. Users fall back to SDRTrunk/OP25 on a PC with the HackRF in USB mode ([RadioReference: Trunk Tracking with HackRF](https://forums.radioreference.com/threads/trunk-tracking-with-hackrf.347524/)). TETRA RX shows control-channel metadata only. |
| **Sensitivity and overload** | "Despite the HackRF's modest sensitivity…" (Looking Glass wiki); FM broadcast and pager overload; no RF filtering; easy to kill the RF amp above −5 dBm ([Receive Quality Issues](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Help!-Im-not-receiving-anything!---Receive-Quality-Issues), [Preamp replacement](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Preamplifier-IC-replacement)) |
| **Self-generated noise** | H2 charger noise when USB is connected; power banks; USB hubs ([same wiki](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Help!-Im-not-receiving-anything!---Receive-Quality-Issues)) |
| **Frequency accuracy** | "Most clone HackRF/PortaPack units have a small frequency offset", corrected via a PPB setting; the Tuner app helps calibrate. HackRF One has no TCXO; the PortaPack's TCXO fixes this only if populated. |
| **Capture bandwidth** | ≤1 MHz reliable; drops above 1.25 MHz; depends on SD card ([Capture wiki](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Capture)) |
| **Clone hardware lottery** | Codec, amp, and CPLD swaps; H3 closed firmware; the README warns to "*ask* the seller about compatibility" |
| **Update friction** | External apps break if firmware and SD contents mismatch; Ubuntu's packaged hackrf tools too old for r9 ([Update firmware](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Update-firmware)) |
| **Half-duplex** | Can't listen while transmitting; Recon's "repeater" is store-and-forward |
| **CB / trunk capture** | **[unverified]** I couldn't gather specific Reddit or forum threads about CB-band and trunked-system capture before the search budget ran out. The limits above (≤1 MHz capture, single window, no trunk following) imply the pain: you can't record an entire 40-channel CB band (~440 kHz, so actually capturable at 500 kHz) *and* demodulate simultaneously. Trunked systems span more than 1 MHz and hop, so they're impossible to follow on-device. |

---

## 6. Adjacent and competing handhelds (concise)

| Device | What it is | RF envelope | Relevance to us |
|---|---|---|---|
| **Flipper Zero** | Multi-tool with a **CC1101** packet transceiver | 300–348, 387–464, 779–928 MHz ([docs](https://docs.flipper.net/zero/sub-ghz)) | Excellent sub-GHz *protocol* UX (record, decode, replay `.sub`) but not an SDR; no spectrum exploration. Mayhem imports `.sub` files (FlipperTX). |
| **PortaRF** (OpenSourceSDRLab, 2026) | HackRF One + H4M on one board, 4" IPS, ESP32-S3 voice add-on | Same as HackRF One | Shows vendors iterating on form factor, not compute ([CNX](https://www.cnx-software.com/2026/05/14/portarf-single-board-sdr-mixes-hackrf-one-and-portapack-h4m-hardware-adds-ai-voice-control/)) |
| **HackRF/SDR + Raspberry Pi builds** | DIY cyberdecks: Pi 5 + RTL-SDR + SDRTrunk as an affordable P25 scanner ([Hackaday](https://hackaday.com/2024/02/10/pi-5-and-sdr-team-up-for-a-digital-scanner-you-can-actually-afford/), [rtl-sdr.com](https://www.rtl-sdr.com/a-low-cost-p25-police-scanner-with-rtl-sdr-raspberry-pi-5-and-sdrtrunk/)); Pi 5 + SDRplay + PiHPSDR ([TechMinds](https://www.rtl-sdr.com/techminds-building-a-diy-standalone-sdr-with-a-raspberry-pi-5-inch-touchscreen-sdrplay-rspdx-and-the-pihpsdr-software/)); Pi 5 + 10" touch + HackRF/Pluto ([RadioMenace](https://radiomenace.com/ultimate-raspberry-pi-5-touchscreen-sdr-mobile-rig/)) | Front-end dependent | Proves Pi 5-class compute runs trunking and desktop SDR apps. These builds are desktop UIs on small screens, not a purpose-built exploration UX. |
| **Malahit DSP2** | Standalone DSP receiver | 10 kHz–380 MHz and 404 MHz–2 GHz; panorama 48/96/192 kHz; claimed 82 dB dynamic range, 0.3 µV sensitivity ([manual](https://malahiteam.com/wp-content/uploads/2021/11/manual_malahiteam_en.pdf), [Radioddity](https://www.radioddity.com/products/raddy-dsp2)) | Good *listening* ergonomics and dynamic range; tiny panorama; RX-only; closed firmware |
| **Deepelec DeepSDR 101** | Aluminum handheld SDR, 4.3" 800×480 IPS | 100 kHz–149 MHz, 16-bit, 192 kHz span, ZIF with Si5351 quadrature LO ([manual](https://deepelec.com/files/sdr101/DeepSDR_101_Product_Manual_V1.1_EN.pdf)) | HF-centric; shows the premium handheld build quality and screen users expect |
| **Uniden SDS100 / SDS200** | Digital scanners with "True I/Q" receivers | 25–512, 758–824, 849–869, 895–960, 1240–1300 MHz; P25 Phase 1/2, X2-TDMA, Motorola, EDACS, LTR; HomePatrol/RadioReference database; Close Call ([SDS100](https://uniden.com/products/sds100), [SDS200](https://uniden.com/products/sds200)) | **UX gold standard for "what is on the air near me"**: location-based system database, auto trunk following, Close Call nearby-transmitter capture. **[unverified]** DMR/NXDN are paid upgrades. |
| **Signal Hound BB60D** | USB 3.0 real-time spectrum analyzer | Up to 6 GHz, 27 MHz instantaneous BW, **24 GHz/s sweep**, 140 MB/s streaming; $4,950–5,450 ([Signal Hound](https://signalhound.com/products/bb60d/)) | Reference for professional sweep and real-time analysis: ~3× HackRF sweep speed with calibrated amplitude and far better dynamic range; not handheld |

---

## 7. Takeaways for our project

### 7.1 Worth keeping from PortaPack/Mayhem

1. **The app catalog is a ready-made decoder backlog and test matrix.** ADS-B, AIS, ACARS, APRS/AX.25, POCSAG/FLEX, ERT, TPMS, weather sensors (26 protocols), radiosondes, SubGhzD's 45 OOK protocols, BLE advertising, EPIRB, SSTV, WeFax, NOAA APT, and TETRA control-channel parsing. Mayhem's C++ implementations are GPL, so check licensing before reuse. Most of these also exist in rtl_433, dump1090, and multimon-ng.
2. **Workflow primitives users like:**
   - Looking Glass marker → one press into a demodulator.
   - Search's hit log (frequency, time, duration) with snap-to-channel-grid.
   - Recon's auto-save of hits and auto-record-on-lock.
   - Signal Hunter's **pre-trigger ring buffer**.
   - Remote's one-touch replay panels.
   - Freqman text files for presets.

   An exploration-first device should unify these into one pipeline: *survey → detect → auto-capture with pre-trigger → classify → hand off to decoder → log*.
3. **Simple, portable file formats:** C8/C16 plus a `.TXT` sidecar (center frequency, sample rate, GPS). Keep that simplicity, but consider SigMF for interoperability.
4. **A remote-control shell and web hub.** The USB shell (`appstart`, `setfreq`, `screenshot`, `button`/`touch`, `accessibility_readall`) and hackrf.app WebSerial hub make the device scriptable and testable. We should design an API-first control plane from day one.
5. **External app packaging** decouples the core from the app catalog. We should avoid the strict version lock-step that bites Mayhem users.
6. **Low-power DSP tricks** that matter if we add an FPGA or want long battery life:
   - fs/4 frequency translation (no multipliers)
   - cascaded CIC/half-band decimation to ~3 Msps before anything else
   - fixed-point FIRs
   - 10 Hz statistics cadence for UI
   - DSP next to the ADC (the Pro gateware already implements DC block, fs/4 shift, and CIC in Amaranth HDL)
7. **Power budget reference:** ≈2 W for a working PortaPack. A Pi 5 or Orin Nano will be 3–15 W, so battery and thermals become a first-class design problem.

### 7.2 Fundamentally limiting (don't inherit)

- **MCU-hosted DSP** (~10 cycles/sample at 20 Msps, ~200 kB RAM). No path to classification, digital voice, trunking, or multi-channel monitoring.
- **One narrow, energy-triggered window with 50–100 ms retune/dwell** as the only exploration model.
- **SD card as the I/Q sink** (≤1 MHz reliable).
- **8-bit ADC with no preselection.** Overload in urban RF is the #1 real-world receive complaint.
- **Clone-hardware variability** and a UI built for 240×320 resistive touch.

### 7.3 HackRF as a front end for Pi 5 / Orin Nano (± FPGA)

What it **can** support:

| Capability | Feasibility | Notes |
|---|---|---|
| **Real-time 20 MHz window** | Yes | ~40 MB/s over USB 2.0; usable ~15–18 MHz after filter skirts (and the DC spike on One). Pi 5 or Orin should handle continuous FFT (e.g. 4096-bin FFTs at ~4.9k/s), a polyphase channelizer, and several simultaneous narrowband demods within that window. *(Engineering estimate, not benchmarked.)* |
| **Sweep-based wideband survey** | Yes | Use the firmware sweep mode (libhackrf `hackrf_init_sweep`), ~8 GHz/s advertised, so 0–6 GHz occupancy about once per second. Opera Cake frequency mode can switch antennas/filters automatically per band. Best for **persistent-emitter mapping and change detection**, not burst capture. |
| **Hybrid "survey then park"** | Yes, and the right model | Sweep to build an occupancy map, then dwell a 20 MHz window on active regions with a pre-trigger ring buffer, capture, classify on GPU/CPU, and route to decoders (dump1090, rtl_433-style, SDRTrunk/OP25, DSD-FME). |
| **Trunked P25/DMR** | Yes, on host | SDRTrunk/OP25 already support HackRF; a 20 MHz window covers many (not all) system footprints. **[unverified]** Typical system spans vary widely; some 700/800 MHz systems exceed 20 MHz. |
| **Better narrowband dynamic range** | Pro only | 16-bit extended precision (ENOB 9–11) at ≥16× decimation, i.e. ≤2.5 Msps from a 40 Msps internal rate. *(Derived.)* Good for weak-signal dwell, not survey. |
| **Timing/sync, multiple radios** | Partial | CLKIN/CLKOUT 10 MHz shared reference; Pro adds trigger in/out. Two HackRFs give two independent 20 MHz windows (e.g. a survey radio plus a dwell radio), at the cost of USB bandwidth, power, and size. |

What it **can't** support:
- More than 20 MHz of contiguous 8-bit bandwidth. The Pro's 40 Msps is only available at 4 bits.
- True real-time wideband (no gaps) monitoring beyond one window.
- High instantaneous dynamic range in dense RF: 8-bit, no preselector. External filters or Opera Cake filter banks help.
- Full duplex, phase-coherent MIMO or DF arrays (beyond time-switched pseudo-Doppler), or calibrated amplitude measurements.
- USB 3-class throughput. If the concept needs ≥40–60 MHz real-time spans or 12–16-bit survey data, the RF head should be a different SDR (covered in other docs in this series).

**Bottom line:** HackRF One or Pro is a reasonable, cheap, well-supported *RF head* for a first prototype. Keep it for its 1 MHz–6 GHz tuning, TX capability, sweep mode, Opera Cake, and huge software support. **Replace the PortaPack's compute, storage, and UI entirely.** Build exploration around a sweep + dwell + auto-capture + classify pipeline on the SBC/GPU. Treat the 8-bit, 20 MHz, USB 2.0 envelope as the constraint to design around or eventually move beyond.

---

## Sources

**Great Scott Gadgets / HackRF**
- HackRF releases: https://github.com/greatscottgadgets/hackrf/releases
- HackRF repo and commit history (GitHub API): https://github.com/greatscottgadgets/hackrf
- HackRF One docs: https://hackrf.readthedocs.io/en/latest/hackrf_one.html
- HackRF Pro docs: https://hackrf.readthedocs.io/en/latest/hackrf_pro.html
- Hardware components: https://hackrf.readthedocs.io/en/latest/hardware_components.html
- Hardware revisions: https://hackrf.readthedocs.io/en/latest/list_of_hardware_revisions.html
- Sampling rate and baseband filters: https://github.com/greatscottgadgets/hackrf/blob/main/docs/source/sampling_rate.rst
- External clock interface: https://hackrf.readthedocs.io/en/latest/external_clock_interface.html
- Gateware (HackRF Pro FPGA): https://github.com/greatscottgadgets/hackrf/blob/main/docs/source/gateware.rst
- HackRF tools / hackrf_sweep: https://github.com/greatscottgadgets/hackrf/blob/main/docs/source/hackrf_tools.rst
- hackrf_sweep.c source: https://github.com/greatscottgadgets/hackrf/blob/main/host/hackrf-tools/src/hackrf_sweep.c
- HackRF Pro block diagram: https://github.com/greatscottgadgets/hackrf/blob/main/docs/images/block-diagram-pro.svg
- Opera Cake: https://hackrf.readthedocs.io/en/latest/opera_cake.html and https://github.com/greatscottgadgets/hackrf/blob/main/docs/source/opera_cake_modes_of_operation.rst
- Meet HackRF Pro (2025-06-26): https://www.greatscottgadgets.com/2025/06-26-meet-hackrf-pro/
- HackRF Pro Q+A: https://greatscottgadgets.com/2025/06-27-hackrf-pro-q-a/
- HackRF Pro product page: https://greatscottgadgets.com/hackrf/pro/
- HackRF Pro production timeline update (Sept 2025): https://www.greatscottgadgets.com/2025/09-05-hackrf-pro-production-timeline-update/
- HackRF Pro receive sensitivity and noise figure (Dec 2025): https://greatscottgadgets.com/2025/12-03-hackrf-pro-receive-sensitivity-and-noise-figure/
- HackRF Pro shipping update (Jan 2026): https://greatscottgadgets.com/2026/01-16-hackrf-pro-shipping-update/
- GSG HackRF blog index: https://greatscottgadgets.com/tags/hackrf/
- HackRF One product page: https://greatscottgadgets.com/hackrf/one/

**Mayhem / PortaPack**
- Mayhem repo and README: https://github.com/portapack-mayhem/mayhem-firmware
- Mayhem releases: https://github.com/portapack-mayhem/mayhem-firmware/releases (v2.4.0: https://github.com/portapack-mayhem/mayhem-firmware/releases/tag/v2.4.0; v2.0.0: https://github.com/portapack-mayhem/mayhem-firmware/releases/tag/v2.0.0)
- Wiki pages: [Firmware Architecture](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Firmware-Architecture), [External apps](https://github.com/portapack-mayhem/mayhem-firmware/wiki/External-apps-informations), [External App Roadmap](https://github.com/portapack-mayhem/mayhem-firmware/wiki/External-App-Roadmap), [Applications](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Applications), [Receivers](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Receivers), [Transmitters](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Transmitters), [Transceivers](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Transceivers), [Utilities](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Utilities), [Looking Glass](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Looking-Glass), [Search](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Search), [Scanner](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Scanner), [Recon](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Recon), [Detector](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Detector), [Signal Hunter](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Signal-Hunter), [Tetra RX](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Tetra-RX), [SubGhzD](https://github.com/portapack-mayhem/mayhem-firmware/wiki/SubGhzD), [Remote](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Remote), [Capture](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Capture), [C16 format](https://github.com/portapack-mayhem/mayhem-firmware/wiki/C16-format), [USB Serial Console](https://github.com/portapack-mayhem/mayhem-firmware/wiki/USB-Serial-Console), [Update firmware](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Update-firmware), [Flash Utility](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Flash-Utility), [HackRF Pro](https://github.com/portapack-mayhem/mayhem-firmware/wiki/HackRF-Pro), [PortaPack Versions](https://github.com/portapack-mayhem/mayhem-firmware/wiki/PortaPack-Versions), [Differences H1/H2](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Differences-Between-H1-and-H2-models), [Hardware overview](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Hardware-overview), [Receive Quality Issues](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Help!-Im-not-receiving-anything!---Receive-Quality-Issues), [Description of the Structure](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Description-of-the-Structure), [Create a Simple App](https://github.com/portapack-mayhem/mayhem-firmware/wiki/Create-a-Simple-App)
- Mayhem source (`next`): `firmware/baseband/proc_nfm_audio.hpp`, `firmware/baseband/proc_wideband_spectrum.cpp`, `firmware/application/external/`
- Issues: [#225](https://github.com/portapack-mayhem/mayhem-firmware/issues/225), [#2507](https://github.com/portapack-mayhem/mayhem-firmware/issues/2507), [#2516](https://github.com/portapack-mayhem/mayhem-firmware/issues/2516), [#1432](https://github.com/portapack-mayhem/mayhem-firmware/issues/1432), [#986](https://github.com/portapack-mayhem/mayhem-firmware/issues/986), [#462](https://github.com/eried/portapack-mayhem/issues/462), [#1360](https://github.com/eried/portapack-mayhem/issues/1360)
- Mayhem Hub: https://github.com/portapack-mayhem/MayhemHub and https://hackrf.app/
- ShareBrained PortaPack firmware: https://github.com/sharebrained/portapack-hackrf
- Havoc: https://github.com/furrtek/portapack-havoc
- OpenSourceSDRLab HackRF Pro + H4M Pro: https://opensourcesdrlab.com/products/hackrf-pro-h4m-pro and blog https://blog.opensourcesdrlab.com/archives/hackrf-pro-h4m-pro
- Lab401 H4M: https://lab401.com/collections/all-products-no-flipper/products/portapack-h4m
- rtl-sdr.com H4M review: https://www.rtl-sdr.com/a-review-of-the-new-hackrf-portapack-h4m/
- CNX Software, PortaRF (2026-05-14): https://www.cnx-software.com/2026/05/14/portarf-single-board-sdr-mixes-hackrf-one-and-portapack-h4m-hardware-adds-ai-voice-control/

**Performance and third-party**
- rtl-sdr.com, 8 GHz/s sweep (Feb 2017): https://www.rtl-sdr.com/scanning-spectrum-8ghz-per-second-new-hackrf-update/
- rtl-sdr.com HackRF initial review: https://www.rtl-sdr.com/hackrf-initial-review/
- rtl-sdr.com Airspy vs SDRplay vs HackRF: https://www.rtl-sdr.com/review-airspy-vs-sdrplay-rsp-vs-hackrf/
- RadioReference, Trunk Tracking with HackRF: https://forums.radioreference.com/threads/trunk-tracking-with-hackrf.347524/
- NXP LPC4320: https://www.nxp.com/products/LPC4320FBD144
- MAX5864 / MAX2839 (Analog Devices; datasheets not re-fetched this session): https://www.analog.com/en/products/max5864.html, https://www.analog.com/en/products/max2839.html

**Adjacent devices**
- Flipper Zero Sub-GHz: https://docs.flipper.net/zero/sub-ghz
- Malahit DSP2 manual: https://malahiteam.com/wp-content/uploads/2021/11/manual_malahiteam_en.pdf; Radioddity: https://www.radioddity.com/products/raddy-dsp2
- DeepSDR 101 manual: https://deepelec.com/files/sdr101/DeepSDR_101_Product_Manual_V1.1_EN.pdf
- Uniden SDS100: https://uniden.com/products/sds100; SDS200: https://uniden.com/products/sds200
- Signal Hound BB60D: https://signalhound.com/products/bb60d/
- Hackaday, Pi 5 digital scanner: https://hackaday.com/2024/02/10/pi-5-and-sdr-team-up-for-a-digital-scanner-you-can-actually-afford/
- rtl-sdr.com, Pi 5 P25 scanner: https://www.rtl-sdr.com/a-low-cost-p25-police-scanner-with-rtl-sdr-raspberry-pi-5-and-sdrtrunk/
- TechMinds Pi 5 standalone SDR: https://www.rtl-sdr.com/techminds-building-a-diy-standalone-sdr-with-a-raspberry-pi-5-inch-touchscreen-sdrplay-rspdx-and-the-pihpsdr-software/
- RadioMenace Pi 5 SDR rig: https://radiomenace.com/ultimate-raspberry-pi-5-touchscreen-sdr-mobile-rig/
