//! Graceful shutdown on Ctrl-C (SIGINT) and SIGTERM for `hk replay`, `hk run`, `hk serve` and
//! `hackriffd` (T-037a).
//!
//! The first signal stops the source through `PipelineHandle::stopper`; the pipeline then drains
//! its readers and chains (recordings finish, or are discarded when incomplete, so no invalid
//! SigMF is left), flushes and closes the Survey and the stores, and the process exits 0. A second
//! signal exits at once (status 130). The handler only sets an atomic (async-signal-safe); a
//! watcher thread does the stopping.
//!
//! # Shutdown is bounded (T-531)
//! The graceful path above is a **chain of unbounded waits**: the supervisor joins every worker
//! thread with no deadline, the control thread will not return until the last chain has wound
//! down, and a chain can be parked in a probe, a plugin handshake or a writer sync measured in
//! tens of seconds. Any one of them holds the process open, and there is nothing in the drain that
//! notices. A 40-minute live sweep took **over 78 seconds** to exit on SIGTERM (T-525) — which did
//! not reproduce on the mock (0.86–1.91 s in every shape tried, main's binary and this one alike),
//! so the drain is not known to be slow so much as known to be **unbounded**, which is the thing
//! a supervisor cannot live with.
//!
//! That is an operational fault, not untidiness. `ops/stage.sh` restarts the server when its
//! health check fails and waits 10 s before `SIGKILL`, so a slow exit overlaps two servers on one
//! device — and only one process can open the HackRF. So the first signal also **arms a
//! deadline** ([`SHUTDOWN_BOUND`]): the drain gets that long to finish on its own, and if it has
//! not, the process says so on stderr and exits. Exiting is the *bound*, not the mechanism — the
//! graceful path is still what normally runs, and normally finishes in about a second.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hk_pipeline::Stopper;

static REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static DEADLINE_ARMED: AtomicBool = AtomicBool::new(false);

/// How long the graceful drain has, from the first signal, before the process exits anyway.
///
/// Generous against what the drain actually costs — measured at 0.86–1.91 s on the mock, including
/// a 4 GiB IQ ring after ten minutes of capture — and inside what the supervisor already allows:
/// `ops/stage.sh` waits 10 s after its `pkill` and then sends `SIGKILL`. Being under that is the
/// point: past the bound the alternative is not a tidier exit, it is `SIGKILL` two seconds later.
///
/// **It is a real trade.** The drain is where "no invalid SigMF is left" is kept, so an exit at the
/// bound can leave a recording half-finalised. That is accepted deliberately: a server that will
/// not let go holds the device, and only one process can open the HackRF. The exit says so on
/// stderr, and `hk-pipeline`'s `report_stragglers` has already named the thread that did not stop.
pub const SHUTDOWN_BOUND: Duration = Duration::from_secs(8);

/// Arms the [`SHUTDOWN_BOUND`] deadline, once per process.
///
/// A detached thread, never joined: by the time it matters the process is on its way out, and the
/// normal path outlives it by exiting first. The exit status is **0** deliberately — the point of
/// the bound is to free the port and the device for whoever is restarting us, and a supervisor
/// that read a slow-but-complete drain as a crash would be worse off than one that reads the
/// stderr line. A second signal still exits 130 at once, which is the user asking, not this.
#[cfg(unix)]
fn arm_exit_deadline(tag: &'static str) {
    if DEADLINE_ARMED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("hk-shutdown-deadline".into())
        .spawn(move || {
            std::thread::sleep(SHUTDOWN_BOUND);
            eprintln!(
                "{tag}: shutdown did not finish within {SHUTDOWN_BOUND:?}; exiting anyway \
                 (something in the drain is not observing the stop)"
            );
            // SAFETY: `_exit` is async-signal-safe and runs no destructors, which is the point:
            // whatever is stuck is stuck.
            unsafe { libc::_exit(0) };
        });
}

#[cfg(not(unix))]
fn arm_exit_deadline(_tag: &'static str) {}

#[cfg(unix)]
extern "C" fn on_signal(_signal: libc::c_int) {
    if REQUESTED.swap(true, Ordering::SeqCst) {
        // SAFETY: `_exit` is async-signal-safe.
        unsafe { libc::_exit(130) };
    }
}

/// Installs the SIGINT/SIGTERM handler (binaries call it once at start).
pub fn install() -> std::io::Result<()> {
    #[cfg(unix)]
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: a zeroed `sigaction` with an empty mask and a handler that only swaps an atomic
        // and may call `_exit`; the struct outlives the call.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// A shutdown signal has arrived.
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// Blocks until a shutdown signal arrives, then arms the [`SHUTDOWN_BOUND`] deadline.
///
/// This is the other way a signal reaches a run: `hk serve` keeps serving after its source ends,
/// and waits here rather than in [`watch`].
pub fn wait_for_signal() {
    while !requested() {
        std::thread::sleep(Duration::from_millis(50));
    }
    arm_exit_deadline("hackriff");
}

/// A watcher thread; dropping it ends the thread.
pub struct Watch {
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn watch(stopper: Stopper, deadline: Option<Instant>, tag: &'static str) -> Watch {
    let done = Arc::new(AtomicBool::new(false));
    let d = Arc::clone(&done);
    let thread = std::thread::Builder::new()
        .name("hk-signal-watch".into())
        .spawn(move || {
            while !d.load(Ordering::SeqCst) && !stopper.is_stopped() {
                if requested() {
                    eprintln!("{tag}: stopping (Ctrl-C again to exit at once)");
                    // Armed before the stop, not after: `Stopper::stop` takes the supervisor's
                    // lock, so a supervisor busy with a re-plumb can hold this very thread up, and
                    // the deadline has to already be running when it does.
                    arm_exit_deadline(tag);
                    stopper.stop();
                    return;
                }
                if deadline.is_some_and(|t| Instant::now() >= t) {
                    stopper.stop();
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
        .ok();
    Watch { done, thread }
}

/// Stops the run when a shutdown signal arrives.
pub fn stop_on_signal(stopper: Stopper) -> Watch {
    watch(stopper, None, "hackriff")
}

/// Stops the run when a shutdown signal arrives or `after` has passed.
pub fn stop_on_signal_or_after(stopper: Stopper, after: Duration) -> Watch {
    watch(stopper, Some(Instant::now() + after), "hackriff")
}
