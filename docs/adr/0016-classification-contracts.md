# ADR-0016 — Classification contracts: Classification with open-set unknown, prior fusion, classical cascade, signatures and clustering, ml-runtime, blind evaluation

**Status:** PROVISIONAL (T-198, core interface, planning only; reviewed before the T-211 skeleton lands)
**Touches:** C15 modulation classifier, C18 fingerprint signatures, C38 ml-runtime; inputs C13/C14/C16, priors C17, consumers C27 inventory, C12/C04 attention ([ADR-0012 §4](0012-attention-memory-contracts.md)), recipes ([ADR-0011 §2.4](0011-decoder-workbench-contracts.md)), decoder synthesis (MAUTO, [docs/15](../15-decoder-synthesis.md), ADR-0015 in progress under T-208); Emitter ([docs/07 §2.11](../07-data-model.md)), Annotation §2.13, Decode §2.15; [ADR-0007](0007-compute-placement.md) provider model
**Code (today):** `hk-estimate::blind::Family {Unknown, Ook, Fsk, Bpsk, Qpsk}` with S5 floors; `hk-pipeline/src/family.rs` (label → service vocabulary, ranked explanations, `FAMILY_MAP_VERSION`); `hk-model` `emitter_classification` table + `Classification {family, confidence, open_set_score, model_version}`, `FAMILY_ORDER` (T-183), `cluster::Fingerprint` v1; `hk-pipeline/src/candidates.rs::class_entropy`. Nothing in this ADR is implemented yet; T-211 lands the skeleton. ~~(As written for T-198.)~~ **Stale as written — M3 has since landed (T-211, T-199–T-205, T-212–T-215); read §9/§10's per-task cells, not this line, for what exists. The one row that did not land is the shadow store and the `/api/ml` routes, reassigned from T-203 to T-844 by the §10 amendment of 2026-09-22 (T-365).**

## Context

M3 (docs/11) adds classification: a cascaded classical classifier with open-set output (C15), signatures and clustering of unknowns (C18), and an ML runtime (C38). Today "family" is a string written by three unrelated producers: demod chain labels (`wfm`, `2fsk`), decoder ids mapped to services (`decoder:readsb` → `adsb`), and track shape. The row has no distribution, taxonomy, or explicit unknown. T-183 only ranks track shape below the rest.

Constraints carried in:
- **Blind first, database suggests.** Priors (C17) never veto measurement. Mismatches get flagged, and unknown signals are the priority (CLAUDE.md).
- **Classical first, ML as a later stage** after normalisation, with open-set output, never a softmax unknown (CLAUDE.md key findings; C15/C38 cards).
- **8-bit front end.** S5 measured the family floors: FSK/OOK labels need ≥ 20 dB in-band SNR, PSK ≥ 15 dB. Below those floors the chain abstains and has never mislabelled (0 trusted-and-wrong in 900 synthetic runs and on real 915 MHz FSK; `spikes/s5-blind-estimation/REPORT.md` §3). OOK/PSK on real signals is still INCONCLUSIVE.
- **Decode confirms.** A CRC-valid decode is the only thing that confirms a signal (docs/15 §1). Classification and signatures rank; they do not identify.
- **Mac first.** GPU and ML providers run on the Mac behind a conformance suite; CUDA/TensorRT waits for the Jetson phase.
- **Thin client** over `docs/api.md`; e2e tests run blind through the mock SDR.

## Decision summary

| Question | Decision |
|---|---|
| Where the types live | `hk_model::classify` (taxonomy, `Classification`, thresholds, pure `fuse`, arbitration rank) and `hk_model::signature` (`EmissionFeatures`, `Signature`, `SignatureMatch`, `SignatureCluster`). Classifier implementation in a new crate **`hk-classify`**. ML host in a new crate **`hk-ml`**. Matching and clustering engines in `hk-context/src/signature/`. |
| Taxonomy | Tree `hk-mod@1`: coarse (analog / digital / noise-like) → family → class. `unknown` is a global open-set outcome, not a leaf. Class names reuse existing labels (`2fsk`, `wfm`, `bpsk`…). |
| Classification | Posterior **and** likelihood-only distributions over families, each including `unknown`. Also: optional within-family class distribution, open-set score, normalised entropy, deciding stage, provenance (features@version, rules/model@version, SNR vs gate, suspect flags), and reason codes. Stored additively on `emitter_classification` (migration 0006). |
| Current family | `FAMILY_ORDER` becomes an arbitration rank: user 0 > decoder (CRC-valid) 1 > lock-verified (demod lock, ALRT/GLRT) 2 > classifier 3 > track shape 4; latest wins among equals. This is a superset of the T-183 rule. |
| Fusion | `P(c∣x,f,ℓ) ∝ p(x∣c)·P(c∣f,ℓ)` over known families only, with λ₀ ≥ 0.1 enforced. The unknown mass is set by the open-set score and is never scaled by priors. **Evidence-dominance:** when the likelihood ratio of the likelihood top-1 over the runner-up is ≥ 10, the posterior top equals the likelihood top. A prior that flips a closer call sets `prior-tiebreak`; disagreement sets `prior-mismatch`. |
| Cascade | Feature tree (C13 shape, instantaneous statistics, cumulants, cyclic features, C16) → class-conditional densities give `p(x∣c)` and a χ² open-set score → optional verifier (ALRT/GLRT, post-sync only, can only re-rank) → optional per-family DL, within-family class only, energy-score open set → fusion → decoders arbitrate. |
| DL enable | Per family, only when the dev-set evaluation beats classical by ≥ 5 points at every SNR bin ≥ the gate, with the 95 % CI lower bound > 0, no AUROC loss > 0.02, and no higher false-known rate. Otherwise the stage runs in shadow mode or is off. |
| C18 | `EmissionFeatures` (per-field value ± σ, method, n) superset of `Fingerprint` v1; immutable versioned `Signature`s (SQLite, seeded from recipe `match` + CRC-confirmed emitters); `SignatureMatch` full/partial/none with ranked candidates, never an identity or status. `Fingerprint` carried T-233's `ModulationStructure` (value ± σ) as split-only evidence until **T-594 removed it**: it had no producer, so it was on 0 real-run fingerprints and took part in 0 comparisons (§5.1). Its family comparison stays **exact**: §1's `family_of` relaxation is withdrawn. |
| Clustering | Online leader clustering on the **tolerance-normalised** distance shared with matching (ε = 1), min 3 members before a cluster becomes visible, nightly batch DBSCAN repair with append-only merge/split history. A cluster is a *type* above emitters (*instances*). |
| ML runtime | `MlProvider` trait over ONNX models loaded at runtime (no rebuild). CPU reference **tract** (pure Rust); Mac acceleration **ort + CoreML EP** behind opt-in `ml-coreml`; Jetson **ort + TensorRT EP** (`ml-trt`, deferred). Candle and Burn are rejected: model code in Rust means a rebuild per model. Hand-written wgpu is rejected for v1. |
| Evaluation | A-priori floors in this ADR. Dev and acceptance seeds are disjoint; OTA labels come only from CRC-valid decodes, split by session. Every number is reported per SNR bin × source (synthetic / OTA). T-206 is the exit gate. |
| MAUTO | `SearchSeed {classification, features, signature_match, cluster}`. The search *orders* by posterior and *prunes* only on the likelihood, so priors cannot prune. It reserves ≥ max(p_unknown, 0.2) of the budget for open search. |

## 1. Taxonomy `hk-mod@1`

| Coarse | Family | Classes (label strings) |
|---|---|---|
| analog | `analog` | `am`, `nbfm`, `wfm`, `ssb`, `cw` |
| digital | `ook-ask` | `ook`, `ask4` |
| digital | `fsk` | `2fsk`, `gfsk`, `msk`, `4fsk` |
| digital | `psk-qam` | `bpsk`, `qpsk`, `8psk`, `qam16`, `qam64` |
| digital | `ofdm` | `ofdm` (C16 parameters as features) |
| digital | `css` | `chirp` |
| digital | `dsss` | `dsss` (C16; may always abstain in M3) |
| digital | `pulsed` | `ppm`, `pulse` (radar/ADS-B-like envelopes) |
| noise-like | `noise-like` | `noise-like` (flat PSD, SK ≈ 0, no cyclic line: jammers, wideband noise, radiometric rises) |

- **`unknown`** is the open-set outcome at every level. A classification may be `coarse: digital`, family `unknown`: the "digital, unknown order" case below ~0 dB (C15 card).
- **Versioning.** The taxonomy is data (`hk_model::classify::taxonomy::HK_MOD_V1`). Adding a class or family is a new version (`hk-mod@2`). Rows keep the version they were written under. Readers map old labels through `taxonomy::family_of(label, version)`. The pre-M3 labels (`fsk`, `ook`, `bpsk`, `qpsk`, `wfm`, `nbfm`, `am`, `2fsk`) map into `hk-mod@1`. Service families (`adsb`, `fm-broadcast`) are not modulation labels. They stay decoder evidence in `family.rs` (`taxonomy: null`).
- **Fingerprint gate fix (T-211) — ~~proposed~~ WITHDRAWN (T-233, 2026-09-16).** The proposal was: `cluster::Fingerprint` compares family exactly, and since `fsk` and `2fsk` would otherwise split one emitter, the comparison should move to `family_of` at the family level. **It does not.** The gate stays exact; see §5.1 for what was measured and why. The withdrawal is itself additive (nothing changed) and `FEATURE_SET_VERSION` stays 1.

## 2. `Classification` contract (C15 output)

```rust
pub struct Classification {                 // hk_model::classify; serde, deny_unknown_fields, validate()
    pub schema: u16,                        // 1
    pub t: Timestamp,
    pub taxonomy: TaxonomyRef,              // "hk-mod@1"
    pub input: Option<InputRef>,            // existing input_kind/input_id (track, detection, demodulation, decode, snippet)
    pub coarse: Coarse,                     // analog | digital | noise-like | unknown
    pub posterior: Vec<LabelP>,             // families + "unknown"; sums to 1 ± 1e-6; no entry is exactly 1.0
    pub likelihood: Vec<LabelP>,            // evidence-only normalisation over the same labels (uniform prior)
    pub prior: Option<PriorUse>,            // {prior_ref, lambda: [λ0..λ3], dist: Vec<LabelP>} or None (no C17 data)
    pub family: String,                     // top posterior label (may be "unknown")
    pub confidence: f64,                    // posterior of `family`, ≤ 0.999
    pub class: Option<ClassCall>,           // {label, p, dist, stage} within `family`; None if below its gate
    pub open_set_score: f64,                // 0..1, higher = further from every known class
    pub entropy_norm: f64,                  // H(posterior)/ln K, K incl. unknown (ADR-0012 §4.1 class_entropy)
    pub stage: Stage,                       // deciding stage: feature-tree | verifier | dl | decoder | user | chain | track-shape
    pub provenance: ClassProvenance,        // below
    pub flags: Vec<ClassFlag>,              // prior-tiebreak, prior-mismatch, below-gate, suspect-input, dl-shadow-disagrees
    pub reasons: Vec<String>,               // machine codes: low_snr, too_short, clipped, multi_signal, no_cyclic_line …
}
pub struct ClassProvenance {
    pub rules: String,                      // "hk-classify/tree@1"
    pub features_version: u32,              // C15 feature-vector version (features@N, §4.2); 1 = indeterminate, see below
    pub features_ref: Option<FeaturesId>,
    pub ml: Option<ModelRef>,               // "amc-psk-qam@0.2.0#sha8", with provider + precision, only if stage=dl
    pub snr_db: Option<f64>, pub snr_gate_db: f64, pub gated: bool,
    pub thresholds: String,                 // "thresholds@1"
    pub suspect: SuspectFlags,              // clipped / suspect_imd / image_candidate / spur from Detection+Provenance
    pub power_mode: Option<String>,
}
```

**Legacy fit.** Columns `family`, `confidence`, `open_set_score` and `model_version` (= `provenance.rules`, or the `ml` ref for DL, or `decoder:<id>`) keep their meaning. So `/api/inventory`, `FAMILY_ORDER` readers, `class_entropy` and `family.rs` evidence keep working. Migration 0006 adds nullable `taxonomy`, `stage`, `arb_rank INTEGER` and `detail TEXT` (JSON of the rest). Rows written before M3 have NULLs and are read as legacy: `stage` is derived (`decoder:` prefix → decoder; `input_kind='track'` → track-shape; else chain). The append-only trigger is unchanged.

**Feature-set version (T-290).** `provenance.features_version` is the version of the **C15 feature vector** (`features@N`, §4.2) — not the C18 `EmissionFeatures` field set, which versions separately — and `1` is reserved to mean **indeterminate**. Every row written before T-290 carries `1` whatever vector produced it: `hk-classify` restated the constant as `1` while the vector moved to `2` (T-248) and `3` (T-286), so rows named a feature set that did not exist and disagreed with the version stamped on the densities they were scored against. The versions differ in what `symmetry` measures and, from `3`, in whether it is measured at all. Nothing is migrated — the table is append-only, and a `1` row's actual vector is not recoverable — so a reader treats `1` as unknown and `> 1` as exact: `hk_model::classify::FEATURES_VERSION_INDETERMINATE` and `ClassProvenance::names_a_feature_set`. There is now one definition of the constant (`hk_classify::features::FEATURES_VERSION`, re-exported by `thresholds`), so provenance and the shipped densities cannot drift again.

**Arbitration rank (replaces `FAMILY_ORDER`):** `ORDER BY coalesce(arb_rank, <derived>) ASC, classification_id DESC`.

| Rank | Stage | Why |
|---|---|---|
| 0 | user | Explicit reclassify (`family::reclassify`) |
| 1 | decoder | A CRC-valid decode is ground truth (C15 card) |
| 2 | verifier, or a chain label with demod lock (pilot/RDS lock, clock lock) | Post-sync evidence |
| 3 | feature-tree / dl (fused classifier) | Pre-sync evidence |
| 4 | track-shape | Occupancy only (T-183) |

- T-183's tests stay green: track-shape remains below everything.
- A rank-3 `unknown` never hides a rank-2 label. It is still appended to the history, and the API shows it as `latest`.

**Per-family thresholds `thresholds@1`** (a-priori; S5-derived where measured, otherwise **unverified** literature guesses):

| Family | SNR gate (in-band, C13 definition) | Class gate | Min confidence to report | Open-set max | DL |
|---|---|---|---|---|---|
| fsk, ook-ask | 20 dB (partial 15) — S5 | +3 dB | 0.6 | 0.5 | off |
| psk-qam | 15 dB — S5 (synthetic only) | order: +5 dB | 0.6 | 0.5 | off |
| analog | 10 dB — *unverified* (Azzouz–Nandi synthetic) | **+3 dB** — dev-measured (T-249) | 0.6 | 0.5 | off |
| css, ofdm, pulsed | 10 dB — *unverified* | +0 | 0.6 | 0.5 | off |
| dsss | 10 dB — *unverified* | — | 0.7 | 0.4 | off |
| noise-like | none (SK/flatness test) | — | 0.7 | — | off |

Below a gate, a family contributes no likelihood mass, so its mass moves to `unknown` with reason `low_snr`. `coarse` can still be set. Continuous signals integrate: the gate applies to the SNR of the analysed extent (S5: RDS rate is trusted at 0 dB over ≥ 0.25 s).

**The analog class gate was `wfm/nbfm/am +0; ssb/cw +5`, an a-priori guess, and the measurement refutes its shape as well as its values (T-249).** `FamilyThresholds` carries one class gate per family rather than per class, so the split was never implemented and the whole family ran at +0 — which is where every analog class error lived. Sweeping the **dev** seeds at 1 dB steps (5 classes × 24 seeds = 120 snippets per step), with the within-family call taken from the fitted class-conditional densities:

| in-band SNR, dB | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 | 20 |
|---|---|---|---|---|---|---|---|---|---|
| class correct | 0.508 | 0.667 | 0.733 | 0.883 | 0.983 | **1.000** | 1.000 | 1.000 | 1.000 |

13 dB is the lowest step at which the call is exact, and it stays exact at every step above — so the gate is **+3 dB over the 10 dB family gate**, uniform across the five classes. The guess was wrong about *which* classes are fragile: `ssb` and `cw` are named correctly at every SNR from 13 dB, and the class that needs the margin is **`am`**, whose envelope troughs go into the noise at 10 dB so that its carrier stops dominating its band and the snippet genuinely measures like a suppressed-carrier emission. Below the gate the class is withheld (reason `below_class_gate`) while the family call stands, per §2's rule that an unmeasurable quantity is absent rather than wrong.

## 3. Fusion with C17 priors

- **Input.** `FamilyPriorSet {prior_ref, lambda, dist: P(family ∣ f, ℓ), status, staleness}` from hk-context (T-212). It is derived from the allocation services at the emitter's measured extent via a service → expected-family table: `fm-broadcast → analog/wfm`, `adsb → pulsed/ppm`, `ais → fsk/gfsk`… Emission designators feed it where present. `P = λ₁P_alloc + λ₂P_license + λ₃P_history + λ₀P_uniform`, and validation refuses λ₀ < 0.1.
- **Computation** (`hk_model::classify::fuse`, pure):
  1. `u = open_set_score`. Unknown posterior = `max(u, likelihood[unknown])`. Priors never touch it.
  2. Known posterior ∝ `likelihood[c] · prior[c]`, renormalised to `1 − p_unknown`.
  3. **Evidence dominance.** Let `a` be the likelihood top-1 and `b` the runner-up. If `L[a]/L[b] ≥ 10` and the posterior top ≠ `a`, the prior factor is tempered, `prior^τ` with τ bisected in [0, 1], until `a` is top again. The row gets flag `prior-mismatch`.
  4. If the prior changed the top within that margin, set `prior-tiebreak`.
  5. If the prior's top ≠ the likelihood top and `L[a] ≥ 0.5`, set `prior-mismatch`. That flag feeds `unexpected-here` in explanations, and ADR-0012 §4.3 zeroes the C17 part of the boring prior.
- **Invariant tests (T-211):** the posterior with a uniform prior equals the likelihood; no prior can raise `p_unknown` or lower it; no prior can flip a ≥ 10:1 likelihood call; `lambda0 = 0` is refused.
- **No C17 data** (`status: no_reference_data`): `prior: None`, posterior = likelihood.

## 4. Classical cascade (C15, T-199/T-200)

1. **Input.** A `NormalisedSnippet` (hk-estimate, CFO-corrected, resampled) plus the C13 `ParameterSet`, C14 `SymbolParameters` and C16 result when present. The raw snippet is kept. Suspect detections (clipped, IMD, image) still classify, but they carry `suspect-input` and never mint signatures (§5).
2. **Features** (`hk-classify/src/features.rs`, one vector, `features_version` = `hk_classify::FEATURES_VERSION`; **`features@9` today** — 1 → 2 (T-248) and 2 → 3 (T-286) both redefined `symmetry`; 3 → 4/5/6 are recorded at the constant itself; 6 → 7 (T-431) referenced `duty` and `low_fraction` to the emission's on level; 7 → 8 (T-447) referenced the Azzouz–Nandi **strong-envelope subset** σ_ap, σ_dp and the de-rotation are measured over to the same on level; 8 → 9 (T-488) referenced the **instantaneous-frequency** subset, and with it all seven `if_*`/`sigma_af` dimensions, to that same on level — the third and last site of the rule. Read the constant's own doc comment for each step; this list goes stale and the constant cannot). Each feature is a value or an abstention reason:
   - Azzouz–Nandi γ_max, σ_ap, σ_dp, σ_aa, σ_af, P;
   - normalised cumulants C̃₂₀, C̃₄₀, C̃₄₂, and μ₄₂;
   - instantaneous-frequency histogram modality and levels;
   - C14 cyclic-line strengths at Rs, 2Rs, 2fc, 4fc, and the rate trust flag;
   - C13 flatness, symmetry, carrier line, OBW/Rs;
   - envelope duty and PRI regularity;
   - chirp IF-slope linearity;
   - C16 CP correlation;
   - spectral kurtosis;
   - the existing `blind::Family` label, as one feature.
3. **Tree.** Coarse split first (C13 `analog_digital`, IF histogram continuity, cyclic line). Then family nodes on constant-envelope vs varying envelope, IF levels, cumulant signatures, chirp slope, CP, duty/PRI, SK. Each leaf has a **class-conditional Gaussian** (diagonal + shrinkage) over the features present, fitted on the synthetic **dev** grid (§7) and shipped as versioned data (`hk-classify/data/densities@1.json`). `p(x∣c)` is the density over non-abstaining dimensions.
4. **Open set.** `open_set_score = 1 − max_c P(χ²_k ≥ d²_c)`, with d_c the Mahalanobis distance and k the dimensions used. Unknown also wins when no family passes its gate, or when the top family's likelihood share is < 0.5 with entropy > 0.9.
5. **Verifier (T-200).** Runs only when a clock is locked (C14 trusted rate, or a recipe clock-recovery lock). The candidates are the within-family classes with p ≥ 0.05 (for example `bpsk`/`qpsk`/`8psk`, or `2fsk`/`gfsk`). ALRT is used with known SNR, GLRT with unknown phase or CFO. It re-ranks, may sharpen or flatten the class distribution, and **never adds a candidate** absent from step 3. Stage `verifier`, rank 2.
6. **Per-family DL (T-204).** Within-family class only (it never chooses the family). The open set uses an **energy score** `E = −T·logsumexp(z/T)`, with the threshold at 95 % TPR on dev in-distribution data; softmax max is never used. Modes per family: `off` / `shadow` / `active`.
   - **Enable rule, a-priori.** On the dev evaluation, at every SNR bin ≥ the family gate:
     - class accuracy of DL − classical ≥ 5 points, with the bootstrap 95 % CI lower bound > 0;
     - open-set AUROC not lower by > 0.02;
     - false-known rate on held-out classes ≤ classical;
     - if ≥ 50 OTA decode-labelled examples exist for the family, OTA accuracy not lower.
   - The evidence file is referenced from the model manifest (§6). Without it, `active` is refused unless the request carries `force: true`, which is audited.
7. **Decoders arbitrate.** A CRC-valid decode writes a rank-1 row (existing `record_decoder_evidence`, now carrying the `hk-mod@1` class the recipe/decoder declares) and an Annotation `ground-truth`.

**Placement.** Per event on the CPU (ADR-0007): features cost µs–ms per snippet. Classification runs where `family::explain_emitter` already runs (track close, chain writers), through a single call site `hk-pipeline/src/classify.rs`, off the ring and DSP real-time threads. In low-power mode only the tree runs; the stage is reported.

*Amendment (T-878, 2026-09-24).* The call site's only caller used to be the fsk chain, after a successful framed write, so classification depended on a decode chain matching, attaching **and** succeeding; through the mock SDR a POCSAG and an ACARS scene (no chain matched), a WFM station (only the non-classifying FM chain attached), a LoRa burst (the fsk chain gave up under `min_bursts`) and a multipath scene (every row dropped at the rank-3 tie) got no row at all. The caller is now `hk-pipeline/src/chains/classify.rs`: a metadata-only measuring chain on the `every-track` trigger (the sweep characteriser's, T-297), attached beside whatever decode chain a confirmed track selected, or none. It classifies the track's longest member box once (capped at `window_s`; a continuous emission is classified as soon as it spans the window) and writes against the entry the inventory recorded for that track, never one found by frequency. At most `max_chains` run at once, counted apart from the decode chains (`classify_attached`). The **rank-3 tie** with an unlocked demodulator-chain label no longer drops the row: it is recorded, and the chain's own label is restated after it so "latest among equals" still gives the chain the family (§2's ladder is unchanged).

## 5. C18 schema: EmissionFeatures, Signature, SignatureMatch, clusters

**`EmissionFeatures`** (`hk_model::signature`, version 1) is aggregated per emitter, append-only snapshot rows. A row is written when a field changes by > σ or ≥ 16 new observations have been folded in. Each field is `Feat<T> {value, sigma, method, n}` or absent:
- band `f_lo/f_hi`, centre, raster offset, OBW, family/class (top from the current Classification);
- symbol rate (+ harmonic alternatives), deviation, levels, constellation order, roll-off / BT;
- line code, preamble, sync word (bits, polarity, rotation set), frame-length histogram, CRC parameters (C21 / assist);
- period, duty cycle, burst length, TDMA period, hop raster, hop set (C10);
- spectral shape: flatness, symmetry, carrier line, comb spacing and count (RFI combs, AWARE-029/030);
- optional `pri_s` / `scan_period_s` (radar) and `cfo_offset_hz` (oscillator, AWARE-051, later);
- `suspect_fraction`, `snr_db` distribution.

`Fingerprint` v1 is a projection (`EmissionFeatures::fingerprint()`). Entity resolution is unchanged (§5.1's `ModulationStructure` was added by T-233 and removed by T-594).

**`Signature`** is immutable per `(signature_id, version)`, like recipes:
- `name`, `kind` (`protocol` / `device-type` / `rfi` / `radar` / `learned`), `taxonomy`;
- `fields: map<field, {expect: value | range | set | bits, tolerance, required, weight}>`. The default symbol-rate tolerance is ±1 %. Sync words match with ≤ k bit errors in both polarities and every PSK rotation;
- `min_discriminating` (default 3 required fields present, e.g. rate + deviation + sync);
- `recipe: {id, version}?`, `content_class` tag, `provenance` (`user` / `recipe-confirmed` / `rtl433-import` / `cluster-promoted`), `author`, `created`, `supersedes`;
- optional `bands[]`. These are **rank-only**: a band never gates a match.
- **Store:** SQLite. Built-ins are seeded read-only from `signatures/*.signature.json`. A `recipe-confirmed` signature is minted when a recipe decode is CRC-valid on ≥ 3 frames from ≥ 2 bursts of one emitter: the recipe `match` plus measured features ± 3σ. Signatures are never minted from all-suspect clusters. Imports are untrusted and validated.

**`SignatureMatch`** is an append-only row per emitter when the outcome or top-1 changes:
- `outcome`, `features_ref`, `signatures_rev`, `reasons`;
- `candidates[] {signature_id, version, name, score 0..1, agreement[] {field, measured, expected, z, ok}, missing[], conflicting[], recipe?}`, top 5.

Pipeline (`hk-context/src/signature/matcher.rs`):
1. Gate on family compatibility (`family_of`; unknown gates nothing).
2. Per-field z = |Δ|/tolerance, with the sync-word distance in bits.
3. `score = Σ w·exp(−z²/2) / Σ w_required`. Missing required fields count 0.
4. Outcome:
   - `full`: every required field present with z ≤ 1, ≥ `min_discriminating`, score ≥ 0.8;
   - `partial`: no required field conflicting (z > 3), but some missing, or score in [0.4, 0.8);
   - `none`: otherwise.

A match **never sets identity, known_status or lifecycle**. It adds explanation evidence of kind `signature` (ranked like `family`), feeds `decoder_available` in ADR-0012 when the signature has a recipe, and seeds MAUTO (§8).

**Clusters of unknowns** (`hk-context/src/signature/cluster.rs`, T-202):
- **Online assignment.** An emitter whose `SignatureMatch` is not `full` is assigned to the nearest active cluster centroid when the tolerance-normalised distance (the same z metric, RMS over shared fields, ≥ 3 shared fields) is ≤ 1. Otherwise it seeds a `pending` cluster.
- **Visibility.** A cluster becomes `active` (visible, API) at ≥ 3 member emitters, or ≥ 3 appearances of one emitter across ≥ 2 sessions. Centroids use a running mean with weight cap 16, as `Fingerprint` does.
- **Repair.** Nightly (or on demand), batch DBSCAN (ε = 1, minPts = 3) over current features re-derives clusters. Differences become append-only `cluster_event` rows (`merge` / `split` / `reassign`); ids of the larger side survive.
- **Promotion.** `POST /api/clusters/{id}/promote` (user) or a CRC-valid recipe decode on a member → a `Signature` (`cluster-promoted` or `recipe-confirmed`) linked back.
- **Identity link.** A cluster groups **emitters** (instances). An emitter has at most one current cluster (`emitter_cluster`, append-only with supersession). Type ≠ instance: two identical sensors share a cluster and stay two emitters (C18 pitfall).

**Privacy.** Clusters and signatures stay local. No instance-level RF fingerprinting is done in M3 (AWARE-047 is later).

### 5.1 The fingerprint's family gate stays exact, and what the fingerprint carries instead (T-233, amendment)

§1 proposed relaxing `Fingerprint`'s family comparison from exact equality to `family_of`, so that an emitter labelled `fsk` by the blind estimator and `2fsk` by the demodulator chain would stop splitting into two inventory rows. T-218 applied it and found it also merges `bpsk` with `qpsk`, `2fsk` with `gfsk` and `am` with `wfm` at an identical centre, bandwidth and symbol rate, and deferred it. T-233 was funded to build the finer discriminator that would make it safe. **It built one, measured it, and the relaxation is withdrawn.** Three findings, in the order they decide the question.

**1. The relaxation is worth less than it looks, because its motivating case is already handled.** Two entries of one emission — a track-based one and a chain or decoder output — are merged by `Repository::same_emission` (T-082), which compares centre frequency and observation overlap and **never looks at the family at all**. The `fsk`/`2fsk` split the relaxation exists to close does not survive that path.

**2. The relaxation is worth more than it looks in the other direction, because only one coarse label is ever written.** `hk_estimate::blind::Family` emits `Fsk` — a family — but `Bpsk`, `Qpsk` and `Ook`, which are classes. So `fsk` is the *only* family-level modulation label any producer writes, and relaxing the gate is precisely a licence to merge an `fsk` row with a `2fsk`, `gfsk`, `msk` or `4fsk` one. The `fsk` family is where the relaxation's entire risk lives.

**3. That family is exactly where no discriminator reaches.** `2fsk`, `gfsk` and `msk` are not three modulations but one modulation at three filter settings, and every one of them is **constant-envelope by construction**. Conditioned as the fingerprint conditions — matched centre, bandwidth, symbol rate and modulation index — over 99 % of genuinely distinct pairs are indistinguishable on the statistic that shipped, at every SNR from the FSK gate upwards. The best statistic T-233 tried on that pair (the instantaneous frequency's Sarle bimodality) still left 5.8 % to 33 % indistinguishable, and was rejected anyway for measuring the observation rather than the emission. Blind tests must never merge two genuinely distinct emitters, and there is no width at which this pair can be gated that honours that.

**What landed instead: `ModulationStructure` on `Fingerprint`, as evidence that can only split — since removed (T-594, below).**

- **The statistic.** `envelope_shape` = `μ₄ = E|s|⁴/(E|s|²)²` of the emission, with the additive-noise contribution removed in closed form (`μ₄ₛ = μ₄ₓ(1+1/ρ)² − 4/ρ − 2/ρ²`, from `E|x|⁴ = E|s|⁴ + 4Pσ² + 2σ⁴`). It is exactly 1 for any constant-envelope emission, and it separates an amplitude modulation from an angle one, and a binary phase alphabet from a quaternary one: a shaped BPSK's 180° transitions carry the trajectory through the origin where a QPSK's mostly do not. It is a ratio of two expectations, so it measures the emission and not the observation — the T-281 rule.
- **Its uncertainty is measured, not assumed.** Each value carries a sigma: the noise correction's own error given an SNR known to ±3 dB, plus the statistic's spread across four blocks of the record, in quadrature. `Tolerances::structure_sigmas` is then a number of sigmas (3), not a width, so nothing is fitted to any pair. The dimension stops discriminating on its own as the SNR falls rather than needing an SNR floor bolted on.
- **Measured** (acceptance seeds, `hk-classify`'s `structure_rates`; bandwidth-matched pairs at an identical centre, bandwidth, symbol rate and family label): `bpsk`/`qpsk` false merge **0.097 at 20 dB** and **0.027 at 25 dB** against a baseline of 1.00, false split under 0.02; at the 15 dB PSK gate the band exceeds the 0.18 separation and the dimension concludes nothing.
- **It can only split.** It is an ordinary scored feature, so a fingerprint carrying structure is compared on strictly more evidence; a pair that matched without it can stop matching and never start. Adding it to entity resolution therefore adds **no** false-merge exposure at all, which is what made it safe to land while the relaxation is not.
- **A second dimension was built and rejected.** `if_concentration` (`IQR/(P95−P5)` of the instantaneous frequency) separates `bpsk` from `qpsk` even at 10 dB, but its quantiles count samples, so it moves with the analysis geometry — one `bpsk` emitter read 0.014 over a 16 384-sample record and 0.078 over a 6 144-sample one, while reporting a confident sigma. That is the T-281/T-310 failure mode, and one dimension that measures the emission beats two where the second measures the look.

**T-594 removed it.** Nothing in `hk-pipeline` ever called `hk_classify::modulation_structure`, so the field was absent in every real run: replaying the real FM and 433 MHz captures blind through the mock SDR stored 18 fingerprints, **0** carrying `structure`, so it took part in **0** comparisons (`hk-pipeline/tests/fingerprint_field_census.rs`). It was removed rather than wired: every producer that holds IQ also writes an exact `family`, and every family those producers write is constant-envelope (analogue FM, the FSK chain, the four-level trunking control channel), where the statistic reads 1.00 by construction. Wired, it could only have split on what moves a constant envelope — multipath fading and the noise correction's error — which measure the look, not the emitter; and the `bpsk`/`qpsk` pair it was measured on has no producer that writes fingerprints. A dead comparison kept "in case" implies a discrimination that is not happening. The statistic stays in `hk_classify::structure` as a measurement with no inventory consumer; `structure_rates` and `structure-probe`, which measured the removed comparison, went with it. Stored fingerprints carrying the key still parse. The same census found the wider state of entity resolution on those runs: only centre, bandwidth, duty cycle and burst length were ever present on both sides of a pair; `family` was on 1 of 18 fingerprints, and symbol rate, deviation, period and the hop fields on none.

T-218's guard test (`t218_the_family_gate_separates_two_emissions_that_share_a_family`) is unchanged and still passes, which is the point: the discriminator did not earn the relaxation, so nothing about the gate moved.

## 6. C38 ml-runtime interface (`hk-ml`, T-203)

```rust
pub trait MlProvider: Send + Sync {
    fn kind(&self) -> MlProviderKind;                       // cpu-tract | ort-cpu | ort-coreml | ort-trt (stub)
    fn conformant(&self) -> bool;                           // ADR-0007 rule: unused unless marked
    fn load(&self, m: &ModelManifest, bytes: &[u8]) -> Result<Box<dyn LoadedModel>, MlError>;
}
pub trait LoadedModel: Send + Sync { fn infer(&self, batch: &TensorBatch) -> Result<Vec<RawOutput>, MlError>; }
pub struct InferenceRequest { model: ModelRef, consumer: ConsumerId, subject: SubjectRef, input: Tensor, deadline: Instant }
pub struct Prediction {
    pub model: ModelRef,                 // id@version#sha8
    pub provider: MlProviderKind, pub precision: Precision,
    pub labels: Arc<[String]>, pub logits: Vec<f32>, pub probs: Vec<f32>, // probs temperature-calibrated
    pub energy: f32, pub unknown_score: f32,                              // energy-based, calibrated 0..1
    pub embedding: Option<Vec<f32>>,
    pub mode: MlMode,                    // shadow | active
    pub latency_ms: f32, pub batch_size: u16, pub t: Timestamp,
}
```

- **Registry.** `<data dir>/models/<id>/<version>/{model.onnx, manifest.json}`, plus read-only built-ins. The manifest contains:
  - `id`, semver `version` (**immutable once saved**), `sha256`, `task` (`family-class` / `embedding` / `detector` / `anomaly`), `consumer`, `taxonomy`;
  - input spec (`iq-2xN`, N 1024 / 4096, canonical sps, normalisation);
  - `labels`, `open_set {method: energy, T, threshold, calibrated_on}`, `precision`;
  - `training {generator@version, seeds, ota_sessions, dataset_ids}`, `metrics_ref` (per-SNR report), `enable_evidence` (§4.6);
  - ONNX **op allowlist**: Conv, BatchNorm (folded), Relu/Gelu, pooling, Gemm/MatMul, LayerNorm, Add/Mul, Reshape/Flatten.
  - Models are data: loading, swapping or rolling back needs no rebuild (ADR-0001's hard requirement). Rollback = set the mode of the previous version.
- **Batching.** One worker thread per loaded model, never a ring/DSP thread. It flushes at batch 32, after 20 ms, or at the earliest deadline (C38 card defaults, *estimates*). The queue is bounded: when full, requests are dropped and counted, and the consumer falls back to classical (the stage is reported). Inference runs only on CFAR-surviving, classified-by-tree events. `max_in_flight` bounds GPU contention. In low-power mode, active and shadow are both off.
- **Shadow mode.** Per `(model, consumer)`: `off` / `shadow` / `active`. Shadow runs the model and appends `{Prediction, classical decision, snr_bin, subject}` to hk-store `ml/shadow/` hourly CRC-line NDJSON (256 MiB, 30 days), with per-SNR agreement aggregates. **Shadow never writes a Classification row or changes any decision.** T-203 unit-tests this and T-206 asserts it.
  - **Landed vs. owed (T-365, 2026-09-22).** T-203 landed the `ShadowSink` trait, the `off`/`shadow`/`active` mode machinery and `MemoryShadowSink`, which is **in-memory** and is the only implementation in the tree. The hk-store `ml/shadow/` NDJSON writer described above does not exist and is **T-844's**, with the `/api/ml` routes that read it (§9, §10 amendment). Until it does, `ModelHost::observe` has no durable consumer, which is the third of the three reasons `hk-ml`'s crate docs give for the host being deliberately unwired.
  - **T-844 (2026-09-25).** Built: `hk_store::ml` (the NDJSON writer above, keyed on the subject's **capture** time), `StoreShadowSink` + `hk_pipeline::ml::MlStage` (one host per provider, modes persisted in `<data dir>/ml/modes.json`), the producer at the classifier's single call site — since T-878 the `every-track` classify chain (`hk-pipeline/src/chains/classify.rs`), after the published row is written and outside the repository lock — and the three `/api/ml` routes. It was held from 2026-09-23 because the producer was vacuous through the device: `hk_ml::gate::admit` correctly refuses a subject whose classical family is `unknown`, and every scene the mock SDR fed the call site then came out `unknown`. T-852, T-876, T-887 and T-888 fixed the device-path causes, and `hk-pipeline/tests/ml_shadow.rs` (unchanged in its assertions) now sees shadow records reach hk-store from a real run. No family is put in `shadow` by default: a model enters it only when an operator installs it and sets the mode, and `active` still needs the §4.6 evidence or an audited `force`.
- **Provider choice (Mac-first).**
  - **CPU reference = `tract-onnx`.** Pure Rust, loads ONNX at runtime, no native library in the default build or CI (*maintenance and op coverage unverified; checked in T-203*).
  - **Mac acceleration = `ort`** (ONNX Runtime bindings) with the CoreML execution provider, behind the opt-in feature `ml-coreml` (*unverified: CoreML EP op coverage for 1-D conv and dynamic batch; ort's build-time binary download*).
  - **Jetson = `ort` + TensorRT EP** (`ml-trt`, stub, deferred with T-026/T-216). The same crate and ONNX source serve both, and engines are never the source of truth.
  - **Rejected:**
    - Candle (Metal): model graphs are Rust code, so a rebuild per model, and ONNX import is partial;
    - Burn: `burn-import` generates code at build time;
    - hand-written wgpu kernels: writing an inference engine. It stays a possible later provider behind the same suite.
  - **Bake-off rule (T-203, day 1).** On the reference model, if ort+CoreML p99 at batch 32 is not ≥ 2× better than tract, ship CPU-only on the Mac and leave `ml-coreml` off. Small CNNs per event may not need a GPU (*unverified*).
- **Conformance suite** (`hk-ml/tests/conformance.rs`). A tiny fixed ONNX model (KB-sized, generated by `py/hkpy/ml/make_conformance_model.py`) plus fixed tensors. Against the CPU reference:
  - FP32 max |Δlogit| ≤ 1e-3;
  - FP16/INT8 top-1 agreement ≥ 99 % and |Δprob| ≤ 0.02 (C38 card, *estimate*);
  - batch invariance (32 singles = one batch of 32);
  - typed shape/op errors;
  - deadline-miss accounting;
  - a manifest sha mismatch refuses the load.

## 7. Blind evaluation protocol

- **Data.**
  - **Synthetic dev.** `py/hkpy/synth` extended with an AMC grid (T-213): every class in §1 × SNR −10…+30 dB (2.5 dB steps from gate − 10 dB to gate + 10 dB, 5 dB elsewhere) × HackRF impairments already modelled (8-bit quantisation, LO ppm, phase noise, IQ imbalance, DC, spurs, blocker/IMD, ADC gain/clipping) plus random symbol rate, roll-off/BT and burst length. **Seeds 0–9999**, 20 trials per cell. Used for densities, thresholds, DL training, and DL enable evidence.
  - **Synthetic acceptance.** Scenes generated at test time from **seeds ≥ 1 000 000**, with randomised composition and a hidden truth file, run through the mock SDR (the M2 scene pattern). Never used for fitting.
  - **Held-out unknowns.** Generators not in the dev grid: 3-level ASK, FSK with a chirped carrier, Costas-hopped tones, 8-level FSK, OFDM with a non-standard CP (dev excludes it), random-phase noise bursts. Plus real negatives: S5 noise snippets and the empty 433 MHz capture.
  - **OTA.** `fixtures/hackrf/2026-09-13` (`fm_100p8M` analog/wfm + RDS, `ism_915M` FSK, `ism_433p62M` negatives, `urban_98M` overload) and the M1 tutorial captures (POCSAG, ACARS, ADS-B). **Labels come only from CRC-valid decodes or user labels** (T-205), split by capture session and date, never by frame.
- **Reporting** (`hk_model::classify::eval::EvalReport`, JSON + Markdown table): for each source (synthetic-dev / synthetic-acceptance / OTA) × stage × SNR bin (measured in-band SNR per C13, plus true SNR for synthetic):
  - top-1 and top-2 family accuracy, class accuracy, abstention rate;
  - wrong-label rate (non-unknown and wrong);
  - confusion matrix, macro-F1;
  - open-set AUROC and false-known rate at the operating point;
  - ECE;
  - latency p50/p99.
  A single SNR-averaged number is never reported alone.
- **A-priori thresholds.** Gates (§2), margins (§4.6) and exit floors (below) are fixed here as `thresholds@1`. Changing one needs an ADR amendment citing **dev** evidence. Tuning against acceptance-scene failures is not allowed (the blind-test rule).
- **M3 exit gate (T-206)**, all through the mock SDR. Floors are a-priori and *unverified*:

  > **Amendment (supervisor ruling, 2026-09-16).** The held-out row previously stated `recall ≥ 0.80` **and** `false-known ≤ 0.10` as if they were independent floors. They are not: the gate computes `false_known = 1 − recall`, so `false-known ≤ 0.10` *is* `recall ≥ 0.90`, and the two clauses were one floor transcribed twice at inconsistent values. Resolved by keeping the **stricter, original** value and stating it once. **This is not a threshold change and not an amendment citing new dev evidence** — it removes a duplicate. It makes M3s exit *harder*, not easier: measured 2026-09-16 the full held-out grid gives recall 0.8510 / false-known 0.1490, so the gate stays **red** under the surviving floor. Per the ruling, the fix is the underlying capability (T-286), never the threshold. The looser 0.80 was not adopted despite the §7-population reading having improved from clearing by 0.017 to clearing by 0.09: "the number improved" is not grounds to revisit a ruling.

| Check | Floor |
|---|---|
| Known families, synthetic acceptance, SNR ≥ gate + 5 dB | top-1 ≥ 0.90, top-2 ≥ 0.95 |
| Wrong-label rate, any bin (abstaining is allowed) | ≤ 0.05 per bin, ≤ 0.02 overall |
| OTA: 915 MHz FSK ≥ 20 dB; FM broadcast (analog/wfm); POCSAG (fsk); ADS-B (pulsed) | family correct ≥ 0.95 of labelled emitters; RDS subcarrier stays abstaining below the gate |
| Held-out unknowns | **false-known ≤ 0.10** — equivalently `unknown` (or open_set ≥ 0.5) **≥ 0.90**, because the gate computes `false_known = 1 − recall`, so these are one floor, not two; noise snippets labelled as a comm family ≤ 0.01 |
| Prior mismatch scene (FSK carrier in the FM allocation, off-raster WFM) | posterior top = likelihood top whenever LR ≥ 10 (100 %); `prior-mismatch` set |
| Signatures, synthetic population incl. the P25/DMR near-collision, ≥ 20 dB | full-match precision ≥ 0.95, recall ≥ 0.80; below `min_discriminating` → `partial` 100 %; never an identity |
| Clustering, multi-day scene with repeated unknowns | ARI ≥ 0.8; ≤ 1.5 clusters per truth type; merge rate ≤ 0.05; identical after restart |
| ML | shadow changes no Classification row; each `active` family has enable evidence; zero ring sample drops with ML on |
| Regression | M0/M1/M2 acceptance unchanged |

**Where each row is asserted.** A gate row nothing checks is satisfied by nothing checking it —
T-206's two vacuous dimensions, found and paid for once. The **ML row** was in that state until
T-366: `tests/e2e/tests/acceptance/m3_*.rs` held no ML assertion at all. It is now
`hk_ml::exit_gate` — the three clauses as a pure predicate over observables (the `(model, consumer)`
modes in force, the `Classification` rows a run persisted, the run's lost-sample count) — asserted
over a real run through the mock SDR by `tests/e2e/tests/acceptance/m3_ml.rs`, with the shape of the
rule being *every ML-attributed `Classification` row must name a model that is `active` with §4.6
enable evidence behind it*.

The honest part: with the host dormant (T-363) the modes table is empty, so the row holds today
**because nothing is on**, and that is what the gate prints (`[T-206] ML: off (clause 3 not
exercised)`) rather than claiming ML was exercised. What makes the assertion worth having is that
it **fails the day ML becomes active without its evidence** — and that is demonstrated, not argued:
`m3_ml_exit_gate_catches_the_states_the_adr_row_forbids` constructs each forbidden state on the
run's own snapshot and shows the gate reporting it, and
`hk-ml/src/host.rs::a_forced_active_model_without_evidence_fails_the_adr_0016_s7_exit_gate` does the
same through a live host on the one path that can reach `active` without evidence (a forced
`set_mode`, which §4.6 audits rather than refuses). The empty-modes premise is itself guarded:
`m3_ml_the_dormant_premise_of_this_gate_is_still_guarded` fails if `hk-ml`'s no-production-caller
tripwire is removed, so whoever wires the host must give the gate a real mode enumeration
(`MlGateSnapshot::from_host`) in the same change.

### 7.1 Canonical baseline, and how a brief cites it (T-415)

The floors above are fixed a priori. The **numbers a brief quotes as a do-not-regress
baseline are not** — they are the latest measurement, and measurements move as real
capability work lands (T-311 below is a legitimate example, not a bug). Two coordinator
briefing errors (T-415) came from treating a measurement as if it were pinned like a floor:
a stale figure got repeated across briefs after the true number had already moved, twice.
The fix is not to freeze a number here forever; it is to give every brief exactly **one**
re-derivable source to quote instead of a remembered digit.

**Command:** `just acceptance-m3` (`HK_E2E_REQUIRE_SYNTH=1 cargo test -p hk-e2e --test
acceptance_m3 -- --nocapture`), reading the `[T-206]` lines it prints.

**Determinism, with the run count that backs it (T-428).** Every figure below is a pure
function of the code and the fixed seeds: no RNG is drawn without a seed, and the
classification path takes no `hk-dsp` compute provider, so no thread count or machine load
reaches it. **Measured, not assumed:** 9 runs of `m3_unknown_recall_and_false_known_rate`
on commit `651335f` (5 filtered, 4 through the whole binary) returned the identical
per-generator breakdown and the identical aggregate, and the T-213 report's own figures were
byte-identical across the 4 full runs. Separately, the command was run once on each of 7
commits spanning 2026-09-17 (`41f6aa1`, `ca40832`, `d7ccd59`, `522f94c`, `9e6a6ae`,
`5a717f1`, `651335f`) and returned 0.9520 / 0.0480 at every one — **`main` did not move**.
A figure in the table below that does not reproduce is therefore evidence of a code change
or of a *different figure being read*, never of run-to-run noise.

**Each figure names one population** (`hk_classify::harness::Summary`, `crates/hk-classify/src/harness.rs`);
quoting one without its population is the failure mode this section exists to close:

| Figure | Population | Measured (commit `651335f`, 2026-09-17, n runs) |
|---|---|---|
| Known top-1 / top-2 | `synthetic-acceptance` source, all 8 taxonomy families, SNR bins ≥ gate+5 dB (the two highest of five bins) | 0.9345 / 0.9861 (4 runs, identical) |
| Wrong-label, overall | `synthetic-acceptance`, every bin including below-gate | 0.0040 (4 runs, identical) |
| Wrong-label, worst bin | `synthetic-acceptance`, bins ≥ gate, n ≥ 10 | 0.0333 (4 runs, identical) |
| Unknown recall / false-known | **the gate's draw: 396 held-out snippets, all 11 generators, seeds `ACCEPTANCE_SEED_BASE + 700_001…`** — `m3_grid.rs::held_out`, asserted by `m3_unknown_recall_and_false_known_rate`. Read **this** row; two other held-out readings are printed by the same command and neither is the gate (see §7.2) | 0.9520 / 0.0480 = 377/396 (9 runs, identical) — but see the sampling spread in §7.2 before quoting 4 dp |

**T-404's 0.9480 top-1 was neither of those failure modes — a third one, now closed by
this record.** It is the same metric over the same population as the row above (no
population split exists for top-1, unlike unknown-recall/false-known), so it was not two
numbers sharing a name. It also was not a transcription slip: T-404's commit message
accurately reported what it had locally measured. What broke is narrower: checking out the
exact commit that merged (`3d37ccf`) and running the named command today reproduces
**0.9345**, not 0.9480, on code that is byte-identical to T-404's own branch tip
(`b3fac0e`) for every file in `hk-classify`/`hk-estimate` (`git diff b3fac0e 3d37ccf --
crates/hk-classify crates/hk-estimate` is empty) — so the merged code never scored 0.9480;
T-404's own pre-commit measurement predated the last edit folded into that same squashed
commit and was never re-run after it. **The number was real, but it described a commit
that was never on `main`.** A brief that had re-derived via the command above at merge
time, rather than trusting the commit message's figure, would have read 0.9345 from the
start.

**The method (T-419's, generalised): stash-and-measure, not assume.** An agent that meets
a mismatch between a briefed baseline and what it measures does not guess which side is
wrong. It stashes its entire diff, checks out the merge base with current `main` (or `main`
itself if there is no in-flight diff), runs the named command, and compares. That
determines — rather than assumes — whether a discrepancy predates the agent's own work.
Only then does it report which failure mode applies: a stale number
that has since legitimately moved (re-cite the fresh figure), a population mismatch
(re-cite the correct row), or a genuine regression introduced by work in flight (fix or
flag it, per the task's own scope — this ADR section is not where thresholds get relaxed
to match a bad run). **It has one blind spot, added by T-428 (§7.2): the protocol
establishes which *tree* was measured, and cannot catch reading the wrong *line* of the
same output. Before concluding anything from a mismatch, check §7.2's table that the two
figures came from the same printed reading — and if the gap is under ±0.02 on the
open-set figures, it is inside the draw's own sampling spread and is not a finding at
all.**

**Moved by T-431 (2026-09-17), measured both sides.** `features@7` references `duty` and
`low_fraction` to the emission's own on level instead of the record's mean envelope, which
changes every class's value on two of the thirty dimensions and required a refit of both
density files. Re-derived with the command above, on the merge base and on the branch, on
one machine back to back:

| Figure | merge base | T-431 | move |
|---|---|---|---|
| Known top-1 / top-2 | 0.9345 / 0.9861 | 0.9325 / 0.9861 | −0.0020 / 0 |
| Wrong-label, overall | 0.0040 | 0.0032 | −0.0008 (better) |
| Wrong-label, worst bin | 0.0333 | 0.0333 | 0 |
| Unknown recall / false-known (gate's draw) | 0.9520 / 0.0480 = 377/396 | 0.9444 / 0.0556 = 374/396 | −0.0076, **under the 0.0084 draw sd of §7.2** |

Every floor is met. The whole of the unknown-recall move is `psk-qam`'s open set
(0.875 → 0.819 over its 72 held-out `apsk16`/`pi4-dqpsk` snippets); every other family's
open set is unchanged to three decimals. Against it, the defect the ticket existed to fix:
`pulse` at the `pulsed` gate goes from top-1 **0.00 / unknown 1.00** to top-1 **0.92**, and
the `pulsed` family at that rung from 0.50 to 0.96, because a 5 %-duty radar train is no
longer denied its own family for being always on. Quote **these** figures as the baseline
from here; the rows above remain the reading at `651335f`. One coincidence to not be
caught by: 0.9444 is also what the merge base's *harness* draw printed (§7.2's second
reading). Both columns above are the gate's line, at the two commits.

**Not moved by T-447 (2026-09-17), and that is the result.** `features@8` takes the
Azzouz–Nandi **strong-envelope subset** — the samples `phase_features` and `derotate`
measure over — against the same on level, closing the third and last site of the rule
T-431 fixed. T-431's own note predicted this would move σ_ap, σ_dp *and every cumulant*.
Measured on the merge base and on the branch, back to back on one machine, with both
density files refitted (and the refit verified deterministic: re-running `fit-densities`
on the unmodified tree reproduces the checked-in files **byte for byte**, so the whole
density diff is attributable to the feature):

| Figure | merge base (T-431) | T-447 | move |
|---|---|---|---|
| **Unknown recall / false-known (the gate's draw)** | 0.9444 / 0.0556 = 374/396 | 0.9444 / 0.0556 = 374/396 | **0.0000** |
| Known top-1 / top-2 | 0.9325 / 0.9861 | 0.9325 / 0.9861 | 0 / 0 |
| Wrong-label overall / worst bin | 0.0032 / 0.0333 | 0.0032 / 0.0333 | 0 / 0 |
| Per-family, per-SNR-bin top-1 (40 rows) | — | — | every row identical |
| Held-out abstention per generator (11 rows) | — | — | every row identical |
| Unknown recall, **harness** draw (§7.2's second reading, *not* the gate) | 0.934 | 0.937 | +0.003 |
| └ of which `psk-qam`'s open set, 72 snippets | 0.819 | 0.833 | +0.014 = one snippet |

So **one snippet of 396 changed outcome, in a draw that is not the gate**, against a draw
sd of 0.0084: nothing here is a result in either direction, and the gate's own line did
not move at all. The largest move in the refitted densities is `pulse`'s `sigma_ap` mean at
**0.05 σ**; no dimension of the 598 moves more than 0.1 σ.

**Why the predicted blast radius did not materialise, measured rather than assumed.** The
premise — a 5 %-duty train's "strong envelope" holds 0.662 of the record at 10 dB instead
of 0.050 — reproduces exactly, and is a statement about the **count** of that subset.
Neither consumer is count-weighted in the way that count suggests:

- `derotate` sums *phasors*, so each pair enters weighted by its own magnitude. Over the
  same `pulse` snippets the 90 % of the subset that was noise carried **11 %** of Σ|z|
  (13 959 → 12 350), the coherence it feeds went 0.881 → 0.995 against a
  `DEROTATE_MIN_COHERENCE` of **0.30** — so the de-rotate/don't decision was never in
  question at either reading — and the removed ramp moved by 4 × 10⁻⁴ rad/sample. That is
  why the cumulants, which depend on that decision and that ramp, do not move.
- σ_ap and σ_dp are dominated by the **other** defect in the same two features (T-240): the
  phase is unwrapped cumulatively, so the noise's walk through every off gap is already
  inside the value at each on sample before any subset is taken. With the corrected subset
  `pulse` still reads σ_dp 41–87 rad at 10 dB across six seeds.

The change is kept because it is correct and free — one rule, stated once, and a subset
that no longer silently degrades as the SNR falls — not because it bought a number. The
honest headline is that **"0.63 of the record instead of 0.05" was a property of the
subset's size and not of anything computed from it**, which is the same class of error as
§7.2's and T-480's: evidence that is sound about an adjacent quantity.

**Moved by T-488 (2026-09-18), and this is the site where the count *was* the statistic.**
`features@9` takes the **instantaneous-frequency** subset against the same on level,
closing the third and last site of T-431's rule — seven dimensions at once (`sigma_af`,
`if_std_norm`, `if_bimodality`, `if_modality`, `if_slope_r2`, `if_local_bimodality`,
`if_local_modality`, all statistics of one `fi` vector). Measured on the merge base and on
the branch back to back on one machine, both density files refitted, and the refit
re-verified deterministic on the unmodified tree first (re-running `fit-densities`
reproduces the checked-in files **byte for byte**, so the whole density diff is
attributable to the feature):

| Figure | merge base (T-447) | T-488 | move |
|---|---|---|---|
| **Unknown recall / false-known (the gate's draw)** | 0.9444 / 0.0556 = 374/396 | 0.9621 / 0.0379 = 381/396 | **+0.0177 = 2.1 draw sd — but inside §7.2's ±0.02 band, so NOT a finding** |
| Known top-1 / top-2 | 0.9325 / 0.9861 | 0.9306 / 0.9861 | −0.0019 (3 snippets of 1 656) / 0 |
| Wrong-label overall / worst bin | 0.0032 / 0.0333 | 0.0024 / 0.0333 | −0.0008 (better) / 0 |
| Per-family, per-SNR-bin top-1 (40 rows) | — | — | 9 rows move, all by 1–4 snippets, in both directions; `pulsed` at its gate 0.96 → **1.00**, `psk-qam` at gate+0 0.97 → 0.90 (its 4 lost snippets go to `unknown`, not to a wrong label — the bin's wrong-label rate is unchanged) |
| Unknown recall, **harness** draw (§7.2's second reading, *not* the gate) | 0.937 | 0.947 | +0.010 ≈ 1.2 draw sd |
| └ `psk-qam` open set, 72 snippets | 0.833 | 0.861 | +2 snippets |
| └ `analog` open set, 72 snippets | 0.847 | 0.875 | +2 snippets |

Every floor is met. **Every figure above is inside the draw's own spread, so the honest
verdict is "no regression", not "an improvement".** The gate's +0.0177 is the largest move
any of the three fixes has produced and it is still under the ±0.02 that §7.2 fixes as the
threshold for a signal; settling it would take the gate's draw re-run at several seed
bases, as §7.2 did, not a fourth decimal place on one.

**What *is* a result is the feature, and it is the opposite of T-447's.** T-447's premise
was sound and its blast radius nil because `derotate` sums phasors and weights each pair by
its own magnitude. Every dimension here is an **unweighted** statistic of the selected
pairs, so the subset's count is exactly what they average over, and the defect was total: a
5 %-duty `pulse` read `sigma_af` **1.7181 rad/sample at 10 dB against π/√3 = 1.8138**, the
standard deviation of a variate uniform on (−π, π] — the emission's "frequency excursion"
was the phase of pure noise, to within 5 % of the closed form for pure noise, on a
dimension named as an excursion. It now reads 0.1001 / 0.0565 / 0.0324 / 0.0190 / 0.0120
across the 10–30 dB ladder, falling by 1.77 / 1.74 / 1.71 / 1.58 per 5 dB rung against the
noise-limited law's 10^(5/20) = 1.778 — the *right* law, because a rectangular pulse train
has no excursion of its own. `pulse`'s `if_bimodality` went from 0.498 / 0.472 / 0.211 /
0.161 / 0.412 — a shape statistic with no monotonicity at all, describing the noise's
distribution at 10 dB and the emission's at 30 — to 0.327 / 0.329 / 0.332 / 0.329 / 0.338.

**The density diff is attributable dimension by dimension.** Of the 598 fitted dimensions
of the above-gate model, the **146 instantaneous-frequency dimensions moved and the other
452 moved by exactly 0.0000 σ** — including every cumulant, which independently confirms
the de-rotation decision is untouched (`derotate` does not read `fi`). Largest moves:
`pulse`'s `sigma_af` 1.3073 → 0.0519 (3.0 σ) and its `if_std_norm` 0.2640 → 0.0106 (2.8 σ),
against T-447's largest of 0.05 σ anywhere. One dimension leaves the below-gate model:
`pulse`'s `if_slope_r2` now abstains, because a 5 %-duty train honestly has too few on–on
pairs to fill the ramp windows.

**`sigma_af`'s `SNR_ORDER_EXCEPTIONS` entry survives, measured rather than assumed.** The
subset fix removes the off-gap noise from every *keyed* class (`cw` 0.206 → 0.148 at 10 dB,
`pulse` 1.718 → 0.100, `ppm` 0.321 → 0.094) and leaves the `am`-vs-`cw` inversion standing,
because `am` is **continuous**: it has no off gaps to exclude, its subset barely moves
(1.067 → 1.030 at 10 dB), and what limits it is the phase noise in the troughs of its own
envelope. Selecting the right samples cannot repair a feature whose reading, on a class with
no excursion of its own, *is* the noise. Two independent defects in one dimension, one fixed
and one not — the same shape as T-447's σ_ap/σ_dp finding. The exact-set assertion in
`tests/feature_length_invariance.rs` is what turned "does it still reproduce?" into a test
rather than a belief: no new inversion appeared, and none of the other four entries moved.

### 7.2 `just acceptance-m3` prints three held-out readings, and only one is the gate (T-428)

Within a day of §7.1 landing, four agents re-derived "held-out unknown recall" on `main`
and split two-two: 0.9520 / 0.0480 (T-421, T-416) against 0.9444 / 0.0556 (T-249, T-422),
with T-422 reaching its figure through the stash-and-measure protocol. **Neither side
misread and neither side was stale.** Both numbers are printed by one run of the one
canonical command, over *different draws of the same quantity*:

| Printed as | Where | Population | Value on `651335f` |
|---|---|---|---|
| `[T-206] RULING — … FULL held-out grid (396 snippets, all 11 generators)` | `m3_grid.rs::held_out`, seeds `+700_001…` | **the gate** | 0.9520 / 0.0480 = 377/396 |
| `\| held-out unknown recall \| 0.944 \|` in the T-213 markdown report, and the `[T-206] open set <family> …` per-family lines that decompose it | `hk_classify::harness`, held-out seeds that **continue the `synthetic-acceptance` sequence** — a different 396 snippets from the same 11 generators | not the gate | 0.9444 / 0.0556 = 374/396 |
| `[T-206] the other reading, over the 216 snippets of the six generators §7 enumerates` | `m3_grid.rs`, subset of the gate's draw | not the gate (§7.1 already) | 0.9954 / 0.0046 |

**The gap is sampling, not scoring.** The two readings also use different predicates — the
gate counts `family == "unknown" || open_set_score ≥ 0.5` (both outcomes, per §7 row 4),
the harness counts `family == "unknown"` alone — so the predicate was the obvious suspect.
It is not the cause: instrumenting `held_out()` to count both predicates over the gate's
draw gives **377 either way**, so the `open_set_score` clause contributes zero and the
entire 3-snippet gap is the different seed range.

**So 4 decimal places were never defensible.** Re-running the gate's own draw at eight seed
bases (`+100_000 … +800_000`) on `651335f` gives 370, 377, 377, 377, 378, 379, 380,
381 of 396 — **0.9343 to 0.9621, mean 0.9530, sd 0.0084**. The "disagreement" that produced
this ticket is 0.0076, *under one standard deviation of the draw-to-draw spread of the same
quantity on identical code*. A 4-dp quote invites exactly the false-precision comparison
that cost four agents a day.

**How to quote it.** The gate's assertion stays exact and reproducible (377/396 on the
pinned seeds, 9/9 runs) — that is what CI checks. But a **brief citing this as a
do-not-regress baseline quotes it as `≈0.95 (draw sd 0.008; a ±0.02 move is noise)`**, and
treats only a move outside that as a signal worth investigating. Comparing two figures to
4 dp is meaningful only when both came from the same seed range, which the printed output
does not make obvious — hence the table above.

**This was a fourth failure mode**, distinct from the three above: not a stale number, not
the §7-vs-full population split §7.1 already names, and not T-404's never-merged commit. It
is **one metric name over two different draws, emitted by the same command in the same
run**, where the quantity's own sampling spread exceeds the gap. The stash-and-measure
protocol cannot catch it — T-422 followed the protocol correctly and still got 0.9444,
because the protocol checks *which tree* was measured and this failure is about *which
line was read*.

### 7.3 The class name moves with the SNR because the *grid's own geometry* does (T-435)

T-429 measured, and deliberately did not assert, that the within-family **class name** changes
with the SNR on 3 of 252 blind ladders — two `qam16`↔`qam64` swaps and, the one that mattered,
**a 16-QAM returned as `analog`/`ssb` at 15 dB**, a confidently-wrong *family* at the gate.
T-435 re-measured both on 21 classes × 7 SNR rungs × 24 seeds (3 528 classifications) and
established the mechanism. **Neither half is a classifier defect.**

**The family error: the generator hands the classifier a snippet at the wrong analysis
geometry.** The same waveform (`Qam16`, one seed), read across the ladder:

| SNR | delivered rate | reported OBW99 | `flatness` | `carrier_line_db` | tree's `analog` rule | verdict |
|---|---|---|---|---|---|---|
| 12.5 dB | 143 kHz | 34 kHz | 0.83 | 0.0 dB | denies `analog` | `unknown` |
| **15 dB** | **500 kHz** | **172 kHz** | **0.24** | **18.4 dB** | **admits `analog`** | **`analog`/`ssb`, confidence 0.999, open set 0.000** |
| 17.5 dB | 143 kHz | 40 kHz | 0.78 | 1.3 dB | denies `analog` | `psk-qam` |
| 20 dB | 143 kHz | 40 kHz | 0.78 | 0.0 dB | denies `analog` | `psk-qam`/`qam16` |

The emission is ~40 kHz wide at every rung. At 15 dB `synth::measured_obw` — a **noise-referenced**
`occupied_band` over the noisy snippet — returned 172 kHz, so `decim = floor(fs / (2·obw))` came
out 2 instead of 12 and the snippet was delivered at 500 kHz. A 40 kHz emission in a 500 kHz band
is spectrally a narrow line in empty space: `flatness` collapses 0.78 → 0.24 and `carrier_line_db`
jumps 0 → 18.4 dB. Those are **exactly the two dimensions `tree.rs` tests** to deny `analog`
(`mu42_a > 1.5 ∧ flatness ≥ 0.45 ∧ carrier_line_db < 14`), so `analog` was admitted; and they are
exactly what `ssb` is fitted as (`carrier_line_db` 35.5, `flatness` 0.10). `ssb` scored m 1.388 /
plausibility 1.000 against `qam16`'s m 6.979 / plausibility 0.000. **The classifier answered
correctly the question it was asked.** Nothing inside it distinguishes this case from a real SSB
carrier, so no abstention rule, confidence cap or open-set threshold on the C15 side can fix it —
any that appeared to would be a constant written against a harness defect.

**And the geometry defect is two orders of magnitude larger than the symptom.** Over the 504
blind ladders, the OBW99 the grid reports for **one** (class, seed) moves with the SNR by:

| worst-rung / top-rung OBW99 | ladders |
|---|---|
| < 1.1× | 154 |
| 1.1–1.5× | 203 |
| 1.5–2× | 32 |
| 2–3× | 25 |
| **≥ 3×** | **90** |

**115 of 504 ladders (23 %) move by ≥ 2× and 90 (18 %) by ≥ 3×**, in six of the eight families
(`analog` 62/120, `ook-ask` 18/48, `psk-qam` 19/120, `fsk` 9/96, `pulsed` 4/48, `ofdm` 3/24;
`css` 0/24 and `noise-like` 0/24 are the two that hold).
The extremes: an `am` carrier reads 82 / 31 / 1 / 1 / 1 / 1 / 1 kHz up the ladder (the occupied
band collapsing onto the carrier bin once the noise falls), and `ofdm` oscillates between 30 kHz
and 1 000 kHz — the `fs/2` fallback — on adjacent rungs. **The delivered analysis geometry is a
statistic of the noise**, which is T-427's `duty` and T-249's `sigma_af` one level up: a quantity
named as a property of the emission that reads the receiver. The densities in
`data/densities-1.json` are fitted over that mixture.

This is also where the remaining wrong-family calls live. Of the **7 confidently-wrong family
calls in 3 528** (0.0020; wrong-label of any confidence 10/3 528 = 0.0028), **5 sit on a rung
whose delivered rate moved > 1.7× from the ladder's mode**, and the other two are below-gate
`analog` absorption. Filed as its own ticket; it is **not** fixable inside a classifier test,
because correcting it changes every waveform in the dev grid and so requires refitting both
density files and re-deriving every figure in §7.1.

**The name half is calibrated, and is T-246.** The within-family name reverses on **9 of 504**
acceptance ladders (1.8 %) and 13 of 504 dev ladders (2.6 %) — so T-429's "3 of 252" reproduces
as a rate of about 2 %, not as three special sequences. Six of the nine acceptance flips are the
identical sequence `qam64 → qam16 → qam16`: at the lowest rung where `psk-qam` may name a class
(gate + 5 dB) a 16-QAM is called 64-QAM and corrects upward. That direction is **T-246**'s open
defect — the psk-qam ranking is monotone in constellation size on a smeared constellation — seen
from the density side rather than the ALRT's. It is **not** T-422's shape: the deciding stage is
`FeatureTree` in every one of these, and the post-sync verifier never ran. And the report is
already honest about it: the flipped names carry `p` 0.47–0.66, i.e. the classifier declares the
call a coin toss, while the `psk-qam` family under it holds at 0.999. **A reversing coin toss is
a calibrated answer, not a wrong one**, so nothing here needs an abstention or a confidence cap.

**What T-435 asserted, and what it did not.** Not the name ladder: a per-ladder exception list is
seed-count fragile (T-429's own reason, measured — 1 flip at 6–8 seeds, 3 at 12), and its cause is
already owned. Not a targeted seed regression: it would pin a harness artefact behind a magic seed.
Not an abstention in the classifier, for the reason above. What it did assert is the gap the
measurement actually exposed: §7's per-bin wrong-label floor of 0.05 says *any* bin, but §7.1's
quoted "worst bin" figure is measured over **bins ≥ gate**, and the "overall" figure averages the
below-gate bins into 630 snippets. So the below-gate bins are inside the floor and outside every
figure anyone reads. `crates/hk-classify/tests/below_gate_absorption.rs` applies §7's **existing**
0.05 to exactly those bins, for the three families that can be gated out while another family is
still entitled to answer (`fsk`, `ook-ask`, `psk-qam` against `analog`'s 10 dB gate), and adds
ADR-0016 §2 as a property: where the measurement was not allowed to be made, abstention must
outnumber replacement. Measured there, worst bin `fsk` at gate − 7.5 dB: **4 wrong of 96 =
0.0417 against the 0.05 floor, with 92 abstentions** — a 1-snippet margin, which is itself the
finding: the below-gate densities are holding this line, and only just.

## 8. MAUTO interface (M3 side; ADR-0015 owns the search)

`hk_model::classify::SearchSeed`, assembled by `hk-pipeline/src/seed.rs` (T-215) and served at `GET /api/inventory/{id}/seed`:

```rust
pub struct SearchSeed {
    pub emitter: EmitterId, pub t: Timestamp,
    pub classification: Classification,          // current by arbitration rank
    pub features: Option<EmissionFeatures>,      // parameter ranges: value ± 3σ, plus harmonic alternatives
    pub signature_match: Option<SignatureMatch>, // full/partial candidates with recipe bindings and missing fields
    pub cluster: Option<ClusterSeed>,            // {cluster_id, members, best_pipeline: Option<{recipe_id, version, evidence_score}>}
    pub budget_hint: BudgetHint,                 // {open_search_min_share, families_ordered: Vec<(family, posterior, likelihood)>}
}
```

Rules the M3 side guarantees (ADR-0015 decides how the search uses them):
- **Order by posterior, prune by likelihood.** A family branch may be pruned only if its likelihood share < 0.02 **and** it is below no gate (a below-gate family is "not measured", not "ruled out"). A prior can reorder but never prune.
- **Open-search share** ≥ `max(p_unknown, 0.2)` of the per-signal budget.
- **Templates first.** `full` and `partial` candidates with a `recipe` are the fast path in score order. `missing[]` names what the search must estimate.
- **Cluster reuse.** A cluster's best partial pipeline so far seeds every member.
- **Feedback.** A search that reaches a CRC-valid decode writes a rank-1 Classification (the recipe's declared class), mints or confirms a `recipe-confirmed` Signature, and promotes the cluster. A failed search writes nothing to Classification: failing to decode is not evidence against a modulation. It is recorded on the cluster as `search_exhausted {family, budget}` for ADR-0015 to use.

## 9. API and data-model deltas

**Routes** (planned; each task moves its rows into a served section of `docs/api.md` with contract tests):

| Method | Path | Task | Shape |
|---|---|---|---|
| GET | `/api/inventory` (row, additive) | T-199/T-201/T-202 | `classification` gains `taxonomy, stage, coarse, class?, top[] (≤5, incl. unknown), entropy_norm, flags`; new `signature {outcome, top?, missing[]}`, `cluster_id?`; filter `cluster`, `family` via `family_of` |
| GET | `/api/inventory/{id}/classification` | T-199 | the full current `Classification` (likelihood, prior, provenance, reasons) plus `latest` if it differs |
| GET | `/api/inventory/{id}/classifications` | T-199 | history, `cursor`/`limit` |
| POST | `/api/inventory/{id}/classify` | T-199 | re-run on the stored snippet/recording (audited) → `Classification` |
| GET | `/api/taxonomy` | T-211 | taxonomy tree, versions, `thresholds@1` |
| GET | `/api/inventory/{id}/features` | T-201 | current `EmissionFeatures` |
| GET, POST | `/api/signatures`, `/api/signatures/{id}` (`?version`) | T-201 | list, read, create a new version (validated, audited) |
| DELETE | `/api/signatures/{id}` | T-201 | retire (versions kept) |
| GET | `/api/signatures/match?emitter=<id>` | T-201 | `SignatureMatch` |
| POST | `/api/signatures/import` | T-214 | rtl_433 flex specs (untrusted, validated) |
| GET | `/api/clusters`, `/api/clusters/{id}` | T-202 | members, centroid, events, best pipeline |
| POST | `/api/clusters/{id}/promote` | T-202 | → `Signature` |
| GET | `/api/ml/models` | ~~T-203~~ **T-844** | registry, loaded, provider, mode per consumer, conformance |
| PUT | `/api/ml/models/{id}/mode` | ~~T-203~~ **T-844** | `{consumer, version, mode, force?}` (audited; `active` needs evidence or `force`) |
| GET | `/api/ml/shadow` | ~~T-203~~ **T-844** | per-model, per-SNR-bin agreement summary |
| POST, GET | `/api/datasets`, `/api/datasets/{id}` | T-205 | export a labelled snippet set (job), list/read manifests |
| GET | `/api/inventory/{id}/seed` | T-215 | `SearchSeed` |

**Stream.** `classifications`: ADR-0004 `messages`, metadata only. One record per change of current family, signature outcome or cluster.

**Migrations** (all T-211, so the M3 groups never touch `MIGRATIONS`):
- `0006_classification.sql`:
  - `emitter_classification` + `taxonomy`, `stage`, `arb_rank`, `detail`;
  - `emission_features` (id, emitter_id, t, version, body; append-only);
  - `signature` (id, version, name, kind, provenance, author, created_at, body; PK (id, version); no-update trigger);
  - `signature_match` (append-only);
  - `signature_cluster` (id, state `pending`/`active`/`merged`/`promoted`, merged_into, signature ref, centroid, created_at);
  - `emitter_cluster` (append-only with supersession, as `emitter_link`);
  - `cluster_event` (append-only).
- `0007_datasets.sql`: `dataset_export` manifest rows (T-205 fills it).

**docs/07 additions** (T-211 applies them):
- §2.11 Emitter: `classification` now references §2.21; add `cluster_ref`, `features_ref`, `signature_match`.
- New sections:
  - §2.21 Classification (this ADR §2, arbitration rank, legacy fit);
  - §2.22 EmissionFeatures;
  - §2.23 Signature;
  - §2.24 SignatureMatch;
  - §2.25 SignatureCluster + cluster events;
  - §2.26 ModelManifest / Prediction (provenance only; shadow records live in hk-store);
  - §2.27 DatasetExport.
- §2.13 Annotation: `kind: label` values use `hk-mod@1` labels with `label_source` (`decoder` / `user`).

## 10. Crate and file ownership (T-211 pre-adds every shared declaration)

| Task | Owns |
|---|---|
| T-211 | `hk-model/src/classify/**`, `hk-model/src/signature/**` (types, `fuse`, taxonomy, eval report); migrations 0006/0007; `repo/cluster.rs` arbitration rank + `family_of` gate; new crates `hk-classify`, `hk-ml` (stubs, Cargo); `pub mod` lines; `hk-api` dispatch stubs `classify.rs`, `signatures.rs`, `clusters.rs`, `ml.rs`, `datasets.rs`; `/api/taxonomy`; docs/07; docs/api.md "Classification (planned)" |
| T-199 | `hk-classify/src/{features,tree,density,openset,pipeline}.rs`, `hk-classify/data/**`, `hk-pipeline/src/classify.rs` (single call site), `hk-api/src/classify.rs` |
| T-200 | `hk-classify/src/verify.rs` |
| T-212 | `hk-context/src/priors/family_prior.rs` |
| T-213 | `py/hkpy/synth/amc/**`, `py/tests/test_amc*.py`, `tests/e2e/tests/acceptance/common/classify.rs` |
| T-201 | `hk-context/src/signature/{features,matcher,store}.rs`, `hk-model/src/repo/signatures.rs`, `hk-pipeline/src/signatures.rs`, `hk-api/src/signatures.rs`, `signatures/*.signature.json` |
| T-202 | `hk-context/src/signature/cluster.rs`, `hk-model/src/repo/clusters.rs`, `hk-api/src/clusters.rs` |
| T-203 | `hk-ml/**`, `py/hkpy/ml/make_conformance_model.py` (**landed**) |
| **T-844** | `hk-store/src/ml/**`, `hk-api/src/ml.rs`, the three `/api/ml` rows in `docs/api.md` (**reassigned from T-203, 2026-09-22 — T-365**; see the amendment note below) |
| T-204 | `py/hkpy/ml/train_amc/**`, `hk-classify/src/dl.rs` |
| T-205 | `hk-store/src/dataset/**`, `hk-api/src/datasets.rs` |
| T-215 | `hk-pipeline/src/seed.rs`, the seed route in `hk-api/src/classify.rs` (after T-199 merges) |
| T-206 | `tests/e2e/tests/acceptance/m3_*.rs`, `just acceptance-m3` |
| T-207 | `ui/src/app/explore/**` classification/signature/cluster views |

Shared, append-only (merge order T-199 → T-201 → T-202 → T-844 → T-205): `http.rs ROUTES`, `api_contract.rs`, `docs/api.md` subsections, `ApiState` fields, `hk-cli` construction lines. Evolution rule as in ADR-0012 §10: additive changes are allowed in the owning PR; renames and semantic changes amend this ADR.

### Amendment 2026-09-22 (T-365): the shadow store and the `/api/ml` routes are T-844's, not T-203's

**The defect this fixes is an attribution one.** T-203 reads `status: done` on the board while this section assigned it `hk-store/src/ml/**` and `hk-api/src/ml.rs`, and §9 assigned it `GET /api/ml/shadow`, `GET /api/ml/models` and `PUT /api/ml/models/{id}/mode`. None of that exists: there is no `crates/hk-store/src/ml/`, no `crates/hk-api/src/ml.rs`, no `/api/ml` row in `docs/api.md`, and `hk_ml::host::MemoryShadowSink` is the only `ShadowSink` in the tree. Every *other* M3 row's routes did land (`/api/taxonomy`, `/api/signatures`, `/api/clusters`, `/api/datasets` are all served), so this was the one row where an ADR assigned work to a completed ticket — and a reader could not tell whether the ticket had been closed early or the ADR had gone stale.

**It is the ADR that was stale, and the board says so in T-203's own words.** T-203's acceptance is *"Mac provider plus CPU reference; conformance suite; batching defaults; inference only on CFAR-surviving detections; shadow mode records predictions without acting"*, and its `scope_audit_2026_09_15` narrowed it further to *"the model host itself: runtime load/unload, batching, model@version provenance, inference gated to CFAR-surviving detections, and the provider conformance suite"*. Neither mentions a store or a route. T-203 delivered exactly that, including the `ShadowSink` **trait** and the in-memory implementation its unit tests need. The durable sink and the operator surface were never in its definition of done; this table's `hk-store`/`hk-api` cells were a planning-time allocation that the re-scope left behind. So T-203's `done` stands and the table moves.

**Why the cells were not simply implemented instead.** ADR-0016 §4.6 puts each family in `off`/`shadow`/`active` on evidence, and T-204 measured that no family earns even `shadow` on the dev evidence available (on `fsk`, AUROC 0.326 against classical's 0.824 and a false-known rate of 1.000 against 0.325). Nothing therefore *produces* a shadow record today, and `hk-ml`'s own crate docs (T-363) record the host as **dormant by design** behind three conditions: a model installed in a registry, a family clearing §4.6, and this durable sink. Building the writer and the three routes now would add a store nothing writes to and an operator surface over an empty table — the capability-with-no-caller pattern T-363 counted seven prior instances of. T-844 therefore owns the sink **together with** the condition that makes it non-vacuous, and is scoped so that its tests can only pass with a real producer behind them.


## Options considered

- **Softmax confidence as the unknown score.** Rejected: overconfident on unknowns (C15/C38 cards).
- **Priors multiplied into every label, unknown included.** Rejected: a strong allocation prior would erase unknowns and hide pirates (C15 pitfall).
- **A new `classification` table replacing `emitter_classification`.** Rejected: it breaks T-183 ranking, inventory reads and append-only history for no gain. The migration is additive.
- **DL chooses the family.** Rejected for M3: the sim-to-real gap (−7 points, 59–80 % under LO offsets, docs/04 §5.3). The within-family class is the smallest blast radius.
- **Signatures as files, like recipes.** Rejected: clusters and matches join with emitters and are minted automatically. Built-ins stay seed files.
- **Plain batch DBSCAN only.** Rejected: an on-device inventory needs an answer per sighting, so batch DBSCAN is used only as repair.
- **Candle / Burn / hand-written wgpu for ML** (§6).

## Consequences

- One classification row shape serves the UI, attention (`class_entropy` from `entropy_norm`), explanations and MAUTO. Legacy rows keep working.
- Priors, DL and signatures are bounded by construction (dominance rule, likelihood-only pruning, within-family DL, no identity from a match), and T-206 tests those bounds.
- Two new crates (`hk-classify`, `hk-ml`). The default build adds one pure-Rust ONNX dependency; ort is opt-in.
- After T-211, the groups M3A (C15), M3B (C18), M3C (C38), M3P, M3V and M3L run in parallel within the 4-build limit.

## M3 task graph

| ID | Title (confirmed / revised) | Deps | Area | Model, effort | Group | Change |
|---|---|---|---|---|---|---|
| **T-211** | M3 contracts skeleton: `hk_model::{classify,signature}` types + `fuse` invariants + taxonomy `hk-mod@1` + eval report schema; migrations 0006/0007; arbitration rank replacing `FAMILY_ORDER` + `family_of` fingerprint gate; `hk-classify`/`hk-ml` crate stubs; API dispatch stubs + `/api/taxonomy`; docs/07 §2.21–2.27; docs/api.md planned section | T-198 | hk-model, new crates, hk-api, docs | Opus, high, core_interface | M3K | **new** (serial first) |
| T-199 | C15 classical feature tree with class-conditional densities, χ² open set, per-family gates, fusion via T-211 `fuse`, single pipeline call site, classification routes | T-211 | hk-classify, hk-pipeline/classify.rs, hk-api/classify.rs | Opus, high, core_interface | M3A | revised: deps; prior source split to T-212; eval data from T-213 (may start on its own grid, switches when T-213 merges) |
| T-200 | C15 ALRT/GLRT verifier within the post-sync class set | T-199 | hk-classify/verify.rs | Opus, medium | M3A | confirmed |
| **T-212** | C17 `FamilyPriorSet`: service → expected-family table, P(family∣f,ℓ) with λ₀ ≥ 0.1, mismatch flag inputs | T-211 | hk-context/priors | Sonnet, medium (Opus review: fusion correctness) | M3P | **new** (split from T-199) |
| **T-213** | M3 evaluation harness: AMC synthetic grid (classes × SNR × HackRF impairments), dev/acceptance seed split, held-out unknown generators, OTA decode-label loader, per-SNR `EvalReport` writer | T-211 | py/hkpy/synth/amc, tests/e2e common | Sonnet, medium | M3V | **new** |
| T-201 | C18 EmissionFeatures aggregation + Signature store (recipe-confirmed minting) + SignatureMatch full/partial/none + routes | T-211 | hk-context/signature, hk-model repo, hk-pipeline/signatures.rs, hk-api | Opus, medium, core_interface | M3B | revised: deps T-211 |
| T-202 | C18 incremental clustering (online leader + nightly DBSCAN repair, merge/split history) linked to emitters; cluster routes + promote | T-201 | hk-context/signature/cluster.rs, hk-model repo, hk-api | Opus, medium | M3B | confirmed; algorithm fixed |
| **T-214** | rtl_433 flex-spec import as untrusted signatures (validated; must match fixtures where available) | T-201 | hk-context/signature/import.rs, hk-api | Sonnet, low; priority low | M3B | **new** |
| T-203 | C38 `hk-ml`: MlProvider (tract CPU reference, ort+CoreML opt-in after the day-1 bake-off), registry/manifest, batching, shadow mode (trait + in-memory sink), conformance suite | T-211 | hk-ml | Opus, high, core_interface | M3C | revised: provider decided, deps T-211; **store + ml routes moved to T-844 (T-365, 2026-09-22)** |
| **T-844** | C38 durable shadow: hk-store `ml/shadow/` NDJSON `ShadowSink` + per-SNR agreement aggregates + the three `/api/ml` routes, landed *with* a producer so the path is non-vacuous | T-203, T-204 | hk-store/ml, hk-api/ml.rs | Opus, high, core_interface | M3C | **new** (T-365: reassigned from T-203) |
| T-204 | C15 per-family DL (within-family class, energy open set), py training on the T-213 grid, shadow first, enable evidence per §4.6 | T-199, T-203, T-213 | py/hkpy/ml, hk-classify/dl.rs | Opus, high | M3C | revised: deps + scope (class only) |
| T-205 | Labelled-capture dataset export: CRC-valid decode + user labels → `hk-mod@1` labels, SigMF snippets with provenance and session split keys, content-class gated | T-211 | hk-store/dataset, hk-api/datasets.rs | Sonnet, medium | M3L | revised: deps T-211, group renamed |
| **T-215** | SearchSeed assembly + `GET /api/inventory/{id}/seed` for MAUTO (§8 rules) | T-199, T-201, T-202 | hk-pipeline/seed.rs, hk-api | Opus, medium, core_interface | M3M | **new** |
| T-206 | M3 blind acceptance through the mock SDR (§7 gate) | T-199, T-200, T-201, T-202, T-204, T-212, T-213, T-215 | tests/e2e | Opus, medium, core_interface | M3E | revised: deps |
| T-207 | MUI: posterior top-k with unknown prominent, flags, signature match, cluster link in focus panel + inventory | T-199, T-201, T-202 | ui/src/app/explore | Sonnet, low | MUI-X3 | revised: + T-202 |
| **T-216** | Jetson: `ort` TensorRT EP provider through the conformance suite + head-only on-device fine-tune spike | T-203 (+ Jetson hardware) | hk-ml `ml-trt` | Opus, high | JET | **new**, blocked (like T-026) |

**Wave plan.**
- Wave 0: T-211.
- Wave 1: T-199, T-201, T-203 (three Rust builds), with T-212, T-213 and T-205 as the fourth slot, rotating (T-213 is mostly Python).
- Wave 2: T-200, T-202, T-204, T-214.
- Wave 3: T-215, T-207.
- Wave 4: T-206.

On-device fine-tuning (docs/11 M3 row) moves to T-216. M3 closes on off-device training plus the T-205 dataset path.

## Open questions (for the user)

1. **Exit floors.** Are the §7 floors (top-1 ≥ 0.90 above gate + 5 dB, wrong-label ≤ 2 %, unknown recall ≥ 0.80) the right bar? Real OOK/PSK truth is still missing (S5 §6). Would you capture an owned 433 MHz remote and an AIS or NOAA pass so the OTA rows aren't FSK/analog only?
2. **ML dependency.** Is tract (pure Rust) in the default build acceptable? And ort only if the Mac bake-off shows ≥ 2×?
3. **User authority.** Should a user reclassification (rank 0) outrank a CRC-valid decode (rank 1), or should a decode win and flag the contradiction? — **Answered 2026-09-23 (user, docs/20 U3 = A):** the user wins (rank 0); the contradicting decode is recorded and shown beside the label. Recorded in ADR-0015 §5.5.
4. **Cluster novelty.** Should a newly `active` cluster (a never-seen *type*) feed ADR-0012 novelty as its own component? That would amend ADR-0012 §4.4.
5. **Signature sharing.** Are signatures exportable (without instance data) for a later friend-user exchange, or local only for now?

## Sources

- `spikes/s5-blind-estimation/REPORT.md` §3.2–3.5 and the SNR-floor table (repository; measured 2026-09-13).
- docs/04 §1.1 (prior formula, λ₀), §5.1–5.5 (cascade, open set, compute), §7.6–7.7 (signature schema, DBSCAN); docs/03 §4.1–4.2 (datasets, OTA accuracy): via capability cards C15, C17, C18, C38, not re-read for this ADR.
- Azzouz & Nandi feature set and cumulant values: via the C15 card (synthetic results, *unverified* for HackRF).
- Energy-based OOD (Liu et al., NeurIPS 2020), OpenMax (Bendale & Boult, CVPR 2016): named in docs/04 §5.4, *not re-read*.
- tract (github.com/sonos/tract), ort (github.com/pykeio/ort, ONNX Runtime CoreML/TensorRT EPs), Candle, Burn: capabilities as described are *unverified*; T-203's bake-off confirms them.
