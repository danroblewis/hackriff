# 04 — Radio Engineering & Signals Analysis Foundations for an Exploration-First SDR

> Scope: the radio-engineering and signal-processing techniques an "exploration-first" SDR must implement to (1) find interesting frequencies automatically, (2) detect and characterize signals, (3) infer modulation and parameters without user input, (4) identify protocols, and (5) record and decode. The spectrum survey is US-focused with international notes. Math is kept short and aimed at implementation. Research date: 2026-09-13.

---

## Table of contents

1. [Radio spectrum basics for exploration](#1-radio-spectrum-basics-for-exploration)
2. [What makes a frequency "interesting": a feature taxonomy](#2-what-makes-a-frequency-interesting-a-feature-taxonomy)
3. [Spectrum sensing and detection](#3-spectrum-sensing-and-detection)
4. [Signal characterization and parameter estimation](#4-signal-characterization-and-parameter-estimation)
5. [Automatic Modulation Classification (AMC)](#5-automatic-modulation-classification-amc)
6. [Automatic analog demodulation selection, squelch, AGC](#6-automatic-analog-demodulation-selection-squelch-agc)
7. [Digital protocol identification and decoding pipeline](#7-digital-protocol-identification-and-decoding-pipeline)
8. [Trunked radio systems](#8-trunked-radio-systems)
9. [Direction finding and geolocation](#9-direction-finding-and-geolocation)
10. [Measurement and calibration](#10-measurement-and-calibration)
11. [Professional spectrum-monitoring workflow and how it maps to a hobbyist tool](#11-professional-spectrum-monitoring-workflow-and-how-it-maps-to-a-hobbyist-tool)
12. [Takeaways: prioritized implementation list](#12-takeaways-prioritized-implementation-list)
13. [Sources / References](#13-sources--references)

---

## 1. Radio spectrum basics for exploration

### 1.1 Allocation vs. assignment vs. actual use

Three layers of "what should be here" apply to any frequency, and each can serve as a classification prior:

| Layer | Authority / data | Granularity | Usefulness as prior |
|---|---|---|---|
| **Allocation** (which *services* may use a band) | ITU Radio Regulations Art. 5 (three ITU Regions; the Americas are Region 2); US: [47 CFR §2.106 Table of Frequency Allocations](https://www.ecfr.gov/current/title-47/chapter-I/subchapter-A/part-2/subpart-B/section-2.106), [NTIA US Frequency Allocation Chart](https://www.ntia.gov/page/united-states-frequency-allocation-chart) (updated Sept 2025) | Band edges, services, footnotes | Coarse: "land mobile", "aeronautical radionavigation", "ISM" |
| **Band plan / channelization** | FCC rule parts (Part 90 LMR, Part 95 personal radio, Part 97 amateur, Part 22/24/27 cellular), industry plans (ARRL band plans, 3GPP band tables) | Channel raster, bandwidth, emission types | Medium: channel spacing (6.25/12.5/25 kHz), expected modulation |
| **Assignment / license** | [FCC ULS public access files](https://www.fcc.gov/wireless/data/public-access-files-database-downloads) (weekly full + daily transactions), FCC LMS (broadcast), [RadioReference DB](https://www.radioreference.com/db/) | Exact frequency, location, licensee, **emission designator** | Fine: "within 30 km there is a licensed 851.2375 MHz P25 site" |
| **Observed use** | Your own history, community catalogs ([sigidwiki](https://www.sigidwiki.com/wiki/Signal_Identification_Guide), [Artemis offline DB](https://aresvalley.github.io/Artemis/database/sigid/)) | Arbitrary | Strongest when local |

**ITU emission designators** in ULS records are especially valuable because they encode necessary bandwidth and modulation class. Examples: `11K2F3E` = 11.2 kHz FM analog voice (narrowband), `16K0F3E` = 16 kHz FM voice (wideband legacy), `8K10F1E` = P25 Phase 1 C4FM voice, `7K60FXE`/`7K60FXD` = DMR, `4K00F1E` = NXDN 6.25 kHz. Per [ITU-R SM.328](https://www.itu.int/dms_pubrec/itu-r/rec/sm/R-REC-SM.328-12-202509-I!!PDF-E.pdf) and NTIA Redbook Annex J ([necessary bandwidth guidance](https://www.ntia.gov/sites/default/files/2023-11/j_2021_edition_rev_2023.pdf)), the first four characters give the bandwidth, the fifth the main-carrier modulation (A = AM DSB, J = SSB suppressed carrier, F = FM, G = PM, P/K/L/M = pulse, W = combination), the sixth the modulating signal nature, and the seventh the information type.

**Machine-readable access paths**

- **eCFR API** serves 47 CFR Part 2 (including §2.106) as XML/JSON. The FCC's own Online Table is PDF-first, so plan on parsing it once and shipping a compact band table.
- **ULS public access files**: pipe-delimited `.dat` tables inside zip archives (e.g., `HD` header, `EN` entity, `LO` location, `FR` frequency, `EM` emission). They are updated weekly (full) and daily (transactions). Community parsers exist (e.g., [gdubin/uls](https://github.com/gdubin/uls)). For offline priors, build a spatial index (lat/lon to frequency/emission/licensee).
- **RadioReference**: a SOAP API ([WSDL](https://api.radioreference.com/soap2/?wsdl&v=latest)). It is free for approved developers, but **each end user needs an active RR Premium subscription** ([RR support](https://support.radioreference.com/hc/en-us/articles/18844460198932-Database-Web-Service-API)). It has the best trunked-system and talkgroup metadata.
- **sigidwiki / Artemis**: a SQLite database with waterfall images and audio samples for several hundred signal types. It is good for UI "looks like" suggestions and weak priors on bandwidth and modulation.
- **International**: CEPT ERC Report 25 (European Common Allocation Table) and the EFIS database; Ofcom's Wireless Telegraphy Register (UK); ISED's Spectrum Management System (Canada).

**Using priors in classification.** Treat the classifier output as a likelihood and combine it with a location/frequency prior:

$$
P(c \mid \mathbf{x}, f, \ell) \;\propto\; p(\mathbf{x}\mid c)\; P(c \mid f, \ell), \qquad
P(c\mid f,\ell) = \lambda_1 P_{\text{alloc}} + \lambda_2 P_{\text{license}} + \lambda_3 P_{\text{history}} + \lambda_0 P_{\text{uniform}}
$$

Keep $\lambda_0 > 0$ so that non-compliant or unknown emitters are never assigned zero probability. The exploration tool exists precisely to surface those.

### 1.2 What lives where (HF to ~6 GHz, US focus)

The table is condensed and does not replace §2.106. "Mod" is the typical modulation of the dominant signals.

| Band / frequency | Service / signals | Typical mod & bandwidth | Notes for automated ID |
|---|---|---|---|
| 0.53–1.7 MHz | AM broadcast | DSB-AM, ~10–20 kHz (10 kHz raster US; 9 kHz in Regions 1/3) | HD Radio hybrid adds OFDM sidebands |
| 2.5/5/10/15/20 MHz | WWV/WWVH time and frequency | AM + BCD time code | **Frequency calibration reference** |
| 3–30 MHz | Shortwave broadcast (AM, DRM), amateur HF, aero/marine HF (USB voice, HFDL), military, OTH radar | AM, SSB (~2.4–3 kHz), CW, many FSK/PSK data modes | SSB convention: LSB below 10 MHz, USB above (amateur); aero/marine HF uses USB |
| 26.965–27.405 MHz | CB (40 ch) | AM, SSB | License by rule |
| 30–50 MHz | Low-band LMR (legacy public safety, highway), 6 m amateur (50–54) | NBFM | |
| 54–88, 174–216, 470–608 MHz | TV broadcast | ATSC 1.0 8-VSB 6 MHz (pilot 309.44 kHz above lower edge); ATSC 3.0 OFDM | The ATSC pilot is a good calibration tone |
| 88–108 MHz | FM broadcast | WFM ±75 kHz, 200 kHz raster; 19 kHz stereo pilot; RDS 57 kHz; HD Radio sidebands | Very reliable; pilot at 19 kHz ±2 Hz |
| 108–117.975 MHz | VOR / ILS localizer | AM with 30 Hz / 9960 Hz subcarriers; Morse ident | |
| 118–136.975 MHz | Aviation voice | **AM** (25 kHz US, 8.33 kHz raster in Europe) | Aviation stays AM: an AM prior in this band |
| 129–131.55 MHz (several) | ACARS | 2400 bps MSK on AM carrier | |
| 136.650–136.975 MHz | VDL Mode 2 | D8PSK, 31.5 kbps, 25 kHz | |
| 137–138 MHz | Weather sats: Meteor-M N2-3/N2-4 LRPT on 137.9 (137.1 backup) | QPSK ~72 ksym/s, ~120 kHz occupied | **NOAA-15/18/19 APT ended in 2025** ([NOAA NESDIS](https://www.nesdis.noaa.gov/news/legacy-orbit-noaa-decommissions-the-poes-satellite-constellation)): NOAA-18 decommissioned Jun 6, NOAA-19 Aug 13, NOAA-15 Aug 19 |
| 144–148 MHz | 2 m amateur; APRS 144.390 (US) | NBFM voice; AFSK 1200 (Bell 202, AX.25); D-STAR/DMR/C4FM(YSF) | |
| 150–174 MHz | VHF LMR: public safety, business, rail | NBFM 12.5 kHz (narrowbanding mandated 2013), P25, DMR, NXDN | MURS 151.82–154.60; VHF paging (POCSAG/FLEX) |
| 156–162.025 MHz | Marine VHF (Ch16 156.800); **AIS 161.975 / 162.025** | NBFM voice; AIS GMSK 9600 bps, HDLC/NRZI, SOTDMA | |
| 162.400–162.550 MHz | NOAA Weather Radio | NBFM + SAME AFSK 520.83 bps | |
| 216–225 MHz | 1.25 m amateur (222–225) | | |
| 225–400 MHz | Military UHF air (AM), UHF MILSATCOM | AM, various | |
| 260–470 MHz (Part 15 §15.231) | Key fobs, garage doors, TPMS (**315 MHz US**, 433.92 EU), sensors | OOK/ASK/FSK, Manchester/PWM | **rtl_433 territory** |
| 400.15–406 MHz | Radiosondes (Vaisala RS41 ~403 MHz; NWS moved from 1680 MHz LMS6 to 403 MHz RS41 at Autosonde sites) | GFSK 4800 bps | Twice daily launches (00Z/12Z): time-of-day prior |
| 406–406.1 MHz | COSPAS-SARSAT distress beacons | BPSK 400 bps bursts | Distress: legal to receive |
| 420–450 MHz | 70 cm amateur (US gov radiolocation primary); 433.05–434.79 ISM (Region 1) | | Many 433.92 devices in the US under §15.231 |
| 450–470 MHz | UHF LMR; FRS/GMRS 462/467 MHz | NBFM, P25, DMR, NXDN | |
| 470–512 MHz | T-band LMR (major metro areas) | | |
| 617–652 / 663–698 MHz | LTE Band 71 / NR n71 | OFDM 5–20 MHz | |
| 698–806 MHz | 700 MHz LTE (B12/13/14/17; **B14 FirstNet** 758–768/788–798); public safety narrowband 769–775/799–805 | OFDM; P25 | |
| 806–824 / 851–869 MHz | 800 MHz public safety / SMR **trunking** (P25, Motorola SmartNet/SmartZone, legacy EDACS) | P25 C4FM/LSM, analog FM | Control channels transmit continuously: 100% duty cycle |
| 824–849 / 869–894 MHz | Cellular 850 (B5/n5) | LTE/NR | |
| 896–901 / 935–940 MHz | 900 MHz LMR, partly realigned for 3×3 MHz private LTE | | |
| **902–928 MHz** | US ISM/Part 15: LoRaWAN US915 (uplink 902.3–914.9, downlink 923.3–927.5), Itron ERT/AMR meters, Z-Wave (908.42), UHF RFID, 802.11ah, 33 cm amateur | CSS (LoRa 125/500 kHz), FSK, OOK, FHSS | Very busy; hopping and bursts |
| 929–932 MHz | Paging | POCSAG (512/1200/2400 bps, ±4.5 kHz FSK), FLEX (1600/3200/6400 bps, 2/4-FSK) | See legal note on common-carrier paging |
| 960–1215 MHz | DME/TACAN, SSR 1030 interrogations, **1090 MHz Mode S/ADS-B** (PPM 1 Mbps, 56/112 bits, CRC-24), **978 MHz UAT** (US) | Pulses | |
| 1164–1300 MHz | GNSS L5/E5 (1176.45), L2 (1227.60), E6; 23 cm amateur; long-range radar | DSSS below noise | |
| 1525–1660 MHz | Inmarsat L-band down (1525–1559), **GNSS L1 1575.42** / BeiDou B1 / GLONASS L1, **Iridium 1616–1626.5** | DSSS; Iridium TDMA/FDMA DE-QPSK | GNSS is below the noise floor: use correlation, not energy |
| 1670–1710 MHz | Met-sat downlinks (GOES-R HRIT/EMWIN 1694.1; Metop AHRPT 1701.3), legacy radiosondes 1680 | BPSK/QPSK | |
| 1710–2200 MHz | AWS (B4/B66), PCS (B2/B25) | LTE/NR | |
| 2320–2345 MHz | SiriusXM satellite radio | OFDM/TDM | |
| **2400–2483.5 MHz** | Wi-Fi (802.11b/g/n/ax), Bluetooth Classic (79×1 MHz, 1600 hops/s), **BLE** (40×2 MHz; advertising ch 37/38/39 = 2402/2426/2480 MHz), 802.15.4/Zigbee (ch 11–26 at 2405+5(k−11) MHz), microwave ovens (~2450), drone links | OFDM, DSSS, GFSK FHSS, O-QPSK | Extremely dense; needs time-frequency object detection |
| 2496–2690 MHz | NR n41 | OFDM | |
| 2700–3000 MHz | NEXRAD WSR-88D and airport surveillance radar | Pulsed, PRF ~hundreds of Hz to ~1.3 kHz | Rotating-antenna periodicity (seconds) |
| 3450–3550 MHz | NR n77 (3.45 GHz service) | | |
| 3550–3700 MHz | CBRS (B48/n48), SAS-managed; incumbent Navy radar | | NTIA measured this band ([TR-20-548](https://its.ntia.gov/publications/download/TR-20-548.pdf)) |
| 3700–3980 MHz | C-band NR n77 | | |
| 4200–4400 MHz | Radar altimeters | FMCW | |
| 5150–5895 MHz | U-NII Wi-Fi 5 GHz (DFS on 5250–5725 to protect radar incl. **TDWR 5600–5650**); 5725–5850 ISM: analog/digital FPV video; 5895–5925 C-V2X | OFDM, FM video | |
| 5925–7125 MHz | Wi-Fi 6E/7 (just above the 6 GHz exploration ceiling) | OFDM up to 320 MHz | |

**Protocol quick-reference used by the decoders in §7–8**

| System | Air interface summary |
|---|---|
| P25 Phase 1 | C4FM (or CQPSK/LSM simulcast), 12.5 kHz, 4800 sym/s = 9600 bps, deviations ±600/±1800 Hz; IMBE voice |
| P25 Phase 2 | 2-slot TDMA in 12.5 kHz; H-DQPSK (fixed site) / H-CPM (subscriber), 6000 sym/s; AMBE+2 voice; control channel normally still Phase 1 FDMA ([RR wiki](https://wiki.radioreference.com/index.php/Phase_2)) |
| DMR (ETSI TS 102 361) | 4FSK, 12.5 kHz, 2-slot TDMA, 9600 bps; Tier II conventional, Tier III trunked; proprietary Capacity Plus / Connect Plus / Hytera XPT |
| NXDN | 4FSK; 4800 bps in 6.25 kHz or 9600 bps in 12.5 kHz; Type-C (control channel) and Type-D (distributed) trunking |
| TETRA (ETSI EN 300 392) | π/4-DQPSK, 25 kHz, 4-slot TDMA, 36 kbps gross; TEA1–4 air-interface encryption (limited US use) |
| Motorola SmartNet/SmartZone | 3600 bps control channel; analog FM or P25 voice |
| EDACS | 9600 bps control channel; analog or ProVoice |
| LoRa | Chirp spread spectrum, SF7–SF12, BW 125/250/500 kHz |
| BLE | GFSK (BT = 0.5, h ≈ 0.5), 1 Mbps / 2 Mbps / Coded PHY |
| 802.15.4 (2.4 GHz) | O-QPSK, 2 Mchip/s, 250 kbps, ~2 MHz occupied |

### 1.3 Legal considerations (US; not legal advice)

- **ECPA, 18 U.S.C. §2511.** Interception is generally prohibited. §2511(2)(g) permits intercepting radio communications "readily accessible to the general public", including public safety, government, private land mobile, amateur, CB, GMRS, marine/aero, and distress traffic ([LII §2511](https://www.law.cornell.edu/uscode/text/18/2511)). Per [18 U.S.C. §2510(16)](https://www.law.cornell.edu/uscode/text/18/2510), a radio communication is **not** readily accessible if it is scrambled or encrypted, uses modulation parameters withheld to preserve privacy, or is carried by a common carrier (with an exception for tone-only paging). **Consequences for the product:** decoding unencrypted P25/DMR voice is generally lawful federally; **decrypting** encrypted traffic is not. Cellular content and common-carrier paging message contents are off-limits even when technically decodable. Detecting that a signal *is* encrypted (e.g., a P25 ALGID ≠ 0x80) is metadata observation.
- **Your own traffic is different.** The restriction is on circumventing the security of *other people's* communications. Anyone may record and decrypt their own transmissions for any purpose, not just engineers doing engineering. An example is sending your own encrypted signal to test whether a link works through a region of the atmosphere, then decrypting it on receive. The tool should support this: key management for your own links, and decrypt only where the user holds the keys.
- **47 U.S.C. §605** prohibits divulging or using intercepted communications for benefit, except broadcast, amateur, CB, and distress ([House USC](https://uscode.house.gov/view.xhtml?req=%2247+USC+605%22)). Recording is not the risk; publishing or streaming contents can be.
- **47 CFR §15.121** requires *scanning receivers* marketed as such to be incapable of tuning cellular bands ([LII §15.121](https://www.law.cornell.edu/cfr/text/47/15.121)). General-purpose SDR hardware is typically not certified as a "scanning receiver", but a *product* that bundles a scanner UI should obtain regulatory review before exposing cellular voice-channel demodulation.
- **State laws.** Several states restrict mobile or in-vehicle scanner use or use during a crime.
- **Transmitting** needs a license or rule authority: Part 97 amateur (licensed operator), Part 95 (GMRS requires a license; FRS/MURS/CB are licensed by rule with certified equipment), Part 15 (certified intentional radiators). Transmitting on public safety, aviation, or cellular spectrum, or jamming ([47 U.S.C. §333](https://uscode.house.gov/)), is illegal. An exploration device should be **receive-only by default**.

---

## 2. What makes a frequency "interesting": a feature taxonomy

"Interesting" is a ranking problem. Each detected emission (or channel) gets a feature vector, and the ranking score combines **salience** (strong, active, structured), **novelty** (differs from the learned baseline), and **decodability** (a decoder exists or the parameters look clean).

| # | Feature | How to measure | Cost | Indicates |
|---|---|---|---|---|
| 1 | **Occupancy / duty cycle** | Fraction of revisits above threshold (ITU FCO, §3.8) | Very low | Busy channels; control channels ≈ 100% |
| 2 | **SNR / power above noise floor** | $\hat P_{sig}/\hat N$ from PSD minus noise-floor estimate | Low | Proximity; decode feasibility |
| 3 | **Bandwidth** (−x dB, 99% OBW) | Cumulative PSD (§4.1) | Low | Service class via emission designator match |
| 4 | **Burstiness** | Burst count/s, on-time distribution, inter-arrival CV; spectral kurtosis | Low | Packet radio, telemetry, TDMA |
| 5 | **Periodicity / beacons** | Autocorrelation or periodogram of the burst start-time series; FFT of on/off sequence | Low | TPMS (~minutes), weather sensors (30–60 s), BLE adverts (20 ms–10 s), radar PRF, rotating radar (seconds) |
| 6 | **Hopping** | Time-frequency tracks with constant bandwidth jumping on a raster; ridge/cluster linking | Medium | Bluetooth, FHSS ISM, military |
| 7 | **Novelty vs. baseline** | Per-bin/time-of-day statistics (mean, percentiles of dB); z-score or robust MAD score; isolation forest over emission features | Low–medium | New transmitters, interference, events |
| 8 | **Known-signal match** | Template/protocol decoders, preamble correlation, emission-designator lookup | Medium | Directly labels the emission |
| 9 | **Spectral shape** | Flatness, symmetry, roll-off, pilot tones, subcarrier comb, center spike | Low | Analog vs digital, OFDM, SSB, carrier presence |
| 10 | **Cyclostationary features** | Cycle frequencies of $x^2$, $\lvert x\rvert^2$, $x^4$ (§4.4) | Medium–high | Symbol rate, carrier, modulation family; works below 0 dB SNR |
| 11 | **Symbol rate** | Cyclic features, envelope/squaring spectral line | Medium | Protocol fingerprint |
| 12 | **Center frequency offset** from raster | $f_c$ minus nearest channel center | Very low | Off-raster = unlicensed, faulty, or Doppler |
| 13 | **Time-of-day / weekly pattern** | 24×7 histograms of occupancy | Very low | Business LMR vs 24/7 services; radiosonde launches |
| 14 | **Inter-channel correlation** | Cross-correlation of on/off sequences across channels; lagged co-occurrence | Medium | Trunked voice following control-channel grants; repeater in/out pairs (e.g., ±5 MHz UHF, ±600 kHz VHF amateur, 45 MHz 800 MHz) |
| 15 | **Emitter location / bearing** | RSSI gradient, DF (§9) | Varies | Mobile vs fixed, local vs distant |
| 16 | **Signal "class uncertainty"** | Classifier entropy or open-set score | Low (given classifier) | Unknown signals: very interesting |

A practical score:

$$
S = w_1\,\text{clip}(\text{SNR}_{dB}/20) + w_2\,\text{novelty} + w_3\,H(\hat p_{\text{class}}) + w_4\,\mathbb{1}[\text{decoder available}] + w_5\,\text{periodicity strength} - w_6\,\text{"boring prior"}
$$

The "boring prior" term down-weights, for example, FM broadcast stations after first discovery. Users should be able to tune these weights.

---

## 3. Spectrum sensing and detection

### 3.1 PSD estimation

- **Periodogram** with window $w[n]$ and $N$ samples: $\hat S(f_k) = \frac{1}{f_s \sum w^2}\left|\sum_n w[n]x[n]e^{-j2\pi k n/N}\right|^2$. It has high variance: each bin is ~χ² with 2 degrees of freedom, so ±several dB of noise.
- **Welch** ([Welch 1967](https://doi.org/10.1109/TAU.1967.1161901)): average $K$ windowed, overlapped segments. Variance drops ~$1/K$ (less with overlap correlation; 50% Hann overlap is standard). The resolution bandwidth is $\text{RBW} = \text{ENBW}_{bins}\cdot f_s/N$ (Hann ENBW = 1.5 bins). This is the workhorse for displays and detection.
- **Multitaper** ([Thomson 1982](https://doi.org/10.1109/PROC.1982.12433)): average spectra computed with $K \approx 2NW-1$ orthogonal DPSS (Slepian) tapers. It gives better bias/variance trade-offs and lower leakage for weak signals next to strong ones, at $K$× compute. Worth it for detailed "zoom" analysis, not for the always-on survey.
- **Spectral kurtosis (SK)** ([Antoni 2006](https://doi.org/10.1016/j.ymssp.2004.09.001); [Nita & Gary 2010](https://arxiv.org/abs/1005.4371)): accumulate $S_1=\sum_{i=1}^M P_i$ and $S_2=\sum P_i^2$ per bin over $M$ spectra:

$$
\widehat{SK} = \frac{M+1}{M-1}\left(\frac{M S_2}{S_1^2} - 1\right)
$$

$\widehat{SK}\approx 1$ for Gaussian noise, $<1$ for constant-envelope/CW-like content, and $>1$ for intermittent or pulsed content. The cost is one extra accumulator per bin. It is used in radio astronomy (e.g., EOVSA FPGA correlators, CHIME) for real-time RFI flagging. **For an exploration device it is a nearly free "non-noise and bursty" detector** that complements energy detection.

### 3.2 Noise-floor estimation

Robust noise-floor estimation is the foundation of every threshold. Options, from simplest:

1. **Percentile / median across frequency** in one PSD frame. It is robust if fewer than ~50% of bins are occupied. For χ²₂ bins, the median of the linear power equals $\sigma^2 \ln 2$, so divide by $\ln 2$ (≈ +1.59 dB) to get the mean noise.
2. **ITU "80% method"** ([ITU-R SM.1753](https://www.itu.int/rec/R-REC-SM.1753), used in [SM.2256-1 §3.4.2](https://www.itu.int/dms_pub/itu-r/opb/rep/R-REP-SM.2256-1-2016-PDF-E.pdf)): discard the highest 80% of samples in a scan and linearly average the lowest 20%. It loses sensitivity when many channels are occupied.
3. **Minimum statistics** ([Martin 2001](https://doi.org/10.1109/89.928915)): recursively smooth each bin over time and track its minimum over a sliding window (~1–3 s in speech; use minutes for RF), with bias compensation. It handles time-varying noise, and a channel only needs to be idle at *some* point in the window.
4. **Forward Consecutive Mean Excision (FCME)** ([Vartiainen et al.](https://onlinelibrary.wiley.com/doi/10.1155/2010/459623)): sort the bins, start with the smallest ~10%, iteratively add bins below $T_{CME}\cdot\text{mean(current set)}$ until convergence, where $T_{CME}$ is set from the desired false-alarm rate of an exponential distribution. It tolerates signals covering up to ~85% of the band and is very cheap.
5. **Per-channel idle-time measurement**, preferred by SM.2256 when possible. It captures locally raised noise (e.g., transmitter phase noise near a strong carrier), which a band-wide estimate misses.

**Guard margin.** SM.2256-1 requires the final threshold to be **at least 3–5 dB above the measured noise** to avoid "phantom occupancy".

### 3.3 Energy detection and Neyman–Pearson thresholds

Test statistic over $N$ complex samples (or $N$ averaged bins): $T=\frac1N\sum|x[n]|^2$. Under $H_0$ (complex AWGN, variance $\sigma^2$), $2NT/\sigma^2 \sim \chi^2_{2N}$. The Neyman–Pearson threshold for false-alarm probability $P_{FA}$ is ([Urkowitz 1967](https://doi.org/10.1109/PROC.1967.5573); [Kay, *Detection Theory*](https://www.pearson.com/)):

$$
\gamma = \frac{\sigma^2}{2N}\, F^{-1}_{\chi^2_{2N}}(1-P_{FA}) \;\approx\; \sigma^2\left(1 + \frac{Q^{-1}(P_{FA})}{\sqrt N}\right)
$$

The required samples scale as $N \approx \left[Q^{-1}(P_{FA}) - Q^{-1}(P_D)\sqrt{1+2\,\text{SNR}}\right]^2/\text{SNR}^2$, i.e., $\propto \text{SNR}^{-2}$ at low SNR.

**SNR wall** ([Tandra & Sahai 2008](https://doi.org/10.1109/JSTSP.2007.914879)). With noise-power uncertainty factor $\rho$ (e.g., 1 dB means $\rho = 10^{0.1}$), an energy detector cannot achieve arbitrary $(P_{FA},P_D)$ below

$$
\text{SNR}_{wall} = \frac{\rho^2-1}{\rho}
$$

For 1 dB uncertainty that is about −3.3 dB. Consequence: **uncalibrated, drifting noise floors cap sensitivity no matter how long you integrate**. Feature detectors (cyclostationary, preamble correlation) escape the wall because they do not depend on absolute noise power.

A survey of alternatives (matched filter, cyclostationary, eigenvalue-based) is in [Yücek & Arslan 2009](https://span.ece.utah.edu/uploads/yucek09-spectrum-sensing-algs-cr.pdf).

### 3.4 CFAR detection across frequency (and time)

Treat each PSD frame (or spectrogram) like a radar range profile ([Richards, *Fundamentals of Radar Signal Processing*](https://www.mhprofessional.com/)):

- **CA-CFAR**: for cell under test $i$, average $N$ reference cells (excluding guard cells) to get $\hat P_n$ and declare a detection if $P_i > \alpha\hat P_n$. For square-law detection in exponential noise, $\alpha = N\left(P_{FA}^{-1/N}-1\right)$. It fails near other signals ("masking") and at noise-floor edges.
- **OS-CFAR** ([Rohling 1983](https://ieeexplore.ieee.org/document/4102829/)): use the $k$-th order statistic of the reference window (typically $k\approx 3N/4$). It is robust to multiple signals in the window and to clutter edges. **Recommended default** for RF spectra, which are full of adjacent channels.
- **GO/SO-CFAR** use the greater or smaller of the leading/lagging window averages to handle edges.
- **2-D CFAR** on spectrograms (time × frequency windows) detects short bursts. Follow it with connected-component labeling to form emission "boxes".

For wide-dynamic-range RF spectra, a practical combination is: global noise floor from FCME or a percentile, per-bin minimum statistics for slowly varying floors, then OS-CFAR for local detection, then hysteresis (separate on/off thresholds) and minimum-duration filters.

### 3.5 Time–frequency analysis

- **STFT/spectrogram**: bin width $\Delta f = f_s/N$ and frame time $\Delta t = N/f_s$ (hop $H$), so $\Delta f\,\Delta t \approx 1$. Choose $\Delta t$ below the shortest burst you care about (e.g., 1 ms requires $\Delta f\ge$ 1 kHz). Run **two resolutions in parallel** (e.g., 1 kHz and 25 Hz bins) for bursts vs. narrow carriers.
- **Reassignment** ([Auger & Flandrin 1995](https://doi.org/10.1109/78.382394)) moves energy to local centroids, sharpening chirps (LoRa) and FM tracks at ~3× STFT cost.
- **Wavelets / CWT** give constant-Q analysis suited to transient onset detection and wavelet-based symbol-rate estimation (§4.4).
- **Persistence ("DPX-style") displays**: accumulate a 2-D histogram $H[f, P_{dB}]$ of every FFT frame, with exponential decay $H \leftarrow \beta H + \text{hits}$. Tektronix DPX processes up to millions of spectra per second, giving **100% probability of intercept (POI)** for events down to 15 µs–100 µs on USB RSAs and 0.43 µs on high-end units ([Tektronix RSA datasheets](https://www.tek.com/en/datasheet/spectrum-analyzers-datasheet)). On an SDR plus GPU, tens of thousands of overlapped FFTs per second are realistic and reveal bursts that averaging hides.

### 3.6 Burst detection and segmentation

Pipeline (per channelized stream or on the spectrogram):

1. Compute the short-term power $p[n]$ in a moving window matched to the minimum symbol or burst time.
2. Apply a double threshold with hysteresis: open at $\hat N\cdot 10^{(\text{th}_{on})/10}$, close at a lower $\text{th}_{off}$; enforce minimum on-time and merge gaps shorter than $g_{min}$.
3. Estimate the burst's frequency extent from the spectrogram box (connected components after 2-D CFAR); refine $f_c$ and bandwidth via the burst-averaged PSD.
4. Emit a **burst record**: $(t_{start}, t_{end}, f_c, BW, \text{peak/mean SNR}, \text{SK})$, together with a pointer to the IQ snippet (store ±20% padding) in [SigMF](https://github.com/sigmf/SigMF) format.
5. Link bursts into **tracks** (same $f_c\pm\epsilon$, similar BW) to compute periodicity, hopping patterns, and TDMA frame structure.

Learned alternatives: object detection on spectrograms (YOLO/DETR-style, e.g., [WBSig53](https://arxiv.org/abs/2211.10335)) handles overlapping signals and odd shapes but needs sim-to-real care (§5.5).

### 3.7 Channelization: DDC and polyphase filter banks

- **Per-signal DDC**: NCO mix ($x[n]e^{-j2\pi f_0 n/f_s}$), CIC or halfband cascade decimation, then an FIR cleanup filter. Cost is ~O(N) per signal, which is best when few signals (<~10) of differing bandwidths are extracted.
- **Polyphase filter bank (PFB) channelizer** ([harris, Dick & Rice 2003](https://doi.org/10.1109/TMTT.2003.809176); [harris, *Multirate Signal Processing*](https://www.pearson.com/)): $M$ uniformly spaced channels from one prototype lowpass of length $\approx K\cdot M$. The cost per input block is $O(KM + M\log M)$, independent of how many channels are active, so it wins for dense rasters (trunked 12.5 kHz, a 2.4 GHz channel grid). Use a **2× oversampled** PFB so that signals straddling channel edges are not lost, and synthesize adjacent channels to recombine wider signals.
- **Hybrid** (recommended): a coarse PFB (e.g., 25–100 kHz channels) for surveillance, with narrow DDCs spawned on demand for each detected emission's exact $f_c$/BW.

### 3.8 Sweep-based survey vs. real-time IBW

Key quantities:

- **Instantaneous bandwidth (IBW)**: usable bandwidth per tune (≈0.8×$f_s$ after anti-alias roll-off and DC exclusion).
- **Dwell time** $T_d$: capture time per step. **Settle time** $T_s$: tuning plus PLL settling plus USB latency.
- **Revisit time** $T_R = N_{steps}(T_s + T_d)$ with $N_{steps}=\lceil \text{Span}/\text{IBW}\rceil$.
- **POI** for a burst of duration $\tau$ arriving at a random time (single occurrence, $\tau < T_R$): $P_{POI} \approx \min\left(1, \frac{\tau + T_d}{T_R}\right)$. With repeated bursts at rate $r$ over observation time $T_{obs}$, $P_{\ge1} = 1-(1-P_{POI})^{rT_{obs}}$.
- A **swept analog analyzer** needs sweep time $\approx k\cdot\text{Span}/\text{RBW}^2$ ([Keysight AN 1318 / AN 150](https://anlage.umd.edu/5968-3411E.pdf)). FFT-based SDR sweeps avoid the RBW² penalty inside each IBW chunk; only the chunk count matters.

**Worked example (HackRF).** `hackrf_sweep` retunes in firmware and scans **~8 GHz/s (0.75 s per 0–6 GHz sweep)**. It uses 20 Msps captures but keeps only two 5 MHz sub-bands per step (interleaved, to avoid the DC spike and band-edge roll-off) and drops ~820 µs per retune ([Ossmann & Spill 2017](https://blackhat.com/docs/us-17/wednesday/us-17-Ossmann-Whats-On-The-Wireless-Automating-RF-Signal-Identification-wp.pdf)). That is ~600 steps × 1.25 ms, so actual dwell is sub-millisecond per step. A 5 ms TPMS or meter burst in 902–928 MHz thus has $P_{POI}\approx (5+0.4)/750 \approx 0.7\%$ per occurrence during a full sweep. Parking the same radio on 902–922 MHz gives ~100%. **Design lesson: sweep to find *where*, then dwell (real-time IBW) to find *what*.** The rtl_power-style approach with RTL-SDRs (2–2.4 MHz IBW, host-driven USB retunes) takes seconds to minutes per wide sweep.

**Recommended survey scheduler** (an exploration device's "attention"):
1. **Discovery sweep** of the full range continuously; update per-bin max-hold, mean, and SK statistics.
2. **Candidate selection** from the §2 score: high occupancy, novelty, bursty SK, known priors.
3. **Dwell** full IBW on the top-k candidates for durations proportional to expected burst intervals. The ITU recommends ≥24 h (or multiples) for occupancy with unknown time patterns ([SM.1880-2](https://www.itu.int/rec/R-REC-SM.1880-2-201709-I/en)). For exploration, adaptive dwell of seconds to minutes with multi-armed-bandit revisit scheduling (UCB on "interestingness") is effective.
4. **Record** IQ for bursts meeting the criteria; run classification and decoders on the recording (not just live).

### 3.9 Occupancy statistics methodology (ITU-R SM.1880 / SM.2256)

From [Report ITU-R SM.2256-1](https://www.itu.int/dms_pub/itu-r/opb/rep/R-REP-SM.2256-1-2016-PDF-E.pdf) and [Rec. SM.1880-2](https://www.itu.int/rec/R-REC-SM.1880-2-201709-I/en):

- **Frequency Channel Occupancy (FCO)**: $\text{FCO} = T_O/T = N_O/N$, the fraction of time or samples a channel's level exceeds the threshold. A channel is "occupied" in a revisit if **any** sample inside the channel exceeds the threshold.
- **Frequency Band Occupancy (FBO)**: the fraction of all (frequency, time) samples above threshold. With fine resolution, FBO is typically much lower than the per-channel FCOs.
- **Spectrum Resource Occupancy (SRO)**: FCO averaged over channels (sampled on channel centers).
- **Resolution**: frequency resolution must be at least as fine as the narrowest channel spacing. If RBW < the emission's OBW, reduce the threshold by $10\log_{10}(\text{OBW}/\text{RBW})$.
- **Thresholds**: *pre-set* (receiver sensitivity plus required S/N for the service) or *dynamic* (noise measured on unused frequencies, in idle time slots, or calculated with the 80% method), always **≥3–5 dB above noise**.
- **Timing**: to capture every transmission, the maximum revisit time must be ≤ half the minimum on/off time of transmissions. Otherwise treat occupancy statistically: SM.2256 Annex 1 gives sample-count and confidence-level analysis, where error is roughly normal and shrinks with more samples and stable revisit times.
- **Duration**: ≥24 h when time patterns are unknown.

Real-world context: [McHenry et al. (Shared Spectrum Co.), Chicago Nov 2005](https://www.sharedspectrum.com/wp-content/uploads/NSF_Chicago_2005-11_measurements_v12.pdf) measured **17.4% average occupancy** over 30–3000 MHz. NTIA/ITS performed broadband surveys in [Denver (TR-13-496)](https://its.ntia.gov/publications/browse-publications) and [San Diego, 108 MHz–10 GHz (TR-14-498)](https://its.ntia.gov/publications/details?pub=2741), and long-term 3.45–3.65 GHz radar-band occupancy at four coastal sites ([TR-20-548](https://its.ntia.gov/publications/download/TR-20-548.pdf)). Most spectrum is quiet most of the time, so an exploration tool should spend its dwell budget where activity is.

---

## 4. Signal characterization and parameter estimation

Assume a detected emission has been channelized to complex baseband $x[n]$ at a few × its bandwidth.

### 4.1 Bandwidth

- **β% occupied bandwidth** (ITU RR, [SM.328](https://www.itu.int/dms_pubrec/itu-r/rec/sm/R-REC-SM.328-12-202509-I!!PDF-E.pdf) / [SM.443](https://www.itu.int/dms_pubrec/itu-r/rec/sm/R-REC-SM.443-4-200702-I!!PDF-E.pdf)): from the noise-subtracted PSD, cumulatively sum power and find $f_L$ where 0.5% is reached and $f_U$ where 99.5% is reached; $\text{OBW}_{99}=f_U-f_L$. Subtract the noise floor first or the OBW inflates to the analysis span.
- **x-dB bandwidth**: the outermost frequencies where the PSD falls x dB below the peak (−26 dB is common in regulation, −3/−6 dB for filters). SM.443 gives x-dB choices approximating the 99% OBW for common emissions.
- Use Welch averaging over the burst duration only (gated), not over the idle time.

### 4.2 Carrier frequency offset (CFO)

- **Spectral centroid** of the noise-subtracted PSD: fast and adequate for symmetric spectra.
- **Power-of-M** for M-PSK: $f_{off} = \frac{1}{M}\arg\max_f\left|\mathcal F\{x^M[n]\}\right|$. Unambiguous range $\pm f_s/(2M)$ ([PySDR Synchronization](https://pysdr.org/content/sync.html)). QAM uses $M=4$.
- **FSK**: mean of the instantaneous frequency $f_i[n] = \frac{f_s}{2\pi}\arg(x[n]x^*[n-1])$ over a balanced sequence, or the midpoint of the two histogram peaks.
- **Carrier-bearing signals** (AM, pilots): peak-bin interpolation (quadratic or Jacobsen estimator), then Kay/Fitz phase-difference estimators for sub-bin precision.
- **OFDM**: CP correlation phase (§4.8) for fractional subcarrier offsets.
- Report CFO **relative to the nearest channel raster** (itself a feature; see §2 #12) after correcting receiver ppm error (§10.1).

### 4.3 SNR estimation

- **Spectral SNR**: $\widehat{\text{SNR}} = \dfrac{P_{band} - \hat N_0 B}{\hat N_0 B}$ with $\hat N_0$ from adjacent idle bins. It is modulation-agnostic.
- **M2M4** (non-data-aided, constant-modulus signals; [Pauluzzi & Beaulieu 2000](https://doi.org/10.1109/26.871393), implemented in [GNU Radio `mpsk_snr_est_m2m4`](https://www.gnuradio.org/doc/doxygen/classgr_1_1digital_1_1mpsk__snr__est__m2m4.html)). With $M_2=E|r|^2$ and $M_4=E|r|^4$ for complex baseband:

$$
\hat S = \sqrt{2M_2^2 - M_4}, \qquad \hat N = M_2 - \hat S, \qquad \widehat{\text{SNR}} = \hat S/\hat N
$$

It works at any sample position (no timing needed) and is biased for non-constant-modulus (QAM) signals; generalized kurtosis-corrected versions exist.
- **Decision-directed / EVM-based** after demodulation: $\text{SNR}\approx 1/\text{EVM}^2$. This is the most accurate once locked.

### 4.4 Symbol-rate estimation

1. **Envelope / squaring spectral line.** For linearly modulated signals with pulse-shape excess bandwidth $\alpha>0$, $|x(t)|^2$ has a spectral line at $R_s = 1/T$. Band-limit, compute $|x|^2$, remove the mean, and find the FFT peak. It is cheap but weak for small $\alpha$ (line strength ∝ roughly α).
2. **Delay-and-multiply**: $y[n]=x[n]x^*[n-D]$ with $D \approx T/2$ strengthens the $R_s$ line.
3. **Cyclostationary analysis** ([Gardner 1991](https://doi.org/10.1109/79.81007); [Spooner's CSP blog](https://cyclostationary.blog/)). The cyclic autocorrelation is

$$
R_x^\alpha(\tau) = \left\langle x(t+\tfrac\tau2)\,x^*(t-\tfrac\tau2)\,e^{-j2\pi\alpha t}\right\rangle_t
$$

   and the spectral correlation function is $S_x^\alpha(f)=\mathcal F\{R_x^\alpha(\tau)\}$. Digital linear modulations have **non-conjugate** cycle frequencies $\alpha = k/T$. BPSK/PAM/ASK also have **conjugate** features at $\alpha = 2f_c + k/T$ (from $x\cdot x$), and QPSK/QAM show $4f_c$ features in 4th-order statistics. Blind, exhaustive estimators are the **SSCA** and **FAM** ([Roberts, Brown & Loomis 1991](https://doi.org/10.1109/79.81008); [CSP blog: SSCA](https://cyclostationary.blog/2016/03/22/csp-estimators-the-strip-spectral-correlation-analyzer/), [FAM](https://cyclostationary.blog/2018/06/01/csp-estimators-the-fft-accumulation-method/)). Use the **spectral coherence** (normalized SCF) to pick significant cycle frequencies without SNR bias. Cost: SSCA is $O(N N')$-ish with FFTs and heavy for wide bands, but fine on a single channelized emission of a few × 10⁴ samples.
4. **Wavelet (Haar) transient method**: the magnitude of the Haar CWT of a PSK/FSK signal peaks at symbol transitions; the spectrum of that magnitude shows $R_s$. It is robust for FSK, where envelope methods fail.
5. **FSK / pulse-width**: histogram of run lengths in the sliced instantaneous frequency (or OOK envelope); the GCD-like fundamental of the run lengths gives $T$. This is what rtl_433/URH effectively do (§7).

Refine the coarse $\hat R_s$ with a timing-recovery loop (§7.2). The loop's steady-state frequency correction gives ppm-level accuracy.

### 4.5 Modulation order, pulse shape, roll-off

- **Constellation order** after timing/carrier recovery: cluster the symbols (k-means over candidate M ∈ {2,4,8,16,32,64}, selected by BIC or silhouette); phase-only histograms for PSK; ring counts for APSK.
- **Cumulants before full synchronization** (Swami & Sadler, §5.2) separate PAM/PSK/QAM order groups using only a coarse CFO correction.
- **Roll-off α**: fit a raised-cosine PSD to the measured spectrum. The −3 dB bandwidth ≈ $R_s$ and the null-to-null bandwidth ≈ $R_s(1+\alpha)$, so $\hat\alpha \approx \text{BW}_{null}/\hat R_s - 1$. Alternatively, use the cyclic-feature strength at $\alpha=R_s$, which grows with roll-off.
- **GFSK BT** is inferred from transition shapes of the instantaneous frequency (eye-opening width) or by fitting Gaussian-filtered FSK templates.

### 4.6 FSK deviation and modulation index

Compute $f_i[n]$ (discriminator), build a histogram, and find $L$ peaks (2/4/8-FSK): deviation $\Delta f$ = half the outer peak spacing, and modulation index $h = 2\Delta f/R_s$ for 2-FSK. $h=0.5$ → MSK/GMSK (GSM, AIS 9600 at BT 0.4); BLE $h \approx 0.5$; P25 C4FM peaks at ±600/±1800 Hz; DMR at ±648/±1944 Hz. The **peak positions alone often identify the LMR protocol** before any framing is decoded.

### 4.7 Hop detection and burst timing

- Build tracks from burst records (§3.6). A hopping emitter shows constant BW and dwell time $T_h$, frequencies on a raster $f_0 + k\Delta$, and (for a single emitter) no temporal overlap between consecutive dwells.
- Estimate: hop rate $1/T_h$ from the dwell-duration mode; channel spacing $\Delta$ as the GCD of frequency differences; hop set as the unique frequencies; sequence period from autocorrelation of the channel-index series.
- Use a sampling rate covering the hop set. Bluetooth Classic (1600 hops/s over 79 MHz) needs ~80 MHz IBW to follow completely; sweeping cannot follow it.
- **TDMA timing**: a histogram of burst start times modulo candidate frame periods (e.g., DMR 60 ms two-slot / 30 ms slots; TETRA 56.67 ms frames; GSM 4.615 ms) identifies the frame structure.

### 4.8 OFDM parameter estimation

The cyclic prefix copies the last $L_{CP}$ samples of each $N_{FFT}$-sample symbol, so the autocorrelation peaks at lag $N_{FFT}$ (in samples at the analysis rate):

$$
\gamma(m, D) = \sum_{k=m}^{m+L-1} x[k]\,x^*[k+D], \qquad \hat D = \arg\max_D \frac{\left|\sum_m \gamma(m,D)\right|}{\text{energy norm}}
$$

- **Useful symbol duration** $T_u = \hat D/f_s$, so **subcarrier spacing** $\Delta f = 1/T_u$ (LTE 15 kHz; NR 15·2^μ kHz; Wi-Fi 312.5 kHz; DVB-T 1116/4464 Hz).
- **CP length** comes from the width of the plateau of $|\gamma(m,\hat D)|$ vs. $m$. **Symbol period** $T_s=T_u+T_{CP}$ is the periodicity of that plateau.
- **Fractional CFO** $= -\angle\gamma/(2\pi T_u)$ ([van de Beek, Sandell & Börjesson 1997](https://doi.org/10.1109/78.599949)).
- The number of active subcarriers follows from OBW / Δf. Pilot/guard structure is visible in the averaged spectrum after sub-bin alignment.
- Cyclic features of OFDM with a CP appear at $\alpha = k/T_s$.

### 4.9 DSSS detection

DSSS (GNSS, 802.11b Barker, CDMA) sits near or below the noise floor, so energy detection fails. Options:
- **Autocorrelation fluctuation / covariance detection**: the second-order moment of short-time autocorrelation estimates at lags $kT_{chip}$ or at the code period (e.g., GPS C/A 1 ms) shows peaks absent in noise.
- **Cyclic feature at the chip rate** ($\alpha = R_c$) in $|x|^2$ or the SCF.
- **Known-code correlation** (GNSS acquisition: FFT-based parallel code-phase search) for identification.
- **Spectral shape**: a sinc² main lobe of width $2R_c$ when SNR allows.

### 4.10 Analog vs. digital discrimination (quick tests)

| Test | Analog voice (AM/FM/SSB) | Digital |
|---|---|---|
| Cyclic feature at a symbol rate | Absent | Present (linear mods, FSK via transitions) |
| Instantaneous-frequency histogram | Continuous, speech-like (FM) | Discrete levels (FSK) |
| PSD shape | Time-varying, speech-envelope, carrier line (AM) | Stationary, flat-top/RRC skirts |
| Envelope kurtosis over 100 ms windows | Highly variable with syllables | Stable |
| Spectral line at CTCSS 67–254 Hz after demod | Common on LMR | N/A |

---

## 5. Automatic Modulation Classification (AMC)

### 5.1 Framework

Survey references: [Dobre, Abdi, Bar-Ness & Su 2007](https://web.njit.edu/~abdi/IEE_COM0176_WithFigures.pdf) (classical); [Xu, Su & Zhou 2011](https://doi.org/10.1109/TSMCC.2010.2076347) (likelihood-based); recent ML surveys ([arXiv 2502.05315](https://arxiv.org/pdf/2502.05315), [arXiv 2503.08091](https://arxiv.org/pdf/2503.08091)).

- **Likelihood-based (LB)**: compute $\Lambda_c = p(\mathbf r \mid c, \theta)$ with unknown parameters $\theta$ averaged (ALRT), maximized (GLRT), or hybrid (HLRT). It is Bayes-optimal under the assumed model but needs accurate models of the channel, timing, CFO, and noise. Complexity grows with parameter dimensionality and constellation size, and it is fragile under model mismatch. **Use it as a verifier** within a small candidate set (e.g., QPSK vs 8PSK after sync), not as a blind front-end.
- **Feature-based (FB)**: extract robust statistics, then use a decision tree, SVM, or gradient boosting. It is cheap, interpretable, and degrades gracefully.
- **Deep learning (DL)**: learn features from raw IQ or spectrograms. It is best in-distribution but brittle out-of-distribution.

### 5.2 Classical features

**Azzouz & Nandi key features** ([Nandi & Azzouz 1998](https://www.semanticscholar.org/paper/Algorithms-for-automatic-modulation-recognition-of-Nandi-Azzouz/30994f858491d3da2ff0ca14758c3123834b0085); [book, 1996](https://www.amazon.com/Automatic-Modulation-Recognition-Communication-Signals/dp/0792397967)). With $a_{cn}[n] = a[n]/\bar a - 1$ (normalized-centered envelope), $\phi_{NL}$ the centered nonlinear phase (after removing the linear carrier phase) over non-weak samples ($a_n > a_t$), and $f_N$ the normalized-centered instantaneous frequency:

| Feature | Definition | Separates |
|---|---|---|
| $\gamma_{max}$ | $\max \lvert\mathcal F\{a_{cn}\}\rvert^2 / N_s$ | Amplitude-varying (AM, ASK, QAM) vs constant envelope (FM, PSK, FSK) |
| $\sigma_{ap}$ | std of $\lvert\phi_{NL}\rvert$ (non-weak samples) | Absolute-phase info: PSK4 vs BPSK; FM vs AM |
| $\sigma_{dp}$ | std of $\phi_{NL}$ (direct) | Phase-bearing (FM, PSK) vs not (AM, DSB with symmetric phase) |
| $P$ | $(P_L-P_U)/(P_L+P_U)$ spectral symmetry about $f_c$ | **SSB** (≈±1) vs DSB/VSB/FM (≈0) |
| $\sigma_{aa}$ | std of $\lvert a_{cn}\rvert$ | ASK2 vs ASK4 |
| $\sigma_{af}$ | std of $\lvert f_N\rvert$ (non-weak samples) | FSK2 vs FSK4 |
| $\mu_{42}^a$, $\mu_{42}^f$ | kurtosis of $a_{cn}$ / of $f_N$ | AM vs others / FM vs FSK |

Thresholds come from training, and reported accuracy is ~high-90s% above ~10–15 dB SNR in the original simulations. These features map directly onto the analog demodulator selector in §6.

**Higher-order cumulants** ([Swami & Sadler 2000](https://www.semanticscholar.org/paper/Hierarchical-digital-modulation-classification-Swami-Sadler/b03fa2f4aeb84d9e7ef2c565d18e4c3acc2e1050)). With $M_{pq} = E[x^{p-q}(x^*)^q]$:

$$
C_{40}=M_{40}-3M_{20}^2,\quad C_{41}=M_{41}-3M_{20}M_{21},\quad C_{42}=M_{42}-|M_{20}|^2-2M_{21}^2
$$

normalized by $C_{21}^2$ (after subtracting the noise variance from $C_{21}$). Ideal noise-free values:

| Constellation | $\tilde C_{40}$ | $\tilde C_{42}$ |
|---|---|---|
| BPSK | −2.00 | −2.00 |
| 4-PAM | −1.36 | −1.36 |
| 8-PAM | −1.24 | −1.24 |
| QPSK | +1.00 (magnitude; phase-dependent) | −1.00 |
| 8-PSK and higher | 0 | −1.00 |
| 16-QAM | −0.68 | −0.68 |
| 64-QAM | −0.62 | −0.62 |

Gaussian noise has zero 4th-order cumulants, so they are asymptotically AWGN-insensitive. Use $|\tilde C_{40}|$ for phase-offset invariance. Complexity is $O(N)$, the method is recursive, and it tolerates phase/frequency offsets (the magnitude of $C_{42}$ is CFO-invariant). Required $N$ grows quickly for high-order QAM at low SNR (thousands to tens of thousands of symbols). The features need symbol-spaced samples for best separation, but over-sampled data still works with adjusted thresholds.

**Cyclostationary features** (§4.4) give modulation-family signatures: the pattern of conjugate/non-conjugate cycle frequencies at 2nd, 4th, and 6th order (e.g., BPSK has $2f_c$; QPSK has none at 2nd order conjugate but has $4f_c$; MSK has $2f_c \pm R_s/2$). Spooner's work shows CSP features are **robust to CFO and sampling-rate shifts**, exactly where CNNs trained on raw IQ generalize poorly ([CSP blog on CSP+DL](https://cyclostationary.blog/2023/06/20/latest-paper-on-csp-and-deep-learning-for-modulation-recognition-an-extended-version-of-my-papers-52/)).

### 5.3 Deep learning approaches and reported numbers

| Work | Data | Model | Reported result |
|---|---|---|---|
| [O'Shea, Corgan & Clancy 2016](https://arxiv.org/abs/1602.04105) | RML2016.10a: 11 classes (8 digital, 3 analog), 128 IQ samples, −20 to +20 dB | Small CNN | "roughly 87.4%" across the test set; beats cumulant-feature baselines by 2.5–5 dB at low SNR, similar above +5 dB; residual 8PSK/QPSK and WBFM/AM-DSB confusion |
| [O'Shea, Roy & Clancy 2018](https://arxiv.org/abs/1712.04578) | RML2018.01A: 24 classes, 1024 samples, −20 to +30 dB; plus over-the-air | ResNet / VGG | ResNet ~95% at high SNR vs **~61%** for boosted trees on higher-order moments; ~5 dB sensitivity gain; **OTA-trained 95.6%**; synthetic-trained evaluated on OTA dropped ~7 points to ~87%; clock/LO offset impairments dropped ResNet to 59–80% at high SNR; ~+3% per doubling of input length, diminishing beyond 512 |
| [Rajendran et al. 2018](https://doi.org/10.1109/TCCN.2018.2835460) | RML2016 | LSTM on amplitude/phase | ~90% at high SNR, small model |
| [Boegner et al. 2022 (Sig53/TorchSig)](https://arxiv.org/abs/2207.09918) | 53 classes (ASK/PAM/PSK/QAM/FSK/OFDM families), 4096 IQ samples, 5M examples, SNR −2 to 30 dB (Es/N0), impairments (fading, IQ imbalance, CFO, resampling) | EfficientNet-B0/B2/B4, XCiT-Nano/Tiny12 | Impaired top-1: B0 62.75%, B4 67.46%, XCiT-Tiny12 70.22%; with online generation XCiT-Tiny12 **71.16%**. Transformers beat ConvNets |
| [Boegner et al. 2022 (WBSig53)](https://arxiv.org/abs/2211.10335) | 550k wideband spectrogram samples, ~2M signals | Object detection (DETR/YOLO-family) | Detection plus recognition of multiple signals per capture |
| Foundation models 2025–26 ([IQFM](https://arxiv.org/pdf/2506.06718), [SpectrumFM](https://arxiv.org/pdf/2505.06256), [Multimodal WFM](https://arxiv.org/abs/2511.15162)) | Self-supervised on large IQ corpora | Transformers | Promising few-shot transfer; not yet a proven drop-in for field AMC |

**Reading these numbers honestly**

- Accuracy is **SNR-conditional**. Averages over −20…+30 dB mix hopeless and trivial cases, so always report accuracy vs. SNR.
- Class sets differ (11 vs 24 vs 53), and the higher-order QAMs (64/128/256) dominate residual error.
- **Dataset quality issues**: the CSP blog documents problems in the DeepSig RML datasets, including "AM-SSB" examples that contain only noise, label-mapping mismatches in 2018.01A, and odd PSDs for some PSK/QAM/PAM examples ([CSP blog: More on DeepSig's RML datasets](https://cyclostationary.blog/2020/08/17/more-on-deepsigs-rml-data-sets/); [comments on the 2016 paper](https://cyclostationary.blog/2017/01/31/machine-learning-and-modulation-recognition-comments-on-convolutional-radio-modulation-recognition-networks-by-t-oshea-j-corgan-and-t-clancy/)). Do not use RML-trained models as-is in a product.

### 5.4 Known issues

1. **Synthetic-to-real gap.** Real front ends add IQ imbalance, DC offsets, phase noise, AGC transients, ADC clipping, multipath, adjacent-channel interference, and filter shapes the generator never simulated. O'Shea 2018 measured a ~7-point drop from synthetic to OTA. Transfer-learning studies show performance depends strongly on the similarity of source and target domains ([Wong et al., RF transfer learning](https://arxiv.org/abs/2210.01158)). **Mitigations:** heavy augmentation (random CFO, resampling ±ppm, fading, IQ imbalance, clipping, adjacent interferers); fine-tuning on a small labeled capture set from *your own hardware*; self-supervised pretraining on unlabeled local captures; front-end-invariant inputs (normalized, CFO-coarse-corrected, resampled to fixed samples/symbol after §4 estimation).
2. **SNR dependence.** Train with the SNR distribution you expect. Add an SNR estimate as an input or gate. Below ~0 dB, fall back to "digital, unknown order" rather than forcing a label.
3. **Parameter normalization.** CNNs over raw IQ at arbitrary samples-per-symbol generalize poorly. **Estimate $R_s$, $f_c$, and BW first (§4), then resample to a canonical sps** before DL. This single step removes most of the nuisance variation.
4. **Open-set recognition.** Softmax confidence is not a reliable "unknown" detector: unknown classes are routinely misclassified with high confidence. Approaches include OpenMax/Weibull calibration, center loss plus distance thresholds, metric-learning embeddings, and energy-based OOD scores ([Open-set DC-LSTM](https://arxiv.org/pdf/2002.12037); [augmenting DL with expert features for open set](https://arxiv.org/pdf/2302.03749); [few-shot open-set AMC](https://arxiv.org/html/2410.10265)). For an exploration device, **"unknown" is a first-class output** that triggers recording and user review.
5. **Multiple and overlapping signals.** Narrowband classifiers assume one signal per snippet; the wideband detection stage must isolate emissions first.
6. **Label taxonomy.** Users care about *protocols* (P25, DMR, POCSAG) more than modulation (4FSK). Classify at modulation level, then refine with parameter fingerprints and decoders.

### 5.5 Compute cost on embedded platforms

- **Classical features** (cumulants, Azzouz–Nandi, spectral line $R_s$): microseconds to milliseconds per snippet on an ARM core; run on every burst.
- **Small CNN/LSTM** (RML-scale, 0.1–1.5M params, 128–1024 samples): sub-millisecond on a Jetson GPU or a few ms on a Cortex-A76 CPU. Binarized/quantized variants cut memory ~4–5× and compute by orders of magnitude ([binarized ResNet AMC](https://arxiv.org/pdf/2110.14357)).
- **EfficientNet-B0/XCiT-Nano scale** (~3–5M params, 4096 samples): a few ms on Jetson Orin with FP16/INT8.
- **TensorRT INT8/FP16** on Jetson AGX Orin gave roughly **10–15× speedups** over PyTorch for CNNs ([Zhou et al., MCSoC 2023](https://userweb.cs.txstate.edu/~k_y47/webpage/pubs/mcsoc23.pdf)); edge AMC benchmarks: [arXiv 2404.15343](https://arxiv.org/pdf/2404.15343).
- **Spectrogram object detectors** (YOLO-class) at 512×512: tens of fps on Orin-class hardware, adequate for 1–10 Hz wideband scene updates.
- **Budget guidance**: always-on detection (FFT + SK + CFAR) on DSP/GPU; per-burst classical features on CPU; DL only on bursts that survive detection, batched; CSP (SSCA) only for "hard" or unknown emissions.

**Recommended architecture:** detector → parameter estimation (§4) → normalization → **cascaded classifier** (fast feature tree for coarse family: analog / FSK / PSK-QAM / OFDM / pulsed / chirp / DSSS; then a small DL model per family; then open-set scoring) → Bayesian fusion with priors (§1.1) → protocol decoders as final arbiters. A decoder that produces valid CRCs is ground truth and should feed back as labels for on-device fine-tuning.

---

## 6. Automatic analog demodulation selection, squelch, AGC

The goal is that the user never picks FM/AM/WFM/SSB or sets squelch.

### 6.1 Discriminating AM / NBFM / WFM / SSB / CW from IQ

Compute over 100–500 ms windows on the channelized signal (bandwidth from §4.1, centroid from §4.2):

| Mode | OBW | Carrier line at center | Envelope ($\sigma_a$, $\gamma_{max}$) | Inst. freq ($\sigma_{af}$, kurtosis) | Spectral symmetry $P$ | Other cues |
|---|---|---|---|---|---|---|
| **AM (DSB-TC)** | 2× audio BW (~6–10 kHz voice; ~10–20 kHz broadcast) | **Strong, stable** | High, speech-correlated | Low (near-constant about carrier) | ≈0 | Aviation band prior; AM carrier persists in pauses |
| **NBFM** | 8–16 kHz (Carson: $2(\Delta f + f_m)$ = 2(2.5+3) = 11 kHz for 12.5 kHz channels; 2(5+3) = 16 kHz legacy) | Carrier present in silence; line smears with modulation | **Low** (constant envelope) | High, continuous distribution | ≈0 | CTCSS tone after demod; FM quieting |
| **WFM (broadcast)** | ~180–260 kHz (±75 kHz deviation, 53–100 kHz MPX) | — | Low | Very high | ≈0 | **19 kHz pilot** after demod; 57 kHz RDS; 200 kHz raster in 88–108 |
| **SSB (USB/LSB)** | ~2.4–3 kHz | **None** (suppressed) | High, speech-like | Erratic (phase meaningless) | **≈ ±1 about the suppressed carrier** | Energy starts ~300 Hz from one edge; band convention |
| **CW** | < ~200 Hz | Keyed carrier | On/off at 5–40 WPM dot rates | Near-zero while on | n/a | Very narrow; Morse timing ratio 1:3:7 |
| **DSB-SC** | 2× audio | None | Speech-like | Phase jumps of π | ≈0 | Squaring yields $2f_c$ line (like BPSK) |

**Decision-tree sketch** (thresholds to be trained on captures):

```
if OBW > 120 kHz and constant envelope        -> WFM (confirm 19 kHz pilot)
elif OBW < 300 Hz and on/off envelope         -> CW
elif constant envelope (sigma_a small)        -> NBFM (or digital FSK if IF histogram is discrete)
elif strong stable carrier line at centroid   -> AM
elif |P| > 0.6 and OBW in [1.8, 3.5] kHz      -> SSB: USB if energy above the inferred carrier, else LSB
else                                          -> unknown/digital -> AMC path (§5)
```

**SSB carrier inference.** An SSB carrier cannot be located from a spectral peak. Estimate it as the spectral edge nearest a voice low-cut (energy begins ~200–300 Hz from the carrier), snap to channel rasters (e.g., 1 kHz or 500 Hz in HF utility; amateur conventions), then refine by maximizing voice pitch-harmonic regularity (the harmonic comb of voiced speech aligns only when the offset is right, within ±10–20 Hz). Mistuning by >50 Hz is audibly wrong, so offer a fine "clarifier" nudge as the only manual control.

### 6.2 Automatic squelch

- **SNR squelch (universal).** Open when in-channel power exceeds $\hat N_0 B \cdot 10^{\text{th}/10}$, with hysteresis (e.g., open at 6 dB, close at 3 dB), attack of 10–20 ms, and a hang time of 300–800 ms for voice. $\hat N_0$ comes from §3.2 (adjacent bins or minimum statistics), so no user threshold is needed.
- **Noise squelch (FM, classic).** FM demodulator output contains strong wideband noise when no carrier is present, and the noise is suppressed ("FM quieting") when a carrier captures the discriminator. High-pass the discriminator output above the voice band (e.g., >4 kHz, or a dedicated 40–100 kHz band in hardware designs), rectify and smooth the result, and close the audio when the noise exceeds a level ([Ham radio wiki: Squelch](http://wiki.hamtools.org/index.php?title=Squelch); [US4359780](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/4359780)). In DSP, demodulate at ≥2× the channel bandwidth so there is out-of-voice spectrum to measure. It is self-normalizing: the ratio of HF noise power to total discriminator output power is independent of the RF gain.
- **Tone squelch detection (metadata plus squelch).**
  - **CTCSS**: ~50 standard tones 67.0–254.1 Hz. Low-pass the demodulated audio below 300 Hz, decimate to ~1–2 kHz, and run a Goertzel filter bank or a 0.5–1 Hz-resolution FFT. Detect within ~150–250 ms and require persistence. Report the tone as a channel attribute.
  - **DCS**: a 134.4 bps NRZ sub-audible bitstream carrying a 23-bit Golay(23,12) word (9-bit octal code plus fixed bits) ([sigidwiki DCS](https://www.sigidwiki.com/wiki/Digital-Coded_Squelch_(DCS))). Low-pass <300 Hz, slice, recover clock, correlate against all codewords in both polarities (and inverted codes), with a 134 Hz "turn-off" burst.
  - These tones distinguish users sharing a frequency, which is useful for grouping transmissions.

### 6.3 AGC design

- Separate **RF/IF gain control** (hardware LNA/VGA/attenuator, slow, avoids ADC clipping and intermod; §10.4) from **demodulator/audio AGC** (digital, per channel).
- Implement a digital AGC in the log domain: $g_{dB}[n+1] = g_{dB}[n] + \mu_{a|d}(L_{ref} - L_{dB}[n])$, with a fast attack (1–10 ms) and slow decay (AM 0.2–0.5 s, SSB "hang" AGC 0.5–2 s with a hang timer). FM needs essentially no pre-demod AGC (a limiter or `arg()` is amplitude-invariant); apply audio loudness normalization after demod.
- AM: use a **coherent (PLL-locked) or synchronous AM** detector to resist selective fading, with a fallback to envelope detection.

### 6.4 De-emphasis, stereo, RDS

- **WFM de-emphasis**: single-pole with τ = **75 µs** (Americas, South Korea) or **50 µs** (Europe and most elsewhere); select by region or band plan. $H(s) = 1/(1+s\tau)$, implemented as a bilinear-transform IIR at the audio rate.
- **NBFM LMR**: 6 dB/octave pre-emphasis over ~300–3000 Hz per TIA-603-style practice. Apply matching de-emphasis plus a 300 Hz high-pass (to remove CTCSS) and a 3 kHz low-pass.
- **FM stereo**: detect the **19 kHz pilot** with a narrow PLL or Goertzel filter in the MPX (typical injection ~8–10%). If present, regenerate 38 kHz (2× pilot phase) and demodulate L−R DSB-SC; blend to mono at low SNR. FCC tolerance on the pilot is ±2 Hz, which makes it a frequency reference too (§10.1).
- **RDS/RBDS** (IEC 62106; NRSC-4 in the US): 57 kHz (3× pilot) subcarrier, **1187.5 bps** biphase (differentially encoded) BPSK. It carries 26-bit blocks (16 data + 10-bit checkword with offset words A/B/C/D) grouped by four; decoding yields PI code, PS name, RadioText, and clock time, which are **auto-labels for FM stations**.
- **HD Radio (IBOC)**: OFDM sidebands at roughly ±130–200 kHz around FM hybrid stations. Detect them to annotate "hybrid digital", and exclude them from the WFM bandwidth estimate.

---

## 7. Digital protocol identification and decoding pipeline

### 7.1 Pipeline overview

```
burst record + IQ snippet
  -> fine CFO/BW/SNR (§4)
  -> modulation family decision (§5): OOK/ASK | 2/4-FSK/GFSK/MSK | PSK/QAM | OFDM | CSS | DSSS
  -> demod to soft symbols:
       OOK/ASK: envelope |x| -> adaptive two-level slicer (k-means/Otsu per burst)
       FSK:     quadrature discriminator f_i[n] = arg(x[n] x*[n-1]) -> slicer (2 or 4 levels)
       PSK/QAM: matched filter (RRC) -> timing recovery -> Costas/decision-directed PLL
       CSS:     dechirp (multiply by conj base chirp) -> FFT -> symbol = argmax bin
  -> clock recovery (§7.2)
  -> bit stream (+ line decoding: NRZ, NRZI, Manchester, differential, PWM, PPM)
  -> preamble / sync-word detection (§7.3)
  -> descrambling / dewhitening (LFSR search)
  -> framing: length field, fixed-length, or terminator
  -> FEC decoding (conv/Viterbi, RS, BCH, Golay)
  -> CRC/checksum identification & validation (§7.4)
  -> protocol fingerprint match / decoder dispatch (§7.5)
  -> structured output (JSON), statistics, and labels fed back to the classifier
```

### 7.2 Clock (symbol timing) recovery

- **Mueller & Müller** ([1976](https://doi.org/10.1109/TCOM.1976.1093326)): decision-directed, 1 sample/symbol: $e[k] = \text{Re}\{\hat d^*[k-1]\,y[k] - \hat d^*[k]\,y[k-1]\}$. Implementation details and loop tips (≈20–30 symbols to lock) are in [PySDR](https://pysdr.org/content/sync.html). It is sensitive to carrier phase, so pair it with a Costas loop or use the non-coherent variant.
- **Gardner TED** ([Gardner 1986](https://ui.adsabs.harvard.edu/abs/1986ITCom..34..423G/abstract)): non-data-aided, 2 samples/symbol, carrier-phase independent: $e[k] = \text{Re}\{(y[k]-y[k-1])\,y^*[k-\tfrac12]\}$. **Recommended default for blind PSK/QAM.**
- **Early–late gate / zero-crossing** detectors: simplest for FSK/OOK bit streams.
- **Polyphase filter-bank clock sync** ([harris & Rice 2001](https://doi.org/10.1109/49.974618); [Rice, *Digital Communications: A Discrete-Time Approach*](https://www.pearson.com/)): a bank of $N_f$ (e.g., 32) fractionally delayed matched filters. The loop selects the filter index (fractional timing) using the derivative-matched-filter error $e = \text{Re}\{y\cdot y'^*\}$. It is what GNU Radio's `symbol_sync`/`pfb_clock_sync` implement. Matched filtering and interpolation happen in one structure, which makes it efficient and accurate.
- Loop design: 2nd-order PI loop with normalized bandwidth $B_nT \approx 0.005$–0.02 and damping ≈0.707. Use **wider bandwidth for short bursts** (packet radio) and narrower for continuous streams, or a feedforward (block) estimator such as Oerder–Meyr squaring for bursts shorter than ~50 symbols.
- Carrier: Costas loop (BPSK/QPSK), a 4th-power loop for QAM, or decision-directed. FSK needs no carrier lock (the discriminator is non-coherent), which is why FSK is the easiest blind-decode family.

### 7.3 Preamble and sync-word detection

- Alternating `1010…` preambles produce a **tone at $R_s/2$** in the demodulated stream. They are easy to detect and give symbol rate and timing for free.
- After the preamble, search for a **sync word** by sliding correlation in both bit polarities (FSK mark/space ambiguity) and, for PSK, all constellation rotations. Common examples: TI CC1101 default `0xD391`; POCSAG frame sync `0x7CD215D8`; Mode S 8-bit pulse preamble; AIS HDLC flag `0x7E`; DMR 48-bit sync patterns (voice/data, BS/MS sourced); P25 48-bit frame sync `0x5575F5FF77FF`.
- **Blind sync-word discovery** across multiple bursts of the same emitter: align bursts on preamble end, then find the longest common bit substring. Bit positions with zero entropy across bursts form preamble, sync, and fixed header fields.

### 7.4 CRC, scrambling, FEC identification

- **CRC reverse engineering** exploits linearity: XOR two valid same-length messages to cancel init/xorout and obtain an affine relation that constrains the polynomial. Brute-force the 8/16/24/32-bit polynomial space; tools like [CRC RevEng](https://reveng.sourceforge.io/) recover width, polynomial, init, reflection, and xorout from a handful of samples. Also try simple sums and XOR checksums (common in cheap 433 MHz sensors).
- **Whitening/scrambling**: test common LFSRs (PN9 $x^9+x^5+1$ used by CC1101 and 802.15.4g; BLE $x^7+x^4+1$ seeded with the channel index; 802.11 $x^7+x^4+1$). Descramble candidates, then score by entropy drop or CRC validity.
- **FEC identification**: rate-1/2 convolutional codes show parity-check structure found by rank-deficiency tests on sliding windows of the bit stream (the rank of the matrix of consecutive $n$-bit blocks drops when $n$ matches a multiple of the code's block structure). Common families: K=7 (171,133) conv codes (CCSDS, Meteor LRPT), Reed–Solomon (255,223), BCH(31,21) (POCSAG), Golay (DCS, P25 headers), P25 trellis 3/4 and BCH(63,16) NID.

### 7.5 How existing tools approach it

- **rtl_433** ([GitHub](https://github.com/merbanan/rtl_433); [ANALYZE.md](https://github.com/merbanan/rtl_433/blob/master/docs/ANALYZE.md)): detects pulses in OOK (adaptive envelope level) or FSK (two-frequency tracking), producing pulse/gap timing lists. Per-device decoders (**~386 protocols** as of the current README) run over the pulse data using modulation "slicers": OOK PCM/RZ, PPM, PWM, Manchester (MC_ZEROBIT), differential Manchester, PIWM, and FSK PCM/PWM/MC. The `-A` **pulse analyzer** histograms pulse and gap widths and proposes a slicer and timings. The **flex decoder** lets users define new protocols (modulation, short/long widths, reset limit, sync/preamble match, bit count) without code. Output goes to JSON, MQTT, InfluxDB, and others. **Lesson:** a time-domain pulse abstraction plus a slicer library plus a timing-histogram guesser covers most sub-GHz OOK/FSK devices cheaply.
- **Universal Radio Hacker (URH)** ([Pohl & Noack, WOOT 2018](https://www.usenix.org/system/files/conference/woot18/woot18-paper-pohl.pdf); [GitHub](https://github.com/jopohl/urh)): a full workflow of SDR capture, automatic detection of demodulation parameters (noise level, modulation type ASK/FSK/PSK, center/threshold, samples per symbol), customizable decodings (e.g., Manchester, whitening, bit inversion), a protocol analysis view with message alignment and field labeling, fuzzing, and simulation for stateful protocols. The authors' follow-up work on automatic wireless protocol reverse engineering (WOOT 2019) infers fields such as length, address, sequence number, and checksum by cross-message analysis. **Lesson:** blind RE is a *multi-message* problem: collect many bursts per emitter before inferring structure.
- **SatDump** ([docs](https://docs.satdump.org/)): a *pipeline* model in which each satellite/mode has a declarative pipeline (baseband → demodulator (e.g., PSK with specific symbol rate and RRC α) → soft symbols → deframer/FEC (Viterbi, RS, CCSDS) → frames → instrument decoders → products such as PNG/GeoTIFF). Pipelines can start at any level (baseband, soft symbols, frames). **Lesson:** make each stage an addressable artifact with standard formats so recordings can be re-processed later with better decoders.
- **Others**: dump1090/readsb (Mode S), AIS-catcher, multimon-ng (POCSAG/FLEX/AFSK/DTMF), rtlamr (Itron ERT SCM/IDM and Neptune R900; [rtlamr](https://github.com/bemasher/rtlamr)), acarsdec/dumpvdl2, radiosonde_auto_rx, gr-lora, SDRTrunk/OP25/trunk-recorder (§8), [inspectrum](https://github.com/miek/inspectrum) (manual burst measurement).

### 7.6 Protocol fingerprinting

Build a fingerprint per emission cluster and match against a signature DB:

```
{ band, f_c raster, OBW, modulation family, levels (2/4), deviation or constellation,
  symbol rate (±1%), line code, preamble length & pattern, sync word, packet length
  distribution, burst periodicity, TDMA frame period, CRC params, hop set }
```

Rules of thumb: symbol rate (to ±1%) plus deviation plus sync word identifies most LMR and ISM protocols uniquely. Periodicity plus length distribution separates sensor families. Keep fingerprints as **editable, shareable signatures**, a community "rtl_433 flex" generalized beyond OOK.

### 7.7 Blind reverse engineering of unknown protocols (practical loop)

1. Cluster bursts by fingerprint (DBSCAN on normalized features).
2. For a cluster: estimate modulation, symbol rate, deviation, and line code (§4, §7.2), and slice to bits.
3. Align messages (preamble/sync), compute per-bit-position entropy, and find constant fields.
4. Correlate candidate fields with side information (time, RSSI, user-entered sensor readings, device IDs printed on labels) to identify counters, IDs, and values.
5. Identify the checksum or CRC (RevEng); test whitening.
6. Emit a draft decoder (flex-style) and verify on held-out bursts via CRC pass rate.

---

## 8. Trunked radio systems

### 8.1 How trunking works

A trunked system shares a pool of RF channels among many **talkgroups**:

- **Control channel (CC)**: a site transmits a continuous outbound data stream (P25 Phase 1 TSBKs at 9600 bps; Motorola SmartNet 3600 bps; EDACS 9600 bps; DMR Tier III/Capacity Max control per ETSI TS 102 361-4; NXDN Type-C CAC). It carries registrations, affiliations, adjacent-site lists, system/site IDs, **channel identifier tables** (P25 `IDEN_UP`: base frequency + channel spacing + TX offset, so a 16-bit channel number maps to a frequency), and **voice channel grants** (e.g., P25 Group Voice Channel Grant with talkgroup, source unit ID, and channel).
- **Voice channels**: on a grant, radios in that talkgroup move to the assigned frequency (Phase 1 FDMA) or frequency plus TDMA slot (P25 Phase 2 / DMR). Voice lasts seconds, then the channel returns to the pool. **Grant updates** repeat on the CC for late entry.
- **Variants without a dedicated CC**: Motorola Capacity Plus (DMR "rest channel" signaling), NXDN Type-D (distributed), LTR (subaudible data on each channel).
- **Simulcast** systems transmit the same signal from multiple sites. Differential delay causes ISI, so P25 simulcast often uses CQPSK/LSM, which a C4FM discriminator demodulates poorly; use coherent QPSK demodulation with equalization.

### 8.2 Why wideband capture helps

A single-channel scanner must hop from CC to voice and back, missing concurrent calls and control updates. **Wideband capture of the whole system span** (e.g., an 800 MHz system spread over ~18 MHz of 851–869 MHz) lets one receiver:
- decode the CC continuously;
- demodulate **every active voice channel simultaneously** with channelizer outputs;
- record calls with complete metadata (talkgroup, radio ID, site, timestamps, encryption flags).

This is exactly the model of [trunk-recorder](https://github.com/TrunkRecorder/trunk-recorder), which "constantly listens to and decodes the control channel" and creates a recorder for each granted frequency from the SDR already capturing that spectrum. It supports P25 and SmartNet. Its maintainers note that **multiple smaller-bandwidth SDRs are often better than one very wide one** because of CPU cost and dynamic range. [SDRTrunk](https://github.com/DSheirer/sdrtrunk/wiki/) supports P25 Phase 1/2, DMR, NXDN, LTR, and MPT-1327 with automatic channel following. [OP25](https://github.com/boatbod/op25) is the GNU Radio reference for P25.

### 8.3 Encryption status

- P25 voice frames (HDU, LDU2) carry **ALGID** (0x80 = unencrypted; e.g., 0x81 DES-OFB, 0x84 AES-256, 0xAA ADP/RC4) and **Key ID**. Grants include a service-options byte with an encryption bit. The tool can therefore label and skip encrypted calls **without attempting decryption**, which matters legally (§1.3) and saves CPU.
- DMR has privacy indicators in the link control / PI headers; vendor "basic privacy" and ARC4/AES are flagged similarly.
- TETRA air-interface encryption (TEA1–4): the 2023 **TETRA:BURST** disclosure (CVE-2022-24401/24402 and others) showed TEA1 contains an intentional key-entropy reduction, making it brute-forceable on consumer hardware ([Midnight Blue](https://www.midnightblue.nl/research/tetraburst)). This is relevant for risk awareness, not a feature to implement.
- A public research reference on P25 security weaknesses and metadata leakage is Clark et al., "Why (Special Agent) Johnny (Still) Can't Encrypt" (IEEE S&P 2011).

### 8.4 Architecture for trunking support

```
Wideband SDR(s) -> PFB channelizer (12.5 kHz raster, 2x oversampled)
   -> CC hunter: scan candidate channels for continuous 100% duty + sync words (P25 FS / DMR sync / SmartNet 3600 bps)
   -> CC decoder (always on) -> system model: IDs, IDEN tables, neighbor sites, talkgroups
   -> Grant handler: allocate voice demod instance (C4FM/CQPSK/H-DQPSK/4FSK) on channel/slot
   -> Voice frames -> encryption check -> vocoder (IMBE / AMBE+2) -> audio + call metadata
   -> Recorder: per-call audio + JSON; optional IQ of CC for audit
```

Considerations:
- **Vocoders**: P25 Phase 1 uses IMBE; Phase 2, DMR, and NXDN use AMBE+2 variants. Open-source implementations exist (e.g., mbelib), but codec IP/licensing (DVSI) must be reviewed for a commercial product; hardware AMBE chips are the licensed path.
- **Talkgroup metadata** comes from RadioReference (§1.1) or is learned from CC traffic.
- **Compute**: one CC decoder is cheap. Each active voice channel is ~one 4FSK demod plus vocoder, a few % of a modern ARM core. The PFB over 20 MHz is the dominant fixed cost, a good fit for GPU or FPGA.
- **Auto-detection**: flag candidate CCs as continuous 4FSK/C4FM emissions with 100% FCO on the LMR raster, then confirm by frame sync and valid CRCs. Inter-channel correlation (§2 #14) confirms which frequencies belong to the system even before decoding.

---

## 9. Direction finding and geolocation

### 9.1 Techniques

| Method | Hardware | Principle | Typical accuracy / notes |
|---|---|---|---|
| **RSSI mapping / gradient** | 1 receiver + GPS | Log-distance path loss $P_r(d) = P_0 - 10n\log_{10}(d/d_0) + X_\sigma$, with $n\approx2$–4 and shadowing $\sigma\approx4$–10 dB | Coarse (tens to hundreds of m); needs many measurements; best "hot/cold" UX |
| **Directional antenna (Yagi) + body-null** | 1 receiver | Max/min of gain pattern | ~10–30°; simple fox hunting |
| **Pseudo-Doppler** | 1 receiver + commutated circular array | Rapid antenna switching imposes phase modulation at the commutation rate; tone phase vs switching reference gives bearing | Several degrees; susceptible to multipath; single-channel SDR friendly |
| **Watson-Watt (Adcock)** | 3-channel (N-S, E-W, sense) or switched | Amplitude comparison of orthogonal figure-8 patterns: $\theta = \operatorname{atan2}(V_{NS}, V_{EW})$ | ~2–3° RMS in good conditions; narrower frequency range; octantal error ([CRFS AoA whitepaper](https://pages.crfs.com/hubfs/whitepapers/Angle%20of%20Arrival-Direction%20Finding.pdf?hsLang=en)) |
| **Phase interferometry** | ≥2 coherent channels | $\Delta\phi = 2\pi d\sin\theta/\lambda$; $d\le\lambda/2$ avoids ambiguity | ~1–2° with calibration; requires phase-coherent receivers |
| **Beamforming / MVDR / MUSIC** | Coherent array (e.g., **KrakenSDR**: 5 coherent RTL-SDR channels with a shared clock and built-in noise-source calibration) | MUSIC: $\hat\theta = \arg\max 1/(\mathbf a^H(\theta)\mathbf V_n\mathbf V_n^H\mathbf a(\theta))$ | High resolution; number of elements must exceed number of sources; linear arrays have front/back ambiguity, so use a UCA ([PySDR DOA](https://pysdr.org/content/doa.html); [KrakenSDR docs](https://github.com/krakenrf/krakensdr_docs/wiki/)) |
| **TDOA** | ≥3 time-synchronized receivers | Hyperbolic multilateration; 1 ns timing error ≈ 30 cm range difference | GPS-disciplined or **reference-transmitter synchronization** (e.g., DAB/FM at known location) makes RTL-SDR TDOA feasible ([Panoradio TDOA](https://panoradio-sdr.de/tdoa-transmitter-localization-with-rtl-sdrs/), [code](https://github.com/DC9ST/tdoa-evaluation-rtlsdr)) |
| **FDOA** | Moving receivers | Doppler differences | Mostly airborne/space |

### 9.2 Portable device: GPS + RSSI mapping

A handheld explorer can offer "where is it?" without an array:
1. Log $(t, \text{lat}, \text{lon}, \text{heading}, \text{RSSI}_{burst})$ per detected burst from the target fingerprint. Use **burst peak** (not average) power, and median over a burst to reject fades.
2. Fit the emitter location $\mathbf p$ and path-loss parameters by nonlinear least squares or a **particle filter**: $\min_{\mathbf p, P_0, n}\sum_i (\text{RSSI}_i - P_0 + 10n\log_{10}\lVert\mathbf x_i-\mathbf p\rVert)^2$ with robust (Huber) loss. Show a posterior heatmap.
3. Improve geometry by prompting the user to walk a loop (diverse bearings). Near the emitter, switch on attenuation (§10.4) so RSSI keeps a gradient instead of saturating.
4. Optional add-ons: body-shielding "null" sweeps for a rough bearing; a 2-element phase interferometer or a KrakenSDR accessory for bearings, fused with the RSSI posterior (each bearing is a wedge likelihood).
5. Crowd/multi-session fusion for fixed emitters (e.g., ISM meters): accumulate across days.

---

## 10. Measurement and calibration

### 10.1 Frequency calibration

- **Error model**: $\delta f = \epsilon_{ppm}\cdot10^{-6}\cdot f_c$. At 1 GHz, 1 ppm = 1 kHz, which is fatal for 12.5 kHz LMR and narrow FSK. Typical TCXO SDRs are 0.5–2 ppm; uncompensated crystals are 10–50 ppm with temperature drift.
- **References** (auto-cal routine: find the reference, measure the offset, update the ppm, repeat on temperature change):
  - **GPSDO / GNSS 1PPS-disciplined clock**: best, and needed for coherent DF/TDOA.
  - **LTE**: PSS/SSS acquisition (Zadoff-Chu sequences) gives the cell carrier; base stations are held to ±0.05 ppm. [LTE-Cell-Scanner](https://github.com/Evrytania/LTE-Cell-Scanner) handles large initial offsets reliably ([calibration guide](https://medium.com/@rxseger/sdr-calibration-via-gsm-fcch-using-kalibrate-and-lte-cell-scanner-on-rtl-sdr-and-hackrf-193a7fb8a3eb)).
  - **GSM FCCH**: a pure tone at +67.708 kHz from the carrier ([kalibrate-rtl](https://github.com/steve-m/kalibrate-rtl); [Berkeley EE123 lab](https://inst.eecs.berkeley.edu/~ee123/sp15/lab/lab4/lab4-Frequency_Calibration_Using_GSM_BaseStations_Q.html)). GSM is largely decommissioned in the US, so treat it as an international fallback.
  - **FM stereo pilot** (19 kHz ±2 Hz relative to the station carrier; broadcast carriers have tight tolerances), **ATSC pilot**, **NOAA Weather Radio**, **WWV** carriers (HF), **ADS-B/AIS** symbol rates (for sample-clock ppm), and GNSS-derived timing.
- **Two separate errors**: LO frequency error (shifts the RF) and **sample-clock error** (scales symbol rates and timing). They share a crystal in most SDRs, so one ppm estimate usually fixes both. Verify with the symbol-timing loop's frequency term on a known-rate signal.

### 10.2 Power calibration: dBFS to dBm

- Measured digital power: $P_{dBFS} = 10\log_{10}\left(\frac{1}{N}\sum|x[n]|^2 / x_{FS}^2\right)$.
- Map to antenna-port power: $P_{dBm} = P_{dBFS} + K(f, G_{setting}, T)$. Build a calibration table from a signal generator (or a noise source with known ENR) over a frequency × gain grid and interpolate. Re-verify gain-state steps, which are often non-ideal on consumer SDRs.
- **Noise floor sanity check**: $N_{dBm} = -174 + NF_{dB} + 10\log_{10}(\text{RBW}_{Hz})$. For example, NF = 6 dB and RBW = 12.5 kHz gives −127 dBm.
- **PSD corrections**: divide bin power by ENBW (Hann: 1.5 bins = +1.76 dB) for density; correct window coherent gain for tones; for a signal wider than the RBW, integrate bins across the OBW.
- **Field strength**: $E_{dB\mu V/m} = P_{dBm} + 107 + AF_{dB/m}$ (50 Ω), with antenna factor $AF$ from the antenna calibration. This is needed only for regulatory-grade reports, but cheap to support.

### 10.3 Spur identification and removal

| Artifact | Signature | Mitigation |
|---|---|---|
| **DC spike / LO leakage** | Fixed at the tuned center (0 Hz baseband) | Offset tuning (tune ¼ IBW away and DDC back); DC-blocking IIR; interleaved sweeps as in hackrf_sweep |
| **IQ imbalance image** | Mirror of strong signal at $-f$ about center, typically −25…−40 dB | Blind correction (estimate $E[x^2]$ circularity and compensate); flag detections at mirror positions of strong signals as "image candidates" |
| **LO/clock harmonics, USB and DC-DC spurs** | Fixed absolute frequencies independent of antenna | Build a **spur map** with the antenna terminated (50 Ω load) across the tuning range; subtract or mask |
| **Aliasing** | Signals from outside the IBW folded in | Proper anti-alias filters; discard band edges (≥10% each side) |
| **Intermodulation** (IM3 at $2f_1-f_2$, $2f_2-f_1$) | Appear with strong in-band or out-of-band signals (FM broadcast, paging, cellular) | See §10.4 |

**Tests that separate real signals from spurs** (automate them):
1. **Retune test**: shift the LO by Δ and re-measure. Real signals stay at the same absolute RF; LO-related spurs and images move.
2. **Gain-step test**: change the front-end gain by X dB. Real signals change by X dB (until compression). **IM3 products change by ~3X dB**, and noise/spurs added after the gain stage change differently.
3. **Antenna-off test**: signals that persist with a terminated input are internal.
4. **Image test**: a detection at $2f_{LO}-f$ of a stronger signal with a correlated envelope is an image.

### 10.4 Dynamic range management

- Instantaneous dynamic range is bounded by ADC bits (~6.02 dB/bit: 8-bit RTL-SDR/HackRF ≈ 48 dB theoretical, fewer effective bits in practice; 12-bit ≈ 72 dB; 14–16-bit for high-end), by LNA/mixer IIP3, and by LO phase noise (reciprocal mixing).
- **Preselection**: tracking or switched bandpass filters; FM broadcast band-stop (88–108 MHz) and cellular/paging notches in urban deployments; separate antennas per band (e.g., the Opera Cake antenna switch mentioned by Ossmann & Spill).
- **Gain control strategy**: maximize sensitivity *subject to* no ADC clipping (track the peak sample magnitude; target ~−6 to −10 dBFS peak) and no IM (run the gain-step test periodically in dense bands). For a survey sweep, use per-band gain tables learned over time.
- **Attenuation near emitters** (DF, §9.2) and a front-end limiter to protect the LNA.
- **Clipping detection**: count full-scale samples per block. If >1e-4, reduce gain and mark those detections "suspect IM".

---

## 11. Professional spectrum-monitoring workflow and how it maps to a hobbyist tool

### 11.1 Professional practice

The [ITU Handbook on Spectrum Monitoring (2011 ed.)](https://itu-ilibrary.org/science-and-technology/spectrum-monitoring_pub/80399e8b-en), [Report ITU-R SM.2156](https://www.itu.int/dms_pub/itu-r/opb/rep/R-REP-SM.2156-2009-PDF-E.pdf) (role of monitoring), [Rec. ITU-R SM.2039](https://www.eett.gr/wp-content/uploads/2021/11/R-REC-SM.2039-0-201308-IPDF-E.pdf) (monitoring evolution), and the occupancy recommendations above describe national monitoring infrastructures of fixed, mobile, and transportable stations (plus remote unattended sensors) that:

1. **Search / scan** bands (sweep or real-time) against schedules.
2. **Detect** emissions (thresholds per SM.2256).
3. **Compare with the license database** (known / licensed / unlicensed / unknown).
4. **Measure technical parameters**: frequency, field strength or power flux density, bandwidth (SM.443), modulation parameters (SM.1682 covers digital signal measurements), occupancy (SM.1880).
5. **Classify and identify**: modulation and protocol, decoding of station identification where lawful.
6. **Locate**: DF with triangulation across stations, TDOA networks, mobile homing.
7. **Log and database** emissions, with standardized data exchange (ITU-R SM.1809 covers data exchange formats for monitoring measurements).
8. **Report and enforce**: interference resolution, compliance, planning input.

Modern commercial systems (e.g., R&S ARGUS-type stations, CRFS RFeye nodes) automate 1–4 with alarm masks ("emission not in the license DB" or "level above mask"), and route 5–7 to operators.

### 11.2 Mapping to an exploration device

| Professional step | Hobbyist exploration equivalent | Implementation |
|---|---|---|
| Scheduled band scan | Continuous discovery sweep plus adaptive dwell | §3.8 scheduler; bandit-based revisit |
| Detection vs threshold | Automatic noise floor + OS-CFAR + SK | §3.2–3.4 |
| License DB comparison | Priors from §2.106 + ULS emission designators + RadioReference + learned baseline | §1.1 Bayesian prior; "unexpected here" badge |
| Technical measurement | Auto OBW, CFO, SNR, symbol rate, deviation | §4 |
| Classification | Cascaded AMC + open-set + decoders | §5–7 |
| DF / localization | RSSI walk-mapping; optional coherent add-on | §9 |
| Logging / database | Local emission DB (SQLite/Parquet), SigMF IQ snippets, timelines, per-channel occupancy | Each emission: first/last seen, count, fingerprint, decoded identity, confidence, recordings |
| Reporting / alarms | Notifications: "new emitter", "channel busier than usual", "decoder available", "encrypted", "unknown signal recorded" | Novelty score (§2) |
| Operator analysis | Guided "drill-down": spectrogram, constellation/eye, IF histogram, cyclic spectrum, bit view | Inspectrum/URH-like views bound to the burst record |

Design principles derived from professional practice:
- **Separate measurement from interpretation.** Store raw burst records and IQ; re-run classifiers and decoders as they improve (the SatDump lesson).
- **Everything carries a confidence and provenance** (which algorithm, which prior).
- **Baseline first.** Alarms are only meaningful against a learned local baseline (24 h+ per SM.1880).

---

## 12. Takeaways: prioritized implementation list

Ordered by value to an exploration device versus effort. "Compute" assumes an ARM SoC with optional GPU (Jetson-class). "Robustness" is qualitative field robustness.

| # | Technique | Why it matters | Compute | Robustness |
|---|---|---|---|---|
| 1 | **Welch PSD + persistence histogram + spectral kurtosis accumulators** | Base of display, detection, burst hints | Low (FFT) | High |
| 2 | **Robust noise floor (FCME / percentile + minimum statistics per bin)** | Every threshold depends on it; avoids SNR-wall surprises | Very low | High |
| 3 | **OS-CFAR detection in frequency + 2-D (time-freq) CFAR + hysteresis → burst records** | Turns spectra into emissions | Low | High |
| 4 | **Sweep/dwell scheduler with POI-aware revisit and interestingness score** | Finds activity efficiently across 6 GHz | Low | High (depends on hardware retune speed) |
| 5 | **Channelizer (2× oversampled PFB) + on-demand DDC** | Parallel demod/decode of many channels (trunking, ISM) | Medium (GPU/FPGA friendly) | High |
| 6 | **Automatic frequency calibration (LTE PSS / FM pilot / GPS) + spur map + retune/gain-step spur tests** | Correct rasters, fewer false "interesting" spurs | Low (periodic) | High |
| 7 | **Parameter estimators: 99% OBW, CFO, spectral + M2M4 SNR, symbol rate via envelope/delay-multiply lines, FSK deviation histogram** | Fingerprints, and normalization for classifiers | Low | Medium–high (symbol rate weak for small roll-off) |
| 8 | **Priors: §2.106 band table + ULS emission designators + optional RadioReference** | Large accuracy gain for near-zero compute | Negligible | High, but don't let priors veto evidence |
| 9 | **Analog auto-mode (Azzouz–Nandi-style features) + SNR/noise squelch + CTCSS/DCS + WFM pilot/RDS** | "Never pick FM/AM" UX goal | Low | High above ~10 dB SNR; SSB carrier inference is the hard part |
| 10 | **rtl_433-style pulse abstraction + slicers + timing-histogram analyzer + flex signatures** | Covers hundreds of ISM devices | Low | High for OOK/FSK sub-GHz |
| 11 | **Blind digital demod chain: discriminator/envelope, Gardner or PFB clock sync, Costas, preamble/sync search, CRC RevEng, LFSR tests** | Unknown protocol exploration | Medium | Medium (burst length, SNR) |
| 12 | **Integrate proven decoders** (ADS-B, AIS, ACARS/VDL2, POCSAG/FLEX metadata subject to legal constraints, APRS, radiosondes, LRPT, ERT, LoRa, BLE adverts) | Immediate, verifiable identity, and ground-truth labels | Low–medium | High |
| 13 | **Trunking: CC detection + P25/DMR/NXDN CC decode + grant following + encryption flags** | Most-requested scanner capability; needs wideband | Medium (fixed PFB cost + per-call) | High for P25 Phase 1 C4FM; simulcast LSM needs coherent demod |
| 14 | **Cumulant-based digital classifier (Swami–Sadler) + cyclic-feature checks** | Cheap, interpretable modulation order/family | Low (cumulants); medium (cyclic) | Medium–high; insensitive to AWGN and CFO |
| 15 | **Small DL classifier per family on normalized (resampled, CFO-corrected) snippets, INT8/TensorRT, with open-set scoring; fine-tuned on own-hardware captures** | Beats features at low SNR and on many classes | Low–medium on GPU | Medium; sim-to-real and open-set are the main risks |
| 16 | **OFDM CP-correlation analyzer; DSSS autocorrelation detector** | Labels LTE/NR/Wi-Fi/DVB and below-noise signals | Low–medium | High for OFDM with CP; DSSS moderate |
| 17 | **Hop tracker and TDMA frame-period estimator** | Bluetooth/FHSS and TDMA identification | Medium | Medium (limited by IBW) |
| 18 | **Occupancy statistics per ITU SM.1880/SM.2256 (FCO/FBO/SRO) with 24×7 baselines and novelty alarms** | "What changed?" | Very low | High |
| 19 | **RSSI + GPS emitter localization (particle filter), optional coherent DF add-on** | "Where is it?" | Low | Low–medium (multipath); good for homing |
| 20 | **Spectrogram object detection (YOLO/DETR-class) for dense bands (2.4/5.8 GHz)** | Overlapping and complex scenes | Medium–high (GPU) | Medium until trained on real captures |
| 21 | **Full cyclostationary analyzer (SSCA/FAM + coherence) on demand** | Hard or unknown signals, low SNR, precise $R_s$/$f_c$ | High | High (theory-grounded, robust to impairments) |

**Bottom line.** The most valuable parts are *not* deep learning: robust noise-floor estimation and CFAR, a smart sweep/dwell scheduler, calibration and spur rejection, parameter estimation, and priors from licensing data together deliver most of the "find interesting things automatically" experience. Classical features and existing decoders then label a large fraction of real-world emissions with verifiable ground truth. DL belongs in a cascaded, normalized, open-set-aware stage, fine-tuned on the device's own captures, and fed by decoders that provide labels for continual improvement.

---

## 13. Sources / References

**Regulatory and allocation data**
- ITU-R Recommendation SM.1880-2 (2017), *Spectrum occupancy measurement and evaluation*: https://www.itu.int/rec/R-REC-SM.1880-2-201709-I/en
- ITU-R Report SM.2256-1 (2016), *Spectrum occupancy measurements and evaluation*: https://www.itu.int/dms_pub/itu-r/opb/rep/R-REP-SM.2256-1-2016-PDF-E.pdf (2012 ed.: https://www.itu.int/dms_pub/itu-r/opb/rep/r-rep-sm.2256-2012-pdf-e.pdf)
- ITU-R SM.328-12 (2025), *Spectra and bandwidth of emissions*: https://www.itu.int/dms_pubrec/itu-r/rec/sm/R-REC-SM.328-12-202509-I!!PDF-E.pdf
- ITU-R SM.443-4 (2007), *Bandwidth measurement at monitoring stations*: https://www.itu.int/dms_pubrec/itu-r/rec/sm/R-REC-SM.443-4-200702-I!!PDF-E.pdf
- ITU-R SM.1682-1, *Methods for measurements on digital broadcasting signals*: https://www.itu.int/dms_pubrec/itu-r/rec/sm/R-REC-SM.1682-1-201109-I!!PDF-E.pdf
- ITU-R SM.1046-3, *Definition of spectrum use and efficiency*: https://www.itu.int/dms_pubrec/itu-r/rec/sm/R-REC-SM.1046-3-201709-I!!PDF-E.pdf
- ITU-R Report SM.2156, *The role of spectrum monitoring*: https://www.itu.int/dms_pub/itu-r/opb/rep/R-REP-SM.2156-2009-PDF-E.pdf
- ITU-R SM.2039, *Spectrum monitoring evolution*: https://www.eett.gr/wp-content/uploads/2021/11/R-REC-SM.2039-0-201308-IPDF-E.pdf
- ITU Handbook on Spectrum Monitoring (2011): https://itu-ilibrary.org/science-and-technology/spectrum-monitoring_pub/80399e8b-en
- 47 CFR §2.106, Table of Frequency Allocations (eCFR): https://www.ecfr.gov/current/title-47/chapter-I/subchapter-A/part-2/subpart-B/section-2.106
- NTIA United States Frequency Allocation Chart: https://www.ntia.gov/page/united-states-frequency-allocation-chart
- NTIA Manual (Redbook) Annex J, necessary bandwidth: https://www.ntia.gov/sites/default/files/2023-11/j_2021_edition_rev_2023.pdf
- FCC ULS Public Access Files: https://www.fcc.gov/wireless/data/public-access-files-database-downloads
- RadioReference Database Web Service API: https://support.radioreference.com/hc/en-us/articles/18844460198932-Database-Web-Service-API ; wiki: https://wiki.radioreference.com/index.php/RadioReference.com_Web_Service
- Signal Identification Wiki: https://www.sigidwiki.com/wiki/Signal_Identification_Guide ; Artemis: https://aresvalley.github.io/Artemis/database/sigid/
- 18 U.S.C. §2511: https://www.law.cornell.edu/uscode/text/18/2511 ; §2510: https://www.law.cornell.edu/uscode/text/18/2510
- 47 U.S.C. §605: https://uscode.house.gov/view.xhtml?req=%2247+USC+605%22
- 47 CFR §15.121: https://www.law.cornell.edu/cfr/text/47/15.121
- NOAA NESDIS, POES decommissioning: https://www.nesdis.noaa.gov/news/legacy-orbit-noaa-decommissions-the-poes-satellite-constellation ; OSPO message: https://www.ospo.noaa.gov/data/messages/2025/08/MSG_20250820_1410.html
- Meteor-M LRPT (sigidwiki): https://www.sigidwiki.com/wiki/Low_Rate_Picture_Transmission_(LRPT) ; WMO OSCAR: https://space.oscar.wmo.int/satellites/view/meteor_m_n2_3
- NWS radiosonde program FAQ: https://www.weather.gov/upperair/faq ; RS41 (sigidwiki): https://www.sigidwiki.com/wiki/Vaisala_RS41-SG_Weather_Balloon_(Radiosonde)

**Occupancy studies**
- NTIA/ITS spectrum survey reports: https://its.ntia.gov/about-its/archive/2014/spectrum-survey-report-series ; San Diego TR-14-498: https://its.ntia.gov/publications/details?pub=2741 ; TR-20-548: https://its.ntia.gov/publications/download/TR-20-548.pdf ; TR-14-500: https://ntia.gov/report/2014/spectrum-occupancy-measurements-3550-3650-megahertz-maritime-radar-band-near-san-diego
- McHenry et al., *Spectrum Occupancy Measurements, Chicago, Nov 2005* (Shared Spectrum Co.): https://www.sharedspectrum.com/wp-content/uploads/NSF_Chicago_2005-11_measurements_v12.pdf

**Detection, PSD, noise-floor estimation**
- P. D. Welch, "The use of FFT for the estimation of power spectra," *IEEE Trans. Audio Electroacoust.*, 1967.
- D. J. Thomson, "Spectrum estimation and harmonic analysis," *Proc. IEEE*, 70(9), 1982.
- J. Antoni, "The spectral kurtosis: a useful tool for characterising non-stationary signals," *MSSP*, 2006; G. Nita & D. Gary, "The generalized spectral kurtosis estimator," *MNRAS Letters*, 2010: https://arxiv.org/abs/1005.4371 ; EOVSA SK correlator: https://arxiv.org/pdf/1702.05391
- R. Martin, "Noise PSD estimation based on optimal smoothing and minimum statistics," *IEEE TSAP*, 9(5), 2001: https://www.researchgate.net/publication/3333805
- J. Vartiainen et al., "Analysis of the consecutive mean excision algorithms," *JECE*, 2010: https://onlinelibrary.wiley.com/doi/10.1155/2010/459623
- H. Urkowitz, "Energy detection of unknown deterministic signals," *Proc. IEEE*, 1967.
- R. Tandra & A. Sahai, "SNR walls for signal detection," *IEEE JSTSP*, 2(1), 2008.
- T. Yücek & H. Arslan, "A survey of spectrum sensing algorithms for cognitive radio applications," *IEEE COMST*, 2009: https://span.ece.utah.edu/uploads/yucek09-spectrum-sensing-algs-cr.pdf
- H. Rohling, "Radar CFAR thresholding in clutter and multiple target situations," *IEEE TAES*, 1983: https://ieeexplore.ieee.org/document/4102829/
- M. A. Richards, *Fundamentals of Radar Signal Processing*, McGraw-Hill.
- S. M. Kay, *Fundamentals of Statistical Signal Processing, Vol. II: Detection Theory*, Prentice Hall, 1998.
- F. Auger & P. Flandrin, "Improving the readability of time-frequency and time-scale representations by the reassignment method," *IEEE TSP*, 1995.
- M. Ossmann & D. Spill, "What's on the Wireless? Automating RF Signal Identification," Black Hat USA 2017: https://blackhat.com/docs/us-17/wednesday/us-17-Ossmann-Whats-On-The-Wireless-Automating-RF-Signal-Identification-wp.pdf ; HackRF tools: https://hackrf.readthedocs.io/en/latest/hackrf_tools.html
- Agilent/Keysight AN 1318, *Optimizing Spectrum Analyzer Measurement Speed*: https://anlage.umd.edu/5968-3411E.pdf
- Tektronix RSA datasheets (DPX POI): https://www.tek.com/en/datasheet/spectrum-analyzers-datasheet ; https://www.tek.com/en/datasheet/spectrum-analyzer-1

**Multirate, synchronization, parameter estimation**
- f. j. harris, *Multirate Signal Processing for Communication Systems*, Prentice Hall (2004; 2nd ed. River Publishers 2021); harris, Dick & Rice, "Digital receivers and transmitters using polyphase filter banks," *IEEE T-MTT*, 51(4), 2003.
- f. j. harris & M. Rice, "Multirate digital filters for symbol timing synchronization in software defined radios," *IEEE JSAC*, 19(12), 2001.
- M. Rice, *Digital Communications: A Discrete-Time Approach*, Pearson, 2009.
- R. G. Lyons, *Understanding Digital Signal Processing*, 3rd ed., Pearson, 2010; dsprelated.com articles.
- J. G. Proakis & M. Salehi, *Digital Communications*, 5th ed., McGraw-Hill, 2008.
- K. Mueller & M. Müller, "Timing recovery in digital synchronous data receivers," *IEEE Trans. Commun.*, 1976.
- F. M. Gardner, "A BPSK/QPSK timing-error detector for sampled receivers," *IEEE Trans. Commun.*, 34(5), 1986: https://ui.adsabs.harvard.edu/abs/1986ITCom..34..423G/abstract
- PySDR (M. Lichtman): Synchronization https://pysdr.org/content/sync.html ; DOA https://pysdr.org/content/doa.html
- D. R. Pauluzzi & N. C. Beaulieu, "A comparison of SNR estimation techniques for the AWGN channel," *IEEE Trans. Commun.*, 48(10), 2000; GNU Radio M2M4: https://www.gnuradio.org/doc/doxygen/classgr_1_1digital_1_1mpsk__snr__est__m2m4.html
- J.-J. van de Beek, M. Sandell, P. O. Börjesson, "ML estimation of time and frequency offset in OFDM systems," *IEEE TSP*, 45(7), 1997: https://portal.research.lu.se/en/publications/ml-estimation-of-time-and-frequency-offset-in-ofdm-systems/

**Cyclostationarity**
- W. A. Gardner, "Exploitation of spectral redundancy in cyclostationary signals," *IEEE SP Magazine*, 1991.
- R. S. Roberts, W. A. Brown, H. H. Loomis, "Computationally efficient algorithms for cyclic spectral analysis," *IEEE SP Magazine*, 1991.
- C. M. Spooner, Cyclostationary Signal Processing blog: https://cyclostationary.blog/ ; SSCA: https://cyclostationary.blog/2016/03/22/csp-estimators-the-strip-spectral-correlation-analyzer/ ; FAM: https://cyclostationary.blog/2018/06/01/csp-estimators-the-fft-accumulation-method/ ; RML dataset critiques: https://cyclostationary.blog/2020/08/17/more-on-deepsigs-rml-data-sets/ and https://cyclostationary.blog/2017/01/31/machine-learning-and-modulation-recognition-comments-on-convolutional-radio-modulation-recognition-networks-by-t-oshea-j-corgan-and-t-clancy/ ; CSP + DL: https://cyclostationary.blog/2023/06/20/latest-paper-on-csp-and-deep-learning-for-modulation-recognition-an-extended-version-of-my-papers-52/

**AMC**
- O. A. Dobre, A. Abdi, Y. Bar-Ness, W. Su, "Survey of automatic modulation classification techniques: classical approaches and new trends," *IET Communications*, 1(2), 2007: https://web.njit.edu/~abdi/IEE_COM0176_WithFigures.pdf
- E. E. Azzouz & A. K. Nandi, *Automatic Modulation Recognition of Communication Signals*, Kluwer, 1996; A. K. Nandi & E. E. Azzouz, "Algorithms for automatic modulation recognition of communication signals," *IEEE Trans. Commun.*, 46(4), 1998.
- A. Swami & B. M. Sadler, "Hierarchical digital modulation classification using cumulants," *IEEE Trans. Commun.*, 48(3), 2000.
- J. L. Xu, W. Su, M. Zhou, "Likelihood-ratio approaches to automatic modulation classification," *IEEE Trans. SMC-C*, 41(4), 2011.
- T. O'Shea, J. Corgan, T. C. Clancy, "Convolutional radio modulation recognition networks," 2016: https://arxiv.org/abs/1602.04105
- T. O'Shea, T. Roy, T. C. Clancy, "Over-the-air deep learning based radio signal classification," *IEEE JSTSP*, 12(1), 2018: https://arxiv.org/abs/1712.04578
- S. Rajendran et al., "Deep learning models for wireless signal classification with distributed low-cost spectrum sensors," *IEEE TCCN*, 2018.
- L. Boegner et al., "Large scale radio frequency signal classification" (Sig53/TorchSig), 2022: https://arxiv.org/abs/2207.09918 ; "Large scale RF wideband signal detection & recognition" (WBSig53): https://arxiv.org/abs/2211.10335 ; TorchSig GRCon 2024: https://events.gnuradio.org/event/24/contributions/628/attachments/190/473/TorchSig_GRCon2024_paper.pdf
- L. Wong et al., "An analysis of RF transfer learning behavior using synthetic data": https://arxiv.org/abs/2210.01158
- Open-set AMC: https://arxiv.org/pdf/2002.12037 ; https://arxiv.org/pdf/2302.03749 ; https://arxiv.org/html/2410.10265
- Edge AMC: https://arxiv.org/pdf/2404.15343 ; Binarized ResNet AMC: https://arxiv.org/pdf/2110.14357 ; TensorRT quantization on Jetson (Zhou et al., MCSoC 2023): https://userweb.cs.txstate.edu/~k_y47/webpage/pubs/mcsoc23.pdf
- Wireless foundation models: IQFM https://arxiv.org/pdf/2506.06718 ; SpectrumFM https://arxiv.org/pdf/2505.06256 ; Multimodal WFM https://arxiv.org/abs/2511.15162
- Recent AMC surveys: https://arxiv.org/pdf/2502.05315 ; https://arxiv.org/pdf/2503.08091

**Protocols, decoders, tools**
- rtl_433: https://github.com/merbanan/rtl_433 ; analysis guide: https://github.com/merbanan/rtl_433/blob/master/docs/ANALYZE.md
- J. Pohl & A. Noack, "Universal Radio Hacker: A suite for analyzing and attacking stateful wireless protocols," WOOT 2018: https://www.usenix.org/system/files/conference/woot18/woot18-paper-pohl.pdf ; URH: https://github.com/jopohl/urh
- SatDump documentation: https://docs.satdump.org/ ; https://github.com/SatDump/SatDump
- rtlamr: https://github.com/bemasher/rtlamr
- CRC RevEng: https://reveng.sourceforge.io/
- SigMF: https://github.com/sigmf/SigMF
- DCS (sigidwiki): https://www.sigidwiki.com/wiki/Digital-Coded_Squelch_(DCS) ; CTCSS: https://en.wikipedia.org/wiki/Continuous_Tone-Coded_Squelch_System
- Squelch overview: http://wiki.hamtools.org/index.php?title=Squelch ; noise-squelch patent US4359780: https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/4359780

**Trunking**
- trunk-recorder: https://github.com/TrunkRecorder/trunk-recorder
- SDRTrunk wiki: https://github.com/DSheirer/sdrtrunk/wiki/
- RadioReference wiki, APCO Project 25: https://wiki.radioreference.com/index.php/APCO_Project_25 ; Phase 2: https://wiki.radioreference.com/index.php/Phase_2
- Midnight Blue, TETRA:BURST: https://www.midnightblue.nl/research/tetraburst
- Standards: TIA-102 series (P25); ETSI TS 102 361-1..4 (DMR); ETSI EN 300 392 (TETRA); NXDN Technical Specifications (NXDN Forum).

**Direction finding and calibration**
- KrakenSDR docs: https://github.com/krakenrf/krakensdr_docs/wiki/ ; product: https://www.krakenrf.com/product-page/krakensdr
- A. Edge (CRFS), *Angle of Arrival / Direction Finding Techniques*: https://pages.crfs.com/hubfs/whitepapers/Angle%20of%20Arrival-Direction%20Finding.pdf?hsLang=en
- DTIC, *Review of Conventional Tactical Radio Direction Finding Systems*: https://apps.dtic.mil/sti/tr/pdf/ADA212747.pdf
- Panoradio, TDOA with RTL-SDRs: https://panoradio-sdr.de/tdoa-transmitter-localization-with-rtl-sdrs/ ; code: https://github.com/DC9ST/tdoa-evaluation-rtlsdr
- kalibrate-rtl: https://github.com/steve-m/kalibrate-rtl ; GSM/LTE calibration walkthrough: https://medium.com/@rxseger/sdr-calibration-via-gsm-fcch-using-kalibrate-and-lte-cell-scanner-on-rtl-sdr-and-hackrf-193a7fb8a3eb ; UC Berkeley EE123 lab: https://inst.eecs.berkeley.edu/~ee123/sp15/lab/lab4/lab4-Frequency_Calibration_Using_GSM_BaseStations_Q.html
