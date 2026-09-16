# ADR-0017 — Signals as time–frequency regions: presence intervals, a view-scoped Explore inventory, and a separate History surface

**Status:** PROVISIONAL (documents only, no code). The *model* below is **settled by the user** (CLAUDE.md, "Signal & inventory model (invariants, from the user 2026-09-16)") and is **recorded, not proposed**. What awaits sign-off is **§9's staged implementation plan**, not the model.

**Source:** the user, from live Explore testing on staging, 2026-09-16 (T-253). Found with it: T-250 (82 near-duplicate candidates around one FM signal), T-251 (seen-counts of 38 → 582,500 per hour with nothing ageing out), T-254 (the 902–928 MHz burst scene).

**Touches:** docs/07 §2.9/§2.11/§2.20 and new §2.27; docs/14; ADR-0011 §8.5; ADR-0012 §7; ADR-0013 §3.1/§3.3/§4.2; ADR-0014; ADR-0015 §5/§6/§11/§12; ADR-0016 §2/§5. Capabilities C09, C10, C26, C27, C39, C40.

---

## Context

The inventory answers **"what has ever been seen here"** but Explore presents it as **"what is here now"**. Those are different questions, and the gap between them is not cosmetic — it produced every symptom the user hit in one sitting:

- A candidate for a signal that stopped hours ago sits in the live list, indistinguishable from one transmitting right now.
- `count` climbs without bound (38 → 582,500/h) because it is the only place "still here" can be written down.
- 82 rows crowd one FM station because nothing says the rows describe *the same emission over the same minutes*.
- A one-off burst — the whole point of the 902–928 MHz ISM playground — has nowhere to live at all. It is either a permanent live candidate (wrong) or nothing (worse).

The root cause is a missing dimension. `Detection` has `t_start`/`t_end`; `Track` has `t_start`/`t_end`; **`Emitter` has `first_seen`/`last_seen`/`count`** — a *hull* and a *counter*, not an extent. A signal that fired once at 09:00 and once at 17:00 has an eight-hour "extent" that is 99.99 % silence, and a counter that only grows.

That hull is not just displayed; it is **queried**. `crates/hk-model/src/repo/inventory.rs:53`:

```sql
WHERE f_lo BETWEEN ?1 AND ?2 AND f_hi >= ?3 AND last_seen >= ?4 AND first_seen <= ?5
```

So `GET /api/inventory?t0&t1` already exists and already **cannot** answer "what was on the air in this window": any emitter whose hull straddles the window matches it, however long it has been silent. The time filter is the right shape with the wrong predicate.

---

## Decision summary

| Question | Answer |
|---|---|
| What is a signal? | A **time–frequency region**: centre, width, **and a time extent**. Bursts and chirps are first-class. |
| Where does the time extent live? | On the **presence interval**, not on the emitter. An Emitter is an identity that **owns an ordered set of disjoint intervals**. |
| Do we need a new table? | **No.** `emitter_observation` (`t_start`, `t_end`, `count`, `f_center`) already *is* the interval set. It is unnamed, unindexed for time, and unsurfaced. |
| What is `first_seen`/`last_seen` then? | The **hull** of the presence track. Never the extent. Never displayed as a duration. |
| What is `count` then? | A lifetime total, valid **only** in History. **Excluded from every liveness decision and from live-list ranking.** |
| Explore Candidate list | **Window-scoped**: only rows with a presence interval intersecting the viewed waterfall window. |
| Explore Confirmed list | **Always listed** (a catalogue of verified emitters, invariant 3) — each carrying its own time-presence track and a liveness state. |
| History | A **separate surface**: the durable catalogue of every event, one-offs included. Nothing is ever deleted to make the live list correct. |
| Why does a box grow? | Because its **open interval's `t_end` advances**. That *is* the accumulator, correctly modelled. |
| Why does a row leave the live list? | Because **no interval of it intersects the window** — a view consequence, not an expiry timer. |
| Decode vs Listen | **Per-reader, not per-pipeline**: a live-edge reader obeys ADR-0011 §8.5; a bounded-region reader is incremental. One pipeline has one reader, so it is one or the other. See §6. |
| Migration needed? | **None for stages TM-1…TM-4.** One **index-only** migration 0012 at TM-5. |
| ADR-0016 arbitration | **Unchanged.** Its *input set* gains an optional time predicate, as an additive projection (§7). |
| Cluster contract | **Unchanged, and made more precise**: membership counts **emitters**, never intervals; "appearance" becomes a defined object (§7). |

---

## 1. Invariant 1 — a signal is a time–frequency region

**The atomic object is already right and should be said out loud.** A `Detection` (docs/07 §2.9) carries `t_start`, `t_end`, `f_lo`, `f_hi`. It is a time–frequency *box*, and it always has been. Nothing in this ADR changes it, because nothing needs to.

What was missing is the object above it.

### 1.1 The presence interval

> **Presence interval** — a maximal span during which one emitter was continuously on the air, within the detector's ability to tell continuity from gaps: `{emitter_id, t_start, t_end, f_center, count, open}`.

- **It is already materialised.** `emitter_observation` (migration 0001, `crates/hk-model/src/repo/migrations/0001_init.sql:243`) carries exactly `(source_kind, source_id, emitter_id, count, t_start, t_end, measurement, f_center)`, keyed per sighting source. A track is a row; a decode sighting is a row. The API already leaks a glimpse of it as `recurrence.recent[]`.
- **Intervals of one emitter are disjoint after normalisation.** Overlapping source rows (a track and a decode of the same minutes) merge into one interval; that is what the existing "overlapping observations not counted twice" rule (docs/07 §2.11) already says, now given a name.
- **`open` is derived, never stored.** An interval is open when `now − t_end ≤ idle_gap`, closed otherwise. The idle gap is a tuning parameter, and storing a decision made under one parameter value is exactly the mistake docs/07's first rule forbids (measurement vs interpretation). A closed interval re-opens only if the gap parameter changes — which is correct, and re-derivable.
- **Closing is not decay.** It is a measurement fact: evidence stopped arriving. It is permanent and it is History.
- **Revival appends, never duplicates.** A returning signal that entity resolution places on the same emitter gets a **new interval on the same `emitter_id`** — automatically, because `emitter_observation` is keyed by `(source_kind, source_id)` and a new track is a new source row.
  - **Corrected by TM-5 (T-262): only the storage half came free.** The clause that matters is *"that entity resolution places on the same emitter"*, and on the user's own pair it did not. The returning FM station failed `Fingerprint::compare` on **burst length alone** — 0.68 s against 0.37 s, normalised error 1.86 — and minted a second **emitter**, which is the duplicate the user reported. Burst length, duty cycle and period are statistics of the window each row was watched over (305 s and 59 s; 19 observations and 6), not properties of the emission, so when two observations' presence intervals are **disjoint** they are excluded (`Fingerprint::compare_across_silence`). Everything describing the emission — centre, bandwidth, family, symbol rate, deviation, hop behaviour — still applies. This is exactly the exclusion T-250 made at the *relation* layer, applied at the *resolution* layer, where the row was actually being minted. **Cost:** two emissions sharing a channel, a bandwidth and a family that never overlap in time can no longer be told apart by duty cycle alone — the ISM case, which TM-9 measures.

### 1.2 The box

The **box** drawn for a row over a view window `[t0, t1]` is

```
(f_lo .. f_hi)  ×  (interval ∩ [t0, t1])    for each interval that intersects the window
```

A persisting signal's box grows because its open interval's `t_end` advances with the live edge. A one-off burst's box is a few milliseconds tall and stays that way forever. No carrier is required, no stable frequency is required, and nothing is forced into a "steady emitter parked on one frequency" shape.

### 1.3 Honest limitation — chirps

**Three different limits hide under "a chirp is drawn as a box", and they do not bite in the same places.** This section used to name only the first, which made the whole thing read as though a swept emission were merely un-drawable. T-255 and T-294 measured where each one applies.

**(a) Representational — always.** The model gives one `(f_lo, f_hi)` per detection and per interval, so a 10-second chirp's box is the **bounding box of its sweep**, with the per-detection ladder underneath it. That is correct and it is first-class — a 10-second chirp is one signal with a 10-second extent — but the box is a rectangle where the truth is a diagonal. Making the box a **polyline `f(t)`** (an ordered centre track per interval) is a later refinement. It is named here so it is not discovered later, and it is **explicitly out of the plan in §9**. `Track`'s `hop_set` and `body` are where it would go.

**(b) The analysis frame — whenever a sweep completes inside one frame.** The detector does not see the IQ; it sees `K` averaged periodograms. The cost of the rectangle is exactly `symbol_duration / analysis_frame`, so it **grows with the spreading factor**, and at the standard US915 uplink rate it is total. T-255 measured this blind, through the mock device:

| | SF9/125 kHz (US915 uplink DR) | SF12/125 kHz |
|---|---|---|
| frames per symbol | **1.0** | 8.0 |
| detected centre spread / channel | 0.44 | **0.84** |
| box vs instantaneous occupancy | **4×** | 32× |
| verdict | sweep **hulled** into one rectangle | sweep **resolved** into a ladder |

At SF9, `T_sym = 2⁹/125 kHz = 4.096 ms` and the detection frame at 500 kS/s is `K·N/fs = 4·512/500 kHz = 4.096 ms`. Exactly one. Inside a frame the emission covers its **whole** channel, so a frame-based box cannot be narrower than the channel and there is no ladder underneath it to resolve: the rectangle is forced, not chosen. **At and below one frame per symbol this is an observability limit of the detector's input, not a representational one** — the polyline of (a) would recover nothing here, because the centre track it wants to draw is not present in that product.

**(c) Genuine observability — only below about −3 dB in the channel.** It would be easy, and wrong, to stop at (b) and rule that "swept emissions below one frame are detected as regions but can never be characterised as chirps". T-294 measured that claim and it is **false**. The IQ in the ring is not the spectrum frame: a lag-product (delay-multiply) estimator reading **one 4.096 ms frame** recovers the sweep rate to 1.6 % and separates a LoRa SF9 chirp from the three species T-255's scene puts beside it — a band-limited wideband burst, a steady carrier and a 2-FSK burst — at **96–99.5 %, against 0–2.2 % on the controls, from +18 dB down to −3 dB in the channel**. It falls to 27.8 % at −6 dB and 0.5 % at −9 dB. T-255's scene runs its chirp at 12 dB. `hk_dsp::chirp` is that measurement, carrying the four-species table and the SNR floor as its own tests.

**What the system claims today (T-297).** The wiring question (a) and (b) left open is answered: a **normal run now characterises a swept region**, and the product a characteriser reads is the **IQ in the ring, channelised to the region's own measured band** — never the detector's input, which for case (b) does not contain the sweep at all. The seam is a chain, `hk_pipeline::chains::sweep`, on its own thread behind its own ring reader and gate cursor, so the capture thread and the DSP readers are untouched (ADR-0001 S1).

It needed one new trigger. `Trigger::ConfirmedTrack` *selects* — the first matching spec wins and the rest never run — which is right for decoding and wrong for measuring, and it is exactly what left the swept region unreachable: measured on T-255's SF9 scene, that region matches `fsk-bursts`, never reaches its four member detections, and closes `unmatched` with **no chain at all**. So `Trigger::EveryTrack` chains attach *beside* the selected decode chain, capped by their own node spec and counted apart from it.

What the run writes is a `sweep_rate_hz_per_s` field on the region's `EmissionFeatures` snapshot: **evidence about a region, never an identity**. It says the region sweeps at rate α; it does not say LoRa, radar or anything else, and it sets no family, status or lifecycle — the rule §7 and ADR-0016 §5 already impose on matches, clusters and classifications. A region that cannot be characterised — the two lags failing to peak and agree, which is what a carrier, a wideband burst, a 2-FSK burst and any sweep below about −3 dB all do — has **no field written at all**. Absent means *not measured*, so nothing here ever records a fabricated rate or a zero.

The distinction this amendment exists for still holds, and now cuts the other way too: a system must not claim a measurement it did not make, **and** must not record a limit its own measurements refute. (c) was never the reason a swept region went uncharacterised; (a) and (b) were, and only the second of those was ever a wiring choice.

**Why not simply use a finer frame.** Three reasons, each measured rather than argued:

1. **The frame at 500 kS/s is already the finest this geometry produces.** `detection_resolution` takes `fft_len = next_pow2(fs / 5 kHz)` clamped to 512..4096 and `K = round(fs · 2.5 ms / fft_len)` clamped to 4..10. At 500 kS/s both land **on their floors** (128 → 512, and 2.44 → 4). A finer frame means moving the clamps, not tuning within them.
2. **The cost is not CPU, it is the false-alarm design.** Halving the frame means `K = 2`. The STFT does the same work on the same samples, so there is **no change on the capture thread and none in the DSP thread's transform budget** — but detection runs at 0 % overlap precisely so that the floor tracker's `n_avg_effective` *is* `K`, and the detector's `Gamma(n)` thresholds hold their design Pfa only for `n ≥ 4` (`hk-pipeline/src/config.rs`). The finer frame is paid for in stated false-alarm rate, which is the one currency this change may not spend.
3. **It never ends.** Each step down in spreading factor halves `T_sym` *and* doubles the chirp rate, so the frame required recedes for ever (SF7 is 1.024 ms). The estimator in (c) moves the other way: a faster sweep puts its lag-product tone in a **higher** bin and is easier to measure, not harder. Chasing this with the frame is chasing an asymptote; reading the IQ is not.

**Assertable, not prose.** T-255's fixture carries `sweep_polyline`, so which limit applies is computed from truth rather than from the spreading factor. `tests/e2e/tests/acceptance/t255_lora_chirp.rs` reads the run's own frame from `detection_resolution` at the fixture's sample rate, measures the emission's frequency excursion inside one such frame from the polyline, and asserts both the boundary — a symbol that fits inside one frame sweeps most of its channel there, one spanning four or more frames sweeps a fraction — and its consequence in the system's own output: **a box may not be narrower than the excursion it contains**.

---

## 2. Invariant 2 — Explore reflects only the viewed waterfall window

### 2.1 What has to change, exactly

Three things, and only three:

1. **The predicate.** `last_seen >= t0 AND first_seen <= t1` is a **hull** test. It must become a **presence-interval overlap** test: `EXISTS (SELECT 1 FROM emitter_observation o WHERE o.emitter_id = e.emitter_id AND o.t_end >= t0 AND o.t_start <= t1)`. This is the single change that makes the whole model work. It needs an index (§8).
2. **The caller.** ADR-0013 §3.3 states that while LIVE "the inventory is unbounded in time". That sentence **is** the observed bug. Live must send `t0 = now − waterfall_span`, `t1 = now`. Reviewing already sends a window.
3. **The ranking.** The live list must not sort by `count`. It sorts by in-window on-air time, then SNR.

### 2.2 Candidates are window-scoped; Confirmed are not

This is a deliberate asymmetry and it comes straight from invariant 3.

- **Candidate** = *a hypothesis about energy in the current window*. Outside the window there is no energy to hypothesise about, so the row simply is not in the list. There is nothing to expire.
- **Confirmed** = *a verified real emitter, carrying its own time-presence track*. It is a catalogue entry. It stays listed whether or not it is transmitting right now, and its liveness state says which.

Without this asymmetry, stage TM-3 would make the user's confirmed stations vanish from Explore whenever they went quiet — a live-visible regression dressed as a fix.

### 2.3 Liveness

Every row carries a **liveness** state, derived, never stored:

| State | Meaning |
|---|---|
| `live` | an interval is open at the live edge |
| `ended` | its latest interval intersecting the window is closed; the row carries `ended_t_s` |
| `absent` | no interval intersects the window (Confirmed rows only — a Candidate in this state is not listed) |

"Ended 4 minutes ago" is the sentence the product currently cannot say. It needs no new state machine — only the interval's `t_end` and the clock.

### 2.4 Scrubbing

Scrubbing the timeline sets `[t0, t1]` and **re-derives** the lists and the boxes from the same query. Recent history comes from the IQ ring (ADR-0014, 30 min on staging) for waterfall detail below the tile resolution, and from `/api/history` tiles above it; the **lists** come from the interval query, which is backed by SQLite and reaches as far back as retention allows. Scrubbing is therefore never "replay the detector" — it is one indexed range query, which is why it can be interactive.

---

## 3. Invariant 3 — Candidate / Confirmed / History

| Surface | Object | Scope | Question it answers |
|---|---|---|---|
| **Candidate** (Explore) | hypotheses | the viewed window | "what energy is in front of me, and what might it be?" |
| **Confirmed** (Explore) | verified emitters + presence track | all, with liveness in-window | "what real emitters do I know here, and which are on air?" |
| **History** (separate surface) | **events** — every interval, one-offs included | all time, any region | "what has happened here?" |

**History is a surface over the same rows, not a second store.** An event in History is a presence interval plus its emitter's identity, classification and explanations. That is why nothing needs to be deleted from the live list to make it correct, and why a one-off burst can be catalogued without ever being a live candidate for more than the seconds it existed.

---

## 4. Invariant 4 — fast, continuous, self-cleaning

Four distinct mechanisms, often conflated:

| Mechanism | What it does | Where it lives |
|---|---|---|
| **Merge** | collapses near-duplicate rows describing one emission | `resolve_overlaps` / `emitter_relation` (T-219, T-250) |
| **Interval close** | records that evidence stopped | derived from `t_end` + `idle_gap` (this ADR, TM-5) |
| **Window scoping** | removes a stopped signal from the live list | the query predicate (this ADR, TM-2/TM-3) |
| **Decay** | lowers a *hypothesis's* confidence as its evidence ages | candidate confidence (T-251, TM-6) |

Only the last is decay, and it is the smallest of the four.

**New merge evidence the time model hands T-250 for free:** skirt fragments of one FM station have intervals that **start and stop together**; two genuinely distinct adjacent stations do not. **Co-onset/co-offset of presence intervals** is therefore a distinguishing signal that needs no bandwidth estimate — which matters, because T-250's live hypothesis is that under-estimated bandwidths stop the band-overlap rules firing at all. It fits in `emitter_relation.detail` as JSON with no schema change (migration 0008 is append-only and already carries `t`). Offered as a strengthening, not a requirement.

---

## 5. Conflict (b) — a growing box **is** the accumulator

The user saw seen-counts of 38 → 582,500 per hour with nothing ageing out. **That counter was not wrong; it was homeless.** "This signal is still here" is real information, and `count` was the only column that could hold it, so it held it in the worst possible form: monotonic, unbounded, unscoped in time, and indistinguishable between "one transmitter for an hour" and "582,500 separate events".

A box with a growing time extent is the same accumulation, correctly modelled. **T-251 and this ADR are one design.** How they compose:

- **What accumulates:** the open interval's `t_end`. Bounded by the live edge, scoped to a window on read, and directly meaningful ("4.2 s on air across 17 events").
- **What decays:** a Candidate's **confidence in its hypothesis** — a function of *time since its latest interval closed*, never a per-tick decrement. A per-tick decrement is the same pathology inverted: a number that moves for reasons unrelated to evidence.
- **What is retained:** everything. Intervals, detections, tracks, links, classifications and relations are append-only and permanent. **Decay never deletes and never touches History.** Confidence is a ranking, not a lifetime.
- **What revives:** a returning signal appends a **new interval to the same emitter** and its confidence recovers from the new evidence. No duplicate row, because entity resolution and `emitter_observation`'s keying already do this. Where a T-219 `emitter_relation` had deferred the row, the existing append-a-revocation-row mechanism un-defers it — also already built.
- **What is removed:** `count` from every liveness decision and from the live list's default sort. It survives as a lifetime total in History, where a monotonic counter is exactly right.

**The consequence T-251 should know about:** once Explore is window-scoped (TM-3), **most of what T-251 was asked to fix is already fixed, without any decay existing.** A candidate whose signal stopped hours ago is not decayed out of the list; it is *not in the window*. Decay is left owning one genuine case — a signal that stopped **inside** the viewed window. There the row *should* still be listed (it happened, and its box is on screen), and what it needs is not expiry but an honest liveness state and a lower rank. That is a far smaller and far safer piece of work than the ticket implies, and it is sequenced after TM-5 in §9 for that reason.

**Measured at TM-3** (T-260, 2026-09-16, the user's own staging database, the same 99.6–102.0 MHz scene): 21 candidate rows holding 24 presence intervals between them. Scoped to the waterfall's own window (512 rows ÷ 25 rows/s ≈ 20.5 s) the Candidate list collapses to **1** row — the single emitter with an interval open at the live edge (101.4614 MHz, 631 s continuous, `count` 3536). Every other candidate's newest interval is 60–614 s stale, and 9 of the 24 intervals have zero duration. **No decay logic exists and none was needed:** window-scoping alone removed 20 of 21 rows, which confirms this section's prediction. It also *overshoots* the ~4–7 estimate, and the cause is the two known gaps rather than the window: the skirt fragments that should have merged into a few live rows (T-280, under-estimated bandwidth — the merge never fires) instead age out of the window one by one, because their intervals are never extended or revived (TM-5/T-262). Until those land, the Candidate list reads quieter than the band actually is, which is the opposite failure to the one reported and a much safer one.

---

## 6. Conflict (a) — invariant 5 versus Listen

### 6.1 The apparent conflict

Invariant 5: *decode operates on a captured region and extends with it; live decoding extends the time extent and decodes only the newly-arrived part, never re-decoding what is done.*

ADR-0011 §8.5 and ADR-0015 §12: Listen is a pipeline with an audio sink, declaring `liveness: {mode: "live-edge", max_backlog_s: 0.6}`. When it falls behind it **seeks to the live edge**, counts the skip and flags `DISCONTINUITY`. *"Latency is a contract for audio and merely a statistic for decoding."*

These look like opposite lifecycles: one promises to process every sample exactly once; the other promises to throw samples away to stay current.

### 6.2 The ruling

**The conflict is apparent, because "decode" names two different objects. The policy is a property of the *reader*, not of the pipeline, the recipe or the signal.**

> **Rule L (latency wins at the live edge).** A pipeline whose input reader is attached at the live edge obeys ADR-0011 §8.5 **unchanged**. It never accumulates a backlog, it skips forward when it falls behind, and every skip is counted and flagged. This governs Listen and anything else a human is consuming in real time.
>
> **Rule I (the region is the unit of work).** A pipeline whose input reader is a **bounded region** of the IQ ring processes `[t_start, t_end]` exactly once. Extending the region enqueues only `[old_end, new_end]`. Nothing already processed is re-processed. This governs `POST /api/analyze` (ADR-0015 §5), the burst path (§6), and any user-captured region.
>
> **A pipeline has exactly one input reader, so it is exactly one of the two. The model forbids a pipeline being both.**

**Invariant 5 governs decode and analysis. It does not govern Listen.** Listen is not a decode of a captured region; it is a live audio tap that happens to share a runtime with decoders. Stating that plainly is the point of this section.

### 6.3 The specific case: a decode whose region is being extended while a listener is attached at the live edge

**It is two readers, therefore two pipelines, and the product must not fuse them.**

- The **listener** attaches to the live pipeline — ephemeral, `live-edge`, owned by its consumer (ADR-0015 §12.3). It produces the audio.
- The **region job** is a separate batch pipeline reading the bounded region from the ring. Extending the region appends work to it.
- They may share the emitter, the recipe, the channel and the tuning. They share **no reader**.

If they were fused, one of the two contracts breaks silently — always the worst kind:

- Fuse onto the region reader and the audio acquires the batch job's backlog. Everything still decodes; it just lags. ADR-0015 §12.10 names this "the highest-risk item in this plan", and no existing test would catch it.
- Fuse onto the live-edge reader and the `live-edge` skip silently discards the very samples the incremental job promised never to re-decode. The job would report complete coverage over a region with holes in it.

**What the user sees.** The region's box keeps growing as the batch job's coverage advances behind the live edge, while audio stays live. If the batch catches up to the edge, the UI may **hand over** — the region job ends and the live pipeline's output continues — but a handover is an **explicit, named state**, carrying a `DISCONTINUITY` if any samples fell between the two, never an implicit merge. A coverage bar that silently has holes in it is worse than no coverage bar.

### 6.4 The third case: a sibling decode output of an audio pipeline

ADR-0011 §8.9's FM recipe has an `audio` output **and** an RDS `messages` output hanging off the same `fm` node — one reader, two outputs.

> **A decode output that is a sibling of an audio output inherits the audio reader's liveness policy.**

So RDS text can have a gap when the audio skips to the live edge. That is correct and deliberate: the alternative is a pipeline that buffers for RDS's benefit and makes the audio late. The gap is recorded as a `DISCONTINUITY`, and **evidence `n` does not count skipped time** (the ADR-0015 §2.2 bits ladder must not be inflated by samples that were never read). A user who wants gapless RDS runs a region job over the ring — which is Rule I, a second pipeline, and exactly the §6.3 shape.

### 6.5 For the user

If the intent behind invariant 5 was that **listening should also replay from the scrub point** rather than skipping to the live edge, that is a different and larger product decision — "scrub-back audio" — and it is not what this ADR rules. Flagged in §11.

---

## 7. Cost in ADR-0016: arbitration and the cluster contract

### 7.1 Arbitration (user 0 > decoder 1 > lock-verified 2 > classifier 3 > track shape 4)

**The rank ladder is unchanged. Its input set gains an optional time predicate.** No ADR-0016 amendment is required; this is an additive projection.

The tension: today the emitter's current family is "lowest rank, latest among equals" over **all** rows. In a window-scoped view, should a row render the family decided from evidence inside the window? For ranks 3–4 (classifier, track shape) — arguably yes, they are statements about *this* emission. For ranks 0–1 (user, decoder) — emphatically **no**: a CRC-valid decode from yesterday still tells you what the thing is. Identity is time-invariant; shape is not.

Rather than rank by time, **report both**:

- **`family`** — arbitration over all rows, exactly as `effective_rank_sql!` computes it today. Unchanged, and it stays the value the `family` filter matches.
- **`family_in_window`** — the same arbitration restricted to rows whose `t` falls in `[t0, t1]`; `null` when the window contains none.

Explore shows `family`, marked *(from earlier)* when `family_in_window` is `null`. Cost: one optional `AND t BETWEEN …` in the rank query and one additive API field. The ADR-0016 §2 rule is preserved verbatim, and the honest case — "this is an FM station, but nothing in the last 30 s re-evidenced that" — becomes expressible instead of being silently asserted.

### 7.2 The cluster contract (a cluster is a **type** above emitters; evidence, never identity)

**Unchanged — and made more precise.** ADR-0016 §5's pitfall is *type ≠ instance*: two identical sensors share a cluster and stay two emitters. The time model introduces a third thing that must not be confused with either:

> **One emitter, many disjoint appearances, is still one emitter — and one cluster member.**

Get this wrong and a single intermittent sensor on a single device inflates a cluster's member count and trips the `≥ 3 member emitters` visibility gate on its own. So:

- **Cluster membership counts emitters, never intervals.** Stated as a rule, not left to inference.
- ADR-0016 §5's alternative gate — "≥ 3 **appearances** of one emitter across ≥ 2 sessions" — already anticipated this, using a word that had no definition. **An appearance is now exactly one presence interval.** The gate gets a precise meaning at no cost.
- **`EmissionFeatures` gains, not loses.** Its `period`, `duty cycle`, `burst length` and `TDMA period` fields become directly computable from the interval set instead of re-derived from raw detections.

**One genuine cost.** `EmissionFeatures` snapshots are written "when a field changes by > σ or **≥ 16 new observations** have been folded in" (ADR-0016 §5). An emitter that is a handful of one-off bursts may never reach 16 and so never gets a snapshot — which means it never gets a `SignatureMatch`, and the ISM playground is precisely where that hurts. **Fix: also snapshot on interval close.** One extra trigger condition, additive, no schema change. Noted here so T-201's implementation carries it.

### 7.3 ADR-0015 §11 (a candidate is a decode pipeline)

No conflict. `CandidatePipeline` rows hang off the emitter and are interpretation; presence intervals are measurement. The `channel` of a pipeline is a hypothesis about *where*, and the interval set says *when*. §11.4's duplicate/artifact resolution gains the co-onset/co-offset evidence of §4 — its `artifact-of` rule already requires "the row must have been seen only while the source was on air", which **is** an interval-containment test, currently approximated from hulls.

---

## 8. The data model and migrations

### 8.1 docs/07 deltas (applied by this task)

| Section | Delta |
|---|---|
| §2.9 Detection | Say out loud that it **is** the atomic time–frequency region; `t_start`/`t_end` are the time extent. No field change. |
| **New §2.27 Presence interval** | The §1.1 object: definition, disjointness, derived `open`/close, revival, retention, tests. Materialised by `emitter_observation`. |
| §2.11 Emitter | `first_seen`/`last_seen` re-specified as the **hull** of the presence track, explicitly *not* an extent and never displayed as a duration. `count` re-specified as a lifetime total for History, **excluded from liveness and from live-list ranking**. Adds derived window-scoped fields. Adds the disjoint-events ruling. |
| §2.20 Selection | Note that `t_lo`/`t_hi` already make a selection a time–frequency region; a timeline drag is the existing object. |
| §2.21 Classification | One sentence: `family_in_window` is an additive projection of the same rank; the ladder is unchanged. |
| §4 | The central region-over-time query gains presence intervals as its event source, beside Detection and Track. |

### 8.2 An emitter that is a set of disjoint events

Asked directly, answered directly: **it is one emitter**, as long as entity resolution says so (fingerprint, band, no distinguishing evidence). Nothing about disjointness argues for splitting it — a doorbell sensor is one device whether it fires once or a thousand times.

What makes it *feel* wrong is that its **hull is meaningless**. So the rule is about display and ranking, not identity:

- Never show the hull as a duration. "First seen 6 h ago" beside "count 582,500" is the current, dishonest rendering.
- Show `intervals` (n) and `on_air_s`. "17 events over 6 h, 4.2 s on air" is honest and is the same data.
- Rank by in-window on-air time, never by lifetime count.

### 8.3 Migrations

**Stages TM-1 … TM-4 need no migration at all.** This is the most consequential practical finding in this ADR: every field required to make Explore honest already exists in `emitter_observation` (migration 0001) and `detection` (migration 0001). The work is a predicate, an index, a projection and a caller.

**Migration 0012 (index-only), required at TM-5:**

```sql
CREATE INDEX idx_emitter_observation_time ON emitter_observation (emitter_id, t_start, t_end);
```

`emitter_observation` today has `idx_emitter_observation_emitter (emitter_id)` and `idx_emitter_observation_measurement (measurement, t_start)` — neither serves the window-overlap query, which is now on the interactive path at every scrub. `region_extent` (migration 0001) keeps bounding the range scan and needs a row for the new query shape, which is maintenance, not schema.

**Deliberately *not* in 0012:**

- **No `closed_at` / `close_reason` column.** Closure is derived from `idle_gap` (§1.1). Storing a decision made under one parameter value is the mistake docs/07's first rule forbids.
- **No decay/confidence column.** If T-251 must persist a confidence, it belongs in an **append-only** table like `emitter_lifecycle` and `emitter_relation`, never a mutable column on `emitter`. A mutable score is a second `count` waiting to happen.

**Existing migrations 0006–0011 are untouched.** 0008 `emitter_relation` is append-only and already carries `t`; the co-onset evidence of §4 goes in its `detail` JSON. 0010 `emission_features` needs the extra snapshot trigger of §7.2, which is code, not schema.

---

## 9. Staged implementation plan

Placeholder ids **TM-1 … TM-10**, per the `CP-*` / `LP-*` precedent (ADR-0015 §11.9, §12.11). **The coordinator mints real `T-` numbers at scheduling time.**

**The governing constraint: the user tests live. A stage that breaks the current Explore view is not acceptable.** Every stage below is independently landable and independently revertible, and each states what would break if it landed alone.

| Stage | What lands | Observably true after it | What breaks if it lands alone |
|---|---|---|---|
| **TM-1** | This ADR; docs/07 §2.9/§2.11/§2.20/§2.27/§4; docs/14. Documents only. | Nothing changes. The model is written down and the plan is signed off. | Nothing. This is the sign-off gate. |
| **TM-2** | **Backend, additive, no migration.** `/api/inventory` rows gain `presence {intervals, on_air_s, last_interval {t_start_s, t_end_s, open}, liveness, ended_t_s}` computed from `emitter_observation`. **Fix the `t0`/`t1` predicate from hull-overlap to interval-overlap** (`inventory.rs:53`, `:84`). Add `family_in_window` (§7.1). `docs/api.md` + `api_contract.rs` together (T-079 rule). | The API can say *"this stopped 4 minutes ago"* and *"17 events, 4.2 s on air"*. Every existing field keeps its name and meaning. | **Nothing in the UI.** The predicate fix changes which rows a time-filtered query returns, and the UI does not send `t0`/`t1` while LIVE today — so Explore is untouched. That ordering is deliberate: the semantics change lands before anything depends on it. |
| **TM-3** | **UI.** The LIVE inventory poll sends `t0 = now − waterfall span`, `t1 = now`. **Candidates window-scoped; Confirmed always listed** (§2.2). Rows render liveness. Default sort drops `count` for in-window on-air time. Amend ADR-0013 §3.3 ("unbounded in time"). | **The 82-candidate pile-up collapses to what is on screen**, and a signal that stopped hours ago leaves the live list — with **no decay logic in existence**. Confirmed stations stay put, marked live or ended. | This is the stage the user live-tests. Without TM-2 the window filter is a hull test and would hide almost nothing. Without the Candidate/Confirmed asymmetry it would make quiet confirmed stations vanish — a live-visible regression. |
| **TM-4** | **UI.** Each in-window row draws a **box** `(f_lo..f_hi) × (interval ∩ window)` across trace and waterfall, replacing the full-height bracket for non-focused rows. Boxes of persisting signals grow. | Bursts look like bursts. A steady station looks like a column. The picture and the list finally agree. | Nothing — a render change over TM-2's data. Keep the existing bracket for the focused row so T-149's drag-to-adjust-band (docs/14) is unaffected. |
| **TM-5** | **Pipeline + migration 0012 (index-only).** Interval close after `idle_gap`; new interval on return, same emitter; normalise overlapping source rows into disjoint intervals. **`count` removed from every liveness decision.** | The presence track is correct across a gap, and a returning signal revives rather than duplicating. Scrub queries stay interactive on a full retention window. | Needs TM-2's predicate to be observable at all. **T-251 must not have landed a conflicting decay before this** — see TM-6. |
| **TM-6** | **T-251 folded in** as hypothesis confidence: a function of *time since the latest interval closed*, applied to Candidate ranking only. Never deletes; never touches History; append-only if persisted at all. | A candidate that stopped **inside** the viewed window ranks below one transmitting now, and reads as ended rather than live. | If it landed before TM-3/TM-5 it re-introduces a decrementing timer with nothing to hang it on — a second unbounded accumulator, inverted. §5 is the brief. |
| **TM-7** | **Timeline scrubber marks + scrub re-derivation.** The scrubber marks past events over the capture window from intervals; scrubbing re-issues inventory + history for `[t−w, t]`; the IQ ring backs waterfall detail below tile resolution (ADR-0014). | Scrubbing back re-derives the live lists and the boxes. Past events are visible on the scrubber before you scrub to them. | Needs TM-2's predicate, or the marks and the list disagree — the worst possible failure for a scrubber, because it teaches the user to distrust both. |
| **TM-8** | **History surface.** `GET /api/events?f_lo&f_hi&t0&t1` (the durable catalogue of intervals, one-offs included) and `GET /api/inventory/{id}/presence` (one emitter's track). A separate UI surface, per workflow #3. `docs/api.md` + contract tests together. | Every event ever recorded is browsable by region and time, including one-offs that were never live candidates for more than their own duration. | Nothing — new routes, new surface. It is the stage that makes "nothing is deleted to make the live list correct" visibly true. |
| **TM-9** | **Blind acceptance, composing with T-254.** The 902–928 MHz scene through the mock SDR from the IQ ring: each short burst is one emitter with one **closed** interval of the right duration, not skirt fragments; one-offs are catalogued in History and are **absent** from the live Candidate list once the window passes them; hidden truth in the assertions only. | The invariants are enforced by a test rather than by a screenshot. | Nothing. It is the proof, and it is what stops TM-3 being tuned to make one screen look right. |
| **TM-10** | **Decode region extension (Rules L/I, §6).** The incremental contract for bounded-region pipelines, the two-reader rule, and the explicit handover state. | A region job's coverage is exactly the samples it read, and audio stays live throughout. | **Blocked on the analyze engine.** `POST /api/analyze` answers `501` and MAUTO is unscheduled (ADR-0015). Attempting this earlier means building an incremental scheduler for a pipeline that does not exist. §6 is a **contract now**; code when MAUTO is scheduled. |

**Waves** (≤ 4 Rust builders, per CLAUDE.md T-144): (1) TM-1; (2) TM-2; (3) TM-3, TM-4; (4) TM-5, TM-7; (5) TM-6, TM-8; (6) TM-9. TM-10 is unscheduled.

**Sequencing against tickets already filed:**

- **T-250** (skirt-fragment merge) is independent and can run in parallel throughout. The co-onset/co-offset evidence of §4 becomes available to it after TM-5 and is an option, not a dependency.
- **T-251** *is* TM-6 and must not start before TM-3 and TM-5. Its acceptance criteria shrink substantially — §5 explains why, and the ticket already says "coordinate rather than pre-empting that design".
- **T-254** *is* TM-9's fixture and can be captured any time; the assertions land with TM-9.
- **T-252** (RDS readout) is unaffected, except that §6.4 tells it a gap in RDS beside live audio is correct behaviour, not a bug to chase.

**Explicitly not in this plan:** chirp polylines (§1.3(a) — and note §1.3(b): for a sweep that completes inside one analysis frame a polyline would recover nothing, so this exclusion costs less than it appears to); scrub-back audio (§6.5); any change to `ConfirmPolicy`'s use of occurrences; any ADR-0016 contract change (§7).

---

## 10. Consequences

**Good**

- The three symptoms the user hit (ghost candidates, unbounded counters, homeless bursts) have **one** cause and one fix, and most of the fix is a query predicate plus its caller.
- Almost nothing new is stored. The interval set has existed since migration 0001; it was never named, indexed for time, or shown.
- Bursts and chirps become first-class without a new object, which is what makes the 902–928 MHz playground testable.
- "Ended 4 minutes ago" and "17 events, 4.2 s on air" are sentences the product can now say. Both were previously unrepresentable.
- T-251 gets much smaller and much safer.

**Costs**

- Explore's list becomes **view-dependent**, which is a real change in mental model: zooming the waterfall changes the Candidate list. Mitigated by always listing Confirmed and by liveness states, but it is the thing to watch in TM-3's live test.
- Two displayed family values (`family`, `family_in_window`) is one more thing on screen. The alternative — silently asserting a stale classification — is worse.
- `first_seen`/`last_seen` remain in the schema and the API with their meaning **narrowed by documentation rather than by types**. Every new reader must be told they are a hull. This is the most likely place for the model to erode.
- The interval-overlap predicate is a correlated subquery where there used to be two column comparisons. Index-backed (migration 0012), but it is on the interactive scrub path and needs measuring, not assuming.
- §6's two-reader rule means a user who asks to "decode what I am listening to" gets **two pipelines** against the listener budget (ADR-0011 §8.8). Correct, and it costs a slot.

**Unverified in this ADR:** ~~the `idle_gap` value~~ (settled in TM-5/T-262: `clamp(2 × revisit, 1 s, 60 s)`, derived from the revisit period and clamped by two tracker constants — §11 question 4); that the interval-overlap query stays interactive over a full retention window; that co-onset/co-offset actually separates skirt fragments from adjacent stations on the user's own 99.6 MHz scene; and the claim that TM-3 alone resolves most of T-251 (TM-9 measures it).

---

## 11. Open questions (for the user)

1. **Scrub-back audio (§6.5).** This ADR rules that Listen stays live-edge and that invariant 5 governs decode, not Listen. If the intent was that scrubbing back should also *play back* audio from that point, say so — it is a separate and larger piece of work, not a wording fix.
2. **Window-scoped Candidates, always-listed Confirmed (§2.2).** Proposed, because the alternative makes quiet confirmed stations disappear. Confirm, or say that Confirmed should be window-scoped too with an "all" toggle.
3. **The default Explore window.** The viewed waterfall span (proposed — it matches "what is on screen"), or a fixed recent window independent of zoom?
4. ~~**`idle_gap`.** Derived from the detector's revisit period (proposed), or a per-band setting?~~ **Answered in TM-5 (T-262): revisit-derived**, `idle_gap = clamp(2 × revisit_period, 1 s, 60 s)` (`hk_model::presence::IdleGap`, docs/07 §2.27). A gap shorter than the revisit period is **not evidence of absence** — the receiver was not listening — so the gap has to come from how often it looked; the factor 2 (two consecutive missed revisits), the 1 s floor (the tracker's `max_transition_gap_s`) and the 60 s ceiling (its `idle_timeout_s`) are each taken from a rule that already exists, so none is a dial. **A chatty 915 MHz ISM burst source therefore reads as fifty intervals, not one, deliberately** — fifty events on *one* emitter, "50 events, 1.0 s on air", each burst's box its own height. Periodicity is `EmissionFeatures`' job (§7.2). A per-band gap is worse on three counts: it has no measurement behind it, it makes the same sensor count differently for no reason but its band, and one wide enough to make ISM "one interval" re-creates this ADR's own hull pathology — a span that is 99.97 % silence presented as time on air — one level further down, where it is harder to see. Revisit the *factor*, not the shape, if TM-9 measures against it.
5. ~~**Chirp polylines (§1.3).**~~ **Answered in two steps; only the drawing half is still open, and it is still out of §9.** T-294 reframed it: for a sweep that completes inside one analysis frame — SF9/125 kHz, the standard US915 uplink rate — a polyline would recover nothing, because the centre track is not in the detector's input at all (§1.3(b)). What *does* recover it is a chirp-rate estimator reading the ring, good to −3 dB in the channel (§1.3(c), `hk_dsp::chirp`). **T-297 then answered the live half — "whether a characteriser should read IQ for candidate swept regions" — with yes, and wired it**: a normal run channelises the region and writes its sweep rate as evidence (§1.3, "What the system claims today"), measured blind through the mock device at 1.6 % of truth. A polyline remains the §1.3(a) representational refinement, unscheduled; it is now the *only* part of this question outstanding, and nothing depends on it.

---

## Sources

- CLAUDE.md, "Signal & inventory model (invariants, from the user 2026-09-16)" — the settled model this ADR records.
- `docs/tasks.yaml` T-250, T-251, T-252, T-253, T-254 (user findings, live Explore testing, 2026-09-16).
- `docs/07-data-model.md` §2.9, §2.10, §2.11, §2.20, §4.
- `docs/14-ui-rewrite.md` (MUI brief and its docs/15 §7 additions).
- [ADR-0011](0011-decoder-workbench-contracts.md) §8.5 (live-edge policy), §8.8 (listener budget), §8.9 (the FM recipe's sibling outputs).
- [ADR-0012](0012-attention-memory-contracts.md) §7 (novelty alarms and the `new-emitter` kind).
- [ADR-0013](0013-ui-architecture.md) §3.1 (slices), §3.3 (LIVE vs reviewing — amended by TM-3), §4.2.
- [ADR-0014](0014-iq-capture-ring.md) (the ring behind scrub-back).
- [ADR-0015](0015-decoder-synthesis-contracts.md) §5 (region-analyze), §6 (burst path), §11 (candidate pipelines), §12 (Listen as an audio pipeline).
- [ADR-0016](0016-classification-contracts.md) §2 (arbitration rank), §5 (signatures, `EmissionFeatures`, the cluster contract).
- Code read for this ADR: `crates/hk-model/src/repo/migrations/0001_init.sql` (`emitter`, `detection`, `track`, `emitter_observation`, `emitter_lifecycle`, `region_extent`), `0008_emitter_relation.sql`, `crates/hk-model/src/repo/inventory.rs:38–86` (the hull predicate), `crates/hk-api/src/query.rs` (the `t0`/`t1` parser), `docs/api.md` (`GET /api/inventory`, `GET /api/history`).
