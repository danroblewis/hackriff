//! **T-978: the seam where an unresolved overlap gets re-measured.**
//!
//! CLAUDE.md's inventory invariant: *"Overlap is an error signal that triggers re-analysis … the
//! system detects the overlap and automatically re-analyzes that region to resolve it to the real
//! signal(s), rather than leaving competing boxes stacked."* T-369 detects it in
//! `hk_model::repo`'s stage 4 and reports the regions it could not resolve
//! (`OverlapOutcome::unresolved`); `hk_detect::overlap::measure_region` measures such a region
//! against the integrated spectrum; `hk_model::region_verdicts` maps the rows onto what was
//! measured. This is the wire between them — the one place where the *spectrum*, which lives on
//! the detect reader, meets the *inventory*, which lives on the detect writer.
//!
//! # The hand-off, and why it is demand-driven (T-453)
//!
//! T-453's constraint binds: work on the capture path is paid whether or not anyone looks, so a
//! snapshot copied every block for a re-analysis that is almost never needed would be a permanent
//! tax on the thread that gates the ring. So [`RegionSpectrum`] is a **one-shot request**:
//!
//! 1. the inventory finds a region stage 4 could not resolve and calls [`RegionSpectrum::want`];
//! 2. at its next integrated evaluation (0.25 s blocks) the reader sees the flag, takes one
//!    snapshot, clears the flag and publishes it — [`RegionSpectrum::publish`] costs one atomic
//!    load and returns when nothing is wanted, which is the steady state;
//! 3. the inventory [`RegionSpectrum::take`]s it, measures every unresolved region against it, and
//!    asks again while any region is still unresolved.
//!
//! Both sides charge their own time ([`RegionSpectrum::stats`]), so the cost of the feature on the
//! capture-adjacent thread is a measured number and not an assumption: `publish_nanos` is what the
//! reader paid, `measure_nanos` what the writer paid.
//!
//! A snapshot is **taken**, not read: a measurement is used once and the next one is requested, so
//! nothing here can resolve a region against a spectrum from minutes ago.
//!
//! # Known limitation with several front ends
//!
//! A run shares **one** hand-off across every front end (`run.rs`, `devices.rs`), so whichever
//! reader reaches the request first answers it. A region in another device's band is then outside
//! that snapshot's tuned span, `hk_detect::overlap::measure_region` answers `None` — never an empty
//! measurement — and the request is simply made again on the next touch. So the output is never
//! wrong, but a region can take several touches to be measured, and against a much busier reader it
//! could wait indefinitely. Fixing it properly means a slot per receive chain, keyed the way
//! `hk_model::relate::ReceiveChain` keys device-local physics; it is deliberately not done here.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hk_detect::IntegratedSnapshot;

/// What the re-analysis cost and did, for the run summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionSpectrumStats {
    /// Snapshots the reader took and published, one per request.
    pub published: u64,
    /// Requests the inventory made.
    pub requested: u64,
    /// Regions measured against a published snapshot.
    pub measured: u64,
    /// Nanoseconds the **detect reader** spent taking snapshots (the capture-adjacent cost).
    pub publish_nanos: u64,
    /// Nanoseconds the **detect writer** spent measuring and applying regions.
    pub measure_nanos: u64,
}

/// The one-shot spectrum hand-off between the detect reader and the inventory writer.
#[derive(Debug, Default)]
pub struct RegionSpectrum {
    wanted: AtomicBool,
    latest: Mutex<Option<Arc<IntegratedSnapshot>>>,
    published: AtomicU64,
    requested: AtomicU64,
    measured: AtomicU64,
    publish_nanos: AtomicU64,
    measure_nanos: AtomicU64,
}

impl RegionSpectrum {
    /// Asks the reader for one snapshot at its next integrated evaluation.
    pub fn want(&self) {
        self.requested.fetch_add(1, Ordering::Relaxed);
        self.wanted.store(true, Ordering::Release);
    }

    /// Whether a snapshot has been asked for. One relaxed load; this is what the reader pays in
    /// the steady state.
    pub fn wanted(&self) -> bool {
        self.wanted.load(Ordering::Acquire)
    }

    /// Publishes a snapshot **if one was asked for**, clearing the request. `take` is called only
    /// then, so a run where no overlap is ever unresolved never allocates a snapshot at all.
    pub fn publish<F>(&self, take: F)
    where
        F: FnOnce() -> Option<IntegratedSnapshot>,
    {
        if !self.wanted.swap(false, Ordering::AcqRel) {
            return;
        }
        let started = std::time::Instant::now();
        let snapshot = take();
        self.publish_nanos
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        let Some(snapshot) = snapshot else {
            // Nothing to publish yet (no evaluation in this segment): keep the request standing.
            self.wanted.store(true, Ordering::Release);
            return;
        };
        self.published.fetch_add(1, Ordering::Relaxed);
        *self.latest.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(snapshot));
    }

    /// Takes the published snapshot, if there is one. A measurement is used once.
    pub fn take(&self) -> Option<Arc<IntegratedSnapshot>> {
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Charges `regions` measured in `spent` to the writer's side.
    pub fn charge(&self, regions: u64, spent: Duration) {
        self.measured.fetch_add(regions, Ordering::Relaxed);
        self.measure_nanos
            .fetch_add(spent.as_nanos() as u64, Ordering::Relaxed);
    }

    /// What it cost and did.
    pub fn stats(&self) -> RegionSpectrumStats {
        RegionSpectrumStats {
            published: self.published.load(Ordering::Relaxed),
            requested: self.requested.load(Ordering::Relaxed),
            measured: self.measured.load(Ordering::Relaxed),
            publish_nanos: self.publish_nanos.load(Ordering::Relaxed),
            measure_nanos: self.measure_nanos.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> IntegratedSnapshot {
        IntegratedSnapshot {
            geometry: hk_detect::Geometry::new(
                100e6,
                2.4e6,
                1024,
                0.0,
                &hk_detect::EdgeRule::default(),
            ),
            span_s: 1.0,
            mean_psd: vec![1e-9; 1024],
            mean_floor: vec![1e-9; 1024],
            block_psd: Vec::new(),
        }
    }

    /// The steady state costs the reader one atomic load and never allocates.
    #[test]
    fn nothing_is_published_until_it_is_wanted() {
        let s = RegionSpectrum::default();
        let mut taken = 0;
        s.publish(|| {
            taken += 1;
            Some(snapshot())
        });
        assert_eq!(taken, 0);
        assert!(s.take().is_none());
        assert_eq!(s.stats().published, 0);
    }

    /// One request, one snapshot, used once.
    #[test]
    fn a_request_yields_exactly_one_snapshot() {
        let s = RegionSpectrum::default();
        s.want();
        s.publish(|| Some(snapshot()));
        s.publish(|| panic!("the request was cleared by the first publish"));
        assert!(s.take().is_some());
        assert!(s.take().is_none(), "a measurement is used once");
        assert_eq!(s.stats().published, 1);
        assert_eq!(s.stats().requested, 1);
    }

    /// A reader with no evaluation yet keeps the request standing rather than losing it.
    #[test]
    fn a_request_survives_a_reader_with_nothing_to_publish() {
        let s = RegionSpectrum::default();
        s.want();
        s.publish(|| None);
        assert!(s.wanted());
        s.publish(|| Some(snapshot()));
        assert!(s.take().is_some());
    }
}
