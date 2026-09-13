//! Snippet extraction: one detection box → a [`ChannelSnippet`] through the hk-dsp DDC.
//!
//! The snippet is centred on the box, at a rate ≥ `rate_factor` × the guarded bandwidth
//! (`bandwidth_guard` × box width), with pre/post pads of `pad_s` (at least
//! `min_pad_samples` output samples) that the estimator uses as its signal-free N0 reference.
//! Integer decimation keeps the output → source time map on whole source samples.
//!
//! **Band edges.** A box whose guarded band crosses ±fs/2 (one transmission seen at both Nyquist
//! edges, S5/fixture `at_nyquist_edge`) is extracted in a Nyquist-wrapped frame: the input is
//! multiplied by `(−1)^n` (exact, by absolute sample index) and the channel is taken around
//! the shifted centre, so the two halves join. Frequencies are then only known modulo fs
//! ([`SnippetFlags::nyquist_wrapped`]).

use std::fmt;
use std::ops::Range;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::{ChannelTime, Ddc, DdcError, DdcSpec, InputInfo, IqSample};
use hk_model::{Detection, SampleTime, Timestamp, Tune};
use num_complex::Complex32;
use serde::{Deserialize, Serialize};

use crate::dsp::wrap_offset;

/// Why a snippet could not be extracted.
#[derive(Clone, Debug, PartialEq)]
pub enum EstimateError {
    /// The request is empty or not finite.
    InvalidRequest(String),
    /// The box does not overlap the samples given.
    OutOfRange {
        /// Requested box, source indices.
        requested: Range<u64>,
        /// Samples available, source indices.
        available: Range<u64>,
    },
    /// The DDC could not be planned.
    Ddc(DdcError),
    /// The DDC produced no output inside the requested span (too few samples for its filters).
    NoOutput,
    /// A value normalisation needs was not measured.
    Unmeasured {
        /// Which value.
        what: &'static str,
        /// Why it abstained.
        reason: crate::Reason,
    },
}

impl fmt::Display for EstimateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EstimateError::InvalidRequest(why) => write!(f, "invalid snippet request: {why}"),
            EstimateError::OutOfRange {
                requested,
                available,
            } => write!(
                f,
                "box {requested:?} does not overlap the samples {available:?}"
            ),
            EstimateError::Ddc(e) => write!(f, "snippet DDC: {e}"),
            EstimateError::NoOutput => f.write_str("DDC produced no output in the snippet span"),
            EstimateError::Unmeasured { what, reason } => {
                write!(f, "{what} was not measured ({reason:?})")
            }
        }
    }
}

impl std::error::Error for EstimateError {}

impl From<DdcError> for EstimateError {
    fn from(e: DdcError) -> Self {
        EstimateError::Ddc(e)
    }
}

/// A detection box in source coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnippetRequest {
    /// First source sample of the box (stream index).
    pub start_index: u64,
    /// One past the last source sample of the box.
    pub end_index: u64,
    /// Box centre relative to the tuned centre, Hz.
    pub center_offset_hz: f64,
    /// Box width, Hz (the detector's occupied bandwidth).
    pub bandwidth_hz: f64,
}

impl SnippetRequest {
    /// The box of a [`Detection`], mapped to source indices through a stream timing anchor.
    pub fn from_detection(det: &Detection, anchor: SampleTime, tune: &Tune) -> Self {
        let fs = tune.sample_rate_hz;
        let index = |t: Timestamp| {
            let dn = i128::from(t.as_unix_nanos()) - i128::from(anchor.host_time.as_unix_nanos());
            let ds = (dn as f64 * fs / 1e9).round();
            (anchor.sample_index as f64 + ds).max(0.0) as u64
        };
        let start = index(det.time.start);
        Self {
            start_index: start,
            end_index: index(det.time.end).max(start + 1),
            center_offset_hz: det.f_center_hz - tune.center_hz,
            bandwidth_hz: det.obw_hz,
        }
    }
}

/// Extraction settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnippetConfig {
    /// Pad before and after the box, seconds (S5: 2 ms).
    pub pad_s: f64,
    /// Minimum pad length, output samples.
    pub min_pad_samples: usize,
    /// Flat DDC bandwidth as a multiple of the box width (S5: 2).
    pub bandwidth_guard: f64,
    /// Smallest flat DDC bandwidth, Hz.
    pub min_bandwidth_hz: f64,
    /// Output rate ≥ `rate_factor` × flat bandwidth (S5: 1.25).
    pub rate_factor: f64,
    /// Smallest output rate, Hz (0: none). Raise it when a consumer (C14) needs more samples
    /// per symbol than the bandwidth rule gives.
    pub min_rate_hz: f64,
    /// DDC stopband attenuation, dB.
    pub stopband_db: f64,
    /// A box edge beyond this fraction of ±fs/2 sets [`SnippetFlags::edge`].
    pub edge_fraction: f64,
}

impl Default for SnippetConfig {
    fn default() -> Self {
        Self {
            pad_s: 2e-3,
            min_pad_samples: 256,
            bandwidth_guard: 2.0,
            min_bandwidth_hz: 500.0,
            rate_factor: 1.25,
            min_rate_hz: 0.0,
            stopband_db: 60.0,
            edge_fraction: 0.9,
        }
    }
}

/// Conditions of an extraction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnippetFlags {
    /// The box reaches beyond `edge_fraction` of ±fs/2 (filter roll-off, truncation possible).
    pub edge: bool,
    /// Extracted in the Nyquist-wrapped frame: frequencies are known modulo fs.
    pub nyquist_wrapped: bool,
    /// The guard band was narrowed to fit the input band.
    pub guard_clamped: bool,
    /// The box extends beyond the samples given (in time).
    pub box_truncated: bool,
    /// The pre-pad is shorter than `min_pad_samples`.
    pub pre_pad_short: bool,
    /// The post-pad is shorter than `min_pad_samples`.
    pub post_pad_short: bool,
}

/// One detection's channelised IQ with its box, pads, time map and provenance.
#[derive(Clone, Debug)]
pub struct ChannelSnippet {
    /// Baseband samples centred on the box (full scale 1).
    pub samples: Vec<Complex32>,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Half-width of the flat DDC passband, Hz.
    pub passband_hz: f64,
    /// Source (input) sample rate, Hz.
    pub source_rate_hz: f64,
    /// Tuned centre of the source, Hz.
    pub tuned_center_hz: f64,
    /// Snippet centre relative to the tuned centre (physical, wrapped into ±fs/2), Hz.
    pub center_offset_hz: f64,
    /// Detection box width, Hz.
    pub box_bandwidth_hz: f64,
    /// Samples inside the detection box; samples before/after are the pads.
    pub box_range: Range<usize>,
    /// Output index 0 → source index map.
    pub time: ChannelTime,
    /// Provenance of the source samples.
    pub provenance: ProvenanceHandle,
    /// Clipped source samples (a component at full scale) over box + pads.
    pub clip_count: u64,
    /// Source samples checked for clipping.
    pub clip_checked: u64,
    /// Extraction conditions.
    pub flags: SnippetFlags,
}

impl ChannelSnippet {
    /// RF frequency of a baseband offset (receiver frame, no ppm correction), Hz. Wrapped into
    /// the source band, so modulo fs in a Nyquist-wrapped snippet.
    pub fn rf_frequency_hz(&self, offset_hz: f64) -> f64 {
        self.tuned_center_hz + wrap_offset(self.center_offset_hz + offset_hz, self.source_rate_hz)
    }

    /// RF centre of the snippet, Hz.
    pub fn rf_center_hz(&self) -> f64 {
        self.rf_frequency_hz(0.0)
    }

    /// Source stream index of snippet sample `k` (fractional).
    pub fn source_index_of(&self, k: usize) -> f64 {
        self.time.source_index_of(k)
    }

    /// Fraction of checked source samples that clipped.
    pub fn clip_fraction(&self) -> f64 {
        if self.clip_checked == 0 {
            0.0
        } else {
            self.clip_count as f64 / self.clip_checked as f64
        }
    }
}

/// Extracts snippets. Holds a scratch buffer reused across calls.
#[derive(Debug, Default)]
pub struct SnippetExtractor {
    config: SnippetConfig,
    scratch: Vec<Complex32>,
}

/// A component at or beyond ±127/128 of full scale (ci8 −128 or 127) counts as clipped.
const CLIP_LEVEL: f32 = 127.0 / 128.0 - 1e-6;

impl SnippetExtractor {
    /// An extractor with `config`.
    pub fn new(config: SnippetConfig) -> Self {
        Self {
            config,
            scratch: Vec::new(),
        }
    }

    /// Settings.
    pub fn config(&self) -> &SnippetConfig {
        &self.config
    }

    /// Cuts the snippet for `request` from contiguous `samples` whose first sample is
    /// `info.time.sample_index` (rate and tuning from `info.provenance`). The samples should
    /// extend beyond the box by the pads plus the DDC filter span; shorter pads are flagged.
    pub fn extract<T: IqSample>(
        &mut self,
        info: InputInfo<'_>,
        samples: &[T],
        request: &SnippetRequest,
    ) -> Result<ChannelSnippet, EstimateError> {
        let c = self.config;
        let tune = &info.provenance.tune;
        let fs = tune.sample_rate_hz;
        if !(fs.is_finite() && fs > 0.0) {
            return Err(EstimateError::InvalidRequest(format!("sample rate {fs}")));
        }
        if request.end_index <= request.start_index
            || !request.center_offset_hz.is_finite()
            || !request.bandwidth_hz.is_finite()
        {
            return Err(EstimateError::InvalidRequest(format!("{request:?}")));
        }
        let avail0 = info.time.sample_index;
        let avail1 = avail0 + samples.len() as u64;
        let box0 = request.start_index.max(avail0);
        let box1 = request.end_index.min(avail1);
        if box1 <= box0 {
            return Err(EstimateError::OutOfRange {
                requested: request.start_index..request.end_index,
                available: avail0..avail1,
            });
        }
        let mut flags = SnippetFlags {
            box_truncated: box0 != request.start_index || box1 != request.end_index,
            ..Default::default()
        };

        // Band geometry: normal or Nyquist-wrapped frame, whichever fits the guard.
        let mut offset = wrap_offset(request.center_offset_hz, fs);
        let mut box_bw = request.bandwidth_hz.max(0.0);
        // A box reaching ±fs/2 is most likely one half of an emission split across Nyquist (a
        // DC-centred detector spectrum cuts it there): use its union with the mirror half, so
        // both halves of a pair give the same snippet.
        let reach = offset.abs() + box_bw / 2.0;
        if offset != 0.0 && fs / 2.0 - reach <= 1e-3 * fs {
            box_bw = 2.0 * (fs / 2.0 - (offset.abs() - box_bw / 2.0)).max(box_bw / 2.0);
            offset = wrap_offset(fs / 2.0, fs);
        }
        flags.edge = offset.abs() + box_bw / 2.0 > c.edge_fraction * fs / 2.0;
        let guarded = (box_bw * c.bandwidth_guard)
            .max(c.min_bandwidth_hz)
            .max(fs * 1e-5);
        let limit = 0.49 * fs;
        let h_normal = limit - offset.abs();
        let wrapped_offset = offset - offset.signum() * fs / 2.0;
        let h_wrapped = limit - wrapped_offset.abs();
        let (mode_offset, wrapped, h) = if guarded / 2.0 <= h_normal || h_normal >= h_wrapped {
            (offset, false, h_normal)
        } else {
            (wrapped_offset, true, h_wrapped)
        };
        flags.nyquist_wrapped = wrapped;
        if h <= 0.0 {
            return Err(EstimateError::InvalidRequest(format!(
                "no room for a channel at {offset} Hz in ±{} Hz",
                fs / 2.0
            )));
        }
        let mut half = (guarded / 2.0).min(h);
        let rate_min = (2.0 * half * c.rate_factor).max(c.min_rate_hz);
        let d = ((fs / rate_min).floor() as usize).max(1);
        let out_rate = fs / d as f64;
        half = half.min(out_rate / (2.0 * c.rate_factor.max(1.0)));
        flags.guard_clamped = half < guarded / 2.0 * (1.0 - 1e-9);

        let spec = DdcSpec::new(mode_offset, 2.0 * half)
            .with_output_rate(out_rate)
            .with_stopband_db(c.stopband_db);
        let mut ddc = Ddc::new(spec, fs)?;
        let plan = ddc.plan();
        let margin = plan.xlate.len() as u64
            + plan
                .resample
                .as_ref()
                .map_or(0, |r| (r.taps_per_phase * plan.xlate_decimation) as u64)
            + 4 * d as u64;
        let pad = ((c.pad_s * fs).round() as u64).max((c.min_pad_samples * d) as u64);
        let want0 = box0.saturating_sub(pad).max(avail0);
        let want1 = (box1 + pad).min(avail1);
        let feed0 = want0.saturating_sub(margin).max(avail0);
        let feed1 = (want1 + margin).min(avail1);
        let local = (feed0 - avail0) as usize..(feed1 - avail0) as usize;

        let clip_span = (want0 - avail0) as usize..(want1 - avail0) as usize;
        let clip_count = samples[clip_span.clone()]
            .iter()
            .filter(|s| {
                let z = s.to_complex32();
                z.re.abs() >= CLIP_LEVEL || z.im.abs() >= CLIP_LEVEL
            })
            .count() as u64;

        let feed_info = InputInfo {
            time: SampleTime {
                sample_index: feed0,
                host_time: info.time.time_of(feed0, fs),
            },
            discontinuity: Discontinuity::STREAM_START,
            dropped_before: 0,
            provenance: info.provenance,
        };
        let block = if wrapped {
            self.scratch.clear();
            self.scratch
                .extend(samples[local].iter().enumerate().map(|(i, s)| {
                    let z = s.to_complex32();
                    if (feed0 + i as u64) % 2 == 1 { -z } else { z }
                }));
            ddc.process(feed_info, &self.scratch)?
        } else {
            ddc.process(feed_info, &samples[local])?
        };
        let t = block.header.time;
        let n_out = block.samples.len();
        let pos = |s: u64| t.output_position_of(s as f64).ceil().max(0.0) as usize;
        let k0 = pos(want0).min(n_out);
        let k1 = pos(want1).min(n_out);
        if k1 <= k0 {
            return Err(EstimateError::NoOutput);
        }
        let b0 = pos(box0).clamp(k0, k1) - k0;
        let b1 = pos(box1).clamp(k0, k1) - k0;
        if b1 <= b0 {
            return Err(EstimateError::NoOutput);
        }
        let len = k1 - k0;
        flags.pre_pad_short = b0 < c.min_pad_samples;
        flags.post_pad_short = len - b1 < c.min_pad_samples;
        let source_index = t.source_index_of(k0);
        let nearest = source_index.max(0.0).round() as u64;
        Ok(ChannelSnippet {
            samples: block.samples[k0..k1].to_vec(),
            sample_rate_hz: out_rate,
            passband_hz: half,
            source_rate_hz: fs,
            tuned_center_hz: tune.center_hz,
            center_offset_hz: offset,
            box_bandwidth_hz: box_bw,
            box_range: b0..b1,
            time: ChannelTime {
                out_index: 0,
                source_index,
                source_per_output: t.source_per_output,
                time: SampleTime {
                    sample_index: nearest,
                    host_time: info.time.time_of(nearest, fs),
                },
            },
            provenance: info.provenance.clone(),
            clip_count,
            clip_checked: clip_span.len() as u64,
            flags,
        })
    }
}
