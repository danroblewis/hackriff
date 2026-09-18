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
