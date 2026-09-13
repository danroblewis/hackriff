# 06 — Capability map

*Architecture planning, Phase 1. Drafted 2026-09-13. Status: **taxonomy approved by the user (39 capabilities, frozen)**; all 398 use cases mapped in `use-cases.yaml`; coverage analysis (§4) and card-feedback resolutions (§5) complete. Later ADRs finalise the items marked provisional in §5.*

This document turns the research in docs 01–05 into a fixed vocabulary of engineering building blocks, then maps every use case in `use-cases.yaml` onto that vocabulary. The point is to find the **shared core**: the few capabilities that unlock most of the 391 use cases, so the roadmap (doc 11) builds those first.

## 1. How to read the taxonomy

- Each capability has a stable slug (used in `use-cases.yaml → capabilities`) and a `Cnn` number for tables.
- Capabilities are grouped in seven layers that follow the product workflow in `CLAUDE.md`: acquire → sense → characterize → demodulate/decode → remember → explain → specialised.
- **Compute cost** is a rough order of magnitude on the target platform (Jetson Orin Nano class, HackRF at 20 Msps). "Per channel" means the cost scales with the number of signals being processed at once; "per event" means it scales with detections per second, not with bandwidth. Numbers come from doc 02 §3 and doc 04 §5.5 where they exist and are engineering estimates otherwise.
- **Reuse** names existing projects the docs identified as worth wrapping or borrowing from (doc 03). Licences are tracked in the Phase 3 ledger, not here.
- Four capabilities form the **substrate** every live use case implicitly needs: `source-abstraction` (C01), `dwell-capture` (C03), `spectral-estimation` (C07) and `live-view-inspector` (C39). The mapping in §3 does **not** list them unless a use case is specifically about them; otherwise they would top every coverage table without saying anything.

## 2. Capability taxonomy (39 capabilities)

### Layer A — Acquire

| ID | Capability | What it does | Inputs → Outputs | Compute | Reuse / notes |
|---|---|---|---|---|---|
| C01 | `source-abstraction` | Uniform driver layer for the RF front end and other sample sources: HackRF One/Pro (libhackrf, incl. firmware sweep mode), later SoapySDR devices, a soundcard for VLF/ELF, and SigMF file replay. Controls tune, sample rate, LNA/VGA/amp, bias-tee, Opera Cake port/filter switching and clock source. Attaches **provenance** to every sample block: gain state, ADC clip count, temperature, active filter, clock lock. | Control commands → timestamped IQ blocks + provenance record | Negligible CPU; USB 2.0 ingest at 40 MB/s is the constraint (doc 01 §1.4) | libhackrf, SoapySDR. Provenance is what lets later stages mark detections "suspect IMD" (doc 04 §10.4). File replay is also the basis of offline tests. |
| C02 | `sweep-survey` | Wideband power-spectrum survey using firmware-driven retuning (`hackrf_init_sweep`), ~8 GHz/s, 5 MHz slices interleaved around the DC spike. Produces SweepFrames: rows of dB-per-bin over a configured span, with per-slice provenance and filter-bank state. | Sweep plan (span, bin width, gain table, filter map) → SweepFrames | Low–medium: continuous 20 Msps FFT on host, ~1 core | `hackrf_sweep` logic, `hackrf_sweeper` library; Opera Cake frequency mode for per-band filters/antennas (doc 01 §1.5). Finds *where*, cannot see bursts <~few ms (doc 04 §3.8). |
| C03 | `dwell-capture` | Real-time capture of one ≤20 MHz window into a RAM ring buffer with sample-accurate timestamps, so a detection can trigger recording that includes **pre-trigger** history. Exposes the window to the channelizer and to on-demand DDCs. | Tune request → continuous IQ window + ring buffer with time index | Memory-bound: 40 MB/s; a 30 s pre-trigger buffer is 1.2 GB | Signal Hunter's 8 ms pre-trigger and SDRangel's SigMF sink pre-trigger are the precedents (docs 01 §3.4, 03 §2.2). Finds *what*. |
| C04 | `attention-scheduler` | Decides where the one half-duplex window points: alternates discovery sweeps with dwells on candidate regions, sized by expected burst intervals and probability-of-intercept, with priorities from user intent (scan plans, "watch this"), novelty scores and decoder demand. Bandit-style revisit. | Scan plans, occupancy/novelty stats, active demod requests → tune schedule and dwell records | Negligible | Doc 04 §3.8 scheduler sketch; ITU SM.1880 revisit rules. The single hardest design problem for a one-radio device. |
| C05 | `calibration` | Frequency calibration (ppm from LTE PSS, FM 19 kHz pilot, WWV, GNSS), power calibration table dBFS→dBm per frequency×gain, **spur map** measured with a terminated input, IQ-image flagging, and the retune/gain-step tests that separate real signals from intermodulation. Maintains a per-band gain table that avoids ADC clipping. | IQ + provenance, reference signals → calibration state, spur mask, "suspect" flags on detections | Low, periodic | Doc 04 §10. kalibrate/LTE-Cell-Scanner ideas. Without this the inventory fills with ghosts in a city (doc 02 §1.7). |
| C06 | `position-time` | Device position, heading, altitude and precise time from an on-board GNSS receiver (and IMU/compass if fitted); optional 1PPS discipline of the HackRF clock. Every record can be geotagged. | GNSS/IMU hardware → position/time/heading stream | Negligible | Needed for RSSI mapping, satellite geometry, licence lookups by location and timestamps that match external feeds. |

### Layer B — Sense

| ID | Capability | What it does | Inputs → Outputs | Compute | Reuse / notes |
|---|---|---|---|---|---|
| C07 | `spectral-estimation` | Welch PSD and multi-resolution STFT of the live window (e.g. 1 kHz and 25 Hz bins in parallel), a DPX-style persistence histogram, and per-bin spectral-kurtosis accumulators. The common input for display, detection and radiometry. | IQ window → SpectrumFrames (PSD rows, persistence, SK) | Medium: at 20 Msps a 4096-point FFT every 205 µs ≈ 4.9k FFT/s, one CPU core or a small fraction of the GPU (doc 02 §3.2) | cuFFT/CuPy on the Jetson; VOLK/liquid on CPU. Doc 04 §3.1, §3.5. |
| C08 | `noise-floor` | Robust noise-floor estimate per bin and per channel: FCME/percentile across frequency for the frame, minimum statistics over time, and per-channel idle-time measurement. Supplies every threshold with a 3–5 dB guard margin. | SpectrumFrames → NoiseFloor estimates (per bin, per channel, with uncertainty) | Very low | Doc 04 §3.2. Also the science-grade "noise floor vs time" measurement itself (SPACE-050, AWARE-031). |
| C09 | `cfar-detection` | Ordered-statistic CFAR across frequency plus 2-D CFAR on the spectrogram, hysteresis and minimum-duration filters, connected components into time×frequency boxes. Emits **Detection** records with SNR, extent, SK, and provenance flags (clipping, spur-mask hit, image candidate). | SpectrumFrames + NoiseFloor + spur mask → Detections (burst records) | Low | Doc 04 §3.3–3.6. Works on both sweep rows (coarse, persistent emitters) and dwell spectrograms (bursts). |
| C10 | `burst-tracking` | Links Detections over time into tracks and emissions: same centre/bandwidth → one emitter; computes burst periodicity, duty cycle, inter-arrival statistics, hop sets and rates, TDMA frame period, inter-channel co-occurrence (repeater pairs, trunk grants). Clusters emissions by fingerprint. | Detections → Tracks/Emissions with timing features | Low–medium | Doc 04 §2 features 4–7, 14; §4.7. Feeds the inventory and the scheduler's "expected next burst". |
| C11 | `channelizer` | 2× oversampled polyphase filter-bank channelizer over the dwell window (e.g. 12.5/25/100 kHz rasters) plus on-demand DDCs for arbitrary centre/bandwidth. Delivers many narrowband streams at once to demodulators and decoders. | IQ window + channel requests → N channel streams | Medium; GPU/NEON friendly, the dominant fixed cost for trunking and dense ISM bands (doc 04 §8.4) | Doc 04 §3.7. cuSignal-in-CuPy or a custom CUDA PFB; liquid-dsp for DDCs. |
| C12 | `occupancy-baseline` | ITU-style occupancy statistics (FCO/FBO/SRO) per channel and band, hour-of-week baselines, novelty and anomaly scores against the learned baseline, "interestingness" ranking of channels. | Detections, Tracks, spectrum history → Occupancy stats, novelty scores, ranked candidates | Very low | Doc 04 §2 score, §3.9; SM.1880/SM.2256. The engine behind "what changed?" and behind scheduler priorities. |

### Layer C — Characterize

| ID | Capability | What it does | Inputs → Outputs | Compute | Reuse / notes |
|---|---|---|---|---|---|
| C13 | `param-estimation` | Per-emission measurements on a channelized snippet: 99% occupied bandwidth and x-dB bandwidth, carrier offset (centroid, power-of-M, pilot interpolation), SNR (spectral and M2M4), spectral shape features, carrier presence, spectral symmetry, analog-versus-digital tests, offset from the nearest channel raster. | Channel snippet → Parameter set with confidences | Low, per event | Doc 04 §4.1–4.3, §4.10. These normalise the input for every classifier and demodulator. |
| C14 | `blind-symbol-estimation` | Symbol rate (envelope and delay-multiply spectral lines, Haar wavelet, run-length histograms), FSK deviation and modulation index from the instantaneous-frequency histogram, modulation order, roll-off, GFSK BT; full cyclostationary analysis (SSCA/FAM) on demand for hard cases. | Channel snippet (+ modulation family guess) → symbol rate, deviation, order, roll-off, cycle frequencies | Low for the spectral-line methods; **high** for SSCA (run only on demand) | Doc 04 §4.4–4.6. SigDigger/Suscan and URH estimators are the reference implementations (doc 03 §3.4). A named spike in Phase 4. |
| C15 | `modulation-classifier` | Cascaded classifier: fast feature tree (Azzouz–Nandi, cumulants, cyclic-feature checks) → modulation family (analog, OOK/ASK, FSK, PSK/QAM, OFDM, CSS, DSSS, pulsed, noise-like) → optional per-family DL model on normalised snippets → **open-set "unknown" score**. Fuses with priors from C17. | Normalised snippet + parameters + priors → class distribution incl. `unknown`, with provenance | Low on CPU for features; ms per event on GPU for DL | Doc 04 §5. TorchSig for synthetic training data; classical features first, DL after normalisation (CLAUDE.md key findings). |
| C16 | `ofdm-dsss-analysis` | OFDM parameter estimation from cyclic-prefix correlation (subcarrier spacing, CP length, symbol period, fractional CFO, active subcarriers) and DSSS detection via autocorrelation/covariance and chip-rate cyclic features; known-code correlation for identification. | Channel snippet → OFDM/DSSS parameters or "none" | Low–medium, per event | Doc 04 §4.8–4.9. Labels LTE/NR/Wi-Fi/DVB/DAB and finds below-noise spread signals. |
| C17 | `known-signal-priors` | Offline reference data and Bayesian fusion: band plan/allocation tables (47 CFR §2.106, ITU regions), FCC ULS extracts indexed by location, RadioReference imports, sigidwiki/Artemis signal DB, transmitter lists (FMLIST, SatNOGS DB), plus the device's own history. Answers "what is *supposed* to be here?" and "is this expected?" with a non-zero prior for unknowns. | Frequency, bandwidth, location, time, features → ranked candidate identities, "expected/unexpected here" flag | Negligible | Doc 04 §1.1 prior formula. Exists to separate known from unknown, not as the goal (CLAUDE.md). |
| C18 | `fingerprint-signatures` | Editable signature database keyed on the doc 04 §7.6 fingerprint (raster, OBW, family, levels, symbol rate, sync word, timing, hop set, CRC) with matching; clustering of unknown emissions into "the same thing I saw before"; hooks for later RF-hardware fingerprinting of individual transmitters. | Parameters + bits + timing → signature match or new cluster id | Low | Generalises rtl_433 flex specs beyond OOK. Basis of emitter identity in the inventory. |

### Layer D — Demodulate and decode

| ID | Capability | What it does | Inputs → Outputs | Compute | Reuse / notes |
|---|---|---|---|---|---|
| C19 | `analog-demod` | Automatic analog mode selection (AM/NBFM/WFM/SSB/CW) from C13 features and band priors, SNR- and noise-based auto squelch, AGC, de-emphasis by region, CTCSS/DCS detection, FM stereo and RDS decode, SSB carrier inference with a single "clarifier" nudge. Produces audio streams and channel attributes. | Channel stream → audio + attributes (tone, RDS PI/PS, mode) | Low per channel | Doc 04 §6. The "never pick FM/AM by hand" promise. |
| C20 | `digital-demod` | Blind digital demodulation to soft symbols and bits: envelope slicer for OOK/ASK, discriminator + multilevel slicer for FSK/GFSK/MSK, matched filter + Gardner/PFB timing + Costas/decision-directed carrier recovery for PSK/QAM, dechirp for CSS, using parameters from C13/C14. | Channel stream + parameters → soft symbols, bits, EVM/lock quality | Low–medium per channel | Doc 04 §7.1–7.2. liquid-dsp modems, Suscan demodulators. |
| C21 | `bit-framing-inference` | From a bitstream (usually many bursts of one emitter): line-code identification, preamble and sync-word discovery, dewhitening/LFSR search, framing, CRC/checksum reverse engineering, FEC structure detection, per-bit-position entropy and field inference. Emits a draft decoder spec. | Bitstreams (multi-message) → frame model, CRC params, field map, draft signature | Low, bursty CPU (offline-style) | Doc 04 §7.3–7.4, §7.7. URH inference ideas, CRC RevEng, delsum. |
| C22 | `decoder-plugins` | Hosts existing decoders as isolated plugins (subprocess or IPC) with a uniform contract: IQ, audio or bits in; structured messages and annotations out. Routes the right channel to the right decoder based on classification, ingests decoded identities and CRC-valid frames as ground-truth labels. Includes a **pipeline registry** (signal type → chain → products). | Channel/audio/bit streams + registry → decoded messages, labels, products | Varies by decoder; typically a few % of a core each | rtl_433, readsb/dump1090, dump978, AIS-catcher, multimon-ng, direwolf, acarsdec/dumpvdl2, radiosonde_auto_rx, SatDump CLI, gr-satellites, redsea, nrsc5, dsd-fme, GNSS-SDR. SatDump's registry and IQEngine's plugin contract are the models (doc 03 §5.2). |
| C23 | `trunking-follow` | Control-channel hunting (continuous-duty 4FSK on the LMR raster + sync words), CC decoding (P25, DMR Tier III/Capacity Plus, NXDN, SmartNet/EDACS/MPT1327), grant following onto channelizer outputs, encryption flagging (metadata only, never decryption of others' traffic), per-call recording and talkgroup metadata. | Channelizer streams → system model, call records, audio for unencrypted calls | Medium: PFB fixed cost + one demod/vocoder per active call | Doc 04 §8. Trunk Recorder's capture-everything model; SDRTrunk/OP25 for protocol logic. Vocoder licensing is a Phase 3 ledger item. |
| C24 | `stream-output` | Streams bits, soft symbols, decoded messages, audio and IQ slices to external programs over sockets/pipes with framing, metadata (SigMF-style headers, timestamps, source emission id) and backpressure. Also the API surface other tools script against. | Any internal stream → external consumers | Negligible | Workflow step 7. Contract defined in a Phase 3 ADR. |

### Layer E — Remember

| ID | Capability | What it does | Inputs → Outputs | Compute | Reuse / notes |
|---|---|---|---|---|---|
| C25 | `sigmf-recording` | Triggered and manual IQ recording as SigMF (with pre-trigger from C03), per-channel decimated recordings, audio recordings, annotations written as SigMF annotations, retention policy and quota on limited disk, an archive index. | Trigger/selection + ring buffer → SigMF datasets + index entries | I/O-bound; full-rate 20 Msps is 144 GB/h so recordings are snippets | SigMF spec, sigmf-python; SDRangel's SigMF sink behaviour. Also the fixture format for tests. |
| C26 | `spectrum-history` | Long-term compressed store of sweep rows and dwell spectrograms (multi-resolution, max/mean/percentile downsampling), plus queries of the form "show me region X over time span T" and survey reports. | SweepFrames/SpectrumFrames → compressed tiles; region/time queries → history views and reports | Very low CPU; storage is the budget (a 4096-bin float32 spectrogram at 30 lines/s is ~0.5 MB/s before compression) | Spectre (HB9TF) and NTIA SCOS as precedents. Workflow steps 1 and 3. |
| C27 | `signal-inventory` | The emitter database: one entry per emission cluster with first/last seen, centre, bandwidth, duty cycle, fingerprint, classification with confidence, decoded identity, known/unknown status, links to recordings, tracks and explanations; queryable by region, time, status and tag. | Tracks, classifications, decodes, labels → Emitter entries; queries → lists/tables | Negligible; SQLite-class | The missing piece in every open-source tool (doc 03 §5.1 #4). |
| C28 | `annotation-labeling` | User labels and corrections on detections, emitters and recordings; "confirmed by decoder" ground-truth labels; export of labelled SigMF datasets for classifier fine-tuning and for sharing later. | User input + decoder results → Annotations; export → labelled datasets | Negligible | RESEARCH-070 and the fine-tuning loop in doc 04 §5.4. |

### Layer F — Explain

| ID | Capability | What it does | Inputs → Outputs | Compute | Reuse / notes |
|---|---|---|---|---|---|
| C29 | `context-feeds` | Offline-first cache and opportunistic sync of external context: NOAA SWPC scales, GOES X-ray, Kp, solar wind; lightning networks; TLEs and SatNOGS pass predictions; SondeHub launches; GPS-jam indices; tropo/Es forecasts; PSKReporter/WSPR spots; FMLIST; DSN schedules. Each feed is a typed ExternalEvent source with cache age. | Network (when available) → ExternalEvents in a local store | Negligible; network only | Workflow "attack map" inputs. Everything must work with a stale cache. |
| C30 | `event-correlation` | Joins local anomalies (novelty from C12, noise-floor changes from C08, new emitters from C27) with ExternalEvents and own history by time, frequency, geometry (satellite pass over the site, storm distance, launch site) and known signatures, producing ranked **Explanations** with confidence and evidence links. | Local anomalies + ExternalEvents + position → Explanation records | Low | The radio "attack map" (doc 05 §3; AWARE-044). Time-coincidence first, geometry second. |

### Layer G — Specialised measurement and output

| ID | Capability | What it does | Inputs → Outputs | Compute | Reuse / notes |
|---|---|---|---|---|---|
| C31 | `rssi-localization` | Logs per-burst peak RSSI with position and heading, fits emitter location and path-loss parameters (robust least squares / particle filter), shows a posterior heat map and "hot/cold" guidance; supports directional-antenna nulls and attenuation near the source. | Detections + position/heading → location posterior | Low | Doc 04 §9.2. Single-receiver "where is it?" |
| C32 | `coherent-df-tdoa` | Bearing and multilateration methods beyond one channel: pseudo-Doppler via Opera Cake time mode, a coherent array accessory (KrakenSDR-class), and TDoA against timestamped remote receivers or reference transmitters. | Multi-channel/multi-site IQ + calibration → bearings, position fixes | Medium | Doc 04 §9.1. Anything beyond pseudo-Doppler needs another SDR. |
| C33 | `radiometry` | Science-grade power measurement: calibrated total-power and spectrometer modes with long integration, drift-scan logging, Y-factor noise-figure and Sun/sky calibration, sidereal-time tagging, RFI flagging via spectral kurtosis, dynamic-spectrum products. | IQ window + calibration + time → calibrated power/spectra time series | Low–medium (integration is cheap; RFI flagging reuses C07) | SDRangel radio-astronomy plugin, Virgo/PICTOR ideas. Serves solar, riometer, H I, meteor and noise-survey use cases. |
| C34 | `doppler-tracking` | Doppler-aware carrier tracking: TLE-driven pre-correction for satellite passes, Doppler-curve fitting to identify or refine orbits, Hz-resolution HF carrier Doppler (Grape-style) and multi-carrier Doppler imaging, meteor/aircraft echo detection on known illuminators. | Channel stream + TLEs/time → Doppler tracks, curve fits, echo events | Low | SigDigger's Doppler analysis, SDRangel satellite tracker. |
| C35 | `passive-radar` | Cross-correlation of a reference and a surveillance channel into range–Doppler maps using broadcast illuminators; target tracks with ADS-B truth overlay. | Two coherent channels → range–Doppler maps, detections | High (correlation over long integrations) | blah2, krakensdr_pr. Always `needs-other-sdr` on a single HackRF. |
| C36 | `gnss-observables` | Software GNSS processing (GNSS-SDR class) from raw L-band IQ: per-satellite C/N0, front-end AGC, navigation messages, pseudorange/phase for TEC and scintillation indices, spoofing tell-tales; also feeds C06 when the SDR is the GNSS source. | L-band IQ (active antenna via bias-tee) → observables, nav data, integrity flags | Medium–high continuous CPU | GNSS-SDR, galmon, OSNMAlib as plugins under C22. |
| C37 | `tx-experiments` | Transmit path for authorised use only: waveform generation, replay of the user's own captures, beacons and sounders (own links, own devices), with explicit legal gating and half-duplex scheduling around receive. | Waveform/plan + authorisation → TX; paired RX via other capabilities | Low | HackRF TX is 5–15 dBm. Receive-only by default (CLAUDE.md). |
| C38 | `ml-runtime` | GPU inference infrastructure: TensorRT/ONNX model hosting, batching of per-event inference, spectrogram object detection for dense bands, unsupervised anomaly models on waterfalls, on-device fine-tuning from labelled captures, model versioning in provenance. | Snippets/spectrograms + models → predictions with versions | Medium–high on GPU; idle when no events | Used by C15, C12 (anomaly), C30 (optional). TorchSig models, TensorRT. |
| C39 | `live-view-inspector` | The interactive surface: spectrum/waterfall/persistence at the device's frame rate, region browsing over history, the signal table, and a per-signal **inspector** workspace (constellation, IF histogram, symbol stream, bit view, cyclic spectrum) bound to an emitter's records. Local display and remote clients. | SpectrumFrames, inventory, records → rendered views; user actions → commands | Medium (GPU waterfall); decided in the Phase 3 UI ADR | SigDigger inspector, DPX persistence, Aaronia/CRFS signal tables (doc 03 §5.2). |

### 2.1 Capability dependencies (for ordering the roadmap)

Corrected after the capability-card review (see §5). The tree shows the main acquire→sense→characterize→demod path; the loops and cross-edges below it are real and matter for ordering.

```
C01 source-abstraction ─┬─ C02 sweep-survey ─────────────────────────────┐
                        ├─ C05 calibration ─(spur mask, gain)─┐           │
                        └─ C03 dwell-capture ─┬─ C07 spectral-estimation ─ C08 noise-floor ─ C09 cfar-detection ─ C10 burst-tracking
                                              └─ C11 channelizer                          ▲(C05)          │
C13 param-estimation ─ C14 blind-symbol-estimation ─┬─ C15 modulation-classifier          │              │
C16 ofdm-dsss-analysis ─────────────────────────────┘   ▲(C17 priors, C38 ml)             │              │
C11 channelizer ─┬─ C19 analog-demod ─┐                                                    │              │
                 └─ C20 digital-demod ─ C21 bit-framing-inference ─┐                        │              │
C22 decoder-plugins ◄─(routed by C15/C17)   C23 trunking-follow ◄─(C11+C12)                 │              │
C24 stream-output ◄─(all of Layer D)                                                        │              │
C26 spectrum-history ─► C12 occupancy-baseline ◄─(C08,C10) ─► C04 attention-scheduler ◄─(C02,C22,C23,C29,C37)
C27 signal-inventory ◄──► C30 event-correlation ◄──► C17 known-signal-priors   (all three form loops)
C29 context-feeds ─► C30, C04, C34   |   C06 position-time ─► C12, C30, C31, C34, C37
C31–C38 specialised, each on the layers above; C38 ml-runtime also feeds C09,C15,C18; C39 reads everything
```

Key points the diagram flattens: **Layer D is not a chain** — `decoder-plugins` (C22) does not require `bit-framing-inference` (C21), `trunking-follow` (C23) needs the channelizer and occupancy (C11, C12), and `stream-output` (C24) takes input from every part of Layer D. **Layer E is loops, not a line** — inventory (C27), correlation (C30) and priors (C17) all read and write each other. **C05↔C09 is a bootstrap loop** — calibration's spur/intermod tests need detections, and detection needs the spur mask, resolved by seeding a factory/terminated-input spur map and refining at runtime.

## 3. Mapping rules for `use-cases.yaml`

*(Applied after taxonomy approval. Recorded here so the mapping is reproducible.)*

**`capabilities`**: an ordered list of slugs. The first entry is the capability the use case is *mostly about*; later entries are what it also exercises. Do not list the substrate (C01, C03, C07, C39) unless the use case is specifically about it. Data-only use cases (archive mining, reading papers, datasets) list only `context-feeds`, `annotation-labeling` or nothing, and are marked as such in `fit_note`.

**`hardware_fit`** (one value) with a free-text `accessory` field when relevant:

| Value | Rule |
|---|---|
| `native` | Receive-only within 1 MHz–6 GHz on HackRF One + Jetson with a normal antenna. Data-only use cases that need no radio are also `native`. |
| `needs-accessory` | Receivable on the HackRF with a named add-on: VLF/ELF front end or soundcard (`below 1 MHz`), HF upconverter or active loop, LNB/downconverter (Ku, X, C band), directional antenna, LNA + bias-tee, filter bank/notch, GPSDO, induction coil, active GNSS antenna, dish. Name it in `accessory`. |
| `needs-tx` | `tx: required` in the catalogue, or the summary is about transmitting. |
| `needs-other-sdr` | Needs what a single HackRF cannot do even with accessories: phase-coherent multi-channel, >20 MHz gap-free bandwidth, ≥12-bit dynamic range as a stated requirement, full duplex, or a second simultaneous receiver. |
| `out-of-scope` | Needs a network of the user's own sensors, spacecraft-only data, non-radio instruments as the primary sensor, or is an attack on other people's systems. Using *public* remote receivers or uploading to public networks is **not** out of scope (it is `context-feeds` / `stream-output`). |

When two values apply, pick the more limiting one in the order `out-of-scope` > `needs-other-sdr` > `needs-tx` > `needs-accessory` > `native`, and mention the other in `fit_note`.

## 4. Coverage analysis

All 391 use cases are mapped in `use-cases.yaml`. The five themes were mapped in parallel by subagents against the frozen taxonomy and §3 rules, then reconciled. Counts below are from the merged file.

### 4.1 Hardware fit

| Fit | Count | Share | Reading |
|---|---:|---:|---|
| `native` | 197 | 50% | Receive-only on HackRF One + Jetson with an ordinary antenna, or data-only. Half the catalogue works on the base device. |
| `needs-accessory` | 109 | 28% | One named add-on unlocks it. Dominated by three clusters (below). |
| `needs-other-sdr` | 39 | 10% | Beyond a single HackRF: phase-coherent multi-channel, >20 MHz gap-free, or full duplex. |
| `needs-tx` | 24 | 6% | Transmitting, for the user's own links/devices under authority. |
| `out-of-scope` | 22 | 6% | Own sensor networks, spacecraft-only data, non-radio instruments, or attacks on others (incl. jamming, RESEARCH-027). |

`hardware_fit` gives the single most-limiting constraint; a separate **`fit_flags`** list carries orthogonal caveats that do not change the fit value, so the mapping stays queryable. Current flags: `marginal-hf` (HackRF HF sensitivity is poor below ~30 MHz; 27 use cases), `marginal-8bit` (8-bit dynamic range limits it in dense RF; 15), `exceeds-window` (the full signal/band is wider than one 20 MHz window, so the scheduler time-shares; 32), `metadata-only` (legally limited to metadata, never content; 13), `data-only` (no local radio needed; 41), `knowledge-item` (a paper/attack the device only observes receive-only; 15). A use case can carry several.

**By theme** (native share tells you how self-contained each area is):

| Theme | native | accessory | other-sdr | tx | out | native % |
|---|---:|---:|---:|---:|---:|---:|
| AWARE (spectrum awareness) | 58 | 7 | 2 | 0 | 3 | 83% |
| SIGNAL (the long tail) | 51 | 27 | 1 | 0 | 0 | 65% |
| RESEARCH (unknown/security/ML) | 38 | 7 | 17 | 12 | 4 | 49% |
| PROP (propagation/sensing) | 26 | 24 | 17 | 8 | 8 | 31% |
| SPACE (space weather/astronomy) | 24 | 44 | 2 | 4 | 7 | 30% |

The "attack map" theme (AWARE) is overwhelmingly native: it is mostly detection, decoding of already-supported protocols, occupancy history and correlation with external feeds. The science themes (SPACE, PROP) lean on accessories because so much of their content is below 1 MHz (VLF/soundcard), needs a dish or LNB, or needs a disciplined clock for sub-Hz Doppler. This matches the product vision: the base device is strong for spectrum awareness and the signal long tail out of the box; science is a first-class but accessory-driven expansion.

### 4.2 The shared core

Ranked by how many use cases each capability serves at all (`any`), how many it is the primary purpose of (`primary`), and how many of the use cases it touches are `native` (`native-any`). Substrate capabilities (C01, C03, C07, C39) are undercounted by design — they are listed only when a use case is specifically about them, but they underpin all 197 native use cases.

| Rank | Capability | any | primary | native-any |
|---|---|---:|---:|---:|
| 1 | `decoder-plugins` (C22) | 139 | 82 | 87 |
| 2 | `context-feeds` (C29) | 108 | 27 | 57 |
| 3 | `event-correlation` (C30) | 93 | 14 | 52 |
| 4 | `spectrum-history` (C26) | 88 | 3 | 43 |
| 5 | `signal-inventory` (C27) | 74 | 3 | 62 |
| 6 | `position-time` (C06) | 68 | 2 | 27 |
| 7 | `radiometry` (C33) | 65 | 37 | 14 |
| 8 | `doppler-tracking` (C34) | 63 | 20 | 16 |
| 9 | `cfar-detection` (C09) | 49 | 12 | 25 |
| 10 | `burst-tracking` (C10) | 46 | 15 | 30 |
| 11 | `stream-output` (C24) | 45 | 2 | 31 |
| 12 | `calibration` (C05) | 43 | 5 | 14 |
| 13 | `tx-experiments` (C37) | 40 | 35 | 1 |
| 14 | `known-signal-priors` (C17) | 39 | 8 | 32 |
| 15 | `sigmf-recording` (C25) | 38 | 6 | 22 |
| 15 | `occupancy-baseline` (C12) | 37 | 2 | 28 |
| 15 | `ofdm-dsss-analysis` (C16) | 37 | 12 | 15 |
| 15 | `digital-demod` (C20) | 37 | 6 | 20 |

Two findings shape the roadmap:

1. **The device is a router of signals to consumers, wrapped in a memory.** `decoder-plugins` is primary for 82 use cases and touches 139; `signal-inventory`, `spectrum-history`, `context-feeds` and `event-correlation` each touch 70+. The largest single lever is the plugin-hosting + inventory + history + correlation spine, not any one DSP algorithm. This is exactly the gap doc 03 §6 found no open tool fills, and it is almost entirely native.
2. **The detection→characterization chain is the second lever and is cheap.** `cfar-detection`, `burst-tracking`, `noise-floor`, `param-estimation`, `occupancy-baseline`, `known-signal-priors`, `calibration` are all low compute (doc 04 §12), mostly native, and feed everything above them. They are the "find interesting things automatically" core.

**Implied build order**, before any expensive stage. Each capability named explicitly so the roadmap can lift it directly:

1. **Acquire substrate:** `source-abstraction` (C01), `dwell-capture` (C03), `calibration` (C05). C05's spur mask and per-band gain tables feed every threshold, so it comes early even though it also refines at runtime (bootstrap: factory/terminated-input spur map first, C09-driven refinement later).
2. **Sense core:** `spectral-estimation` (C07) → `noise-floor` (C08) → `cfar-detection` (C09) → `burst-tracking` (C10). All low compute, all native, and the foundation of "find interesting things automatically". `noise-floor` (C08) is the hinge: every threshold and the SNR wall depend on it, so it lands with C07 and before C09.
3. **Attention:** `attention-scheduler` (C04) enters as soon as C09/C10/C12 give it something to prioritise. It is not deferrable — with one half-duplex window it decides what every later stage ever sees — but its first version is a simple sweep/dwell alternation, upgraded to bandit revisit later.
4. **Memory + priors:** `spectrum-history` (C26), `signal-inventory` (C27), `known-signal-priors` (C17), `occupancy-baseline` (C12). C26 feeds C12 (baselines need history); C12 and C10 feed C04.
5. **Characterize (cheap tier):** `param-estimation` (C13) then `blind-symbol-estimation` (C14). C13 normalises every snippet for classifiers and demodulators and is a prerequisite for the whole demod/decode layer, so it precedes anything in Layer D.
6. **Channelize + route:** `channelizer` (C11) is the dominant fixed compute cost of Layer D and the prerequisite for parallel demod/decode and for trunking; it lands with the first decoders. Then `decoder-plugins` (C22) + `stream-output` (C24).
7. **Explain:** `context-feeds` (C29) + `event-correlation` (C30).

That set covers most of the 197 native use cases and is the input to the Phase 6 roadmap. Expensive or accessory-gated capabilities (§4.3) come after.

### 4.3 Costly or narrow capabilities — candidates to defer

| Capability | any | native-any | Why defer |
|---|---:|---:|---|
| `passive-radar` (C35) | 11 | 0 | Needs phase-coherent reference + surveillance channels: `needs-other-sdr` for every one of its 11 use cases. High compute (long correlations). Zero native reach. Defer until a coherent front end exists. |
| `coherent-df-tdoa` (C32) | 6 | 2 | Needs a coherent array (KrakenSDR-class) or multi-site timing. The native pair are public-receiver TDoA, not local. Defer with C35. |
| `gnss-observables` (C36) | 22 | 2 | Primary for 18 use cases but only 2 native — nearly all need an active GNSS antenna, and continuous software GNSS is medium-high sustained CPU. Cluster it into one "GNSS accessory" milestone rather than spreading it across the core. |
| `ml-runtime` (C38) | 24 | 16 | GPU inference is real but the docs are clear that classical DSP delivers most value first and ML must come after normalization with open-set output (CLAUDE.md, doc 04 §5). Build the classical cascade first; add C38 as a later stage. |
| `radiometry` (C33) | 65 | 14 | Huge reach (37 primary) but only 14 native — most science needs a VLF front end, dish or GPSDO. Not costly in compute, but gated on accessories, so its payoff tracks the accessory roadmap, not slice 1. |
| `trunking-follow` (C23) | 2 | 2 | Only 2 use cases by this count, but it is the single most-requested scanner capability (doc 04 §8) and needs the channelizer + vocoder licensing. Medium compute. Treat as a distinct milestone justified by user demand, not by use-case count. |

`tx-experiments` (C37) is primary for 35 use cases but is deliberately out of the receive-first core: it is gated on legal authority and half-duplex scheduling, and belongs to a later, opt-in milestone.

### 4.4 Notable mapping decisions

- **Public networks are in scope; the user's own networks are not.** Using KiwiSDR/WebSDR receivers, or uploading to WSPRnet/PSKReporter/SatNOGS/SondeHub/Blitzortung, maps to `context-feeds` or `stream-output` and stays `native`. Building a mesh of the user's own sensors is `out-of-scope` (e.g. AWARE-037/043, PROP-028/068/069, SPACE-045). This follows the "one self-contained device, don't design out sharing" constraint in CLAUDE.md.
- **The 20 MHz window recurs as the fit boundary.** Cellular (LTE ≥20 MHz, 5G n77/n78 up to 100 MHz), Wi-Fi OFDM, Bluetooth/BLE hopping across ~80 MHz, and dual-frequency GNSS (bands 250–400 MHz apart) all push use cases to `needs-other-sdr`; narrower slices of the same systems (LTE MIB/SIB in the central 1.4 MHz, DroneID's ~10 MHz OFDM burst, single-band GNSS C/N0) stay native but are often flagged marginal on 8 bits. `fit_note` records these calls.
- **HF is native but flagged.** The HackRF tunes from 1 MHz, but its sensitivity below ~30 MHz is poor without an active antenna or upconverter; HF use cases are `native` with a standing "marginal on HackRF HF" note rather than downgraded, since the user can add an active loop.
- **Knowledge and dataset items** (much of RESEARCH) are mapped to the capability that would *observe* the phenomenon receive-only, or to `annotation-labeling`/`ml-runtime`/`sigmf-recording` for datasets, and flagged `data-only` or `knowledge item` in `fit_note`, so they still exercise real code paths in the test suite.
- **Jamming is out of scope even against your own devices.** RESEARCH-027 (RollJam: jam + record + replay) is `out-of-scope` because transmitting to block a receiver violates 47 USC 333 regardless of target. Capture-and-analysis of the same key fobs without jamming stays in scope (SIGNAL-047, RESEARCH-028 RollBack is replay-only). This follows the legal guardrails in CLAUDE.md.
- Full lists of the `out-of-scope`, `needs-other-sdr` and `needs-tx` decisions, with reasons, are in the `fit_note` field of each entry in `use-cases.yaml`; orthogonal caveats are in `fit_flags`.

## 5. Resolved taxonomy questions (from capability-card review)

Writing the 39 capability cards surfaced missing edges, unowned responsibilities and overlaps, collected in `docs/capabilities/taxonomy-feedback.md`. The taxonomy stays at 39 capabilities (the user froze it); these are **ownership and edge decisions within it**, not new capabilities. Decisions that a later ADR or the data model (doc 07) will finalise are marked **provisional**.

**Dependency edges** — the corrected set is folded into §2.1 above.

**Ownership decisions** (which capability owns a responsibility that had none):

| Responsibility | Owner | Note |
|---|---|---|
| Decimated narrowband "zoom" stream (the 25 Hz-bin case) | `channelizer` (C11) | C11 produces the decimated stream; `spectral-estimation` (C07) then FFTs it. C07 does not itself decimate to 800k-point FFTs. |
| Own-key decryption of the user's own traffic | `decoder-plugins` (C22) | A decoder stage with user-supplied keys; never applied to others' traffic. Records the key source in provenance. **Provisional** pending the doc 07 provenance/key model. |
| Restricted-content gating (cellular, common-carrier paging content, 47 USC 605) | `stream-output` (C24) as the enforcement point, with `signal-inventory`/`sigmf-recording` honoring a content-class flag set at classification | Metadata always allowed; content gated. **Provisional**, and a legal-guardrail ADR in Phase 3 will pin it. |
| Conventional (non-trunked) digital voice P25/DMR/NXDN | `digital-demod` (C20) + `decoder-plugins` (C22) | No separate capability; C23 is only the trunking control/grant logic. |
| OFDM/CSS/radar-pulse *demodulation* (vs parameter estimation) | `digital-demod` (C20) | C16 estimates OFDM/DSSS parameters and C14 estimates chirp/pulse parameters; C20 demodulates. Split is estimate (C13/C14/C16) vs recover (C19/C20). |
| Storage quota and retention across recordings/history/inventory | shared policy defined in doc 07 | Not a capability; a cross-cutting policy object. Owners C25/C26/C27 honor it. **Provisional.** |
| The shared "anomaly" record emitted by C08/C12/C27 and consumed by C30 | doc 07 domain object | A typed Anomaly/Detection-derived record. **Provisional** — defined in the data model. |
| Map/geo and dashboard views | `live-view-inspector` (C39) | C39 explicitly owns live view, history browser, signal table, inspector, plus map/geo and the attack-map dashboard. UI ADR (Phase 3) decides how. |
| Satellite-pass / TLE propagation | `context-feeds` (C29) computes passes from cached TLEs; `doppler-tracking` (C34) consumes them; `event-correlation` (C30) uses pass windows | TLE propagation is a feed-side computation, not its own capability. |
| GNSS reflectometry (GNSS-IR) use cases | `gnss-observables` (C36) | C36 covers observables and the SNR-vs-elevation products reflectometry needs. |

**Overlaps resolved:**

- **Clustering emissions** into "same thing seen before": owned by `fingerprint-signatures` (C18). `burst-tracking` (C10) links bursts into tracks over short time; C18 clusters emissions across time by fingerprint; `bit-framing-inference` (C21) aligns messages within a cluster. Three scales, one owner each.
- **Interestingness score:** computed by `occupancy-baseline` (C12) (it has the baselines and novelty); consumed by `attention-scheduler` (C04). C04 does not recompute it.
- **Bayesian fusion of features and priors:** `known-signal-priors` (C17) *supplies* priors; `modulation-classifier` (C15) *fuses* them with likelihoods. C17 does not classify.
- **Noise-floor-vs-time as science data** (SPACE-050, AWARE-031): the measurement is `noise-floor` (C08); when it is the deliverable with calibration and long integration it is `radiometry` (C33). Both may appear; C33 leads when it is the product.
- **RDS/RBDS:** owned by `analog-demod` (C19) (it rides the FM MPX). Not a `decoder-plugins` external tool.
- **FEC:** `bit-framing-inference` (C21) *identifies* code structure; `decoder-plugins` (C22) *executes* known decoders. Inference vs execution.
- **Amplitude bearings** (Yagi max/body-null): `rssi-localization` (C31); coherent phase/interferometry/TDoA is `coherent-df-tdoa` (C32).
- **FMLIST / SatNOGS DB** appear as both reference data (C17) and feeds (C29): C29 fetches and caches them; C17 is the query/prior interface over the cache. Same data, two roles.
- **`stream-output` (C24) mixes data egress with the control/API surface.** **Provisional split** deferred to the Phase 3 stream-contract ADR: the ADR decides whether the outbound bitstream/message transport and the control API are one capability or two.

**Factual corrections applied to the cards** (see §5 tasks below):

- **No hardware 1PPS on HackRF One** (no PPS input). `position-time` (C06) and `calibration` (C05) discipline via a 10 MHz GPSDO into CLKIN, or software ppm/time correction from GNSS. C06's "1PPS discipline" wording is corrected.
- **`dwell-capture` (C03) "sample-accurate timestamps"** needs a stated method: host arrival time plus a running sample count, optionally GNSS-tagged, with an error budget. Flagged for the doc 07 provenance section.
- **`gnss-observables` (C36):** the HackRF has no GNSS-style front-end AGC to report; dual-frequency TEC cannot fit one 20 MHz window; GNSS-SDR/galmon licences are unchecked (added to the Phase 3 licence ledger).
- **`passive-radar` (C35):** whether two HackRFs on a shared 10 MHz clock stay phase-coherent enough is unverified and becomes a Phase 4 spike; its result can move some C35 use cases between `needs-other-sdr` and `needs-accessory`.

**Mapping observations** (kept, not errors): `blind-symbol-estimation` (C14) is never a *primary* capability but gates every unknown-digital case; `ofdm-dsss-analysis` (C16) is primary for metadata-fenced cellular items; `param-estimation` (C13) legitimately stretches onto bench-measurement and side-channel items. Card example lists with low agreement to the final mapping (C04, C11, C20, C24, C28, C32) are regenerated from the YAML as part of the card-sync task.

