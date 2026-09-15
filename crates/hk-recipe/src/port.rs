//! Port types: what flows between blocks (ADR-0011 §1.1).

use serde::{Deserialize, Serialize};

/// The sample kind of a block port. Closed set: a new kind is a contract change.
///
/// | Type | Element | Rate | Stage stream (`docs/stream-contract.md` §14.4) |
/// |---|---|---|---|
/// | `iq` | `Complex32` baseband | sample rate | `iq`, `cf32_le` |
/// | `real` | `f32` waveform (discriminator, MPX, envelope) | sample rate | `audio`, `rf32_le` |
/// | `soft` | `f32` soft symbol/bit, positive = 1 | symbol rate | `symbols`, `rf32_le` |
/// | `bits` | `u8` hard bit, 0 or 1 | bit rate | `bits`, `ru8` |
/// | `frames` | byte record: bytes + bit length + frame info (+ layer tree) | frame rate | inspector (`messages`, `frame` records) |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PortType {
    /// Complex baseband samples.
    Iq,
    /// Real waveform samples.
    Real,
    /// Soft symbols (LLR-like, positive = 1).
    Soft,
    /// Hard bits, one byte per bit.
    Bits,
    /// Byte records, one per frame.
    Frames,
}

impl PortType {
    /// Every port type.
    pub const ALL: [PortType; 5] = [
        PortType::Iq,
        PortType::Real,
        PortType::Soft,
        PortType::Bits,
        PortType::Frames,
    ];

    /// Wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            PortType::Iq => "iq",
            PortType::Real => "real",
            PortType::Soft => "soft",
            PortType::Bits => "bits",
            PortType::Frames => "frames",
        }
    }

    /// Sample-rate ports carry elements at a fixed rate and are **allocation-free in steady
    /// state**; `frames` is a frame-rate port and may allocate a bounded amount per frame
    /// (ADR-0011 §1.4).
    pub const fn is_sample_rate(self) -> bool {
        !matches!(self, PortType::Frames)
    }

    /// Stream kind (`docs/stream-contract.md` §4) and SigMF datatype of this port's raw stage
    /// stream; `frames` go out as `messages` streams of `frame` records (§14).
    pub const fn stage_stream(self) -> (&'static str, Option<&'static str>) {
        match self {
            PortType::Iq => ("iq", Some("cf32_le")),
            PortType::Real => ("audio", Some("rf32_le")),
            PortType::Soft => ("symbols", Some("rf32_le")),
            PortType::Bits => ("bits", Some("ru8")),
            PortType::Frames => ("messages", None),
        }
    }
}

impl std::fmt::Display for PortType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
