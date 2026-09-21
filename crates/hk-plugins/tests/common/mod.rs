//! Shared test support for the plugin-host test binaries (T-493).
//!
//! # The start of a subprocess is not a quantity these tests may bound
//!
//! Every wait in these binaries is really two waits glued together: *the OS getting a freshly
//! built plugin binary running*, and then *the host doing its job*. Only the second is what any of
//! these tests were written to check, and the first has a range that no fixed bound survives.
//!
//! **Traced, inside `run_once`, on a failing run.** The child was created in **193 µs** —
//! `posix_spawn` returned with a pid, and the host's stdout reader thread was running at
//! +320 µs — and then **executed nothing for 30 seconds**. Not one byte: not even
//! `hk-dummy-plugin`'s first `eprintln!`, which runs immediately after argument parsing and
//! before it reads anything from stdin. Meanwhile the test's own 2 ms polling thread ran
//! normally and the host had already accepted the whole input
//! (`records_offered: 100, records_enqueued: 100, decodes: 0`, and an **empty log tail**).
//! Nothing in `hk-plugins` was wrong on either side of that 30 s gap.
//!
//! It arrives in bursts and takes the whole family with it: one reproduction failed **13 of the
//! 19 tests in a single run**, each exactly on its own deadline — 10.06 s against a 10 s bound,
//! 15.04 s against 15 s, 20.06 s against 20 s, 30.04 s against 30 s. That is the shape the
//! coordinator saw in a real gate, and the shape five agents in one session each re-measured.
//!
//! **What it is not.** Three candidate mechanisms were measured and rejected:
//!
//! - **The macOS `pipe(2)`/`FD_CLOEXEC` race** (docs/10 §3.4, T-257) is real — reproduced
//!   directly, 11 leaked pipe write ends in 16 000 probes with 8 concurrently spawning threads in
//!   one process, 0 with 6 — but it can only make nextest print `LEAK`, which is a pass, and it
//!   needs two spawns racing *inside one process*. These tests are one per nextest process and
//!   spawn sequentially. Observed separately, and consistent: runs came back "16 passed
//!   (3 leaky)" — leaks without failures.
//! - **Cold-binary exec cost.** 114 samples of 19 concurrent execs taken immediately after a
//!   relink: min 0.16 s, median 0.20 s, max **0.24 s**. And paying it up front did not help — an
//!   exec-and-wait warm-up before every test left the family failing 6 runs out of 6.
//! - **`limits.nice = 10`**, which the host applies with `setpriority(PRIO_PGRP, …)` right after
//!   the spawn. Measured from a host-shaped parent (own process group, all three stdio piped,
//!   the same `setpriority` call), 8 concurrent probes × 25 interleaved samples each at load
//!   average 153: nice 10 and nice 0 are indistinguishable, median 0.005 s, max 0.015 s either
//!   way, and **no sample over 1 s on either side**.
//!
//! So the start-up term is not something this repo controls, and its measured range is 0.0002 s
//! to over 30 s. T-383's rule — *a test may not bound a quantity whose natural range it has not
//! measured* — says plainly what follows: **don't bound it.** [`wait_started`] waits for the
//! child to demonstrate that it has run at all, off every test's clock, with a grace that is
//! declared out loud below. Every existing bound then starts where the test's own subject does.
//! It is not a longer deadline, a retry, a thread cap or a serial group: a plugin that never
//! starts still fails, and one that starts and then misbehaves fails exactly as tightly as before.

use std::thread;
use std::time::{Duration, Instant};

use hk_plugins::{PluginMonitor, PluginStats};

/// Longest [`wait_started`] waits for the plugin process to reach its first instruction.
///
/// This is a **cap on a quantity outside this repo's control**, not a budget for anything the
/// tests assert. It is set well past the worst observed stall (over 30 s, measured above) so that
/// a machine having one of its episodes does not fail a run, and short enough that a plugin which
/// genuinely never starts — a bad path, a missing binary, a crash in `main` — still fails the
/// test rather than hanging it. Nothing measures success against it: a healthy plugin passes it
/// in milliseconds (median 0.005 s, measured), so on a normal run it costs nothing.
pub const START_GRACE: Duration = Duration::from_secs(120);

/// Whether the plugin process has produced any evidence that it ran.
///
/// Deliberately broad, because the failure being absorbed is *total* silence: in every observed
/// instance the log tail was empty and every output counter was zero. Both halves are needed —
/// a test whose content ceiling forbids plugin text gets no log lines but does get
/// `*_withheld` counts.
///
/// What must NOT be used here, and why: `starts` and the `records_*` counters are the host's own
/// bookkeeping and tick whether or not the child ever ran, and so is `ready` — a manifest without
/// `input.ready_signal` is ready the moment its process is *attached*. The failing runs showed
/// exactly that: `ready: true`, `records_enqueued: 100`, `decodes: 0`, empty log.
pub fn plugin_has_run(mon: &PluginMonitor) -> bool {
    let s: PluginStats = mon.stats();
    let evidence = s.decodes
        + s.annotations
        + s.malformed
        + s.class_clamped
        + s.class_unknown
        + s.content_gated
        + s.metadata_sanitized
        + s.sample_index_out_of_range
        + s.log_lines_withheld
        + s.stderr_lines_withheld
        + s.crashes
        + s.clean_exits
        + s.spawn_failures;
    evidence > 0 || !mon.log_tail().lines.is_empty()
}

/// Blocks until the plugin process has shown it is running, or [`START_GRACE`] passes.
///
/// Call it once, straight after `PluginInstance::spawn`, before anything the test times. Panics
/// if the grace expires, because at that point the plugin really has not started and no later
/// assertion would mean anything.
pub fn wait_started(mon: &PluginMonitor) {
    let deadline = Instant::now() + START_GRACE;
    let t0 = Instant::now();
    while !plugin_has_run(mon) {
        assert!(
            Instant::now() < deadline,
            "the plugin process produced nothing in {START_GRACE:?} - it never started: {:?} {:?}",
            mon.stats(),
            mon.log_tail()
        );
        thread::sleep(Duration::from_millis(2));
    }
    // Loud only when it mattered, so a run that hit the OS stall says so instead of looking fast.
    let waited = t0.elapsed();
    if waited > Duration::from_secs(1) {
        eprintln!(
            "T-493: plugin took {waited:?} to reach its first instruction (see common/mod.rs)"
        );
    }
}
