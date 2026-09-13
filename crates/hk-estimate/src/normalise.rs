//! Normalised snippets for demodulation and classification (T-011 C14, T-012/T-013, C15).
//!
//! [`normalise`] runs a second DDC over the snippet: centred at the estimated CFO, flat over
//! `bandwidth_obw` × OBW99, at `samples_per_obw` × OBW99 (never above the snippet rate; raise
//! [`SnippetConfig::min_rate_hz`](crate::SnippetConfig::min_rate_hz) at extraction when more is
//! needed). The output is cut to the burst extent and scaled to unit mean power, and carries
//! the [`ParameterSet`], the provenance and an output → source time map composed through both
//! DDCs.

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::{ChannelTime, Ddc, DdcSpec, InputInfo};
use hk_model::SampleTime;
use num_complex::Complex32;
use serde::{Deserialize, Serialize};

use crate::estimate::Reason;
use crate::params::ParameterSet;
use crate::snippet::{ChannelSnippet, EstimateError};

/// Normalisation settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NormaliseConfig {
    /// Target output rate as a multiple of OBW99.
    pub samples_per_obw: f64,
    /// Flat channel bandwidth as a multiple of OBW99.
    pub bandwidth_obw: f64,
    /// Keep only the burst extent (else the whole snippet).
    pub extent_only: bool,
    /// Stopband attenuation, dB.
    pub stopband_db: f64,
}

impl Default for NormaliseConfig {
    fn default() -> Self {
        Self {
            samples_per_obw: 2.0,
            bandwidth_obw: 1.5,
            extent_only: true,
            stopband_db: 60.0,
        }
    }
}

/// Conditions of a normalisation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormaliseFlags {
    /// The target rate was not reachable (capped at the snippet rate or raised to fit the
    /// channel): see [`NormalisedSnippet::samples_per_obw`].
    pub rate_limited: bool,
    /// The channel bandwidth was narrowed to fit the snippet band.
    pub bandwidth_clamped: bool,
    /// The extent reaches beyond the samples the DDC could produce.
    pub extent_truncated: bool,
}

/// A snippet recentred, resampled and power-normalised for downstream consumers.
#[derive(Clone, Debug)]
pub struct NormalisedSnippet {
    /// Samples, unit mean power.
    pub samples: Vec<Complex32>,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Achieved samples per OBW99.
    pub samples_per_obw: f64,
    /// RF centre of the output (receiver frame), Hz.
    pub rf_center_hz: f64,
    /// CFO removed, Hz relative to the snippet centre.
    pub cfo_applied_hz: f64,
    /// Flat channel bandwidth, Hz.
    pub channel_bandwidth_hz: f64,
    /// Gain applied to reach unit mean power (output = scale × channel samples).
    pub power_scale: f64,
    /// Signal-to-noise in the channel bandwidth, dB (from the extent or box SNR), if measured.
    pub snr_in_channel_db: Option<f64>,
    /// Output index 0 → source stream index map.
    pub time: ChannelTime,
    /// The estimates it was normalised with.
    pub params: ParameterSet,
    /// Provenance of the source samples.
    pub provenance: ProvenanceHandle,
    /// Conditions.
    pub flags: NormaliseFlags,
}

/// Normalises `snip` with its estimates. Needs a measured OBW99 and CFO.
pub fn normalise(
    snip: &ChannelSnippet,
    params: &ParameterSet,
    config: &NormaliseConfig,
) -> Result<NormalisedSnippet, EstimateError> {
    let obw = params.obw99_hz.value().ok_or(EstimateError::Unmeasured {
        what: "obw99_hz",
        reason: params.obw99_hz.reason().unwrap_or(Reason::Upstream),
    })?;
    let cfo = params.cfo_hz.value().ok_or(EstimateError::Unmeasured {
        what: "cfo_hz",
        reason: params.cfo_hz.reason().unwrap_or(Reason::Upstream),
    })?;
    let fs = snip.sample_rate_hz;
    let mut flags = NormaliseFlags::default();
    let room = 0.49 * fs - cfo.abs();
    if room <= 0.0 {
        return Err(EstimateError::InvalidRequest(format!(
            "CFO {cfo} Hz is outside the snippet band ±{} Hz",
            fs / 2.0
        )));
    }
    let mut bw = config.bandwidth_obw * obw;
    if bw / 2.0 > room {
        bw = 2.0 * room;
        flags.bandwidth_clamped = true;
    }
    let mut rate = config.samples_per_obw * obw;
    if rate < 1.2 * bw {
        rate = 1.2 * bw;
        flags.rate_limited = true;
    }
    if rate > fs {
        rate = fs;
        flags.rate_limited = true;
        bw = bw.min(fs / 1.2);
    }

    let mut prov = snip.provenance.get().clone();
    prov.tune.sample_rate_hz = fs;
    prov.tune.center_hz = snip.rf_center_hz();
    let prov = ProvenanceHandle::with_id(snip.provenance.id(), prov);
    let spec = DdcSpec::new(cfo, bw)
        .with_output_rate(rate)
        .with_stopband_db(config.stopband_db);
    let mut ddc = Ddc::new(spec, fs)?;
    let info = InputInfo {
        time: SampleTime {
            sample_index: 0,
            host_time: snip.time.time.host_time,
        },
        discontinuity: Discontinuity::STREAM_START,
        dropped_before: 0,
        provenance: &prov,
    };
    let block = ddc.process(info, &snip.samples)?;
    let t = block.header.time;
    let n = block.samples.len();
    let (s0, s1) = match (config.extent_only, params.extent) {
        (true, Some(e)) => (e.start, e.end),
        _ => (0, snip.samples.len()),
    };
    let p0 = t.output_position_of(s0 as f64);
    let p1 = t.output_position_of(s1 as f64);
    flags.extent_truncated = p0 < 0.0 || p1 > n as f64;
    let k0 = (p0.ceil().max(0.0) as usize).min(n);
    let k1 = (p1.floor().max(0.0) as usize).min(n);
    if k1 <= k0 {
        return Err(EstimateError::NoOutput);
    }
    let mut samples = block.samples[k0..k1].to_vec();
    let mean_power =
        samples.iter().map(|s| f64::from(s.norm_sqr())).sum::<f64>() / samples.len() as f64;
    let power_scale = if mean_power > 0.0 {
        1.0 / mean_power.sqrt()
    } else {
        1.0
    };
    for s in &mut samples {
        *s *= power_scale as f32;
    }

    let local = t.source_index_of(k0);
    let source_index = snip.time.source_index + local * snip.time.source_per_output;
    let nearest = source_index.max(0.0).round() as u64;
    let time = ChannelTime {
        out_index: 0,
        source_index,
        source_per_output: t.source_per_output * snip.time.source_per_output,
        time: SampleTime {
            sample_index: nearest,
            host_time: snip.time.time.time_of(nearest, snip.source_rate_hz),
        },
    };
    let snr = params
        .snr_extent_db
        .value()
        .or_else(|| params.snr_box_db.value());
    Ok(NormalisedSnippet {
        samples,
        sample_rate_hz: rate,
        samples_per_obw: rate / obw,
        rf_center_hz: snip.rf_frequency_hz(cfo),
        cfo_applied_hz: cfo,
        channel_bandwidth_hz: bw,
        power_scale,
        snr_in_channel_db: snr.map(|db| db + 10.0 * (obw / bw).log10()),
        time,
        params: params.clone(),
        provenance: snip.provenance.clone(),
        flags,
    })
}
