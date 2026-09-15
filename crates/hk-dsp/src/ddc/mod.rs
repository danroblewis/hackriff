//! On-demand digital down-converter (C11): one channel at an arbitrary centre offset and
//! bandwidth, built at runtime from a [`DdcSpec`] data spec.
//!
//! **Strategy** (details on [`DdcPlan`]): NCO mix + xlating decimating FIR (integer decimation
//! at the input rate), then an optional polyphase stage at the low rate: integer, exact
//! rational `P/Q` (`P ≤ 64`), or fractional (64 branches, linearly interpolated) for any other
//! ratio. The PFB-channel-pick alternative is not used: a DDC serves one detection at its
//! exact centre, while the PFB ([`crate::channelizer`]) serves dense fixed rasters.
//!
//! **Phase continuity.** The NCO phase is computed from the absolute source sample index of
//! each output, so output is bit-identical however the input is chunked, and the mix phase is
//! consistent across resets. A tone at `f0 + δ` comes out as `A·e^{j(2πδ·τ/fs + φ)}`, where
//! `τ` is the output's source index from [`ChannelTime`].
//!
//! **Continuity and time map.** Same rules as the PFB: resets (history cleared, the first
//! output needs a full window again) on [`DEFAULT_CHANNEL_RESET_ON`] flags and any gap; flags
//! and losses ride on the next non-empty [`DdcBlock`]; a rate change re-plans the filters.
//!
//! **Node shape.** A runtime chain (ADR-0001, spike S1) owns a [`Ddc`] on its own ring reader
//! and calls [`Ddc::process`] with each `ReadChunk` (`InputInfo::from(&chunk)`) and the
//! samples, `Complex32` or `Complex<i8>`; the returned block borrows the DDC's output buffer
//! (no allocation in steady state).

mod plan;
mod stages;

pub use plan::{
    DdcError, DdcPlan, DdcSpec, FRACTIONAL_PHASES, MAX_RATIONAL_UP, ResampleKind, ResamplePlan,
};

use hk_core::Discontinuity;
use num_complex::Complex32;

use crate::channelizer::{ChannelHeader, ChannelTime, DEFAULT_CHANNEL_RESET_ON, StreamTracker};
use crate::stft::{InputInfo, IqSample};
use stages::{Polyphase, Xlating};

/// One call's DDC output.
#[derive(Clone, Copy, Debug)]
pub struct DdcBlock<'a> {
    /// Time map, rate, provenance and continuity.
    pub header: ChannelHeader<'a>,
    /// Baseband samples, centred on the channel.
    pub samples: &'a [Complex32],
    /// Channel centre offset from the input centre, Hz.
    pub center_offset_hz: f64,
}

impl DdcBlock<'_> {
    /// Absolute channel centre, Hz (provenance centre + offset).
    pub fn center_hz(&self) -> f64 {
        self.header.provenance.tune.center_hz + self.center_offset_hz
    }
}

/// A running DDC. See the [module docs](self).
pub struct Ddc {
    spec: DdcSpec,
    plan: DdcPlan,
    xlate: Xlating,
    resample: Option<Polyphase>,
    tracker: StreamTracker,
    out: Vec<Complex32>,
    out_index: u64,
}

impl Ddc {
    /// Plans and builds a DDC for inputs at `input_rate_hz` (allocates and designs filters).
    pub fn new(spec: DdcSpec, input_rate_hz: f64) -> Result<Self, DdcError> {
        let plan = DdcPlan::new(&spec, input_rate_hz)?;
        Ok(Self::from_plan(
            spec,
            plan,
            StreamTracker::new(DEFAULT_CHANNEL_RESET_ON),
            0,
        ))
    }

    fn from_plan(spec: DdcSpec, plan: DdcPlan, tracker: StreamTracker, out_index: u64) -> Self {
        let xlate = Xlating::new(
            &plan.xlate,
            plan.xlate_decimation,
            plan.center_offset_hz,
            plan.input_rate_hz,
        );
        let resample = plan.resample.as_ref().map(Polyphase::new);
        Self {
            spec,
            plan,
            xlate,
            resample,
            tracker,
            out: Vec::new(),
            out_index,
        }
    }

    /// The spec it was built from.
    pub fn spec(&self) -> &DdcSpec {
        &self.spec
    }

    /// The current plan.
    pub fn plan(&self) -> &DdcPlan {
        &self.plan
    }

    /// Output rate, Hz.
    pub fn output_rate_hz(&self) -> f64 {
        self.plan.output_rate_hz
    }

    /// Sets the flags that reset filter state (a gap always resets).
    pub fn set_reset_on(&mut self, flags: Discontinuity) {
        self.tracker.set_reset_on(flags);
    }

    /// Clears all state; the next input is a stream start.
    pub fn reset(&mut self) {
        self.tracker.clear();
        self.restart(0);
    }

    fn restart(&mut self, start: u64) {
        self.xlate.restart(start);
        if let Some(r) = &mut self.resample {
            r.restart();
        }
    }

    fn next_source_index(&self) -> f64 {
        match &self.resample {
            None => self.xlate.source_index_of(self.xlate.count() as f64),
            Some(r) => self.xlate.source_index_of(r.next_position()),
        }
    }

    /// Down-converts contiguous samples.
    ///
    /// Errors when the input's sample rate makes the spec unrealisable. The check runs on every
    /// call, so every block at such a rate errors, and its samples are counted as lost. The
    /// first block at a realisable rate re-plans (if needed), restarts, and reports
    /// `RATE_CHANGE | GAP` with the lost samples. Re-planning allocates; it happens only at a
    /// rate change.
    pub fn process<T: IqSample>(
        &mut self,
        info: InputInfo<'_>,
        samples: &[T],
    ) -> Result<DdcBlock<'_>, DdcError> {
        let begin = self.tracker.begin(&info, samples.len());
        let fs = info.provenance.tune.sample_rate_hz;
        let replan = fs != self.plan.input_rate_hz;
        if replan {
            let plan = match DdcPlan::new(&self.spec, fs) {
                Ok(plan) => plan,
                Err(e) => {
                    self.tracker.mark_lost(samples.len() as u64);
                    return Err(e);
                }
            };
            let tracker = std::mem::replace(
                &mut self.tracker,
                StreamTracker::new(DEFAULT_CHANNEL_RESET_ON),
            );
            *self = Self::from_plan(self.spec.clone(), plan, tracker, self.out_index);
        }
        if begin.reset || begin.rate_changed || replan {
            self.restart(info.time.sample_index);
        }
        let first_source = self.next_source_index();
        let first_out = self.out_index;

        self.out.clear();
        let bound = samples.len() / self.plan.xlate_decimation + 2;
        if self.out.capacity() < bound {
            self.out.reserve(bound);
        }
        let out = &mut self.out;
        match &mut self.resample {
            None => self.xlate.push(samples, |y| out.push(y)),
            Some(r) => {
                let mut sink = |z| out.push(z);
                self.xlate.push(samples, |y| r.push_one(y, &mut sink));
            }
        }
        let produced = self.out.len();
        self.out_index += produced as u64;

        let (discontinuity, dropped_before) = if produced > 0 {
            self.tracker.take_pending()
        } else {
            (Discontinuity::NONE, 0)
        };
        let time = ChannelTime::new(
            first_out,
            first_source,
            self.plan.decimation(),
            self.tracker.anchor(),
            self.plan.input_rate_hz,
        );
        Ok(DdcBlock {
            header: ChannelHeader {
                time,
                sample_rate_hz: self.plan.output_rate_hz,
                provenance: self.tracker.provenance(),
                discontinuity,
                dropped_before,
            },
            samples: &self.out,
            center_offset_hz: self.plan.center_offset_hz,
        })
    }
}

/// The DDC's filter stages without stream tracking or provenance (ADR-0011 §1.6): mix, low-pass
/// and resample contiguous samples whose caller keeps its own time map (the decoder-workbench
/// blocks, whose chunks carry `ChunkMeta` instead of a `ReadChunk`). Same plan, stages and
/// chunking invariance as [`Ddc`]; the NCO phase is referenced to the first sample after
/// construction or [`DdcKernel::reset`].
pub struct DdcKernel {
    plan: DdcPlan,
    xlate: Xlating,
    resample: Option<Polyphase>,
}

impl DdcKernel {
    /// Plans and builds the stages for inputs at `input_rate_hz` (allocates, designs filters).
    pub fn new(spec: &DdcSpec, input_rate_hz: f64) -> Result<Self, DdcError> {
        let plan = DdcPlan::new(spec, input_rate_hz)?;
        let xlate = Xlating::new(
            &plan.xlate,
            plan.xlate_decimation,
            plan.center_offset_hz,
            plan.input_rate_hz,
        );
        let resample = plan.resample.as_ref().map(Polyphase::new);
        Ok(Self {
            plan,
            xlate,
            resample,
        })
    }

    /// The plan.
    pub fn plan(&self) -> &DdcPlan {
        &self.plan
    }

    /// Clears filter history; the next sample is input position 0 again.
    pub fn reset(&mut self) {
        self.xlate.restart(0);
        if let Some(r) = &mut self.resample {
            r.restart();
        }
    }

    /// Input position (samples since the last reset, fractional) that the next output
    /// represents, filter delay included.
    pub fn next_input_position(&self) -> f64 {
        match &self.resample {
            None => self.xlate.source_index_of(self.xlate.count() as f64),
            Some(r) => self.xlate.source_index_of(r.next_position()),
        }
    }

    /// Most outputs a call with `n` input samples can append.
    pub fn max_outputs(&self, n: usize) -> usize {
        n / self.plan.xlate_decimation + 2
    }

    /// Filter span in input samples (history needed before an output reflects an input).
    pub fn span_samples(&self) -> usize {
        self.plan.xlate.len()
            + self
                .plan
                .resample
                .as_ref()
                .map_or(0, |r| r.taps_per_phase * self.plan.xlate_decimation)
    }

    /// Filters `samples`, appending outputs to `out` (no allocation when `out` has
    /// [`DdcKernel::max_outputs`] spare capacity).
    pub fn process(&mut self, samples: &[Complex32], out: &mut Vec<Complex32>) {
        match &mut self.resample {
            None => self.xlate.push(samples, |y| out.push(y)),
            Some(r) => {
                let mut sink = |z| out.push(z);
                self.xlate.push(samples, |y| r.push_one(y, &mut sink));
            }
        }
    }
}
