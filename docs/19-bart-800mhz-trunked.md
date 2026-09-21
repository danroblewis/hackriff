# 18 — BART's 800 MHz trunked system: research, capture plan and ground truth

**Status: research complete. CAPTURE TAKEN 2026-09-20 AND NEGATIVE — BART is not receivable at this location, and no trunked control channel is reachable anywhere in the 800 MHz downlink band. See §7, which supersedes the forward-looking language in §4 and §6.** Written for T-544 (phase 1 of the
BART blind auto-decode target: T-544 research + capture, T-545 failing blind tests, T-546 make them
pass). Use case: [`SIGNAL-087`](05-use-cases-and-explorations.md) — minted 2026-09-20 for exactly
this target, because the three tickets were filed against `SIGNAL-001`, which is an existing
unrelated ID and IDs are permanent.

Everything below is from **public sources only**. No capture has been taken; nothing here was
measured. Claims carry an explicit confidence, because a guess recorded as fact here poisons every
test T-545 and T-546 build on top of it.

**Confidence legend.** **[measured]** — nothing yet, by construction. **[documented]** — stated by a
primary or standards source. **[curated]** — from RadioReference's edited database, which is
maintained by hobbyists from monitoring, generally accurate and occasionally stale.
**[reported]** — a forum observation by one person on one date. **[inferred]** — my reasoning from
the above, not stated by any source. **[unverified]** — I could not confirm it and it matters.

---

## 1. What the system is

### 1.1 The headline

BART (San Francisco Bay Area Rapid Transit) runs an **APCO Project 25 trunked radio system in the
800 MHz band**, typed by RadioReference as **"Project 25 Phase II"**, **System ID 338**,
**WACN 92762**, with **two simulcast sites** — one underground, one above ground. **[curated]**
([RadioReference SID 12049](https://www.radioreference.com/db/sid/12049), last updated
2025-10-12.)

Its FCC authorisation is under the licence **WPSH605**, *SAN FRANCISCO BAY AREA RAPID TRANSIT
DISTRICT*, radio service "PubSafty/SpecEmer/PubSaftyNtlPlan, **806-817/851-862 MHz**, Trunked".
**[documented]** ([FCC ULS licKey
1986151](https://wireless2.fcc.gov/UlsApp/UlsSearch/license.jsp?licKey=1986151) — the licence record
exists and the service description is quoted from the search index; **the frequency schedule itself
could not be retrieved**, as ULS returns HTTP 403 to this environment. **[unverified]** for the
per-frequency authorisations.)

Note the band: **851–862 MHz is the 800 MHz SMR / public-safety "general category" band, not
NPSPAC.** Post-rebanding NPSPAC is 821–824 / 866–869 MHz. One search summary of BART's own
engineering specification asserted "NPSPAC frequencies"; the frequencies RadioReference lists
(851.0375–853.8625) are **not** in the NPSPAC sub-band, and the FCC service description says
806-817/851-862. I could not open the BART specification to check its exact wording — the host
`webapps.bart.gov` serves an expired TLS certificate and is unreachable from this environment.
**Treat "NPSPAC" as a probable mis-summary and the 851–854 range as correct. [inferred]**

### 1.2 Sites and frequencies **[curated]**

All figures MHz, from [RadioReference SID 12049](https://www.radioreference.com/db/sid/12049).
RadioReference marks control-capable channels with a superscript `c`; **every** frequency on both
sites is so marked, which on a Motorola-style system means the control channel **rotates** across
the pool rather than living on one fixed frequency.

| Site | RFSS | Site # | NAC | Frequencies |
|---|---:|---:|---|---|
| **Underground Simulcast** | 1 | 001 | (not listed) | 851.6375, 852.1625, 852.3375, 852.6500, 853.6750, 853.8625 |
| **Above-Ground Simulcast** | 30 | 030 | **028** | 851.0375, 851.3125, 851.5625, 851.8875, 852.0375, 852.2375, 852.5625, 852.8125, 853.0375, 853.3625 |

Sixteen frequencies spanning **851.0375 – 853.8625 MHz** (a 2.825 MHz span). Every one falls on the
US 800 MHz 12.5 kHz raster (851.0125 + n × 12.5 kHz) — I checked each. **[inferred]**

**Which one is the control channel right now is not knowable from a database and must not be looked
up.** That is the point of the exercise: the control channel is the *continuous, 100 %-duty* emission
in the band, and finding it that way is [`SIGNAL-085`](05-use-cases-and-explorations.md)'s whole
claim. The table above exists so this document can be honest about what was and was not known in
advance; **the acceptance tests must never see it** (§5).

### 1.3 Phase 1 or Phase 2 — and why it matters more than anything else here

This is the single most consequential open question for T-546, and **the sources disagree**:

- RadioReference types the system **"Project 25 Phase II"** with voice "APCO-25 Common Air Interface
  Exclusive", and marks almost every talkgroup mode **`T`** = TDMA (Phase 2). The exceptions carry
  **`D`** = Digital (Phase 1 FDMA): the whole Interoperability block (TGs 34000–34018, including the
  below-ground fire talkgroups) and TG 1359 "EMF Shop". One talkgroup, **1018 BPD Disp**, is marked
  encrypted. **[curated]**, updated 2025-10-12. (Mode letters per the RadioReference database
  convention: `D` digital, `T` TDMA, second letter `E`/`e` full-time/part-time encryption —
  [forum](https://forums.radioreference.com/threads/what-does-t-in-the-mode-column-in-the-database-designate.389164/).)
- A RadioReference forum observer ("Radiobern", **2024-02-22**) reported the opposite for the train
  channels: *"Most transmissions on the P25 system are Phase 2 **except the road channels are Phase
  1**."* **[reported]**
  ([thread](https://forums.radioreference.com/threads/bart-800-mhz-edacs.468827/))

The database is 20 months newer than the forum post and says the Road talkgroups are `T`. **I am not
picking a winner.** Either the road channels were migrated to Phase 2 between February 2024 and
October 2025, or the database is wrong on that row. Both are ordinary. **[unverified]**

**The part that is not in doubt, and the part the acceptance test actually rests on:** on a P25
Phase 2 system the *control channel* is normally still **Phase 1 FDMA C4FM at 4800 Bd / 9600 bps**.
Phase 2 TDMA applies to the *voice* channels. The RadioReference wiki states: *"control channels are
sent with the same C4FM or CQPSK modulation seen in Phase 1 systems"*, and notes that **TDMA control
channels are a recent, rare Harris-only deployment**. **[documented]**
([RR wiki: APCO Project 25](https://wiki.radioreference.com/index.php/APCO_Project_25))
So a blind capture of this band should contain one continuous C4FM carrier that our existing
demodulator can read. **[inferred]** — and §4.4 says how to *verify that from the IQ itself* rather
than believe it.

BART's vendor is **[unverified]**. I found no primary source naming Motorola, L3Harris, Tait or
anyone else. This matters only because a Harris system is the one family that might put a **TDMA**
control channel on the air, in which case nothing in this repo can touch it (§3.2).

### 1.4 History — why there are two sites and why EDACS keeps coming up **[reported]**

- **2022-06/08:** BART's P25 network existed as core infrastructure for years, but was "for
  underground use only, and is currently only used for interoperability by first responders", running
  **Phase 1**; above-ground RF sites had no P25 equipment until an upgrade wave that year. The legacy
  **EDACS** system still carried operations.
  ([thread](https://forums.radioreference.com/threads/bart-p25-system.444583/))
- **2024-01-30:** 853.0375 retained "as a data channel"; 852.2375 became an additional P25 voice
  channel.
- **2024-02-22:** *"I am no longer picking up any EDACS data channel and noticed P25 transmissions on
  the remaining EDACS frequencies."* EDACS is down; P25 carries everything.
  ([thread](https://forums.radioreference.com/threads/bart-800-mhz-edacs.468827/))

So: **EDACS is historical.** If a 2026 capture shows an EDACS 9600 bps GFSK control channel, that is
a finding worth recording, but do not expect one. **[inferred]**

### 1.5 What is on it — what a "correct decode" would even look like

The talkgroup catalogue is large and structured, and this is what tells us what a decode *means*.
**[curated]** ([RadioReference SID 12049 talkgroups](https://www.radioreference.com/db/sid/12049)):

| Category | Range | Examples |
|---|---|---|
| Train Operations | 1273–1331 | `Road W Line` (Daly City–Millbrae), `Road M Line` (SF Market St), `Road A Line` (Fremont), terminal-zone turnbacks, Hayward Test Track |
| Station Operations | 1529–1536 | per-line station ops |
| Maintenance | 1307–1492 | `Power/Way` (3rd-rail distribution), `Train Control`, `Comm Maint`, Maintenance 1–12 |
| Train Yards & Shops | 1325–1449 | Concord/Daly City/Hayward/Richmond towers and shops |
| Police (BPD) | 1017–1047 | `BPD Main`, `BPD Disp` (**encrypted**), zone tacticals 1–6, `BPD CAP` |
| Incident Command | 1388–1558 | ICS 1/2, eBART ICS |
| Interoperability | 34000–34018 | `BART CALL`, BART INT 1–5, below-ground fire primary/secondary/command — **Phase 1 (`D`)** |

**A correct decode, in increasing order of difficulty:**

1. **Sync lock.** The C4FM symbol stream contains the P25 frame sync `0x5575F5FF77FF` at 4800 Bd,
   repeating. **[documented]**
   ([GopherTrunk, P25 Phase 1 physical layer](https://gophertrunk.org/blog/deep-dives/protocol-decoders-02-p25-phase-1-physical-layer/))
2. **NID.** A 64-bit Network ID follows each sync: **12-bit NAC** + **4-bit DUID** under
   **BCH(63,16,11)** plus one flag bit. A correct decode yields a *constant* NAC across the whole
   capture — RadioReference says the above-ground site's NAC is **028** hex, which is the first
   cross-check available and the first thing the test must **not** be told. **[documented]** /
   **[curated]**
3. **TSBK messages.** Trunking Single Block messages, 48 information bits (12 bytes), rate-1/2
   trellis coded to **98 channel dibits**, block-interleaved over those 98 dibits, with a **2-bit
   status symbol inserted every 35 data dibits (stride 36)**, and an **augmented CRC-CCITT**
   trailer. **[documented]** (GopherTrunk, as above; corroborated independently inside this repo at
   `crates/hk-detect/src/trunk/confirm.rs:980-1035`, which reached the same five conclusions in
   T-300.)
4. **Message content.** Identity messages (NAC, **System ID 338**, **WACN 92762**, RFSS, site) and
   `IDEN_UP` band-plan messages that define the channel-number → frequency mapping, then **group
   voice channel grants**: talkgroup (16-bit), source radio ID (24-bit), channel number, service
   options including the encryption/ALGID flag. **[documented]** for the structure, **[curated]**
   for the specific ID values.
5. **The self-proving decode.** A grant says "talkgroup 1274 on channel N". `IDEN_UP` turns channel N
   into a frequency. **RF energy must then appear at that frequency, at that time, in the same
   capture.** This is the assertion that makes the whole exercise honest, and §5.3 builds the ground
   truth on it.

**Voice audio is explicitly NOT the target.** Phase 2 voice is 2-slot TDMA with the proprietary
**AMBE+2** vocoder, and the one police dispatch talkgroup is encrypted anyway. The acceptance target
is the **control-channel bitstream and its message fields** — which is also what
[`SIGNAL-086`](05-use-cases-and-explorations.md) (encryption-aware, metadata-only logging) already
scopes. **[inferred]**

---

## 2. The physical layer, in numbers

Everything a blind estimator has to arrive at *by measurement*. Listed here so §4.4 and §5 can check
the capture against it — **not** so any of it can be handed to the system.

### 2.1 P25 Phase 1 FDMA / C4FM **[documented]**

| Property | Value | Source |
|---|---|---|
| Channel spacing | 12.5 kHz | [sigidwiki](https://www.sigidwiki.com/wiki/Project_25_(P25)) |
| Occupied bandwidth | ~8–10 kHz hump inside the 12.5 kHz slot | [GopherTrunk C4FM](https://gophertrunk.org/blog/deep-dives/p25-end-to-end-01-c4fm-carrier/) |
| Symbol rate | **4800 symbols/s**, 2 bits/symbol → 9600 bps raw | sigidwiki, RR wiki |
| Deviation levels | **±1800 Hz** (outer), **±600 Hz** (inner); slicer thresholds at ±1200 Hz | GopherTrunk, [VIAVI](https://www.viavisolutions.com/sites/default/files/support/understanding-p25-modulation-fidelity-application-notes-en.pdf) |
| Dibit mapping | +3→`01`, +1→`00`, −1→`10`, −3→`11` | GopherTrunk |
| TX filter | raised cosine α = 0.2 (flat to 1920 Hz, rolls off to 2880 Hz) + inverse-sinc | VIAVI, GopherTrunk |
| RX filter | integrate-and-dump over one symbol (**not** RRC) | GopherTrunk |
| Frame sync | `0x5575F5FF77FF`, 24 dibits | GopherTrunk |
| NID | 12-bit NAC + 4-bit DUID + 47-bit BCH(63,16,11) parity + 1 flag bit | GopherTrunk |
| TSBK | 48 info bits → rate-1/2 trellis → 98 channel dibits → 98-dibit block interleave | GopherTrunk |
| Status symbols | 2 bits every 35 data dibits (stride 36) | GopherTrunk |
| Duty cycle | **control channel transmits continuously**; voice channels key up per call | GopherTrunk |
| Recommended channel rate | 48 kHz = exactly 10 samples/symbol | GopherTrunk |

### 2.2 P25 Phase 2 TDMA **[documented]**

2-slot TDMA in the same 12.5 kHz, **6000 symbols/s → 12000 bps**; **H-DQPSK** outbound (fixed site →
subscriber), **H-CPM** inbound (subscriber → site); AMBE+2 half-rate vocoder.
([sigidwiki](https://www.sigidwiki.com/wiki/Project_25_(P25)),
[RR wiki](https://wiki.radioreference.com/index.php/APCO_Project_25))

**Consequence for a blind estimator, and it is a clean discriminator:** a Phase 1 C4FM channel's
instantaneous frequency has **four discrete modes at ±600/±1800 Hz clocked at 4800 Bd**; a Phase 2
H-DQPSK channel is a *linear* modulation at **6000 Bd** with no such FM-discriminator structure, and
it is **bursty in 30 ms slots**, not continuous. These are distinguishable from the IQ with no prior
knowledge. **[inferred]**

### 2.3 Simulcast — a real hazard, not a footnote

Both BART sites are **simulcast** (multiple transmitters, same frequency, same content). Differential
delay between transmitters smears C4FM symbols; GopherTrunk notes the discriminator path has "no
equalizer and hard-decision FEC", producing decode gaps "on weak or **simulcast-smeared** signals".
**[documented]**

**So a capture taken in an overlap zone can be strong and still undecodable**, and that failure looks
exactly like a demodulator bug. §4 says to record location and §5.4 says how to tell the two apart.
**[inferred]**

---

## 3. What this repo already has (and what it will do on first contact with real BART RF)

Surveyed in the worktree, 2026-09-20. **This section is the answer to "if BART is P25, reuse the M4
decoder rather than rebuild" — the answer is *reuse the front two thirds and expect the back third to
fail, loudly and predictably*.**

### 3.1 Reusable today

| Piece | Path | What it does |
|---|---|---|
| C4FM demodulator | `crates/hk-demod/src/fsk/c4fm.rs` | IQ → dibits. 4800 Bd, ±600/±1800 Hz, discriminator → integrate-and-dump → timing by phase search → 4-level slicer. Needs ≥ 3 samples/symbol (≥ 14.4 kS/s at the channel) and ≥ 64 symbols. |
| Frame-sync correlator | `crates/hk-detect/src/trunk/confirm.rs:41` | The **real** P25 sync `0x5575F5FF77FF`. This half is standards-correct. |
| LMR raster fit | `crates/hk-detect/src/trunk/raster.rs` | 12.5 kHz raster candidacy, spectral only. |
| TSBK semantics | `crates/hk-detect/src/trunk/tsbk.rs` | `IdenUp` band plan, `Grant`, `ChannelMap`, `ServiceOptions`, `P25_ALGIDS`. The *meaning* layer is written. |
| The hunt chain | `crates/hk-pipeline/src/chains/trunk.rs:453` | `trunk-cc-hunt`: occupancy-triggered, takes raw `ci8` off the ring and a raster; the only frequency it is given is where the device says it is tuned. |

**The trigger band `[851.0e6, 869.0e6]` is already in the built-in chain registry**
(`crates/hk-pipeline/src/chains/spec.rs:596-606`). BART's band is inside it, so a replayed capture
should reach `hunt()` with zero configuration — **and T-545 must decide whether that pre-pinned band
is itself a lookup-and-tune violation.** My read: it is a *band gate*, not a frequency lookup — the
system is not told 851.6375 MHz, it is told "digital LMR lives in this stretch of spectrum", which is
band-plan prior knowledge of the kind ADR-0017 permits as an explainer. But it is band-*gated* rather
than measurement-driven, and a stricter reading of T-545's "nothing may pass the frequency to the
system" would require the occupancy trigger to fire from the measured 100 %-duty narrowband
structure instead. **Flagging it for the T-545 author rather than deciding it here.**

### 3.2 What will fail, and the shape of the failure

`crates/hk-detect/src/trunk/confirm.rs:980-1035` carries a comment block written in T-300 for exactly
this moment. This build's P25 framing is **not standards-compliant in five ways**:

1. **CRC variant** — checks CRC-16/CCITT-FALSE; real TSBKs use the **augmented CRC-CCITT** (init 0,
   final XOR 0xFFFF = CRC-16/GSM).
2. **No trellis code** — reads the 12-byte TSBK as 48 dibits straight off the air; real P25 expands to
   98 channel dibits.
3. **No deinterleaver** — packs dibits in arrival order.
4. **No status symbols** — reads contiguously from sync; real frames insert 2 bits every 35 dibits.
5. **No NID** — neither reads the 64-bit NID nor skips past it.

Every one of those is independently confirmed by the public sources in §2.1. **They agree completely
with each other**, which is the strongest evidence in this document, because they were arrived at
separately.

T-300 also established the *shape* of the failure, and it is worth repeating because it will cost
someone a day otherwise: **a correctly-formed real TSBK fails this confirmer with certainty, at the
fixed residue 0x99F6** — the two CRC trailers differ by a constant and CRC is linear in that
difference, so it is not statistical. Meanwhile the symbol layer, which is what anyone checks first,
reports a clean metric. **On the first real capture this presents as a demodulator or front-end
fault.** It is not. Budget the five fixes as T-546 work, and do not "fix" one of them — T-300's
finding is that fixing one of five buys no compliance and makes the remaining gap harder to see.

### 3.3 What is missing entirely

- **P25 Phase 2 / TDMA: nothing.** `SupportLevel::Unimplemented` at
  `crates/hk-detect/src/trunk/support.rs:187`. No H-DQPSK, no CQPSK, no π/4-DQPSK, no linear-modulation
  demodulator anywhere in `hk-demod`. No TDMA burst timing or slot alignment. The classifier can
  *name* `Pi4Dqpsk` as a held-out class but nothing can demodulate it. **So if BART's control
  channel turns out to be TDMA, this target is out of reach and that is the finding, not a bug.**
- **No external P25 oracle.** `plugins/` holds only `dummy` and `readsb`. No SDRTrunk, OP25, DSD or
  Trunk Recorder wrapper. §5.3 proposes adding one as an **oracle**, which is precisely T-299's ask.
- **MAUTO has no engine.** `POST /api/analyze` returns `501 not_implemented`
  (`crates/hk-api/src/analyze.rs`); the search seed (`crates/hk-model/src/classify/seed.rs`) is
  explicitly "an interface, not an engine"; `crates/hk-recipe/src/matching.rs` ranks four existing
  recipes and tunes nothing. The only working auto-selector is `AnalogAuto`
  (`crates/hk-demod/src/mode.rs`), which chooses among WFM/NBFM/AM/SSB/CW. **Nothing today picks a
  demod+decode chain for a digital emission from measurements.** That gap is T-546's whole job.

### 3.4 Why a synthetic fixture cannot substitute

Every trunking fixture in the repo is generated by `py/hkpy/synth/trunking.py`, whose header says
outright *"Nothing here should be read as a standards-compliant P25 encoder"* and whose coding field
reads `"coding": "none"`. **The generator and the decoder were written from the same reading of the
same references**, so a shared misreading passes both suites while the air still fails. That is
T-299's verification hole, and it is why `SIGNAL-087`'s `test_tier` is **`offline-recorded`, not
`offline-synth`**. T-544's capture is the thing T-299 has been blocked on.

---

## 4. The capture plan

**Not yet executed.** Nothing in this section has been run; no `hackrf_*` command has been issued.

### 4.1 Parameters

| Parameter | Value | Why |
|---|---|---|
| **Centre** | **852.456 25 MHz** | Midpoint of 851.0375–853.8625 is 852.45 MHz, but that lands *on* the 12.5 kHz raster, so HackRF's DC/LO spur would sit on a channel. Offsetting by 6.25 kHz puts the spur exactly between two raster slots. |
| **Sample rate** | **4 Msps** (`ci8`) | Spans 850.456–854.456 MHz: all sixteen listed frequencies with ~600 kHz guard each side. 8 MB/s. A 20 Msps capture would also reach 869 MHz cellular and cost 5× the disk for no extra BART. |
| **Baseband filter** | **3.5 MHz** | Widest HackRF setting below the 4 MHz Nyquist span. |
| **Gain** | **Ladder: amp OFF, LNA 16/24/32 dB × VGA 20/30 dB**, 3 s each | The gain that matters is the one that does not clip; §4.3. Amp on only if the whole ladder is quiet. |
| **Antenna** | Whatever telescopic/broadband whip is on the device, **collapsed as short as it goes** | λ/4 at 852 MHz is 8.8 cm; an ANT500-class whip collapses to ~20 cm, so shortest is closest. Recorded verbatim in metadata either way. |
| **Duration** | **One 8–10 minute survey** at the chosen gain | The control channel is continuous so 10 s would prove it exists, but voice **grants** are event-driven and a quiet 30 s window may contain none. Ten minutes at a busy hour is what makes a grant→voice cross-check possible (§5.3). |
| **Time of day** | **Weekday commute peak** (07:00–09:00 or 16:00–18:00 local) | Maximises train-operations and station-operations traffic. |
| **Bias tee** | **OFF**, and recorded as `off` | Passive whip; per `docs/sigmf-extension.md` an absent key reads as *unknown*, so write it explicitly. |

Disk: 10 min × 4 Msps × 2 B = **4.8 GB**. Free space must be checked first (`df -h /` showed ~42 GB
available at the time of writing, which is enough but not generous). **The survey does not get
committed** — see §4.5.

### 4.2 Sequence

1. `hackrf_info` to confirm the device is free and to capture serial / firmware / board rev for
   `core:hw`. Note `pkill -f 'hk serve.*127.0.0.1:8899'` releases the user's demo first, and the user
   is told afterwards so they can restart it.
2. Gain ladder: six short captures, 3 s each, at the centre and rate above.
3. Pick the gain from the clipping and floor analysis in §4.3. Record the whole ladder's numbers in
   the capture README — a rejected gain setting is evidence, not waste.
4. One 8–10 minute survey at the chosen gain.
5. **Also capture ~30 s with the antenna disconnected (50 Ω terminated if one is available)** at the
   same gain, as an internal-spur / artefact reference. Without it, an internally generated birdie in
   the band is indistinguishable from a real emitter — the 2026-09-15 FM capture's README documents
   three such artefacts and is the model to follow.
6. Release the device; tell the coordinator.

### 4.3 Choosing the gain — the 8-bit problem, stated honestly

The HackRF has **no preselector and an 8-bit ADC**. The capture window is 4 MHz wide but the *front
end* sees everything: 851–869 MHz is dense 800 MHz SMR and public safety, and immediately above it
**869–894 MHz is cellular downlink**, which in an urban area is among the strongest things in the
spectrum. Those signals hit the ADC whether or not they are in our 4 MHz window, and they set the
gain we can use. This is the `marginal-8bit` flag on `SIGNAL-087`, and it is the main reason a
capture could be technically successful and still undecodable.

Per gain setting, measure and record:

- **Clipping:** fraction of samples with |I| or |Q| ≥ 127. Target **< 10⁻⁵**; anything above 10⁻³ is
  rejected. Written to `hackriff:clip_count` per capture segment.
- **Headroom:** the 99.9th percentile of |I|,|Q| should sit around 60–100 of 127 — using the ADC
  without railing it.
- **Noise floor vs quantisation floor:** if the floor is within 3 dB of the quantisation floor, set
  `quantisation_limited: true` and go up a gain step.
- **Control-channel SNR:** the continuous carrier's peak-to-floor in a 12.5 kHz bin. C4FM with
  hard-decision FEC and no equaliser wants comfortable margin; **I could not find a published
  threshold figure** and will not invent one — record the measured number and let T-545 set
  tolerances from it. **[unverified]**

The best gain is the one maximising control-channel SNR subject to the clipping bound, not the
highest one.

### 4.4 How to tell from the IQ whether anything real was caught

Five checks, in order. The first four are **blind** — they use no database, no frequency list and
nothing from §1. That matters: these same measurements are what the pipeline ought to be making, so
the analysis script is a sketch of the capability.

1. **Raster structure.** An averaged PSD over the whole capture should show narrow ~8–10 kHz humps
   whose centres land on a **12.5 kHz grid**. Finding the grid without being told it (fit the spacing
   that minimises residual) is itself the first real result.
2. **A continuous emitter.** A spectrogram over the full 10 minutes should show at least one channel
   at essentially **100 % duty for the entire capture**, against neighbours that key up and drop.
   That is the control channel, identified by behaviour alone.
3. **Four-level structure.** Down-convert and decimate that channel to 48 kHz, FM-discriminate, and
   histogram the instantaneous frequency at the symbol instants. **Four modes near ±600 and
   ±1800 Hz** is C4FM and nothing else. A symbol-rate estimate of **4800 Bd** confirms it. If instead
   the eye is linear-modulation-shaped at 6000 Bd and bursty in ~30 ms slots, the control channel is
   **TDMA** and §3.3 applies — report it, do not force it.
4. **Sync lock.** Correlate the dibit stream for `0x5575F5FF77FF`; a genuine P25 control channel
   produces repeated hits at a regular cadence. The repo's existing correlator
   (`confirm.rs:41`) can do this directly, and its sync half is standards-correct — so **sync hits
   with zero CRC-valid blocks is the expected and diagnostic outcome** (§3.2), not a failed capture.
5. **Only then**, and only in the *annotation* step outside the system, compare against §1.2 and
   §1.5: does the occupied set match the published frequencies, is the NAC constant, does it read
   028? Disagreement is interesting and gets written down, not corrected.

**Failure is a legitimate result and gets written up as one**: nothing found, everything clipped, or
a continuous carrier that will not sync. An empty or weak capture that silently becomes an acceptance
fixture would be worse than no fixture at all.

### 4.5 What actually becomes the fixture

`fixtures/README.md` caps committed `.sigmf-data` at **25,000,000 bytes** (Git LFS); larger captures
go to the external store. At 4 Msps `ci8`, 25 MB is **~3.1 seconds** — enough for the control channel
(continuous) but far too short to be sure of containing a grant *and* its voice channel.

So, two artefacts:

- **`fixtures/store/` (external, not committed):** the full 8–10 minute survey, plus the terminated
  reference capture, plus the gain ladder. Indexed in `fixtures/manifest.json` with
  `status: external` and a sha256.
- **`fixtures/hackrf/capture-2026-09-<dd>-bart-851/` (committed, LFS):** a window cut from the survey
  with `py/fixtures/trim.py` (sample-exact, preserves `core:global_index`, `core:datetime`, clip
  counts and provenance). **The window is chosen by the oracle's grant log (§5.3), not by eye**: pick
  the interval containing at least one complete grant whose granted voice channel is visibly active
  within it.

If 3.1 s cannot hold a grant plus its voice burst, the honest options are, in preference order:
(a) accept an external-store fixture and mark the acceptance test as requiring
`$HACKRIFF_FIXTURE_STORE` — the existing `real_fixture()` helper already skips gracefully when LFS
data is absent, so CI degrades rather than breaks; (b) commit **two** short windows, one
control-channel-only and one grant-and-voice; (c) raise the cap, which is a policy change and needs
the user. **I would take (a) for the grant test and (b)'s control-channel window for CI.** Decide
with real numbers once the survey exists, not now.

### 4.6 Metadata to record

Per `docs/sigmf-extension.md` and `fixtures/README.md`, following
`fixtures/hackrf/capture-2026-09-15-fm-band/iq.sigmf-meta` as the worked example:

- `core:hw` — HackRF serial, firmware version, board revision, libhackrf version, **antenna described
  in words** ("telescopic whip, collapsed, ~20 cm, on the device, indoors, window-side" — whatever is
  true).
- `hackriff:provenance` — `device_id`, `tune {center_hz, sample_rate_hz, lna_db, vga_db, amp_on,
  bandwidth_hz}`, `overload`, `quantisation_limited`, `bias_tee: "off"`, `clock_source: "internal"`,
  `clock_locked`, `timestamp_method: "host-arrival"`, `timestamp_error_budget_ns`.
- `hackriff:clip_count` per capture segment.
- `core:datetime`, and **location at the precision the user is comfortable with** — this matters
  technically, not just for provenance: distance and line-of-sight to a BART right-of-way is the
  difference between a decodable capture and simulcast mush, and a future reader cannot interpret the
  SNR without it. Ask before recording anything finer than "a named city".
- A `README.md` beside the capture recording the **gain ladder results**, the terminated-antenna
  reference, and every artefact found — marked `unverified` where it is a hypothesis, exactly as the
  FM-band capture does.

---

## 5. Hidden ground truth — the hard part

T-545's tests assert against this, so it has to be right, and it has to be **produced without the
system ever seeing it**.

### 5.1 The mechanism already exists

`tests/e2e/src/blind.rs` implements strip-and-seal: `strip_truth()` clears annotations, nulls the
description, optionally shifts every capture frequency, writes a stripped copy and symlinks the data;
the mock SDR replays the stripped copy; `TruthVault` seals the original; `assert_truth_free` and
`BlindHandle::finish` verify after the run that the device served only stripped copies and the truth
file was never opened. Truth itself lives in `hackriff:truth` annotations
(`docs/sigmf-extension.md`). **Reuse this; do not invent a parallel path.**

**Additional hazard specific to this fixture:** the truth here includes a *frequency list from a
public database*. `strip_truth` handles annotations, but §1.2's table also exists in this document and
in RadioReference. T-545 must ensure no band-plan or prior-knowledge table containing BART's
frequencies is loaded into the known-signal database in a way that lets the detector *start* there.
The DB may **explain** ("851.6375 is in the 800 MHz trunked LMR allocation"); it may never
**identify** or **tune**.

### 5.2 Three tiers of truth, by how much they can be trusted

- **Tier A — geometry. High confidence, decoder-independent.** Per-emission time–frequency boxes from
  a high-resolution spectrogram: centre, occupied bandwidth, start and stop. Produced by an offline
  annotation script, reviewed by eye, written as `hackriff:truth` annotations. This is what T-545's
  "detect the emission with centre, bandwidth and time extent" asserts against, and it rests on
  nothing but the FFT.
- **Tier B — modulation. High confidence, measurement-based.** For the continuous channel: four-level
  deviation histogram at ±600/±1800 Hz and a measured 4800 Bd symbol rate → "C4FM". For any bursty
  channel: whichever of C4FM / linear-6000-Bd-TDMA / analogue FM the same measurements say. Recorded
  as *measured values with their uncertainties*, not as protocol labels — "4-level FM, 4798 ± 3 Bd,
  outer deviation 1796 Hz" is truth; "P25" is an interpretation.
- **Tier C — content. This is where it gets hard.** The decoded messages: NAC, system/WACN/RFSS/site
  identity, `IDEN_UP` band plan, and the grant log with talkgroup, source ID, channel and timestamp.

### 5.3 Establishing Tier C without trusting any single decoder

This repo's own P25 decoder **cannot** produce Tier C (§3.2 — it will produce zero CRC-valid blocks
with certainty), and even if it could, using it would be circular: the thing under test cannot author
its own answer key.

**The proposal, which is exactly T-299's long-blocked ask:** run an **independent oracle** —
SDRTrunk, OP25 or DSD+ — offline over the same IQ, behind the C22 plugin process boundary per
ADR-0010 (these are GPLv3), **gated to the hardware/off-air tier so CI never depends on it**. It is an
oracle, never a runtime. T-299 has been blocked on "a real off-air P25 capture exists"; T-544's
capture is that unblock, and this should be recorded on T-299 when the capture lands.

**And then do not trust it either.** The oracle's output is cross-validated against Tier A:

1. Every **grant** the oracle reports names a channel number; `IDEN_UP` turns that into a frequency.
2. Tier A's boxes, derived from the spectrogram alone, say what was actually radiating and when.
3. **A grant enters the truth list only if RF energy appears at the granted frequency within the
   grant window.** A grant with no corresponding energy is dropped (the call was on the other
   simulcast site, or the oracle misdecoded). Energy with no grant is kept as a Tier-A-only emission
   with no content truth.

Two independent chains — one a protocol decoder, one an FFT — have to agree before anything is called
truth. That is what makes this an answer key rather than one program's opinion. Internal
consistency is a further free check: the NAC should be constant, System ID 338 and WACN 92762 should
repeat, talkgroup IDs should land in the ranges of §1.5 — and **a mismatch there is recorded as a
finding about the database, not corrected into agreement.**

### 5.4 If the oracle cannot decode it either

Then the capture is too weak, too clipped or too simulcast-smeared, and **that is the finding**. The
fixture ships as **Tier A + Tier B only**: real off-air 800 MHz trunked RF with honest
time–frequency and modulation truth, and no content truth. T-545 then asserts detection, extent and
blind modulation estimation — which is still a genuine, useful, currently-failing blind test — and
the decode assertion waits for a better capture. Distinguishing "our decoder is wrong" from "this
capture is undecodable" is the *entire* value of having an independent oracle, and it is worth
standing up for that reason alone.

---

## 6. Honest read on whether this will work

**Reach at 851 MHz: probably fine, and the risk is dynamic range rather than sensitivity.** BART's
above-ground sites are trunked 800 MHz repeaters running high ERP from fixed sites across the Bay
Area, and a continuously-keyed control channel is the easiest possible target: always on, so you can
average as long as you like. A collapsed telescopic whip at 852 MHz is electrically longer than λ/4
(8.8 cm) and mismatched, but mismatch loss on a strong local repeater is not what will stop this.
**[inferred]**

**The three things that actually could stop it, ranked:**

1. **8-bit dynamic range with no preselector** (§4.3). Cellular downlink at 869–894 MHz and the dense
   800 MHz public-safety band will set the usable gain. This is a known, documented limitation of the
   chosen front end — `marginal-8bit` exists as a `fit_flag` for this exact reason — and the gain
   ladder is the mitigation. If it defeats us, the answer is an 800 MHz bandpass filter, which is a
   **hardware purchase and a user decision**, not something to improvise.
2. **Simulcast smearing** (§2.3). Strong signal, unreadable symbols, and it looks like a code bug.
   The terminated-antenna reference and the independent oracle are what disambiguate it.
3. **Underground-only reception.** The Underground Simulcast site (RFSS 1) feeds in-tunnel radiating
   cable and is probably **not** receivable above ground. Expect the Above-Ground site's ten
   frequencies (851.0375–853.3625) and treat the other six as likely absent. Their absence is not a
   capture failure. **[inferred]**

**The honest bottom line.** Phase 1 of this target — detection, time-extent, blind C4FM estimation,
sync lock — is likely to succeed on the first capture. Phase 3 (T-546, an actual decode) depends on
two things that are *not yet true*: the five framing fixes of §3.2, none of which have been done; and
the control channel being FDMA C4FM rather than TDMA, which §1.3 says is very likely but which **no
source I found states about BART specifically**. The capture will settle that question in about ten
seconds of analysis, and settling it is reason enough to take the capture even if everything
downstream turns out to be harder than hoped.

---

---

## 7. Capture attempt 1 (2026-09-20, ~20:38–20:50 local): BART IS NOT RECEIVABLE HERE

**Result: no BART, and no trunked control channel anywhere in the 800 MHz downlink band.**
Measured, not inferred. §6 called the antenna "probably fine" and the risk "dynamic range"; both
turned out to be right about the *receiver* and wrong about the *outcome* — the receiver is working
well and BART simply is not present.

The status line at the top of this document stood as "capture NOT YET TAKEN"; it now reads: taken,
and negative. **No fixture was produced and none should be manufactured from this material.**

### 7.1 Equipment state

HackRF One serial `0000000000000000d2b861dc263bc293`, firmware 2026.01.3 (API 1.10), board revision
older than r6, on its own USB bus. Antenna: whatever whip was already attached — **not touched**,
because changing the RF setup is reserved to the user (CLAUDE.md). **Location: not recorded.** No
existing capture in `fixtures/` states one, so there was no convention to follow and I would rather
leave it blank than invent a city. This matters for §7.6 and is the first thing to fix on a retry.

### 7.2 The receiver is working — three independent proofs

This has to come first, because "found nothing" is worthless unless the receiver is known good.

1. **VHF reference.** At 100.8 MHz, 4 Msps, LNA 32 / VGA 30 / amp off: RMS 11.1 LSB, peak 47/127,
   and FM broadcast carriers at 98.899, 99.700, 100.000 and 101.299 MHz, the strongest +33 dB over
   floor. The antenna and receive chain are alive.
2. **UHF reference.** At 875 MHz, 20 Msps, LNA 32 / VGA 40 / amp on: **39 % of samples clipping**,
   RMS 132. Cellular downlink is enormous here. The receiver reaches 875 MHz with power to spare, so
   a null at 852 MHz is not a UHF path failure.
3. **The 800 MHz band itself is far from empty** — 28 occupied 12.5 kHz channels (§7.4), the
   strongest 23 dB over floor. We are hearing 800 MHz LMR perfectly well. Just not BART's.

### 7.3 Gain ladder, and why the first pass looked dead

The planned ladder (amp **off**, LNA 16/24/32 × VGA 20/30) was run first and looked like a dead
input: RMS 2.5–3.4 LSB, peak 11/127, everything quantisation-limited. That was a **planning error in
§4.1, not a hardware fault** — at 852 MHz with the amp off the HackRF's noise figure is poor, and the
ladder simply sat below the ADC's useful range. The plan said "amp on only if the whole ladder is
quiet"; the ladder was quiet, and amp-on is where the band actually appears.

| Config | clip | p99.9 | max | RMS | verdict |
|---|---|---:|---:|---:|---|
| LNA 32 / VGA 30 / amp off | 0 | 7 | 11 | 3.4 | quantisation-limited, unusable |
| **LNA 32 / VGA 40 / amp on** | **1.0 × 10⁻⁶** | **63** | **111** | **26.2** | **chosen** |
| LNA 32 / VGA 54 / amp off | 3.4 × 10⁻⁴ | — | — | — | usable, 14 dB less pre-mixer gain |
| LNA 40 / VGA 62 / amp on | 0.81 | 128 | 128 | 168 | grossly overdriven |

`LNA 32 / VGA 40 / amp on` is the setting to reuse: essentially no clipping, p99.9 at half scale.

### 7.4 What is actually on the air here (851–862 MHz, 30 s, 12 Msps)

Blind sweep of 850.9–862.1 MHz, per 12.5 kHz channel, duty measured over 10.9 ms rows:

| Channel MHz | dB over floor | duty |
|---|---:|---:|
| 852.3750 | 18.9 | 88.2 % |
| 853.6375 | 20.1 | 80.5 % |
| 852.2000 | 17.2 | 79.0 % |
| 851.2375 | 21.3 | 61.9 % |
| 851.8000 | 21.9 | 54.7 % |
| 853.7750 | 19.1 | 48.6 % |
| 853.8750 | 23.2 | 43.4 % |
| 853.2125 | 15.1 | 34.7 % |
| …20 more | 6–19 | < 30 % |

**28 occupied channels out of 880 probed. The highest duty in the entire band is 88 %. None exceeds
95 %.** A P25 control channel transmits *continuously* — that is its defining, protocol-independent
signature and the whole basis of `SIGNAL-085`. **There is no control channel here to find.** The
traffic is conventional (non-trunked) LMR, which has no control channel by definition.

Modulation check on the four busiest channels — down-convert to 48 kHz, FM-discriminate, histogram
the instantaneous frequency: kurtosis **2.86 / 3.17 / 3.39 / 3.58** against ~1.6–2.0 for a flat
four-level C4FM distribution; |Δf| 99th percentile ~7 kHz; a continuous Δf distribution clustered at
zero rather than four discrete lobes at ±600/±1800 Hz; and **no symbol-rate structure near 4800 Bd**.
These are **analogue FM voice**, not P25. So there is no digital LMR fixture to salvage here either.

### 7.5 BART's own channels: empty, not weak

Every one of the sixteen published frequencies (§1.2), probed in both the 69 s / 4 Msps capture and
the 30 s / 12 Msps sweep:

| | |
|---|---|
| dB over noise floor | **0.0 – 1.4 dB** on fifteen of sixteen |
| duty | **0.0 %** on fifteen of sixteen |

The sole apparent exception is 853.8625 at 14.2 dB / 3.3 % — which sits **12.5 kHz from the real,
busy emitter at 853.8750** and is its filter skirt, not BART. Everything else is indistinguishable
from noise.

This is the difference that matters: the nine live channels are 15–23 dB over floor while BART's are
at 0 dB. **BART is not attenuated, it is absent.** A marginal signal would have shown as a few dB of
excess; nothing did.

### 7.6 Two measured results worth keeping regardless

**(a) The receiver's clock is −9.6 ppm off, and it nearly sent this investigation down a rabbit
hole.** The emissions first appeared to sit *off* the US 800 MHz 12.5 kHz raster by awkward amounts
(−29 to −80 kHz from published channels), which briefly looked like evidence they were spurious. They
are not: fitting a single constant correction across all nine puts every one on the raster.

| Observed MHz | Corrected | Channel | residual |
|---|---|---|---:|
| 851.24531 | 851.23711 | #18 = 851.2375 | −390 Hz |
| 851.80781 | 851.79961 | #63 = 851.8000 | −390 Hz |
| 852.20820 | 852.20000 | #95 = 852.2000 | 0 Hz |
| 852.38398 | 852.37578 | #109 = 852.3750 | +780 Hz |
| 853.64570 | 853.63750 | #210 = 853.6375 | 0 Hz |
| 853.88203 | 853.87383 | #229 = 853.8750 | −1170 Hz |

**Best constant correction −8200 Hz at 852.456 MHz = −9.6 ppm**, median raster residual 390 Hz. That
is ordinary for a HackRF One (plain crystal, no TCXO; HackRF Pro adds one), but it is **larger than
the 12.5 kHz raster tolerance matters at** — nearly ⅔ of a channel — so any raster-fitting or
band-plan comparison this project does at 800 MHz must estimate and remove it rather than assume
zero. Worth its own calibration ticket under C05; it is exactly the "blind raster fit" the detector
should be doing anyway, and it fell out of the data in one line.

**(b) The candidate emissions are real, proven by a gain-slope test — which also substitutes for the
antenna-disconnected reference I could not take.** The coordinator specifically wanted the terminated
capture so an internal birdie could not masquerade as a signal. **I could not take it: disconnecting
the antenna is a physical change to the RF setup, reserved to the user.** So I used a different and
arguably stronger discriminator — the RF amp and LNA sit *before* the mixer, so stepping pre-mixer
gain while compensating with post-mixer VGA separates real signals (1 dB per dB) from third-order
intermodulation (3 dB per dB), which was a live hypothesis given cellular clips the ADC here.

| Pre-mixer gain step | real signal predicts | IM3 predicts | **measured** |
|---|---|---|---|
| 46 → 32 dB (amp off) | +14 dB | +42 dB | **+9.9 to +11.8 dB** |
| 32 → 16 dB (LNA down) | +16 dB | +48 dB | **+14.0 to +16.6 dB** |

All nine candidates track 1:1 across a 30 dB range. **Real external signals, not intermodulation and
not internal birdies.** (The amp step measuring ~11 dB rather than 14 is the amp's real gain plus
slight compression in the amp-off leg; the LNA step is exact.) This test needs no hardware change,
takes three captures, and should be the standard way this project separates real from spurious.

### 7.7 What this means for T-544/545/546

- **T-544 cannot be completed at this location with this antenna.** No fixture; nothing to annotate.
  Per the ticket's own DoD, an empty capture is a finding, and this is the finding.
- **`SIGNAL-087` survives and does not need changing.** It was deliberately written as "a local
  800 MHz trunked control channel… BART's above-ground simulcast is the reference instance; the claim
  is the blind chain, not that one system." Any reachable trunked control channel satisfies it. The
  problem is that *no* trunked control channel is reachable here, so the use case needs a different
  instance, not a different definition.
- **T-545 and T-546 are blocked on a fixture that does not exist.** They should not be started
  against synthetic material — §3.4 is precisely why that would prove nothing.
- **T-299 is NOT unblocked.** The note added to it on this branch was written in the expectation that
  this capture would land. It did not. The note now says so.

### 7.8 What to try next, in order of cost

1. **Establish where we are.** Record the location (named city) and the distance and line-of-sight to
   the nearest BART right-of-way or above-ground BART site. Without that, "BART not receivable" is a
   measurement without a context, and we cannot tell a 3 km indoor-null problem from a 40 km
   out-of-range one. **This needs the user and costs nothing.**
2. **Move the antenna** — a window, outdoors, or higher. Indoor building loss at 850 MHz is easily
   15–25 dB, and BART's channels are ≥ 15–23 dB below emitters we hear fine, which is exactly the
   right order of magnitude. **The cheapest plausible fix, and it needs the user** (physical RF
   change).
3. **A better antenna for 800 MHz** — a proper 850 MHz whip or a small directional. The present whip
   works (FM is strong, cellular clips), but nothing about it is matched at 852 MHz.
4. **Reconsider the target.** If no trunked system is reachable, the honest options are to find one
   that is (a wideband survey for any 100 %-duty narrowband emission across the LMR bands is the
   blind, on-mission way to look, and is a good exercise in its own right), or to accept that
   `SIGNAL-087`'s first instance is not BART.
5. **Do not** trim any of this material into an acceptance fixture. It contains no control channel,
   no digital LMR and no BART. A fixture built from it would encode a false premise into T-545 and
   T-546 permanently.

### 7.9 Data retained

In the session scratchpad (not committed; ~1.2 GB, delete when read): the six-point amp-off gain
ladder, the three-point pre-mixer gain sweep of §7.6(b), a 69 s 4 Msps capture at 852.456 MHz, a 30 s
12 Msps sweep of 850.9–862.1 MHz, plus VHF and cellular reference captures. Every number in §7 is
reproducible from them. **None of it is fixture material** — it is diagnostic evidence for this
write-up, and its value is entirely in the tables above.

## Sources

Every claim above is tagged with its confidence; these are the underlying sources.

- [RadioReference — Bay Area Rapid Transit (BART) P25 Trunking System, SID 12049](https://www.radioreference.com/db/sid/12049) — system type, IDs, sites, frequencies, talkgroups. Curated hobbyist database; last updated 2025-10-12.
- [FCC ULS — licence WPSH605, San Francisco Bay Area Rapid Transit District (licKey 1986151)](https://wireless2.fcc.gov/UlsApp/UlsSearch/license.jsp?licKey=1986151) — licensee and 806-817/851-862 MHz trunked public-safety service. **Page returns 403 to automated fetch; only the search-index description was read.**
- [RadioReference forums — "BART 800 MHZ EDACS" (thread 468827)](https://forums.radioreference.com/threads/bart-800-mhz-edacs.468827/) — 2024 EDACS shutdown, 853.0375 as data channel, the Phase 1 / Phase 2 mix claim.
- [RadioReference forums — "BART P25 System" (thread 444583)](https://forums.radioreference.com/threads/bart-p25-system.444583/) — 2022 status: underground-only, Phase 1, interop use, above-ground upgrade.
- [RadioReference forums — meaning of the database Mode column (thread 389164)](https://forums.radioreference.com/threads/what-does-t-in-the-mode-column-in-the-database-designate.389164/) — `D` digital, `T` TDMA, `E`/`e` encryption.
- [RadioReference wiki — APCO Project 25](https://wiki.radioreference.com/index.php/APCO_Project_25) — Phase 1/2 rates, control-channel modulation on Phase 2 systems, TDMA control channels as rare and Harris-only, NAC/System ID/WACN/talkgroup/radio-ID field widths.
- [sigidwiki — Project 25 (P25)](https://www.sigidwiki.com/wiki/Project_25_(P25)) — bands, 12.5 kHz, 4800 Bd / 9600 bps Phase 1, 2-slot 12000 bps Phase 2, H-DQPSK / H-CPM.
- [GopherTrunk — P25 End to End 01: C4FM & the shape of a P25 carrier](https://gophertrunk.org/blog/deep-dives/p25-end-to-end-01-c4fm-carrier/) — occupied bandwidth, deviations, slicer thresholds, RX filter, control vs voice duty cycle, 48 kHz channel rate. Independent blog; corroborated by VIAVI and sigidwiki on every number used.
- [GopherTrunk — Protocol Decoders 02: P25 Phase 1 NID, sync & the FEC that gates lock](https://gophertrunk.org/blog/deep-dives/protocol-decoders-02-p25-phase-1-physical-layer/) — frame sync hex, NID/BCH(63,16,11), dibit mapping, TSBK 48→98 dibits, trellis, 98-dibit interleave, status-symbol stride.
- [VIAVI — Understanding P25 Modulation Fidelity](https://www.viavisolutions.com/sites/default/files/support/understanding-p25-modulation-fidelity-application-notes-en.pdf) — ±1800 / ±600 Hz deviation at a 4800 Hz symbol clock, raised-cosine α = 0.2. Vendor application note.
- BART's own engineering standard, *Section 33 83 01 — Radio Network / Trunked Radio System* (`webapps.bart.gov/BFS/.../33 83 01.pdf`, releases R3.1 / R3.1.3 / R3.2) — **could not be retrieved: expired TLS certificate, and the host is unreachable from this environment.** It is the one primary source that would settle vendor, protocol and channel count, and is worth retrieving by hand.
- Internal, in this repository: `crates/hk-detect/src/trunk/confirm.rs:980-1035` (T-300's five-way non-compliance survey and failure-mode analysis), `crates/hk-demod/src/fsk/c4fm.rs`, `crates/hk-pipeline/src/chains/trunk.rs`, `py/hkpy/synth/trunking.py`, `tests/e2e/src/blind.rs`, `fixtures/README.md`, `docs/sigmf-extension.md`, `fixtures/hackrf/capture-2026-09-15-fm-band/`.
