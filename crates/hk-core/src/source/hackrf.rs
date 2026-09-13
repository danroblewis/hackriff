//! HackRF One source: a **stub** behind the [`Source`] trait. Every operation returns
//! [`SourceError::NotAvailable`].
//!
//! # Why a stub: licensing (ADR-0010)
//!
//! ADR-0010 lists libhackrf / hackrf tools as GPL, to verify before in-core use. Checked
//! 2026-09-13 against `greatscottgadgets/hackrf` `master`:
//!
//! - `host/hackrf-tools/src/hackrf_transfer.c` (and the other tools) and the firmware are
//!   **GPL-2.0-or-later**. The repository's top-level `COPYING` is GPLv2.
//! - `host/libhackrf/src/hackrf.c` carries a **BSD-3-Clause** header.
//!
//! So the library itself may be linkable from a non-GPL core. That is not yet recorded as a
//! confirmed ADR-0010 ledger row, and the brief for this task treats libhackrf as GPL. Until the
//! ledger confirms it, this crate **does not link libhackrf**. The live source will reach the
//! device through an isolated process: `hackrf_transfer -r -` piping ci8 bytes, decoded with
//! [`super::format::decode_into`], or a small driver process. Linking in-process needs a
//! confirmed licence row first.
//!
//! The capability descriptor ([`SourceCapabilities::hackrf_one`]) is real, so the planner can
//! reason about the device before the driver exists.

use num_complex::{Complex, Complex32};

use super::{Gains, Source, SourceCapabilities, SourceError};
use crate::block::BlockHeader;

/// The HackRF One source (stub). [`HackRfSource::open`] always fails with
/// [`SourceError::NotAvailable`].
pub struct HackRfSource {
    capabilities: SourceCapabilities,
}

impl HackRfSource {
    /// Driver name used in errors and capabilities.
    pub const NAME: &'static str = "hackrf-one";

    /// Opens a HackRF One by serial (or the first found). Not available yet.
    pub fn open(_serial: Option<&str>) -> Result<Self, SourceError> {
        Err(Self::not_available())
    }

    /// The capability descriptor this source will report.
    pub fn capabilities_descriptor() -> SourceCapabilities {
        SourceCapabilities::hackrf_one()
    }

    fn not_available() -> SourceError {
        SourceError::NotAvailable {
            source_name: Self::NAME,
            reason: "the HackRF driver is not integrated yet (T-003 stub). libhackrf is not \
                     linked (ADR-0010); live capture will run through an isolated process such \
                     as `hackrf_transfer -r -`. Use SigmfReplaySource for offline work."
                .into(),
        }
    }
}

impl Source for HackRfSource {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.capabilities
    }

    fn tune(&mut self, _center_hz: f64) -> Result<(), SourceError> {
        Err(Self::not_available())
    }

    fn set_sample_rate(&mut self, _sample_rate_hz: f64) -> Result<(), SourceError> {
        Err(Self::not_available())
    }

    fn set_gains(&mut self, _gains: &Gains) -> Result<(), SourceError> {
        Err(Self::not_available())
    }

    fn start(&mut self) -> Result<(), SourceError> {
        Err(Self::not_available())
    }

    fn stop(&mut self) -> Result<(), SourceError> {
        Err(Self::not_available())
    }

    fn read_block(
        &mut self,
        _samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        Err(Self::not_available())
    }

    fn read_block_ci8(
        &mut self,
        _samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        Err(Self::not_available())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_reports_not_available() {
        let err = HackRfSource::open(None).err().expect("stub never opens");
        assert!(matches!(
            err,
            SourceError::NotAvailable {
                source_name: "hackrf-one",
                ..
            }
        ));
        assert!(err.to_string().contains("not available"));
        assert_eq!(HackRfSource::capabilities_descriptor().adc_bits, 8);
    }
}
