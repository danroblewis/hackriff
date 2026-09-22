# 18 — Decoder coverage: GNU Radio as the reference set

Status: **direction / planning** (2026-09-20, from the user); **§§6–9 added 2026-09-21 by T-554** as the
per-family **disposition audit** and the **ranked hk-blocks catalogue gap list**. Milestone **MAUTO**
(design track; build gated behind the robustness work). No product code comes from this document.
§4's proposed reference rule was **cancelled by the user** (T-555) and binds nothing; the standing rule
is ADR-0010's process boundary, and the audit in §6 applies that rule rather than §4. Contracts it touches:
[ADR-0010](adr/0010-language-and-licence-ledger.md) (licence boundary),
[ADR-0003](adr/0003-process-plugin-model.md) (plugin process model),
[ADR-0011](adr/0011-decoder-workbench-contracts.md) (blocks and recipes),
[ADR-0015](adr/0015-decoder-synthesis-contracts.md) (synthesis and templates).

**The user's direction, verbatim:**

> "We expect to support many more demod/decode types. Target the whole suite of GNU Radio decoders as
> the reference set (we can use their code as reference). Per ADR-0010 GPLv3 GNU Radio code stays
> behind the plugin process boundary. Fold this into the MAUTO/decoder roadmap — a GNU-Radio-decoder-
> coverage direction, not a rewrite."

## 0. The argument in one page

GNU Radio's decoder ecosystem is the most complete public answer to "what can be demodulated and
decoded from RF", and it is the right **reference set**: a map of the territory, a specification
corpus, and a source of protocol parameters. It is a poor **acquisition target**. Four findings from
the survey in §1 drive that split, and the rest of this document supports them.

1. **Most of the out-of-tree ecosystem is not alive.** §1 surveys 65 modules: **38 % last pushed
   over four years ago, 22 % declare GNU Radio ≤ 3.8 and will not build against 3.10** — and that
   sample was chosen for notability, so the real picture is worse. There is **no maintained,
   version-aware index**: CGRAN lists 163 modules, only 1 % touched in the past year, with the
   supported-version column blank for 67 % of them. "Wrap them all" is not on the table even before
   the licence question, because much of what would be wrapped needs porting before it runs, at
   which point the wrapping cost is no longer the cost.
2. **The most valuable part of GNU Radio is in-tree, not out-of-tree**, and that inverts the obvious
   reading of the direction. `gr-digital`, `gr-fec`, `gr-trellis` and `gr-analog` are mature,
   documented, tested, and — crucially — **not subject to OOT bitrot**. They contain exactly the
   block families §3 ranks as the next investment: Costas loops, `symbol_sync`, constellation soft
   decoders, equalisers, OFDM channel estimation, Viterbi, Reed–Solomon, LDPC, polar and turbo. The
   reference set worth reading closely is the in-tree signal-processing library; the OOT decoders are
   mostly a *map of what people have found worth decoding*.
3. **A wrapped plugin is invisible to the thing this project is for.** ADR-0015's synthesis engine
   searches over *recipe structures* — a plugin is a black box consumer, not a searchable structure.
   Every family that lands as a plugin is a family MAUTO cannot reason about, cannot refine from the
   processed output, and cannot offer as a ranked hypothesis. Plugins are the escape hatch that
   [docs/13](13-m1-decoder-workbench.md) already describes them as, and the escape hatch should not
   become the roadmap.
4. **The cheapest coverage is not code at all.** ADR-0015 §4's **template** is a recipe or skeleton
   reference plus parameter ranges, priors and plausibility checks — protocol *facts*, not
   implementation. For every protocol whose structure the existing block catalogue can already
   express, coverage is a data-entry cost, not an engineering cost. That is where the reference set
   pays most, and it crosses the licence boundary cleanly because facts are not expression (§4).

So the direction is: **read the whole suite, wrap almost none of it, and convert most of what it
knows into templates and block-catalogue additions.** The ranked roadmap in §3 is ordered by that
rule, not by protocol fame. ADS-B, pagers and radiosondes are the well-known cases and are *already
covered* — ADS-B by both the readsb plugin (T-015) and a native recipe (tutorial 4), POCSAG by a
native recipe (tutorial 2). A coverage roadmap that spends itself there has missed the target.

One calibration on what "the whole suite" is worth, from §1: **GNU Radio has no maintained POCSAG or
FLEX decoder at all** — every GR paging module is a *transmitter*, and decoding lives in multimon-ng,
outside GNU Radio entirely. hackriff's paging coverage already exceeds GNU Radio's. The suite is a
map with real gaps in it, not a superset to catch up to.

### The trap this direction must not fall into

[docs/13](13-m1-decoder-workbench.md) opens with the user's M1 instruction, and it constrains this
document directly:

> "The user does **not** want a Rust module written per protocol. Decoders are **built inside the
> exploratory system** from reusable blocks and **declarative recipes**, discovered from the signal,
> **not coded from a spec**. External decoders (rtl_433, readsb) stay only as a long-tail escape
> hatch and as **test oracles**, never the headline path."

A naive reading of "target the whole suite of GNU Radio decoders" would reinstate exactly what that
paragraph rejects — a protocol catalogue, one hand-written artefact per entry, just in recipes
instead of Rust modules. It would also contradict CLAUDE.md's "general, not a decoder catalogue".

Templates are the reason this direction does not collapse into that. **A template is a prior that
seeds a search, not a decoder that replaces one.** ADR-0015 §4 is explicit and its safeguards are
the point: a template orders which hypotheses are tried first and narrows their parameter ranges;
it never ranks a result, never confirms a signal, and its band hints only raise the rank of an
emitter *already detected* there. So importing a thousand protocols' parameters makes the blind
search **faster and better informed**, and cannot make it dishonest. The discovery path is still the
headline path, and a signal that matches nothing in the library still gets the open search with its
reserved budget floor.

The test to apply to any coverage work: *does this make the system better at decoding something it
has never seen?* Templates and blocks pass. A wrapped plugin per protocol does not.

---

## 1. What the ecosystem actually contains

**Method and its limits.** Surveyed 2026-09-20. Last-push dates, licences and archive flags come
from the GitHub and GitLab REST APIs; the **target GR version** column comes from grepping each
repo's top-level `CMakeLists.txt` for `find_package(Gnuradio "…")`, which is build-time truth rather
than a README claim. Status labels (*Active* < 1 y, *Maintenance* 1–2 y, *Dormant* 2–4 y,
*Abandoned* > 4 y / archived / pinned ≤ 3.8) are a judgement derived from those dates; the dates are
not. **The 65-module sample below is biased toward notable modules** — they were chosen by family,
so the healthy end is over-represented and the ecosystem-wide picture is *worse* than these numbers,
not better. Items that could not be confirmed are marked **unverified**.

Three corrections to [docs/03](03-sdr-software.md), which should be applied when that document is
next touched:

- **gr-inspector is not "GR 3.8".** Its `main` branch declares `find_package(Gnuradio "3.10")` and
  it carries a `maint-3.10` branch. Last commit 2025-03-03. Accurately: **3.10-ready but dormant
  ~18 months** ([gnuradio/gr-inspector](https://github.com/gnuradio/gr-inspector)).
- **GR4's MIT core is not at the repo docs/03 implies.** `fair-acc/gnuradio4` is the historical
  prototype and is **LGPL-3.0-or-later** with a static-linking exception. The current official core
  is [`gnuradio/gnuradio4-core`](https://github.com/gnuradio/gnuradio4-core), plus `-library` and
  `-blocks` — all three **MIT**, all three pushed within days of this survey.
- **CGRAN is alive**, not redirected to the wiki — but ~11 months stale (below).

### 1.1 There is no maintained, version-aware index

| Index | Entries | State |
|---|---|---|
| [CGRAN](https://www.cgran.org/) | **163** module rows | Live but stale: newest entry's most-recent-commit is **2025-10-07**. Auto-generated from PyBOMBS recipes; descriptions are visibly templated (several unrelated modules share the string "Short description of gr-aep"), so it is scripted ingestion, not curation. |
| [wiki `OutOfTreeModules`](https://wiki.gnuradio.org/index.php/OutOfTreeModules) | 0 — it is a **tutorial**, not a list | Carries a deprecation banner; last edited 2023-02-03 |
| [GitHub topic `gnuradio`](https://github.com/topics/gnuradio) | 414 repos | Uncurated; mixes core, apps, receivers and OOTs |
| [GitHub topic `gnuradio-oot`](https://github.com/topics/gnuradio-oot) | **1** repo | The community never converged on a tag |
| [gr-recipes](https://github.com/gnuradio/gr-recipes) (PyBOMBS) | 53 `gr-*.lwr` | Pushed 2026-03-24 |
| [PyBOMBS](https://github.com/gnuradio/pybombs) (the tool) | — | **Dormant**: last code push 2022-04-25; soft-deprecated for conda-forge / Radioconda |

CGRAN's own staleness, computed from its table (162 of 163 rows carry a parseable date):

| Entry's last commit | Share |
|---|---|
| < 1 year ago | **1 %** |
| < 2 years | 16 % |
| < 3 years | 23 % |
| < 5 years | 49 % |

And CGRAN's "GNU Radio supported versions" column is **blank for 109 of 163 entries (67 %)**; only
21 mention 3.10 at all. The one index that tries to record compatibility does not record it for
two-thirds of what it lists.

### 1.2 The maintenance reality

Across the 65 OOT modules surveyed:

| Last push | Share | | Declared GR version | Share |
|---|---|---|---|---|
| < 1 year (Active) | **20 %** | | 3.10 | **35 %** |
| 1–2 years (Maintenance) | 14 % | | 3.9 | 22 % |
| 2–4 years (Dormant) | 28 % | | 3.8 | 8 % |
| > 4 years (**Abandoned**) | **38 %** | | 3.7 or older | 14 % |
| | | | unversioned / no CMakeLists | 22 % |

**22 % declare GR ≤ 3.8 and will not build against 3.10 without porting**, and another 22 % declare
no version at all, which in practice means "whatever was current when it was written". Four of the
65 are GitHub-archived.

**The abandoned tail is not the obscure end.** gr-gsm (1498★), rpp0/gr-lora (642★), gr-air-modes
(483★) and gr-bluetooth (219★) are all either pinned to ≤ 3.8, archived, or both. Popularity did not
keep them alive; it is not a proxy for buildability.

Bitrot is a known, documented problem with identifiable causes:

- **SWIG → pybind11.** The announcement to discuss-gnuradio was explicit: *"If you're on master,
  we'll be breaking your OOT."* Bindings must now be declared by hand rather than parsed from
  headers ([discuss-gnuradio 2020-06](https://lists.gnu.org/archive/html/discuss-gnuradio/2020-06/msg00095.html)).
- **3.7 → 3.8** broke CMake version detection and generated bindings
  ([gnuradio#2075](https://github.com/gnuradio/gnuradio/issues/2075)).
- **Official porting guides exist for every hop** — which is itself the evidence that every hop
  broke things ([3.8 guide](https://wiki.gnuradio.org/index.php/GNU_Radio_3.8_OOT_Module_Porting_Guide),
  [3.10 guide](https://wiki.gnuradio.org/index.php/GNU_Radio_3.10_OOT_Module_Porting_Guide)).
  A concrete casualty: gr-leo failed with 98 compiler errors against 3.10.1.1
  ([gr-leo#48](https://gitlab.com/librespacefoundation/gr-leo/-/issues/48)).
- **The mechanism, shown by the healthiest OOT in the ecosystem.** gr-satellites runs *parallel
  release lines per GR major version* — 3.8 → v3.17.x, 3.9 → v4.10.x, 3.10 → v5.3.x — and its
  maintainer said he lacked time to maintain the 3.9 line himself
  ([discussion #459](https://github.com/daniestevez/gr-satellites/discussions/459)). If the
  best-resourced decoder OOT treats each GR version as a separate ongoing maintenance line, a
  single-maintainer module simply picks one and freezes.

**What does not exist, stated plainly:** there is no OOT build-health dashboard, no CI matrix across
OOTs, and no published count of how many OOTs build against 3.10. The figures above are this
survey's own computation, not a citation. *(A GRCon21 "Maintenance Update" talk exists at
[youtube](https://www.youtube.com/watch?v=mSS0pFfWRHY) but its content could not be retrieved —
**unverified**.)*

### 1.3 The most valuable part is in-tree, not out-of-tree

This is the survey's most useful finding for this project, and it inverts the obvious reading of
"target the whole suite of GNU Radio decoders". GNU Radio's **in-tree** signal-processing modules are
mature, documented, tested, uniformly GPLv3, and **not subject to OOT bitrot** — and they are
precisely the block families §3 ranks as Tier 2 and Tier 3.

| In-tree module | What it contains | RX / TX |
|---|---|---|
| **gr-digital** | Costas loop, FLL band-edge, `symbol_sync_cc`, M&M and polyphase clock recovery, constellation objects + soft decoder, linear and decision-feedback equalisers, CRC-16/32, access-code correlators, HDLC deframer, OFDM chanest / equaliser / serializer, `header_payload_demux`, MPSK SNR estimators | RX + TX |
| **gr-fec** | Convolutional (Viterbi) encode/decode, CCSDS R=1/2 K=7, **LDPC** (bit-flip, G/H-matrix), **polar** (SC, SC-list), repetition, TPC, **Reed–Solomon**, puncture/depuncture | RX + TX |
| **gr-trellis** | FSM-based convolutional/trellis coding, Viterbi, SISO, **PCCC and SCCC turbo**, interleavers, constellation metrics | RX + TX |
| **gr-analog** | Quadrature demod, FM detector, PLL carrier-tracking / freq-det / ref-out, AGC1/2/3, power and CTCSS squelch, CPFSK | RX + TX |
| **gr-vocoder** | Codec2, GSM full-rate, CVSD, G.721/723, A-law/µ-law, FreeDV | RX + TX |
| **gr-dtv** | Broadcast TV — see the RX/TX split below | mixed |
| **gr-channels** | Fading and impairment models (flat, selective, dynamic, CFO, SRO) | simulation |

**gr-dtv is half a receiver, and which half matters.** Verified by listing its sources: **ATSC**
(`atsc_equalizer`, `atsc_fpll`, `atsc_sync`, `atsc_viterbi_decoder`, `atsc_rs_decoder`,
`atsc_deinterleaver`, plus `uhd_atsc_rx.grc`) and **DVB-T** (`dvbt_ofdm_sym_acquisition`,
`dvbt_demod_reference_signals`, `dvbt_demap`, `dvbt_viterbi_decoder`, `dvbt_reed_solomon_dec`) are
**genuine receive chains**. **DVB-S2, DVB-T2, the DVB common layer and CATV are transmit only** —
DVB-S2 receive is an out-of-tree gap filled by
[gr-dvbs2rx](https://github.com/igorauad/gr-dvbs2rx).

Release cadence, for context: the latest 3.10 is **v3.10.12.0, 2025-02-20** — no point release in
19 months.

### 1.4 The OOT modules by family

*(Licence column: verified from the repo's own `LICENSE`/`COPYING`, a source header, or the GitHub
licence API. "none" means no licence file was found — treat those as **not safe to vendor**.)*

**Aviation / maritime**

| Module | Last push | Target GR | Licence | Status |
|---|---|---|---|---|
| [gr-adsb](https://github.com/mhostetter/gr-adsb) | 2026-04-09 | 3.10 | GPL-3.0 | **Active** |
| [gr-ais](https://github.com/bistromath/gr-ais) | 2026-09-12 | 3.10 | GPL-3.0-or-later (README) | **Active** — the live AIS one |
| [gr-air-modes](https://github.com/bistromath/gr-air-modes) | 2021-02-12 | 3.8 | GPL-3.0 | Abandoned |
| [gr-aistx](https://github.com/drmpeg/gr-aistx) | 2021-09-16 | 3.8 | GPL-3.0 | Abandoned (TX only) |
| [gr-acars2](https://github.com/antoinet/gr-acars2) | 2016-04-10 | 3.7 | none | Abandoned |
| **VDL2** | — | — | — | **No GNU Radio OOT exists.** The real tool is [dumpvdl2](https://github.com/szpajder/dumpvdl2) (2026-08-01, GPL-3.0, active), not GR |
| **HFDL** | — | — | — | **No GNU Radio OOT exists.** [dumphfdl](https://github.com/szpajder/dumphfdl) (2026-05-31, GPL-3.0, active) |

**Satellite / space**

| Module | Last push | Target GR | Licence | Status |
|---|---|---|---|---|
| [gr-satellites](https://github.com/daniestevez/gr-satellites) | 2026-09-11 | 3.10 | GPL-3.0 | **Active** (982★) — the ecosystem's flagship |
| [gr-satnogs](https://gitlab.com/librespacefoundation/satnogs/gr-satnogs) | 2026-02-21 | 3.10 | GPL-3.0-or-later | **Active** — on **GitLab**; the [GitHub mirror](https://github.com/satnogs/gr-satnogs) is archived and dead |
| [gr-iridium](https://github.com/muccc/gr-iridium) | 2026-07-02 | 3.9 | GPL-3.0-or-later | **Active** (491★) |
| [gr-dvbs2rx](https://github.com/igorauad/gr-dvbs2rx) | 2024-07-28 | 3.10 | GPL-3.0 | Maintenance — fills the in-tree DVB-S2 RX gap |
| [gr-isdbt](https://github.com/git-artes/gr-isdbt) | 2025-01-30 | 3.10 | GPLv3 | Maintenance — ISDB-T receiver |
| [gr-leo](https://gitlab.com/librespacefoundation/gr-leo) | 2026-09-16 | unverified | GPL-3.0 | Active — a link *simulator*, not a decoder |
| [gr-dslwp](https://github.com/daniestevez/gr-dslwp) | 2022-11-20 | 3.7.2 | none | Abandoned |

**Cellular — the family that left GNU Radio**

| Module | Last push | Target GR | Licence | Status |
|---|---|---|---|---|
| [gr-gsm](https://github.com/ptrkrysik/gr-gsm) | 2025-03-10 | **3.8** | GPLv3-or-later | **Pinned to 3.8; will not build on 3.10.** 1498★ — the most-starred OOT decoder in the ecosystem, and stuck |
| [gr-lte](https://github.com/kit-cel/gr-lte) | 2020-10-15 | 3.7.2 | GPL-3.0 | Abandoned |
| [gr-dect2](https://github.com/pavelyazev/gr-dect2) | 2025-03-16 | 3.10 | GPL-3.0 | Maintenance |
| [LTESniffer](https://github.com/SysSec-KAIST/LTESniffer) | 2024-10-23 | **srsRAN, not GR** | AGPL-3.0 | Dormant (2232★) |
| [5GSniffer](https://github.com/spritelab/5GSniffer) | 2024-11-14 | **srsRAN, not GR** | none stated | Dormant |

**There is no `gr-nr`.** Modern LTE and 5G analysis is built on srsRAN, not on GNU Radio blocks.

**Short-range / IoT**

| Module | Last push | Target GR | Licence | Status |
|---|---|---|---|---|
| [gr-lora_sdr](https://github.com/tapparelj/gr-lora_sdr) | 2026-01-05 | 3.10 | GPL-3.0 | **Active** (1009★) — the EPFL full TX+RX LoRa PHY |
| [gr-ieee802-11](https://github.com/bastibl/gr-ieee802-11) | 2026-05-19 | 3.9 | GPL-3.0 | **Active** (913★) |
| [gr-rds](https://github.com/bastibl/gr-rds) | 2026-01-04 | 3.10 | GPL-3.0 | **Active** |
| [gr-ieee802-15-4](https://github.com/bastibl/gr-ieee802-15-4) | 2023-07-28 | 3.9 | GPL-3.0 | Dormant |
| [gr-lora](https://github.com/rpp0/gr-lora) | 2022-08-18 | 3.9 | GPL-3.0 | Dormant |
| [gr-zwave_poore](https://github.com/cpoore1/gr-zwave_poore) | 2022-08-28 | 3.10 | **MIT** | Dormant |
| [gr-X10](https://github.com/cpoore1/gr-X10) | 2023-08-29 | 3.10 | **MIT** | Dormant |
| [gr-bluetooth](https://github.com/greatscottgadgets/gr-bluetooth) | 2024-08-16 | **3.7.0** | GPL-2.0 | **Archived** |
| [gr-wmbus](https://github.com/oWCTejLVlFyNztcBnOoh/gr-wmbus) | 2015-08-31 | no CMakeLists | none | Abandoned |

**No maintained GNU Radio BLE receiver exists.** `gr-bluetooth` (classic BR, GR 3.7) is archived;
live BLE work moved to Ubertooth firmware and nRF-based sniffers.

**Digital voice / LMR**

| Module | Last push | Target GR | Licence | Status |
|---|---|---|---|---|
| [op25 (boatbod)](https://github.com/boatbod/op25) | 2026-09-21 | **3.10** | GPLv3-or-later (source headers; **no top-level LICENSE file**) | **Active** |
| [trunk-recorder](https://github.com/TrunkRecorder/trunk-recorder) | 2026-09-01 | unversioned | GPL-3.0 | **Active** (1141★) — P25 + SmartNet, built *on* GR |
| [gr-dsd](https://github.com/argilo/gr-dsd) | 2024-01-30 | 3.9 | GPL-3.0 | Maintenance — a wrapper around DSD |
| [gr-ysf](https://github.com/HB9UF/gr-ysf) | 2021-10-25 | 3.8.1 | GPL-3.0 | Abandoned |
| [osmo-tetra](https://github.com/osmocom/osmo-tetra) | 2025-12-18 | **not GR** | AGPL-3.0 | Maintenance |
| **NXDN** | — | — | — | **No dedicated `gr-nxdn` exists** — NXDN lives inside op25 and DSD |

**Paging / data — and a finding worth stating plainly**

| Module | Last push | Target GR | Licence | Status |
|---|---|---|---|---|
| [gr-mixalot](https://github.com/unsynchronized/gr-mixalot) | 2024-07-09 | 3.10 | GPL-3.0 | Maintenance — POCSAG/FLEX/GSC **encoders (TX only)** |
| [on1arf/gr-pocsag](https://github.com/on1arf/gr-pocsag) | 2018-10-22 | no CMakeLists | none | Abandoned (sender) |
| [multimon-ng](https://github.com/EliasOenal/multimon-ng) | 2026-07-27 | **not GR** | GPL-2.0 | **Active** (1136★) |

**There is no maintained GNU Radio POCSAG or FLEX *decoder*.** Every GR paging module is a
transmitter; decoding lives in multimon-ng, outside GNU Radio entirely. This project already has a
native POCSAG recipe (tutorial 2) — i.e. **hackriff's paging coverage already exceeds GNU Radio's**,
which is a useful calibration on what "the whole suite" actually means.

**Analysis helpers — the most directly relevant corner**

| Module | Last push | Target GR | Licence | Status |
|---|---|---|---|---|
| [gr-fhss_utils](https://github.com/sandialabs/gr-fhss_utils) | 2026-09-16 | 3.10 | GPL-3.0 | **Active** — burst detection and measurement |
| [gr-pdu_utils](https://github.com/sandialabs/gr-pdu_utils) | 2026-09-19 | 3.10 | GPL-3.0 | **Active** |
| [gr-timing_utils](https://github.com/sandialabs/gr-timing_utils) | 2024-02-08 | 3.10 | GPL-3.0 | Maintenance |
| [gr-inspector](https://github.com/gnuradio/gr-inspector) | 2025-03-03 | 3.10 | GPL-3.0 | Dormant ~18 mo, **3.10-ready** |
| [gr-sigmf](https://github.com/skysafe/gr-sigmf) | 2022-04-22 | 3.9 | GPLv3 | Dormant |
| [gr-burst](https://github.com/gr-vt/gr-burst) | 2016-10-12 | 3.7 | GPL-3.0 | Abandoned |
| [FISSURE](https://github.com/ainfosec/FISSURE) | 2026-09-20 | framework | GPL-3.0 | **Active** (2048★) — bundles many OOTs |

Sandia's `gr-fhss_utils` and `gr-pdu_utils` are the two most relevant active modules in the whole
survey for *this* project — burst detection and measurement, which is C10's territory and the
time-bounded signal model's playground — and they are worth reading as prior art regardless of what
the coverage roadmap does (RESEARCH-001, SIGNAL-052).

### 1.5 Licence facts

**Everything that matters in this ecosystem is GPLv3**, with a few GPLv2 (`gr-bluetooth`, `gr-m17`,
multimon-ng), a few **AGPL-3.0** (LTESniffer, osmo-tetra, `gr-nrf24-sniffer`, `bafe/gr-pocsag` —
worth flagging separately, since AGPL's network clause is stricter than GPL's), and exactly **two
permissive decoders found in the entire survey**: `gr-zwave_poore` and `gr-X10`, both MIT, both
dormant.

Three traps in the detail, all of which matter more than the headline:

- **Several notable modules have no licence file at all** — `gr-dsdcc`, `gr-dmr`, `gr-tempest`,
  5GSniffer, `gr-acars2`, `gr-wmbus`, `bkerler/gr-ais`. No licence is **not** permissive: it means
  all rights reserved, which is *worse* than GPL for this project's purposes, because the plugin
  boundary does not help with code you have no right to redistribute at all.
- **GitHub's licence field is unreliable here.** `gr-gsm`, `gr-isdbt` and `gr-sigmf` all report
  `NOASSERTION` because their `LICENSE`/`COPYING` files carry modified preambles; all three are
  GPLv3 when read directly. A licence must be read from the file, not from the API.
- **`op25` states GPLv3-or-later only in its source headers**, with no top-level `LICENSE` at all.

**This is the strongest structural argument for ADR-0010's rule**: there is no permissive decoder
set to adopt instead, so the process boundary is not a preference, it is the only available shape —
and for the unlicensed modules, not even that is enough.

### 1.6 GNU Radio 4.0

| Repo | Licence | Role |
|---|---|---|
| [gnuradio/gnuradio4-core](https://github.com/gnuradio/gnuradio4-core) | **MIT** | Current official core |
| [gnuradio/gnuradio4-library](https://github.com/gnuradio/gnuradio4-library) | **MIT** | Official |
| [gnuradio/gnuradio4-blocks](https://github.com/gnuradio/gnuradio4-blocks) | **MIT** | Official |
| [fair-acc/gnuradio4](https://github.com/fair-acc/gnuradio4) | **LGPL-3.0-or-later** (static-linking exception) | Historical prototype, predates the relicensing |

The project has answered the licence question for ported blocks directly: *"Blocks ported directly
from GNU Radio 3 will retain their GPLv3 license"*
([GR4 community stewardship, 2026-05-21](https://www.gnuradio.org/news/2026-05-21-gr4-community-stewardship/)).
So GR4 is a **two-tier model — MIT core and runtime, GPLv3 permitted (neither required nor banned)
downstream** — and **it changes nothing for this project's boundary rule**, because the decoder
blocks are the GPLv3 tier.

What GR4 does *not* yet have, stated as absence rather than assumed:

- **No confirmed OOT port.** Searching for GR4 ports of gr-satellites, gr-leo, gr-osmosdr and others
  by name found none. **Unverified whether any production OOT has made the jump.**
- **No compatibility shim announced.** The
  [RC1 announcement](https://www.gnuradio.org/news/2026-03-22-gr4-release-candidate-1/) lists
  "block development and migration from GNU Radio 3" as a *call for contributors*, not a delivered
  tool.
- **No "3.10 supported for N years" commitment** was found in any fetchable official source —
  **unverified in both directions.**
- Porting guides exist on the wiki, but their full text **could not be fetched** (Cloudflare
  challenges non-browser clients) — **unverified beyond search snippets.**

**Practical read:** GR4 is a release candidate with an MIT core, no Python bindings, no GUI, no
ported decoder ecosystem and no announced migration mechanism. The decoder reference set lives
entirely on the 3.10 line, and 3.10 has had no point release since 2025-02-20.

### 1.7 The one-line framing

**GNU Radio does not have N decoders; it *had* N decoders.** Thirteen of the sixty-five surveyed
modules are actively maintained, and for several marquee families — Bluetooth/BLE, paging, VDL2,
HFDL, NXDN, 5G — the living implementation is **outside GNU Radio entirely**. Even for ADS-B and the
ACARS family, the healthy tools (`readsb`, `dump1090`, `dumpvdl2`, `dumphfdl`, `libacars` — the last
of which is **MIT**) are non-GR, so the GNU Radio path is the weaker one.

As a **reference set of capabilities to eventually cover**, the ecosystem is excellent and there is
nothing better. As a **set of components to depend on**, most of it is a snapshot of what someone got
working on GR 3.7 in 2016. Everything in §§2–5 follows from holding those two sentences at once.

One module deserves singling out for a different reason: **gr-inspector is the nearest public prior
art to this project's own blind-detection work** — energy detection of continuous signals, OFDM
parameter estimation, blind OFDM synchronisation, and a TensorFlow AMC block. It is dormant
(2025-03-03) but 3.10-ready, and it is worth reading whatever the coverage roadmap decides. *(Its
README notes that the FAM visualisation and TensorFlow AMC "are not on GR 3.8 yet" — which is
probably the origin of docs/03's "stale, GR 3.8" description.)*

### 1.8 Explicitly unverified in this survey

`gr-lucifer` — no repository or reference found anywhere; the name may be wrong. `tetra-kit` — no
canonical repository located. `gr-rft` — did not exist as searched; `gr-rftap` is the likely
intended module. The wiki page `List_of_gr-modules` — could not confirm it exists. The Debian `gr-*`
package count (~17) is a prefix search and may not be exhaustive. gr-leo's declared GR version. The
GRCon21 maintenance-talk content. Whether gr-leo's 3.10 build failure was later fixed. Whether the
wiki's `OutOfTreeModules` page changed since the 2026-06-05 Wayback snapshot. And, most importantly,
**the 65-module statistics are a biased sample of notable modules, not an ecosystem census.**

Two searched-for absences, recorded as findings rather than gaps: there is **no** `gr-vdl2`,
`gr-hfdl` or `gr-nxdn`, and no ecosystem-wide OOT build-health census or dashboard exists — any
claim of one should be treated as unverified.

---

## 2. The licence mechanics, concretely

### 2.1 What "behind the plugin process boundary" means in this codebase

It is not a figure of speech and it is not a build flag. It is a **separate operating-system
process with a defined wire contract**, and the mechanism already exists and is in use.

[ADR-0003](adr/0003-process-plugin-model.md) and
[docs/stream-contract.md §9](stream-contract.md) define it; `crates/hk-plugins` implements it
(host, manifest, ingest, output — about 3 000 lines). The shape:

| Piece | What it is |
|---|---|
| **Manifest** | `plugins/<id>/manifest.json`. Declares `id`, `version`, **`licence`**, `executable`, input (`kind`, `datatype`, `sample_rates_hz`, `center_hz` range, `ready_signal`), output (`schema_id`, `content_class`, `metadata_keys`), `restart` policy and `limits`. Unknown fields are errors. |
| **Data plane** | Channel-rate IQ into the child's **stdin**, `hackriff-v1` framed or raw. The full-rate sample path never crosses the boundary (C22 card: full rate is 40 MB/s). |
| **Control/result plane** | NDJSON `decode` / `annotation` / `log` lines out on **stdout**. |
| **Supervision** | Restart with backoff, `stall_timeout_ms`, `ready_timeout_ms`, process-**group** kill, bounded input queue with drop counters, `nice`. A decoder segfault restarts one subprocess; capture is untouched. |
| **Policy** | The manifest's `content_class` is a ceiling, clamped further by the input channel's class; restricted classes need a typed `metadata_keys` allowlist; a restricted line's `sample_index` must fall inside the input the host offered. |

**Nothing is linked.** The child is `exec`'d. That is the entire licence argument: GPLv3 obligations
attach to the work being distributed and linked, and a separately-distributed executable spoken to
over a pipe is not linked into the core. ADR-0010 records this for every GPL tool in the ledger, and
the ledger is a gate on adoption, not bookkeeping after the fact.

**And there is no alternative to it.** §1.5's licence census is the strongest structural argument
for ADR-0010's rule: of 65 surveyed OOT decoders, exactly **two** are permissive (`gr-zwave_poore`
and `gr-X10`, both MIT, both dormant), a handful are **AGPL-3.0** — whose network clause is stricter
than GPL's and deserves its own flag if any of them are ever considered — and several notable ones
have **no licence file at all**, which makes them not-safe-to-vendor regardless of intent. There is
no permissive decoder set to adopt instead. The process boundary is not a preference; it is the only
available shape.

### 2.2 The worked example: readsb

`plugins/readsb/manifest.json` plus `hk-plugin-readsb` (a bin target of `crates/hk-plugins`) is the
one real instance, landed as T-015. It is worth reading before estimating any other wrapping job,
and it is worth being clear about **why it is easy**:

- readsb is a **self-contained C program** with no framework under it. Its licence row in ADR-0010
  reads GPL-3.0-or-later, verified from `brew info readsb --json`, pinned at 3.16.16.
- The wrapper's whole job is format adaptation: convert the host's `ci8` to readsb's UC8, run it
  with `--freq {input.center_hz}`, parse `--raw` output into NDJSON `decode` lines.
- The only subtlety is a **ready signal**: readsb needs its Beast connection and a pre-roll before
  it decodes, measured at 1.6 s under load (T-223), so `ready_timeout_ms` is set explicitly to 5 s
  — roughly 3× the worst case — and a live chain reads no samples while it waits.

That is the floor of the wrapping cost, not the median.

### 2.3 What a GNU Radio decoder specifically drags in

A GNU Radio OOT is a different animal, and the difference is worth stating precisely because it is
usually stated imprecisely.

**Its runtime.** `gnuradio-runtime` (GPLv3), the block libraries it depends on (`gr-blocks`,
`gr-filter`, `gr-digital`, … all GPLv3) and **VOLK**, which ADR-0010's ledger places
**plugin/subprocess only, never in a non-GPL core**. (VOLK itself is LGPL-3.0-or-later from 3.0,
T-556 found. The placement is unchanged because VOLK only reaches us through GPLv3 GNU Radio.)
All of that lives inside the child
process. That is fine, and it is exactly what the boundary is for. It is also a dependency closure
of hundreds of megabytes that must exist on the **Jetson** as well as the Mac, and a handheld
device's disk and image size are real constraints (CLAUDE.md: portable, one self-contained unit).

**Its Python — and the common misconception about it.** Most OOTs ship a GRC flowgraph and Python
bindings, so wrapping one usually means running a Python process. But GNU Radio's Python is a
**graph-construction** layer: once `tb.start()` runs, the blocks execute in C++ under a C++
scheduler, and Python is mostly idle. So CLAUDE.md's rule — "Python is for orchestration and
research only, never the real-time path" — is **not violated** by a GR plugin, because the rule is
about *this project's* sample path and the plugin's samples never touch Python. What Python does
cost is start-up latency (interpreter plus imports, which sets `ready_timeout_ms`), memory, and a
second language runtime to install on the target. Some OOTs implement blocks *in* Python
(`gr::sync_block` subclasses); those genuinely do put Python on their own sample path and should be
assumed slow until measured.

**Its scheduler.** GNU Radio owns its own threads and its own buffers, with its own scheduling
policy. The host can set `nice` and kill the process group; it cannot arbitrate inside. So a GR
plugin is a second scheduler competing with `hk-core`'s ring and `hk-pipeline`'s chains for the same
cores — on a 6-core Orin Nano class part, running from a battery. This is the cost that does not
appear in a per-decoder estimate and does appear when three of them run at once.

**Its time base.** GNU Radio carries stream tags and its own sample counting; the C22 card lists
"wall-clock timestamps from plugins" as a known pitfall and the rule is to **re-stamp from host
sample indices**. Under a restricted `content_class` this stops being a quality issue and becomes an
enforcement one: a restricted line's `sample_index` must fall inside the window the host offered, or
the line is dropped and counted.

**Its version.** A wrapped tool is pinned and recorded in the ADR-0010 ledger with its licence. For
a GR OOT that means pinning GNU Radio itself, because an OOT built against 3.10 does not load under
3.9 or 4.0 — which is the mechanism behind the bitrot §1 describes.

### 2.4 The per-decoder cost, and the shape of the curve (measured: T-556)

**T-556 measured it**, on gr-lora_sdr (C++ blocks) and gr-satellites (Python flowgraph), both
wrapped behind the §9 plugin contract and run under the real `hk_plugins::PluginInstance`. The
write-up is
[`spikes/t556-gnuradio-wrap/README.md`](../spikes/t556-gnuradio-wrap/README.md).

- **Runtime is cheap.**
  - Private memory is 27 MB (LoRa) and 55 MB (gr-satellites).
  - CPU at real time is 2.4–3 % and 11.5 % of one M3 core. On the Orin's A78AE expect several
    times that (**unverified**).
  - Warm start-up to ready is 0.3 s and 1.6 s. The first launch after a relink is 4–12 s, which
    fits T-629's first-byte rule only because the adapter writes no byte before ready.
- **The intercept is the runtime install.**
  - On JetPack 6's Ubuntu 22.04, `apt install gnuradio` is 375 packages and ~1.1 GB, 60 of them
    GUI/GL. It ships GR 3.10.1 and pybind11 2.9, against the Mac's 3.10.12 and pybind11 3.1:
    two build matrices.
  - On the Mac, two of three first builds failed on ABI skew (a leaked conda `fmt`, and the
    Homebrew bottle's pybind11 internals version).
  - Estimate: 0.5–1 day on the Mac, 1–2 days on the Jetson (**unverified**), repeated at every
    GR upgrade.
- **The slope, per decoder after the first:**
  - **2–4 developer-hours** to "decoding, arrival-stamped, parity-checked against the stock
    tool". The second wrapper took an agent 14 minutes, 5.5 of them compiling.
  - **1.5–3 days** for product grade with exact host sample-index stamps, *when the OOT has a
    single framing point to carry the input offset through*. gr-lora_sdr did, at the price of an
    8-line fork patch.
  - **Not feasible without forking dozens of files** when it has none. gr-satellites' ~60
    deframers emit bit-domain PDUs after variable-ratio clock recovery, so its decodes can only
    be arrival-stamped. That is the C22 pitfall, accepted rather than fixed.
- **A shared `hk-plugin-gnuradio` host does not change the slope.**
  - One process running both graphs saves about one runtime floor per extra decoder (~38 MiB
    RSS, 0.2–0.5 s of start-up).
  - It costs fault isolation, one GIL for Python deframers and adapters, ABI coupling between
    OOTs, and a change to the one-manifest-one-process contract.
  - Per-decoder processes are affordable and preferable. The slope is per-decoder engineering
    (stamping, parity fixtures, CRC semantics), which no host amortises.

**Consequence for §3:** a wrapped GNU Radio decoder is a narrow, opt-in disposition, used only
when no native recipe is planned, the OOT can carry host sample offsets, and parity is under test.
The reference set's main use is as a **specification source** for native recipes, which is what
§3 already prefers.

---

## 3. Coverage as a ranked roadmap

Ranked by value per unit of work against [docs/05](05-use-cases-and-explorations.md) and the front
end's real reach — 1 MHz – 6 GHz, ~20 Msps instantaneous, 8-bit, half-duplex, no preselector, **one
tuned window at a time**. The disposition column is the claim this roadmap makes; **T-554** is the
ticket that tests each one properly and produces the block-catalogue delta.

**§6 is the audit this section asked for**, and it revises two of the calls below: OFDM unlocks far
less than it appears to (§7, finding 2) and DECT, SmartNet/EDACS and Z-Wave turn out to need no new
blocks at all.

Today's `hk-blocks` catalogue, which is what "native" is measured against:

> `mix`, `lowpass`, `resample`, `fm_demod`, `am_demod`, `fsk_demod`, `msk_demod`, `ppm_demod`,
> `subcarrier`; `clock_recovery`, `slicer`, `diff_decode`, `nrzi`, `manchester`; `sync_search`,
> `deframe`, `interleave`, `deinterleave`, `assemble`, `length`; `crc`, `bch`, `parity`, `checksum`;
> `fields`, `text`, `consensus`; `follow_hops`, `identity`.

That set reaches **2-FSK / GFSK / MSK / OOK / PPM, framed, with CRC or BCH** — and stops.

### Tier 1 — pays immediately, needs no new blocks

**1. The short-range ISM long tail (OOK/ASK/FSK + PWM/PPM/Manchester + CRC).**
*Disposition: **templates**, not code.* This is the single highest-return row in the document and
the one that is pure data entry. `rtl_433` alone covers ~380 protocols and every one of them is
within the existing catalogue's expressive range. CLAUDE.md names 902–928 MHz as **the canonical
playground** for the time-bounded signal model; the bursts that exercise ADR-0015's burst path live
here.
Use cases: SIGNAL-046 (TPMS), SIGNAL-049 (ERT smart meters), SIGNAL-050 (R900 water meters),
SIGNAL-051 (wM-Bus), SIGNAL-052 (rtl_433 long tail), SIGNAL-057 (ALERT gauges), SIGNAL-055
(Z-Wave), SIGNAL-047 (keyless-entry fingerprinting), AWARE-070 (IoT census), AWARE-069 (mesh
growth), RESEARCH-001/002/012.
Why it is first: it converts "support many more decode types" from an engineering cost into a
template-authoring cost, **and** every template is a MAUTO search seed, so it improves the synthesis
engine at the same time. T-557 designs the bridge.

**2. VHF/UHF data and paging beyond what is already built.**
*Disposition: **native recipes**, existing blocks.* AFSK/AX.25 (APRS), railroad EOT and ATCS,
MPT1327 signalling, 4-level FSK data. All reachable with `fsk_demod` + `clock_recovery` +
`sync_search` + `crc`, in band, narrowband, cheap.
Use cases: SIGNAL-041, SIGNAL-042, SIGNAL-044, PROP-002-adjacent beacon work.

### Tier 2 — high value, each needs one block family

**3. PSK, and the FEC that follows it.**
*Disposition: **native blocks** — `psk_demod`/Costas, then convolutional + Viterbi, then
Reed–Solomon.* ADR-0015 already names the first of these as optional task M-14, and **M-14 alone is
too narrow**: a PSK demodulator with no FEC stops at `demodulated` for essentially every real
satellite downlink, because CCSDS framing means convolutional coding and Reed–Solomon. The three
together are one investment, and they unlock the largest single block of use cases in docs/05.
Use cases: SIGNAL-034 (cubesat telemetry), SIGNAL-033 (SatNOGS), SIGNAL-024 (Orbcomm), SIGNAL-023
(Iridium bursts), SIGNAL-069 (STANAG 4285), SPACE-081, RESEARCH-016.
Reference set: this is where **gr-satellites** is most valuable, and it is valuable as a
*specification corpus* — it encodes the framing, coding and field layout of 100+ real spacecraft.
It is also the ecosystem's healthiest decoder OOT (active, 3.10, GPL-3.0, 982★). §4's rule governs
how that knowledge crosses. For the algorithms themselves the better reference is **in-tree**:
`gr-digital` (Costas, `symbol_sync_cc`, constellation soft decoder, equalisers) and `gr-fec`
(convolutional/Viterbi, CCSDS R=1/2 K=7, Reed–Solomon, LDPC, polar) — mature, documented, and not
subject to the OOT bitrot §1.2 measures.
**The cost may be much lower than it looks**, and T-554 should check this first:
[liquid-dsp](https://github.com/jgaeddert/liquid-dsp) is **MIT**, already the project's designated
DSP kernel library (ADR-0010), actively released (v1.8.2, 2026-08), and already ships PSK/QAM/ASK
modems, NCO/PLL and framing, with FEC alongside. Under ADR-0011 §1.6 a block is an *adapter over an
existing kernel*, so a licence-clean tier-2 family can be an adapter job rather than a DSP job — and
that route needs no reference reading at all.

**4. CSS / chirp (LoRa).**
*Disposition: **native blocks** — dechirp + symbol demap.* One block family, a large and growing
population, entirely in band and narrowband enough for the front end.
Use cases: SIGNAL-053 (LoRa/LoRaWAN), AWARE-069 (Meshtastic mesh map), PROP-078 (terrain link
budget), RESEARCH-063/064.
Reference set: [gr-lora_sdr](https://github.com/tapparelj/gr-lora_sdr) (EPFL, active, 3.10, GPL-3.0,
1009★) is the reference implementation and the PHY has published reverse-engineering write-ups
(RESEARCH-064 exists precisely because of them) — so tier-2 of §4's rule applies cleanly, with a
public description to work from.

**5. Digital voice signalling — control channels and metadata, not audio.**
*Disposition: **native blocks** for C4FM/4-level FSK + the framing; **audio deliberately out of
scope**.* P25/DMR/NXDN control channels are FSK-family and reachable, and the trunking logic is C23.
The **vocoder is the wall**: ADR-0010 already records that AMBE/IMBE IP is a separate,
non-code licence constraint to be reviewed before trunking voice ships. Stating the boundary is
better than discovering it: *this device can inventory, follow and characterise an LMR system
without decoding a word of speech*, and that is a legitimate and useful product.
Use cases: SIGNAL-080, SIGNAL-081, SIGNAL-082, SIGNAL-083, SIGNAL-084.
Reference set: [op25](https://github.com/boatbod/op25) (active, 3.10) and
[trunk-recorder](https://github.com/TrunkRecorder/trunk-recorder) (active, 1141★) are the live
implementations and are the natural **plugin** candidates if the vocoder question is ever settled.
Note from §1.4 that there is **no `gr-nxdn`** — NXDN lives inside op25 and DSD, so "the GNU Radio
suite" is not a per-protocol menu here either.

### Tier 3 — expensive, and the front end pushes back

**6. OFDM.** *Disposition: **large native investment**, and partly out of reach.* Unlocks Wi-Fi,
LTE/5G control channels, DAB, DVB-T, HD Radio and DroneID — a lot of docs/05. But: 20 Msps is a
**marginal** fit for a 20 MHz Wi-Fi or LTE channel and no fit at all for 100 MHz 5G NR; and 8-bit
with no preselector in a city mostly shows intermodulation (docs/02, and the standing finding in
CLAUDE.md). A partial disposition is honest here — **cell and AP discovery and metadata** are
achievable where full demodulation is not.
Use cases: AWARE-016/018/019 (cell inventory and load), AWARE-023/024 (Wi-Fi anomalies),
AWARE-020/021 (Remote ID, DroneID), SIGNAL-063/064 (HD Radio, DAB+), RESEARCH-024/025.

**7. DSSS.** *Disposition: **split**.* Zigbee/802.15.4 (O-QPSK DSSS at 2.4 GHz) is a reachable
native target once PSK exists. GNSS is **already carved out** by
[ADR-0018](adr/0018-gnss-known-code-exception.md) as the one documented known-code-led exception and
is C36's business, not this roadmap's.
Use cases: SIGNAL-054 (Zigbee/Thread), RESEARCH-036; GNSS via ADR-0018.

**8. Wideband satellite (DVB-S2, HRPT, HRIT).** *Disposition: **plugin, or out of reach**.* SatDump
already does this well and is GPLv3, so it is the archetype of a decoder to **wrap rather than
rebuild** — where the bandwidth allows at all. DVB-S2 at tens of MSym/s does not fit 20 Msps.
Use cases: SIGNAL-025/026/027/028, RESEARCH-017, PROP-047.

### Out of reach, stated plainly

Below 1 MHz (VLF/ELF — SPACE-001/002 need an accessory receiver), above 6 GHz (Ku-band — PROP-047,
RESEARCH-014), anything requiring transmit (C37 is gated), anything requiring coherent multi-channel
receive (DF and TDOA — C32, PROP-052), and anything needing more instantaneous bandwidth than 20 MHz.
These are front-end facts, not roadmap choices, and docs/06's `hardware_fit` already records them
per use case.

### What is already covered, and should not be re-spent on

ADS-B (readsb plugin T-015 **and** a native recipe, tutorial 4), POCSAG (native recipe, tutorial 2),
ACARS (native recipe, tutorial 3), RDS (native recipe, tutorial 1), AIS and radiosondes (clean
plugin targets, if wanted). **These are the well-known cases and they are not the point** — CLAUDE.md
says so directly. They matter here only as *oracles*: a family with a known-good external decoder is
a family where a native recipe or a synthesized pipeline can be checked against ground truth, which
is what tutorials 1–4 already do.

---

## 4. What "reference" means — the rule proposed to the user

> **CANCELLED — NOT IN FORCE, AND NOT PENDING EITHER (user, 2026-09-20, T-555).** This section is a
> **proposal** written by the MAUTO design track. The user cancelled the ticket that would have
> ratified it, on the grounds that **there is no licence rule to ratify**: the standing position
> stands unchanged and needs none. It binds nothing and **no implementing task may cite it as
> authority.** The rule in force is the one that already exists: GPLv3 decoder code stays in a
> subprocess (ADR-0003/ADR-0010) and nothing is derived from it in-core. §6's audit applies that
> rule — every "native" disposition there rests on a public specification, a published
> reverse-engineering write-up, or a licence-clean kernel, never on reading GPLv3 source. The
> section is kept for the reasoning it records, not as a live proposal.

It is a working engineering rule for this project, not legal advice, and the project's own licence
remains undecided (CLAUDE.md).

Reading GPLv3 source to understand an algorithm and then writing an independent implementation is a
different act from linking that source. The boundary between them is real, and it is not a line
anyone can draw perfectly — so the rule below draws it conservatively and **fails closed**, the same
principle the gate classifier and `Coverage::Unobserved` already follow in this repo.

### The three tiers

**Tier 1 — Facts. Permitted, recorded.**
Protocol constants and structure: modulation family, symbol rates and tolerances, sync/preamble
words, CRC and FEC polynomials and parameters, interleaver and whitening definitions, field layouts,
frequency allocations. These are facts about an air interface, not expression, and they are
routinely published in standards, on sigidwiki, and in reverse-engineering write-ups.
**They may be taken from any source — including a GPLv3 decoder's source or its tests — and placed
in a `hackriff.template` or a recipe's parameters, provided the source is recorded.**
Precedent already in the ADR-0010 ledger: T-013 took CRC parameters from the public RevEng catalogue
("parameters only, no code copied"); T-012 used redsea and a Python reference for RDS ("references
only, no code copied").

**Tier 2 — Algorithms. Permitted, with a named public description.**
The general technique: a Costas loop, Gardner timing recovery, a dechirp, a Viterbi traceback, a
polyphase channelizer. Reading GPLv3 source to *understand* such an algorithm and then implementing
it is permitted **provided the implementation is written from the algorithm's public description** —
a paper, a textbook, a standard — and not from the source's structure.
The practical test, and it is deliberately a blunt one: **the implementer must be able to name the
public description they worked from.** If the only description that exists is the source code, then
tier 2 does not apply and tier 3 does.

**Tier 3 — Expression. Not permitted in-core.**
Translating, transliterating or closely paraphrasing GPLv3 source: its decomposition into functions
and files, its identifier and constant tables, magic numbers that were arrived at by tuning rather
than derived, its comment structure, its test vectors where those are original work.
**If a family can only be covered this way, it takes the plugin disposition instead.** That is what
the process boundary is for, and choosing it is not a failure.

### The operational rules that give it teeth

1. **Record derivation per block, in the ADR-0010 ledger.** The ledger already carries a row per
   *dependency*; the same discipline for *derivation* costs one line and is the only durable
   evidence of provenance. Two precedents are already there and set the format.
2. **Brief agents by tier.** This rule is crossed accidentally, by context, not deliberately. An
   agent implementing a native block at tier 2 should be given the specification and **not** given
   the GPLv3 source in its context. If a task can only be done with the source open, that is the
   signal that its disposition should be plugin. This is the one operational change with real
   effect, and it costs nothing.
3. **When in doubt, plugin.** The whole point of ADR-0003's boundary is that the hard cases do not
   need this judgement call. Defaulting to the boundary is always safe; defaulting to in-core is not.
4. **Templates are always safe.** A template is data, it is tier 1, and ADR-0015's own safeguards
   mean it cannot make the system claim anything it did not measure: priors order the search and
   never rank a result or confirm one; a template can never confirm a signal on its own; bands only
   raise rank for an emitter *already detected* there. Importing a protocol's parameters is
   therefore safe in a way importing a decoder would not be.

### What to flag to the user

- The rule is conservative on purpose, and its cost is real: it will push some families to the
  plugin disposition that a looser rule would keep native — and native is what MAUTO can search.
- **If the project ever ships under GPLv3 itself, this whole question changes shape** and tier 3
  becomes available. CLAUDE.md keeps the licence undecided and says not to let licences gate work;
  this rule honours that by never blocking, only by routing work to a different disposition.
- The tier-2 test ("name the public description") is a judgement call and will occasionally be
  wrong. It is proposed because it is checkable in a brief and in a review, not because it is exact.

---

## 5. How this folds into the MAUTO roadmap

The coverage direction is not a separate programme. It lands in three places that already exist:

| Where | What coverage adds |
|---|---|
| **ADR-0015 §4 templates** | The bulk of tier-1 coverage. Every imported protocol is a search seed as well as a decoder. T-557 designs the provenance and the fact/implementation line; T-554 says which families are template-shaped. |
| **ADR-0011 §1.5 block catalogue** | The tier-2 and tier-3 families. T-554 produces the delta (PSK/Costas, Viterbi, Reed–Solomon, CSS, OFDM, DSSS, scramblers, SSB/CW) with an ordering. ADR-0015's M-14 covers only `psk_demod` and is too narrow on its own. Note the pattern ADR-0011 §1.6 already sets: **blocks are adapters over existing kernels, not rewrites** — today over hk-dsp, hk-demod and hk-estimate, and tomorrow over liquid-dsp (MIT), which already carries much of what tier 2 wants. A new block is often an adapter job, not a DSP job, and that is the first thing T-554 should check per family. |
| **ADR-0003 / `plugins/`** | The handful of genuinely active, genuinely hard decoders where wrapping beats rebuilding — SatDump, op25, gr-satellites. T-556 measured it (§2.4): wrapping is cheap to run, but gr-satellites' decodes can only be arrival-stamped, so wrapping it beats rebuilding only where its time placement doesn't matter. |

And it changes one thing about how the engine tickets should be read: **a block-catalogue gap is a
product-visible state, not a silent absence.** ADR-0015 §8 already says an unsupported structure is
reported as the verdict reason; T-550 makes that distinguishable from "searched and failed", because
"we have no CSS block" and "this is not LoRa" must not read alike to a user. The coverage roadmap and
the honesty rules are the same work seen from two sides.

### Two gaps in the engine set, to settle before it is signed off

Both were found while writing this document and neither is a coverage item — they are holes in
ADR-0015 §10's own M-1…M-14 sketch.

1. **Nothing owns the search trace.** M-3 builds the beam and prunes, M-8 serves the API, M-11
   renders results — and between them the record of *what was rejected and why* is never produced.
   M-3 as specified actively discards it: the beam keeps only "the best pruned node", and `progress`
   keeps bare counts. The shipped system would be able to say "here is FSK at 4800 Bd" and unable to
   say "why not PSK", or even whether PSK was looked at. **T-549** is the design; the build needs an
   owner, and it must be *inside* M-3 — a beam that has already thrown the information away cannot
   have it retrofitted, only re-instrumented.
2. **M-14's scope stops short of being useful.** It is "optional `psk_demod`/Costas", and PSK with no
   FEC reaches verdict `demodulated` and no further for essentially every real satellite downlink,
   because CCSDS framing means convolutional coding plus Reed–Solomon. PSK + Viterbi + Reed–Solomon
   is **one** investment, not one plus two optional extras, and it is the largest single block of
   docs/05 use cases available (SIGNAL-034, -033, -024, -023, -069, SPACE-081). Check liquid-dsp
   (MIT, already designated, already ships PSK/QAM modems and FEC) before scoping any of it — under
   ADR-0011 §1.6 this may be an adapter job, which would also moot the T-555 licence question for
   this family entirely.

---

## 6. The disposition audit (T-554)

Status: **audit** (2026-09-21, T-554). Nothing here is implemented, no ADR status changes, and
ADR-0011 is not rewritten — §8 lists what it would have to absorb. §3 ranked families by value;
this section decides, per family, **where the capability lands in this codebase** and **what is
missing before it can**.

### 6.1 What the dispositions mean here, and the two constraints applied rather than rediscovered

- **native recipe** — expressible as an `hk-blocks` DAG plus a recipe document, either with the
  catalogue as it stands ("native now") or with a **named** small addition ("native + additions").
  Licence-clean, hot-editable, and **the only form the MAUTO search can search over**.
- **wrapped plugin** — a subprocess behind `hk-plugins` (ADR-0003). Cheap per decoder, opaque to
  the search, and it brings a build and runtime dependency to both the Mac and the Jetson.
- **out of reach** — the front end or the law of the thing forbids it: bandwidth, frequency,
  coherent channels, transmit, or IP the project will not touch.
- **unknown** — deliberately used, and marked rather than guessed. See §6.4.

**Constraint 1 — the licence boundary decides "wrapped" by itself.** GPLv3 code (VOLK, GNU Radio,
almost every decoder in §1.4) stays behind the plugin process boundary, ADR-0010. That is the
standing rule and this audit applies it without reopening it: a family whose only implementation is
a GPLv3 codebase, and whose algorithm has no public description to work from, takes the plugin
disposition. **Nothing in §4 is relied on** — T-555 was cancelled by the user on the grounds that
there is no rule to ratify, so every "native" call below rests on a public specification, a
published reverse-engineering write-up, or a licence-clean kernel (liquid-dsp, MIT), never on
reading GPLv3 source.

**Constraint 2 — the front end.** 1 MHz – 6 GHz, receive only, ~20 Msps instantaneous, 8-bit, no
preselector, one tuned window at a time. The `fit` column uses docs/06 §3's vocabulary exactly
(`native`, `needs-accessory` + the accessory, `needs-tx`, `needs-other-sdr`, `out-of-band`), so a
family marked `needs-accessory` is **reachable** with the named add-on and is not an honest
exclusion; a family marked `needs-other-sdr` or `out-of-band` is.

### 6.2 The disposition table

Ordered by disposition, then roughly by value. "Blocks needed" names additions from §7's catalogue
delta. Families already shipped are marked ✓.

**Native recipe, with the catalogue as it stands today**

| Family | Fit | Use cases | Note |
|---|---|---|---|
| ADS-B / Mode S ✓ | native | SIGNAL-001, AWARE-008, AWARE-010, RESEARCH-018 | Native recipe (tutorial 4) **and** the readsb plugin — the one family with both. |
| ACARS ✓ | native | SIGNAL-003, SIGNAL-005 | Native recipe (tutorial 3). |
| POCSAG ✓ | native | *(none — see §6.4)* | Native recipe (tutorial 2). **GNU Radio has no maintained POCSAG decoder at all** (§1.4). |
| RDS / broadcast FM ✓ | native | SIGNAL-062 | Native recipe (tutorial 1), plus ADR-0011 §8.9's audio sibling. |
| NFM / AM analog ✓ | native | SIGNAL-067, listening | Blocks exist or are specified by ADR-0011 §8.4. |
| rtl_433 long tail (OOK/ASK/PWM/PPM/Manchester + CRC) | native | SIGNAL-052, SIGNAL-046, SIGNAL-049, SIGNAL-050, SIGNAL-057, SIGNAL-047, AWARE-070, AWARE-035, AWARE-027, RESEARCH-001, RESEARCH-002 | §3's Tier 1: **templates, not code**. ~380 protocols inside the existing expressive range. |
| Z-Wave (G.9959) | native | SIGNAL-055 | GFSK + checksum. One of only two permissive GR modules in the survey (`gr-zwave_poore`, MIT) — and it is not needed. |
| MPT1327 | native | SIGNAL-044 | FFSK + BCH(63,48): `fsk_demod` + `clock_recovery` + `sync_search` + `bch`. |
| Motorola SmartNet / SmartZone, EDACS control channel | native | SIGNAL-083, SIGNAL-084, SIGNAL-085, SIGNAL-087, AWARE-067 | 3600 bps binary FSK + Manchester + BCH(63,16). Entirely inside today's catalogue — **the cheapest trunking win on the board** and the subject of SIGNAL-087's blind end-to-end target. |
| Railroad EOT and ATCS | native | SIGNAL-041, SIGNAL-042 | 4-level and 2-level FSK data with CRC, entirely inside the catalogue — §3's Tier 1 row 2. |
| DECT (base-station metadata, framing) | native | SIGNAL-056 | 1.152 Mbps GFSK in 1.728 MHz — comfortably inside 20 Msps. `gr-dect2` is maintained; it is not needed. |

**Native recipe, with named additions**

| Family | Fit | Blocks needed | Use cases |
|---|---|---|---|
| CCSDS / amateur-satellite telemetry (the `gr-satellites` corpus) | needs-accessory (Yagi) / native for strong passes | `psk_demod`, `viterbi`, `reed_solomon`, `descramble` | SIGNAL-034, SIGNAL-033, SPACE-081 |
| Meteor-M LRPT | native | `psk_demod`, `viterbi`, `reed_solomon`, `descramble` | SIGNAL-027, SIGNAL-029 |
| GOES HRIT / GK-2A LRIT | needs-accessory (1.7 GHz dish + LNA, bias-tee) | same four | SIGNAL-025, SIGNAL-026 |
| Metop / FengYun HRPT, AHRPT | needs-accessory (tracked dish + LNA) | same four | SIGNAL-028 |
| Inmarsat STD-C / EGC, Aero | needs-accessory (L-band patch + LNA) | `psk_demod`, `viterbi` | SIGNAL-019, SIGNAL-007 |
| Orbcomm | native | `psk_demod` (`diff_decode` already exists) | SIGNAL-024 |
| VDL Mode 2 | native | `psk_demod` (D8PSK), `descramble`, `bitstuff`, `reed_solomon` | SIGNAL-004, SIGNAL-008 |
| AIS | native | `bitstuff` (HDLC destuff) — everything else exists | SIGNAL-015, AWARE-007, AWARE-012, PROP-024, PROP-025 |
| AX.25 / APRS / ISS digipeater | native | `bitstuff` | SIGNAL-036, SIGNAL-042-adjacent |
| Bluetooth LE (advertising, metadata) | native | `descramble` (whitening LFSR) — `follow_hops` already exists | AWARE-025, AWARE-026, AWARE-028, RESEARCH-029 |
| ASTM F3411 Remote ID, **BLE transport** | native | `descramble` | AWARE-020 |
| Radiosondes (RS41 and family) | native | `reed_solomon`, `descramble` | SIGNAL-074, PROP-037, AWARE-062 |
| LoRa / CSS | native | `css_demod`, `descramble`, a short-block code (see §7's Golay/Hamming question) | SIGNAL-053, AWARE-069, RESEARCH-064, PROP-078 |
| Zigbee / Thread (802.15.4, O-QPSK DSSS) | native | `psk_demod` (offset QPSK), `despread` | SIGNAL-054 |
| Wireless M-Bus | native | `codeword_map` (3-of-6, mode T) — modes C/S need nothing | SIGNAL-051 |
| FLEX paging | native | `mlevel_slicer` (2- and 4-level FSK) | *(none — see §6.4)* |
| P25 Phase 1 — **control channel and metadata only** | native | `mlevel_slicer`, `viterbi` (trellis), `reed_solomon`, Golay/Hamming | SIGNAL-080, SIGNAL-085, SIGNAL-086, SIGNAL-087, AWARE-067 |
| DMR Tier III / Capacity Plus — control and metadata | native | `mlevel_slicer`, Golay/Hamming (BPTC over the existing `deinterleave`) | SIGNAL-082, SIGNAL-086 |
| NXDN Type-C — control and metadata | native | `mlevel_slicer`, `viterbi` | SIGNAL-084 |
| TETRA — control and metadata | native | `psk_demod` (π/4-DQPSK), `viterbi` (RCPC), `descramble` | RESEARCH-019 |
| HF data modems (STANAG 4285, MIL-STD-188-110, 2G ALE) | native | `psk_demod`, `equalise`, M-ary FSK via `mlevel_slicer`, Golay | SIGNAL-069, SIGNAL-070 |
| RTTY / NAVTEX / SITOR-B / Marine DSC | native (DSC) · needs-accessory for 490/518 kHz (HF upconverter / LF loop) | `codeword_map` (CCIR-476), a `text` charset addition (Baudot/CCIR) | SIGNAL-016, SIGNAL-017 |
| DAB+ | native | `ofdm_demod`, `viterbi`, `reed_solomon` | SIGNAL-064, PROP-027 |
| HF radiofax / SSTV / APT / Tempest raster | native · needs-accessory below 1 MHz | a **`raster` output kind** — a contract change, not a block | SIGNAL-018, SIGNAL-036, RESEARCH-039 |
| SSB / CW (analog, today `legacy` in Listen) | native | `ssb_demod`, `cw_demod` | SIGNAL-072, SIGNAL-037 (via LNB), SIGNAL-013 (via upconverter) |

**Wrapped plugin**

| Family | Fit | Why wrapped | Use cases |
|---|---|---|---|
| DVB-T | native (8 MHz fits 20 Msps, marginally) | `gr-dtv` is a **genuine in-tree receive chain** (§1.3), mature and maintained; rebuilding OFDM + Viterbi + RS + the DVB interleavers natively buys nothing the search can use at this size | PROP-027, AWARE-041 |
| ATSC 1.0 (8VSB) | native (6 MHz) | same: `gr-dtv` has a real RX | SIGNAL-066 is **bootstrap detection only**, is native today and needs no decoder |
| ISDB-T | native | `gr-isdbt` is a maintained receiver; same argument as DVB-T | PROP-027-adjacent |
| GSM (BCCH / system information) | native | `gr-gsm` is the most-starred OOT in the ecosystem **and is pinned to GR 3.8** (§1.4) — wrapping it is a porting job before it is an adapter job. Flagged, not priced | AWARE-016, AWARE-017, AWARE-066, SIGNAL-048 |
| HD Radio (NRSC-5) | native | The living decoder (`nrsc5`) is **not** GNU Radio; its licence was **not verified in this audit** and must be before adoption | SIGNAL-063 |
| FT8 / FT4 / WSPR / JS8 | native | The canonical decoders are the WSJT-X family, non-GR. Native would need LDPC(174,91) and a Fano sequential decoder for **one** family — the worst ratio on the board | PROP-002, PROP-004, PROP-005, SIGNAL-073, AWARE-059 |
| DVB-S / DVB-S2 **narrow feeds only** (≲ 8 MSym/s) | needs-accessory (Ku/C LNB + dish) | SatDump and `gr-dvbs2rx` already do this well; §3's "wrap rather than rebuild" archetype | RESEARCH-014, RESEARCH-017 |
| Codec2 / FreeDV voice | native | A codec, not a demodulator; `gr-vocoder` carries it and there is no reason for it in-core | (no docs/05 ID claims it) |

**Out of reach**

| Family | Why | Accessory changes it? | Use cases |
|---|---|---|---|
| 802.11 a/g/n frame decode | 20 MHz at 8-bit with no preselector; in a city the front end shows intermodulation (docs/02). **Presence, channel occupancy and anomaly detection are already native** and are what docs/05 actually asks for | No | AWARE-023, AWARE-024, AWARE-048 |
| LTE full demodulation | ≥ 10 MHz channels, 8-bit, no preselector; the living tooling is srsRAN, not GR | No | AWARE-018, RESEARCH-025 |
| 5G NR | 100 MHz FR1 channels ≫ 20 Msps instantaneous | No | AWARE-019, RESEARCH-024 |
| DVB-S2, typical transponder | tens of MSym/s ≫ 20 Msps | No — the LNB fixes the frequency, not the bandwidth | PROP-047, SIGNAL-025-adjacent |
| AMBE / IMBE / AMBE+2 voice audio | IP, not capability (ADR-0010). **The control channel and the metadata are not blocked by this** | No | SIGNAL-080–SIGNAL-084 (voice halves only) |
| TX-only modules (`gr-aistx`, `gr-mixalot`, `gr-paint`, DVB-S2/T2 TX) | `needs-tx`; C37 is gated | No | PROP-003, PROP-078, RESEARCH-063 |
| Passive radar, DF, TDOA | phase-coherent multi-channel receive | `needs-other-sdr` | PROP-053, PROP-055, PROP-060, PROP-022 |
| X-band deep space (~8.4 GHz) | above 6 GHz with no commodity downconverter | `out-of-band` — no accessory closes it | RESEARCH-014-adjacent |

**Unknown — marked, not guessed**

| Family | What is unknown | What would settle it |
|---|---|---|
| HFDL | The FEC, interleaver and frame geometry. There is **no GNU Radio OOT** (§1.4); `dumphfdl` is the living tool and is non-GR | A public HFDL PHY description, or a `dumphfdl` plugin spike |
| Iridium bursts | The burst structure, LCW handling and the demod's acquisition strategy. The modulation is DQPSK and native-shaped, but whether the framing is expressible as a block DAG is not knowable from the outside | Reading the published Iridium RE write-ups, or a `gr-iridium` plugin spike |
| P25 Phase 2 | H-DQPSK / H-CPM TDMA burst structure. The TIA standard is paywalled and the only public implementation is op25 | Access to the standard, or a scoped op25 plugin |
| DJI DroneID / OcuSync | Proprietary OFDM; public RE exists but the scrambler/frame detail was not verified here | Reading the published DroneID papers |
| TETRA voice | The ACELP codec's availability and licensing; `osmo-tetra` is **AGPL-3.0**, whose network clause is stricter than GPL's | A licence read, if voice is ever wanted. The control channel is unaffected |
| GSM native partial | Whether a BCCH-only native recipe is expressible without the TDMA burst scheduler living outside the block DAG | A read of the GSM 05.03 channel coding spec against the catalogue |
| LoRa's diagonal interleaver | Whether the existing `deinterleave` `permutation` parameter already expresses it, or whether CSS needs its own | A 30-minute check against `hk_blocks::blocks::framing` |
| Golay(24,12) and Hamming(13,9,3) | Whether the existing `bch` block already expresses them — **both are BCH codes**, so it may already be a solved problem and one line of the gap list may not exist | A 30-minute check against `bch`'s `n`/`k`/`poly` parameters. **Do this before filing a `golay_hamming` ticket** |

### 6.3 Counts

| Disposition | Families |
|---|---|
| **native recipe** | **36** — 11 with the catalogue as it stands (5 of them shipped), 25 with named additions |
| **wrapped plugin** | **8** |
| **out of reach** | **8** |
| **unknown** | **6** (plus two sub-questions that are cheap checks, not research) |

Total **58 families** (60 table rows, two of which are the sub-questions). GNSS is **excluded by carve-out**, not counted: ADR-0018 makes it the one documented
known-code-led exception and it is C36's business. `gr-inspector`, `gr-fhss_utils`, `gr-pdu_utils`
and `gr-leo` are excluded too — they are analysis helpers and a simulator, not decoder families,
and §1.4 already records them as prior art worth reading for C10.

**The shape of the result, stated plainly:** the largest disposition by a wide margin is *native*,
and that is not an optimistic reading — it falls out of the front end. The families this device can
actually receive are overwhelmingly narrowband, and narrowband is exactly where a block DAG is a
complete answer. The families that want GNU Radio's heavy machinery (OFDM television, cellular,
wideband satellite) are mostly the families the 8-bit, unpreselected, 20 Msps front end cannot serve
anyway. **Wrapping is the right answer for 8 families, not for 58**, which is the opposite of what
"target the whole suite of GNU Radio decoders" sounds like it implies, and it is the audit's main
finding.

### 6.4 The honest exclusions, and the ADS-B point

**Honest exclusions** (front-end facts, already recorded per use case in `docs/use-cases.yaml`):
everything above 6 GHz with no commodity downconverter, everything needing transmit, everything
needing phase-coherent multi-channel receive, and everything needing more than ~20 MHz of
instantaneous bandwidth. Separately and *not* an exclusion: below 1 MHz (SPACE-001/002, SIGNAL-013,
SIGNAL-016, SIGNAL-078) and Ku/C band (SIGNAL-037, PROP-036, RESEARCH-014) are `needs-accessory` —
an upconverter, a VLF front end or an LNB genuinely brings them into range, and docs/06 §3 is
explicit that this is a different tag from `out-of-band`.

**The ADS-B / pagers / radiosondes point, explicitly.** Of the 58 families, ADS-B, POCSAG, ACARS and RDS
are **already shipped** and radiosondes and AIS are one small block away each. They contribute **zero**
of the top five catalogue gaps in §7 and account for 4 of the 58. A coverage roadmap whose
visible progress is another ADS-B decoder has spent itself on the part that was already done —
CLAUDE.md says so directly, and this audit is the arithmetic behind it. Their remaining value is as
**oracles**: a family with a known-good external decoder is a family where a synthesized pipeline can
be checked against ground truth, which is what tutorials 1–4 already do and what ADR-0015 §7's
corpus needs.

**And a gap in docs/05 itself, found while filling the use-case column: there is no paging use case.**
Neither POCSAG nor FLEX has an ID — `grep -i 'POCSAG\|FLEX' docs/use-cases.yaml` is empty — even though
this project ships a POCSAG recipe as tutorial 2 and `mlevel_slicer` would add FLEX. That is not an
argument for renumbering anything (IDs are permanent and appended), and it is not this ticket's to
fix; it is recorded so the next person filling a coverage table does not conclude the families are
low-value when what is actually missing is the row in docs/05. AWARE-070 (IoT sensor census) is the
nearest neighbour and is **not** about paging.

---

## 7. The catalogue gap list — ranked by families unlocked

This is the audit's main product. Each row is an addition to ADR-0011 §1.5's catalogue. "Families"
counts the §6.2 rows that **cannot reach a decode verdict without it**; a family blocked on four
blocks is counted against all four, because none of them alone unlocks it.

| Rank | Block | Families | Additive or contract change | Rough cost | Unlocks |
|---|---|---|---|---|---|
| **1** | **`psk_demod`** (carrier recovery + matched filter; BPSK, QPSK, OQPSK, D8PSK, π/4-DQPSK) | **11** | **Additive, with one contract question** (§8) | Medium, **possibly small** via liquid-dsp | CCSDS telemetry, LRPT, HRIT/LRIT, HRPT, Inmarsat, Orbcomm, VDL2, Zigbee, TETRA, HF modems, (Iridium) |
| **2** | **`descramble`** (additive/multiplicative LFSR: CCSDS, PN9, BLE whitening, V.35) | **10** | Additive | **Small** — an LFSR and a reset rule | CCSDS telemetry, LRPT, HRIT/LRIT, HRPT, VDL2, BLE, Remote ID, radiosondes, TETRA, LoRa |
| **3** | **`viterbi`** (convolutional, punctured, CCSDS R=1/2 K=7, trellis) | **10** | Additive (`soft\|bits → bits`, ports already exist) | Medium | CCSDS telemetry, LRPT, HRIT/LRIT, HRPT, Inmarsat, TETRA, P25 P1, NXDN, DAB+, MIL-STD-188-110 |
| **4** | **`reed_solomon`** (RS(255,223) dual-basis, RS(255,239), RS(204,188), short P25 codes) | **8** | Additive — sits beside `bch` in group `fec`, same `frames → frames` shape | Medium | CCSDS telemetry, LRPT, HRIT/LRIT, HRPT, VDL2, radiosondes, P25 P1, DAB+ |
| **5** | **`mlevel_slicer`** (M-ary symbol decision: 4-level C4FM dibits, 8-ary) | **5** (+1 contingent on P25 P2) | Additive — `soft → bits`, k bits per symbol | **Small** | FLEX, P25 P1, DMR, NXDN, 2G ALE |
| 6 | `bitstuff` (HDLC flag/zero destuffing) | 3 | Additive | **Very small** | AIS, AX.25/APRS, VDL2 |
| 7 | `codeword_map` (constant-weight / m-of-n table) | 2 | Additive | Very small | RTTY/NAVTEX/SITOR/DSC, wM-Bus mode T |
| 8 | `ssb_demod`, `cw_demod` | 1 | Additive — ADR-0011 §8.4 **explicitly declined** them | Medium (carrier estimate, raster snap, clarifier) | SSB/CW; retires ADR-0015 §12.5's `legacy` path |
| 9 | `css_demod` (dechirp + demap) | 1 | Additive | Medium | LoRa |
| 10 | `despread` (chip-sequence correlator) | 1 | Additive | Small | Zigbee/802.15.4 |
| 11 | `ofdm_demod` + chanest/equalise/demap | 1 | **Contract change** (symbol-vector output) | **Large** | DAB+ — and *only* DAB+ (see below) |
| 12 | `equalise` (adaptive, decision-feedback) | 1 | Additive | Medium | HF data modems |
| 13 | a `raster` output kind | 1 | **Contract change** — a new output kind beside §8.2's `audio` | Medium | HF fax, SSTV, APT, Tempest |
| — | `ldpc`, `turbo`, `polar` | **0** | — | — | **Deliberately not on this list.** Every family they serve (DVB-S2 transponders, 5G, FT8) is *out of reach* or *wrapped* for reasons a block cannot fix |

**Three things the ranking says that the intuition does not.**

1. **`descramble` is the best buy on the board.** It ties `viterbi` at 10 families and costs an
   LFSR — no liquid-dsp, no new port semantics, no reference reading. It also directly serves
   RESEARCH-012 (whitening/scrambler identification) as a *detection* capability, not just a
   decode stage. **Build it first**, even though `psk_demod` ranks above it, because rank 1 is a
   months-long investment and rank 2 is a week.
2. **OFDM is the audit's sharpest inversion.** It *looks* like the biggest unlock — Wi-Fi, LTE,
   DAB, DVB-T, HD Radio, DroneID, ATSC — and it is the **smallest**, because the front end takes
   Wi-Fi, LTE, 5G and DroneID off the table, `gr-dtv`'s real receive chains make DVB-T/ATSC/ISDB-T
   plugin jobs, and `nrsc5` takes HD Radio. One clean native family survives: **DAB+**, at 1.536 MHz.
   A large native OFDM investment for one family is not obviously wrong, but it must be argued as
   "for DAB+", not as "for OFDM".
3. **PSK + Viterbi + Reed–Solomon is one investment, and the ticket's premise is confirmed.**
   ADR-0015 §10's M-14 ("optional `psk_demod`/Costas") buys one stage of a seven-stage ladder.
   Eight families need all three; **none** of the eight reaches past verdict `demodulated` with only
   the first. Scoping them as one-plus-two-optional-extras would deliver a PSK demodulator that
   unlocks Orbcomm and nothing else. §9's tickets reflect this.

### 7.1 Check liquid-dsp first — and the finding that changes the estimate

The ticket asks that liquid-dsp be established **before** scoping, because ADR-0011 §1.6's rule is
that a block is an **adapter over an existing kernel**, and liquid-dsp is MIT, already this
project's designated DSP kernel library (ADR-0010's table names it for exactly this), actively
released, and already ships PSK/QAM modems, NCO/PLL, framing and FEC — which would make ranks 1, 3,
4 and 12 adapter jobs rather than DSP jobs, and would moot any licence question for them entirely.

**The finding: liquid-dsp is designated in ADR-0010 but is not a dependency of any crate today.**
`grep liquid crates/*/Cargo.toml` returns nothing; every block in the catalogue adapts an in-repo
kernel (hk-dsp, hk-demod, hk-estimate) per §1.6's list. So "it is an adapter job" is a **claim about
a binding that does not exist yet**, and the first of these blocks pays for landing the FFI — a C
build in the workspace, aarch64/JetPack cross-compilation for the Jetson, and a decision about
vendoring versus a system library. That is a real cost and it belongs to the first ticket, not
spread invisibly across five. **T-607 lands or refuses that binding, and T-608/T-609/T-610 are
priced against its answer.** This is exactly the check the ticket asked for, and the answer is "not
yet", not "yes".

---

## 8. What ADR-0011 would have to absorb

Not written here — ADR-0011 is untouched by this audit and its status is unchanged. This is the
delta an amendment (T-606) would carry, in the style of §8's audio amendment.

1. **Catalogue rows.** §7's ranks 1–13 as additive §1.5 entries, each pinning a descriptor in
   `planned()` and enforced by the existing `implemented_blocks_match_their_pinned_descriptors`
   drift test. Groups: `iq` (`psk_demod`, `css_demod`, `ssb_demod`, `cw_demod`, `ofdm_demod`),
   `symbol` (`mlevel_slicer`, `descramble`, `bitstuff`, `codeword_map`, `despread`, `equalise`),
   `fec` (`viterbi`, `reed_solomon`).
2. **The one real contract question: what a PSK demodulator puts on its output port.** §1.1's
   `soft` type is "`f32` soft symbol/**bit**, positive = 1" — one value per item. That is a complete
   answer for BPSK and for every 2-level family the catalogue serves today, and it is **not** one
   for QPSK, 8PSK or a constellation whose symbol carries k bits jointly. Two options, and the
   amendment must pick one:
   - **Additive (preferred):** `psk_demod` de-maps *inside the block* and emits **one `soft` item
     per bit** at k × the symbol rate. No new port type, no change to `slicer`, `descramble`,
     `viterbi` or anything downstream, and it works today. The cost is that the joint-symbol
     information is discarded at the block boundary, which a soft-decision equaliser or an
     iterative decoder would want.
   - **Contract change:** a new `symbols` port type carrying complex constellation points or LLR
     vectors. §1.1 says adding a type is a contract change reviewed like the ADR, and it would
     touch every block that accepts `soft`.
   **`ofdm_demod` forces the same question and cannot take the additive answer**, because its
   natural output is a subcarrier × symbol grid. So the honest sequencing is: take the additive
   answer for PSK now, and treat the port-type change as **OFDM's** cost rather than PSK's.
3. **Soft-decision FEC.** `viterbi` at rank 3 wants soft input; the existing `fec` group is
   hard-decision `frames → frames`. A `soft|bits → bits` streaming decoder sits inside the existing
   port types, so this is additive — but it makes `fec` a group with two shapes in it, and §1.5's
   table should say so rather than leave it to be discovered.
4. **A new output kind, if rank 13 is taken.** `raster` beside §8.2's `audio`, with the same
   double-gating (`content_class` ceiling, `output_policy` clamp) and the same schema-version rule.
   §8.6 bumped `schema_version` 2 → 3 for three keys at once; this is a fourth key of the same shape.
5. **§1.6's "adapter over an existing kernel" rule needs a second column.** Today it names in-repo
   kernels. Ranks 1/3/4/12 would name **liquid-dsp**, which §7.1 establishes is not yet linked. The
   rule should say what a block adapts *and whether that kernel is in the build*, so the next audit
   cannot repeat this mistake.
6. **A block-catalogue gap is a product-visible state.** Already ADR-0015 §8 and T-550's territory,
   restated here because the gap list makes it concrete: "we have no `css_demod`" and "this is not
   LoRa" must not read alike.

---

## 9. Tickets filed from this audit

Filed 2026-09-21 by T-554, appended to `docs/tasks.yaml`. **No block is implemented by T-554.**

| Ticket | What |
|---|---|
| **T-606** | ADR-0011 amendment: the coverage catalogue delta and §8.2's port-type decision (`core_interface`) |
| **T-607** | liquid-dsp: land the FFI binding or record the refusal — §7.1's finding, and the thing four block tickets are priced against |
| **T-608** | `descramble` — rank 2, the best cost-per-family on the list, and the one that needs neither T-607 nor a contract answer |
| **T-609** | `psk_demod` — rank 1; **supersedes ADR-0015 §10's M-14**, which is too narrow on its own |
| **T-610** | `viterbi` — rank 3 |
| **T-611** | `reed_solomon` — rank 4 |
| **T-612** | `mlevel_slicer` — rank 5, the trunking/paging unlock |
| **T-613** | `bitstuff` — rank 6; smallest block on the list, three families |

Not filed, and deliberately: `css_demod`, `despread`, `codeword_map`, `ssb_demod`/`cw_demod`,
`ofdm_demod`, `equalise` and the `raster` output kind. Each unlocks one or two families, and filing
thirteen tickets from an audit whose whole point is that one block can be worth six would be the
error the audit exists to prevent. They are recorded in §7 and are ready to be filed when the top
five have landed and the ratio has changed.

---

## Sources

- ADR-0003 — process and plugin model: [docs/adr/0003-process-plugin-model.md](adr/0003-process-plugin-model.md)
- ADR-0010 — language, toolchain and dependency licence ledger: [docs/adr/0010-language-and-licence-ledger.md](adr/0010-language-and-licence-ledger.md)
- ADR-0011 — decoder workbench contracts (blocks, recipes): [docs/adr/0011-decoder-workbench-contracts.md](adr/0011-decoder-workbench-contracts.md)
- ADR-0015 — decoder synthesis contracts (search, evidence, templates): [docs/adr/0015-decoder-synthesis-contracts.md](adr/0015-decoder-synthesis-contracts.md)
- ADR-0018 — GNSS known-code exception: [docs/adr/0018-gnss-known-code-exception.md](adr/0018-gnss-known-code-exception.md)
- Plugin wire contract: [docs/stream-contract.md §9](stream-contract.md); plugin authoring rules and the trust boundary: [plugins/README.md](../plugins/README.md)
- The readsb worked example: [plugins/readsb/manifest.json](../plugins/readsb/manifest.json), `crates/hk-plugins` (host, manifest, ingest, output)
- Capability card C22 — decoder plugins: [docs/capabilities/C22-decoder-plugins.md](capabilities/C22-decoder-plugins.md)
- Existing framework survey: [docs/03 §1.1–1.2, §3](03-sdr-software.md) (GNU Radio 3.10 and 4.0, OOT status, decoder inventory, gap table)
- Use cases and their `hardware_fit`: [docs/05](05-use-cases-and-explorations.md), [docs/use-cases.yaml](use-cases.yaml)
- Tutorials as decoder precedent: [RDS](tutorials/01-rds.md), [POCSAG](tutorials/02-pocsag.md), [ACARS](tutorials/03-acars.md), [ADS-B](tutorials/04-adsb.md)

### Web sources for the §1 survey

Surveyed 2026-09-20. Per-module repository URLs are cited inline in §1.4; the indexes, the primary
evidence on bitrot and the GR4 licence statements are:

- [CGRAN](https://www.cgran.org/) · [GNU Radio wiki `OutOfTreeModules`](https://wiki.gnuradio.org/index.php/OutOfTreeModules) · [GitHub topic `gnuradio`](https://github.com/topics/gnuradio) · [gr-recipes](https://github.com/gnuradio/gr-recipes) · [gr-etcetera](https://github.com/gnuradio/gr-etcetera) · [PyBOMBS](https://github.com/gnuradio/pybombs)
- SWIG → pybind11, "breaks OOT interface": [discuss-gnuradio, 2020-06](https://lists.gnu.org/archive/html/discuss-gnuradio/2020-06/msg00095.html)
- "OOTs don't work with 3.8": [gnuradio#2075](https://github.com/gnuradio/gnuradio/issues/2075)
- Porting guides: [3.8](https://wiki.gnuradio.org/index.php/GNU_Radio_3.8_OOT_Module_Porting_Guide) · [3.10](https://wiki.gnuradio.org/index.php/GNU_Radio_3.10_OOT_Module_Porting_Guide) (GR4 guide exists; full text unverified — Cloudflare blocks non-browser fetches)
- gr-leo 98 compiler errors against 3.10.1.1: [gr-leo#48](https://gitlab.com/librespacefoundation/gr-leo/-/issues/48)
- gr-satellites' per-GR-version release lines: [discussion #459](https://github.com/daniestevez/gr-satellites/discussions/459), [issue #215](https://github.com/daniestevez/gr-satellites/issues/215)
- GR4 licence and ported-block policy: [Community stewardship, 2026-05-21](https://www.gnuradio.org/news/2026-05-21-gr4-community-stewardship/) · [RC1, 2026-03-22](https://www.gnuradio.org/news/2026-03-22-gr4-release-candidate-1/) · [GR4 workflows, 2025-12-17](https://www.gnuradio.org/news/2025-12-17-gr4-transform-sdr-workflows/) · [gnuradio4-core](https://github.com/gnuradio/gnuradio4-core) (MIT) vs [fair-acc/gnuradio4](https://github.com/fair-acc/gnuradio4) (LGPL-3.0, prototype)
- [liquid-dsp](https://github.com/jgaeddert/liquid-dsp) (MIT, v1.8.2 2026-08), the licence-clean kernel alternative referenced in §3
