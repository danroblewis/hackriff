# 18 — Decoder coverage: GNU Radio as the reference set

Status: **direction / planning** (2026-09-20, from the user). Milestone **MAUTO** (design track; build
gated behind the robustness work). No product code comes from this document. Contracts it touches:
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
`gr-filter`, `gr-digital`, … all GPLv3) and **VOLK** — which ADR-0010's ledger already flags as
GPLv3 and **plugin/subprocess only, never in a non-GPL core**. All of that lives inside the child
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

### 2.4 The per-decoder cost, and the shape of the curve

The honest answer is that **nobody has measured it for a GNU Radio decoder**, which is why
**T-553** is a spike and not an estimate. What can be said about the shape:

- The **first** one pays the runtime integration cost: getting GNU Radio to build and run on the
  Mac and on aarch64/JetPack, establishing the adapter pattern, deciding how a flowgraph is
  parameterised from the manifest.
- The **second and later** ones pay manifest + adapter + fixture + ledger row + contract test —
  broadly the readsb job, which is a known quantity.
- There is a **structural question that changes the slope**: whether one shared
  `hk-plugin-gnuradio` host that loads a named flowgraph can amortise the runtime across decoders,
  or whether per-decoder processes are unavoidable. If a shared host works, the marginal decoder is
  a flowgraph file; if it does not, each one is a full process with a full runtime. T-553 answers
  this, and the roadmap's slope depends on the answer.

Until that number exists, **§3 ranks by disposition, and prefers dispositions that do not need it.**

---

## 3. Coverage as a ranked roadmap

Ranked by value per unit of work against [docs/05](05-use-cases-and-explorations.md) and the front
end's real reach — 1 MHz – 6 GHz, ~20 Msps instantaneous, 8-bit, half-duplex, no preselector, **one
tuned window at a time**. The disposition column is the claim this roadmap makes; **T-551** is the
ticket that tests each one properly and produces the block-catalogue delta.

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
engine at the same time. T-554 designs the bridge.

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
**The cost may be much lower than it looks**, and T-551 should check this first:
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

**This section is a proposal. It is the user's call, and T-552 exists to get it decided and
recorded.** It is a working engineering rule for this project, not legal advice, and the project's
own licence remains undecided (CLAUDE.md).

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
| **ADR-0015 §4 templates** | The bulk of tier-1 coverage. Every imported protocol is a search seed as well as a decoder. T-554 designs the provenance and the fact/implementation line; T-551 says which families are template-shaped. |
| **ADR-0011 §1.5 block catalogue** | The tier-2 and tier-3 families. T-551 produces the delta (PSK/Costas, Viterbi, Reed–Solomon, CSS, OFDM, DSSS, scramblers, SSB/CW) with an ordering. ADR-0015's M-14 covers only `psk_demod` and is too narrow on its own. Note the pattern ADR-0011 §1.6 already sets: **blocks are adapters over existing kernels, not rewrites** — today over hk-dsp, hk-demod and hk-estimate, and tomorrow over liquid-dsp (MIT), which already carries much of what tier 2 wants. A new block is often an adapter job, not a DSP job, and that is the first thing T-551 should check per family. |
| **ADR-0003 / `plugins/`** | The handful of genuinely active, genuinely hard decoders where wrapping beats rebuilding — SatDump, op25, gr-satellites. T-553 measures whether that is true at all. |

And it changes one thing about how the engine tickets should be read: **a block-catalogue gap is a
product-visible state, not a silent absence.** ADR-0015 §8 already says an unsupported structure is
reported as the verdict reason; T-547 makes that distinguishable from "searched and failed", because
"we have no CSS block" and "this is not LoRa" must not read alike to a user. The coverage roadmap and
the honesty rules are the same work seen from two sides.

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
