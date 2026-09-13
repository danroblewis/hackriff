//! The scheduler's injectable clock: wall clock live, synthetic in tests.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use hk_model::Timestamp;

/// A time source for the scheduler.
pub trait Clock {
    /// The current time.
    fn now(&self) -> Timestamp;
}

/// The host's wall clock, made monotonic: wall time is read once (at the first reading in the
/// process) and then advanced by [`Instant`]. A later wall-clock step (NTP, a manual set, a
/// daylight fix) does not move it, so step start times never go backwards and a preemption never
/// rolls back a step that already finished. It follows the wall clock's rate (the OS monotonic
/// clock is frequency-disciplined) but not its steps; restart the process to re-anchor.
#[derive(Clone, Copy, Debug, Default)]
pub struct WallClock;

impl Clock for WallClock {
    fn now(&self) -> Timestamp {
        static ANCHOR: OnceLock<(Timestamp, Instant)> = OnceLock::new();
        let (wall, mono) = ANCHOR.get_or_init(|| (Timestamp::now(), Instant::now()));
        let elapsed = i64::try_from(mono.elapsed().as_nanos()).unwrap_or(i64::MAX);
        wall.saturating_add_nanos(elapsed)
    }
}

/// A monotonic clock anchored to a wall clock: `wall` is read once at construction, then the time
/// advances by `mono`'s elapsed time. [`WallClock`] is this with the host clock and [`Instant`];
/// this form lets tests inject both sources (e.g. a backwards wall step).
#[derive(Clone, Debug)]
pub struct AnchoredClock<M> {
    wall_anchor: Timestamp,
    mono: M,
    mono_anchor: Timestamp,
}

impl<M: Clock> AnchoredClock<M> {
    /// Anchors `mono` to `wall.now()`.
    pub fn new(wall: &impl Clock, mono: M) -> Self {
        let mono_anchor = mono.now();
        Self {
            wall_anchor: wall.now(),
            mono,
            mono_anchor,
        }
    }
}

impl<M: Clock> Clock for AnchoredClock<M> {
    fn now(&self) -> Timestamp {
        let elapsed = self
            .mono
            .now()
            .as_unix_nanos()
            .saturating_sub(self.mono_anchor.as_unix_nanos())
            .max(0);
        self.wall_anchor.saturating_add_nanos(elapsed)
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
