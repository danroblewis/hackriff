//! Pre-trigger extraction: copy `[trigger − pre, trigger + post)` out of the ring by stream
//! sample index. The pre-trigger part is copied at once. The post-trigger part is filled as the
//! writer delivers it ([`PreTriggerCapture::poll`] / [`PreTriggerCapture::wait`]).
//!
//! The destination buffer is allocated once when the capture starts (the window size is known),
//! never per block. The result lists [`CaptureSegment`]s: one per contiguous run with unchanged
//! provenance. A gap, retune or gain change starts a new segment, so an extraction never
//! silently spans two states (C03) and maps directly onto SigMF captures (C25). Samples that
//! could not be delivered are counted, never interpolated.

use std::time::{Duration, Instant};

use hk_model::SampleTime;

use super::{ReadOutcome, ResyncPolicy, RingHandle, RingReader, RingSample};
use crate::block::{Discontinuity, ProvenanceHandle};

/// A trigger window by stream sample index: `[trigger − pre, trigger + post)`. The trigger
/// sample is the first post-trigger sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TriggerWindow {
    /// Stream index of the trigger.
    pub trigger_sample: u64,
    /// Samples before the trigger.
    pub pre_samples: u64,
    /// Samples from the trigger on (inclusive of the trigger sample).
    pub post_samples: u64,
}

impl TriggerWindow {
    /// First stream index of the window.
    pub fn start(&self) -> u64 {
        self.trigger_sample.saturating_sub(self.pre_samples)
    }

    /// One past the last stream index of the window.
    pub fn end(&self) -> u64 {
        self.trigger_sample.saturating_add(self.post_samples)
    }

    /// Window length in samples.
    pub fn len(&self) -> u64 {
        self.end() - self.start()
    }

    /// The window is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A contiguous run of captured samples under one provenance.
#[derive(Clone, Debug)]
pub struct CaptureSegment {
    /// Stream index of the first sample.
    pub first_sample: u64,
    /// Offset of the first sample in [`CapturedWindow::samples`].
    pub offset: usize,
    /// Samples in the run.
    pub len: usize,
    /// Time of the first sample.
    pub time: SampleTime,
    /// Provenance in force.
    pub provenance: ProvenanceHandle,
    /// Discontinuity flags at the start of the run (set if the run starts a block after a change).
    pub discontinuity: Discontinuity,
    /// Samples dropped by the source before the run's first block.
    pub dropped_before: u64,
}

/// Progress of a capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureStatus {
    /// Waiting for post-trigger samples; `next_sample` is the next index expected.
    Pending {
        /// Next stream index expected.
        next_sample: u64,
    },
    /// The window is done (fully captured, or truncated by a closed ring).
    Complete,
}

/// An in-progress pre-trigger capture. Create with [`RingHandle::pre_trigger`].
pub struct PreTriggerCapture<T: RingSample> {
    reader: RingReader<T>,
    window: TriggerWindow,
    samples: Vec<T>,
    filled: usize,
    segments: Vec<CaptureSegment>,
    missing_pre: u64,
    lost_samples: u64,
    truncated: bool,
    complete: bool,
}

impl<T: RingSample> PreTriggerCapture<T> {
    pub(crate) fn new(ring: &RingHandle<T>, window: TriggerWindow) -> Self {
        let len = usize::try_from(window.len()).expect("trigger window fits in memory");
        let mut capture = Self {
            reader: ring
                .reader_at(window.start())
                .with_resync_policy(ResyncPolicy::Oldest),
            window,
            samples: vec![T::default(); len],
            filled: 0,
            segments: Vec::with_capacity(4),
            missing_pre: 0,
            lost_samples: 0,
            truncated: false,
            complete: window.is_empty(),
        };
        capture.poll();
        capture
    }

    /// The window being captured.
    pub fn window(&self) -> TriggerWindow {
        self.window
    }

    /// Samples captured so far.
    pub fn filled(&self) -> usize {
        self.filled
    }

    /// Copies whatever has arrived since the last call, without waiting.
    pub fn poll(&mut self) -> CaptureStatus {
        let (start, end) = (self.window.start(), self.window.end());
        while !self.complete {
            if self.reader.position() >= end {
                self.complete = true;
                break;
            }
            match self
                .reader
                .read_until(&mut self.samples[self.filled..], end)
            {
                ReadOutcome::Data(chunk) => {
                    let offset = self.filled;
                    self.filled += chunk.len;
                    let merge = self.segments.last().is_some_and(|last| {
                        last.first_sample + last.len as u64 == chunk.first_sample()
                            && chunk.discontinuity.is_empty()
                            && chunk.dropped_before == 0
                            && last.provenance == chunk.provenance
                    });
                    if merge {
                        if let Some(last) = self.segments.last_mut() {
                            last.len += chunk.len;
                        }
                    } else {
                        self.segments.push(CaptureSegment {
                            first_sample: chunk.first_sample(),
                            offset,
                            len: chunk.len,
                            time: chunk.time,
                            provenance: chunk.provenance,
                            discontinuity: chunk.discontinuity,
                            dropped_before: chunk.dropped_before,
                        });
                    }
                }
                ReadOutcome::Overrun {
                    dropped_samples,
                    resume_at,
                } => {
                    let lost_from = (resume_at - dropped_samples).max(start);
                    let lost = resume_at.min(end).saturating_sub(lost_from);
                    if self.filled == 0 {
                        self.missing_pre += lost;
                    } else {
                        self.lost_samples += lost;
                    }
                }
                ReadOutcome::Empty => {
                    if self.reader.position() >= end {
                        self.complete = true;
                    } else {
                        return CaptureStatus::Pending {
                            next_sample: self.reader.position(),
                        };
                    }
                }
                ReadOutcome::Closed => {
                    self.truncated = true;
                    self.complete = true;
                }
            }
        }
        CaptureStatus::Complete
    }

    /// Polls until complete or `timeout` elapses.
    pub fn wait(&mut self, timeout: Duration) -> CaptureStatus {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.poll();
            if status == CaptureStatus::Complete || !self.reader.wait_for_data(deadline) {
                return status;
            }
        }
    }

    /// Ends the capture (complete or not) and returns what was captured.
    pub fn finish(mut self) -> CapturedWindow<T> {
        self.samples.truncate(self.filled);
        CapturedWindow {
            window: self.window,
            samples: self.samples,
            segments: self.segments,
            missing_pre: self.missing_pre,
            lost_samples: self.lost_samples,
            truncated: self.truncated || !self.complete,
        }
    }
}

/// The result of a pre-trigger capture.
#[derive(Clone, Debug)]
pub struct CapturedWindow<T> {
    /// The requested window.
    pub window: TriggerWindow,
    /// Captured samples in stream order; segment boundaries mark gaps and state changes.
    pub samples: Vec<T>,
    /// Contiguous same-provenance runs covering `samples`.
    pub segments: Vec<CaptureSegment>,
    /// Pre-trigger samples already overwritten when the capture started.
    pub missing_pre: u64,
    /// Samples lost to an overrun after capture began.
    pub lost_samples: u64,
    /// The ring closed (or the capture was finished) before the window end.
    pub truncated: bool,
}

impl<T> CapturedWindow<T> {
    /// Every sample of the window was captured, in one contiguous run with no state change.
    pub fn is_gapless(&self) -> bool {
        self.missing_pre == 0
            && self.lost_samples == 0
            && !self.truncated
            && self.samples.len() as u64 == self.window.len()
            && self
                .segments
                .iter()
                .skip(1)
                .all(|s| s.dropped_before == 0 && !s.discontinuity.contains(Discontinuity::GAP))
    }
}

#[cfg(test)]
mod tests {
    use hk_model::{ClockSource, Provenance, Timestamp, TimestampMethod, Tune};
    use num_complex::Complex32;

    use super::super::{RingConfig, RingWriter, ring_buffer};
    use super::*;
    use crate::block::BlockHeader;

    fn prov(center_hz: f64) -> ProvenanceHandle {
        ProvenanceHandle::new(Provenance {
            device_id: "synthetic:extract-test".into(),
            tune: Tune {
                center_hz,
                sample_rate_hz: 1000.0,
                lna_db: 0.0,
                vga_db: 0.0,
                amp_on: false,
                bandwidth_hz: 1000.0,
            },
            quantisation_limited: false,
            overload: false,
            temperature_c: None,
            antenna_port: None,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: Some(0),
        })
    }

    fn push(
        w: &mut RingWriter<Complex32>,
        first: u64,
        n: u64,
        p: &ProvenanceHandle,
        flags: Discontinuity,
    ) {
        let header = BlockHeader {
            time: SampleTime {
                sample_index: first,
                host_time: Timestamp::from_unix_nanos(first as i64 * 1_000_000),
            },
            provenance: p.clone(),
            discontinuity: flags,
            dropped_before: 0,
        };
        let samples: Vec<_> = (first..first + n)
            .map(|i| Complex32::new(i as f32, 0.0))
            .collect();
        w.push(&header, &samples).unwrap();
    }

    fn indices(w: &CapturedWindow<Complex32>) -> Vec<u64> {
        w.samples.iter().map(|s| s.re as u64).collect()
    }

    #[test]
    fn exact_window_with_post_trigger_arriving_later() {
        let (mut w, ring) = ring_buffer::<Complex32>(RingConfig {
            sample_capacity: 1 << 14,
            block_capacity: 64,
        });
        let p = prov(1e6);
        for b in 0..6 {
            push(&mut w, b * 1000, 1000, &p, Discontinuity::NONE);
        }
        // Trigger at 5500: pre 1200 → start 4300; post 800 → end 6300 (not yet written).
        let window = TriggerWindow {
            trigger_sample: 5500,
            pre_samples: 1200,
            post_samples: 800,
        };
        let mut capture = ring.pre_trigger(window);
        assert_eq!(capture.poll(), CaptureStatus::Pending { next_sample: 6000 });
        assert_eq!(capture.filled(), 1700);
        push(&mut w, 6000, 1000, &p, Discontinuity::NONE);
        assert_eq!(capture.poll(), CaptureStatus::Complete);
        let got = capture.finish();
        assert!(got.is_gapless());
        assert_eq!(indices(&got), (4300..6300).collect::<Vec<_>>());
        assert_eq!(got.segments.len(), 1);
        assert_eq!(got.segments[0].first_sample, 4300);
        assert_eq!(
            got.segments[0].time.host_time.as_unix_nanos(),
            4300 * 1_000_000
        );
    }

    #[test]
    fn missing_pre_trigger_history_is_counted() {
        let (mut w, ring) = ring_buffer::<Complex32>(RingConfig {
            sample_capacity: 1024,
            block_capacity: 64,
        });
        let p = prov(1e6);
        for b in 0..4 {
            push(&mut w, b * 500, 500, &p, Discontinuity::NONE);
        }
        // Retained: [976, 2000). Window [500, 1500).
        let got = ring
            .pre_trigger(TriggerWindow {
                trigger_sample: 1000,
                pre_samples: 500,
                post_samples: 500,
            })
            .finish();
        assert_eq!(got.missing_pre, 476);
        assert_eq!(indices(&got), (976..1500).collect::<Vec<_>>());
        assert!(!got.is_gapless());
    }

    #[test]
    fn gaps_and_retunes_split_segments() {
        let (mut w, ring) = ring_buffer::<Complex32>(RingConfig {
            sample_capacity: 1 << 12,
            block_capacity: 64,
        });
        let (a, b) = (prov(1e6), prov(2e6));
        push(&mut w, 0, 100, &a, Discontinuity::STREAM_START);
        push(&mut w, 100, 100, &a, Discontinuity::NONE);
        push(&mut w, 250, 100, &a, Discontinuity::NONE); // gap of 50
        push(
            &mut w,
            350,
            100,
            &b,
            Discontinuity::RETUNE | Discontinuity::PROVENANCE_CHANGE,
        );
        let mut capture = ring.pre_trigger(TriggerWindow {
            trigger_sample: 200,
            pre_samples: 150,
            post_samples: 200,
        });
        assert_eq!(
            capture.wait(Duration::from_millis(10)),
            CaptureStatus::Complete
        );
        let got = capture.finish();
        let spans: Vec<_> = got
            .segments
            .iter()
            .map(|s| (s.first_sample, s.offset, s.len, s.dropped_before))
            .collect();
        assert_eq!(
            spans,
            vec![(50, 0, 150, 0), (250, 150, 100, 50), (350, 250, 50, 0)]
        );
        assert!(got.segments[1].discontinuity.contains(Discontinuity::GAP));
        assert!(
            got.segments[2]
                .discontinuity
                .contains(Discontinuity::RETUNE)
        );
        assert_eq!(got.segments[2].provenance, b);
        assert!(!got.is_gapless());
    }

    #[test]
    fn closed_ring_truncates() {
        let (mut w, ring) = ring_buffer::<Complex32>(RingConfig {
            sample_capacity: 1024,
            block_capacity: 16,
        });
        let p = prov(1e6);
        push(&mut w, 0, 100, &p, Discontinuity::NONE);
        let mut capture = ring.pre_trigger(TriggerWindow {
            trigger_sample: 50,
            pre_samples: 50,
            post_samples: 500,
        });
        drop(w);
        assert_eq!(capture.poll(), CaptureStatus::Complete);
        let got = capture.finish();
        assert!(got.truncated);
        assert_eq!(got.samples.len(), 100);
    }
}
