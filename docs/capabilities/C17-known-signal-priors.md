# C17 · known-signal-priors
> Layer C — Characterize · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C06, C29, C27, C12 · Used by: C15, C19, C13, C27, C30, C04, C23

## Purpose
Answers "what is *supposed* to be here?" and "is this expected?" for a frequency, bandwidth, location and time. It uses cached allocation tables, band plans, licence extracts, signal databases and the device's own history. It exists to separate known from unknown (workflow step 4), not as the goal, so it always leaves non-zero probability for unknown or non-compliant emitters. It reasons only about frequencies, licences and emission metadata; no content interception is implied. C17 only *supplies* priors — C15 fuses them with likelihoods and does the classifying (docs/06 §5). It sits in a Layer E loop: C17↔C27 and C17↔C30 read and write each other (inventory and correlation both consume priors and write status back; docs/06 §2.1).

## Interface
- **Query inputs** (provisional names):
  - Frequency (ppm-corrected) ± uncertainty, and bandwidth.
  - Optional features: family, symbol rate, deviation.
  - Location from C06, or a user-set region.
  - UTC time; ITU region/country.
- **Output: `PriorResult`.**
  - `candidates[]`: identity/service, source layer, P(c|f,ℓ), evidence (ULS record, parsed emission designator), distance, data age.
  - Expected raster, modulation and bandwidth.
  - `status ∈ {expected, unexpected, no_reference_data}`.
  - Unknown share; coverage and staleness flags.
- **Config:** λ₀…λ₃ (λ₀ > 0 enforced), licence radius, per-source enable, region.
- **Offline packs:** band table, ULS spatial index, Artemis SQLite, FMLIST/SatNOGS lists (synced by C29), per-user RadioReference import.

## Methods
- **Prior** (docs/04 §1.1):
  - `P(c|f,ℓ) = λ₁P_alloc + λ₂P_license + λ₃P_history + λ₀P_uniform`.
  - Posterior ∝ `p(x|c)·P(c|f,ℓ)`.
  - Priors inform but never veto evidence (docs/04 §12 #8).
- **Layers** (docs/04 §1.1):
  - **Allocation:** ITU RR Art. 5; 47 CFR §2.106 via the eCFR API (parse once, ship a compact table).
  - **Band plan:** FCC rule-part rasters (6.25/12.5/25 kHz), 3GPP tables.
  - **Assignment:** FCC ULS pipe-delimited `.dat` tables (HD, EN, LO, FR, EM); weekly full plus daily transactions; build a lat/lon spatial index.
  - **Observed:** own history (C27/C12) and sigidwiki/Artemis as weak "looks like" priors.
- **Emission designators** (docs/04 §1.1):
  - Characters 1–4 = bandwidth (`11K2` = 11.2 kHz), 5 = main-carrier modulation, 6 = signal nature, 7 = information type.
  - `8K10F1E` = P25, `7K60FXE` = DMR. Compare with C13/C15 measurements.
- **Time priors:** e.g. radiosondes at 00Z/12Z (docs/04 §1.2).
- **"Unexpected here" badge:** mirrors professional licence-DB comparison and alarm masks (docs/04 §11.1–11.2).
- **Tolerance:** widen by the C05 ppm uncertainty. 1 ppm at 1 GHz = 1 kHz misplaces 12.5 kHz channels (docs/04 §10.1).
- **International:** CEPT ERC Report 25/EFIS, Ofcom WT Register, ISED SMS.

## Platform constraints
- **Compute** negligible (docs/06). Storage and indexing are the budget; ULS extract size isn't in the docs (spike).
- **Offline-first.** Works on a stale cache and shows data age; syncs via C29.
- **No GNSS fix:** fall back to region-level allocation priors.
- **Ghost alarms.** The 8-bit, preselector-less front end makes IMD at unlicensed frequencies (docs/02 §1.7). Require clear C05 suspect flags before raising `unexpected`.

## Prior art and reuse
- **eCFR API, NTIA chart, FCC ULS files; gdubin/uls parser.** Licence: check.
- **RadioReference SOAP API:** best trunking metadata, but **each end user needs RR Premium**. No bundled data; per-user import only.
- **Artemis** v4.2.0 (2026-07), active; offline frequency/bandwidth/mode/modulation/ACF DB. Licence: check.
- **sigidwiki:** human-oriented, no feature vectors (docs/03 §3.7). Licence: check.
- **FMLIST, SatNOGS DB:** licence: check. These appear in both C17 and C29 — C29 fetches and caches them, C17 is the query/prior interface over the cache (same data, two roles; docs/06 §5).
- **RTL-ML:** service-level classification with location context is cheap and useful (docs/03 §4.2).

## Pitfalls
- **Allocation ≠ assignment ≠ use.** Licence-by-rule and Part 15 bands (ISM, key fobs, FRS/MURS/CB) have no site records, so "no record" isn't "unexpected" there.
- **Federal assignments are generally not in ULS** (not in the docs; verify).
- **Staleness.** NOAA APT ended in 2025 (docs/04 §1.2); static DBs decay.
- **Regional differences:** 315 vs 433.92 MHz, 8.33 kHz aviation raster, 9 kHz AM raster.
- **Self-reinforcement.** A history prior repeats early misclassifications; keep provenance and decay.
- **Overweighted priors** hide the pirates and stuck transmitters the tool exists to find.
- **Privacy and legal:**
  - ULS and amateur records name individuals. Show service and licence class by default; gate licensee identity.
  - Honour RadioReference terms.
  - Labels like "cellular" or "common-carrier paging" never imply a content path (docs/04 §1.3).

## Testing
- **Unit tests:**
  - Designator parser against the docs/04 §1.1 examples.
  - ULS parser on a small public extract.
  - Band edges.
  - Invariant: unknown mass > 0 on every query.
- **Scenario fixtures** (docs/04 §1.2):

| Query | Expected |
|---|---|
| 118–137 MHz | AM prior |
| 161.975 MHz | AIS |
| ~403 MHz at 00Z/12Z | Radiosonde |
| 1090 MHz | ADS-B |
| 902–928 MHz | Many ISM candidates, none "unexpected" |
| Unlisted 88–108 MHz carrier | `unexpected` |

- **SigMF replay with GNSS metadata:** check flags against decoder truth (RDS PI vs FMLIST, AIS, ADS-B).
- **Metrics:**
  - C15 top-k accuracy with and without priors; docs/04 §12 #8 claims a "large accuracy gain" without a number.
  - City false-`unexpected` rate.
- **Offline:** stale-cache and no-GNSS tests.

## Example use cases
Regenerated from `use-cases.yaml`:
- AWARE-017 — Tower inventory cross-reference
- AWARE-052 — Pirate / unlicensed broadcaster hunting
- AWARE-053 — Allocation lookup for unknown signals
- AWARE-054 — Amateur-band intruder logging
- SIGNAL-071 — Numbers stations & UVB-76
- RESEARCH-007 — Catalog unknowns against Sig ID Wiki
- RESEARCH-014 — (priors primary)
- RESEARCH-015 — (priors primary)
- AWARE-013 — (cellular/tower cross-reference)
- AWARE-016 — (licence cross-reference)

## Open questions
- **Fusion owner (resolved, docs/06 §5).** C17 supplies priors; C15 fuses them. C17 does not classify.
- **λ weights.** Fixed, user-tuned, or learned from decoder labels?
- **Overlap with C29 (resolved, docs/06 §5).** FMLIST/SatNOGS: C29 fetches/caches, C17 is the query/prior interface — same data, two roles.
- **Data packs.** Format, cadence, size budget, non-US packs. Licensee-display policy (doc 07 / a Phase 3 ADR).
- **Missing edges (resolved, docs/06 §2.1).** C17 feeds C15, C19, C04, C27 and C30, and sits in C17↔C27 / C17↔C30 loops.

## Reading list
1. docs/04 §1.1 "Allocation vs. assignment vs. actual use"
2. docs/04 §1.3 "Legal considerations (US; not legal advice)"
3. docs/04 §1.2 "What lives where (HF to ~6 GHz, US focus)"
4. docs/04 §11.2 "Mapping to an exploration device"
5. docs/03 §3.7 "Signal identification references"
6. docs/04 §12 "Takeaways: prioritized implementation list" (item 8)
