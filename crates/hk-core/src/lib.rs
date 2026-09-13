//! hackriff core sample path. It holds the source abstraction for HackRF and later SDRs behind
//! one trait pair (C01), sweep survey (C02), and dwell capture into a RAM ring buffer with
//! pre-trigger read (C03). It hosts the survey/dwell attention scheduler (C04, ADR-0005) and the
//! SigMF file-replay source that drives offline tests. Real-time path: no Python, no allocation in
//! the steady state.
//!
//! - [`block`]: [`SampleBlock`] / [`BlockHeader`] with time, monotonic sample counter,
//!   [`ProvenanceHandle`] and [`Discontinuity`] flags.
//! - [`source`]: the [`Source`] stream / [`SourceControl`] split, [`SourceCapabilities`],
//!   [`SigmfReplaySource`] and the [`HackRfSource`] stub.
//! - [`ring`]: the single-writer, multi-reader RAM ring ([`ring_buffer`]) with exact loss
//!   accounting and streaming or in-memory pre-trigger capture.
//! - [`rt`]: best-effort priority and memory-lock hooks for the capture/writer thread.
//! - [`scheduler`]: the v1 attention scheduler (C02 sweep hops + C03 POI dwells, C04 policy):
//!   ScanPlan → deterministic [`ScheduleStep`]s applied through [`SourceControl`].
//!
//! The substrate is plain Rust with no dataflow-framework dependency (ADR-0001; spike S1 chose
//! an owned dataflow whose chains are reader cursors over this ring).

pub mod block;
pub mod ring;
pub mod rt;
pub mod scheduler;
pub mod source;

pub use block::{BlockHeader, Discontinuity, ProvenanceHandle, SampleBlock};
pub use ring::{
    CaptureError, CaptureSegment, CaptureStatus, CapturedWindow, PreTriggerCapture, ReadChunk,
    ReadOutcome, ResyncPolicy, RingConfig, RingError, RingHandle, RingReader, RingSample,
    RingWriter, TriggerRead, TriggerStream, TriggerWindow, ring_buffer,
};
pub use rt::MemoryLock;
pub use scheduler::{ScheduleStep, Scheduler, SchedulerConfig};
pub use source::{
    BasebandFilters, ControlMailbox, Coverage, DeviceInfo, GainStage, Gains, HackRfConfig,
    HackRfDeviceInfo, HackRfDriver, HackRfSource, HackRfStats, MockClock, MockEnd, MockOptions,
    MockSdrControl, MockSdrDriver, MockSdrSource, MockStats, NamedGain, OpenRequest, Pacing,
    PendingControl, Recording, ReplayOptions, SigmfReplaySource, Source, SourceCapabilities,
    SourceControl, SourceDriver, SourceError, SourceStats, SweepCapability, SweepPlan,
};

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        assert_eq!(hk_model::sigmf::Datatype::Ci8.bytes_per_sample(), 2);
    }
}
