//! IQ and demodulation blocks (T-086). Wrap hk-dsp (`filter::design_lowpass`, `filter::Nco`,
//! the DDC's polyphase stages) and hk-demod (discriminator, pilot PLL, RDS subcarrier) per
//! ADR-0011 §1.6; don't fork their DSP.

use hk_recipe::PortType::{Iq, Real, Soft};
use hk_recipe::{BlockDescriptor, PortSpec};

use crate::Registry;
use crate::schema::{ParamExt, descriptor, float, int, object, one_of, param};

/// Pinned descriptors of this group.
pub fn planned() -> Vec<BlockDescriptor> {
    let io = |t_in, t_out| {
        (
            vec![PortSpec::new("in", t_in)],
            vec![PortSpec::new("out", t_out)],
        )
    };
    let unpinned = |name: &str, doc: &str, (i, o): (Vec<PortSpec>, Vec<PortSpec>)| {
        descriptor(name, "iq", doc, i, o, vec![], false)
    };
    let same = || {
        (
            vec![PortSpec::any_of("in", &[Iq, Real])],
            vec![PortSpec::any_of("out", &[Iq, Real])],
        )
    };
    let (fm_in, fm_out) = io(Iq, Real);
    let (sc_in, sc_out) = io(Real, Iq);
    vec![
        unpinned("mix", "Frequency shift by an offset (NCO).", io(Iq, Iq)),
        unpinned("lowpass", "Low-pass FIR (Kaiser design).", same()),
        unpinned("resample", "Rational/fractional resampler.", same()),
        descriptor(
            "fm_demod",
            "iq",
            "Polar discriminator: instantaneous frequency, scaled to ±1 at the deviation.",
            fm_in,
            fm_out,
            vec![
                param(
                    "deviation_hz",
                    float(1.0, 500_000.0, "Hz"),
                    "Peak deviation; absent: estimated (C13/C14).",
                )
                .hot(),
                param(
                    "output_rate_hz",
                    float(1.0, 20e6, "Hz"),
                    "Decimate to this rate; absent: input rate.",
                ),
                param(
                    "deemphasis_s",
                    float(0.0, 1e-3, "s"),
                    "De-emphasis time constant; absent: none.",
                ),
            ],
            true,
        ),
        unpinned("am_demod", "Envelope detector.", io(Iq, Real)),
        unpinned(
            "fsk_demod",
            "FSK discriminator (real, per-sample).",
            io(Iq, Real),
        ),
        unpinned(
            "msk_demod",
            "MSK demodulator (real, per-sample).",
            io(Iq, Real),
        ),
        unpinned(
            "ppm_demod",
            "Pulse-position chip-pair comparison to soft bits.",
            io(Iq, Soft),
        ),
        descriptor(
            "subcarrier",
            "iq",
            "Extracts a subcarrier from a real waveform to complex baseband, optionally phase-locked to a reference tone.",
            sc_in,
            sc_out,
            vec![
                param(
                    "carrier_hz",
                    float(1.0, 10e6, "Hz"),
                    "Subcarrier frequency.",
                )
                .required(),
                param(
                    "bandwidth_hz",
                    float(1.0, 10e6, "Hz"),
                    "Two-sided bandwidth kept.",
                )
                .required(),
                param(
                    "output_rate_hz",
                    float(1.0, 10e6, "Hz"),
                    "Baseband output rate.",
                )
                .required(),
                param(
                    "reference",
                    object(vec![
                        param("pilot_hz", float(1.0, 10e6, "Hz"), "Reference tone.").required(),
                        param("multiple", int(1, 8), "carrier_hz = multiple × pilot_hz.")
                            .required(),
                        param(
                            "pll_bandwidth_hz",
                            float(0.1, 1_000.0, "Hz"),
                            "Reference PLL loop bandwidth.",
                        )
                        .default_value(10.0),
                    ]),
                    "Derive the carrier phase from a pilot (RDS: 3 × 19 kHz); absent: free-running mix.",
                ),
                param(
                    "phase_tracking",
                    one_of(&["none", "bpsk", "qpsk"]),
                    "Residual carrier-phase tracking.",
                )
                .default_value("none")
                .hot(),
            ],
            true,
        ),
    ]
}

/// Registers this group's implemented blocks (none yet).
pub fn register(_r: &mut Registry) {}
