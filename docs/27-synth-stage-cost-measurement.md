# 27 — What one candidate evaluation costs on the M1 blocks (T-552 measurement)

**Status:** MEASUREMENT, Mac only. No `hk-synth` exists yet (ADR-0015 is PROVISIONAL, M-1 unscheduled), so this
measures the M1 blocks and M1 `hk_estimate::assist` operators ADR-0015 §3.3's budget profiles are meant to
pay for, not the search engine itself (beam bookkeeping, prefix hashing, LRU cache management have no code
to measure). Jetson figures are explicitly out of scope (deferred to MAUTO task M-13).

**One-line answer:** the per-stage DSP cost (S0–S6) is cheap and dominated by S0 channelisation, so the
beam's prefix memoisation pays off exactly as ADR-0015 hoped — but the **proposal operators**
(`assist.sync`/`assist.codes`/`assist.fields`, §3.2) are 2–4 orders of magnitude more expensive than a full
S0–S6 DSP pass and are what the `quick`/`standard`/`deep` wall-clock budgets actually have to ration. The
3 s/20 s/120 s numbers were never stress-tested against that cost, and on this measurement `quick` buys too
few proposal-operator calls to be confident it covers "templates and the top-2 open skeletons" once those
skeletons need sync/code/field search of their own.

## 0. Method and honesty about the machine

All numbers are **release builds** (`cargo test/nextest --release`), single measurement run each (no
statistical replication — this is an order-of-magnitude survey, not a calibrated table like T-547's).
Machine: **Apple M3 Ultra, 28 cores (20P + 8E), 275 GB RAM** — a desktop workstation, not remotely
representative of the Jetson Orin Nano handheld target. Every wall-clock number below should be read as an
**optimistic floor**: the target device has an order of magnitude less single-thread performance and far
less memory bandwidth. Where a number is stable per-operation regardless of total wall time (the assist
`ns/op` figures), that is closer to machine-independent and is the more portable evidence.

Benchmarks added as `#[ignore]` release-only tests beside the existing M1 test suites (same pattern as the
pre-existing `hk-blocks::blocks::iq::tests::throughput_bench` and
`hk-estimate::assist::tests::assist_bench_work_cap_timing`, both already in the tree before this ticket and
reused directly for §2/§3 below):

- `hk-blocks::blocks::fec::tests::s5_check_throughput_bench` (new, S5)
- `hk-blocks::blocks::framing::tests::s4_sync_search_throughput_bench` (new, S4)
- `hk-blocks::blocks::parse::fields::tests::s6_fields_throughput_bench` (new, S6)

Run with `cargo test --release -p <crate> --lib <path> -- --ignored --nocapture`.

## 1. Per-stage cost, S0–S6

### 1.1 S0/S1/S2/S3 (`hk-blocks::blocks::iq::tests::throughput_bench`, existing bench, re-run here)

240 kHz IQ input (a representative demodulated-channel rate), 16 384-item chunks, 2^20 = 1 048 576 items:

| Block | Params | items/s | ms per 1 s of IQ at 240 kHz |
|---|---|---:|---:|
| mix (S0) | offset 12345 Hz | 6.5e7 | 3.7 |
| lowpass (S0) | cutoff 20 kHz, transition 5 kHz | 8.4e6 | **28.6** |
| resample (S0) | to 48 kHz | 9.8e7 | 2.5 |
| fm_demod (S1) | deviation 75 kHz | 1.4e8 | 1.7 |
| fm_demod (S1) | + resample to 48 kHz | 5.6e7 | 4.3 |
| am_demod (S1) | — | 3.0e8 | 0.8 |
| fsk_demod (S1) | offset tracking 0.5 s | 1.5e8 | 1.6 |
| msk_demod (S1) | — | 1.8e8 | 1.4 |
| ppm_demod (S1, 2 Msps) | ADS-B-style, 1 Mbit/s | 6.7e6 | 299 (per 1 s at 2 Msps) |
| subcarrier (S1, RDS) | 57 kHz, BPSK | 4.8e7 | 5.0 |
| clock_recovery (S2, POCSAG 1200 Bd @ 24 kHz) | — | 1.9e8 | 0.13 |
| clock_recovery (S2, RDS 1187.5 Bd @ 9.5 kHz, biphase, max-contrast) | — | 4.5e7 | 0.21 |
| clock_recovery (S2, MSK 2400 Bd @ 24 kHz, RRC, Mueller-Müller) | — | 1.8e7 | 1.4 |
| slicer (S3, 1 kHz) | — | 1.3e9 | ~0 |
| diff_decode/nrzi/manchester (S3, 1 kHz) | — | 0.2–1.1e9 | ~0 |

**S0's `lowpass` dominates the channelisation head** at ~29 ms per second of 240 kHz IQ — 8–20x the cost of
any single S1 demod, and ~2 orders of magnitude above S2/S3. `mix`+`lowpass`+`resample` (a representative S0
chain) costs **≈ 34.7 ms per second of window** single-threaded.

### 1.2 S4 sync_search (new bench)

Worst case: 4,000,000 noise bits (never locks, so every window is scored — the search's own "no match"
path, not the demodulated-signal fast path):

| Case | bits/s | µs per 1000 bits |
|---|---:|---:|
| RDS sync (26-bit offset words) | 6.0e7 | 16.6 |
| POCSAG sync (32-bit) | 8.2e8 | 1.2 |

At representative symbol/bit rates (RDS 1187.5 bps, POCSAG 1200 bps) a few-second window is a few thousand
bits, so S4's own DSP cost is **tens of microseconds** — negligible next to S0.

### 1.3 S5 check (new bench)

50,000 frames, released per-frame:

| Block | Case | frames/s | ns/frame |
|---|---|---:|---:|
| crc | width-24 Mode-S | 5.0e6 | 202 |
| bch | (31,21) | 1.1e7 | 90 |

A window with a handful of frames costs low single-digit microseconds. Negligible.

### 1.4 S6 fields (new bench)

200,000 64-bit frames, a 4-field (8 bits each) map:

| Case | frames/s | ns/frame |
|---|---:|---:|
| fields, 4×8-bit over 64-bit frames | 1.85e6 | 540 |

Also negligible at the frame counts a real window produces.

### 1.5 Summary: where the DSP time goes

For a **3-second search window** at 240 kHz IQ, evaluating one full prefix S0→S6 (representative FSK/RDS
path) costs **≈ 105–120 ms single-threaded**, of which **~85 ms (≈ 80 %) is the S0 channeliser**
(dominated by `lowpass`), ~5–13 ms is S1, and S2–S6 combined are under 1 ms. This is stable across families:
S0 is always the expensive stage because it runs at the full IQ rate; every later stage runs at a
progressively decimated rate (symbol rate, then frame rate).

## 2. The memoisation payoff (item 2)

ADR-0015 §3.1 memoises stage outputs by prefix hash, so a child pays only for its **new** stage. §1.5 answers
the question directly: **the S0/S1 head dominates** (≈ 85–95 % of a from-scratch evaluation), so **yes, the
beam is much cheaper than a naive full-recompute estimate** — once S0 is cached, adding S1 costs ~5 ms,
adding S2 costs under 1 ms, and S3–S6 cost microseconds. A beam that shares one S0 across all its S1
children, and one (S0, S1) pair across all its S2 children, pays the 85 ms channelisation cost **once**, not
once per beam node.

**Caveat this ADR text doesn't spell out:** S0 itself has free parameters (centre, bandwidth, rate) that the
search grids coarsely (§3.1 step 3), so distinct S0 grid points are distinct cache keys — memoisation does
**not** collapse the S0 grid to one evaluation. If the search tries, say, 3–9 S0 variants (centre × bandwidth
combinations) before settling, the *effective* head cost for one job is 3–9× the single-S0 figure above
(≈ 250 ms–770 ms for a 3 s window), still small next to a 3 s (`quick`) or 20 s (`standard`) wall budget, but
worth stating: "the beam is cheap" is true for stage depth, not for width at S0.

## 3. Proposal operators (item 3) — reusing the existing `assist_bench_work_cap_timing` bench

`cargo test --release -p hk-estimate --lib assist_bench -- --ignored --nocapture` (this bench predates T-552;
it measures exactly the calls ADR-0015 §3.2 wraps as `assist.sync`/`assist.codes`/`assist.fields`):

| Budget | Case | wall (s) | ops | ns/op |
|---|---|---:|---:|---:|
| default | crc search, 100 frames × 4000 bits, tails≤256 | 0.775 | 5.0e8 | 1.55 |
| default | crc search, 2000 frames × 112 bits | 0.763 | 5.0e8 | 1.52 |
| default | crc search, 20000 frames × 20 bits | 0.680 | 4.8e8 | 1.41 |
| default | sync-frame hunt, 2000×112 | 0.013 | 1.1e7 | 1.23 |
| default | sync-stream search, 400k-bit noise | 0.340 | 5.0e8 | 0.68 |
| default | sync-stream search, RDS-shaped | 0.334 | 5.0e8 | 0.67 |
| default | field-map search, 100×4000 | 0.770 | 5.0e8 | 1.54 |
| default | field-map search, 20000×20 | 0.064 | 5.4e7 | 1.20 |
| max ops | crc search, 100×4000 | 2.420 | 1.5e9 | 1.61 |
| max ops | crc search, 2000×112 | 1.252 | 7.8e8 | 1.60 |
| max ops | sync-stream search, 400k-bit noise | 1.015 | 1.5e9 | 0.68 |
| max ops | field-map search, 100×4000 | 2.443 | 1.5e9 | 1.63 |

**Finding: a single `assist.codes` or `assist.fields` call at the crate's own default work cap already costs
0.3–0.8 s, and 1–2.4 s at its "max" cap — 3 to 30x a full S0–S6 DSP evaluation (§1.5's ~0.1 s).** The
per-operation cost (1.2–1.6 ns/op for the GF(2)/bit-serial work, 0.7 ns/op for stream search) is stable
across the default/max caps, which is evidence the *existing* `Budget{max_ops}` in `hk-estimate` is already
tracking near-machine-independent work — unlike the `SynthBudget{wall_s}` ADR-0015 proposes layering on top.

## 4. How many evaluations each profile actually buys (item 4)

Take the 3 s window figures above as one representative "evaluate a candidate, then propose fragments for
it" cycle:

- DSP evaluation of one new stage once S0/S1 are cached: **≤ 5 ms** (S2–S6 are ≤ 1 ms).
- One `assist.sync`/`assist.codes`/`assist.fields` call, needed once per S4/S5/S6 slot a candidate tries: **0.3–2.4 s** depending on the operator's own budget tier.

So **DSP re-evaluation is not what a profile's wall-clock buys — proposal-operator calls are.**

| Profile | wall_s | S0-grid heads afforded (§2, ~85–250 ms) | assist calls afforded at *default* op-budget (~0.3–0.8 s) | assist calls afforded at *max* op-budget (~1–2.4 s) |
|---|---:|---:|---:|---:|
| `quick` | 3 | ~10–35 | **~4–9** | **~1–3** |
| `standard` | 20 | ~65–230 | ~25–65 | ~8–20 |
| `deep` | 120 | ~400–1400 | ~150–400 | ~50–120 |

A single candidate that reaches S6 through open search plausibly needs **at least one call each** of
`assist.sync` (find the sync word), `assist.codes` (find the check), `assist.fields` (propose a field map) —
3 calls minimum, more if the first proposal is wrong and the beam backtracks, or if multiple surviving beam
nodes (up to 8 at S0–S2, 4 at S3–S6, with ≤ 2 per family) each run their own proposals. §3.3's own text says
`quick` is scoped to "templates and the top-2 open skeletons" — but even 2 open skeletons each needing a
3-call minimum already consumes `quick`'s entire *default-op-budget* allowance (6 of ~4–9 calls), leaving
nothing for a wrong first guess, a template mismatch, or a deferred family. **At the assist crate's own "max"
op tier, `quick` cannot complete even one open skeleton's full sync+codes+fields sequence with margin.**

This is the ticket's hypothesised failure mode, observed: **`quick` at 3 s buys too few evaluations for the
top-2-open-skeletons workload the profile is specified to cover**, at least at the assist operators' current
default work cap on this machine — and the Jetson will be slower, not faster.

## 5. What fraction of the machine, and the ring-safety question (item 5)

**Fraction of the machine:** all figures above are **single-threaded** (`quick` uses 1 thread by design;
`standard` 2; `deep` 4). On this 28-core measurement machine that is a vanishingly small fraction of total
capacity; on a 6-core Jetson Orin Nano it is 1/6 to 4/6 of the CPU, which is a materially different risk
picture the ADR's "unverified guesses" line already flags.

**What this ticket did NOT and could not measure:** whether a `synth` chain at lower OS priority disturbs the
capture ring (T-453's rule — per-arriving-row and per-job cost on the capture thread must be *measured*, not
assumed). That requires a running capture pipeline with a concurrent synth-shaped workload contending for
the same cores, and **no `hk-synth` or `synth` chain kind exists yet to run** (ADR-0015 is planning-only,
M-1 unscheduled). This is an **honest gap**, not a finding — it is squarely MAUTO task M-3 (search engine +
power/throttle) and M-13 (Jetson budget/power measurement)'s job once there is a chain to instrument, not
something this ticket can produce evidence for by proxy. Flagging it here rather than filing a new ticket:
the ADR's own task graph (§10, M-3/M-13) already owns it.

## 6. Recommendation

**Change the profiles' shape rather than only re-baselining their numbers.**

1. **Budget the expensive, variable part — proposal-operator calls — by evaluation count (or by the
   `hk-estimate::assist::Budget{max_ops}` this crate already has), not by wall time.** §3's ns/op figures
   are near machine-independent; §4 shows wall time is dominated by *how many* proposal calls fire, which is
   a property of the search (beam width, family count, how often the first guess is wrong), not of the
   window duration being analysed. A `max_proposal_calls` (or a shared op budget across all three operators)
   travels to the Jetson unchanged; `wall_s` does not.
2. **Keep a wall-clock ceiling as a backstop** (`max_wall_s`), since the API and power-policy story (battery
   refuses `deep`, `503 busy`) need a human-meaningful duration — but stop treating `wall_s` as the thing
   that determines *how much search happens*. Two runs that both hit `wall_s` should have done comparably
   much work; on this measurement, they would not, because one bad first-guess retry burns the whole budget
   on a single `assist.codes` "max" call.
3. **`quick`'s 3 s figure specifically should be re-examined against §4's table before it ships in the public
   API.** Either narrow `quick`'s scope further (templates only, no open skeletons — §3.3 already lists this
   as a fallback framing) or raise its evaluation allowance to guarantee at least one full sync+codes+fields
   sequence per skeleton it claims to try.
4. **S0's channelisation cost (§1.5, §2) is the one number in this measurement that *does* support the
   existing design as specified:** memoisation genuinely amortises it, so **stage depth is cheap once the
   head is cached** — nothing here argues for changing the beam or stage-ladder structure, only the budget
   unit.

## 7. Confidence and what would sharpen this

- Single-run wall-clock timing, no repeated trials, no statistical bars — this is an order-of-magnitude
  survey suitable for a shape decision, not a calibration table (contrast T-547's replicated methodology).
- No search-engine bookkeeping cost is measured (hashing, LRU eviction, beam ranking) because no code exists
  yet — the §4 "evaluations afforded" table is therefore an **upper bound** on what `quick`/`standard`/`deep`
  can do; real engine overhead will only shrink it further.
- Jetson numbers are the explicit non-goal of this ticket (MAUTO M-13); every `s`/`ms` figure above should be
  treated as this workstation's floor, not the handheld's number.
