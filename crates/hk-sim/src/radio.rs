//! The single half-duplex radio: what a [`ScheduleStep`] actually observes, and what each retune
//! or mode switch costs.
//!
//! - **Sweep mode** (`Purpose::Sweep` steps) models `hackrf_sweep` firmware sweeps: 20 Msps
//!   captures keeping ~10 MHz per step (two interleaved 5 MHz sub-bands, so no DC notch), ~1.25 ms
//!   per step of which ~0.82 ms is retune/settle (docs/04 §3.8: ~600 steps, ~0.75 s for 0–6 GHz,
//!   sub-ms dwell).
//! - **Stream mode** (every other step) is a real-time window: usable `rate × usable_fraction`
//!   (≤ 20 MHz, ~15 MHz at 20 Msps) around the tuned centre, minus a DC notch.
//! - Switching mode costs `mode_switch_ns` (unmeasured on the HackRF; a parameter, C04), a
//!   stream retune `stream_retune_ns`, a sweep step `sweep_settle_ns`. The cost is dead time at the
//!   start of the step (the scheduler's clock is not stretched).
//! - **Detection:** a transmission is detected when it overlaps the live part of a window by at
//!   least `min_overlap_ns`, its centre is inside the observed band (and outside the DC notch in
//!   stream mode), and its SNR is at or above `detect_snr_db`.

use hk_core::ScheduleStep;
use hk_core::scheduler::{Purpose, SchedulerConfig};
use serde::Serialize;

use crate::scenario::MS;

/// Radio mode of a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RadioMode {
    /// Firmware sweep.
    Sweep,
    /// Real-time stream (dwell).
    Stream,
}

/// Radio model parameters.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RadioModel {
    /// Sample rate of sweep captures, Hz (20 Msps).
    pub sweep_rate_hz: f64,
    /// Band kept per sweep step, Hz (10 MHz).
    pub sweep_span_hz: f64,
    /// One sweep step, ns (1.25 ms).
    pub sweep_step_ns: i64,
    /// Dead time at the start of each sweep step, ns (0.82 ms).
    pub sweep_settle_ns: i64,
    /// Usable fraction of the stream rate (0.75).
    pub stream_usable_fraction: f64,
    /// Largest usable stream span, Hz (20 MHz).
    pub max_span_hz: f64,
    /// DC notch half-width in stream mode, Hz (50 kHz).
    pub dc_notch_hz: f64,
    /// Stream-mode retune dead time, ns (10 ms).
    pub stream_retune_ns: i64,
    /// Sweep↔stream switch dead time, ns (50 ms; unmeasured, C04).
    pub mode_switch_ns: i64,
    /// Detection threshold, dB.
    pub detect_snr_db: f64,
    /// Minimum overlap of a transmission with the live window, ns (1: any overlap).
    pub min_overlap_ns: i64,
}

impl Default for RadioModel {
    fn default() -> Self {
        Self {
            sweep_rate_hz: 20e6,
            sweep_span_hz: 10e6,
            sweep_step_ns: 1_250_000,
            sweep_settle_ns: 820_000,
            stream_usable_fraction: 0.75,
            max_span_hz: 20e6,
            dc_notch_hz: 50e3,
            stream_retune_ns: 10 * MS,
            mode_switch_ns: 50 * MS,
            detect_snr_db: 6.0,
            min_overlap_ns: 1,
        }
    }
}

/// Where the radio was tuned before a step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    /// Mode.
    pub mode: RadioMode,
    /// Centre, Hz.
    pub center_hz: f64,
    /// Rate, Hz.
    pub rate_hz: f64,
}

impl RadioModel {
    /// Mode a step runs in.
    pub fn mode_of(&self, step: &ScheduleStep) -> RadioMode {
        match step.purpose {
            Purpose::Sweep { .. } => RadioMode::Sweep,
            _ => RadioMode::Stream,
        }
    }

    /// Observed band `(lo, hi)`, Hz.
    pub fn band(&self, step: &ScheduleStep, mode: RadioMode) -> (f64, f64) {
        let span = match mode {
            RadioMode::Sweep => self.sweep_span_hz,
            RadioMode::Stream => (step.rate_hz * self.stream_usable_fraction).min(self.max_span_hz),
        };
        (step.center_hz - span / 2.0, step.center_hz + span / 2.0)
    }

    /// Dead time at the start of `step` after `prev`.
    pub fn latency_ns(&self, prev: Option<Tuning>, mode: RadioMode, step: &ScheduleStep) -> i64 {
        let Some(prev) = prev else {
            return self.mode_switch_ns;
        };
        if prev.mode != mode {
            return self.mode_switch_ns;
        }
        let same = prev.center_hz == step.center_hz && prev.rate_hz == step.rate_hz;
        match (mode, same) {
            (_, true) => 0,
            (RadioMode::Sweep, false) => self.sweep_settle_ns,
            (RadioMode::Stream, false) => self.stream_retune_ns,
        }
    }

    /// Whether a transmission at `f_hz` with `snr_db` is visible in a window at `center_hz`.
    pub fn visible(
        &self,
        mode: RadioMode,
        band: (f64, f64),
        center_hz: f64,
        f_hz: f64,
        snr_db: f64,
    ) -> bool {
        f_hz >= band.0
            && f_hz <= band.1
            && snr_db >= self.detect_snr_db
            && (mode == RadioMode::Sweep || (f_hz - center_hz).abs() >= self.dc_notch_hz)
    }

    /// Scheduler settings whose sweep hops match this radio's sweep steps (span and duration);
    /// everything else at the hk-core defaults.
    pub fn sweep_scheduler_config(&self) -> SchedulerConfig {
        SchedulerConfig {
            sweep_rate_hz: self.sweep_rate_hz,
            usable_fraction: (self.sweep_span_hz / self.sweep_rate_hz).min(1.0),
            max_span_hz: self.max_span_hz,
            sweep_step_ns: self.sweep_step_ns,
            ..SchedulerConfig::default()
        }
    }

    /// Scheduler settings whose discovery windows are stream windows of this radio.
    pub fn stream_scheduler_config(&self) -> SchedulerConfig {
        SchedulerConfig {
            sweep_rate_hz: self.sweep_rate_hz,
            usable_fraction: self.stream_usable_fraction,
            max_span_hz: self.max_span_hz,
            ..SchedulerConfig::default()
        }
    }
}
