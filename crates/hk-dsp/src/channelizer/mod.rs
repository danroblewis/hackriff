//! Channelizer (C11): a 2× oversampled polyphase filter bank (PFB) over a dwell window.
//!
//! **Geometry.** `M` channels (a power of two) split the input rate `fs` into channels spaced
//! `Δ = fs/M`, DC-centred like [`crate::spectrum::Spectrum`] bins: channel `c` (0..M) is
//! centred at `(c − M/2)·Δ`, so channel `M/2` is DC and channel 0 straddles `±fs/2`. Each
//! channel is output at `2Δ = 2·fs/M` (decimation `M/2`).
//!
//! **Prototype** ([`pfb_prototype`]): Kaiser low-pass, passband `Δ/2`, stopband `Δ`, default
//! 60 dB. Every frequency within `±Δ/2` of a channel centre passes flat (edge-straddling
//! signals appear in full in both neighbours); adjacent-channel leakage is ≤ −A dB; the
//! transition band stays inside the output Nyquist band, so channels are alias-free to −A dB.
//! The prototype is an *analysis* design: it is not power-complementary, so this is not a
//! perfect-reconstruction bank (synthesis/recombination is not in T-008's scope); its ripple
//! (≈ 0.018 dB p-p at 60 dB) and stopband meet or beat the PR-grade figures.
//!
//! **Algorithm.** Per output frame (every `M/2` input samples) the newest `L` samples are
//! weighted by the prototype and folded modulo `M` — each sample into slot
//! `(absolute stream index) mod M` — and transformed by one `M`-point FFT. Folding by absolute
//! index makes each channel output exactly "mix the input to baseband at the channel centre
//! using the absolute stream sample index, low-pass, sample":
//! `y_c[t] = Σ_i x[i]·h[n_t − i]·e^{−j2π f_c i/fs}`, with `n_t` the newest sample in the
//! window. So channel phase is continuous across blocks and resets, and matches a DDC tuned
//! to the channel centre. The taps carry an extra `(−1)^i` (equivalently `(−1)^slot`, `M`
//! even), which shifts the FFT by `M/2` bins so bin `c` *is* DC-centred channel `c`: with all
//! channels active the fold lands directly in the output frame and is transformed in place
//! (no fftshift, no copy).
//!
//! **Output layout.** Frame-major: `samples()[f·A + j]` is frame `f` of the `j`-th active
//! channel (`A` active channels). [`PfbOutput::frame`] gives a frame as a slice;
//! [`PfbOutput::channel`] gives one channel as a [`ChannelSamples`] stride view (iterate or copy
//! out). Frame-major storage is what keeps the bank fast: writing each frame across `M`
//! separate per-channel rows cost more than the fold and FFT together at `M = 512`.
//!
//! **Time map.** Output sample `t` represents source index `n_t − (L−1)/2` (group delay
//! removed); [`ChannelTime`] carries the first sample's source index, `M/2` source samples per
//! output, and a [`SampleTime`] from the input's anchor.
//!
//! **Continuity.** Filter state resets (history cleared, no output until `L` new samples) on
//! the flags in [`DEFAULT_CHANNEL_RESET_ON`] and on any gap. Flags and dropped-sample counts
//! from inputs are delivered on the next *non-empty* output block's [`ChannelHeader`].
//!
//! **Placement (ADR-0007).** On the Jetson the PFB runs on the GPU (`channelizer::gpu::CudaPfb`
//! behind the `gpu` feature; a stub for now). [`Pfb`] is the CPU implementation and fallback:
//! no per-block allocation in steady state, `Complex32` or `Complex<i8>` input converted as it
//! enters the filter window.

mod stream;

#[cfg(feature = "gpu")]
pub mod gpu;

pub use stream::DEFAULT_CHANNEL_RESET_ON;
pub(crate) use stream::StreamTracker;

use std::fmt;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_model::SampleTime;
use num_complex::{Complex, Complex32};
use serde::{Deserialize, Serialize};

use crate::fft::{CpuFft, FftBackend};
use crate::filter::history::History;
use crate::filter::kernels::fold;
use crate::filter::{DEFAULT_STOPBAND_DB, DesignError, FirDesign, pfb_prototype};
use crate::stft::{InputInfo, IqSample};

/// Maps output samples of a channel stream back to the source stream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelTime {
    /// Output-stream index of the first sample (counts every sample this processor emitted).
    pub out_index: u64,
    /// Source stream sample index the first output sample represents (group delay removed;
    /// fractional for resampled DDC outputs).
    pub source_index: f64,
    /// Source samples per output sample (`input_rate / output_rate`).
    pub source_per_output: f64,
    /// The first output sample's nearest source sample and its host time (from the input's
    /// anchor).
    pub time: SampleTime,
}

impl ChannelTime {
    /// Source index represented by output sample `k` of the block.
    pub fn source_index_of(&self, k: usize) -> f64 {
        self.source_index + k as f64 * self.source_per_output
    }

    /// Fractional output position (relative to the block start) of source index `source`.
    pub fn output_position_of(&self, source: f64) -> f64 {
        (source - self.source_index) / self.source_per_output
    }

    pub(crate) fn new(
        out_index: u64,
        source_index: f64,
        source_per_output: f64,
        anchor: SampleTime,
        input_rate_hz: f64,
    ) -> Self {
        let nearest = source_index.max(0.0).round() as u64;
        Self {
            out_index,
            source_index,
            source_per_output,
            time: SampleTime {
                sample_index: nearest,
                host_time: anchor.time_of(nearest, input_rate_hz),
            },
        }
    }
}

/// Metadata for one block of channel output.
#[derive(Clone, Copy, Debug)]
pub struct ChannelHeader<'a> {
    /// Output → source time map.
    pub time: ChannelTime,
    /// Output sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Provenance of the newest input sample that contributed.
    pub provenance: &'a ProvenanceHandle,
    /// Discontinuities since the previous non-empty block (reset reasons and pass-through
    /// flags). Empty blocks carry `NONE`; their flags move to the next non-empty block.
    pub discontinuity: Discontinuity,
    /// Source samples lost since the previous non-empty block.
    pub dropped_before: u64,
}

/// Why a channelizer could not be built.
#[derive(Clone, Debug, PartialEq)]
pub enum ChannelizerError {
    /// Invalid configuration (reason attached).
    InvalidConfig(String),
    /// Prototype design failed.
    Design(DesignError),
}

impl fmt::Display for ChannelizerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelizerError::InvalidConfig(why) => write!(f, "invalid PFB config: {why}"),
            ChannelizerError::Design(e) => write!(f, "PFB prototype: {e}"),
        }
    }
}

impl std::error::Error for ChannelizerError {}

impl From<DesignError> for ChannelizerError {
    fn from(e: DesignError) -> Self {
        ChannelizerError::Design(e)
    }
}

fn default_stopband_db() -> f64 {
    DEFAULT_STOPBAND_DB
}

/// PFB settings; also a serde data spec (`{"channels": 64, "stopband_db": 60, "active": [..]}`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PfbConfig {
    /// Channel count `M`: a power of two, 2..=65536.
    pub channels: usize,
    /// Prototype stopband attenuation, dB (default 60).
    #[serde(default = "default_stopband_db")]
    pub stopband_db: f64,
    /// DC-centred channel indices to materialise (in this order); `None` = all `M`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Vec<usize>>,
}

impl PfbConfig {
    /// All `channels` channels, 60 dB stopband.
    pub fn new(channels: usize) -> Self {
        Self {
            channels,
            stopband_db: DEFAULT_STOPBAND_DB,
            active: None,
        }
    }

    /// Checks the settings.
    pub fn validate(&self) -> Result<(), ChannelizerError> {
        let m = self.channels;
        if !(2..=65_536).contains(&m) || !m.is_power_of_two() {
            return Err(ChannelizerError::InvalidConfig(format!(
                "channels = {m}: need a power of two in 2..=65536"
            )));
        }
        if let Some(active) = &self.active {
            if active.is_empty() {
                return Err(ChannelizerError::InvalidConfig("empty active list".into()));
            }
            if let Some(&bad) = active.iter().find(|&&c| c >= m) {
                return Err(ChannelizerError::InvalidConfig(format!(
                    "active channel {bad} >= channels {m}"
                )));
            }
            let mut seen = vec![false; m];
            for &c in active {
                if std::mem::replace(&mut seen[c], true) {
                    return Err(ChannelizerError::InvalidConfig(format!(
                        "active channel {c} listed twice"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Input samples per output sample: `M/2`.
    pub fn decimation(&self) -> usize {
        self.channels / 2
    }

    /// Channel spacing `fs/M`, Hz.
    pub fn channel_spacing_hz(&self, input_rate_hz: f64) -> f64 {
        input_rate_hz / self.channels as f64
    }

    /// Per-channel output rate `2·fs/M`, Hz.
    pub fn output_rate_hz(&self, input_rate_hz: f64) -> f64 {
        2.0 * input_rate_hz / self.channels as f64
    }

    /// Centre offset of DC-centred channel `c`, Hz.
    pub fn channel_offset_hz(&self, c: usize, input_rate_hz: f64) -> f64 {
        (c as f64 - (self.channels / 2) as f64) * self.channel_spacing_hz(input_rate_hz)
    }

    /// The channel whose centre is nearest `offset_hz`.
    pub fn channel_for_offset_hz(&self, offset_hz: f64, input_rate_hz: f64) -> usize {
        let m = self.channels as i64;
        let k = (offset_hz / self.channel_spacing_hz(input_rate_hz)).round() as i64;
        (k + m / 2).rem_euclid(m) as usize
    }
}

/// One channel's samples in a frame-major [`PfbOutput`]: a stride view, no copy.
#[derive(Clone, Copy, Debug)]
pub struct ChannelSamples<'a> {
    data: &'a [Complex32],
    width: usize,
    slot: usize,
    frames: usize,
}

impl<'a> ChannelSamples<'a> {
    /// Samples in the block.
    pub fn len(&self) -> usize {
        self.frames
    }

    /// No samples.
    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// Sample `t`.
    pub fn get(&self, t: usize) -> Option<Complex32> {
        (t < self.frames).then(|| self.data[t * self.width + self.slot])
    }

    /// The samples in order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Complex32> + 'a {
        let (data, width, slot) = (self.data, self.width, self.slot);
        (0..self.frames).map(move |t| data[t * width + slot])
    }

    /// Copies the samples into `out[..len]`.
    pub fn copy_into(&self, out: &mut [Complex32]) {
        for (o, s) in out[..self.frames].iter_mut().zip(self.iter()) {
            *o = s;
        }
    }

    /// The samples as a new vector (allocates; for tests and tooling).
    pub fn to_vec(&self) -> Vec<Complex32> {
        self.iter().collect()
    }
}

/// One call's worth of PFB output: `frames` samples per active channel, stored frame-major.
#[derive(Clone, Copy, Debug)]
pub struct PfbOutput<'a> {
    /// Shared metadata (all channels have the same time map).
    pub header: ChannelHeader<'a>,
    /// Samples per channel in this block.
    pub frames: usize,
    /// Channel count `M`.
    pub channels: usize,
    /// Channel spacing, Hz.
    pub channel_spacing_hz: f64,
    active: &'a [usize],
    slot_of: &'a [u32],
    data: &'a [Complex32],
}

impl<'a> PfbOutput<'a> {
    /// Materialised channels (DC-centred indices), in slot order.
    pub fn active_channels(&self) -> &'a [usize] {
        self.active
    }

    /// No samples in this block.
    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// All samples, frame-major: `[f · active.len() + slot]`.
    pub fn samples(&self) -> &'a [Complex32] {
        self.data
    }

    /// Frame `f`: one sample per active channel, in slot order.
    pub fn frame(&self, f: usize) -> &'a [Complex32] {
        let a = self.active.len();
        &self.data[f * a..(f + 1) * a]
    }

    /// Samples of DC-centred channel `c`, if materialised.
    pub fn channel(&self, c: usize) -> Option<ChannelSamples<'a>> {
        let slot = *self.slot_of.get(c)?;
        (slot != u32::MAX).then(|| self.slot(slot as usize))
    }

    /// Samples of the `j`-th materialised channel.
    pub fn slot(&self, j: usize) -> ChannelSamples<'a> {
        assert!(j < self.active.len(), "slot {j} out of range");
        ChannelSamples {
            data: self.data,
            width: self.active.len(),
            slot: j,
            frames: self.frames,
        }
    }

    /// Centre offset of channel `c` from the input centre, Hz.
    pub fn channel_offset_hz(&self, c: usize) -> f64 {
        (c as f64 - (self.channels / 2) as f64) * self.channel_spacing_hz
    }
}

/// The PFB interface shared by the CPU implementation ([`Pfb`]) and the Jetson GPU backend
/// (`gpu::CudaPfb`). A runtime chain owns one on its own ring reader and feeds it chunks.
pub trait PfbBackend: Send {
    /// Short name for logs and benchmarks.
    fn name(&self) -> &'static str;

    /// Settings.
    fn config(&self) -> &PfbConfig;

    /// The analysis prototype.
    fn prototype(&self) -> &FirDesign;

    /// Filter-state reset flags (a gap always resets).
    fn set_reset_on(&mut self, flags: Discontinuity);

    /// Channelises contiguous `Complex32` samples.
    fn process_c32(&mut self, info: InputInfo<'_>, samples: &[Complex32]) -> PfbOutput<'_>;

    /// Channelises contiguous `Complex<i8>` samples (converted as they enter the window).
    fn process_ci8(&mut self, info: InputInfo<'_>, samples: &[Complex<i8>]) -> PfbOutput<'_>;

    /// Clears all state; the next input is a stream start.
    fn reset(&mut self);
}

/// CPU polyphase filter bank. See the [module docs](self).
pub struct Pfb {
    config: PfbConfig,
    design: FirDesign,
    /// Prototype taps times `(−1)^i`: shifts FFT bins so bin `c` is channel `c`.
    shifted_taps: [Vec<f32>; 2],
    m: usize,
    d: usize,
    len: usize,
    history: History,
    acc: Vec<Complex32>,
    fft: CpuFft,
    all_active: bool,
    active: Vec<usize>,
    slot_of: Vec<u32>,
    out: Vec<Complex32>,
    frame_cap: usize,
    countdown: usize,
    start: u64,
    frames_since_reset: u64,
    out_index: u64,
    tracker: StreamTracker,
}

impl Pfb {
    /// Designs the prototype and plans the FFT (allocates; do this off the real-time path or
    /// once per chain).
    pub fn new(config: PfbConfig) -> Result<Self, ChannelizerError> {
        config.validate()?;
        let design = pfb_prototype(config.channels, config.stopband_db)?;
        let m = config.channels;
        let d = m / 2;
        let len = design.len();
        let active: Vec<usize> = config.active.clone().unwrap_or_else(|| (0..m).collect());
        let all_active = active.len() == m && active.iter().enumerate().all(|(j, &c)| j == c);
        let mut slot_of = vec![u32::MAX; m];
        for (j, &c) in active.iter().enumerate() {
            slot_of[c] = j as u32;
        }
        let even: Vec<f32> = design
            .taps
            .iter()
            .enumerate()
            .map(|(i, &h)| if i % 2 == 0 { h } else { -h })
            .collect();
        let odd = even.iter().map(|&h| -h).collect();
        Ok(Self {
            history: History::new(len, len.max(d)),
            acc: vec![Complex32::default(); m],
            fft: CpuFft::new(m),
            shifted_taps: [even, odd],
            config,
            design,
            m,
            d,
            len,
            all_active,
            active,
            slot_of,
            out: Vec::new(),
            frame_cap: 0,
            countdown: len,
            start: 0,
            frames_since_reset: 0,
            out_index: 0,
            tracker: StreamTracker::new(DEFAULT_CHANNEL_RESET_ON),
        })
    }

    /// Prototype taps `L`.
    pub fn taps(&self) -> usize {
        self.len
    }

    /// Current reset flags.
    pub fn reset_on(&self) -> Discontinuity {
        self.tracker.reset_on()
    }

    fn restart(&mut self, start: u64) {
        self.history.clear();
        self.countdown = self.len;
        self.start = start;
        self.frames_since_reset = 0;
    }

    fn window_end(&self, frame: u64) -> u64 {
        self.start + (self.len - 1) as u64 + frame * self.d as u64
    }

    /// Channelises contiguous samples of either type. See [`PfbBackend::process_c32`].
    pub fn process<T: IqSample>(&mut self, info: InputInfo<'_>, samples: &[T]) -> PfbOutput<'_> {
        let begin = self.tracker.begin(&info, samples.len());
        if begin.reset {
            self.restart(info.time.sample_index);
        }
        let fs = info.provenance.tune.sample_rate_hz;
        let first_end = self.window_end(self.frames_since_reset);
        let first_out = self.out_index;

        let upcoming = if samples.len() >= self.countdown {
            1 + (samples.len() - self.countdown) / self.d
        } else {
            0
        };
        if upcoming > self.frame_cap {
            // Grows only when a block is larger than any before (not in steady state).
            self.frame_cap = upcoming;
            self.out
                .resize(self.active.len() * upcoming, Complex32::default());
        }

        let mut frames = 0;
        let mut pos = 0;
        while pos < samples.len() {
            let take = self.countdown.min(samples.len() - pos);
            self.history.push(&samples[pos..pos + take]);
            pos += take;
            self.countdown -= take;
            if self.countdown == 0 {
                self.frame(frames);
                frames += 1;
                self.countdown = self.d;
            }
        }

        let (discontinuity, dropped_before) = if frames > 0 {
            self.tracker.take_pending()
        } else {
            (Discontinuity::NONE, 0)
        };
        let delay = ((self.len - 1) / 2) as u64;
        let time = ChannelTime::new(
            first_out,
            (first_end - delay) as f64,
            self.d as f64,
            self.tracker.anchor(),
            fs,
        );
        PfbOutput {
            header: ChannelHeader {
                time,
                sample_rate_hz: self.config.output_rate_hz(fs),
                provenance: self.tracker.provenance(),
                discontinuity,
                dropped_before,
            },
            frames,
            channels: self.m,
            channel_spacing_hz: self.config.channel_spacing_hz(fs),
            active: &self.active,
            slot_of: &self.slot_of,
            data: &self.out[..frames * self.active.len()],
        }
    }

    #[inline]
    fn frame(&mut self, f: usize) {
        let n = self.window_end(self.frames_since_reset);
        // Fold each window sample into slot (absolute index) mod M; see the module docs.
        let mask = self.m - 1;
        let first = ((n + 1 - self.len as u64) & mask as u64) as usize;
        let taps = &self.shifted_taps[first & 1];
        if self.all_active {
            let dst = &mut self.out[f * self.m..(f + 1) * self.m];
            fold(dst, taps, self.history.window(), first);
            self.fft.forward(dst);
        } else {
            fold(&mut self.acc, taps, self.history.window(), first);
            self.fft.forward(&mut self.acc);
            let a = self.active.len();
            for (o, &c) in self.out[f * a..(f + 1) * a].iter_mut().zip(&self.active) {
                *o = self.acc[c];
            }
        }
        self.frames_since_reset += 1;
        self.out_index += 1;
    }
}

impl PfbBackend for Pfb {
    fn name(&self) -> &'static str {
        "cpu-pfb"
    }

    fn config(&self) -> &PfbConfig {
        &self.config
    }

    fn prototype(&self) -> &FirDesign {
        &self.design
    }

    fn set_reset_on(&mut self, flags: Discontinuity) {
        self.tracker.set_reset_on(flags);
    }

    fn process_c32(&mut self, info: InputInfo<'_>, samples: &[Complex32]) -> PfbOutput<'_> {
        self.process(info, samples)
    }

    fn process_ci8(&mut self, info: InputInfo<'_>, samples: &[Complex<i8>]) -> PfbOutput<'_> {
        self.process(info, samples)
    }

    fn reset(&mut self) {
        self.tracker.clear();
        self.restart(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_geometry_is_dc_centred() {
        let c = PfbConfig::new(8);
        assert_eq!(c.channel_offset_hz(4, 8e3), 0.0);
        assert_eq!(c.channel_offset_hz(0, 8e3), -4e3);
        assert_eq!(c.channel_offset_hz(7, 8e3), 3e3);
        assert_eq!(c.channel_for_offset_hz(-1e3, 8e3), 3);
        assert_eq!(c.channel_for_offset_hz(4e3, 8e3), 0);
        assert_eq!(c.output_rate_hz(8e3), 2e3);
    }

    #[test]
    fn config_serde_and_validation() {
        let c: PfbConfig = serde_json::from_str(r#"{"channels": 64, "active": [1, 2]}"#).unwrap();
        assert_eq!(c.stopband_db, 60.0);
        assert!(c.validate().is_ok());
        assert!(PfbConfig::new(48).validate().is_err());
        let dup = PfbConfig {
            active: Some(vec![3, 3]),
            ..PfbConfig::new(8)
        };
        assert!(dup.validate().is_err());
        assert!(serde_json::from_str::<PfbConfig>(r#"{"channels": 8, "bogus": 1}"#).is_err());
    }
}
