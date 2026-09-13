//! hackriff core sample path. It holds the source abstraction for HackRF and later SDRs behind
//! one trait (C01), sweep survey (C02), and dwell capture into a RAM ring buffer with pre-trigger
//! read (C03). It hosts the survey/dwell attention scheduler (C04, ADR-0005) and the SigMF
//! file-replay source that drives offline tests. Real-time path: no Python, no allocation in the
//! steady state.
//!
//! - [`block`]: [`SampleBlock`] / [`BlockHeader`] with time, monotonic sample counter,
//!   [`ProvenanceHandle`] and [`Discontinuity`] flags.
//! - [`source`]: the [`Source`] trait, [`SourceCapabilities`], [`SigmfReplaySource`] and the
//!   [`HackRfSource`] stub.
//! - [`ring`]: the single-writer, multi-reader RAM ring ([`ring_buffer`]) with overrun
//!   accounting and pre-trigger capture.
//! - [`rt`]: best-effort priority hook for the capture/writer thread.
//!
//! The substrate is plain Rust with no dataflow-framework dependency (ADR-0001; spike S1 chose
//! an owned dataflow whose chains are reader cursors over this ring).

pub mod block;
pub mod ring;
pub mod rt;
pub mod source;

pub use block::{BlockHeader, Discontinuity, ProvenanceHandle, SampleBlock};
pub use ring::{
    CaptureSegment, CaptureStatus, CapturedWindow, PreTriggerCapture, ReadChunk, ReadOutcome,
    ResyncPolicy, RingConfig, RingError, RingHandle, RingReader, RingSample, RingWriter,
    TriggerWindow, ring_buffer,
};
pub use source::{
    HackRfSource, Pacing, ReplayOptions, SigmfReplaySource, Source, SourceCapabilities, SourceError,
};

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        assert_eq!(hk_model::sigmf::Datatype::Ci8.bytes_per_sample(), 2);
    }
}
