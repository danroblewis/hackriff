# ADR-0019 — Presence is an interval with endpoints: the box runs to the live edge until an END is detected

**Status:** PROVISIONAL. The *model* is **settled by the user** (2026-09-16, T-410) and is recorded, not proposed. What is proposed here is where the honesty burden lands, the end detector that has to carry it, and the START/END/REOPEN stream shape.

**Source:** the user, 2026-09-16, T-410. It **inverts a constraint the coordinator imposed on T-388** eleven hours earlier, deliberately and with reasons.

**Touches:** ADR-0004 §15 (stream contract, schema bump `hackriff.presence/1` → `/2`); ADR-0017 §1.1/§1.2 (the presence interval and the box); docs/07 §2.27; docs/stream-contract.md §15; `hk-model::presence`, `hk-detect::track`, `hk-pipeline::presence`, `hk-api::query`/`presence`/`events`, `ui/src/presence.ts`, `ui/src/timebox.ts`, `ui/src/app/centre/overlays.ts`. Capabilities C09, C10, C39, C40.

---

## Context: two contracts, both defensible

T-388 fixed a live box that grew in ten-second steps. The coordinator's brief to it contained a constraint:

> A box drawn to the live edge on the **assumption** the signal is still there is a claim about air nobody measured.

That agent implemented it faithfully. The result is **contract A**.

**Contract A — presence is an accumulation of observations.** The box's top is the newest *measured* end (`t_last_end`, the end of the last burst the detector actually saw). Anything not yet observed is not yet claimed. A stream of extension records pushes the measured end forward as it advances; `open` is a decoration on the top edge. The client holds three refusals, of which the third is:

> **Not contiguous.** An extension whose span starts *after* the end on screen describes a different stretch of air, with silence in between. (`ui/src/presence.ts:65`)

**Contract B — presence is an interval with endpoints.** The measurement is the **START event plus the absence of an END**. The box runs from its start **straight to the live edge**, and caps only on a real detected end. This is what tracking is: a track is open until it closes, and the open-ness is itself the claim being made, revisable by a later close.

Both are coherent. **They are different contracts, not one looser than the other** — which is why this is an ADR and not an edit.

### What forced the choice

Contract A cannot say the one thing a live spectrum display exists to say: **"this signal is on the air now."** "Now" is always after the last measurement, so under A the box is structurally, permanently short by the reporting latency, and the only way to shorten that is to report more often — which is the per-poll presence bump the user explicitly rejected, and which costs a record per open emitter per tick forever.

Contract B says it in three events over a signal's whole life, and says nothing at all while a signal merely continues. The user's phrasing is exact: the stream needs only **START / END / REOPEN**, **never a per-poll presence bump to advance the box top**.

### Rejected, recorded so they are not re-proposed

| Rejected | Why |
|---|---|
| **Rapidly re-testing presence** | The user's own rejection. It is contract A with a shorter period — the same unbounded record rate, the same structural lateness, and it makes the box flicker on every missed frame. |
| **Gating the next waterfall row on an end-decision** | The user's own rejection. The waterfall is capture; capture is always-on (CLAUDE.md, "Pause freezes the view, not the capture"). A display decision may never hold up a row. |
| **Client-side extrapolation past the live edge** | Unchanged from T-388. The box stops *at* the live edge; it never runs past it, and nothing in the client reads a clock to invent rows that do not exist. Contract B extends the box **to** available time, not **beyond** it. |
| **A stored `closed_at` column** | ADR-0017 §8.3 stands. Closure is derived from interval boundaries and the idle gap on every read, because storing a decision made under one parameter value is the measurement-versus-interpretation mistake docs/07's first rule forbids. |

---

## Decision

### 1. The box runs to the live edge while its interval is open

An open presence interval draws a box from `t_start` to **the live edge of the view** — which, live, is the newest row held, and, paused or scrubbed, is the end of the viewed window. That is the same edge `open` is already derived against (`hk_model::presence_in_window`, "the silence is measured to `window.end`, which **is** the caller's live edge"), so the box and the liveness flag cannot disagree about what "now" means.

A closed interval draws a box from `t_start` to its measured `t_end`, exactly as today.

### 2. The box distinguishes measured air from assumed air — this is where half the honesty burden lands

Contract B makes a claim about air nobody measured. It is allowed to, **on condition that it says so.**

Every open box carries three times, not two: `t_start`, the **last measured end**, and the live edge. The span from the last measured end to the live edge is the **open cap** and is drawn as assumption — never with the same weight as the measured body. The display therefore always shows where measurement stops and assumption begins, and the open cap **grows visibly as the silence grows**, so a suspected end is legible without being acted on.

This is not decoration. It is the same device as the coverage map's grey (T-368), which refuses to spell "never looked" as "looked and it was quiet", moved onto the time axis: here it refuses to spell "have not yet decided it stopped" as "measured it transmitting".

**The client may not fabricate the boundary.** The last measured end is `presence.last_interval.t_end_s`, served by the backend, already carried by every inventory row and every stream record. The client computes no times (T-362: a box carries a band fraction and two absolute capture times and no screen position; the conversion happens in the waterfall's row pass, every frame).

### 3. The other half lands on the end detector, and its latency is the idle gap

Under contract A a late end-decision cost nothing: the box simply stopped growing. Under contract B **a late or missed END means the box over-claims silent air, all the way to the live edge.** So the end detector is now the accuracy of the display, and its numbers belong in this ADR.

**The rule.** An interval closes when the receiver has **observed** `idle_gap` of silence since the last measured end. This is not new: it is `hk_model::presence::intervals_from_spans`, unchanged, with its existing derivation

```text
idle_gap = clamp(REVISIT_FACTOR × revisit_period, MIN_IDLE_GAP_S, MAX_IDLE_GAP_S)
         = clamp(2 × revisit_period, 1 s, 60 s)
```

and its existing reason: **a gap shorter than the revisit period is not evidence of absence, because the receiver was not listening.** Every constant in it is taken from a rule that already exists (2 = the bandit's own "two consecutive missed revisits"; 1 s = the tracker's `max_transition_gap_s`, below which nothing may split what the tracker joined; 60 s = the tracker's `idle_timeout_s`, past which the track was already closed upstream).

**The latency today is 60 s, and that is the defect this ticket exists to fix.** The served path passes `IdleGap::conservative()` — the 60 s end — at every site:

| site | today |
|---|---|
| `hk-api/src/query.rs:1486` (`/api/inventory` rows) | `IdleGap::conservative()` |
| `hk-api/src/presence.rs:147, 172` (`/api/inventory/<id>/presence`) | `IdleGap::conservative()` |
| `hk-api/src/events.rs:235` | `IdleGap::conservative()` |

with the comment *"hk-api does not know the scheduler's revisit period"*. The pipeline sets a derived gap only when a **bandit plan** is configured (`hk-pipeline/src/control.rs:290`, from `plan().revisit_bound_ns`). A live dwell on one centre — the entire Explore case — configures no bandit, so nothing derives a gap and every reader takes 60 s.

A receiver dwelling continuously on a band revisits it **every STFT frame**. Its revisit period is milliseconds, and the honest gap is the 1 s floor. Calling that "unknown" and taking 60 s is not conservatism; it is discarding a measurement the run has in hand.

**The fix: derive the gap from coverage, which is the only thing that knows how often the receiver looked.** hk-api already reads the IQ ring's tune journal for exactly this purpose (`hk-api/src/coverage.rs:87`, `ring_spans` — one segment per retune, each with `t0_ns`/`t1_ns`/`center_hz`/`sample_rate_hz`). Over a query's window, the coverage of a band is a set of spans; the revisit period of that band is the **largest silence between consecutive spans covering it**.

```text
coverage contiguous over the band  ⇒ IdleGap::continuous()  = MIN_IDLE_GAP_S = 1 s
coverage combed, period P          ⇒ IdleGap::from_revisit_s(P)
no coverage record at all          ⇒ IdleGap::conservative() = 60 s   (unchanged)
```

`IdleGap::continuous()` is not a new number and not a fourth option: it is `from_revisit_s` of a revisit shorter than half the floor, named so that **"the receiver never looked away" is distinguishable from "nobody recorded whether it looked"** — which `from_revisit_s(0.0)` currently cannot be, since it treats a zero period as unknown and returns 60 s.

This also satisfies the CLAUDE.md invariant directly: *"the backend keeps a coverage map derived from the SDR configuration/tune history — for each interval, which centre/span/rate (and which device) was active — so observed-versus-unobserved is computed from what was actually sampled."* Absence, like observation, is computed from what was actually sampled.

**Resulting latency.** Under a live dwell: `idle_gap` = 1 s, plus at most one stream tick (`PRESENCE_PUSH_NS`, 250 ms) ⇒ **END lands ≤ 1.25 s after the emission stops**, and the box retracts to the measured end. Under a sweep with a 10 s revisit: 20 s of *observed* silence — correctly longer, because the receiver was not there to see it stop.

**The END carries the measured end, not the decision instant.** So when it fires, the box does not stop where the assumption had reached; it **retracts** to where measurement actually stopped. The over-claim is transient and bounded by the latency above, never baked in.

### 4. False negatives: a missed END, and the poll as the backstop

| failure | consequence | bound |
|---|---|---|
| Rate cap truncates a tick | END not sent this tick | carried to the next tick (see §6); never dropped |
| Slow consumer dropped by publisher policy (ADR-0004 §7) | END lost for that client | next inventory poll, **≤ 5 s** |
| Socket drops, client paused, client offline | END never seen | next inventory poll on resume, **≤ 5 s** |
| Track closes by idle / capacity / end-of-stream without a silence-derived END | — | the close emits END too; there is no path that produces no END |

**The backstop is the 5 s inventory poll, and making it work requires a second contract change: the poll must be allowed to *cap* a box.** Under contract A both the poll and the stream could only ever extend, so a stale open box was impossible. Under contract B the poll's `presence.last_interval` with `open: false` is authoritative and closes the box. This is why §3's gap fix is load-bearing on **both** surfaces: if the stream closed at 1 s and the poll still reported `open` for 60 s, the poll would re-open every box the stream closed. Fast and slow surfaces are kept in agreement the way the rest of this codebase keeps them — by making them the same function of the same inputs, not by two rules maintained in step.

**So the worst case a viewer can see is a box over-claiming ≤ 5 s of silent air, and the typical case is ≤ 1.25 s.** Those are the numbers that replace "the box simply stopped growing".

### 5. What the box does while an end is suspected but not decided

**Nothing changes on suspicion.** The box keeps running to the live edge, and the open cap — already drawn as assumption (§2) — grows. There is deliberately no third state, no dimming schedule, no probationary styling that switches at some sub-threshold silence:

- A state that re-tests presence to resolve the suspicion is the rejected option.
- A state that changes the box's *extent* on suspicion is contract A with extra steps: it makes the box's top jitter on every missed frame, which is worse than being late by a stated, bounded amount.
- The suspicion is **already visible**, because the open cap is exactly the silence: a box whose assumed region has grown to a third of its height is visibly suspicious, with no new state and no new threshold.

The one place suspicion is acted on is ranking, and that already exists and is not touched: `Presence::confidence` (ADR-0017 TM-6) decays a closed candidate one `1/e` per idle gap of observed absence, and is flat at 1 while the interval is open — which is to say, flat over exactly the suspicion window. That is deliberate and stays: the receiver has observed no absence yet, so there is nothing to rank down on.

### 6. REOPEN: the threshold is the idle gap, and it is the same number for the same reason

A reopen and a new start are **two questions, not one**, and neither is tuned:

- **Is this a new interval?** An absence question. Answered by the idle gap — the same constant, the same derivation, the same reason as §3. A silence of at most `idle_gap` is *not* evidence the emitter stopped, so the interval simply continues and **no event is emitted at all**. That is precisely why contract B needs no per-poll bump: continuing is the default, and only endpoints are news.
- **Is this the same emitter?** An identity question. Answered by entity resolution — the same binding (`Inventory::emitter_of_track`) that already decides which box a track's extent belongs to. A returning signal that resolves to an existing emitter **reopens** it; energy that resolves to no emitter **starts** a new one.

```text
silence ≤ idle_gap                          → same interval; no event
silence > idle_gap, resolves to emitter E   → REOPEN E: a new interval on E, drawn as a SECOND box
silence > idle_gap, resolves to no emitter  → START: a new emitter, a new box
silence > MAX_IDLE_GAP_S (60 s)             → the tracker already closed the track upstream;
                                              the two intervals can never be rejoined
```

The 60 s ceiling is not a third threshold. It is hk-model's existing rule that two source rows further apart than the tracker's own `idle_timeout_s` were judged discontinuous by a **measurement**, and re-joining them here would overrule a measurement with a parameter.

**REOPEN never stretches a box across the silence.** It opens a second box, with the gap between them drawn as a gap. This is how the purpose of T-388's third refusal survives its removal (§7): the silence is still never claimed — it is now *shown* rather than hidden behind a box that stopped updating.

**A REOPEN is not inferable from a START by the client**, because the client's row holds only `last_interval`, not the whole set; it cannot see that this emitter has prior intervals when the prior one has scrolled out of the window. So the kind is stated on the wire rather than derived on arrival.

### 6.1 "A detected end is provisional and revocable" — how far that goes, and where it stops

CLAUDE.md's statement of the model says an end is revocable: *"if later samples show the signal resumed within tolerance, null the end and keep the single interval open rather than splitting it or spawning a new emitter."*

**Most of that is satisfied before an end exists to revoke.** The idle gap *is* the tolerance: a silence shorter than it produces **no END at all**, so a signal that blips off and returns inside it keeps one unbroken box, one interval and one emitter, with nothing published either way. Assume-ongoing-retract-on-a-detected-end is the whole rule, and the detection threshold is one unit of observed absence.

**The remaining case — a return *after* an END — is deliberately left to the poll, because the stream has no honest start to publish for it.** The tracker joins bursts across its own `idle_timeout_s` (60 observed s), far past the gap a *box* caps at, so the same track reappears after its END. But `LiveExtent::t_start_ns` is `t_first`, **the track's first burst**, not the resumption; publishing it as the new interval's start would claim the exact silence the END was drawn for. The resumption's own start is not in the extent. So an END is recorded per *track*, that track publishes nothing further, and the next inventory poll serves the new interval with the start it actually has (≤ 5 s). A **new track** bound to the same emitter is different and does publish: its `t_first` is its own first burst, which is a correct interval start. **That is the derivation of REOPEN's identity half — it is the tracker's own continuity judgement, not a threshold chosen here.**

**What is NOT done, and why it is the user's call.** Making the end revocable *after* it fires would mean `intervals_from_spans` joining source rows across a longer silence than the idle gap — and that collides head-on with a settled, reasoned decision in this codebase: ADR-0017 and docs/07 §2.27 hold that a chatty 915 MHz ISM sensor firing every 30 s is **fifty intervals, not one** ("50 events, 1.0 s on air" is honest; smearing fifty transmissions into one span of mostly silence is the hull pathology ADR-0017 exists to remove), asserted by `hk_model::presence::tests::a_chatty_burst_source_reads_as_one_interval_per_burst`. Any revocation window wide enough to rejoin a real dropout is also wide enough to swallow a burst cadence, and the two readings cannot both be right. **Flagged for the user rather than decided here.**

### 7. The three client-side refusals

| T-388 refusal | Under contract B |
|---|---|
| **1. No interval on the row conjures no box.** `last_interval: null` means no interval intersects the viewed window; a box built from an event would be a rectangle the windowed query did not return. | **Kept verbatim.** Rows are created by the poll, never by the stream. ADR-0004's "a push never creates" survives for creation. |
| **2. Not newer — a reordered or replayed record can never shorten a box.** | **Kept, restated.** No event may shorten the **measured** extent: `t_start` may not move later, and an END at or before an END already applied is dropped. The open cap is not measured extent, so an END capping it is not shortening — it is the assumption being replaced by the measurement it was standing in for. |
| **3. Not contiguous — an extension starting after the end on screen describes a different stretch of air.** | **Removed.** Replaced by REOPEN (§6), which serves the same honesty with better latency: the second stretch of air gets its own box immediately, and the silence between them is drawn as silence instead of being represented by a box that quietly stopped moving. |

### 8. The stream: START / END / REOPEN (`hackriff.presence/2`)

ADR-0004 §15 is amended. The `presence` stream carries three record kinds in place of `presence-extension`, which is retired:

| kind | when | says |
|---|---|---|
| `presence-start` | a new interval opens on an emitter with no prior interval | `{t_start_s, open: true}` |
| `presence-reopen` | a new interval opens on an emitter that has prior intervals | `{t_start_s, open: true}` |
| `presence-end` | an interval closes (observed silence > idle gap, or the track closes) | `{t_start_s, t_end_s, open: false}` |

`metadata.last_interval` stays the **same three-field object** `/api/inventory` serves, so a client assigns it rather than rebuilding it, and the fast surface still cannot invent a shape the slow one would disagree with. Envelope `t_ns` stays integer Unix nanoseconds (T-354). No frequency on any of them: an endpoint is time, not geometry (T-362).

**A continuing interval emits nothing.** For a band of steady broadcast carriers the stream is silent, where contract A emitted one record per emitter per tick forever.

**ADR-0004's §15 rule "a push never creates, only extends" is amended to "a push never creates; it opens, extends and closes."** Its companion rule — *"the stream says what was measured, never what is presumed; a client may not interpolate towards the live edge"* — **survives unchanged and is now load-bearing in a new place**: the END carries the **measured** end, and the presumption lives entirely in the renderer, where §2 requires it to be drawn as presumption. The stream never presumes.

**Rate bound unchanged, truncation policy changed.** ≤ 1 tick per 250 ms, ≤ 32 records per tick, a hard 128 records/s ceiling, drops counted. But under contract A a record left out of a tick cost only freshness, so dropping it was right; under contract B a dropped END costs an over-claim of silent air. So within a tick, **ENDs are emitted before STARTs and REOPENs**, and what does not fit is **carried to the next tick** rather than discarded. The ceiling still holds — the cap is on records per tick, not on emitters — and the counters still say what was deferred.

### 9. Paused views

Unchanged: **a paused view opens no socket** (`ui/src/app/explore/presence-stream.ts`, asserted with a WebSocket stub). A paused view is answering about a fixed past window, where every interval's endpoints are already known and served by the poll; a live endpoint stream has nothing to add. Going live re-subscribes, and the poll reconciles.

---

## Consequences

**Good.** The display can finally say "on the air now", which is the basic statement a spectrum display exists to make, and it says it within one poll of a signal starting rather than lagging structurally forever. Stream traffic for a steady band falls to zero. The end detector's latency goes from **60 s to ≤ 1.25 s** — and because the gap now comes from coverage, it is *right* rather than merely shorter: a sweeping receiver gets a proportionally longer gap, as it should.

**Bad, and stated rather than hidden.** A box can over-claim up to ~1.25 s of silent air normally and up to ~5 s if an END is lost, where contract A could never over-claim at all. §2's open cap is what keeps that honest, and it is a rendering rule — the weakest kind of guarantee in this codebase, since it can be undone by a styling change that looks cosmetic. It is therefore asserted by test, not left to care.

**The gap fix has reach beyond boxes.** `IdleGap` also sets `Presence::confidence`'s decay constant and the scheduler's re-check horizon (ADR-0017 TM-6). Moving a live dwell's gap from 60 s to 1 s makes a stopped candidate decay ~60× faster there — which is the intended correction (CLAUDE.md: "candidate confidence decaying / expiring when a region goes quiet", "target ~2–10 s"), but it is a behaviour change to the live list, not only to the boxes, and it is the reason this ADR is `core_interface`.

**Not fixed here.** `Tracker`'s own `idle_timeout_s` of 60 s is untouched, and so is the closed-track route to confirmation that T-398/T-403 are working on. A track staying open for 60 s after its emission stops is now harmless to the *box*, because the box's end comes from the presence interval's idle gap and not from track close — which is the point of keeping the two separate. See §Relationship below.

---

## Relationship to T-398 / T-403 (confirmation) — the same symptom, two different defects

T-398 and T-403 found that a permanently-on emitter has no close until the 60 s idle timeout, and that **confirmation route B is weighed only on track close**. T-390's in-band fragment rule also depends on track close.

**That is not this defect, and fixing this does not fix that.** They share a number — 60 s — and they share a shape — "waits for a close" — but they are two different consumers of two different closes:

| | T-403 | T-410 (this) |
|---|---|---|
| which close | **`Tracker` track close** (`CloseCause::Idle`, `idle_timeout_s` = 60 observed s, stretched to 4× the mean inter-arrival, capped at 1 h) | **presence-interval close** (`intervals_from_spans`, `idle_gap` = `clamp(2 × revisit, 1 s, 60 s)`) |
| who decides | `hk-detect::track::tracker` | `hk-model::presence`, derived on every read |
| why 60 s today | the tracker's configured timeout, deliberately long so a bursty emitter stays one track | `IdleGap::conservative()` in hk-api, because nobody supplies a revisit period |
| the fix | give the continuous-and-trusted route live evidence so it need not wait for close | derive the gap from coverage instead of defaulting to unknown |

They are **independent and non-conflicting**. This ADR does not shorten the tracker's timeout — doing so would fragment bursty emitters into many tracks and is exactly what the tracker's 60 s exists to prevent — and it does not touch the confirmation routes. Conversely, T-403 making confirmation live does not move a box's end by one nanosecond, because the box reads the presence interval.

The one thing they share is the **ceiling**: `MAX_IDLE_GAP_S` = 60 s is *defined* as the tracker's `idle_timeout_s`, so that a presence interval is never re-joined across a silence the tracker already judged discontinuous. If T-403 changes `idle_timeout_s`, that constant must move with it, and hk-model's doc comment says so.

---

## Alternatives considered

**Keep contract A and shorten the extension period.** Rejected by the user, and independently unsound: it is an unbounded record rate that still cannot say "now".

**Store the closure as a fact (`closed_at`).** Would make the fast and slow surfaces agree trivially. Rejected: ADR-0017 §8.3 and docs/07's first rule — closure is an interpretation under a parameter, and storing it freezes one reading of data that should be re-derivable. The coverage-derived gap achieves agreement without storing an interpretation, because both surfaces derive from the same measured coverage.

**Publish the run's idle gap on a session/status surface for hk-api to read.** Simpler plumbing than reading coverage, and rejected because it is wrong for the multi-device, multi-window case CLAUDE.md requires: one run can have several front ends covering different ranges with different revisit periods, so a single run-wide gap would claim the wrong absence for every band but one. Coverage is per band because the receiver's attention is per band.

**Make `open` mean "drawn to the live edge" only for Confirmed rows.** Rejected: candidates are the rows a viewer is watching appear and vanish, so they need the live edge more than confirmed rows do, and two rendering contracts for one object is how overlays desync.

---

## Sources

- The user, 2026-09-16 (T-410), and CLAUDE.md "Signal & inventory model" and "Time, the waterfall, and the live view" invariants.
- T-388 (`51d033a`) — contract A, the presence push, the three refusals, the rate bound.
- T-362 (`041a67f`) — a box carries times, not screen positions; overlays re-lay-out every frame.
- T-354 — `t_ns`, absolute capture time, everywhere on the stream.
- ADR-0017 §1.1/§1.2/§5/§8.3, ADR-0004 §15, docs/07 §2.27.
- T-398 / T-403 — the confirmation-side end-of-recording defect, and why it is a different one.
