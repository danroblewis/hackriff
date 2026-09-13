//! The scheduler's injectable clock: wall clock live, synthetic in tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use hk_model::Timestamp;

/// A time source for the scheduler.
pub trait Clock {
    /// The current time.
    fn now(&self) -> Timestamp;
}

/// The host's wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct WallClock;

impl Clock for WallClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// A manually advanced clock. Clones share one time, so a test can keep a handle while the
/// scheduler owns another.
#[derive(Clone, Debug)]
pub struct SyntheticClock(Arc<AtomicI64>);

impl SyntheticClock {
    /// A clock stopped at `start`.
    pub fn new(start: Timestamp) -> Self {
        Self(Arc::new(AtomicI64::new(start.as_unix_nanos())))
    }

    /// Sets the time.
    pub fn set(&self, t: Timestamp) {
        self.0.store(t.as_unix_nanos(), Ordering::SeqCst);
    }

    /// Moves the time forward by `ns` (saturating).
    pub fn advance_ns(&self, ns: i64) {
        let now = self.0.load(Ordering::SeqCst);
        self.0.store(now.saturating_add(ns), Ordering::SeqCst);
    }
}

impl Clock for SyntheticClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_nanos(self.0.load(Ordering::SeqCst))
    }
}
