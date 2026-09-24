//! Audio blocks (ADR-0011 §8.4, T-866 = ADR-0015 LP-2): what a recipe needs to *be* a listenable
//! analog demodulator. They wrap the Listen path's squelch/AGC rules (`hk_demod::audio`, C19),
//! `hk_demod::dsp::Deemphasis` and `hk_stream::audio::encode_pcm`; they don't fork that DSP.
//!
//! | Block | Ports | What |
//! |---|---|---|
//! | `squelch` | real → real | emits **no items** while closed (not silence); the first chunk after it re-opens carries `DISCONTINUITY` |
//! | `agc` | real → real | peak-envelope AGC to a target, gain capped |
//! | `deemphasis` | real → real | single-pole de-emphasis, `tau_s` hot |
//! | `stereo_decode` | real → real (`left`), real (`right`) | FM multiplex → L/R: 19 kHz pilot PLL, 38 kHz L−R demodulation, matrix; **mono (L = R) whenever the pilot is absent or unlocked**, `stereo` status 1 only while locked (ADR-0015 §12.13, T-873) |
//! | `audio_out` | real → (sink) | resample to 48 kS/s, 960-sample `ri16_le` frames for the `audio` output kind |
//!
//! **Order in an FM chain:** `fm_demod` (no de-emphasis) → `squelch` (`fm-noise`) → `deemphasis`
//! → `agc` → `audio_out`. The FM noise squelch measures the discriminator's out-of-band noise,
//! which de-emphasis would already have removed. **Stereo:** `fm_demod` → `stereo_decode` (fed
//! the whole multiplex, ≥ 110 kS/s) → `deemphasis` on each of `left` and `right` (the standard
//! de-emphasises L and R, not the multiplex).

use hk_recipe::PortType::Real;
use hk_recipe::{BlockDescriptor, PortSpec};

use crate::Registry;
use crate::blocks::iq::common;
use crate::schema::{ParamExt, boolean, descriptor, float, int, one_of, param};

mod agc;
mod deemphasis;
mod out;
mod squelch;
mod stereo;
#[cfg(test)]
mod tests;

/// Pinned descriptors of this group.
pub fn planned() -> Vec<BlockDescriptor> {
    let io = || {
        (
            vec![PortSpec::new("in", Real)],
            vec![PortSpec::new("out", Real)],
        )
    };
    let (sq_in, sq_out) = io();
    let (agc_in, agc_out) = io();
    let (de_in, de_out) = io();
    vec![
        descriptor(
            "squelch",
            "audio",
            "Squelch: passes the audio while open and emits no items while closed (a gap, not silence); the first chunk after re-opening is flagged DISCONTINUITY. fm-noise: the discriminator's quieting — total power over the out-of-band noise above 0.65 × Nyquist extrapolated to the band (gain-independent; place it before de-emphasis, and feed it at ≥ 2.5 × the audio bandwidth). snr: level over noise_dbfs; without noise_dbfs it stays open.",
            sq_in,
            sq_out,
            vec![
                param("mode", one_of(&["fm-noise", "snr"]), "Measure.")
                    .default_value("fm-noise")
                    .hot(),
                param(
                    "open_snr_db",
                    float(-20.0, 60.0, "dB"),
                    "Opens at this SNR (fm-noise: quieting ratio).",
                )
                .default_value(6.0)
                .hot(),
                param(
                    "hysteresis_db",
                    float(0.0, 30.0, "dB"),
                    "Closes this much below the opening threshold.",
                )
                .default_value(3.0)
                .hot(),
                param(
                    "attack_s",
                    float(0.001, 1.0, "s"),
                    "Level-estimate time constant.",
                )
                .default_value(0.015)
                .hot(),
                param(
                    "hang_s",
                    float(0.0, 5.0, "s"),
                    "Stays open this long after the level falls below the closing threshold.",
                )
                .default_value(0.5)
                .hot(),
                param(
                    "noise_dbfs",
                    float(-200.0, 20.0, "dBFS"),
                    "snr mode: the noise power at this port (e.g. the probe's N0·B carried through the demodulator); absent: the snr squelch stays open.",
                )
                .hot(),
            ],
            true,
        ),
        descriptor(
            "agc",
            "audio",
            "Automatic gain: a peak-envelope follower (fast attack, slow decay, optional hang) sets the gain to reach target_dbfs, capped at max_gain_db; output clamped to ±1. The gain survives a DISCONTINUITY (a squelch gap), so a re-opened channel does not start at full gain; a RESET starts over.",
            agc_in,
            agc_out,
            vec![
                param("enabled", boolean(), "Apply the gain; off: pass-through.")
                    .default_value(true)
                    .hot(),
                param(
                    "target_dbfs",
                    float(-60.0, 0.0, "dBFS"),
                    "Peak-envelope target.",
                )
                .default_value(-6.0)
                .hot(),
                param("max_gain_db", float(0.0, 100.0, "dB"), "Largest gain.")
                    .default_value(60.0)
                    .hot(),
                param(
                    "attack_s",
                    float(1e-5, 1.0, "s"),
                    "Envelope rise time constant.",
                )
                .default_value(0.002)
                .hot(),
                param(
                    "decay_s",
                    float(1e-3, 30.0, "s"),
                    "Envelope fall time constant.",
                )
                .default_value(0.5)
                .hot(),
                param(
                    "hang_s",
                    float(0.0, 5.0, "s"),
                    "Holds the envelope this long after a peak before it decays (SSB syllables).",
                )
                .default_value(0.0)
                .hot(),
            ],
            true,
        ),
        descriptor(
            "deemphasis",
            "audio",
            "Single-pole de-emphasis H(s) = 1/(1 + sτ): 75 µs in the Americas and South Korea, 50 µs elsewhere; 0 is a pass-through.",
            de_in,
            de_out,
            vec![
                param(
                    "tau_s",
                    float(0.0, 1e-3, "s"),
                    "Time constant (regional; no default on purpose).",
                )
                .required()
                .hot(),
            ],
            true,
        ),
        descriptor(
            "stereo_decode",
            "audio",
            "FM stereo decoder: the multiplex (an fm_demod output without de-emphasis, at ≥ 110 kS/s) to left and right audio. A PLL locks to the 19 kHz pilot; while it is locked the 38 kHz L−R subcarrier (the pilot's second harmonic) is demodulated and matrixed with the mono sum, L = M + S, R = M − S. While the pilot is absent or the PLL unlocked the L−R channel is not decoded at all and left = right = the mono sum: an honest mono fallback, never mono labelled stereo. Status: lock, pilot_locked, stereo (1 only while L−R is decoded), lock_losses (every locked → unlocked transition, so a mid-stream loss is reported), pilot_hz, pilot_amplitude; quality = pilot phase coherence. Evidence: S1 pilot_lock. De-emphasise each output afterwards.",
            vec![PortSpec::new("in", Real)],
            vec![PortSpec::new("left", Real), PortSpec::new("right", Real)],
            vec![
                param(
                    "output_rate_hz",
                    float(40_000.0, 192_000.0, "Hz"),
                    "Output rate of both channels (at most the input rate).",
                )
                .default_value(48_000.0),
                param(
                    "audio_bandwidth_hz",
                    float(5_000.0, 15_000.0, "Hz"),
                    "Audio band kept in each channel (broadcast stereo carries 15 kHz).",
                )
                .default_value(15_000.0),
                param(
                    "pll_bandwidth_hz",
                    float(1.0, 100.0, "Hz"),
                    "Pilot PLL natural frequency.",
                )
                .default_value(10.0),
            ],
            true,
        ),
        descriptor(
            "audio_out",
            "audio",
            "Audio sink (no output port): anti-aliased resampling to 48 kS/s (15 kHz audio band, the 19 kHz pilot rejected), optional loudness normalisation, clamp to ±1, 960-sample ri16_le frames — the stream contract §12.2 audio profile, served by an `audio` output. Needs an input rate ≥ the output rate.",
            vec![PortSpec::new("in", Real)],
            vec![],
            vec![
                param(
                    "output_rate_hz",
                    float(48_000.0, 48_000.0, "Hz"),
                    "Output rate (the audio profile serves 48 kS/s).",
                )
                .default_value(48_000.0),
                param(
                    "datatype",
                    one_of(&["ri16_le"]),
                    "Sample encoding (the audio profile's).",
                )
                .default_value("ri16_le"),
                param(
                    "frame_samples",
                    int(960, 960),
                    "Samples per data record (20 ms).",
                )
                .default_value(960),
                param(
                    "loudness_target_dbfs",
                    float(-60.0, 0.0, "dBFS"),
                    "Slow loudness normalisation to this RMS (±30 dB, 3 s); absent: the level as demodulated.",
                )
                .hot(),
            ],
            true,
        ),
    ]
}

/// Registers this group's blocks.
pub fn register(r: &mut Registry) {
    let planned = planned();
    let mut add =
        |name: &str, build: common::BuildFn| common::register_pinned(r, &planned, name, build);
    add("squelch", squelch::build);
    add("agc", agc::build);
    add("deemphasis", deemphasis::build);
    add("stereo_decode", stereo::build);
    add("audio_out", out::build);
}
