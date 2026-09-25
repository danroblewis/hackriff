# 20 — MAUTO decision brief: every open ADR-0015 question, in one sitting

**Decision brief for the user (T-553, 2026-09-20). Decided by the user 2026-09-23 — see the decision table's "Decision" column: U1–U4 took the recommendation, U5 overrode it (stereo audio is wanted). ADR-0015 is ACCEPTED on these answers (its §16).**
**The table at the end is the document. Everything above it is why.**

---

## What this is, and why it exists

[ADR-0015](adr/0015-decoder-synthesis-contracts.md) is the MAUTO contract: how an unknown signal gets
searched into a runnable decode pipeline. It is PROVISIONAL, and it ends three of its sections with
an "Open questions (for the user)" list. The ticket said there were ten. **There are fifteen**, in
three places (§10, §11.10, §12.12), and no one has ever read them together.

The cost of that is precise and singular. The MAUTO task graph starts at **M-1**, whose definition of
done includes *"ADR-0015 → ACCEPTED review"*. An ADR carrying fifteen unanswered questions cannot be
reviewed to ACCEPTED, so **every engine ticket in the MAUTO graph (M-1…M-14, CP-1…CP-9, LP-1…LP-8) is
downstream of this one page.** That is the whole blocking structure; there is no second one.

The other thing that happened is that the list drifted. Of the fifteen, **five are genuine product
calls only you can make**; **six are engineering defaults that got escalated because the author was
unsure, not because you have a stake**; **two are the same question asked twice** and one of them is
already answered inside the ADR; and **two are numbers**, which is a measurement problem and not a
preference. This brief decides the eleven that are not yours and puts five in front of you.

**How to use it:** read the table at the bottom. Say "yes" to accept all five recommendations, or
name the ones you want changed. The eleven decided below are recorded here as decisions; overrule any
of them the same way.

---

## The five questions that are yours

### U1 — Does one clean burst confirm a signal?

*(ADR-0015 §10 Q1, plus the one number T-548 needs from you.)*

**The question.** A single ADS-B squitter arrives, 56 bits, and its CRC-24 checks out. Does the
device mark that aircraft **Confirmed** by itself, or list it as "solved" and wait for you to press
Confirm? The general rule needs three distinct valid frames; this asks whether a one-off burst — the
thing the signal model calls first-class — is exempt.

**Why it is not obvious.** Confirmation is one-way. `change_emitter_lifecycle` forbids a return to
`candidate` (§11.5), so a wrong auto-confirm can only be undone by deleting the entry. Against that:
requiring a click per aircraft is the workflow the device exists to remove.

| Option | Cost to build | Runtime cost | What it forecloses |
|---|---|---|---|
| **A. Attach only; you promote** (the ADR's proposal) | Zero — M-9 / CP-4 exactly as written | None | Nothing structurally; the flip is one predicate. But in a busy ISM or ADS-B window it is a click per emission, which is the automation the product promises |
| **B. Auto-confirm only when the check was *template-fixed* and ≥ 24 bits** | One extra branch in `ConfirmPolicy.synthesized` (inside M-9 / CP-4, no new ticket) + one blind acceptance case in M-12 / CP-9 proving 200 noise windows never trigger it | None | Nothing. Searched checks keep the three-frame rule untouched |
| **C. Auto-confirm on any check ≥ 24 bits, searched or not** | Same code as B | None | This is the dangerous one: a searched CRC polynomial over 10⁴ hypotheses finds a "valid" frame by chance, which is exactly the failure the look-elsewhere term exists to price |

**Recommendation: B.** The reason a single frame is normally weak is the look-elsewhere cost — you
tried many hypotheses, so one fit means little. A **template-fixed** check searched zero hypotheses,
so its false-confirm probability is 2⁻²⁴ per frame outright, which is *stronger* evidence than three
frames under a searched polynomial. Confine auto-confirm to exactly that case and the irreversibility
never touches open search.

**One number to say out loud while you are here.** T-548 exists to re-derive the guessed thresholds
(64 bits, 3 frames, width 16) from a stated false-confirm budget, and it cannot start without the
budget. Proposed: **at most one wrong Confirmed emitter per week of unattended running.** Say a
different number if that is not your tolerance.

**Blocks.** M-9 and CP-4 (the shape of the confirm rule) and T-548 (which needs the budget line).
Under B as a stated assumption both can be written now — the exemption is one predicate on
`template.provenance == builtin && check.searched == false`.

---

### U2 — May the device analyze on its own, and may it do so on battery?

*(ADR-0015 §10 Q3, merged with the battery half of §10 Q2. The *numbers* in §10 Q2 are not a question — see "Not being asked", M2.)*

**The question.** When blind detection finds a signal it cannot explain, should the attention
scheduler start an analyze job by itself, or does every search wait for you to ask? And separately:
may the heaviest profile (`deep`: 120 s wall, 400 s CPU, 4 threads) run while you are on battery?

**Why it matters more than it looks.** This is the one question on the list that changes an
architecture rather than a default. Auto-queued jobs mean the `503 busy` queue has a second producer,
the power policy becomes a real input rather than the "minimal one" M-3 was going to add, and the
device spends energy while you are not watching. It is also, bluntly, where the "AUTO" in MAUTO is.

| Option | Cost to build | Runtime cost on the handheld | What it forecloses |
|---|---|---|---|
| **A. User-triggered only; `deep` refused on battery** (the ADR's proposal) | Zero beyond §3.3 as written — an admission check in `hk-pipeline::synth` (M-3) | Nothing until you ask | The product claim. Workflow #4 ("recommend explanations for what was detected") reads as something the device does, not something you request per signal. A tool you must point at each unknown is a workbench |
| **B. Auto-queue at `quick` only (3 s, 1 thread), unknown candidates, mains only, one job at a time** | ~2 tickets: wire ADR-0012's attention score into the analyze queue in `hk-pipeline`, and a real power-policy object (M-3 already adds a stub); plus a queue view + stop button in MUI (M-11) | 3 s of one low-priority thread per unknown. The `synth` chain kind already sits below ring readers and halves its threads when `lost_samples` rises, so capture is protected by machinery that already exists | Nothing. `deep` stays manual; the switch is a config value |
| **C. Auto-queue at any profile, battery included** | Same code as B | The device decides to spend two minutes of four threads on your battery without asking. On a handheld that is a thermal and runtime decision you should own | Your control over a power budget |

**Recommendation: B.** The safety argument is already built: §3.3's throttle-on-`lost_samples` rule
means an automatic search cannot hurt the ring, so the residual risk is power, not correctness — and
"mains only, `quick` only" removes the power risk entirely. Do not ship A; a device that only
explains signals you already pointed at is not the product described in CLAUDE.md.

**Blocks.** M-3 (budget, power and admission — the profiles table and the queue), M-11 (whether the
UI needs a job queue at all). Under B, M-3 can proceed now with an `auto_profile: quick | none`
policy field defaulted to `none`, so the decision later is configuration, not code.

---

### U3 — When you and a valid decode disagree, who wins?

*(ADR-0015 §11.10 Q5 **and** ADR-0016 open question 3 — the same call, asked in two ADRs. Answering it here closes both.)*

**The question.** You have labelled an emitter by hand. A pipeline then decodes it with a valid CRC
and says it is something else. Does your label stand, or does the decode override it?

| Option | Cost to build | What it forecloses |
|---|---|---|
| **A. You win (rank 0); the contradicting decode is recorded and shown beside your label** (both ADRs propose this) | Zero — this is `effective_rank_sql!` as it already stands (`repo/classify.rs:18`) | Nothing. The decode is not discarded, only outranked, and it is visible |
| **B. The decode wins; the contradiction is flagged** | ~1 ticket: a rank change in ADR-0016's ladder plus a contradiction surface in MUI | User authority as an invariant — which is load-bearing elsewhere ("a user delete wins", §5.5; "a user promote/reject is not overridden by evidence", §11.3). Breaking it here makes it a special case everywhere |

**Recommendation: A.** A CRC-valid decode is strong evidence, but B's failure mode is silent: a
template-bound decode can produce a plausible identity by coincidence under a searched check, and
under B you would have no way to make a correction stick. Keep rank 0 and make the contradiction
loud in the UI instead of authoritative in the database.

**Blocks.** CP-4 (promotion writes a rank-1 decoder row), ADR-0016's rank ladder and its own
acceptance. Nothing is actually unstartable under A, because **A is what the code already does** —
answering it is about closing the question, not about unblocking a build.

---

### U4 — Which missing demodulators get funded?

*(ADR-0015 §10 Q5 merged with §12.12 Q2. §12.12 Q3 — "keep the legacy Listen chain?" — is not an independent question; it is a consequence of this answer, and is decided as D6 below.)*

**The question.** Three modulations have no block. **Generic PSK** has no `psk_demod`/Costas, so any
PSK signal stops at verdict `demodulated` and never reaches bits. **SSB and CW** have no blocks
either; Listen serves them today only through the hand-written legacy chain. Do we fund blocks for
them, or accept the gaps permanently?

**The asymmetry that decides it.** PSK is a *decoding* gap: without it an entire modulation family is
permanently unreachable by the search, and reaching bits is MAUTO's whole claim. SSB and CW are
*listening* modes — nothing decodes from them — so a legacy path there costs a file in the build, not
a capability.

| Option | Cost to build | What it forecloses |
|---|---|---|
| **A. Fund PSK; SSB/CW stay legacy forever** | M-14 promoted from "optional" to required: 1 ticket in `hk-blocks` (Opus, real DSP). `chains/listen.rs` (1093 lines) stays in the build permanently for two modes | Nothing. LP-8 already says "retire for cut-over modes only" |
| **B. Fund all three** | M-14 plus `ssb_demod`/`cw_demod` (~2 more tickets, `hk-blocks` + LP-* parity harness runs) | Nothing, but it spends two tickets of DSP work on modes no decoder consumes |
| **C. Fund none** | Zero | MAUTO ships unable to reach bits on PSK — the single largest hole in the coverage claim |

**Recommendation: A.** Fund `psk_demod`; make M-14 a required MAUTO ticket rather than an optional
one. Leave SSB and CW on the legacy chain indefinitely and **write that into the ADR as a decision**
rather than leaving it as a standing question. §12.5 already says a permanent legacy path for them is
"an acceptable end state, not a failure" — this just stops re-asking.

**Blocks.** M-14's priority (currently "optional"), and LP-8's scope. Nothing is unstartable under A.

---

### U5 — Do you want stereo audio?

*(ADR-0015 §12.12 Q4. One line, answered in one word.)*

Nothing in the product asks for it today. It is a **wire change**, not a block: `channels` plus
interleaved `ri16_le` breaks the mono assumption baked into every audio client (the UI dock,
`py/examples/hk_audio_wav.py`, the TCP one-liners), so it is ~2 tickets plus a stream-contract
version bump, and it buys a nicer broadcast-FM listen.

**Recommendation: no — and remove the question from the ADR.** If you ever miss it on a broadcast
station, it is an ordinary ticket filed then, not a contract decision made now.

**Decided (user, 2026-09-23): yes — against this recommendation.** Stereo audio is part of decoding
the signal; it stays in ADR-0015 (§12.13: a `stereo_decode` block and the `channels` wire change,
placeholder ids LP-9/LP-10).

**Blocks.** Nothing.

---

## The eleven not being asked, and why

Six of these were escalated because the ADR's author was unsure, not because you have a stake; two
are duplicates; two are numbers; one is forced by another answer. **They are decided here.** Overrule
any of them the same way you would answer the five above.

### D1 — Discovered templates stay local JSON files; no share format is designed now
*(§10 Q4. Same answer applies to ADR-0016 Q5, signature sharing.)*
CLAUDE.md's own rule settles it: *"don't over-build plugin infrastructure for hypothetical
contributors"* against *"don't design anything that rules out sharing later."* A versioned, immutable
JSON file with a `provenance` block **already is** an export format — sharing one is copying a file.
Designing a share protocol for one friend-user before the engine exists is the exact inversion.
**Not your call; it is a YAGNI judgement the project's own conventions already make.**

### D2 — Pipeline rows are materialised lazily, not one per emitter
*(§11.10 Q1.)*
§11.1 already states that an emitter with no pipeline row reads as an implicit `energy` hypothesis,
which is precisely today's behaviour — so every reader must handle zero rows regardless, and eager
rows buy uniformity a reader cannot rely on anyway. Against that: CLAUDE.md's signal model says
candidates churn continuously, created and deleted, so "a row per box" is a write per box per churn
cycle on a battery device. **Lazy. Engineering detail.**

### D3 — A pipeline claims a *set* of output kinds; promotion conflicts per kind
*(§11.10 Q2 and §12.12 Q5 — **the same question asked twice**, and §12.8 already answers it.)*
§11.1's single-valued `output_kind` cannot describe ADR-0011 §8.9's FM recipe, which has two outputs
(audio + RDS messages). §12.8 corrects this to `output_kinds`, a set, with the rule *"no two promoted
pipelines on one emitter may claim the same output kind."* That is not a preference, it is forced by
a recipe that already exists. **Both questions should be deleted from the ADR and the correction
folded into §11.1 by CP-1's implementing task.**

### D4 — Receiver artifacts are hidden by default, behind a filter that already exists
*(§11.10 Q3.)*
§11.8 already specifies `?artifacts=hidden|all` on `/api/inventory`. So this is a **default value
behind an existing query parameter** — reversible in one line, and it gates nothing. The real use
("see the front end misbehaving") is served by the filter plus listing an emitter's attributed
artifacts in its own detail view, not by putting images and intermods in the top-level inventory
where they compete with real signals. **Hidden by default. UI default, not a contract.**

### D5 — A class-changing retune still ends the audio stream; the client reconnects
*(§12.12 Q1.)*
§12.6 already decided this in the same amendment: making an audio pipeline survive a class-changing
retune is *"a deliberate non-goal here"*, because it needs the content-class derivation to re-run
mid-pipeline and there is no test behind a change to live-visible behaviour. The question contradicts
a decision two paragraphs above it. **Keep today's tested behaviour; delete the question.**

### D6 — The legacy Listen chain is kept permanently, for SSB/CW
*(§12.12 Q3.)*
Forced by U4-A: if SSB and CW never get blocks, the chain that serves them cannot be deleted. There
is no independent answer here. If you pick U4-B instead, this flips to "delete it after stage 7", and
LP-8's scope widens with it.

### M1 — The 4-bit supersession margin and the 0.6 channel-overlap fraction are measurements
*(§11.10 Q4, and the unverified note under §11.10.)*
Both were escalated because they are first guesses, which makes them a measurement problem and not a
preference. They belong to **T-547** (does the evidence-bits ladder survive 8-bit quantisation and
gain state?) and to the real FM capture plus the adjacent-station guard case the ADR already names.
**Measure them in T-547 unless you want to overrule a number sight-unseen.**

### M2 — The `quick`/`standard`/`deep` budgets are measurements
*(the numbers half of §10 Q2.)*
3 s / 20 s / 120 s are described in the ADR as *"unverified guesses"*. **T-552** exists to measure
what one candidate evaluation actually costs on the M1 blocks, and M-13 re-baselines on the Jetson.
The only part of §10 Q2 that was ever yours is the battery policy, which is folded into **U2**.
**Measure them in T-552.**

---

## The decision table

Five questions. Say "yes" to take all five recommendations, or name the ones you want changed.

**Decided 2026-09-23 (user, relayed by the supervisor): U1 B, U2 B, U3 A, U4 A, U5 yes — U5 against the recommendation.**

| # | The question, in one line | Recommended answer | What it blocks | Decision (user 2026-09-23) |
|---|---|---|---|---|
| **U1** | Does one clean burst (an ADS-B squitter passing CRC-24) confirm a signal by itself? | **Yes — but only when the check was template-fixed and ≥ 24 bits.** Searched checks keep the three-frame rule. Plus: set the false-confirm budget at **≤ 1 wrong Confirmed emitter per week unattended** | M-9, CP-4, T-548. All three can proceed on this as a stated assumption | **B, as recommended** — one frame confirms only on a template-fixed check ≥ 24 bits; budget ≤ 1 wrong Confirmed emitter per unattended week (already in ADR-0022) |
| **U2** | May the device start analyze jobs on its own, and may `deep` run on battery? | **Yes, auto-queue at `quick` only, on mains only, one job at a time. `deep` stays user-triggered.** Not A — a device that only explains what you pointed at is a workbench | M-3, M-11. M-3 can proceed with an `auto_profile` field defaulted off | **B, as recommended** — auto-queue at `quick` only, mains only, one job at a time; `deep` user-triggered and refused on battery |
| **U3** | You labelled it; a valid CRC decode disagrees. Who wins? | **You win (rank 0); the decode is recorded and shown beside your label, not applied.** This closes ADR-0016 Q3 too | CP-4, ADR-0016's ladder. Nothing is stalled — this is what the code already does | **A, as recommended** — the user wins (rank 0); the decode is recorded and shown beside the label. ADR-0016 Q3 closed |
| **U4** | Fund blocks for generic PSK, SSB and CW, or accept the gaps? | **Fund PSK only** (M-14 becomes required — without it MAUTO can never reach bits on PSK). **SSB/CW stay on the legacy chain permanently**, written in as a decision | M-14's priority, LP-8's scope, D6 | **A, as recommended** — fund `psk_demod`, M-14 required (its block already landed as T-609); SSB/CW legacy permanently |
| **U5** | Do you want stereo audio? | **No.** Re-file it as an ordinary ticket if you ever miss it | Nothing | **Yes — overrides the recommendation.** The user wants stereo audio; it is part of decoding the signal. Kept in ADR-0015 (§12.13) |

**Decided in this brief, not asked:** D1 templates stay local files · D2 pipeline rows are lazy ·
D3 `output_kinds` is a set (two duplicate questions deleted) · D4 artifacts hidden behind the existing
filter · D5 retune still ends the audio stream · D6 legacy chain kept (follows U4).
**Sent to measurement:** M1 the 4-bit margin and 0.6 overlap → T-547 · M2 the search budgets → T-552.

**Once the five are answered:** ADR-0015 can go to review for ACCEPTED, which is M-1's gate, which is
the whole MAUTO engine graph. **Answered 2026-09-23; ADR-0015 is ACCEPTED (§16), and the MAUTO
M-1/M-3 chain proceeds.**

---

## Sources

- [ADR-0015](adr/0015-decoder-synthesis-contracts.md) §§1–10 (contracts, search, budgets, confirm-by-decode),
  §10 open questions; §11 candidate-pipeline amendment (T-220) and §11.10; §12 Listen-as-audio-pipeline
  amendment (T-221) and §12.12. All fifteen questions are quoted or paraphrased from those three lists.
- [ADR-0016](adr/0016-classification-contracts.md) open questions 3 (merged into U3) and 5 (answered by D1).
- [ADR-0011](adr/0011-decoder-workbench-contracts.md) §8 (audio sink, `audio` output kind) — the source of D3's forcing case.
- [docs/15](15-decoder-synthesis.md) (the design brief behind ADR-0015); [docs/17](17-burst-recall-vs-open-set.md) (the precedent for a priced decision brief with no floor moved).
- `docs/tasks.yaml`: T-547, T-548, T-549, T-550, T-551, T-552 (the MAUTO design/research track that M1/M2 are handed to).
- CLAUDE.md, "Product constraints" (one developer, one friend-user, handheld power budget) and "Product vision" (workflow #4) — the two constraints U2, U4 and D1 are decided against.
