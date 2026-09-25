//! `audio_out`: the catalogue's first sink (ADR-0011 §8.4). It turns a `real` port into the
//! stream contract §12.2 audio profile — 48 kS/s, 960-sample `ri16_le` frames — and hands the
//! frames to the runtime through [`Block::audio_frames`]; the `audio` output kind publishes them.
//!
//! - **Resampling:** the shared DDC stages ([`Rate`], as `resample` uses) keep a 15 kHz audio band
//!   and reach the stopband by 18.5 kHz, so an FM MPX's 19 kHz pilot and everything above it
//!   are rejected. The rate only goes down: an input below 48 kS/s is refused at `init`.
//! - **Loudness** (optional, hot): a slow RMS follower (3 s) scales toward
//!   `loudness_target_dbfs`, within ±30 dB — C19's "loudness normalisation after demod" for FM.
//! - **Framing and time:** a frame's `sample_index` counts output samples from 0 at the stream's
//!   first sample. Contiguous input advances it by exactly one per sample; after a gap
//!   (`DISCONTINUITY`: a closed squelch, a live-edge skip, lost ring samples) it is re-derived from
//!   the source time map, so the jump in `sample_index` is the gap's duration, and the frame is
//!   flagged — the Listen profile's gap rule. A frame the gap cut short is dropped, never padded
//!   or glued to audio from after the gap (counted as `partial_dropped`).

use hk_recipe::{Params, PortType};
use hk_stream::audio::encode_pcm;
use num_complex::Complex32;

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::iq::common::*;
use crate::blocks::iq::filter::Rate;
use crate::registry::BuildCtx;
use crate::sink::AudioFrames;
use crate::status::Status;

/// Audio passband kept, Hz (broadcast FM's audio band).
const AUDIO_BAND_HZ: f64 = 15_000.0;
/// Stopband edge, Hz: below the 19 kHz stereo pilot.
const STOP_HZ: f64 = 18_500.0;
/// Loudness follower time constant, s.
const LOUDNESS_TAU_S: f64 = 3.0;
/// Loudness gain bound, dB either way.
const LOUDNESS_RANGE_DB: f64 = 30.0;
/// Output-level readout time constant, s.
const LEVEL_TAU_S: f64 = 0.3;

pub(crate) fn build(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let out_rate = f64_or(p, "output_rate_hz", 48_000.0);
    let frame = i64_or(p, "frame_samples", 960).max(1) as usize;
    Ok(Box::new(AudioOut {
        out_rate,
        frame,
        loudness: loudness_target(p),
        rate: Rate::new(out_rate, 2.0 * AUDIO_BAND_HZ, 60.0),
        pending: Vec::with_capacity(frame),
        pending_index: 0,
        pending_source: 0.0,
        pending_disc: false,
        next_index: None,
        origin: None,
        gap: true,
        frames: AudioFrames::default(),
        loud_ms: 0.0,
        loud_n: 0,
        loud_gain: 1.0,
        level_ms: 0.0,
        partial_dropped: 0,
        frames_out: 0,
        non_finite: 0,
        status: Status::default(),
    }))
}

fn loudness_target(p: &Params) -> Option<f64> {
    get_f64(p, "loudness_target_dbfs").map(|d| 10f64.powf(d / 20.0))
}

struct AudioOut {
    out_rate: f64,
    frame: usize,
    /// Loudness target RMS, linear.
    loudness: Option<f64>,
    rate: Rate,
    /// Samples of the frame being filled.
    pending: Vec<f32>,
    pending_index: u64,
    pending_source: f64,
    pending_disc: bool,
    /// Stream index of the next output sample while the input is contiguous.
    next_index: Option<u64>,
    /// Source index of stream sample 0.
    origin: Option<f64>,
    /// The next output sample follows a gap.
    gap: bool,
    frames: AudioFrames,
    loud_ms: f64,
    loud_n: u64,
    loud_gain: f64,
    level_ms: f64,
    partial_dropped: u64,
    frames_out: u64,
    non_finite: u64,
    status: Status,
}

impl AudioOut {
    /// Stream index of an output sample at source index `s` after a gap: its time since the
    /// stream began, never earlier than what was already emitted.
    fn index_after_gap(&mut self, s: f64, ring_rate: f64) -> u64 {
        let origin = *self.origin.get_or_insert(s);
        let t = ((s - origin) * self.out_rate / ring_rate).round().max(0.0) as u64;
        t.max(self.next_index.unwrap_or(0))
    }
}

impl Block for AudioOut {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "audio_out", &[PortType::Real])?;
        if input.rate_hz < self.out_rate * (1.0 - 1e-9) {
            return Err(BlockError::Unrealisable(format!(
                "audio_out needs an input rate of at least {} Hz",
                self.out_rate
            )));
        }
        let (max_out, _) = self.rate.init(&input, 0.0, Some(STOP_HZ))?;
        self.frames = AudioFrames::with_capacity(2 * self.frame, max_out / self.frame + 2);
        self.pending = Vec::with_capacity(self.frame);
        self.rate.restart(0);
        Ok(Vec::new())
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = real_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.rate.restart(m.index);
            self.partial_dropped += self.pending.len() as u64;
            self.pending.clear();
            self.gap = true;
        }
        let ring_rate = m.rate_hz * m.source_per_item;
        let first = self.rate.next_source(&m);
        let per = self.rate.per_item(&m);
        self.rate.scratch_in.clear();
        for &s in x {
            let s = finite_or_zero(s, &mut self.non_finite);
            self.rate.scratch_in.push(Complex32::new(s, 0.0));
        }
        self.rate.run();
        let loud_a = ema_alpha(LOUDNESS_TAU_S, self.out_rate);
        let level_a = ema_alpha(LEVEL_TAU_S, self.out_rate);
        let range = 10f64.powf(LOUDNESS_RANGE_DB / 20.0);
        let n_out = self.rate.scratch_out.len();
        for k in 0..n_out {
            let mut v = f64::from(self.rate.scratch_out[k].re);
            if let Some(target) = self.loudness {
                let w = loud_a.max(1.0 / (self.loud_n + 1) as f64);
                self.loud_n = self.loud_n.saturating_add(1);
                self.loud_ms += w * (v * v - self.loud_ms);
                self.loud_gain =
                    (target / self.loud_ms.sqrt().max(1e-12)).clamp(1.0 / range, range);
                v *= self.loud_gain;
            }
            let v = v.clamp(-1.0, 1.0);
            self.level_ms += level_a * (v * v - self.level_ms);
            if self.pending.is_empty() {
                let s = first + k as f64 * per;
                self.pending_index = if self.gap || self.next_index.is_none() {
                    self.index_after_gap(s, ring_rate)
                } else {
                    self.next_index.unwrap_or(0)
                };
                self.pending_source = s;
                self.pending_disc = std::mem::take(&mut self.gap);
            }
            self.pending.push(v as f32);
            self.next_index = Some(self.pending_index + self.pending.len() as u64);
            if self.pending.len() == self.frame {
                let (pending, frames) = (&self.pending, &mut self.frames);
                frames.push_with(
                    self.pending_index,
                    self.pending_source,
                    self.pending_disc,
                    |b| encode_pcm(pending, b),
                );
                self.pending.clear();
                self.frames_out += 1;
            }
        }
        let st = &mut self.status;
        st.items_in += x.len() as u64;
        st.items_out += n_out as u64;
        st.extra
            .set("level_dbfs", 10.0 * self.level_ms.max(1e-20).log10());
        st.extra.set("frames", self.frames_out as f64);
        st.extra.set("partial_dropped", self.partial_dropped as f64);
        if self.loudness.is_some() {
            st.extra
                .set("loudness_gain_db", 20.0 * self.loud_gain.max(1e-12).log10());
        }
        report_non_finite(st, self.non_finite);
        Ok(())
    }

    fn reset(&mut self) {
        self.rate.restart(0);
        self.partial_dropped += self.pending.len() as u64;
        self.pending.clear();
        self.gap = true;
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        // The cold keys each have one legal value (the audio profile's), so a schema-valid
        // update can only change `loudness_target_dbfs`, which is hot.
        self.loudness = loudness_target(p);
        if self.loudness.is_none() {
            self.loud_gain = 1.0;
        }
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }

    fn audio_frames(&mut self) -> Option<&mut AudioFrames> {
        Some(&mut self.frames)
    }
}
