//! Live front-end control (T-042): a typed, device-generic handle from the running pipeline's
//! source into [`crate::ApiState::live_control`], for the authenticated control API (T-050) to
//! change the tuned centre, sample rate, named gains and (optionally) the bias tee. **Receive-only
//! controls**: there is no transmit operation. This module adds no HTTP endpoint.
//!
//! - [`LiveControl`] is the contract: read the capabilities and current [`LiveTuning`], then
//!   `set_center` / `set_rate` / `set_gains` (named stages, e.g. `lna`, `vga`, `amp` on a HackRF
//!   One; any stage a device's capabilities list) / `set_bias_tee` (optional capability). Each
//!   returns the tuning now in force, or a [`LiveControlError`] with an HTTP status
//!   ([`LiveControlError::http_status`]).
//! - [`SourceLiveControl`] implements it over any `hk_core::SourceControl`: values are checked
//!   against the source's `SourceCapabilities` (frequency ranges, sample rates, named gain stages
//!   quantised by `GainStage::quantise`, bias-tee capability) before the command is posted; the
//!   source applies it at its next block boundary. Requests are serialised.
//! - **Policy hooks** set by the server composition:
//!   - [`SourceLiveControl::with_window_policy`] refuses a `(centre, rate)` window the run may
//!     not tune to. `hk serve` uses it to keep the run's content class: a window whose
//!     band-derived class differs (e.g. into a paging allocation) is refused, since spectrum,
//!     recordings and decodes were gated for the class computed at start.
//!   - [`SourceLiveControl::with_fixed_rate`] refuses rate changes: the pipeline's detection
//!     resolution, ring and history geometry are fixed per run, so a new rate needs a restart.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hk_core::{NamedGain, SourceCapabilities, SourceControl, SourceError};

/// The front-end settings in force (as last accepted).
#[derive(Clone, Debug, PartialEq)]
pub struct LiveTuning {
    /// Centre frequency, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz (the displayed span).
    pub sample_rate_hz: f64,
    /// Named gains, one per stage set so far.
    pub gains: Vec<NamedGain>,
    /// Bias-tee state; `None` when the device has no bias tee.
    pub bias_tee: Option<bool>,
}

/// Why a control request was not applied.
#[derive(Debug)]
pub enum LiveControlError {
    /// The value is outside the device's capabilities (HTTP 400).
    OutOfRange {
        /// Which setting (e.g. `centre frequency (Hz)`, `gain stage lna`).
        what: String,
        /// The rejected value.
        value: f64,
    },
    /// The device lacks the capability, e.g. a bias tee (HTTP 501).
    Unsupported(&'static str),
    /// The running pipeline refuses it: another content class, or a fixed rate (HTTP 409).
    Refused(String),
    /// The source rejected the command (HTTP 502; 400 for its own range checks).
    Source(SourceError),
}

impl LiveControlError {
    /// The HTTP status an endpoint should answer with.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::OutOfRange { .. } | Self::Source(SourceError::OutOfRange { .. }) => 400,
            Self::Unsupported(_) | Self::Source(SourceError::Unsupported { .. }) => 501,
            Self::Refused(_) => 409,
            Self::Source(_) => 502,
        }
    }
}

impl fmt::Display for LiveControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange { what, value } => write!(f, "{what} {value} is out of range"),
            Self::Unsupported(what) => write!(f, "the device has no {what}"),
            Self::Refused(why) => f.write_str(why),
            Self::Source(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LiveControlError {}

/// Live control of the running source (see the [module docs](self)).
pub trait LiveControl: Send + Sync {
    /// What the source can do (ranges, gain stages, bias tee).
    fn capabilities(&self) -> &SourceCapabilities;
    /// The settings in force.
    fn tuning(&self) -> LiveTuning;
    /// Retunes the centre frequency, Hz.
    fn set_center(&self, center_hz: f64) -> Result<LiveTuning, LiveControlError>;
    /// Changes the sample rate (span), Hz.
    fn set_rate(&self, sample_rate_hz: f64) -> Result<LiveTuning, LiveControlError>;
    /// Sets named gain stages (all validated before any is applied; each quantised to its stage).
    fn set_gains(&self, gains: &[NamedGain]) -> Result<LiveTuning, LiveControlError>;
    /// Switches the bias tee (optional capability).
    fn set_bias_tee(&self, enabled: bool) -> Result<LiveTuning, LiveControlError>;
}

/// Decides whether the run may tune to `(center_hz, sample_rate_hz)`; `Err` explains why not.
pub type WindowPolicy = Arc<dyn Fn(f64, f64) -> Result<(), String> + Send + Sync>;

/// [`LiveControl`] over an `hk_core::SourceControl`.
pub struct SourceLiveControl {
    control: Arc<dyn SourceControl>,
    tuning: Mutex<LiveTuning>,
    policy: Option<WindowPolicy>,
    fixed_rate: bool,
}

impl SourceLiveControl {
    /// Controls `control`, whose source currently runs with `initial`.
    pub fn new(control: Arc<dyn SourceControl>, initial: LiveTuning) -> Self {
        Self {
            control,
            tuning: Mutex::new(initial),
            policy: None,
            fixed_rate: false,
        }
    }

    /// Refuses windows `policy` rejects.
    pub fn with_window_policy(mut self, policy: WindowPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Refuses any change of sample rate.
    pub fn with_fixed_rate(mut self) -> Self {
        self.fixed_rate = true;
        self
    }

    fn check_window(&self, center_hz: f64, rate_hz: f64) -> Result<(), LiveControlError> {
        match &self.policy {
            Some(p) => p(center_hz, rate_hz).map_err(LiveControlError::Refused),
            None => Ok(()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, LiveTuning> {
        self.tuning.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Checks named gains against the capabilities' stages and quantises each.
pub fn validate_gains(
    caps: &SourceCapabilities,
    gains: &[NamedGain],
) -> Result<Vec<NamedGain>, LiveControlError> {
    gains
        .iter()
        .map(|g| {
            caps.gain_stage(&g.stage)
                .and_then(|s| s.quantise(g.db))
                .map(|db| NamedGain::new(g.stage.clone(), db))
                .ok_or_else(|| LiveControlError::OutOfRange {
                    what: format!("gain stage {}", g.stage),
                    value: g.db,
                })
        })
        .collect()
}

impl LiveControl for SourceLiveControl {
    fn capabilities(&self) -> &SourceCapabilities {
        self.control.capabilities()
    }

    fn tuning(&self) -> LiveTuning {
        self.lock().clone()
    }

    fn set_center(&self, center_hz: f64) -> Result<LiveTuning, LiveControlError> {
        let mut t = self.lock();
        if !(center_hz.is_finite() && self.capabilities().supports_frequency(center_hz)) {
            return Err(LiveControlError::OutOfRange {
                what: "centre frequency (Hz)".into(),
                value: center_hz,
            });
        }
        self.check_window(center_hz, t.sample_rate_hz)?;
        self.control
            .tune(center_hz)
            .map_err(LiveControlError::Source)?;
        t.center_hz = center_hz;
        Ok(t.clone())
    }

    fn set_rate(&self, sample_rate_hz: f64) -> Result<LiveTuning, LiveControlError> {
        let mut t = self.lock();
        if !(sample_rate_hz.is_finite()
            && self.capabilities().sample_rates.supports(sample_rate_hz))
        {
            return Err(LiveControlError::OutOfRange {
                what: "sample rate (Hz)".into(),
                value: sample_rate_hz,
            });
        }
        if self.fixed_rate && sample_rate_hz != t.sample_rate_hz {
            return Err(LiveControlError::Refused(format!(
                "the sample rate is fixed at {} Hz for this run (detection resolution, ring and \
                 history geometry); restart with the new rate",
                t.sample_rate_hz
            )));
        }
        self.check_window(t.center_hz, sample_rate_hz)?;
        self.control
            .set_sample_rate(sample_rate_hz)
            .map_err(LiveControlError::Source)?;
        t.sample_rate_hz = sample_rate_hz;
        Ok(t.clone())
    }

    fn set_gains(&self, gains: &[NamedGain]) -> Result<LiveTuning, LiveControlError> {
        let mut t = self.lock();
        let accepted = validate_gains(self.capabilities(), gains)?;
        for g in accepted {
            self.control
                .set_gain(&g.stage, g.db)
                .map_err(LiveControlError::Source)?;
            match t.gains.iter_mut().find(|x| x.stage == g.stage) {
                Some(x) => x.db = g.db,
                None => t.gains.push(g),
            }
        }
        Ok(t.clone())
    }

    fn set_bias_tee(&self, enabled: bool) -> Result<LiveTuning, LiveControlError> {
        let mut t = self.lock();
        if !self.capabilities().bias_tee {
            return Err(LiveControlError::Unsupported("bias tee"));
        }
        self.control
            .set_bias_tee(enabled)
            .map_err(LiveControlError::Source)?;
        t.bias_tee = Some(enabled);
        Ok(t.clone())
    }
}

#[cfg(test)]
mod tests {
    use hk_core::Gains;

    use super::*;

    struct Recorder {
        caps: SourceCapabilities,
        calls: Mutex<Vec<String>>,
    }

    impl Recorder {
        fn push(&self, s: String) -> Result<(), SourceError> {
            self.calls.lock().unwrap().push(s);
            Ok(())
        }
    }

    impl SourceControl for Recorder {
        fn capabilities(&self) -> &SourceCapabilities {
            &self.caps
        }
        fn tune(&self, hz: f64) -> Result<(), SourceError> {
            self.push(format!("tune {hz}"))
        }
        fn set_sample_rate(&self, hz: f64) -> Result<(), SourceError> {
            self.push(format!("rate {hz}"))
        }
        fn set_gains(&self, _: &Gains) -> Result<(), SourceError> {
            unreachable!("the generic path uses set_gain")
        }
        fn set_gain(&self, stage: &str, db: f64) -> Result<(), SourceError> {
            self.push(format!("gain {stage} {db}"))
        }
        fn set_baseband_filter(&self, _: f64) -> Result<(), SourceError> {
            unreachable!()
        }
        fn set_bias_tee(&self, on: bool) -> Result<(), SourceError> {
            self.push(format!("bias {on}"))
        }
        fn start(&self) -> Result<(), SourceError> {
            Ok(())
        }
        fn stop(&self) -> Result<(), SourceError> {
            Ok(())
        }
    }

    fn live(caps: SourceCapabilities) -> (SourceLiveControl, Arc<Recorder>) {
        let rec = Arc::new(Recorder {
            caps,
            calls: Mutex::new(Vec::new()),
        });
        let initial = LiveTuning {
            center_hz: 100.8e6,
            sample_rate_hz: 2.4e6,
            gains: vec![NamedGain::new("lna", 32.0), NamedGain::new("vga", 30.0)],
            bias_tee: Some(false),
        };
        (
            SourceLiveControl::new(Arc::clone(&rec) as Arc<dyn SourceControl>, initial),
            rec,
        )
    }

    #[test]
    fn values_are_validated_against_the_capabilities() {
        let (lc, rec) = live(SourceCapabilities::hackrf_one());
        assert_eq!(lc.set_center(500e3).unwrap_err().http_status(), 400);
        assert_eq!(lc.set_center(f64::NAN).unwrap_err().http_status(), 400);
        assert_eq!(lc.set_rate(40e6).unwrap_err().http_status(), 400);
        let bad = [NamedGain::new("vga", 20.0), NamedGain::new("lna", 48.0)];
        assert_eq!(lc.set_gains(&bad).unwrap_err().http_status(), 400);
        let unknown = [NamedGain::new("mixer", 3.0)];
        assert!(
            lc.set_gains(&unknown)
                .unwrap_err()
                .to_string()
                .contains("mixer")
        );
        assert!(
            rec.calls.lock().unwrap().is_empty(),
            "nothing invalid reached the source"
        );
        let t = lc.set_center(101.1e6).unwrap();
        assert_eq!(t.center_hz, 101.1e6);
        let t = lc
            .set_gains(&[NamedGain::new("lna", 30.0), NamedGain::new("amp", 11.0)])
            .unwrap();
        assert_eq!(
            t.gains,
            vec![
                NamedGain::new("lna", 24.0),
                NamedGain::new("vga", 30.0),
                NamedGain::new("amp", 11.0)
            ],
            "quantised, merged by stage"
        );
        let t = lc.set_bias_tee(true).unwrap();
        assert_eq!(t.bias_tee, Some(true));
        assert_eq!(lc.tuning(), t);
        assert_eq!(
            *rec.calls.lock().unwrap(),
            vec![
                "tune 101100000".to_string(),
                "gain lna 24".to_string(),
                "gain amp 11".to_string(),
                "bias true".to_string()
            ]
        );
    }

    #[test]
    fn a_device_without_a_bias_tee_reports_unsupported() {
        let mut caps = SourceCapabilities::hackrf_one();
        caps.bias_tee = false;
        let (lc, rec) = live(caps);
        assert_eq!(lc.set_bias_tee(true).unwrap_err().http_status(), 501);
        assert!(rec.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn policies_refuse_other_windows_and_rate_changes() {
        let (lc, rec) = live(SourceCapabilities::hackrf_one());
        let lc = lc
            .with_window_policy(Arc::new(|center, _rate| {
                if (88e6..108e6).contains(&center) {
                    Ok(())
                } else {
                    Err("another content class".into())
                }
            }))
            .with_fixed_rate();
        let e = lc.set_center(930.5e6).unwrap_err();
        assert_eq!(e.http_status(), 409);
        assert!(e.to_string().contains("content class"));
        let e = lc.set_rate(10e6).unwrap_err();
        assert_eq!(e.http_status(), 409);
        assert!(lc.set_rate(2.4e6).is_ok(), "the current rate is accepted");
        assert!(lc.set_center(99.5e6).is_ok());
        assert_eq!(lc.tuning().center_hz, 99.5e6);
        assert!(
            !rec.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.contains("930500000"))
        );
    }
}
