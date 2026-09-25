# 28 · Automatic front-end gain management (T-945)

**Status:** design note for T-945 (M2-hardening). Written before the implementation and corrected
against it. PROVISIONAL like every ADR here.

**Touches:** **ADR-0005** (survey/dwell scheduler — the gain loop is a *second* control policy over
the same one front end, and the two must be arbitrated), **ADR-0014** (a gain change is a provenance
change and so cuts an interval in the IQ ring), **C01** (source abstraction: capabilities, named
gain stages, the ADC clip count), **C05** (calibration: the per-band clip-free gain table and the
−6…−10 dBFS peak target). It **answers C05's open question** — "who owns the real-time
clip-avoidance gain loop: C01, C03 or C05?" — with: *none of them owns a clip-avoidance loop,
because clip avoidance is the wrong objective*; see §2. The loop is a control-plane policy beside
the scheduler (`hk_core::gain`), actuated through the one gated `DeviceAction` path
(`hk_api::gain`).

## 1. The finding

2026-09-25, the explorer, live HackRF One in San Francisco, stock antenna, FM band
(`~/.hackriff-ops/explorer/journal-20260925.md`, window 03:43–06:43):

- Every clip was flagged `overload: true` at LNA 32 / VGA 30 / amp on (73 dB total). **The app
  flagged it and nothing reduced the gain** — there is no front-end AGC anywhere in the tree
  (`grep -rn 'agc' crates/` finds only the *audio* AGC in `hk-demod`/`hk-blocks`, which is a
  post-demod level control and has nothing to do with the ADC).
- One manual retry at amp off / LNA 24 / VGA 24 (48 dB) made the decode **worse**: 98.9 MHz went
  from 106 RDS groups to 0, and 98.1 MHz from 3 to 0.

Those two facts together are the whole design. The first says a flag without an actuator is not a
feature. The second says the obvious actuator — "clipping, so turn the gain down" — is *wrong*: it
walked off the working point and took the decode with it.

## 2. Why the overload flag is not the policy

The flag says the ADC pinned. It does not say whether pinning cost anything, and on an 8-bit front
end with no preselector the alternative to a little pinning is often quantisation noise, which costs
more. The two failure modes sit on opposite sides of one axis:

- **Too much gain:** the strong stations saturate, hard limiting generates intermodulation across
  the whole window, and the weak station you were decoding drowns in the receiver's own products.
- **Too little gain:** the weak station falls towards the LSB and the receiver's own post-gain noise
  (`hk_core::source::mock::device_noise_codes2`, Friis-referred to the ADC) dominates. Nothing
  clips, every flag is clean, and the decode is dead.

So there is an interior optimum, it is a **property of the scene and of what you are trying to
decode**, and it cannot be read off a flag or a peak-dBFS target. It has to be *measured*, from the
processed output — which is the project's standing rule ("Tune from the processed output", CLAUDE.md,
"Further goals"; "Modulation, bandwidth, squelch and AGC are estimated from the signal, never picked
manually", workflow #5).

Measured on the mock SDR's calibrated gain model, for a front-end-limited dense-FM scene (two strong
stations at −12 dBFS with a third-order product landing on a −34 dBFS target, the target's RDS the
thing being worked), with one second of dwell per state:

| total gain | clip fraction | `overload` | channel SNR | CRC-valid RDS groups |
|---|---|---|---|---|
| 0 dB | 0 | false | — (no channel) | 0 |
| 30 dB | 0 | false | 5.7 dB | 0 |
| 36 dB | 0 | false | 10.6 dB | 0 |
| 42 dB | 0 | false | 14.8 dB | 4 |
| **44 dB** | **0** | **false** | **18.4 dB** | **8** |
| 50 dB | 1.4e−3 | **true** | — (clipped) | **8** |
| 54 dB | 2.6e−1 | true | — | 0 |
| 73 dB (the explorer's state) | 8.6e−1 | true | — | 0 |

Three things in that table are the design:

1. The best decode is **interior**: both ends of the ladder decode nothing.
2. A state flagged `overload: true` (50 dB, 1.4e−3 of components pinned) decodes **as well as the
   clean one**. A policy that treats the flag as a veto throws that state away for nothing; the
   explorer's live 98.9 MHz result (106 groups *while flagged*) is the same observation on real air.
3. **SNR is the guide and the decode is the judge.** Between 30 and 44 dB the decode metric is flat
   at zero and only SNR moves, so SNR is what tells the search which way to walk; from 42 dB up the
   decode separates states SNR cannot (42 and 44 dB differ by 4 groups). A single scalar objective
   would lose one half or the other.

## 3. What the loop is

A **policy object off the sample path** (like the scheduler, ADR-0005 "Consequences"), in three
pieces:

- `hk_core::gain` — pure, device-generic, no I/O: the state space, the quality record, the
  comparator, the search. Unit-testable without a device, which is where the field evidence of §1 is
  pinned as a regression test.
- `hk_api::gain::GainManager` — the actuator. Each probe is one `LiveControl::set_gains`, i.e. one
  `DeviceAction::Gains` through the one `DeviceGate` (T-343), so a probe serialises against retunes
  and against another device action exactly as a user gain change does, and is recorded against the
  same `device_id` as the frames it produced.
- the caller supplies the **measurement**: a closure that, after the settle gap, collects the dwell
  and returns a `GainQuality`. The loop never measures anything itself, because what "quality" means
  depends on what is being worked — RDS group rate for an FM station, CRC-valid frame rate for a
  packet mode, channel SNR when nothing is being decoded.

### 3.1 The state space is a ladder derived from capabilities

`SourceCapabilities::gain_stages` is the only input: named stages with `min_db`, `max_db`,
`step_db`. The cross product is not searchable (HackRF One: 6 × 32 × 2 = 384 states, and a probe
costs a settle plus a dwell), so the policy searches a **one-dimensional monotone ladder** through
it, `GainLadder`:

- stages whose one step spans their whole range (an on/off RF amplifier is exactly this — the
  capabilities descriptor already models it that way, `GainStage::quantise`) are **engaged last**,
  at the top of the ladder: a binary stage is the least controllable thing in the chain, so it is
  the last resort for more gain rather than something the search steps through early.
- the rest ascend **balanced**: each rung raises whichever stage is furthest below its own maximum
  *as a fraction of its range*, ties to the finer step. With no chain-order information in the
  capabilities descriptor, spreading gain across stages is the standard low-risk distribution, and
  it reproduces the practical HackRF advice (move LNA and VGA together; leave the amp off unless you
  need it) without naming a HackRF anywhere.
- the ladder is therefore strictly monotone in total gain, has exactly Σ(steps per stage) rungs
  (HackRF One: 37, spanning 0…113 dB) and each rung is one stage-step from the last.

**What the ladder deliberately does not decide:** *which* stage should carry the gain for best
linearity. That is chain-order physics the capabilities descriptor does not express (the HackRF
declares `lna, vga, amp`, but the amp is physically *first*), and it is not guessed: the fine phase
(§3.3) walks the finest stage locally, so where a distribution matters the search finds it by
measurement. The mock cannot exercise this at all — its gain model is a function of *total* gain only
— so the distribution rule is HIL work (T5), and this note claims nothing about it.

### 3.2 The score: the processed output wins, the flag is a tiebreak

`GainQuality` records what one dwell at one state measured: the **state it was measured under**
(mandatory), clip fraction, the device's `overload` flag, peak dBFS, channel SNR, noise floor, and
an optional `DecodeQuality { metric, rate_per_s, valid_fraction }` — the processed output, named, as
a rate so dwells of different lengths compare.

States are ordered by a **bucketed lexicographic** tuple (a total order, so the argmax does not
depend on probe order):

1. **decode rate**, bucketed by `decode_margin_per_s` — a quarter of a group per second by default,
   so the search does not chase counting noise;
2. **SNR**, bucketed by `snr_margin_db` (1 dB);
3. **clip fraction**, bucketed by **decade** — this is where the overload evidence finally acts, and
   it acts as a tiebreak between states whose *output* is indistinguishable, never as a veto;
4. **lower total gain** — less IMD risk, less heat, and a deterministic tiebreak.

A term that was not measured (`None`) sorts **below** any measured value: nothing said is not a
value (the T-325 rule). A state whose decode could not be measured therefore never beats one whose
could, and a run in which nothing decodes falls through to SNR by construction.

One hard rule sits outside the comparator: a state whose clip fraction exceeds `clip_ceiling`
(default 0.10 — a tenth of all components pinned) is marked **unusable** and is never committed even
if it scores best, because at gross saturation the ADC is a limiter and nothing else measured in
that window can be trusted (the same judgement the inventory already makes when it refuses an
overloaded window, `hk_pipeline::inventory`). A committed state that is *flagged* but under the
ceiling is committed **and said so** in the report.

### 3.3 The search: two monotone passes, no rung twice, bounded

1. **Coarse.** From the state in force, probe ladder rungs at `coarse_stride_db` (8 dB) spacing,
   nearest-first, in the direction the first measurement suggests (clipping → down; otherwise up),
   then the other direction, up to `coarse_probes`.
2. **Fine.** Around the coarse best, probe states reached by moving the **finest** movable stage one
   step at a time, alternating sides, up to `fine_probes`. This is the phase that resolves the knee:
   the ladder's rungs are up to 8 dB apart where a coarse stage moves, and the working window in §2's
   table is 8 dB wide.
3. **Commit** the argmax and report.

Termination and non-oscillation are structural, not tuned:

- a rung (and a fine state) is probed **at most once per run** — the visited set is the search, not
  an optimisation;
- the run is bounded by `max_probes` (12 by default: ≈ 12 × (settle + dwell));
- an upper-bound prune: above a state that is *both* clipping past the ceiling *and* worse than the
  best so far, higher-gain rungs are not probed at all;
- after committing, the controller **holds** — it issues no further device action until something
  `trigger()`s a new run, and a trigger inside `min_rerun_s` (30 s) of the last commit is refused.
  A re-run that lands on the same state doubles that interval, bounded, so a scene that has nothing
  better to offer stops asking.

`GainTrigger` names why a run started (`Manual`, `Retune`, `QualityLoss`, `Overload`,
`PeriodicReview`) and the reason is in the report.

### 3.4 Off by default

`GainPolicy::default()` is `enabled: false`, and a disabled controller answers `GainStep::Disabled`
and issues no device action, ever. This is a policy that moves the radio on its own; it ships dark
until HIL proves it on real air (the M2-hardening exit for T-945 is the mock; T5 is the proof).

### 3.5 Honesty rules

- **A measurement that cannot name its state is not evidence.** `observe` takes the state the dwell
  was actually taken under and refuses a quality that does not answer the outstanding probe.
- **The applied state, not the commanded one.** `set_gains` returns what the device took after
  quantisation; the probe is scored and recorded against *that*, and the report shows both when they
  differ (the rule the RTL driver already follows for its 29-step table).
- **A dwell spanning a discontinuity is not evidence.** The settle gap exists because the source
  contract drops samples across a gain change and flags the boundary; the caller must not score
  across it.
- **The report says why.** `GainReport` carries every probe (state, quality, score, and whether it
  was flagged/unusable/pruned), the committed state, the runner-up, and a one-line explanation
  naming the metric that decided, in the form the panel can print verbatim: *"committed LNA 24 /
  VGA 20 (44 dB): 8.0 rds-groups/s, best of 9 probed states; the state we started at (73 dB) gave
  0.0 and pinned 86 % of components; 50 dB decoded as well but is flagged overload and 6 dB hotter."*

## 4. Arbitration with the scheduler (ADR-0005)

The front end is one resource with one attention (ADR-0005). A gain run costs `max_probes` dwells,
during which the window is *not* being used for survey or POI dwells, and during which detections
carry provenance that changes every probe. Three rules keep that honest, and only the first is
implemented by T-945:

1. **Every probe is a `DeviceAction::Gains` through the `DeviceGate`**, so a gain run cannot
   interleave with a retune, and a scheduler hop in flight makes the probe fail cleanly with
   `device_busy` rather than racing it. *Implemented.*
2. A gain run should be *scheduled*, as a dwell of its own, rather than fired from a detection
   callback — the scheduler owns when the window is spent. *Not implemented: the controller is
   driven by its caller, and `hk serve` does not drive it yet (§6).*
3. Detections made during a run are made under deliberately-wrong gains for part of it. They are
   already marked: each probe cuts a provenance interval (ADR-0014), `overload` is on the interval,
   and `hk_pipeline::inventory` already refuses to confirm an emitter from an overloaded window. No
   new suppression is proposed; the coverage map's honesty machinery already covers it.

## 5. What the mock proves, and what it does not

**Proves** (offline, deterministic, in `crates/hk-pipeline/tests/gain_manage_overload.rs`): a scene
that starts overloaded with no decode converges, through the real device interface and the real
gate, to a state whose decode is strictly better; the search visits no state twice and stays inside
its probe budget; with the policy disabled — today's behaviour — nothing moves and nothing decodes.

**Does not prove**: (a) any per-stage distribution claim (§3.1 — the mock's model is a function of
total gain); (b) real IMD structure (the mock's nonlinearity is ADC saturation of the summed scene,
which does generate intermodulation, but not the mixer's or LNA's own compression); (c) that
`min_rerun_s`, the probe budget or the dwell are right for real air — those are HIL numbers.
Nothing here is a substitute for the physical fix at the explorer's site: an FM-band antenna plus a
notch/preselector (an existing user item), which is what actually restores the other stations'
RDS.

## 6. Not built by T-945

Named so the follow-ups are visible rather than implied:

- **No route and no UI.** `GainReport` is a Rust value; no `/api/control/gain_policy`, no panel, no
  persistence of past runs. A user cannot turn this on from the UI, which is consistent with §3.4.
- **`hk serve` does not run it.** Nothing constructs a `GainManager` in the composed pipeline yet;
  ADR-0005 §4.2 arbitration is the prerequisite.
- **No learned per-band table.** C05's "per-band clip-free gain tables" would let a run start from
  the last good state for that band instead of from wherever the radio was. That is the obvious next
  step and needs a store.
- **No HIL.** T5 on the bench rig, and the explorer's own FM-band scene, are the real proof.
