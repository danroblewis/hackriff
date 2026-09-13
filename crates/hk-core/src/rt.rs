//! Real-time thread hooks: raise the capture/writer thread's scheduling priority, best-effort.
//!
//! The capture thread feeds the ring. If it is preempted long enough, the USB transfer
//! backs up and the device drops samples (C01 pitfalls). Raising its priority is cheap
//! insurance, but it must never be a hard requirement: without privileges it quietly degrades.
//!
//! - **macOS:** QoS class `USER_INTERACTIVE` (no privileges needed).
//! - **Linux (Jetson):** `SCHED_FIFO` at min+10 (needs `CAP_SYS_NICE` or an `rtprio` limit),
//!   else nice −10 for this thread (needs `CAP_SYS_NICE` or a `nice` limit).
//! - **Elsewhere:** [`PriorityOutcome::Unsupported`].

use std::io;
use std::thread::{self, JoinHandle};

/// What [`raise_current_thread_priority`] achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriorityOutcome {
    /// Priority raised with the named mechanism.
    Applied(&'static str),
    /// Every mechanism was refused (usually missing privileges); the thread runs at default
    /// priority.
    Failed {
        /// The mechanism that failed last.
        mechanism: &'static str,
        /// OS error code.
        errno: i32,
    },
    /// No mechanism on this platform.
    Unsupported,
}

/// Raises the calling thread's scheduling priority for capture work. Best-effort.
pub fn raise_current_thread_priority() -> PriorityOutcome {
    imp::raise()
}

/// Spawns a named capture thread, raises its priority, then runs `f` with the outcome (so the
/// caller can record it, e.g. in logs or provenance notes).
pub fn spawn_capture_thread<F, T>(name: impl Into<String>, f: F) -> io::Result<JoinHandle<T>>
where
    F: FnOnce(PriorityOutcome) -> T + Send + 'static,
    T: Send + 'static,
{
    thread::Builder::new()
        .name(name.into())
        .spawn(move || f(raise_current_thread_priority()))
}

#[cfg(target_vendor = "apple")]
mod imp {
    use super::PriorityOutcome;

    pub fn raise() -> PriorityOutcome {
        // SAFETY: changes the QoS class of the calling thread only; no pointers are passed.
        let rc = unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
        };
        if rc == 0 {
            PriorityOutcome::Applied("macOS QoS user-interactive")
        } else {
            PriorityOutcome::Failed {
                mechanism: "pthread_set_qos_class_self_np",
                errno: rc,
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::PriorityOutcome;

    fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    pub fn raise() -> PriorityOutcome {
        // SAFETY: plain syscalls on the calling thread (pid/tid 0 or our own tid); `param` is a
        // valid, initialised sched_param that outlives the call.
        unsafe {
            let min = libc::sched_get_priority_min(libc::SCHED_FIFO);
            let max = libc::sched_get_priority_max(libc::SCHED_FIFO);
            let mut param: libc::sched_param = std::mem::zeroed();
            param.sched_priority = (min + 10).min(max);
            if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) == 0 {
                return PriorityOutcome::Applied("Linux SCHED_FIFO");
            }
            // On Linux, setpriority(PRIO_PROCESS, tid) applies to that one thread.
            let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
            if libc::setpriority(libc::PRIO_PROCESS, tid, -10) == 0 {
                return PriorityOutcome::Applied("Linux nice -10");
            }
            PriorityOutcome::Failed {
                mechanism: "sched_setscheduler/setpriority",
                errno: errno(),
            }
        }
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
mod imp {
    use super::PriorityOutcome;

    pub fn raise() -> PriorityOutcome {
        PriorityOutcome::Unsupported
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_thread_runs_whatever_the_outcome() {
        let outcome = spawn_capture_thread("hk-test-capture", |outcome| outcome)
            .unwrap()
            .join()
            .unwrap();
        // Best-effort: any outcome is acceptable, but on macOS QoS needs no privileges.
        #[cfg(target_vendor = "apple")]
        assert!(
            matches!(outcome, PriorityOutcome::Applied(_)),
            "{outcome:?}"
        );
        let _ = outcome;
    }
}
