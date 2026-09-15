# Design doc — Decoder synthesis (auto-decode) + the UI it needs

Status: **design / planning** (2026-09-15, from the user). Milestone **MAUTO**, proposed and not yet scheduled — build after M2 closes and MUI is usable. No product code from this doc yet; it becomes the contract (ADR-0015) when MAUTO is scheduled. Some UI pieces here are general and should fold into the **MUI rewrite (docs/14)** now, not wait for MAUTO — see §7.

## 1. The problem

Detection finds an anomaly in the RF — it might be noise, might be a real message. To explain it we must find **which decoding pipeline and configuration** turns it into valid data. There are hundreds of candidate protocols and, across continuous/discrete parameters, effectively millions of configurations. We cannot try them all. This is a **search / synthesis problem**: synthesize the decoder (pipeline structure + parameters) that best explains the signal, judged by evidence that each decoding step is getting *closer to a real message*.

This closes the exploration loop: **detect → auto-classify → guided search for a decoder → decode → confirm-by-successful-decode.** A full CRC/BCH pass over several frames is near-certain proof the signal is solved → promote to Confirmed; partial evidence → a ranked Candidate with its best-so-far pipeline.

Principle alignment: exploration-first (nothing external is truth — templates only *rank*, the decode *decides*); tune-from-the-processed-output (the refinement loop already does the local-optimization step); classical-first (ML is optional and replaceable — see §5).

## 2. The search formulation

**The space factorizes:**
- **Structure** — which blocks and in what order: `{fm,am,fsk,msk,psk,ppm}_demod → mix/filter/resample → clock_recovery → {diff,nrzi,manchester} → sync_search → deframe → {crc,bch,parity} → fields`.
- **Parameters** — center, bandwidth, symbol rate, deviation, polarity, sync word, FEC polynomial, field layout. Continuous + discrete.

**Do not brute-force the final space.** Exploit that the pipeline is *staged*, and each stage emits a cheap "getting-warmer" score (§3). Search becomes a **guided tree search** (best-first / beam search) over partial pipelines: expand the most promising partial decoders, prune branches whose score cannot improve, under a per-signal **compute budget**.

**Two collapses that make it tractable:**
1. **Blind estimators as heuristics** (already built): symbol rate (cyclostationarity), CFO, bandwidth, modulation-family features. Seed the search near the estimate instead of scanning all values — the difference between intractable and small.
2. **A protocol/template library as priors** (§4): known protocols are recipe skeletons with expected parameter ranges. Try ranked templates first (fast path); fall back to open blind search for unknowns.

**Coarse-to-fine:** quantize and evaluate coarsely; when the promising region is small, switch to **local optimization** — binary search on 1-D params (symbol rate, center), pattern search / Nelder-Mead for a few dims, gradient descent where a differentiable proxy exists. The existing output-driven refinement loop (T-070) is exactly this local optimizer generalized from parameters to structure+parameters.

## 3. Evidence metrics (the objective at each stage)

Each is cheap and monotone-ish toward "real message", used both to rank search nodes and to score the final result:
- post-filter: SNR / in-band energy, spectral shape match
- carrier/pilot: lock quality (e.g. 19 kHz pilot for FM)
- clock recovery: **eye-diagram openness**, timing-error variance
- slicer: constellation tightness / EVM, bimodality of the soft symbols
- framing: **sync-word correlation peak** vs background, regular inter-sync spacing
- FEC/fields: **CRC/BCH pass rate**, and output **bit structure / entropy** (real data is neither random nor constant)

A weighted, stage-gated objective; a stage that cannot clear a floor prunes the branch.

## 4. Template library (priors, never truth)

A data file of protocol templates: `{name, modulation, symbol-rate range(s), framing/sync, FEC, field layout, expected band(s)}`. Used to (a) rank which pipelines to try first for a given detection + band, and (b) provide tight parameter ranges. Templates never confirm a signal on their own — only a successful decode does. Unknown signals bypass templates into the open search and, if partially solved, can be **saved as a new template** (the user grows the library by discovering).

## 5. ML is optional and off the critical path

ML could serve as *one heuristic*: modulation classification, or a learned value function to order the search. Every such slot has a classical fallback (feature classifiers, the estimators in §2). Build the classical guided-search core; leave a typed slot where an ML heuristic can plug in if it ever earns its place. Matches the user's read that ML research here was unpromising.

## 6. What it reuses

- **Blocks + recipes (M1):** the pipeline representation the search assembles; recipes are the synthesized artifact (savable, hot-editable).
- **Blind estimators (hk-estimate) + parser assist (sync/period/entropy/CRC search):** search heuristics.
- **Refinement loop (T-070):** the local-optimization step.
- **Inventory candidate/confirmed (T-078):** the confirm-by-decode target; prunable candidate list (the user will discard many, since chasing infrequent signals floods candidates).
- **Short-burst detector (T-075) + always-on IQ ring:** so a one-off burst (an ADS-B squitter, a POCSAG page on an arbitrary freq like 100.7 MHz) is *caught and its raw IQ retained*, giving the search something to work on without a steady repeating signal. Infrequent signals are a first-class case, not only steady ones.

## 7. UI (some for MUI now, some for MAUTO)

The synthesis engine needs a way to be pointed at a signal, and its results need somewhere to show. Several of these are **general UI gaps the user wants regardless**, so build them in the **MUI rewrite (docs/14)** now; the analyze-action is stubbed until MAUTO lands the engine.

**Region selection → analyze (MUI hook, MAUTO engine):**
- Select a region **manually** by dragging on the waterfall — multiple bands at once (extend the existing selections).
- Select a region of **history** (drag on the capture timeline / a past window) — so you can run analysis on something that already happened, from the IQ ring.
- A **"Analyze / synthesize decoder"** action on any selection or signal → runs §2 search on that band (live or from history IQ) and streams back the best pipeline + evidence, attaching the result to the emitter.

**Confirmed-signal boxes (MUI, general):**
- Every **Confirmed** signal is drawn as a **yellow box** around its band.
- The box **extends up through the spectrogram above the waterfall** (spans both the spectrum trace and the waterfall), not just a waterfall overlay.
- The box has **adjustable left/right edges** (drag to widen/narrow the band; updates the emitter's f_lo/f_hi and can re-trigger analysis/refine).

**Right-click context menu (MUI, general):**
- Right-clicking a signal/box opens a menu with the actions currently in the right-side details bar (Listen, Decode / Analyze, Record / Export clip, Stream out, Promote, Delete, adjust band).
- Moving actions into the context menu **frees the right panel** for display instead of controls.

**Freed panel = per-signal output (MUI + MAUTO):**
- Use the freed region to show the **chosen decoder's final output**: the packet inspector (frame list / hex+ASCII / field tree) for digital, or the decoded text/records stream.
- For **FM/AM audio**, show a **waveform / audio scope** (and RDS text for FM) in that space.
- Multiple confirmed signals can each own an output panel (ties to concurrent demod / multi-stream, T-071).

## 8. MAUTO task sketch (when scheduled)

Design contract first (ADR-0015: pipeline/objective representation, evidence-metric API, search strategy, template format, per-signal compute budget, region-analyze API). Then, roughly parallelizable: search engine core (beam + prune + budget); stage evidence metrics; local-refinement generalization of T-070; template library + loader; the region→analyze API and streaming of best pipeline + evidence; confirm-by-decode wiring into inventory; "save as template" for discovered protocols; burst/one-off path using the IQ ring. UI pieces in §7 land in MUI with the analyze-action wired to the engine as it comes online.

## 9. Expectations & non-goals

- Auto-solves the large class of standard modulation + framing (RDS, POCSAG, ACARS, generic FSK/PSK with CRC); returns **ranked partial results** for the rest ("reached sync, 40% CRC — probable framed data, unknown protocol").
- Will not crack encrypted/proprietary formats — but converts them from mysteries into well-characterized candidates.
- **Channel hopping is out of scope here** — defer until after trunking / multi-channel tracking (M4).
- Compute-bounded per signal; heavier searches are opt-in and respect the device power/thermal budget (Jetson later).
