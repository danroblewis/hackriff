# 10 — Test strategy

*Architecture planning, Phase 5. Drafted 2026-09-13; revised 2026-09-13 (user feedback: e2e through the SDR device interface, blind ground truth). Status: **PROVISIONAL.** The use cases in [docs/05](05-use-cases-and-explorations.md) / [`use-cases.yaml`](use-cases.yaml) are the acceptance targets; this document says how an ID becomes tests and defines the `test_tier` rubric now filled into the YAML.*

The guiding rule from CLAUDE.md and [docs/04 §11.2](04-radio-engineering-and-signals-analysis.md): **build end-to-end tests that replay IQ through the full pipeline and assert on detections, estimated parameters, decoded bits, and inventory entries**, with unit tests under the DSP blocks. Two user rules (2026-09-13) sharpen it: **the IQ enters through the SDR device interface, never as a file handed to the pipeline** (§1.1), and **the system under test never sees the ground truth** (§3.1). Because measurements are immutable and interpretations are versioned ([docs/07](07-data-model.md)), the same fixture re-run after an algorithm change is a regression test for free.

## 1. Test tiers

| Tier | What it covers | Runs in CI? | Example |
|---|---|---|---|
| **T1 DSP unit** | One block/kernel against known input/output | Yes | FFT/PSD correctness; OS-CFAR false-alarm rate on synthetic noise; M2M4 SNR estimate |
| **T2 component** | One capability end to end in isolation | Yes | noise-floor tracker on a drifting-floor fixture; channelizer channel isolation; SQLite inventory upsert/query; device-interface conformance of the mock and HackRF sources |
| **T3 end-to-end IQ replay** | A SigMF recording served by the **mock SDR device** (§1.1) and driven through the whole system via the device/control interface, detecting blind and asserting on Detection / Demodulation / Decode / Emitter / Explanation rows against a hidden truth list (§3.1) | Yes | mock device loaded with an ADS-B capture → system surveys and finds 1090 MHz itself → assert CRC-valid message count, one aircraft Emitter, "ADS-B" in the top-k explanations |
| **T4 synthetic scenario** | Generated IQ with known ground truth (emitters, noise, interference, overload), served by the same mock device, exercising detection→classify→estimate→inventory | Yes | TorchSig/own generator: 3 emitters + FM blocker at −10 dBFS → assert all found, blocker flagged, false-alarm under bound |
| **T5 hardware-in-the-loop (HIL)** | **The same T3/T4 acceptance suite** with the device set to the real HackRF against live air (receive-only), plus live capture correctness and timing/throughput | No (nightly on a bench rig / manual) | FM broadcast band blind survey through the real HackRF (T-053); sustained-rate drop test (spike S2); TX→RX loopback only once C37 is un-gated |
| **T6 field** | Real over-the-air conditions that cannot be captured-and-replayed meaningfully | No (opportunistic, logged) | a real Sporadic-E opening; walking a DF fix; a solar flare SID event |

T1–T4 are the **CI backbone** and need no hardware. T3/T4 are where use-case acceptance lives. T5/T6 confirm what only real hardware or real propagation can. T1/T2 may feed in-memory buffers or files straight into a block; **from T3 up, IQ only enters through the device interface.**

### 1.1 The device interface is the test seam

E2E and acceptance tests drive the system exactly as a user with real hardware would: through the generic SDR device/control interface (T-048), not by handing a recording to the pipeline. The interface is generic, with no HackRF specifics in the core, so a SoapySDR-backed device can be added later without touching tests ([SoapySDR](https://github.com/pothosware/SoapySDR)). It covers open, capabilities, tune, sample rate, baseband filter, **named gain stages** (LNA/VGA/amp on HackRF), optional bias-tee, optional sweep mode, start/stop RX, sample timestamps and overrun/drop reporting. A trait-level conformance test that every device must pass keeps the mock honest against the real source.

The **mock SDR device** (T-049) implements that interface and replays SigMF IQ (recorded or synthetic) behind it, honouring control changes realistically:

- **Retune inside the recording's coverage** serves that band: the mock shifts, filters and decimates the recorded IQ.
- **Retune outside coverage** serves calibrated noise and raises an out-of-coverage flag in Provenance, so a test can't pass by tuning somewhere the recording doesn't cover.
- **Sample-rate changes** resample.
- **Gain changes** scale the samples and clip at 8 bits, setting overload flags just as the HackRF would.
- **Bias-tee and sweep mode** are modelled, sample timestamps are emitted, pacing is real-time or accelerated, and overruns can be injected.
- **Limits (unverified until HIL):** gain on a recording is digital scaling only. The mock can't model the front end's noise figure or the intermodulation a real LNA produces at high gain, so those claims stay T5/T6.

Because a test only names a device, the same test runs against the real HackRF by switching the device selection (e.g. `HK_DEVICE=hackrf`, T-053). The HIL run is receive-only, one HackRF user at a time, and its results are logged without gating CI.

## 2. `test_tier` rubric (filled into use-cases.yaml)

Each use case gets one **primary** `test_tier` — the highest-fidelity tier at which its core claim can be asserted **cheaply and repeatably**. Most acceptance is T3/T4 (offline IQ replay through the mock device); the value says whether a use case is CI-testable offline or needs hardware/field/data.

| Value | Meaning | Assigned when |
|---|---|---|
| `offline-synth` | Validated with **generated** IQ + hidden truth (T4, plus T1/T2 underneath) | The signal can be synthesised faithfully enough (most detection, parameter-estimation, classification, modem, and framing use cases) |
| `offline-recorded` | Validated by **replaying a recorded/public SigMF** fixture through the mock device (T3) | A real signal we can capture once (or fetch from a public archive) and replay — most decoder use cases (ADS-B, AIS, rtl_433 devices, pagers, sats) |
| `hil` | Needs the **HackRF in the loop** (T5) | TX/own-link use cases, and live-behaviour/throughput/timing claims not capturable as a static file |
| `field` | Needs **real OTA conditions** (T6) | Propagation, space-weather, DF/localization, passive radar, moving-emitter, and "explain a real event" use cases |
| `data-only` | Validated against **cached feed/archive data**, no IQ | `data-only` fit_flag use cases (spot-archive mining, dataset/toolkit items, external-feed correlation with frozen caches) |

Rules when several could apply, most-CI-friendly first: prefer `offline-synth` if we can generate the signal; else `offline-recorded` if we can capture/fetch it; else `hil` (own TX/live) ; else `field`; `data-only` only when no local IQ is involved. Attack-map/event-correlation use cases are `offline-synth` at the pipeline level (frozen cache + synthetic anomaly, per [docs/07 §5.2](07-data-model.md)) even though the real event is `field` — the test asserts the correlation logic, not the weather. `offline-*` tests are HIL-ready by construction (§1.1), so a `hil` run of an offline use case is extra confirmation, not a different test.

## 3. Fixtures

- **Format: SigMF** everywhere ([docs/03 §1.6](03-sdr-software.md), [SigMF spec](https://github.com/sigmf/SigMF)) — `.sigmf-data` + `.sigmf-meta` with ground-truth **annotations** (time/frequency boxes, labels, expected decodes, the `hackriff:truth` field in [sigmf-extension.md](sigmf-extension.md)). A fixture is self-describing and doubles as a Recording object ([docs/07 §2.12](07-data-model.md)). The annotations are for the test harness only; §3.1 says how they are kept from the system.
- **Sources, with licences checked before use** (tracked in [ADR-0010](adr/0010-language-and-licence-ledger.md)):
  - **Own captures** — the primary source; captured with the HackRF One per the [docs/12](12-implementation-plan.md) fixture plan, annotated, committed as fixtures. Licence: ours.
  - **Public SigMF / IQEngine archives, sigidwiki samples** — for signals we can't easily generate; **check each set's licence** (many are CC-variants) before committing.
  - **Synthetic (TorchSig / own generator, MIT)** — for T4 scenarios with exact ground truth and impairments (IQ imbalance, spurs, overload, fading) matching real front-end effects ([docs/03 §4.1](03-sdr-software.md)).
  - **RadioML** — usable for algorithm prototyping only, **not** as product acceptance (documented dataset errata, non-commercial licence, [docs/04 §5.3](04-radio-engineering-and-signals-analysis.md)).
- **Size & storage:** IQ fixtures are large; keep them out of the main Git history. **Git LFS** for fixtures under a size cap, or an external fixture store fetched by a script with checksums. CI pulls only the small fixtures it needs; large HIL/field captures live in the external store. Decided concretely in [docs/12](12-implementation-plan.md).
- **Synthetic-first for CI:** generated fixtures are tiny to store (a generator seed + params), so most T4 scenarios are code, not files.

### 3.1 Ground truth is hidden (blind acceptance)

Acceptance tests prove the system **finds and explains** signals on its own. They don't check that it can look up a frequency it was already told about (T-047).

- **Truth list per fixture.** Every fixture, recorded or synthetic, carries a list of its known-interesting emissions: frequency, bandwidth/extent, time span and label. Recorded fixtures get theirs from annotation; synthetic ones get theirs from the generator.
- **Stripped before replay.** The mock device serves only IQ plus capture metadata (centre, rate, gains, provenance), with annotations and truth removed. The harness asserts that the system never opens the truth file.
- **Blind detection, then two asserts per truth emission:**
  1. It was **detected**, within frequency/extent/time tolerances.
  2. A reasonable **explanation appears among the top-k recommended explanations** (ranked output from T-039). k and the tolerances are set once in the harness; the values are provisional until T-047 lands.

  Emissions the system finds beyond the truth list are allowed, within a false-alarm bound.
- **Perturbed variants.** Each key case gets a variant the database can't answer, so a lookup can't pass it. For example, the `fm_100p8M` FM station shifted +150 kHz in a synthetic variant must still be detected, keep "FM broadcast" in its top-k, and be **flagged off-raster**. Other useful perturbations: off-allocation placement (AWARE-053 `unexpected-here`), level changes into overload, and truncated or overlapping bursts.
- **The known-signal database only recommends.** Band plans, licences and signal databases rank candidate explanations and set `known`/`unexpected-here` ([docs/07 §2.11](07-data-model.md)). They are never truth, never tell the scheduler where to tune in a test, and never pre-populate the inventory.
- **HIL truth.** Live air has no fixture metadata, so the T5 run derives its truth list from an independent survey of an always-occupied band (FM broadcast, T-053). It is logged, not gated.

### 3.2 Anti-patterns (review rejects these)

- **Direct file feeding.** A T3+ test constructs the pipeline from a SigMF reader or sample array and bypasses the device interface. Use the mock device instead.
- **Lookup-and-tune.** A test (or test-only code path) reads a frequency from the known-signal DB, band plan or truth list, then tunes there or asserts only there. Tuning comes from the system's own survey/dwell scheduler ([ADR-0005](adr/0005-survey-dwell-scheduler.md)); the truth list is only read by the asserting harness, after the run.
- **Demo seeds in serving.** `hk serve`/`hackriffd` pre-populate the inventory or explanations from seed data or the DB. Normal serving has no demo seeds: a fresh server with no input has an empty inventory. Seeds live only inside unit tests (T-042).
- **Truth leakage.** The pipeline reads annotations, `hackriff:truth` or truth filenames, or a test passes because the fixture's label reached the system.
- **DB-as-truth.** A test asserts a label is correct only because it matches the database entry for that frequency, with no perturbed variant.

### 3.3 Temp-directory lifetime (T-229)

Every replay/test-harness data directory is created by `hk_cli::pipeline::temp_data_dir()` under the system temp dir as `hk-replay-<pid>-<unix_nanos>-<n>` and holds the run's database, IQ ring (`ring.ci8`, potentially many GB) and recordings. Its lifetime:

- **Created:** by `temp_data_dir()`, on every call — production (`hk serve`/`hk replay`/`hackriffd` without an explicit `--data-dir`) and test call sites alike.
- **Removed on a normal run:** call sites wrap the directory in `hk_cli::pipeline::TempDataDirGuard`, an RAII guard. Dropping it during a normal return removes the directory recursively, tearing down the IQ ring with it.
- **Kept on failure:** if the guard drops while its thread is unwinding from a panic (a failed `assert!`/`assert_eq!`), it leaves the directory in place instead and prints its path to stderr, so a developer can inspect the run's database, IQ ring and recordings that produced the failure. A run that is killed outright (SIGKILL, a hard abort) leaves its directory too — no Drop runs at all.
- **Swept when stale:** `temp_data_dir()` also runs (once per process) a sweep of `hk-replay-*` orphans in the system temp dir, removing only those whose embedded pid names no currently-live process **and** whose contents are older than one hour (comfortably past this repo's slowest replay test, ~20 minutes) — reclaiming directories a crashed or killed run couldn't clean up itself. Both conditions gate every removal: a live pid is never touched regardless of age (other agents and sessions run tests on this machine concurrently), and age alone is never trusted because a dead run's pid can be reused by an unrelated live process before the sweep runs — that directory is then left behind (a rare, safe residual: it is reclaimed once the pid that reused it also exits) rather than risk removing something live.
- **Proof:** `crates/hk-cli/tests/temp_dir_hygiene.rs` asserts a representative set of normal replay runs leaves no net new `hk-replay-*` directories, that a panicking guard keeps its directory, and that the sweep's pid-alive/age gates behave correctly — the last two against a private fixture directory the test creates and destroys itself, never the live system temp dir. `every_temp_data_dir_call_site_is_guarded` (T-232) is a structural regression guard: it greps every `.rs` file in the repo and fails if a `let ... = temp_data_dir()` binding isn't followed by a `TempDataDirGuard::new` within a few lines, so a future call site (like the one T-217 added without one) can't slip past review again.

#### T-232: a race with detached server threads, not a killed child process

A full-suite run after T-229 still grew `hk-replay-*` orphans (30 → 56 in one coordinator measurement), all with dead pids, containing a live server's data (`baselines/`, `captures/`, `control-audit.jsonl`, `hackriff.db`+wal, `iqbuffer/ring.ci8`) rather than a bare test scratch dir — including two pids that had each left exactly 25 directories, one fixed set of tests apiece. The leading hypothesis was a spawned `hk serve` **child process** getting SIGKILLed at test teardown, so its `TempDataDirGuard`'s `Drop` (an in-process RAII destructor) could never run.

**That hypothesis was refuted.** None of the hk-cli tests that produced these orphans (`api_contract.rs`, `control_http.rs`, `iq_buffer_allocation_http.rs`) spawn an OS child process; all call `hk_cli::serve::start()` **in-process**, which runs the pipeline and `hk_api::http::Server` on threads within the test's own process. The real mechanism, confirmed by direct reproduction on this shared dev machine under real concurrent-agent load:

- `hk_api::http::Server::shutdown()` (its `Drop`) joins only its accept-loop thread. Each accepted connection is handled on its own **detached** thread (spawned, `JoinHandle` discarded) that "finishes on its own" per the `Server` doc comment — never joined by `shutdown()`.
- A test that spawns a server, makes HTTP calls, stops the pipeline and drops both the server and its `TempDataDirGuard` in the same scope can race one of those detached connection threads: it can still be touching a file under the guarded directory (the audit log, the ring, SQLite's WAL, the observation log) when `remove_dir_all` walks it, which then fails (`ENOTEMPTY`/similar on the entry that reappeared mid-removal) — an error the pre-T-232 `Drop` silently swallowed (`let _ = ...`), leaving the directory behind.
- This race is rare in isolation (an isolated single-test run under `cargo test --exact` never reproduced it, including under ~20 ambient load average from other agents) but was routine under a full `hk-cli` nextest run on this loaded, shared machine: **every** server-spawning test (26/26) left an orphan, one per pid, before the fix — closely matching the coordinator's original "two pids, 25 each" observation (very likely two other concurrent agents' own `api_contract` runs hitting the identical race at the same time).

**Fix (in scope: `crates/hk-cli/src/pipeline.rs` and hk-cli test helpers only, not `hk-api`; the window was later shortened to ~180 ms by T-236 below):** `TempDataDirGuard::drop` retries `remove_dir_all` on failure with a bounded backoff (`REMOVE_RETRY_BACKOFF`, ~7 s total) before giving up, absorbing the race without requiring every crate that spawns a thread near a data directory to join it first. `iq_buffer_allocation_http.rs`'s one unguarded call site (a bare `remove_dir_all` a panic could skip entirely) was also wrapped in the guard.

**Verdict: bounded lag, not an unbounded leak — with one known, rare, self-healing residual.** Across repeated full-suite-scale measurements after the fix (two `cargo nextest run -p hk-cli` runs, one full-workspace `just test-rust` run of 1530 tests, and `just acceptance`), the leak dropped from 26/26 server-spawning tests to at most **one** residual directory per run, always the same test (`api_contract.rs`'s `observation_coverage_reports_interactive_tuning_without_a_scheduler`, the one test that reads back from the observation log over HTTP *after* stopping the pipeline but *before* dropping the server) and always a much smaller artifact (just its `observations/` log subtree, never a full server dir). It never reproduced in five isolated repeats of that single test. Because the residual's pid is dead by the time anyone looks and the sweep's one-hour age floor comfortably exceeds any test's runtime, this residual is reclaimed automatically within an hour — a bounded lag. Fully closing it at the root needs `hk_api::http::Server::shutdown()` to track and join its per-connection threads too; that's out of this task's scope (hk-cli and test helpers only) and is left as a follow-up for whoever next works on `hk-api`'s `Server`. Set `HK_DEBUG_TEMP_DIR_RETRY=1` to print each retry attempt's error and the directory's remaining entries if this needs re-diagnosing.

#### T-236: the root cause closed, and the residual turned out to be drop order

**The root cause is fixed.** `hk_api::http::Server::shutdown()` no longer leaves connection threads detached: the server registers every accepted connection (its id and a cloned socket handle) and, after joining the accept thread, closes each open socket and waits on a condvar until the last handler thread has retired, bounded by `SHUTDOWN_DRAIN_TIMEOUT` (2 s). Closing the sockets first is what makes that wait sub-millisecond in practice — a handler blocked on a stalled peer, or parked in `bridge::watch_peer` on a WebSocket that never closes, fails its read at once instead of holding shutdown for its own (much longer) socket timeout — so the budget only has to cover a handler in the middle of *compute*. A thread still running at the deadline is abandoned, counted (`Server::abandoned_connections()`) and reported on stderr; shutdown never hangs. The wait runs on the shutdown path only, and the registry lock is taken solely around a connection's registration and retirement — never during request handling, and never by the capture or audio path. Proof: `crates/hk-api/tests/shutdown.rs` (removal of the handlers' directory succeeds on the first attempt over ten repeats with unread requests in flight; a stalled client cannot extend shutdown; a handler stuck in compute is abandoned at the deadline and counted) and `hk_cli::serve`'s `shutdown_releases_the_data_dir_on_the_first_removal_attempt`, which does the same through a real `hk serve` run's data directory.

**The residual was not that race.** Re-measured under a *private* `TMPDIR` (essential on this shared machine: other agents' concurrent runs write `hk-replay-*` into the system temp dir, and the leaking test cannot be attributed without it), the surviving orphan was still one per `api_contract` run — but with `HK_DEBUG_TEMP_DIR_RETRY=1` **no removal attempt ever failed**. The directory was removed successfully and then recreated. Cause: `let (serving, addr, _dir_guard) = start_server();` — the bindings of one `let` drop in reverse order, so the guard bound last dropped *first*, removing the data directory while the server and pipeline were still alive; their teardown (`ObservationLog`'s `Inner::drop` seals the open hour, and `append_bytes` does `create_dir_all`) then recreated `observations/<date>/` under the deleted path. No retry window could ever have fixed that. `start_server` now returns the guard **first** (`(TempDataDirGuard, Serving, SocketAddr)`) so it drops last, and the hk-cli test harness now joins the waiter threads it spawns for `handle.wait()` (which drop the pipeline handle, and so tear its stores down, *after* sending their result).

**Result:** zero new `hk-replay-*` directories from a full `hk-cli` run (57 tests) and from `api_contract` alone, measured under a private `TMPDIR`, against exactly one per run before — the documented residual is gone, and `REMOVE_RETRY_BACKOFF` shrank from ~7 s to ~180 ms, kept only as a backstop for the unjoined tail (pipeline writer threads, SQLite's WAL). `every_temp_data_dir_call_site_is_guarded` still guards the convention.

### 3.4 "Leaky" tests: what a nextest `LEAK` line means here (T-257)

nextest waits a short period (200 ms by default in 0.9.144) after a test process exits for that process's stdout/stderr pipes to close. If they are still open it prints `LEAK` for that test. **A leaky test is still a pass, and the run's exit code is still 0** unless a `leak-timeout = { result = "fail" }` policy is configured. `.config/nextest.toml` sets no leak policy, and none should be added.

T-257 was filed on the belief that `hk-plugins::bin/hk-plugin-readsb tests::spawned_args_always_carry_no_fix_and_never_fix` left a `readsb` child alive, and that this made `just test` exit 1. **Both halves are false**, and the measurements that settle it are worth keeping:

- **That test spawns nothing.** It asserts over a local array of the fixed arguments; it never calls `spawn_readsb`.
- **A leaky run exits 0.** Twelve `-p hk-plugins` runs at `--test-threads 16`: five leaky, every one `exit=0` — matching nextest's documented default.
- **The leaked test is random.** Across ~35 instrumented runs, `LEAK` landed on a dozen different tests across four binaries and two crates, never the same one twice in a row — including pure parsers (`manifest::tests::validation_errors`), SQLite `ingest` tests, and, decisively, **`hk-core`** (`source::hackrf::tests::clipping_marks_the_tune_state_overloaded_until_the_next_gain_change`), a crate that contains no `Command`, `fork` or `posix_spawn` anywhere in `src/` or `tests/`.
- **So the holder is a sibling test process, not a leaked child.** Within one nextest run the only processes are the runner and its test binaries, so an exited test's pipe can only be held by another test process. macOS has no `pipe2(2)`: `std` creates a pipe with `pipe(2)` and sets `FD_CLOEXEC` in a second syscall, and a spawn on another runner thread inside that window inherits the fd. The sibling then holds the exited test's pipe, and nextest reports `LEAK` against the innocent test.
- **It scales with spawn concurrency, not with what a test does.** Zero leaks in 25 `hk-plugins` runs at `--test-threads 6` and in a full 1754-test workspace run at `--test-threads 8`; 6 of 12 runs at 16; 6 of 10 at this machine's default (28). **T-436 has since made 8 the configured default** (`[profile.default] test-threads` in `.config/nextest.toml`), for an unrelated reason — rotating wall-clock-timing failures at 28 — so this measurement is now a second, independent reason for the same number, and the leak-free regime is what a normal run gets. CI keeps one-per-CPU (`[profile.ci]`), which on a 2–4 vCPU runner is well inside it too.
- **No orphan processes, in tests or in production.** `pgrep -fl readsb` / `hk-dummy-plugin` after every one of 50+ runs found nothing. The plugin host's teardown is not implicated: it SIGKILLs the whole process group, blocks in `wait_exit_unreaped` before reaping so no pid can be reused, and joins the output readers with a timeout so a descendant that escaped the group (`setsid`) can delay neither restart nor shutdown (`crates/hk-plugins/src/host.rs`; `review_fixes.rs` covers both grandchild cases).

**Verdict: a runner-side artifact, not a test bug and not a product defect.** Don't read a `LEAK` line as a leaked child, and don't silence it with a leak policy in `.config/nextest.toml` — that would hide the genuine orphaned-child case this project has actually been bitten by, which is exactly the class of pin T-228 was filed to remove. If a `LEAK` line ever does coincide with a real stray, `pgrep -fl <child>` straight after the run is the discriminator. And **if `just test` exits 1 on an all-passing Rust summary, the failure is in one of the other three steps the recipe chains** (`test-doc`, `test-py`, `test-ui` — `test-ui` runs `npm ci`, so it needs the network); `just` names the failing recipe on its last line. Re-measured on main at `1346085`, all four steps exit 0.

### 3.5 Quarantined load-sensitive tests — the list is empty (T-504 opened it, T-493 closed it)

At least five agents in one session independently spent runs re-measuring the same two `hk-plugins::host` failures and each concluding "pre-existing, not mine". T-504 quarantined them with `#[ignore]` so a sixth wouldn't have to. **T-493 then found what was actually wrong, fixed it, and deleted both markers, so this list is now empty.** Keep the section: the next quarantine goes here, under the same rules, and the finding below is the one to check before re-diagnosing anything in `hk-plugins`.

**What it turned out to be — and what it was not.** It was **not** §3.4's macOS `pipe(2)`/`FD_CLOEXEC` race. That race is real and T-493 reproduced it directly (11 leaked pipe write ends in 16 000 probes with 8 concurrently spawning threads in one process, 0 with 6), but it can only make nextest print `LEAK`, which is a pass — and it needs two spawns racing *inside one process*, which these tests, one per nextest process and spawning sequentially, never do. Baseline runs confirmed the split: at `--test-threads num-cpus` under load, runs came back "16 passed (3 leaky)" — leaks without failures.

The failures were the **plugin host's own `nice`**. `plugins/*/manifest.json` ship `limits.nice = 10` so a decoder can never starve capture, and the host applies it with `setpriority(PRIO_PGRP, …)` immediately after the spawn. Every one of these tests then bounded, with a wall clock, how long a *deliberately de-prioritised* subprocess takes to produce output — on the machine CLAUDE.md commits to running four concurrent builders on. Nice +10 is a promise that the process runs when nothing else wants the CPU; on this box something always does, so that quantity has no upper bound at all.

Traced, with timestamps inside `run_once`: the child was created in **193 µs** (`posix_spawn` returned; the host's stdout reader was running at +320 µs) and then **executed nothing for 30 s** — not even `hk-dummy-plugin`'s first `eprintln!`, which runs before it reads a byte of stdin. The test's 2 ms polling thread ran throughout and the host had already enqueued everything (`records_offered: 100, records_enqueued: 100, decodes: 0`, empty log tail). Whole-suite runs with the plugin binaries relinked cold before each:

| `limits.nice` in the test manifests | load average | tests failing per run |
|---|---|---|
| `10` (as shipped) | 9–11 | 10, 1, 12, 0, 4, 10, 0 |
| `0` (the fix) | 9–129 | 1, 1, 0, 0, 0 |

Note the load averages: removing the de-prioritisation survived 129 while keeping it failed at 9. Not a load threshold, not a thread count, not a retry — the tests simply stopped timing something the scheduler never promised to deliver, which is §3.2's and T-383's rule (*a test may not bound a quantity whose natural range it has not measured*). The test manifests set `nice = Some(0)`, not `None`, so the host still calls `setpriority` and that path stays exercised; the shipped manifests are untouched.

**The product finding this leaves behind, which is not a test problem:** a decoder nice'd to 10 on a busy machine really can be starved for 30 s, and the host's own 10 s `stall_timeout` would kill and restart a perfectly healthy plugin for it. That became T-540 and is fixed: the host now splits that one budget in two on an **observation** — the child's first byte of output — so a child that has not run yet is judged against `startup_timeout` (60 s) and one that has against `stall_timeout` (10 s), each kill naming its budget (`crates/hk-plugins/tests/startup_budget.rs`).

**Rules for the next entry here (T-436, T-383).** A cap or a skip makes a defect rarer or invisible, not fixed. So a quarantine is `#[ignore]` with its measurement and its owning ticket inline, never a deletion, a looser bound or a new `heavy-serial` member; each stays runnable by the exact command in its own `#[ignore = "…"]` message. Add a test here only with its own isolation-vs-load measurement — a suspicion is not evidence — and never quarantine a test that turned out to be a real, fixed defect (e.g. `tests/e2e/tests/app-trace.e2e.mjs`, `surface-region.e2e.mjs`), which would hide a regression instead of a runner artifact.

## 4. From a use-case ID to tests

A use-case ID becomes one or more test cases that assert on the **data-model objects** it should produce. Every T3/T4 case loads its fixture into the mock device, lets the system survey and detect blind, and then asserts against the hidden truth list. Worked pattern (matching the [docs/07 §5](07-data-model.md) examples):

- **SIGNAL-001 ADS-B** (`offline-recorded`): mock device loaded with a recorded 1090 MHz SigMF fixture; the system finds the emission itself → assert (T3) the truth emission is detected, N CRC-valid Decode messages, ≥1 aircraft Emitter with a hex identity, "ADS-B" in the top-k explanations, and inventory `last_seen` updated. Underneath: T1 preamble correlation, T2 the readsb plugin wrapper.
- **SIGNAL-062 FM broadcast** (`offline-recorded` + synthetic variant): mock device serving `fm_100p8M` → the station is detected at its true frequency, auto-mode selects WFM, and "FM broadcast" is in the top-k. The +150 kHz shifted variant must also be detected, keep "FM broadcast" in its top-k, and be flagged off-raster. No test tunes to a DB frequency.
- **AWARE-036 unknown ISM burst** (`offline-synth`): generate an FSK sensor burst train (T4), served through the mock device → assert Detections matching the truth bursts, one unknown Emitter with `known_status: unknown`, estimated symbol rate ±1%, recovered framing, and (if seeded with a CRC) a valid Decode flipping status toward identified.
- **AWARE-053 allocation priors** (`offline-synth`): the same emitter placed on- and off-allocation → the priors recommend the allocation in the top-k and set `known` vs `unexpected-here`. The truth list, not the database, says which placement is off-allocation.
- **AWARE-006 GNSS jamming attack-map** (`offline-synth`): synthetic L1 noise-floor rise served through the mock device + a frozen gpsjam ExternalEvent → assert an Anomaly and a top-ranked Explanation of type time-coincidence/geometry; assert no Explanation when the cache lacks a matching event. The frozen feed is context input, not truth.
- **SPACE-050 noise-floor survey** (`field` primary, `offline-synth` for the pipeline): T4 asserts the calibrated floor is recovered within ±1 dB on a synthetic capture served through the mock device, including its out-of-coverage noise and gain/clip behaviour; the science claim itself is confirmed in the field (T6).

Each acceptance test names its use-case ID(s) and its fixture's truth list, so coverage is traceable both ways; the Phase 7 tasks carry the IDs they satisfy. Existing acceptance tests that feed files directly or use lookup-and-tune are audited and rewritten under T-047.

## 5. CI without hardware, and what only the field can verify

- **CI (no hardware):** T1 unit, T2 component (including device-interface conformance), T3 replay of committed/LFS fixtures through the mock device, T4 synthetic scenarios through the mock device, and the frozen-cache correlation tests. This covers the great majority of use cases (all `offline-synth`, `offline-recorded`, `data-only`). CI asserts detections, estimated parameters, decoded bits/messages, and inventory/explanation rows blind against hidden truth — the full pipeline, deterministically.
- **Nightly/bench HIL (T5):** the **same acceptance suite** run with the real HackRF selected (receive-only, T-053), plus sustained-rate tests on a bench rig. It is triggered manually or on a self-hosted runner with the radio, one HackRF user at a time, and its results are recorded as dated reports (not gating CI).
- **Field (T6):** propagation, space-weather, DF, passive-radar, and "explain a real event" claims. These are validated opportunistically, logged as dated field reports with the SigMF capture attached, and their *pipeline* portion is still covered offline. They never gate CI.
- **Provenance in tests:** every replayed fixture carries provenance so tests can assert that overload/spur flags propagate (the trust requirement, [docs/07 §2.6](07-data-model.md)). The mock device's gain-clip and out-of-coverage flags exercise this path deterministically.

## 6. Coverage bookkeeping

`test_tier` is filled for all 398 use cases in `use-cases.yaml`. Distribution:

| test_tier | Count | In CI (no hardware)? |
|---|---:|---|
| `field` | 143 | No — opportunistic, logged; pipeline portion still tested offline |
| `offline-recorded` | 92 | Yes (T3) |
| `offline-synth` | 80 | Yes (T4) |
| `data-only` | 53 | Yes (frozen-cache / archive) |
| `hil` | 30 | No — bench rig |

**225 of 398 (57%) are CI-testable with no hardware** (`offline-synth` + `offline-recorded` + `data-only`); 173 need hardware or the field (`hil` + `field`). The `field` count is high because SPACE/PROP science use cases anchor on their real-world physical claim (the SPACE-050 rule in §4) even when the underlying algorithm is separately synth-testable — for those, the *pipeline* is still covered offline and only the science claim is field-confirmed.

- The first vertical slice ([docs/11](11-roadmap.md)) deliberately picks use cases that are `offline-synth`/`offline-recorded` (six of seven) so the whole chain is provable in CI before any field work. M0b moves those acceptance tests behind the mock device, makes them blind (§1.1, §3.1), and runs them once against the real HackRF.
- Regenerate this table when `test_tier` values change: `python3 -c "import yaml,collections; print(collections.Counter(u['test_tier'] for u in yaml.safe_load(open('docs/use-cases.yaml'))['use_cases']))"`.

## Sources

- [SigMF specification](https://github.com/sigmf/SigMF): fixture format, captures and annotations.
- [SoapySDR](https://github.com/pothosware/SoapySDR): the vendor-neutral device API the generic interface should be able to back later. Its mapping onto our interface is unverified until a SoapySDR device is added.
- Internal: [docs/07](07-data-model.md) (objects asserted on), [ADR-0005](adr/0005-survey-dwell-scheduler.md) (where tuning decisions come from), [sigmf-extension.md](sigmf-extension.md) (`hackriff:truth`), [tasks.yaml](tasks.yaml) T-039, T-042, T-047, T-048, T-049, T-053.

### Software acceptance / user-simulation field check (user, 2026-09-16)

**This is not a radio test and not calibration.** It is a **software acceptance suite**: it drives the
**live app through browser instrumentation** against real hardware and ambient AM/FM, to prove the
software works end to end as a user experiences it. The radio is the *fixture*, not the subject.

**The simulated user session:**

1. Open the app.
2. Tune to — or **search for** — a region likely to hold good signals.
3. Sit a while, and confirm **candidate *and* confirmed** signals appear.
4. Confirm they can be **tuned to**.
5. Confirm **decoding works**. **RDS on FM is the headline** — prove we decode the RDS sideband. Where
   automated decoding exists, capture and verify its output.
6. Confirm **no duplicate bands appear** — live dedup correctness.

**It is a curated subset of the existing e2e and unit assertions**, reused where they map, run against
the **live app via the browser** rather than against the mock device. Assume AM/FM present as the
signal source.

Because it drives the real product rather than the analysis, it may tune directly to a known region
without violating the no-lookup-and-tune rule: that rule protects *blind detection* from being handed
its answer, and this suite is not testing detection's blindness — it is testing that the software
works. **Keep it separate from, and labeled apart from, the blind suites**, whose value comes entirely
from that discipline.

- **Tier: productized T5/T6 (HIL / field). It never gates CI.**
- **New dependency: a browser-automation harness (Playwright).**
- **Placement: after or alongside M7.** Not scheduled now.

*Note: step 6 is the same property the user is currently seeing fail live (see T-369, T-390) — a field
check that asserts it would have caught it.*

*Superseded framing: an earlier version of this note, written from a truncated message, described this
as an "equipment self-test" and implied frequency calibration. That framing is withdrawn by the user:
a frequency-reference check is a minor optional extra at most, and is not the point.*

### The merge gate: `just gate` (T-396, user 2026-09-16)

**Run `just gate`.** It inspects the diff, classifies it, prints the decision, and runs exactly the
suites that class needs. The rule lives in the runner, not in each agent's judgement, so the same diff
gets the same gate whoever ran it - which is the difference between a convention and a gate.

| classification | suites |
|---|---|
| files only under `ui/` (and not `crates/` or `docs/api.md`) | `just test-ui` |
| anything touching `crates/` or `docs/api.md` | `just lint` + `just test` + `just acceptance-ci` |
| `docs/` only | nothing - see below |
| `py/` only | `just lint-py` + `just test-py` |
| **anything else** | **the full gate** |

**Classification fails closed.** A path matching no class runs the full gate, never the cheapest. This
is the repo's own principle applied to its own tooling: `BiasTee::Unknown` is not `Off`,
`Coverage::Unobserved` is not quiet, `Encryption::Unknown` is not clear - nothing said is never
permissive. Three cases a naive version gets wrong, each deliberate:

- **`fixtures/` is full.** It is acceptance *input*, and the suites read fixture metadata (T-317,
  T-373 and T-382 all changed it).
- **The `justfile` and `.github/` are full.** They *are* the gate. A gate that could classify a change
  to itself as cheap could certify its own weakening.
- **`py/hkpy/gate.py` and `py/tests/test_gate.py` are full**, not `py`-only, for the same reason.

`tests/`, `plugins/`, `.config/`, `recipes/`, `Cargo.*`, repo-root files and any new top-level
directory are unclassified and therefore full.

**The ui-only case keeps its original reasoning**: the thin-client architecture means all signal logic
lives in the backend, the backend is contract-tested (T-079), and **no acceptance test drives the
browser** - the e2e suites run through the **device interface**. So a ui-only change cannot move the
Rust signal path, and running those suites against one proves nothing while costing minutes per
iteration. It gets `just test-ui` (npm ci + build + `tsc --noEmit` + the `ui/test` suites); this repo's
`just lint` is lint-rust + lint-py and has no JS/TS linter, so the UI's lint equivalent is the
typecheck already inside `test-ui`.

**`docs`-only runs nothing, and says so.** There is no link checker and no markdown linter in this
repo. The gate prints that rather than implying a check it does not perform; adding a dependency to
satisfy a word in a table would be the wrong trade.

**Where the decision comes from.** By default: the merge base with `main` (not `main` itself - a moved
main makes unrelated files look changed) plus everything uncommitted, including untracked files.
`--merge` (see below), `--base REF`, `--staged`, `--worktree` and `--files a b c` override it;
`--dry-run` prints the decision and runs nothing. In GitHub Actions, a pull request uses `origin/$GITHUB_BASE_REF`; a **push** has no
base worth trusting and runs the full gate, so `main` - the branch everything else is measured against
- is always verified whole. Renames and deletions count as touching the path (both sides of a rename).
An empty diff is a printed no-op, not an accidental full run.

In practice, **an agent runs `just gate` in its task worktree**, where the default source is exactly
the branch's own diff. Untracked files count as changes on purpose - a new `newdir/thing.rs` nobody
has `git add`ed is still a change, and leaving it out is the one way this could fail open - so a stray
scratch file forces the full gate. That is visible rather than mysterious: it is printed as a deciding
file, and `--base REF` or `--files …` overrides it.

#### Which source the coordinator's per-merge gate uses: `just gate-merge` (T-424, decided 2026-09-17)

**The decided rule.** There are two subjects, so there are two sources, and each command answers one
question:

| who | command | question it answers | source |
|---|---|---|---|
| an **agent**, checking its own work in a task worktree | `just gate` | *what is in my tree that isn't on main?* | merge base with `main` **+ everything uncommitted, untracked included** |
| the **coordinator**, at merge, from inside `git merge --no-ff --no-commit` | **`just gate-merge`** | *what will this merge put on `main`?* | **the merge index vs HEAD** (`git diff --cached`) |
| CI | `just gate --phase …` | unchanged | PR: merge base with `origin/$GITHUB_BASE_REF`; push: the full gate |

**Why the coordinator needed a different source.** The main checkout permanently holds untracked files
that can never be committed: `tools/` (the user's HackRF experiments, excluded by CLAUDE.md) and
diagnostic SigMF captures under `fixtures/`. Both are unclassified-or-`fixtures/`, both therefore force
the full gate, and both are *always there* - so the diff-aware gate degraded to **always-full in the
one tree it was built to help**. Measured while merging T-415: a single-file ADR text edit was charged
`lint + test + acceptance-ci` (~7 min) instead of `docs` (nothing). T-409, ui-only, would have been
charged the same instead of ~10 s.

**The weakening argument, stated rather than assumed.** `--merge` classifies a **strict subset** of the
paths the default classifies, and `classify()` is monotone - any `full` path forces `full` - so a
smaller input set can only ever produce a **cheaper or equal** gate. It is a narrowing, and T-396's own
rule is that the gate must not certify its own weakening. Three things make this one sound:

1. **It is not "ignore some files"; it is "classify the right diff."** The gate's guarantee is about
   what lands on `main`. During `git merge --no-ff --no-commit`, **git itself** builds the index as the
   merge result versus HEAD, and `git commit` commits the index. So the index *is* the change being
   certified. Untracked files are not in it and cannot reach `main` through that merge. For this
   subject the default source is the one that is wrong: it classifies *what is in my tree*, which is a
   different question. The default stays right for an agent, where untracked means
   **not-yet-`git add`ed but about to be committed** - there, untracked genuinely is part of the change.
2. **The one real counterexample does not survive contact.** An untracked fixture *can* change what a
   suite sees, because the suites read the **working tree**. But classifying untracked paths as `full`
   never protected against that: the stray file changes the suite's result whenever that suite runs, at
   whatever class was chosen. Fail-closed on untracked paths therefore buys **cost, not safety**, for
   the merge subject - it makes a contaminated run expensive, not clean. Working-tree contamination is
   a property of *where the suites run*, not of *how the diff is classified*, and the honest mitigations
   are a clean checkout and CI, not a more expensive classification. (The narrow sub-case - an untracked
   fixture that would break acceptance, on a `docs`-only merge that skips acceptance - is not a hole
   either: a `docs`-only merge skips acceptance **by design**, under any source.)
3. **The narrowing is guarded, and the guards are the part that is tested.**
   - **It requires an in-progress merge.** `--merge` checks `MERGE_HEAD` (or `SQUASH_MSG`, for
     `git merge --squash`). Without one, nothing guarantees the index is a merge result, so it **forces
     the full gate** rather than classifying whatever happens to be staged. A blanket `--staged` would
     not have this property, which is why the recipe is `--merge` and not `--staged`.
   - **Nothing is silent.** Every uncommitted path it did *not* classify is **printed**, with the class
     it would have had (`tools/fm_rx.py [would be full] unclassified path — the gate fails closed`).
   - **There is no ignore list, and no path is exempt.** `fixtures/` staged into a merge is still
     `full`; so is `tools/` if it is ever actually committed. The only thing that changed is *which set
     is classified* - which was the explicit constraint on this ticket, because a classifier with a
     silent ignore list is precisely the shape of self-weakening the fail-closed rule exists to prevent.

**What misuse would let through, and how it shows.** Running `just gate-merge` when the index is
deliberately partial (staged half a change, no merge in progress) is caught by guard 3 and runs full.
Running it inside a real merge while the tree holds an unstaged edit to something the suites read - say
a modified-but-unstaged `.config/nextest.toml` - classifies without it, so a suite that the default
would have run may not. That path is printed in the `outside` list every time, and the residual risk is
**delayed discovery of a problem that exists only in the coordinator's tree**, never a change reaching
`main` unverified. Periodic `just lint` + `just test` + `just acceptance-ci` by hand remains the
backstop, as it already was.

**Not done here, on purpose: proving a path inert.** A related complaint has a different shape - T-437
merged spike artifacts (JS/JSON/PNG/markdown under `spikes/`, which the workspace `exclude`s, with no
`.rs` and no Cargo files) and the gate said `full` because `spikes/` is unclassified. That was resolved
by a **human proof recorded in the merge commit**, not by an exemption, and that is the right handling:
an escalation that is argued and visible in git history is categorically different from a list the
classifier consults silently. Generalising it ("can this path be *proved* inert?") would need the
classifier to model what each suite reads, and would itself become a weakening surface. If it recurs it
earns its own ticket.

**It always prints its decision before running anything**, including the deciding files: a silent
classifier is a worse version of the judgement it replaces, because nobody can see or challenge the
choice. For a full gate it prints the files that *forced* it, which is the answerable question.

**Both CI jobs call it.** `test` runs `just gate --phase check`, `acceptance` runs
`just gate --phase acceptance` - one classifier, one diff, two halves of one answer, so the jobs cannot
disagree about what changed. `acceptance-ci` stays a separate job on purpose: the two run in parallel
on separate runners, it needs no Node or nextest, and an acceptance failure is a different signal from
a unit-test failure. Running `just lint` + `just test` + `just acceptance-ci` by hand remains the
periodic/milestone check.

The classifier is a pure function in `py/hkpy/gate.py`; `py/tests/test_gate.py` asserts the *chosen
suites* for each class, the fail-closed case (`newdir/x.rs` -> full), the three `fixtures/` /
`justfile` / `.github/` cases, mixtures (`ui/` + `crates/` -> full), and - for `gate-merge` - that it
classifies the index, prints what it narrowed away, exempts no path, and forces full outside a merge.

**The gap this leaves - and the full gate left it too.** Contract tests assert that the *server* serves
a route correctly; nothing asserts that the *client* asks for the right thing. T-367 found the time
navigator requesting `/api/timeline` with no band at all, drawing an empty canvas, with every suite
green. The guard belongs in `ui/test`: **assert the request the client builds**, not only the response
it renders. T-367 added exactly that (a regex forbidding the unscoped request shape from returning),
and T-389 generalised it (`listed` / `boxed` / `noExtent` derived from one collection, `boxed` a strict
subset).

#### What the ticket cycle actually costs, and which crates the gate runs (T-543, 2026-09-20)

**Measure before optimising, and the measurement changed the target.** T-543 was filed because
iteration felt like ~4 h per ticket and "the gate is 20-25 min" was the suspected cause. `just
cycle-time` (`py/hkpy/cycletime.py`) reads `ops/merge-runner.log` plus git and reports:

| quantity | n | median | p90 | max |
|---|---|---|---|---|
| gate (`MERGE start` -> `MERGED`) | 12 | **21.4 min** | 29.3 min | 31.9 min |
| queue wait (last commit -> gate start) | 11 | **119.7 min** | 452.3 min | 462.8 min |
| commit -> merged | 12 | **272.6 min** | 2289.8 min | 2742.6 min |

So the gate is **~8 %** of a ticket's 4.5 h cycle. Halving it saves ~10 minutes of 272; halving the
queue saves an hour. Three consequences, in value order.

**1. The bulk merge had never once worked.** Eight `BULK attempt` lines in the log, zero merges, every
one "conflict across branches", and the attempt of 09-20 09:43 left a `git merge-octopus` process
wedged for over ten hours. The cause is that `git merge A B C…` with three or more heads selects the
**octopus** strategy, which refuses outright any path more than one head modified - it does not attempt
a content merge. Nearly every branch here touches `docs/tasks.yaml`, so octopus was guaranteed to
refuse every time, and "fall back to individual" was not a safety net but the only path the code ever
took. `ops/merge-runner.sh` now merges the batch **one branch at a time** (two heads each, ordinary
recursive merge, which does resolve a shared `tasks.yaml`) and runs **one** gate over
`just gate --base <pre-batch sha>`, rewinding to that sha on red and falling back to individual gates.
N x 22 min of serial gating becomes 1 x ~25 min. Rewinding `main` is acceptable only because this
runner is the sole merger, nothing is pushed, and the batch is reconstructible from its branches - and
it is still guarded on HEAD not having moved.

**2. The gate times itself, durably.** `py/hkpy/gatelog.py` appends one JSON line per run to
`$HACKRIFF_OPS/gate-timings.jsonl`: class, phase, each suite command's duration, exit code, branch, and
the **load average** it ran under. The start line is written **before the first suite**, so a gate that
is killed or starved - the 62-minute one - leaves a trace rather than nothing; `gatelog.runs()` reports
such a run as unfinished instead of dropping it. It can never fail the gate: every write is wrapped,
and a line truncated by a kill costs that one run, not the history. Load is recorded because it is the
dominant term: measured on 2026-09-20, `cargo build -p hk-plugins --bins`, documented in the justfile
as "~0.05 s on a warm target", took **500 s** at load 211 with three agents building and an `hk serve`
holding 9-13 cores. No gate change competes with that.

**3. Affected-crate selection exists, but only for an agent's own local iteration — the merge and CI
gates never use it (T-543, corrected by the user 2026-09-20).** `py/hkpy/crates.py` computes the
reverse-dependency closure from `cargo metadata` at **target** level: a package's test binaries link
its lib deps *plus* its dev-deps and each dev-dep's own lib closure, so both edge kinds are followed
and they are followed differently. Measured here: `hk-cli` 2 of 20 packages, `hk-sim` 2,
`hk-pipeline` 3, `hk-api`/`hk-plugins` 4, `hk-detect` 5, `hk-ml` 6, `hk-recipe` 12, `hk-dsp` 14,
`hk-stream` 15, `hk-model` 20. The number that makes it work is that `hk-e2e`'s **lib** depends only on
`hk-model` - everything else it names is a dev-dependency, which reaches hk-e2e's own tests and stops
there rather than flowing into the eight crates that dev-depend on hk-e2e.

**The user's ruling, corrected into the gate itself, not just documented:** affected-crate selection is
for the agent iteration path only. `just gate --select-crates` opts in; unset (the default) means the
whole workspace. `py/hkpy/gate.py`'s `resolve_selection` checks `merge` and `ci` **first, unconditionally,
before it even looks at the flag** - `just gate-merge` (T-424, the coordinator's per-merge gate) and
`just gate` running inside `GITHUB_ACTIONS` (CI) both always get the whole workspace, so a future call
site cannot recreate the original mistake by wiring `--select-crates` into the merge/CI path by
accident. `py/tests/test_gate.py` pins this on the function, not on trust that no caller ever passes it.

**What the narrowing stops catching, stated plainly, for the opt-in case.** It follows the Cargo graph,
so it cannot see a coupling that exists only at run time. Three answers:

- **Acceptance is not narrowed, even when opted in.** `hk-e2e` is in the affected set of *every* crate,
  so `just acceptance-ci` - the tier that caught T-484's dark demo - runs for every `crates/` change.
  That is the graph agreeing with the policy, not the policy overriding the graph, and it is asserted by
  a test so a future graph change cannot quietly drop it.
- **The UI tiers are not narrowed.** `just test-ui` and `just test-ui-e2e` are untouched by the crate
  selection. Separately, `just test-ui` (not `test-ui-e2e`) is skipped outright for a `full`-class diff
  with no `ui/` path, on the merge/CI gate too - `npm run build`/`tsc --noEmit`/`npm test` are pure
  functions of `ui/` source with no generated Rust input, so a `crates/`-only diff cannot change their
  answer; `test-ui-e2e`, the browser tier that notices a backend change breaking the page, is never
  skipped. That skip stays on the merge gate because it is provably not a coverage reduction, unlike
  crate selection, which is a probabilistic-enough narrowing (it follows the *build* graph, not
  everything a suite might assert against) that it stays opt-in only.
- **Anything the graph does not model forces the whole workspace**: `Cargo.lock`, `Cargo.toml`,
  `.config/` (the thread cap and serial groups - *how* every test is scheduled), `fixtures/`,
  `plugins/`, `recipes/`, `.github/`, the `justfile`, and any path under `crates/` or `tests/` that
  belongs to no workspace package. So does a `cargo metadata` that fails for any reason, and
  `just gate --no-select` on demand.

The selection reaches the suites through exactly one variable, `HK_GATE_CRATES`, read by `lint-rust`,
`test-rust` and `test-doc`. **Unset means the whole workspace**, which is the safety property: an old
justfile, a hand-typed `just test-rust`, a crashed classifier, a shell that dropped the variable, or
simply not passing `--select-crates` all land on the expensive answer, never a cheap one. Same
fail-closed shape as the class rule, one level down.

**Levers measured and rejected.**

- **sccache gives nothing across worktrees, and it cannot.** Live hit rate over ~1000 observed
  requests: **0.00 %**, with 578 of 943 calls non-cacheable for the reason `incremental` (most builds
  on the box run with incremental compilation on, which sccache refuses). It is not broken - touching
  and rebuilding one crate *in the same worktree* hits. But compiling byte-identical source with a
  byte-identical argv from a **different working directory** misses, every time; `SCCACHE_BASEDIR` does
  not change that, and neither does `--remap-path-prefix`. sccache 0.18 keys Rust entries on the
  working directory, so four parallel worktrees at four paths can never share an entry. Leave it on -
  CLAUDE.md's rule against clearing it stands, and it does help a repeated build at one path - but stop
  counting it as a reason parallel worktrees are cheap. **The APFS target clone is the mechanism that
  actually works.**
- **Acceptance stays on plain `cargo test`, for a measured structural reason.** Moving it under nextest
  would put it in the `heavy-serial` group (`.config/nextest.toml` pins `package(hk-e2e)` at
  `max-threads = 1`), making every acceptance test run strictly one at a time; today `cargo test` runs
  each binary's tests in parallel and the binaries in sequence. That is a slowdown unless the group
  membership is also changed, and changing it is precisely the timing-flake risk `.config/nextest.toml`
  documents at length. The change worth *measuring* first is smaller and separate: CLAUDE.md already
  says the plain-`cargo test` paths "want `-- --test-threads 6`" and nothing passes it, so acceptance
  runs the most timing-sensitive tests in the repo at 28-way parallelism on a box T-436 measured as
  both slower and flakier at 28. That is a reliability question - 17 gate failures are in the log, each
  costing a full re-gate - and it deserves its own measured ticket rather than an unmeasured edit here.

##### The rest of T-543: the UI skip, the ledgers, and the guard that fires by itself

**`just test-ui` is skipped for a `full` diff with no `ui/` path** (`HK_GATE_SKIP_UI`, set by
`gate.py`, printed before anything runs). Measured by the diagnostic pass at **154 s on every
crate gate**. It is sound because `ui/` has no generated input: `npm run build` is esbuild over
`ui/src`, `typecheck` is `tsc --noEmit` over the same tree, and `npm test` is node over `ui/test`.
None reads a Rust artifact, so a `crates/`-only change cannot alter their answer. **What still runs
is the part that could:** `just test-ui-e2e`, the browser tier over a real `hk serve`, stays in the
`full` class's acceptance phase and is never skipped by this or anything else.

This is not the T-358 defect it resembles. T-358 was a suite that *self*-skipped, silently, when
node was missing — green by doing nothing. This skip is decided by the runner from the diff,
printed, attributable, and unset-means-run.

**`npm ci` only when the lockfile changed** (`just _npm-deps`). `npm ci` deletes `node_modules` and
reinstalls it from scratch every time; the stamp records the lockfile's SHA-256 and is written
*only after a successful install*, so a missing stamp, a mismatched stamp and a failed install all
re-install. A half-installed tree cannot be mistaken for a good one.

**Two ledgers and one guard, all append-only text — no database, no daemon.**

- `$HACKRIFF_OPS/gate-timings.jsonl` — `py/hkpy/gatelog.py`, one line per gate run and one per
  suite command: class, phase, affected crates, duration, exit code, load average.
- `$HACKRIFF_OPS/landed.jsonl` — `ops/merge-runner.sh`, one line per landed ticket:
  `{ticket, branch, first_commit_ts, merge_ts, land_minutes, gate_attempts}`. `gate_attempts` is
  read from the `merge-attempts.txt` ledger the runner already keeps rather than counted a second
  way, and `first_commit_ts` comes from the merge commit's **second parent**, so it survives the
  branch and its worktree being deleted.
- **`just gate-stats`** prints p50/p90 per class and per phase, and says whether any class's
  rolling median is over budget. **`py/tests/test_gate.py` asserts the same budgets**, so a
  slow-down fails a test instead of waiting for someone to notice.

The guard is a **rolling median over the last 7 runs of a class, with a 5-run floor**, not a
per-run bound, and the reason is this machine: one contended run says nothing (`cargo build -p
hk-plugins --bins`, documented at ~0.05 s warm, measured at **500 s at load 211**), so a per-run
bound would be noise, and a noisy guard gets muted. Fewer than 5 runs reports nothing at all —
failing on an absence of evidence is the fastest way to get a guard switched off.

**Sized but not done, with numbers, in case they are worth tickets.**

- **204 integration test binaries**, each its own compile *and* link: hk-pipeline **47**, hk-dsp 25,
  hk-api 17, hk-detect 16, hk-core 15, hk-e2e 15, hk-estimate 12, hk-demod 10, hk-classify 10,
  hk-cli 6, hk-blocks 6, hk-store 5, hk-stream 4, hk-context 4, hk-recipe 3, hk-plugins 3, hk-gnss
  3, hk-ml 2, hk-sim 1. Consolidating a crate's integration tests behind one `tests/main.rs` with
  `mod` per current file gives roughly **10x fewer links** for that crate at no loss of coverage —
  nextest still lists and runs each test individually, and its filtersets address `binary()`, so
  `.config/nextest.toml`'s `binary(data_path)`, `binary(live_edge_tiles)` and the other
  heavy-serial memberships **would have to be rewritten** as `test()` filters first. That
  rewrite is the risk, and it is why this is sized rather than done: those memberships are the
  repo's defence against the timing flakes, and getting one wrong is a silent loss.
- **The timing flakes are a cycle-time cost, not only a quality one.** 17 gate failures are in the
  merge log; each costs a full re-gate and a requeue, i.e. more than the 21.4 min median. T-430,
  T-433 and T-446 remain open; T-537's fix — drive frames rather than wall-clock milliseconds — is
  the precedent worth copying.

##### The two levers T-543 rejected with numbers: the linker, and incremental-instead-of-sccache

**`ld64.lld` is not faster than Apple's `ld` here — rejected.** Measured per target with
`cargo rustc -p … -- -C link-arg=-fuse-ld=/opt/homebrew/bin/ld64.lld`, which changes the linker for
**one** target and so does not invalidate the workspace the way a `RUSTFLAGS` change does. Six
pairs, alternating, on the two most link-dominated targets available:

| target | size | Apple `ld` | `ld64.lld` |
|---|---|---|---|
| `hk` binary (touch `bin/hk.rs`, 478 lines) | 47 MB | 1.33 / 1.35 / 1.38 s | 1.57 / 1.51 / 1.38 s |
| `hk-pipeline::data_path` test binary | 28 MB | 1.94 / 2.29 / 2.23 s | 2.27 / 2.20 / 2.32 s |

lld is equal-to-slightly-slower on both. Apple's shipped linker is `ld64-954` (the `ld-prime`
generation), which is already fast; mold has no Mach-O backend and is not an option. Nothing to
buy here. Worth re-measuring only if the toolchain changes.

**`CARGO_INCREMENTAL=1` costs 21 GB per worktree — disqualifying at four worktrees, regardless of
speed.** The proposal was reasonable on its face: sccache never hits (above), and `CARGO_INCREMENTAL=0`
exists to serve it, so incremental might be free speed. Measured: a **partial** `-p hk-dsp` rebuild
with `CARGO_INCREMENTAL=1` produced a **21 GB** `target/debug/incremental`, and free space on the dev
Mac fell from 24 GB to 13 GB during the run — past CLAUDE.md's 20 GB floor, with three other agents
on the box. The run was stopped and the space reclaimed. Four worktrees at that rate is 80+ GB the
machine does not have.

So `CARGO_INCREMENTAL=0` has a **second, independent justification** that has nothing to do with
sccache: it is what keeps four worktree targets on this disk. The speed half of that A/B is
therefore **unmeasured and not worth measuring** until the disk answer changes. For the record, the
baseline it would have had to beat: `cargo nextest run -p hk-dsp --no-run` after touching
`hk-dsp/src/lib.rs`, today's configuration, **54.6 / 47.9 s**.

**A note on what these measurements cost.** Both A/Bs required whole-tree rebuilds because
`RUSTFLAGS` and `CARGO_INCREMENTAL` are part of every crate's fingerprint, and a rebuilt APFS clone
turns shared blocks private. Anyone repeating this should use a **dedicated throwaway worktree on
an otherwise idle box**, watch `df` between runs, and prefer the per-target `cargo rustc` trick
above wherever the question allows it — it answered the linker question for a few seconds of build
instead of a few gigabytes.

#### The "60 % slower in two days" was two different measurements, plus one real 5-minute step (T-763, 2026-09-22)

The duration guard T-543 built fired on 2026-09-22: the rolling median of the last seven `full`
runs read 34.0 min against a 35 min budget set from the 21.4 min recorded above. T-762 moved the
assertion off the merge path (a monitor on the merge path deadlocks the pipeline); T-763 is the
regression it flagged. **Most of the step is an artefact, and the remainder is one located
defect.** Both halves are measurable from records nobody had to be watching to collect.

**The first finding is that the instrumentation was not recording the runs that mattered.**
`$HACKRIFF_OPS/gate-timings.jsonl` held **33 runs, 31 of them written by the test suite itself** —
`py/tests/test_gate.py`'s round-trip through `gate.main()` used the real `$HACKRIFF_OPS`, so every
`just test-py` appended a 0.0-second `py` record to the production history (their `root` is a
pytest tmpdir; fixed by pointing the test at its own). Exactly **two** real `full` gates were in
there, against **48** the merge runner had run over the same window. The record that survived is
the line `py/hkpy/gate.py` *prints* next to each `gatelog.append()` — `gate: just test took 1221s
(exit 0)` — which `ops/merge-runner.log` has captured since before the structured log existed, and
which is per suite. `just cycle-time --suites` (`parse_suite_runs`/`suite_stats`) reads it back.

**Two rules make that history mean something, and both were being broken:**

- **An aborted run is not a measurement of the suite.** The gate stops at the first failing suite,
  so a failed run measures a *prefix*. Of the 48 full gates, **24 never reached the UI e2e suite at
  all**. Averaged in, they make the gate look faster every time an unrelated test breaks.
  `rolling_medians` now excludes them.
- **Two classes are never pooled into one median.** `suite_stats` reports per class, where a run's
  class is the set of suites it launched.

**That second rule is where the 21.4 min came from.** It is this document's table above: gate start
-> `MERGED`, so *passing gates only*, **across all classes** — and a 17-second `py` gate sits in
the same median as a 40-minute `full` one. The 34.0 the guard reported is `full` only, failures
and bulk runs included. They are not the same quantity, and the step between them is mostly that.

**Like for like, over the 24 complete passing `full` gates recorded 09-20 23:37 -> 09-22 05:06:**

| suite | earlier half | recent half | moved |
|---|---|---|---|
| lint | 60 s | 63 s | +3 s (the `ld64.lld` link gain is real and holds) |
| the Rust workspace suite | 16.9 min | 19.1 min | **+2.2 min** |
| the acceptance suite | 3.7 min | 5.3 min | +1.6 min |
| the UI e2e suite | 7.5 min | 8.0 min | +0.5 min |
| **total** | **30.0 min** | **34.7 min** | **+4.7 min, +16 %** |

So: **test-bound, as the ticket said — and +16 %, not +60 %.**

**The Rust-suite growth is one step on one day, not a suite outgrowing its budget.** From the
nextest `Summary` lines in the same log, the workspace run went **462 s -> 756 s at the 09-21 04:55
gate**, while the test count moved **2395 -> 2403 (+0.3 %)**. Test-count growth (2302 on 09-19 to
2485 on 09-22, +8 %) does not explain a 64 % jump. What changed in that same run is visible in the
`SLOW` lines: four `hk-classify` binaries — `accuracy_sweep`, `below_gate_absorption`,
`open_set_stats`, `verifier_gain` — went from never-slow to past the 60/120/180 s thresholds and
have stayed there every run since, and `hk-estimate::receiver_lines` went from `>60 s` to `>240 s`
(later `>300 s`) alongside them. That gate carried T-246 and T-589, both of which change the
classifier internals those statistical sweeps exercise. The test *files* are weeks old; they did
not appear, they got about three times more expensive.

**The mechanism is that these are now the critical path, and T-436's measurement of it is stale.**
None of the five binaries is in `heavy-serial`. T-436 measured the serial group at 224 s = 96 % of
a 234 s run; with `receiver_lines` alone past 300 s and four classify binaries at 60-180 s in the
free pool of 8 threads, the critical path has moved outside the group that was tuned for it. Any
further tuning of `test-threads` or of the group memberships should be re-measured against that,
not against the 2026-09-14 numbers still recorded in `.config/nextest.toml`.

**The budget stays at 35 min, deliberately.** The extra ~5 minutes is attributable to roughly 300 s
in five named binaries, which makes it a defect to shrink rather than a new honest price for the
loop. Raising the number to fit it is the move that turns a budget into a record of whatever
happened.

**What dominates the loop is still not the gate's duration.** 36 gate failures are recorded against
44 merge-runner gates, and *half of all full gates never finish their suite set*. A failure costs a
whole re-gate plus a requeue — more than the entire 4.7 min of drift this ticket was filed for —
and the UI e2e suite alone aborted 9 of them. The 2026-09-20 conclusion holds unchanged: queue
wait and re-gates are the cycle, and the gate's own minutes are ~8 % of it.
