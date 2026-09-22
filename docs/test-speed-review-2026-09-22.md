# Test-suite speed review, 2026-09-22

An independent look at why the merge gate went from ~11 min to ~36 min. Everything below is
measured: from `$HACKRIFF_OPS/merge-runner.log` (per-suite and per-test durations, 64 complete
workspace runs from 09-19 to 09-22), `$HACKRIFF_OPS/gate-timings.jsonl`, and scoped `cargo nextest`
runs in a worktree at `CARGO_BUILD_JOBS=2`.

**Headline: the suite did not grow. Test count went 2302 → 2499 (+8.6 %) while the workspace run
went 347 s → ~1000-2000 s (3-6x).** Seconds per test went 0.151 → 0.434. The user's instinct —
"I don't think we added many more tests" — is exactly right, and it is the most important fact in
this document.

---

## 1. Where the 36 minutes go

### 1.1 The four gate suites

Median over the **38 runs in the log that completed all four suites** (`just gate`, class `full`):

| suite | median | mean | range | share |
|---|---|---|---|---|
| `just lint` | 62 s | 63 s | 5-198 s | 3 % |
| `just test` | **1085 s** | 1102 s | 423-1869 s | **56 %** |
| `just acceptance-ci` | 308 s | 310 s | 165-650 s | 16 % |
| `just test-ui-e2e` | 477 s | 560 s | 418-2059 s | 25 % |
| **total** | **32.5 min** | 33.9 min | 19.6-58.3 min | |

And it is still climbing. Same figures, first ten complete gates vs last ten:

| | lint | test | acceptance-ci | ui e2e | total |
|---|---|---|---|---|---|
| first 10 | 44 s | 720 s | 190 s | 448 s | **23.4 min** |
| middle 10 | 64 s | 1078 s | 310 s | 462 s | **31.9 min** |
| last 10 | 80 s | 1440 s | 311 s | 620 s | **40.8 min** |

Note what this does to the numbers already in `docs/10`: the "gate median 21.4 min" from T-543 is
**all classes pooled** and is now two days stale. The `full`-class median is 32.5 min and the
current ten are 40.8 min.

### 1.2 Inside `just test` (build vs run)

`just test` = `nextest-config-check` + `test-rust` + `test-doc` + `test-py` + `test-ui`. Paired
across the 44 passing runs in the log:

| component | median | how measured |
|---|---|---|
| cargo builds (`Finished 'test' profile in …`) | 40-350 s, median ~90 s | the two `Finished` lines |
| **nextest workspace run** | **700-1200 s (latest: 1997 s)** | the `Summary [Ns]` line |
| `just test-py` (pytest, 305 tests) | 15 s | pytest's own footer |
| `just test-ui` | skipped when no `ui/` path in the diff | the gate says so |
| **unaccounted** | **median 272 s, often 350-500 s** | `test` duration − the above |

That 272 s is the nextest **list** phase (234 test binaries each spawned with `--list`) plus
`just test-doc`. `test-doc` runs `cargo test --workspace --exclude hk-e2e --doc` over **19 crates
and finds 2 doctests** (`hk_blocks` 1, `hk_recipe` 1); every other crate prints
`0 passed; 0 failed`. Nobody has ever measured what that step costs. It should be instrumented
before it is changed, but a quarter of `just test` currently has no owner at all.

### 1.3 `just acceptance-ci`

Two nextest invocations, `hk-e2e` in the `e2e-bounded` group at max-threads = 6:

| run | tests | wall | CPU-s | occupancy at 6 threads |
|---|---|---|---|---|
| M0 slice (`acceptance_m0`) | 50 | 157-286 s | 789-1446 s | 84 % |
| harness (10 targets) | 32 | 84-100 s | 331-494 s | 66-83 % |

This suite is the best-behaved of the four: it is genuinely CPU-bound in a well-filled pool.
Cutting its *work* (§3.3) cuts its wall clock almost proportionally.

### 1.4 `just test-ui-e2e`

13 spec files, driven by `ui/e2e/run.mjs`, **one shared `hk serve` backend that starts in 1.6-5.7 s**,
and then a plain `for (const f of files)` loop — **strictly sequential**. Per-spec, from the log:

| spec | s | | spec | s |
|---|---|---|---|---|
| canvas-journey | 172.8 | | app-trace | 32.7 |
| live-edge | 106.9 | | surface-contention | 23.7 |
| surface-nav | 59-84 | | surface-retune | 22.5 |
| surface-colour | 55.5 | | surface-load | 2.8-13.1 |
| fog-of-war | 50.3 | | surface-address | 9.9 |
| scan-everything | 47.4 | | app-surface | 9.2 |
| | | | surface-region | 6.5 |

Sum ≈ 600 s, which is the whole suite. The backend costs 2 s of it. **This is 10 minutes of the
gate spent running one browser at a time on a 28-core machine.**

### 1.5 The top-20 slowest tests

Median over every run in the log in which the test appears (`n` = number of gate runs seen).
"Cause" is from reading the test.

| # | s (median) | n | test | cause |
|---|---|---|---|---|
| 1 | 259.2 | 84 | `hk-estimate::receiver_lines::t394_the_stations_pilot_and_subcarrier_survive_the_discriminator` | surveys the 216 MB `capture-2026-09-15-fm-band` through blind estimation. **Was 81.6 s before 09-21 04:55** (§2.2) |
| 2 | 122.0 | 64 | `hk-classify::below_gate_absorption::a_family_below_its_gate_is_missed_and_not_replaced` | 21 classes × 4 SNR offsets × **24 seeds** = ~2000 synthesise-and-classify trials. Was 48.9 s |
| 3 | 111.0 | 85 | `hk-classify::open_set_stats::t248_alternative_open_set_statistics…` | same shape of sweep. Was 38.0 s |
| 4 | 100.7 | 88 | `hk-classify::accuracy_sweep::per_family_and_per_snr_accuracy_meets_the_a_priori_floors` | 31 classes × 5 offsets × 6 trials. Was 34.0 s |
| 5 | 83.3 | 9 | `hk-e2e::acceptance_m0::t118_occupancy::…48h_markov_scene` | generates + replays a 48 h synthetic scene |
| 6 | 82.0 | 10 | `hk-e2e::acceptance_m0::report_scene::report_over_48h_scene…` | the same 48 h scene, generated again |
| 7 | 66.7 | 47 | `hk-store::history::tests::live_coarse::a_checkpoint_does_not_swallow_the_row_it_closes` | ingests hundreds of stream-seconds through a 64-node lattice |
| 8 | 66.2 | 9 | `hk-e2e::listen_live::listen_on_a_live_paced_device_streams_audio_through_re_refinement` | **real-time-paced** replay, by design |
| 9 | 63.8 | 47 | `hk-store::…live_coarse::a_restart_keeps_the_open_tiles_rows_in_every_coarse_node` | same lattice ingest |
| 10 | 62.9 | 19 | `hk-cli::api_contract::coverage_survives_a_refused_iq_ring_and_never_calls_the_lost_evidence_grey` | **61.2 s in ISOLATION too.** Waits on a 60 s epoch-aligned rollup boundary — the exact defect T-383 fixed in its sibling `coverage_greys_only…`, which now costs 0.83 s |
| 11 | 62.9 | 46 | `hk-store::…live_coarse::per_arriving_row_fold_work_is_bounded_and_does_not_grow_with_node_count` | lattice ingest |
| 12 | 61.2 | 85 | `hk-classify::verifier_gain::family_level_answers_are_bit_identical_with_and_without_the_verifier` | runs the whole grid twice. Was 20.5 s |
| 13 | 57.7 | 47 | `hk-store::…live_coarse::a_viewport_read_triggers_no_tile_generation_at_any_level` | lattice ingest |
| 14 | 56.4 | 47 | `hk-store::…live_coarse::committing_a_row_writes_nothing…` | lattice ingest |
| 15 | 52.7 | 87 | `hk-classify::open_set_diag::t248_why_each_held_out_generator_is_claimed` | sweep. Was 17.0 s |
| 16 | 50.4 | 50 | `hk-classify::higher_order_clock_lock::the_order_8_gate_admits_8psk…` | sweep |
| 17 | 49.3 | 10 | `hk-e2e::acceptance_m0::scene_48h::scene_48h_replays_time_compressed…` | the 48 h scene a third time |
| 18 | 48.1 | 86 | `hk-api::tile_ceiling::the_predicate_agrees_with_the_read_over_the_whole_lattice` | 12 × 15 lattice walk on an empty store — and **serialised** (§3.1) |
| 19 | 44.9 | 78 | `hk-store::history::tests::byte_budget_respected_with_coarse_coverage_kept` | store ingest |
| 20 | 43.3 | 85 | `hk-pipeline::attention::tests::attention_baseline_default_cap_does_not_refuse_a_parked_week_at_9700_cells` | a week of cells |

Two families dominate: **hk-classify's parameter sweeps** (six entries, all of which roughly
tripled at one instant) and **hk-store's `live_coarse` lattice ingests** (six entries, ~350 s
between them).

---

## 2. What actually happened (and it is not what docs/10 says)

### 2.1 The heavy-serial group is STILL 100 % of the workspace run's critical path

`.config/nextest.toml`'s T-763 note says *"THE CRITICAL PATH HAS MOVED OUT OF THE GROUP"* and names
five binaries outside it as the lever. **That is wrong, and the arithmetic it replaced was right.**

I reconstructed `heavy-serial` membership from the filtersets and summed its tests' durations per
run. In **17 of 17** reconstructable workspace runs the nextest wall clock equals the group's
serial sum to under one second:

| tests | wall | heavy-serial sum | everything else | else ÷ 7 threads |
|---|---|---|---|---|
| 2302 | 347 | **345** | 963 | 138 |
| 2332 | 389 | **384** | 964 | 138 |
| 2380 | 493 | **492** | 1257 | 180 |
| 2403 | 756 | **756** | 3428 | 490 |
| 2425 | 706 | **706** | 2400 | 343 |
| 2485 | 895 | **895** | 2657 | 380 |
| 2481 | 1016 | **1016** | 3060 | 437 |
| 2499 | 1997 | **1996** | 8523 | 1218 |

`max-threads = 1` means the group runs start-to-finish with no gaps, so wall == sum is what a
critical path looks like. And "everything else" fits in the free 7 threads with slack in **every**
run (1218 < 1996 even in the worst). The five heavy binaries T-763 named are not on the critical
path — they are the *load* that inflates it.

Both statements are true at once and they are not in tension: shrinking the five sweeps is a
lever, but it works by relieving contention, not by shortening a path they sit on. Which matters,
because it changes what "done" looks like: you cannot verify the fix by watching those binaries get
faster, only by watching the `heavy-serial` sum come down.

### 2.2 The step change on 09-21 04:55 was CONTENTION, not new tests and not new work

Timestamped workspace runs show a clean, permanent step:

```
09-21 04:10  n=2388  wall= 397
09-21 04:30  n=2395  wall= 462      <-- last "normal" gate
09-21 04:55  n=2403  wall= 756      <-- the step
09-21 05:30  n=2415  wall= 702
...                                  (never returns)
09-22 09:55  n=2499  wall=1084
09-22 11:13  n=2499  wall=1997
```

Diffing the two runs test by test:

- **9 tests were added. Their combined cost is 0.0 s** — nine `hk-model::retune` / `hk-model::emitter`
  unit tests, each under 5 ms.
- **2282 s of inflation landed on tests that did not change.**

Per crate, before → after, for tests present in both runs:

| crate | before | after | × |
|---|---|---|---|
| hk-recipe | 1 s | 40 s | **79×** |
| hk-sim | 1 s | 20 s | 40× |
| hk-dsp | 16 s | 326 s | **20×** |
| hk-core | 5 s | 96 s | 18× |
| hk-gnss | 2 s | 22 s | 12× |
| hk-stream | 2 s | 19 s | 10× |
| hk-detect | 43 s | 234 s | 5.5× |
| hk-demod | 27 s | 100 s | 3.7× |
| hk-estimate | 181 s | 548 s | 3.0× |
| hk-classify | 302 s | 819 s | 2.7× |
| hk-cli | 129 s | 324 s | 2.5× |
| hk-pipeline | 348 s | 757 s | 2.2× |
| hk-api | 221 s | 214 s | 0.97× |
| hk-store | 553 s | 521 s | 0.94× |

`hk-recipe`'s tests got **79× dearer** and `hk-dsp`'s **20×**. Neither crate was touched, and
neither contains anything that could take 20× longer for a code reason — they are small,
CPU-bound unit tests. A 79× inflation of a 1-second crate is a scheduler artefact, not a
regression. The whole box became oversubscribed at that gate and has stayed that way.

**This is the single biggest cause of 11 → 36 minutes.** It is also the least visible, because no
individual test looks broken.

One caveat, stated plainly: I could not identify *what* changed on the box. Gate-time `loadavg`
samples are not higher after the step (09-20 median 23.9, 09-21 10.5, 09-22 14.7) — but those are
sampled at gate start/end, not during, so they measure the wrong thing. The orchestration migration
landed at 09-21 01:03 (`1b07811d`, role-based `.claude/`) and the work-runner's build-pressure cap
at 09-21 00:00 (`eaa772fb`, T-559); either could have changed how many agents build concurrently
during a gate. **Somebody should sample load *during* a gate before tuning anything else** — this
is worth more than every other item in §3 combined.

### 2.3 A secondary, real code cost landed in the same merge (worth a ticket of its own)

The 09-21 04:55 gate was a bulk merge of five branches including **T-589** (`b3ba6bc8`, "C14's
digital-structure gate could not see an order-8 alphabet"). Inside the general inflation, one set
of tests moved much more than its crate's average:

| test | before | after |
|---|---|---|
| `hk-estimate::receiver_lines::t394_…` | 81.6 s | 258.7 s (+177) |
| `hk-classify::below_gate_absorption::…` | 48.9 s | 133.0 s (+84) |
| `hk-classify::open_set_stats::…` | 38.0 s | 109.4 s (+71) |
| `hk-classify::accuracy_sweep::…` | 34.0 s | 99.8 s (+66) |
| `hk-classify::verifier_gain::…` | 20.5 s | 66.8 s (+46) |

T-589 added `let (c8, _, _, n8) = carrier_line(plans, &xon, fs, 8);` **unconditionally** to
`hk_estimate::blind` — a third carrier-line search where there were two — and, by its own commit
message, turned a previously-skipped stage on: *"the stage now RUNS on 11 of 11 that reach psk-qam,
against 0 before"*. Every test above runs blind symbol estimation thousands of times.

I have not separated this from the ambient contention (that needs an A/B build across `b3ba6bc8`),
and the two are confounded. But **this is on the real-time path, not just in tests**, and nothing in
the ticket measured its cost. That is the part that should worry somebody more than the gate does.

### 2.4 `OnceLock` fixture sharing does not work under nextest

Eight test files share an expensive fixture through a `static OnceLock` — `fm_band_2026_09_15.rs`,
`canvas_fidelity.rs`, `signal_062.rs`, `signal_087.rs`, `t254_ism_bursts.rs`, `m2_scene.rs`,
`m3_grid.rs`, plus `hk-pipeline::gnss_l1_dwell` and `hk-context::aware_006_e2e`. **nextest runs each
test in its own process**, so a `OnceLock` is initialised once *per test*, not once per binary. The
sharing is silently a no-op, and has been since T-631 moved `hk-e2e` onto nextest.

The log proves it: if the cache worked, one test per module would be slow and the rest instant.
Instead every test in a module costs the same:

| module | tests | per-test medians |
|---|---|---|
| `acceptance_m0::fm_band_2026_09_15` | 6 | 19.2, 19.1, 19.0, 18.9, 18.9, 18.6 |
| `canvas_fidelity` | 5 | 16.6, 16.5, 16.5, 16.4, 16.4 |
| `acceptance_m0::aware_053` | 6 | 17.1, 16.7, 16.2, 16.1, 11.0, 0.0 |
| `acceptance_m0::overlap` | 5 | 20.7, 16.6, 16.1, 15.9, 14.9 |
| `acceptance_m0::latency` | 4 | 16.6, 16.2, 16.1, 11.8 |
| `acceptance_m0::tutorial_rds` | 4 | 23.2, 18.9, 15.5, 5.8 |
| `acceptance_m0::inventory_lifecycle` | 3 | 15.8, 14.3, 4.6 |
| `concurrent_demod` | 4 | 17.8, 16.9, 2.4, 0.0 |

`fm_band` replays a **216 MB** recording six times per gate to assert six different things about the
one replay. In total ~330 s of the acceptance suite's ~790 CPU-s is re-doing work a `OnceLock` was
written to do once. This is precisely the "load sample data once and share it" the user asked about,
and the mechanism is in place — it is the runner that defeats it.

### 2.5 `heavy-serial` contains three binaries with no shared resource

`filter = 'package(hk-api) or package(hk-cli)'` is a blanket, and the file's own justification for it
is "bind real TCP/WebSocket ports and spawn the daemon process". Three hk-api binaries do neither:

| binary | `TcpListener::bind` / `TcpStream::connect` / `local_addr()` hits | gate cost |
|---|---|---|
| `tile_ceiling` | **0** | 123 s |
| `tile_no_generation` | **0** | 35 s |
| `tile_cost` | **0** | 13 s |
| (every other hk-api binary) | 4-14 | |

They call `hk_api::tiles::tile_read` in-process against a `TempDir` named with `process::id()` +
`thread::id()`. They are expensive — a 12 × 15 lattice walk on an empty store, deliberately — but
`max-threads = 1` was serialising pure CPU work against itself, **on the critical path**.

### 2.6 `hk-cli::api_contract`: 311 s of critical path, ~50 s of intrinsic work

54 tests, all spawning a real `hk serve` over the mock SDR. Run scoped in a worktree at
`--test-threads 1`, 44 of them completed before an unrelated failure stopped the run:

- **32 tests cost 23 s between them** (0.7 s each) — they read static reference data
  (`/api/taxonomy`, `/api/signatures/match`, cluster/recipe shapes) and assert JSON shape.
- **11 tests cost 300 s** — they `wait_for` real pipeline or rollup progress.

In the gate the same binary costs 311 s and its cheap tests read 16-18 s each. So most of
`api_contract`'s critical-path cost is contention, not work — and the part that *is* work is
concentrated in 11 tests, one of which (`coverage_survives_a_refused_iq_ring…`, 61 s) is the same
epoch-aligned-rollup defect `.config/nextest.toml` already documents T-383 fixing in its sibling.

---

## 3. Recommendations, ranked by minutes saved per unit of effort

### R1. Find out what is oversubscribing the box during a gate — then fix that first

- **Mechanism.** §2.2: untouched crates got 20-79× dearer at one instant with zero new work. The
  gate is competing with concurrent agent builds for the same 28 cores, and the suite's wall clock
  is the `heavy-serial` sum, i.e. exactly the tests that are latency-bound.
- **Evidence.** The per-crate inflation table; `hk-recipe` 1 s → 40 s.
- **Saving.** If per-test cost returned to its 09-21 04:30 level, the workspace run goes ~1085 s →
  ~460 s: **~10 min off the gate**, more than every other item here combined.
- **Effort.** Low to measure (sample `uptime` + `pgrep -c cargo` every 30 s for one gate and write
  it beside `gate-timings.jsonl`), unknown to fix — it may be a policy call about how many agents
  run while a gate runs.
- **Risk.** None to what the tests prove. The risk is the opposite: continuing to "fix" individual
  tests for a problem that is not in them.

### R2. Parallelise the ui-e2e spec loop

- **Mechanism.** `ui/e2e/run.mjs` runs 13 spec files one at a time against one shared backend that
  costs 2 s. Replace the `for` loop with a bounded pool (3-4).
- **Evidence.** §1.4: 600 s of specs, longest 172.8 s. At 4-way the suite is bounded by
  canvas-journey: ~200-250 s.
- **Saving.** **~6 min.**
- **Effort.** Small — the runner already isolates each spec in its own process with its own timeout
  and process-tree kill, so the pool is a scheduling change, not a redesign.
- **Risk.** Moderate and specific: specs share one backend, so anything asserting on
  *global* server state could interfere. `surface-contention` (asserts `/api/tiles` backpressure)
  and `live-edge` are the two to check first; if either is order-sensitive, pin it to run alone and
  pool the other eleven (still ~5 min).

### R3. Move the hk-classify sweeps and the `live_coarse` lattice tests out of the merge gate

- **Mechanism.** Six hk-classify sweep tests (823 s median between them) run full parameter grids —
  `below_gate_absorption` alone is 21 classes × 4 offsets × **24 seeds**. Keep a deterministic
  subset in the gate (e.g. `SEEDS = 24 → 6`, the count the file's own comment says T-429 measured
  as adequate for stability) and run the full grid in a nightly tier. Same for `hk-store`'s six
  `live_coarse` ingests (~350 s).
- **Evidence.** §1.5; hk-classify is 2291 CPU-s (21.8 %) of the workspace run and hk-store 1137 s
  (10.8 %) — and by §2.1 that CPU is what starves the critical path.
- **Saving.** ~900-1200 CPU-s removed, which by §2.1's mechanism should return a large part of the
  `heavy-serial` inflation: plausibly **4-8 min**, but this one must be measured, not predicted.
- **Effort.** Medium. A `#[ignore]`-plus-nightly-recipe split, or an env-gated seed count.
- **Risk.** Real: fewer seeds means fewer chances to catch a per-class regression, and the sweeps
  are how the a-priori accuracy floors of ADR-0016 §7 are defended. Mitigate by keeping the *cells*
  (family × SNR) and shrinking only the trials, and by making the nightly failure loud.

### R4. Make the `OnceLock` fixture caches actually shared

- **Mechanism.** §2.4. Two options: (a) persist the replay result to a content-hash-keyed cache
  under `target/` (the same shape `hk_e2e::synth` already uses for the Python generator) so the
  second process loads instead of replays; or (b) merge each module's tests into one `#[test]` with
  many assertions.
- **Evidence.** The per-module uniform timings table; `fm_band` replays 216 MB six times.
- **Saving.** ~330 CPU-s of the acceptance suite → **~1 min** of gate wall clock (the suite is
  already well-parallelised at 6, so the wall saving is much smaller than the CPU saving).
- **Effort.** Medium for (a) — `blind_replay` returns a `BlindRun` holding a temp dir, so the cache
  has to cover the directory, not just the summary. Small for (b), but it costs per-assertion
  granularity, which is real: right now a red name tells you which property broke.
- **Risk.** (a) is a caching bug waiting to happen if the hash misses an input — it must key on the
  fixture bytes *and* the pipeline config *and* the binary's own mtime. (b) loses diagnosis quality.
- **Note.** Prefer (a). And whichever way it goes, `OnceLock`-per-process is now a trap the codebase
  will fall into again — it deserves a line in `docs/10`.

### R5. Narrow `heavy-serial` to the binaries that actually share a resource — **IMPLEMENTED**

- **Mechanism.** §2.5. Drop `tile_ceiling`, `tile_cost`, `tile_no_generation` from the group.
- **Evidence and measurement**, this worktree, same box, same flags, 10 tests:

  | | wall | CPU |
  |---|---|---|
  | in `heavy-serial` (max-threads = 1) | **243.9 s** | 195 s |
  | ungrouped, `--test-threads 3` | **116.4 s** | 189 s |

  Identical CPU; 52 % less wall. In the gates themselves these three were 171 s **of the critical
  path**, and the 7 free threads they move onto had idle capacity in every run measured.
- **Saving.** ~170-240 s, i.e. **~3 min**, one-for-one off the workspace run.
- **Effort.** Four lines of `.config/nextest.toml`.
- **Risk.** Near zero. Grep-verified: zero `TcpListener::bind` / `TcpStream::connect` /
  `local_addr()` in all three; temp dirs are per-process-and-thread. What they prove is unchanged —
  they run the same assertions, just not one at a time.

### R6. Fix `coverage_survives_a_refused_iq_ring…` the way T-383 fixed its sibling

- **Mechanism.** 61.2 s **in isolation** — it waits for the next epoch-aligned 60 s spectrum-history
  rollup. `.config/nextest.toml` already documents the identical defect in
  `coverage_greys_only…`, fixed by asking over a window the pyramid can answer (58 s → 0.7 s). That
  sibling now costs 0.83 s; this one was never done.
- **Saving.** ~60 s off the critical path, **~1 min**.
- **Effort.** Small, and the pattern is already written down.
- **Risk.** Low — the sibling's fix is the proof it can be done without weakening the assertion. It
  is also a *correctness* fix: a test whose pass depends on where in the wall-clock minute it starts
  is a flake generator, which is exactly what T-383 found.

### R7. Measure `just test-doc` and the nextest list phase

- **Mechanism.** §1.2: a median 272 s of `just test` is unattributed, and one known component runs
  `cargo test --doc` over 19 crates to execute **2 doctests**.
- **Saving.** Unknown — possibly 2-4 min, possibly little. That is the point.
- **Effort.** Trivial: time the two steps in `test-rust`/`test-doc` and append them to
  `gate-timings.jsonl` like the four suites already are.
- **Risk.** None. Do not narrow `--doc` to the two crates that currently have doctests — that
  silently stops testing the next one written.

### R8. Cheap-test / expensive-test split in `hk-cli::api_contract`

- **Mechanism.** §2.6: 32 of 54 tests cost 0.7 s and only need a server on an ephemeral port; 11
  cost 300 s waiting on pipeline progress. Splitting the file into two binaries lets the cheap one
  leave `heavy-serial` while the waiting one stays.
- **Saving.** Modest on its own (~20-40 s of critical path) and largely subsumed by R1. Listed
  because the *diagnosis* matters: it is evidence that most of `api_contract`'s 311 s is contention.
- **Effort.** Medium — a 10 213-line file with a shared helper block.
- **Risk.** Low, but it is churn in the file `docs/api.md` contract changes have to touch (T-079).
  Not worth doing before R1.

---

## 4. What this contradicts

1. **`.config/nextest.toml`, T-763: "THE CRITICAL PATH HAS MOVED OUT OF THE GROUP".** It has not.
   Run wall clock equals the `heavy-serial` serial sum to under one second in 17 of 17
   reconstructable runs, including the most recent (1996 s of 1997 s). The five binaries T-763
   names are the *load*, not the path. The consequence is practical: the note tells the next
   reader that `test-threads` tuning "rests on" arithmetic that is no longer true, when in fact
   that arithmetic is exactly as true as it was — 8 still beats 28 for the same reason.
2. **`.config/nextest.toml`, T-763: "All five tripled in cost at one gate (+8 tests)".** True, but
   incomplete in a way that misdirects: at that same gate **`hk-recipe` went up 79×, `hk-dsp` 20×
   and `hk-core` 18×**, and the 8 new tests cost 0.0 s. Reading it as "those five binaries got
   dearer" points at the sweeps; reading the whole table points at the machine.
3. **`docs/10` / CLAUDE.md, "gate median 21.4 min" (T-543).** Two days stale and pooled across
   classes. `full`-class median is 32.5 min over 38 complete runs; the last ten are 40.8 min.
   Worse, of the 44 `just test` runs in the log, **the ones that reach all four suites are a
   minority** — most gates abort at the first failure, so pooled medians are biased low.
4. **`.config/nextest.toml`, "hk-api … bind real TCP/WebSocket ports".** Three of its nineteen test
   binaries bind nothing at all, and they were the three most expensive.
5. **The implicit belief that a `OnceLock` shares a fixture across a binary's tests.** Under
   nextest it shares nothing. Eight files rely on it.
6. **"We added tests."** Test count +8.6 %, wall clock +200-500 %. Seconds-per-test went
   0.151 → 0.434.

---

## 5. If only three things get done

1. **R1** — measure load *during* a gate, and decide how much of the box a gate gets. ~10 min.
2. **R2** — pool the ui-e2e spec loop. ~6 min, small change.
3. **R3** — sweeps to a nightly tier with a deterministic in-gate subset. ~4-8 min, needs care.

R5 is already committed on this branch and is worth ~3 min for four lines. R6 is worth ~1 min and
fixes a latent flake. Together these are the difference between a 36-minute gate and something
closer to 12.
