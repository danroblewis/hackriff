# San Francisco target list — cited reference for the explorer

**Purpose (T-924, user directive 2026-09-25 02:25).** A curated, cited, cached list of what a
receiver in San Francisco is likely to hear, for the AI "simulated user" explorer to pick targets
from during hardware sessions (Mac Studio only — it holds the HackRF) and append findings to. This
is **reference, not ground truth**: per the product invariants in the root `CLAUDE.md`, blind
detection always runs first and this list only supplies candidate explanations and priors — it
never pre-populates the inventory and a match here never overrides what was measured. Every entry
states the source it was pulled from and is marked **unverified** where the explorer has not yet
confirmed it against a real capture. Antenna and hardware gaps are called out per row.

Centre location assumed: San Francisco, CA (roughly 37.77 N, 122.42 W) unless noted. Frequencies
in MHz.

## How to read a row

- **Expected modulation/protocol** — what the signal *should* look like if blind detection and
  auto-demod/AMC estimate it correctly (this is a prior for the explanation-ranking step, §"Signal &
  inventory model" in the root `CLAUDE.md` — never a lookup-and-tune target).
- **"Decoded" means** — the concrete, checkable artifact that counts as a successful decode for
  this class of signal: e.g. "RDS PI/PS strings recovered", "CRC-valid frame", "audio with
  intelligible content", "NAC/WACN identity fields recovered". This is what an explorer session or
  an acceptance fixture should assert on, not just "signal present".
- **Status** — `unverified` (from public sources only, not yet captured here), or `explorer-found`
  once a session appends a real observation (see "Explorer findings" below).

## NOAA Weather Radio (NWR)

| Freq (MHz) | Callsign/site | Expected modulation | "Decoded" means | Status |
|---|---|---|---|---|
| 162.400 / 162.425 / 162.450 / 162.475 / 162.500 / 162.525 / 162.550 | 7 NWR channels, one active per region | NFM, ~5 kHz deviation voice + SAME digital header (AFSK, mark 2083.3 Hz / space 1562.5 Hz, 520.83 baud), followed by a 1050 Hz Warning Alarm Tone before voice | Audio with intelligible synthesized voice, and/or a CRC-valid SAME header (originator, event code, FIPS codes) | unverified |

Bay Area transmitter is commonly cited at **162.400 MHz** (San Bruno Mountain, KEC66 or similar
call). **[unverified — RadioReference NOAA Weather Radio pages, not independently confirmed]**
SAME tones per 47 CFR §11.31: mark 2083.3 Hz, space 1562.5 Hz, 520.83 baud; the 1050 Hz tone is the
separate Warning Alarm Tone that follows the header, not a data tone.
Source: [NOAA NWR station list](https://www.weather.gov/nwr/), [RadioReference NWR frequency
table](https://www.radioreference.com/db/browse/ctid/191), [47 CFR
§11.31](https://www.ecfr.gov/current/title-47/section-11.31) (SAME encoder/header specification).

## ADS-B (1090 MHz Extended Squitter)

| Freq (MHz) | Expected modulation | "Decoded" means | Status |
|---|---|---|---|
| 1090.0 | PPM (Manchester-like pulse-position, 1 Mbit/s), Mode S Extended Squitter | CRC-valid 112-bit DF17/18 frame; ICAO address + optionally position (CPR-decoded lat/lon), callsign, altitude | unverified — **antenna pending (T-025)** |

Tutorial 4 (`docs/tutorials/04-adsb.md`) already runs the ADS-B recipe
(`ppm_demod` → `crc` → `fields`) blind against synthetic/mock-SDR fixtures at 16/16, and cross-checks
against `readsb`. Live HIL over SF airspace (SFO approach/departure corridors, dense triangulation of
Bay Area airports) is blocked until a 1090 MHz antenna is available — see `docs/19` for the analogous
"documented but not receivable here without hardware" caveat pattern this list follows for BART.
Source: [Mode S / ADS-B 1090ES spec summary, Wikipedia](https://en.wikipedia.org/wiki/Automatic_dependent_surveillance%E2%80%93broadcast),
this repo's own `docs/tutorials/04-adsb.md` (T-097/T-110).

## APRS

| Freq (MHz) | Expected modulation | "Decoded" means | Status |
|---|---|---|---|
| 144.390 | AFSK 1200 baud (Bell 202), AX.25 framing on NFM | CRC-valid AX.25 frame; parsed APRS payload (position, comment, or telemetry) | unverified |

North American APRS calling frequency. San Francisco has active digipeaters/iGates (e.g. on Bay
Area high points); exact local digipeater call/PL is not verified here. **[unverified]**
Source: [APRS.org frequency reference](http://www.aprs.org/doc/APRS101.PDF),
[aprs.fi](https://aprs.fi/) (live map, not itself a citable spec).

## Marine AIS

| Freq (MHz) | Channel | Expected modulation | "Decoded" means | Status |
|---|---|---|---|---|
| 161.975 | AIS 1 (87B) | GMSK, 9600 baud, HDLC framing | CRC-valid AIS message (Type 1/3 position report: MMSI, lat/lon, SOG/COG, or Type 5 static/voyage data) | unverified |
| 162.025 | AIS 2 (88B) | GMSK, 9600 baud, HDLC framing | Same as above | unverified |

San Francisco Bay is a working harbour (container terminal, ferries, tankers at anchor) so AIS
traffic density should be high, day and night — a good early target for a short capture. Source:
[ITU-R M.1371](https://www.itu.int/rec/R-REC-M.1371) (AIS technical spec, summarized secondhand),
[RadioReference AIS overview](https://wiki.radioreference.com/index.php/Automatic_Identification_System).

## FM broadcast + RDS

| Band | Expected modulation | "Decoded" means | Status |
|---|---|---|---|
| 87.5–108.0 (100 kHz raster, SF stations e.g. KQED 88.5, KMEL 106.1, KOIT 96.5, KGO 810 is AM not FM — verify per-station) | WFM (±75 kHz deviation) with 19 kHz pilot + 57 kHz RDS subcarrier (BPSK, 1187.5 baud, differential coding) | Pilot lock (19 kHz present) as a channel-quality gate; RDS PI code and PS (station name) string recovered as the decode artifact; optionally RT (radiotext) | unverified |

Tutorial 1 (`docs/tutorials/01-rds.md`) is the decoder workbench's reference RDS recipe and already
decodes RDS blind against the `hk_demod::rds` oracle — this row is the live-air analogue. Specific
SF-market call signs/frequencies are not verified against a current FCC query and should be treated
as illustrative only. **[unverified — station list not cross-checked against FCC CDBS at doc time]**
Source: [FCC FM query](https://www.fcc.gov/media/radio/fm-query), RDS standard summarized in
`docs/tutorials/01-rds.md`.

## Pagers (POCSAG / FLEX)

| Freq (MHz) | Expected modulation | "Decoded" means | Status |
|---|---|---|---|
| 929.0–932.0 (paging allocation; exact channels per FCC paging band plan) | FSK, POCSAG (512/1200/2400 baud) or FLEX (1600/3200/6400 baud) | BCH-valid POCSAG codeword (for POCSAG: capcode + numeric/alphanumeric message text); FLEX frame with valid checksum | unverified |

Tutorial 2 (`docs/tutorials/02-pocsag.md`) already decodes a synthetic 4-channel POCSAG net blind
(4/4 against multimon-ng), so the recipe exists; whether SF still has live commercial paging
traffic on 929–932 MHz in 2026 is **unverified** — much of this band has migrated to other uses over
the last two decades and a real capture may show nothing. Source: [FCC Part 22/90 paging band
plan summary, RadioReference](https://wiki.radioreference.com/index.php/Paging), this repo's
`docs/tutorials/02-pocsag.md`.

## ISM (433 MHz, 915 MHz — rtl_433-style sensors, remotes, LoRa)

| Band | Expected modulation | "Decoded" means | Status |
|---|---|---|---|
| 433.05–434.79 (US Part 15, secondary — less used in the US than in EU; check for LPD/legal US devices) | OOK/ASK or FSK, short bursts, protocol varies by device (weather stations, TPMS emulators, remotes) | Protocol-specific: a `rtl_433`-style decoded record (device ID, sensor readings) or a CRC-valid frame per that device's protocol | unverified |
| 902–928 (US ISM band) | OOK/FSK short bursts (rtl_433-class sensors) and LoRa chirp spread spectrum | Per-device: sensor telemetry fields, or for LoRa: CRC-valid demodulated payload (header + payload CRC per LoRaWAN/raw LoRa PHY) | unverified |

902–928 MHz is called out in the root `CLAUDE.md` as "the canonical playground" for ephemeral,
first-class short-burst signals — expect many short, unrelated bursts rather than one steady
emitter. TPMS (tire-pressure monitors, typically 315/433 MHz depending on region) also lands here;
US vehicles commonly use 315 MHz. Source: [FCC Part 15.247/15.249 ISM rules
summary](https://www.fcc.gov/general/ism-frequency-bands), [rtl_433 supported protocol
list](https://github.com/merbanan/rtl_433/blob/master/docs/DEVICES.md) (used as the informal
"what's out there" catalogue, not itself a decoder we've wrapped).

## DMR / P25 public safety

| Freq range | Expected modulation | "Decoded" means | Status |
|---|---|---|---|
| SF public-safety trunked systems (typically 470–512 MHz UHF T-Band and/or 800 MHz; specific SFPD/SFFD system IDs not verified here) | C4FM (P25 Phase 1) or H-DQPSK (P25 Phase 2 downlink; CQPSK is the Phase 1 simulcast variant), or 4FSK (DMR, 12.5 kHz, 2-slot TDMA) | P25: NAC + CRC-valid TSBK/voice frame with identity fields; DMR: CRC-valid burst with colour code + talkgroup/radio ID | unverified |

`docs/19-bart-800mhz-trunked.md` (T-544) is the closest existing research to this row and should be
read before attempting SF public-safety decode — it documents that this repo's C4FM demod and sync
correlator can be reused but the TSBK codec fails with *certainty* per T-300, i.e. this is a **known
gap**, not a simple "point the recipe at it" target. SFPD/SFFD's actual current system (P25 vs
still-analogue, trunked vs conventional, exact frequencies) is **not verified** in this document —
check RadioReference's San Francisco County trunked-system pages before spending capture time here.
Source: [RadioReference San Francisco County, CA
database](https://www.radioreference.com/db/browse/ctid/187), `docs/19-bart-800mhz-trunked.md`.

## BART 800 MHz (documented, not receivable here)

| Freq range | Expected modulation | "Decoded" means | Status |
|---|---|---|---|
| 851.0375–853.8625 (16 channels, 12.5 kHz raster) | P25 Phase II / C4FM per `docs/19`, System ID 338 / WACN 92762, two simulcast sites | NAC + System ID + WACN identity fields from a CRC-valid TSBK, or a decoded voice/data grant | documented, **not receivable here** (see below) |

Full research already exists in `docs/19-bart-800mhz-trunked.md` and is **not duplicated here** —
that document is the citation. Per that doc and per this ticket's own text, BART 800 MHz is flagged
**not receivable from the explorer's SF location** in the current setup; it stays on this list only
as a pointer so a session doesn't waste capture time chasing it, and as the worked example of what
"decoded" means for a trunked P25 system (identity fields, not just "signal present"). Source:
`docs/19-bart-800mhz-trunked.md` (RadioReference, FCC ULS, cited per-claim inside that document).

## Utility / SCADA

| Band | Expected modulation | "Decoded" means | Status |
|---|---|---|---|
| Various (licensed utility telemetry commonly sits in VHF/UHF land-mobile allocations, e.g. 450–470 MHz, and some unlicensed telemetry in the 900 MHz ISM band) | Narrowband FSK/FM telemetry, protocol vendor-specific (no single public standard) | Vendor-specific — typically a CRC-valid or checksummed telemetry frame; often no public decoder exists, so "decoded" may only mean "bitstream extracted, protocol unidentified" | unverified |

This is the vaguest row on the list deliberately: SCADA/utility telemetry in the Bay Area is not
well catalogued in public sources the way broadcast or public-safety allocations are. Treat any hit
here as a genuinely unknown-signal case (the product's stated priority) rather than expecting a
name-and-decode match. Source: none specific — flagged **unverified / speculative** pending an
explorer finding.

## Wireless microphones / low-power auxiliary

| Band | Expected modulation | "Decoded" means | Status |
|---|---|---|---|
| Historically VHF (169–172, 174–216) and UHF TV white space (470–608, since the 2010 repack narrower); post-2010 auction changes shrank available UHF space | Analogue WFM (companded) most common in legacy gear; some digital (proprietary) in newer systems | Audio with intelligible content (analogue) or a vendor-specific decoded frame (digital) — no universal standard | unverified |

Likely to be found opportunistically (venues, theatres, churches) rather than at a fixed
frequency; included for completeness per the ticket's list, not because a specific SF frequency is
known. Source: [FCC wireless microphone rules
summary](https://www.fcc.gov/wireless-microphones), general knowledge, not independently verified
for this document.

## Explorer findings

_Appended by explorer sessions as they capture and decode. Each entry should update the relevant
row's Status to `explorer-found` and add the observed centre frequency, timestamp, capture
provenance (device/antenna/gain, per the root `CLAUDE.md`'s provenance requirement) and what was
actually decoded, with a link to the fixture if one was captured (`capture-sigmf` skill)._

(none yet as of 2026-09-25 — this document was created ahead of the first explorer session per
T-924.)

## Sources

- [NOAA Weather Radio station list](https://www.weather.gov/nwr/)
- [RadioReference NOAA Weather Radio frequency table](https://www.radioreference.com/db/browse/ctid/191)
- [RadioReference San Francisco County, CA database](https://www.radioreference.com/db/browse/ctid/187)
- [Mode S / ADS-B 1090ES, Wikipedia](https://en.wikipedia.org/wiki/Automatic_dependent_surveillance%E2%80%93broadcast)
- [APRS 101 protocol reference](http://www.aprs.org/doc/APRS101.PDF)
- [aprs.fi live map](https://aprs.fi/) (informal, not a spec)
- [ITU-R M.1371 (AIS)](https://www.itu.int/rec/R-REC-M.1371)
- [RadioReference AIS overview](https://wiki.radioreference.com/index.php/Automatic_Identification_System)
- [FCC FM query](https://www.fcc.gov/media/radio/fm-query)
- [RadioReference Paging overview](https://wiki.radioreference.com/index.php/Paging)
- [FCC ISM band rules (Part 15.247/15.249) summary](https://www.fcc.gov/general/ism-frequency-bands)
- [rtl_433 supported device/protocol list](https://github.com/merbanan/rtl_433/blob/master/docs/DEVICES.md)
- [FCC wireless microphone rules summary](https://www.fcc.gov/wireless-microphones)
- This repo: `docs/19-bart-800mhz-trunked.md`, `docs/tutorials/01-rds.md`, `docs/tutorials/02-pocsag.md`,
  `docs/tutorials/04-adsb.md`
