//! T-981: telling a **front-end event** from a signal, one spectrum frame at a time.
//!
//! An 8-bit front end driven past full scale (a strong burst keying up with the amp on, a start-up
//! transient) does not add a signal: it clips, and clipping spreads energy across the whole
//! window. On the canvas that is a stripe the width of the tuned span, one row tall, drawn exactly
//! like energy — and the detector, seeing a broadband step, used to store it as an (impulsive)
//! Detection. The ADC told us what happened: its samples sat on the rails.
//!
//! A frame is judged here from two measurements and nothing else:
//!
//! - **Clipping**: the clipped fraction of the frame's samples ([`ClipCount`], counted from the raw
//!   ci8 codes). Above [`FrontEndConfig::clip_fraction`] the frame is `clipped` — provenance about
//!   the front end, reported on every such frame whatever else is in it.
//! - **A whole-span step**: the frame's median PSD (in dB) against a reference kept from recent
//!   non-event frames under the same tuning and gains. The median is what a stripe moves and a
//!   narrow emission does not: a strong carrier that clips a little leaves the median where it was.
//!
//! A frame is a **front-end event** when it is clipped **and** its median stepped up by at least
//! [`FrontEndConfig::step_db`] — or, with no reference yet (the first frames after a start or a
//! retune: the start-up transient), when it is saturated outright
//! ([`FrontEndConfig::saturation_fraction`]). An event frame never updates the reference — but an
//! event is a *step*, a change: once events have run for [`FrontEndConfig::max_event_s`] without a
//! break, the level they hold is adopted as the reference and the frames after it are `clipped`
//! (still reported, still provenance) but no longer an event. So a front end that is saturated
//! from the start, or stays saturated, is one bounded event and then a clipped steady state, never
//! an event without end.
//!
//! Every input is a measurement of this stream; no frequency, band or known signal is consulted.

use hk_model::Tune;

use crate::clip::ClipCount;

/// Thresholds of the front-end judgement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrontEndConfig {
    /// Clipped fraction of a frame's samples above which the frame is `clipped`. The detector's
    /// own rule-8 fraction ([`crate::config::Rules::clip_fraction`]'s default), so a
    /// clipped frame here is a clipped frame there.
    pub clip_fraction: f64,
    /// Rise of the frame's median PSD over the reference, dB, that makes a clipped frame an event.
    pub step_db: f32,
    /// Clipped fraction that makes a frame an event with no reference to step from.
    pub saturation_fraction: f64,
    /// Weight of each non-event frame in the reference (an exponential average in dB).
    pub reference_alpha: f32,
    /// Longest unbroken run of event frames, s, before its level becomes the reference.
    pub max_event_s: f64,
}

impl Default for FrontEndConfig {
    fn default() -> Self {
        Self {
            clip_fraction: 1e-4,
            step_db: 6.0,
            saturation_fraction: 1e-2,
            reference_alpha: 0.1,
            max_event_s: 2.0,
        }
    }
}

/// One frame's judgement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameVerdict {
    /// The clipped samples of the frame's span.
    pub clip: ClipCount,
    /// Clipped above [`FrontEndConfig::clip_fraction`].
    pub clipped: bool,
    /// The frame's median PSD, dB (of the PSD's own unit).
    pub level_db: f32,
    /// `level_db` minus the reference, when there is one.
    pub step_db: Option<f32>,
    /// A front-end event (see the module docs).
    pub event: bool,
}

/// The per-stream state of the judgement: the reference level and the tuning it belongs to.
#[derive(Clone, Debug, Default)]
pub struct FrontEndMonitor {
    config: FrontEndConfig,
    tune: Option<Tune>,
    reference_db: Option<f32>,
    /// Seconds of the unbroken run of event frames so far.
    event_run_s: f64,
    scratch: Vec<f32>,
}

impl FrontEndMonitor {
    /// A monitor with `config`.
    pub fn new(config: FrontEndConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// The thresholds in force.
    pub fn config(&self) -> &FrontEndConfig {
        &self.config
    }

    /// Forgets the reference (a discontinuity: the next frame is not comparable with the last).
    pub fn reset(&mut self) {
        self.reference_db = None;
        self.event_run_s = 0.0;
    }

    /// Judges one frame: `psd` its linear PSD, `clip` the clipped samples of its span, `tune` the
    /// tuning and gains it was captured under (a change forgets the reference).
    pub fn observe(&mut self, psd: &[f32], clip: ClipCount, tune: &Tune) -> FrameVerdict {
        if self.tune.as_ref() != Some(tune) {
            self.tune = Some(tune.clone());
            self.reset();
        }
        let level_db = median_db(psd, &mut self.scratch);
        let fraction = clip.fraction();
        let clipped = fraction > self.config.clip_fraction;
        let step_db = self
            .reference_db
            .filter(|_| level_db.is_finite())
            .map(|r| level_db - r);
        let mut event = clipped
            && match step_db {
                Some(s) => s >= self.config.step_db,
                None => fraction >= self.config.saturation_fraction,
            };
        if event {
            let rate = tune.sample_rate_hz;
            if rate.is_finite() && rate > 0.0 {
                self.event_run_s += clip.samples as f64 / rate;
            }
            if self.event_run_s > self.config.max_event_s + 1e-9 && level_db.is_finite() {
                // Not a step any more: the steady state. Start from here.
                self.reference_db = Some(level_db);
                self.event_run_s = 0.0;
                event = false;
            }
        } else {
            self.event_run_s = 0.0;
        }
        if !event && level_db.is_finite() {
            let a = self.config.reference_alpha;
            self.reference_db = Some(match self.reference_db {
                Some(r) => r + a * (level_db - r),
                None => level_db,
            });
        }
        FrameVerdict {
            clip,
            clipped,
            level_db,
            step_db,
            event,
        }
    }
}

/// Median of `psd` in dB (`-inf` for an empty or all-zero frame).
fn median_db(psd: &[f32], scratch: &mut Vec<f32>) -> f32 {
    if psd.is_empty() {
        return f32::NEG_INFINITY;
    }
    scratch.clear();
    scratch.extend(psd.iter().map(|&p| if p.is_finite() { p } else { 0.0 }));
    let mid = scratch.len() / 2;
    let (_, m, _) = scratch.select_nth_unstable_by(mid, f32::total_cmp);
    10.0 * m.max(0.0).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tune(center_hz: f64) -> Tune {
        Tune {
            center_hz,
            sample_rate_hz: 2e6,
            lna_db: 24.0,
            vga_db: 20.0,
            amp_on: false,
            bandwidth_hz: 1.75e6,
        }
    }

    /// A flat floor at `floor` with one strong narrow line.
    fn frame(floor: f32) -> Vec<f32> {
        let mut v = vec![floor; 1024];
        v[600] = floor * 1e5;
        v
    }

    const CLEAN: ClipCount = ClipCount::new(0, 80_000);

    #[test]
    fn a_clipped_whole_span_step_is_an_event_and_a_clipped_carrier_is_not() {
        let mut m = FrontEndMonitor::default();
        let t = tune(915e6);
        for _ in 0..10 {
            let v = m.observe(&frame(1e-9), CLEAN, &t);
            assert!(!v.clipped && !v.event);
        }
        // A strong carrier clipping slightly: clipped (provenance), no step, not an event.
        let v = m.observe(&frame(1e-9), ClipCount::new(40, 80_000), &t);
        assert!(v.clipped && !v.event, "{v:?}");
        assert!(v.step_db.unwrap().abs() < 0.5);
        // The whole window lifts 30 dB while the ADC sits on the rails: an event.
        let v = m.observe(&frame(1e-6), ClipCount::new(30_000, 80_000), &t);
        assert!(v.clipped && v.event, "{v:?}");
        assert!(v.step_db.unwrap() > 25.0);
        // …and it did not become the reference: the next quiet frame is at the old level.
        let v = m.observe(&frame(1e-9), CLEAN, &t);
        assert!(!v.event && v.step_db.unwrap().abs() < 0.5, "{v:?}");
        // The same step with no clipping is energy, not the front end: never an event here.
        let v = m.observe(&frame(1e-6), CLEAN, &t);
        assert!(!v.clipped && !v.event, "{v:?}");
    }

    #[test]
    fn a_sustained_saturation_is_one_bounded_event_then_a_clipped_steady_state() {
        let mut m = FrontEndMonitor::default();
        let t = tune(915e6);
        // Rows of 80 000 samples at 2 Msps: 40 ms each, so 2 s is 50 rows.
        let hot = ClipCount::new(8_000, 80_000);
        let events = (0..200)
            .map(|_| m.observe(&frame(1e-6), hot, &t))
            .take_while(|v| v.event)
            .count();
        assert_eq!(
            events, 50,
            "one event of max_event_s, then the steady state"
        );
        for _ in 0..20 {
            let v = m.observe(&frame(1e-6), hot, &t);
            assert!(v.clipped && !v.event, "{v:?}");
        }
        // Back to quiet, then a fresh step: an event again.
        for _ in 0..40 {
            m.observe(&frame(1e-9), CLEAN, &t);
        }
        assert!(m.observe(&frame(1e-6), hot, &t).event);
    }

    #[test]
    fn with_no_reference_only_saturation_is_an_event() {
        let mut m = FrontEndMonitor::default();
        let t = tune(915e6);
        // A start-up transient: saturated before any reference exists.
        let v = m.observe(&frame(1e-6), ClipCount::new(8_000, 80_000), &t);
        assert!(v.event && v.step_db.is_none(), "{v:?}");
        // A retune forgets the reference; light clipping there is not enough on its own.
        m.observe(&frame(1e-9), CLEAN, &t);
        let v = m.observe(&frame(1e-6), ClipCount::new(40, 80_000), &tune(916e6));
        assert!(v.clipped && !v.event && v.step_db.is_none(), "{v:?}");
    }
}
