//! `squelch`: C19's two squelches as a real → real block (ADR-0011 §8.4).
//!
//! - **`fm-noise`** (C19 "FM noise squelch", docs/04 §6.2): a discriminator's output is broadband
//!   noise without a carrier and quiets as a carrier captures it. The block high-passes its input
//!   above 0.65 × Nyquist (a Kaiser low-pass subtracted from the delayed input) and compares the
//!   total power with that out-of-band power extrapolated over the band (× 2.5). Noise alone reads
//!   about 0 dB (white) or −3 dB (the discriminator's parabolic noise); a captured carrier reads
//!   well above. The ratio is independent of gain, needs no noise estimate, and needs spectrum above
//!   the signal: feed it before de-emphasis at ≥ 2.5 × the audio bandwidth.
//! - **`snr`** (C19 "SNR squelch vs N̂0·B"): the smoothed level against `noise_dbfs`, the noise
//!   power at this port. Without it the squelch stays open — the Listen path's rule when its
//!   probe has no noise estimate (`hk_demod::audio`).
//!
//! Both open at `open_snr_db`, close `hysteresis_db` lower once the level has stayed below for
//! `hang_s`. No decision opens the squelch before one `attack_s` of data after a restart. The
//! estimates are one-pole power averages over `attack_s`, so on a channel that was noise the
//! `fm-noise` squelch opens about 4 × `attack_s` after a carrier appears (its noise estimate
//! must fall ~15 dB) and closes `hang_s` plus about one `attack_s` after it goes.
//!
//! **A closed squelch emits no items, not silence** (ADR-0011 §8.4): a long silence costs nothing
//! downstream and on the wire, and the gap shows as a jump in the output time map plus
//! `DISCONTINUITY` on the first chunk after re-opening, exactly how Listen marks a gap. A chunk
//! carries one contiguous run of items (its metadata has one time map), so if the squelch closes
//! and re-opens inside one chunk the second run starts at the next chunk; with the default
//! 0.5 s hang that needs a chunk longer than the hang.

use hk_demod::dsp::{FirDecimator, lowpass_taps};
use hk_recipe::{Params, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::iq::common::*;
use crate::buffer::ChunkFlags;
use crate::registry::BuildCtx;
use crate::status::{Lock, Status};

/// Lower edge of the out-of-band noise measurement, × Nyquist.
const NOISE_BAND_EDGE: f64 = 0.65;
/// The low-pass the noise band is subtracted from: passband edge, × Nyquist.
const NOISE_LP_PASS: f64 = 0.55;
/// Out-of-band share of the band the noise power is extrapolated from (the high-pass's −6 dB
/// point sits mid-transition, at 0.6 × Nyquist).
const NOISE_BAND_SHARE: f64 = 0.4;

pub(crate) fn build(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    Ok(Box::new(Squelch {
        cfg: Cfg::from(p),
        noise_lin: None,
        fs: 0.0,
        alpha: 1.0,
        hang_items: 0,
        warm_items: 0,
        hp: None,
        total: 0.0,
        noise: 0.0,
        n: 0,
        open: false,
        below: 0,
        gap: true,
        squelched_items: 0,
        non_finite: 0,
        status: Status::default(),
    }))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Cfg {
    fm_noise: bool,
    open_db: f64,
    hysteresis_db: f64,
    attack_s: f64,
    hang_s: f64,
    noise_dbfs: Option<f64>,
}

impl Cfg {
    fn from(p: &Params) -> Self {
        Self {
            fm_noise: str_or(p, "mode", "fm-noise") == "fm-noise",
            open_db: f64_or(p, "open_snr_db", 6.0),
            hysteresis_db: f64_or(p, "hysteresis_db", 3.0),
            attack_s: f64_or(p, "attack_s", 0.015),
            hang_s: f64_or(p, "hang_s", 0.5),
            noise_dbfs: get_f64(p, "noise_dbfs"),
        }
    }

    fn open_lin(&self) -> f64 {
        10f64.powf(self.open_db / 10.0)
    }

    fn close_lin(&self) -> f64 {
        10f64.powf((self.open_db - self.hysteresis_db) / 10.0)
    }
}

/// Input minus its Kaiser low-pass (delay-matched): the band above [`NOISE_BAND_EDGE`].
struct HighPass {
    lp: FirDecimator<f32>,
    delay: Vec<f32>,
    pos: usize,
}

impl HighPass {
    fn new(fs: f64) -> Result<Self, BlockError> {
        let nyq = 0.5 * fs;
        let taps = lowpass_taps(fs, NOISE_LP_PASS * nyq, NOISE_BAND_EDGE * nyq, 50.0)
            .map_err(|e| BlockError::Unrealisable(format!("squelch noise filter: {e}")))?;
        // The designer returns odd lengths, so the group delay is a whole number of samples.
        let d = (taps.len() - 1) / 2;
        Ok(Self {
            lp: FirDecimator::new(taps, 1),
            delay: vec![0.0; d + 1],
            pos: 0,
        })
    }

    fn clear(&mut self) {
        self.lp.clear();
        self.delay.fill(0.0);
        self.pos = 0;
    }

    #[inline]
    fn push(&mut self, x: f32) -> f32 {
        let lp = self.lp.push(x).unwrap_or(0.0);
        self.delay[self.pos] = x;
        self.pos = (self.pos + 1) % self.delay.len();
        // The oldest sample in the ring is `delay.len() − 1` = group delay behind `x`.
        self.delay[self.pos] - lp
    }
}

struct Squelch {
    cfg: Cfg,
    /// `snr` mode's noise reference, linear.
    noise_lin: Option<f64>,
    fs: f64,
    alpha: f64,
    hang_items: u64,
    warm_items: u64,
    /// Built at `init` whatever the mode, so switching mode by hot edit never allocates.
    hp: Option<HighPass>,
    /// Smoothed input power.
    total: f64,
    /// Smoothed out-of-band power (`fm-noise`).
    noise: f64,
    /// Items since the estimators restarted (fast start, warm-up).
    n: u64,
    open: bool,
    below: u64,
    /// The next emitted item does not follow the previous one.
    gap: bool,
    squelched_items: u64,
    non_finite: u64,
    status: Status,
}

impl Squelch {
    fn derive(&mut self) {
        self.alpha = ema_alpha(self.cfg.attack_s, self.fs);
        self.hang_items = (self.cfg.hang_s * self.fs).round() as u64;
        self.warm_items = (self.cfg.attack_s * self.fs).ceil() as u64;
        self.noise_lin = self.cfg.noise_dbfs.map(|n| 10f64.powf(n / 10.0).max(1e-30));
    }

    /// Drops the estimators' history; the open/closed state is kept, so a gap in an open
    /// channel does not chop the audio while the estimate re-converges.
    fn restart(&mut self) {
        if let Some(h) = &mut self.hp {
            h.clear();
        }
        self.total = 0.0;
        self.noise = 0.0;
        self.n = 0;
        self.below = 0;
    }

    /// The metric against the thresholds, linear; `None` when this mode cannot decide (`snr`
    /// without a noise reference).
    fn metric(&self) -> Option<f64> {
        if self.cfg.fm_noise {
            Some(self.total / (self.noise / NOISE_BAND_SHARE).max(1e-30))
        } else {
            self.noise_lin.map(|n| self.total / n)
        }
    }

    /// One sample through the estimators and the open/close rule.
    #[inline]
    fn step(&mut self, x: f32, open_lin: f64, close_lin: f64) {
        let w = self.alpha.max(1.0 / (self.n + 1) as f64);
        self.n = self.n.saturating_add(1);
        let xf = f64::from(x);
        self.total += w * (xf * xf - self.total);
        if self.cfg.fm_noise
            && let Some(h) = &mut self.hp
        {
            let y = f64::from(h.push(x));
            self.noise += w * (y * y - self.noise);
        }
        let Some(m) = self.metric() else {
            self.open = true;
            return;
        };
        if self.n < self.warm_items {
            return;
        }
        if m >= open_lin {
            self.open = true;
            self.below = 0;
        } else if self.open && m < close_lin {
            self.below += 1;
            if self.below > self.hang_items {
                self.open = false;
                self.below = 0;
            }
        } else {
            self.below = 0;
        }
    }
}

impl Block for Squelch {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "squelch", &[PortType::Real])?;
        self.fs = input.rate_hz;
        self.hp = Some(HighPass::new(self.fs)?);
        self.derive();
        self.restart();
        self.open = false;
        self.gap = true;
        Ok(vec![PortInfo {
            hold_items: 0,
            ..input
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = real_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.restart();
            self.gap = true;
        }
        let (open_lin, close_lin) = (self.cfg.open_lin(), self.cfg.close_lin());
        let out = io.output(0)?;
        let y = real_out(out)?;
        let mut first: Option<usize> = None;
        let mut run_over = false;
        let mut starts_after_gap = false;
        for (k, &s) in x.iter().enumerate() {
            let s = finite_or_zero(s, &mut self.non_finite);
            self.step(s, open_lin, close_lin);
            if self.open && !run_over {
                if first.is_none() {
                    first = Some(k);
                    starts_after_gap = std::mem::take(&mut self.gap);
                }
                y.push(s);
            } else {
                run_over |= first.is_some();
                self.gap = true;
                if !self.open {
                    self.squelched_items += 1;
                }
            }
        }
        let emitted = y.len();
        set_meta(
            out,
            &m,
            m.source_index_of(first.unwrap_or(0)),
            m.source_per_item,
        );
        if starts_after_gap {
            out.meta.flags |= ChunkFlags::DISCONTINUITY;
        }
        let metric_db = self
            .metric()
            .filter(|_| self.n > 0)
            .map(|v| (10.0 * v.max(1e-30).log10()) as f32);
        let st = &mut self.status;
        st.items_in += x.len() as u64;
        st.items_out += emitted as u64;
        st.lock = if self.open {
            Lock::Locked
        } else {
            Lock::Searching
        };
        st.snr_db = metric_db;
        st.extra.set("open", if self.open { 1.0 } else { 0.0 });
        st.extra
            .set("squelched_s", self.squelched_items as f64 / self.fs);
        st.extra
            .set("level_dbfs", 10.0 * self.total.max(1e-30).log10());
        report_non_finite(st, self.non_finite);
        Ok(())
    }

    fn reset(&mut self) {
        self.restart();
        self.gap = true;
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        // Every parameter is hot.
        self.cfg = Cfg::from(p);
        self.derive();
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}
