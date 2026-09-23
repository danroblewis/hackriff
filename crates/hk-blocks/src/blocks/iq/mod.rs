//! IQ and demodulation blocks (T-086). Wrap hk-dsp (`filter::design_lowpass`, `filter::Nco`,
//! the DDC's stages via `DdcKernel`) and hk-demod (discriminator, de-emphasis, FIR decimator,
//! pilot PLL) per ADR-0011 §1.6; don't fork their DSP.

use hk_recipe::PortType::{Frames, Iq, Real, Soft};
use hk_recipe::{BlockDescriptor, PortSpec};

use crate::Registry;
use crate::schema::{ParamExt, descriptor, float, frame_length, hex, int, object, one_of, param};

pub(crate) mod common;
mod demod;
mod filter;
pub mod mauto;
mod ppm;
mod subcarrier;
#[cfg(test)]
pub(crate) mod testkit;
#[cfg(test)]
mod tests;

/// Pinned descriptors of this group.
pub fn planned() -> Vec<BlockDescriptor> {
    let io = |t_in, t_out| {
        (
            vec![PortSpec::new("in", t_in)],
            vec![PortSpec::new("out", t_out)],
        )
    };
    let same = || {
        (
            vec![PortSpec::any_of("in", &[Iq, Real])],
            vec![PortSpec::any_of("out", &[Iq, Real])],
        )
    };
    let stopband = || {
        param(
            "stopband_db",
            float(20.0, 120.0, "dB"),
            "Stopband attenuation.",
        )
        .default_value(60.0)
    };
    let offset_params = || {
        vec![
            param(
                "offset_hz",
                float(-10e6, 10e6, "Hz"),
                "Known carrier offset removed from the discriminator output.",
            )
            .default_value(0.0)
            .hot(),
            param(
                "offset_tracking_s",
                float(0.0, 60.0, "s"),
                "Also remove the running mean of the discriminator (residual carrier offset) with this time constant; 0: off.",
            )
            .default_value(0.0)
            .hot(),
        ]
    };
    let (fm_in, fm_out) = io(Iq, Real);
    let (sc_in, sc_out) = io(Real, Iq);
    let (mix_in, mix_out) = io(Iq, Iq);
    let (lp_in, lp_out) = same();
    let (rs_in, rs_out) = same();
    let (am_in, am_out) = io(Iq, Real);
    let (fsk_in, fsk_out) = io(Iq, Real);
    let (msk_in, msk_out) = io(Iq, Real);
    vec![
        descriptor(
            "mix",
            "iq",
            "Frequency shift (exact u64 NCO): the component at offset_hz moves to 0 Hz.",
            mix_in,
            mix_out,
            vec![
                param(
                    "offset_hz",
                    float(-10e6, 10e6, "Hz"),
                    "Frequency moved to 0 Hz (output = input · e^(−j2π·offset·t)).",
                )
                .required()
                .hot(),
            ],
            true,
        ),
        descriptor(
            "lowpass",
            "iq",
            "Low-pass FIR (Kaiser design, unity DC gain), same rate; output delayed by the group delay in the time map.",
            lp_in,
            lp_out,
            vec![
                param("cutoff_hz", float(1.0, 10e6, "Hz"), "Passband edge.").required(),
                param(
                    "transition_hz",
                    float(1.0, 10e6, "Hz"),
                    "Transition width above the cutoff; absent: cutoff/4.",
                ),
                stopband(),
            ],
            true,
        ),
        descriptor(
            "resample",
            "iq",
            "Anti-aliased rate reduction (the DDC's integer/rational/fractional polyphase stages) to output_rate_hz ≤ input rate.",
            rs_in,
            rs_out,
            vec![
                param("output_rate_hz", float(1.0, 20e6, "Hz"), "Output rate.").required(),
                param(
                    "bandwidth_hz",
                    float(1.0, 20e6, "Hz"),
                    "Two-sided bandwidth kept flat; absent: 0.8 × output rate.",
                ),
                stopband(),
            ],
            true,
        ),
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
        descriptor(
            "am_demod",
            "iq",
            "Envelope detector. normalized: |x| / carrier level − 1 (the modulation, independent of gain and DC); envelope: |x|.",
            am_in,
            am_out,
            vec![
                param(
                    "mode",
                    one_of(&["normalized", "envelope"]),
                    "Output form.",
                )
                .default_value("normalized")
                .hot(),
                param(
                    "time_constant_s",
                    float(1e-4, 10.0, "s"),
                    "Carrier-level averaging time (normalized mode).",
                )
                .default_value(0.05)
                .hot(),
            ],
            true,
        ),
        descriptor(
            "fsk_demod",
            "iq",
            "FSK discriminator (real, per-sample): instantaneous frequency minus the carrier offset, ±1 at the deviation (higher frequency positive).",
            fsk_in,
            fsk_out,
            [
                vec![
                    param(
                        "deviation_hz",
                        float(1.0, 10e6, "Hz"),
                        "Peak deviation (output ±1); absent: estimated from the RMS.",
                    )
                    .hot(),
                ],
                offset_params(),
            ]
            .concat(),
            true,
        ),
        descriptor(
            "msk_demod",
            "iq",
            "MSK demodulator (real, per-sample): non-coherent discriminator, ±1 at the MSK deviation symbol_rate/4 (higher frequency positive).",
            msk_in,
            msk_out,
            [
                vec![
                    param(
                        "symbol_rate_bd",
                        float(1.0, 10e6, "Bd"),
                        "Symbol rate (deviation = rate/4); absent: deviation estimated from the RMS.",
                    )
                    .hot(),
                ],
                offset_params(),
            ]
            .concat(),
            true,
        ),
        descriptor(
            "ppm_demod",
            "iq",
            "Pulse-position demodulator: finds the chip preamble on the magnitude, decides each data bit from its chip pair (early > late = 1) and emits one frame of data bits per preamble, its length from length_from (ADS-B DF → 56/112) capped at frame_bits. Diagnostic soft: the chip-pair soft bits.",
            vec![PortSpec::new("in", Iq)],
            vec![
                PortSpec::new("out", Frames),
                PortSpec::new("soft", Soft).diagnostic(),
            ],
            [
                vec![
                    param(
                        "bit_rate_bd",
                        float(1.0, 10e6, "Bd"),
                        "Data bit rate (ADS-B 1e6).",
                    )
                    .required(),
                    param("chips_per_bit", int(2, 16), "Chips per data bit.").default_value(2),
                    param(
                        "preamble",
                        hex(64),
                        "Preamble chip pattern, first chip = MSB (ADS-B 0xA140).",
                    )
                    .required(),
                    param("preamble_chips", int(1, 64), "Preamble length, chips.").required(),
                    param(
                        "min_snr_db",
                        float(0.0, 60.0, "dB"),
                        "Preamble acceptance: on-chip over off-chip level.",
                    )
                    .default_value(6.0)
                    .hot(),
                    param(
                        "frame_bits",
                        int(1, 4_096),
                        "Most data bits per frame (with length_from: the maximum).",
                    )
                    .required(),
                ],
                frame_length()
                    .into_iter()
                    .filter(|p| p.name == "length_from")
                    .collect(),
            ]
            .concat(),
            true,
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

/// Registers this group's blocks.
pub fn register(r: &mut Registry) {
    let planned = planned();
    let mut add =
        |name: &str, build: common::BuildFn| common::register_pinned(r, &planned, name, build);
    add("mix", filter::build_mix);
    add("lowpass", filter::build_lowpass);
    add("resample", filter::build_resample);
    add("fm_demod", demod::build_fm);
    add("am_demod", demod::build_am);
    add("fsk_demod", demod::build_fsk);
    add("msk_demod", demod::build_msk);
    add("ppm_demod", ppm::build);
    add("subcarrier", subcarrier::build);
}
