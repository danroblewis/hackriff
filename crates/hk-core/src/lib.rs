//! hackriff core sample path. It holds the source abstraction for HackRF and later SDRs behind
//! one trait pair (C01), sweep survey (C02), and dwell capture into a RAM ring buffer with
//! pre-trigger read (C03). It hosts the survey/dwell attention scheduler (C04, ADR-0005) and the
//! SigMF file-replay source that drives offline tests. Real-time path: no Python, no allocation in
//! the steady state.
//!
//! - [`block`]: [`SampleBlock`] / [`BlockHeader`] with time, monotonic sample counter,
//!   [`ProvenanceHandle`] and [`Discontinuity`] flags.
//! - [`source`]: the [`Source`] stream / [`SourceControl`] split, [`SourceCapabilities`],
//!   [`SigmfReplaySource`], the [`HackRfSource`] and [`RtlSdrSource`] drivers (cargo features
//!   `hackrf` / `rtlsdr`).
//! - [`gain`]: automatic front-end gain management (T-945): the device-generic policy that walks a
//!   device's gain stages and converges on the state whose *processed output* is best, rather than
//!   reacting to the overload flag. Off by default; actuated through `hk_api::gain`.
//! - [`ring`]: the single-writer, multi-reader RAM ring ([`ring_buffer`]) with exact loss
//!   accounting and streaming or in-memory pre-trigger capture.
//! - [`rt`]: best-effort priority and memory-lock hooks for the capture/writer thread.
//! - [`scheduler`]: the v1 attention scheduler (C02 sweep hops + C03 POI dwells, C04 policy):
//!   ScanPlan → deterministic [`ScheduleStep`]s applied through [`SourceControl`].
//!
//! The substrate is plain Rust with no dataflow-framework dependency (ADR-0001; spike S1 chose
//! an owned dataflow whose chains are reader cursors over this ring).

pub mod block;
pub mod gain;
pub mod ring;
pub mod rt;
pub mod scheduler;
pub mod source;

pub use block::{BlockHeader, Discontinuity, ProvenanceHandle, SampleBlock};
pub use gain::{
    DecodeQuality, GainController, GainError, GainLadder, GainPhase, GainPolicy, GainProbe,
    GainProbeRecord, GainQuality, GainReport, GainScore, GainState, GainStep, GainTrigger,
};
pub use ring::{
    CaptureError, CaptureSegment, CaptureStatus, CapturedWindow, PreTriggerCapture, ReadChunk,
    ReadOutcome, ResyncPolicy, RingConfig, RingError, RingHandle, RingReader, RingSample,
    RingWriter, TriggerRead, TriggerStream, TriggerWindow, ring_buffer,
};
pub use rt::MemoryLock;
pub use scheduler::{ScheduleStep, Scheduler, SchedulerConfig};
pub use source::{
    AccessoryKind, AccessoryMockDriver, AccessoryMockOptions, AccessorySource, AudioInput,
    AudioRead, BasebandFilters, ControlMailbox, Coverage, DeviceInfo, GainStage, Gains,
    HackRfConfig, HackRfDeviceInfo, HackRfDriver, HackRfSource, HackRfStats, InUseCertainty,
    MockClock, MockEnd, MockFault, MockOptions, MockSdrControl, MockSdrDriver, MockSdrSource,
    MockStats, NamedGain, OpenRequest, Pacing, PendingControl, R820T_GAINS_DB, R820T_MAX_HZ,
    R820T_MIN_HZ, R820T_RATES_HZ, R820T_TUNING_STEP_HZ, Recording, ReplayOptions, RtlSdrConfig,
    RtlSdrControl, RtlSdrDeviceInfo, RtlSdrDriver, RtlSdrSource, RtlSdrStats, SigmfReplaySource,
    Source, SourceCapabilities, SourceControl, SourceDriver, SourceError, SourceStats,
    SweepCapability, SweepPlan, TuningStep,
};

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        assert_eq!(hk_model::sigmf::Datatype::Ci8.bytes_per_sample(), 2);
    }
}
