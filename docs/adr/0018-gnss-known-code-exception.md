# ADR-0018 — GNSS acquisition is known-signal-led: the one documented exception to blind-first, and the boundary that confines it

**Status:** PROVISIONAL (T-274, opening M5). The *exception itself* is forced by physics and is not really a choice; what awaits review is **which mechanism confines it** (§3) and the decision not to wire the crate into the pipeline yet (§6).

**Touches:** capability C36 (`gnss-observables`), and by exclusion C09/C17. Use cases SIGNAL-030, SIGNAL-032, PROP-033, AWARE-002, AWARE-003. New crate `crates/hk-gnss`. docs/07 §2.31.

---

## Context

CLAUDE.md's central rule is that signals are found by **blind detection from RF data first**, and that the known-signal database never leads:

> **Never a source of truth.** The database is never the starting point, never pre-populates the inventory, and never overrides what was measured.

That rule is load-bearing. It is what makes hackriff an exploration instrument rather than a scanner with a frequency list, and every capability so far has honoured it: `hk-detect` takes a `SpectrumFrame`, a `FloorFrame` and a clip count, and the band plan only ever attaches *afterwards*, downstream of detection, as a low-scoring ranked suggestion.

**GPS L1 C/A cannot honour it**, for a reason that is physical rather than architectural. L1 arrives at roughly −128 dBm spread over about 2 MHz, which puts it **20–30 dB below the thermal noise floor**. It is not a weak bump that a better threshold would reach; there is no bump. CFAR, spectral kurtosis and cyclostationary search all operate on energy, and the energy is not there (docs/04 §4.9: "DSSS sits near or below the noise floor, so energy detection fails").

Recovering it requires **despreading against the published PRN codes**: correlating 1 ms of IQ against a known 1023-chip Gold code buys about `10·log₁₀(1023) ≈ 30 dB` of processing gain, which is what lifts the signal into view. The known-signal pipeline must **lead**, not suggest. GNSS-SDR is the reference implementation.

This is measured, not asserted. `crates/hk-gnss/tests/below_noise_acquisition.rs` builds one piece of synthetic IQ at −20 dB SNR in 2.046 MHz and runs both paths over it:

| Statistic | Satellite present | Satellite absent |
|---|---:|---:|
| Peak periodogram excursion above median | 6.19 dB | 6.16 dB |

A 0.02 dB difference: the spectra are indistinguishable, so no energy detector separates them at any false-alarm rate. Known-code correlation on **that same IQ** recovers PRN 11 at 511.00 chips (truth 511) and 1250 Hz Doppler (truth 1180 Hz, one grid step), peak-to-mean 17.2, C/N0 estimated 42.1 dB-Hz against an analytic 43.1.

**The danger is not that the exception is wrong. It is that it leaks.** If "the database may lead here" becomes reachable from the general detector, the project silently becomes the thing it was built not to be. So the question this ADR answers is not *whether* to make the exception but *what structurally prevents it from spreading*.

---

## Options for confining the exception

| # | Mechanism | Strength |
|---|---|---|
| A | A documented convention — a comment saying "GNSS only" | None. Not a boundary; the next author has no reason to notice it. |
| B | A runtime flag or config gate (`allow_known_signal_lead`) | Weak, and actively dangerous: a flag is a knob, and knobs get turned on. It makes leading *reachable by configuration* from the ordinary path. |
| C | A type-level witness the general detector cannot construct | Moderate. Makes call sites greppable and explicit, but within one crate privacy is a convention that an edit can undo. |
| D | **A separate crate the blind-detection crates do not depend on**, with a test that fails if the edge appears | Strong. Enforced by the compiler and by CI, and any attempt to cross it is a visible manifest change in review. |

### Trade-offs

**B is the option to reject explicitly**, because it is the one a hurried implementation would reach for. A boolean named something like `known_signal_may_lead` turns a structural property into a runtime one: the detector would then *contain* the capability to be led and merely decline to use it. Every future bug, default change or config file becomes a way to flip the project's central invariant. An exception that is reachable by configuration is not confined.

**A is what the task brief rules out** — "a comment saying 'only for GNSS' is not a boundary."

**C is worth having but insufficient alone.** It documents intent in the type system and makes review easy, but it does not stop anything.

**D costs a crate.** That is real overhead: another manifest, another workspace member, another build unit. The cost is small and the property bought is the strongest available in Rust — you cannot name a type from a crate you do not depend on.

---

## Decision

**D, backed by C.** Both, because they fail differently.

1. **GNSS lives in its own crate, `hk-gnss`,** holding the PRN codebook, the correlator, the observable model and the integrity logic. The blind detection path — `hk-detect`, and the `hk-core`/`hk-dsp`/`hk-estimate` it is built on — **does not depend on it**, and does not depend on `hk-context` (the band-plan crate) either. A `PrnCodebook` is therefore un-nameable inside the detector. Reaching it requires adding a dependency edge to a manifest, which is a visible act in review.

2. **`crates/hk-gnss/tests/blind_path_boundary.rs` fails if that edge appears.** It parses the four blind-path manifests and asserts none names `hk-gnss` or `hk-context`; it asserts `hk-gnss` does not depend on `hk-detect` (so the exception cannot steer detection from its own side either); it asserts no source file in `hk-gnss` names a `Detection`, so an acquisition can never be laundered into the inventory as a blind measurement; and it asserts `hk-detect`'s config names no `BandTable`, `PrnCodebook` or `AllocationRow`. The boundary is machine-checked, not remembered.

3. **`acquire()` demands a `KnownCodeLed` witness**, whose only constructor takes a `PrnCodebook`, and every result carries `AcquisitionEvidence::KnownCodeCorrelation { codebook }`. This is the weakest of the three and is documented as such: it exists so that review can *see* the known-signal-led path, not to enforce it.

**Why it cannot be reached from the ordinary blind path:** the detector's inputs are measurements (`SpectrumFrame`, `FloorFrame`, `ClipCount`) and its config carries only receiver knowledge — tuned-centre sensitivity profiles, hardware filter edges, a measured spur mask. There is no parameter through which a code, a frequency list or an allocation could arrive, and no dependency through which one could be named. The leak has neither a route nor a shape.

---

## The other half stays blind, and must

GNSS is two capabilities wearing one name, and only one of them is an exception.

**Jamming and spoofing (AWARE-002, AWARE-003) remain ordinary blind detection**, for the symmetric physical reason that makes acquisition impossible: **a jammer sits above the noise floor.** It is exactly the kind of thing the blind path is good at.

The rule that keeps this honest is in the signature:

```rust
pub fn assess_jamming(
    power: &PowerEvidence,
    lock: Option<&LockEvidence>,   // optional, by design
    cfg: &IntegrityConfig,
) -> JammingAssessment
```

With `None` — no PRN codes, no correlator, no receiver running at all — an in-band floor rise still returns `JammingSuspect`. `integrity::tests::jamming_fires_without_any_observables` asserts it, so the blind half cannot silently acquire a dependency on the exception. Receiver observables, when a receiver happens to be running, only raise confidence.

Nothing in the crate tells a detector where to look. A GNSS floor rise is found wherever it happens by the ordinary path, and the band plan then *suggests* "GNSS allocation here" through the existing `hk-context` machinery, where allocation-only explanations already score low (`ALLOCATION_ONLY_SCORE = 0.2`) and cannot set an emitter's status. That is suggesting, not leading, and this ADR does not change it.

The C36 pitfall is enforced too: satellites lost **without** a floor rise returns `BlockageSuspect`, not jamming — a handheld indoors or against the body must not fill the attack map with the user's own body.

---

## Consequences

- The project now has exactly **one** documented exception to blind-first, confined to one crate, checked by a test, and argued from a measurement rather than from convenience. Any second exception should be made to justify itself against this ADR.
- **Wired by T-322, around the boundary rather than through it.** `hk_pipeline::gnss` is the one place in the product that names both `hk_gnss` and the pipeline, and it is not on the blind path. C04 is offered a recurring `ScheduledDwell` at L1 — a centre, a rate and a duration, nothing GNSS-shaped from the scheduler's side — and **only when the run's own scan plan already covers L1**, which is the gate rather than a flag. Applying a granted step arms the `hk-gnss` reader for that window; the reader takes one contiguous window off the ring, down-converts L1 to baseband and runs `acquire`. `assess_jamming` turns the result plus in-band power into a verdict, which crosses into `hk-context` as `GnssServiceEvidence` — **scalars and a verdict, no codes** — and `hk_context::gnss_service` appends it as an `Explanation` (`Cause::OwnHistory`, `Evidence::Value` rows) to anomalies **the blind path already opened**. It has no branch that opens one. No `hk-detect → hk-gnss` edge appeared; §3's guard stays green and gained a clause: neither carrier file may name a `Detection`, an `Emitter`, an inventory or `hk_detect`, so the exception cannot reach the inventory by the second door either.
  - A consequence worth recording, found by the wiring: `AcquisitionConfig`'s default `threshold_ratio` of 2.5 is a floor sized for a short profile, and the number of code-phase cells is the rate's samples per 1 ms period (2046 at the minimum rate, 20 000 at 20 Msps). Left fixed at 4 Msps it acquires **all 32 satellites out of pure noise**, which would report an intact constellation on every dwell and silently disable the jamming assessment. `hk_pipeline::gnss::acquisition_threshold` computes the bar from the profile length and the non-coherent block count for a stated false-alarm probability.
- **Acquisition without tracking cannot produce a fix.** Tracking loops (DLL/PLL), navigation-message decode, ephemeris, PVT, SBAS/WAAS and OSNMA are not built. SIGNAL-030's "compute a fix" half and most of SIGNAL-032 remain open.
- Every sensitivity claim rests on **synthetic IQ** and is unverified against hardware. Settling it needs an active GNSS antenna on the bias-tee and a real L1 capture, which is user-triggered.
- The C36 recommendation to wrap GNSS-SDR as a plugin is untouched and still the sensible route to a full receiver; this crate is not an attempt to reimplement one.

---

## Status

**PROVISIONAL.** Confirmed by a spike or by the user, on one remaining point: that the crate boundary (rather than a lighter in-crate mechanism) is the right cost. The staging question is settled — T-322 wired the crate without needing the edge, which is the evidence the boundary was not merely convenient to keep while nothing used it.
