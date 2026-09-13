//! HackRF One source: a **stub** behind the [`Source`] / [`SourceControl`] split. Every
//! operation returns [`SourceError::NotAvailable`].
//!
//! # Licensing (ADR-0010)
//!
//! Verified upstream (`greatscottgadgets/hackrf` `master`) and recorded in the ADR-0010 ledger:
//!
//! - `host/libhackrf/src/hackrf.c` and `hackrf.h` are **BSD-3-Clause**.
//! - libusb, libhackrf's dependency, is **LGPL-2.1**, used by dynamic linking.
//! - `host/hackrf-tools` (e.g. `hackrf_transfer.c`) and the firmware are GPL-2.0-or-later. They
//!   are not linked.
//!
//! Linking libhackrf in-process therefore looks permissible for a non-GPL core. The driver itself
//! is later work: its capture thread will own the stream and apply [`super::ControlMailbox`]
//! changes at block boundaries. Until then this stub keeps the API shape, and the capability
//! descriptor ([`SourceCapabilities::hackrf_one`]) is real, so the planner can reason about the
//! device before the driver exists.

use std::sync::Arc;

use num_complex::{Complex, Complex32};

use super::{Gains, Source, SourceCapabilities, SourceControl, SourceError};
use crate::block::BlockHeader;

/// The HackRF One stream (stub). [`HackRfSource::open`] always fails with
/// [`SourceError::NotAvailable`].
pub struct HackRfSource {
    control: Arc<HackRfControl>,
}

/// The HackRF One control handle (stub).
pub struct HackRfControl {
    capabilities: SourceCapabilities,
}

impl HackRfSource {
    /// Driver name used in errors and capabilities.
    pub const NAME: &'static str = "hackrf-one";

    /// Opens a HackRF One by serial (or the first found). Not available yet.
    pub fn open(_serial: Option<&str>) -> Result<Self, SourceError> {
        Err(not_available())
    }

    /// The capability descriptor this source will report.
    pub fn capabilities_descriptor() -> SourceCapabilities {
        SourceCapabilities::hackrf_one()
    }
}

fn not_available() -> SourceError {
    SourceError::NotAvailable {
        source_name: HackRfSource::NAME,
        reason: "the HackRF driver is not integrated yet (T-003 stub). Use SigmfReplaySource for \
                 offline work."
            .into(),
    }
}

impl Source for HackRfSource {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.control.capabilities
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        self.control.clone()
    }

    fn read_block(
        &mut self,
        _samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        Err(not_available())
    }

    fn read_block_ci8(
        &mut self,
        _samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        Err(not_available())
    }
}

impl SourceControl for HackRfControl {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.capabilities
    }

    fn tune(&self, _center_hz: f64) -> Result<(), SourceError> {
        Err(not_available())
    }

    fn set_sample_rate(&self, _sample_rate_hz: f64) -> Result<(), SourceError> {
        Err(not_available())
    }

    fn set_gains(&self, _gains: &Gains) -> Result<(), SourceError> {
        Err(not_available())
    }

    fn set_baseband_filter(&self, _bandwidth_hz: f64) -> Result<(), SourceError> {
        Err(not_available())
    }

    fn set_bias_tee(&self, _enabled: bool) -> Result<(), SourceError> {
        Err(not_available())
    }

    fn start(&self) -> Result<(), SourceError> {
        Err(not_available())
    }

    fn stop(&self) -> Result<(), SourceError> {
        Err(not_available())
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
        let control = HackRfControl {
            capabilities: HackRfSource::capabilities_descriptor(),
        };
        assert!(matches!(
            control.set_bias_tee(true),
            Err(SourceError::NotAvailable { .. })
        ));
    }
}
