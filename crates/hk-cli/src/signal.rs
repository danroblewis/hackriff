//! Graceful shutdown on Ctrl-C (SIGINT) and SIGTERM for `hk replay`, `hk run`, `hk serve` and
//! `hackriffd` (T-037a).
//!
//! The first signal stops the source through `PipelineHandle::stopper`; the pipeline then drains
//! its readers and chains (recordings finish, or are discarded when incomplete, so no invalid
//! SigMF is left), flushes and closes the Survey and the stores, and the process exits 0. A second
//! signal exits at once (status 130). The handler only sets an atomic (async-signal-safe); a
//! watcher thread does the stopping.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hk_pipeline::Stopper;

static REQUESTED: AtomicBool = AtomicBool::new(false);

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

/// Blocks until a shutdown signal arrives.
pub fn wait_for_signal() {
    while !requested() {
        std::thread::sleep(Duration::from_millis(50));
    }
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
