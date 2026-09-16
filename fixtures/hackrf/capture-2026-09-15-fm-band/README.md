# Capture 2026-09-15 — FM band around 100.8 MHz (live HackRF)

Preserved live off-air capture from the dev/demo HackRF, saved before the staging
server's data dir was recycled. Intended to become a blind-detection acceptance
fixture (T-025 style) for the 88–108 MHz region as sampled at this site.

## Files

- `iq.sigmf-data` — raw IQ, **ci8**, 216,006,656 bytes (216 MB). **Large: keep in
  Git LFS or the external fixture store, never plain git.**
- `iq.sigmf-meta` — hackriff output sidecar (v1.0). Convert to a proper
  `.sigmf-meta` (SigMF core) when formalized.

## Capture provenance (from the sidecar — this is measured truth, keep it)

| field | value |
|---|---|
| center | 100.800 MHz |
| sample rate | 2.400 Msps |
| span recorded | 99.600 – 102.000 MHz (requested band; single tune, no retune) |
| duration | ~45.0 s (2026-09-15T23:41:01.975Z → 23:41:46.977Z) |
| device | HackRF One serial …d2b861dc263bc293, fw 2026.01.3, board rev 0, libhackrf 0.9.2 |
| gain | LNA 32 dB, VGA 30 dB, **amp ON** |
| baseband filter | 1.75 MHz |
| antenna | unknown port |
| integrity | 0 dropped records, 0 lost samples, 1648 records |

Note the amp is ON with high LNA/VGA on a preselector-less 8-bit front end: expect
images / IMD / harmonics of the two strong WFM stations. That is itself a useful
test property (spur/image rejection, and the signal-relationship / reflection
detection the user asked for).

## Hidden ground-truth candidates (HUMAN/HEURISTIC ANNOTATION — draft)

Derived from the live inventory clusterer at capture time + the user's own ears.
**Not authoritative** and **not for lookup-and-tune tests.** A blind acceptance
test replays this IQ through the full pipeline and asserts (a) each emission below
is *detected* from the RF, and (b) a sensible explanation is among the top-k
recommendations — it must never tune to these frequencies from a table. Frequencies
are approximate emission centers; the clusterer over-split several into skirt
fragments (see "Known artifacts").

| # | center (approx) | type (hypothesis) | notes / why interesting |
|---|---|---|---|
| G1 | ~99.75 MHz | WFM broadcast | strong, wideband (100–360 kHz across fragments); ~280 looks on core. Over-split into ~20 skirt fragments 99.63–99.83. |
| G2 | **99.999 MHz** | **steady carrier (near-CW)** | narrow (~19 kHz), persistent. User-identified "steady carrier wave at 99.99". Good unmodulated-carrier / frequency-accuracy case. |
| G3 | **100.300 MHz** | **narrowband data bursts (AM?)** | narrow (~8.5 kHz), very high hit count (1263 looks on core) but user reports it is intermittent/bursty. User: "data, maybe AM, short intermittent bursts." Prime auto-decode / short-burst target. |
| G4 | ~100.735 MHz | weak intermittent narrowband | ~13 kHz, few looks; near the user's hypothetical 100.7 pager band. Verify real vs spur. |
| G5 | 101.05–101.16 MHz | weak narrowband fragments | low looks; likely weak signals or intermod products — candidate reflection/IMD test material. |
| G6 | ~101.45 MHz | WFM broadcast (dominant) | strongest emission (4810 looks, 58–108 kHz). Over-split into skirt fragments 101.42–101.47. (Previously eyeballed as "101.3" — confirm the dial.) |
| G7 | **~101.70 MHz** | signal (narrowband, ~12 kHz) | user-identified "101.7"; ~350 looks on core, fragments 101.68–101.716. |

## Known artifacts in this capture (test material, not bugs to hide)

- **Over-splitting:** the two strong WFM stations (G1, G6) each produced ~15–20
  duplicate "emitters" along their skirts (narrow fragments, 1–40 looks). This is
  the duplicate-candidate / overlap-resolution problem (docs/15 §10): a good
  fixture for the "confirmed suppresses overlapping candidates" and
  "overlapping candidates compete by evidence" rules once built.
- **Possible images/IMD:** amp-on + high gain, no preselector. Some of the weak
  G5 fragments may be images/harmonics/intermod of G1/G6 rather than real
  emissions — exactly the reflection/signal-relationship detection the user wants
  (memory: user-signal-relationships-reflection). A test can assert these are
  attributed as image/IMD of a strong emitter, not catalogued as independent.

## To formalize (coordinator task)

1. Move `iq.sigmf-data` to LFS/external store; write a real SigMF `.sigmf-meta`
   (SigMF core + antenna/gain annotations) from the sidecar.
2. Verify each G# above by replaying + inspecting; adjust centers/bandwidths;
   classify G3 (AM vs FSK/OOK data), G2 (true CW vs pilot/leakage), G4/G5
   (real vs image/IMD).
3. Write the hidden ground-truth list in the fixture's test-side manifest (not in
   this README, which stays a human note) and add a blind acceptance test by
   use-case ID. Candidate IDs: a SIGNAL- entry for the 100.3 data burst and a
   SIGNAL- entry for the reflection/image attribution.
