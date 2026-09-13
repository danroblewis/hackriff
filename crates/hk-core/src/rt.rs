//! Real-time hooks for the capture/writer thread, all best-effort: raise its scheduling priority
//! and pin ring memory.
//!
//! The capture thread feeds the ring. If it is preempted long enough, or waits on a page fault,
//! the USB transfer backs up and the device drops samples (C01 pitfalls). Both hooks are cheap
//! insurance, but neither may be a hard requirement: without privileges they quietly degrade.
//!
//! - **macOS:** QoS class `USER_INTERACTIVE` (no privileges needed).
//! - **Linux (Jetson):** `SCHED_FIFO` at min+10, capped at `RLIMIT_RTPRIO` when that limit is set
//!   (needs `CAP_SYS_NICE` or an `rtprio` limit), else nice −10 for this thread (needs
//!   `CAP_SYS_NICE` or a `nice` limit).
//! - **Elsewhere:** [`PriorityOutcome::Unsupported`].
//! - **Memory:** `mlock` on Unix ([`crate::RingHandle::lock_memory`]), limited by `RLIMIT_MEMLOCK`.

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

/// What a best-effort memory lock achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryLock {
    /// The memory is pinned.
    Locked {
        /// Bytes pinned.
        bytes: usize,
    },
    /// The OS refused (usually `RLIMIT_MEMLOCK`); the memory stays pageable.
    Failed {
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

/// Pins `memory` in RAM, best-effort. The pages stay locked until the memory is freed.
pub(crate) fn lock_memory<T>(memory: &[T]) -> MemoryLock {
    let bytes = size_of_val(memory);
    if bytes == 0 {
        return MemoryLock::Locked { bytes };
    }
    mlock(memory.as_ptr().cast(), bytes)
}

#[cfg(unix)]
fn mlock(addr: *const std::ffi::c_void, bytes: usize) -> MemoryLock {
    // SAFETY: `mlock` only changes the paging attributes of `[addr, addr + bytes)`; it neither
    // reads nor writes through the pointer. The range is exactly a live slice owned by the
    // caller. Freeing that memory later unlocks its pages, so no lock outlives it.
    let rc = unsafe { libc::mlock(addr, bytes) };
    if rc == 0 {
        MemoryLock::Locked { bytes }
    } else {
        MemoryLock::Failed {
            errno: io::Error::last_os_error().raw_os_error().unwrap_or(0),
        }
    }
}

#[cfg(not(unix))]
fn mlock(_addr: *const std::ffi::c_void, _bytes: usize) -> MemoryLock {
    MemoryLock::Unsupported
}

/// The `SCHED_FIFO` priority to request: min+10, capped at the policy maximum and at the soft
/// `RLIMIT_RTPRIO` limit when one is set, since an unprivileged thread may not exceed it. A limit
/// of 0 means real-time needs `CAP_SYS_NICE`, so the uncapped request is left to the kernel.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn rt_priority(min: i32, max: i32, rtprio_limit: Option<u64>) -> i32 {
    let wanted = min.saturating_add(10).min(max);
    match rtprio_limit {
        Some(limit) if limit > 0 => wanted
            .min(i32::try_from(limit).unwrap_or(i32::MAX))
            .max(min),
        _ => wanted,
    }
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

    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    pub fn raise() -> PriorityOutcome {
        // SAFETY: plain syscalls on the calling thread (pid/tid 0 or our own tid); `limit` and
        // `param` are valid, initialised structs that outlive the calls.
        unsafe {
            let min = libc::sched_get_priority_min(libc::SCHED_FIFO);
            let max = libc::sched_get_priority_max(libc::SCHED_FIFO);
            let mut limit: libc::rlimit = std::mem::zeroed();
            let rtprio_limit = (libc::getrlimit(libc::RLIMIT_RTPRIO, &mut limit) == 0
                && limit.rlim_cur != libc::RLIM_INFINITY)
                .then_some(limit.rlim_cur as u64);
            let mut param: libc::sched_param = std::mem::zeroed();
            param.sched_priority = super::rt_priority(min, max, rtprio_limit);
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

    #[test]
    fn rt_priority_respects_the_rtprio_limit() {
        // Linux SCHED_FIFO: 1..=99.
        assert_eq!(rt_priority(1, 99, None), 11);
        assert_eq!(rt_priority(1, 99, Some(0)), 11, "0: left to CAP_SYS_NICE");
        assert_eq!(rt_priority(1, 99, Some(5)), 5);
        assert_eq!(rt_priority(1, 99, Some(50)), 11);
        assert_eq!(rt_priority(1, 8, None), 8);
        assert_eq!(rt_priority(1, 99, Some(u64::MAX)), 11);
    }

    #[test]
    fn memory_lock_is_best_effort() {
        let memory = vec![0u64; 1024];
        match lock_memory(&memory) {
            MemoryLock::Locked { bytes } => assert_eq!(bytes, 8 * 1024),
            MemoryLock::Failed { .. } | MemoryLock::Unsupported => {}
        }
        assert_eq!(lock_memory::<u64>(&[]), MemoryLock::Locked { bytes: 0 });
    }
}
