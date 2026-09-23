//! ADR-0011 §9 (T-606) catalogue rows for group `iq`: `psk_demod`, `css_demod`, `ssb_demod`,
//! `cw_demod`. **Ports are pinned; parameters are placeholders** (`params_pinned: false`) that
//! each block's implementing ticket pins (the others are unfiled, docs/18 §9). `psk_demod`'s
//! parameters are pinned (T-609) and it is implemented in `psk.rs`; the rest are not. `ofdm_demod` is a reserved name with **no**
//! descriptor: its output needs a port type that does not exist yet (ADR-0011 §9.2).

use hk_recipe::PortType::{Iq, Real, Soft};
use hk_recipe::{BlockDescriptor, PortSpec};

use crate::schema::{ParamExt, boolean, descriptor, float, hex, int, one_of, param};

/// The group-`iq` rows ADR-0011 §9.1 adds.
pub fn planned() -> Vec<BlockDescriptor> {
    vec![
        descriptor(
            "psk_demod",
            "iq",
            "PSK demodulator: coarse carrier acquisition, RRC matched filter, symbol timing and \
             carrier recovery (liquid-dsp symtrack; native for OQPSK), de-mapped inside the \
             block to one soft item per BIT (k per symbol, label MSB first, positive = 1, \
             max-log LLR up to a common scale).",
            vec![PortSpec::new("in", Iq)],
            vec![
                PortSpec::new("out", Soft),
                PortSpec::new("symbols", Iq).diagnostic(),
                PortSpec::new("timing_error", Real).diagnostic(),
            ],
            vec![
                param(
                    "modulation",
                    one_of(&[
                        "bpsk",
                        "dbpsk",
                        "qpsk",
                        "oqpsk",
                        "dqpsk",
                        "pi4-dqpsk",
                        "8psk",
                        "d8psk",
                    ]),
                    "Constellation and differential coding (k = 1, 2 or 3 bits per symbol); \
                     differential modes emit data bits.",
                )
                .required(),
                param(
                    "symbol_rate_bd",
                    float(1.0, 10e6, "Bd"),
                    "Nominal symbol rate (OQPSK: I/Q pairs per second).",
                )
                .required(),
                param(
                    "pulse",
                    one_of(&["rrc", "rect", "half-sine"]),
                    "Matched-filter pulse; rect and half-sine (802.15.4) are OQPSK-only.",
                )
                .default_value("rrc"),
                param("rolloff", float(0.05, 1.0, ""), "RRC roll-off.").default_value(0.35),
                param(
                    "mapping",
                    one_of(&["gray", "natural"]),
                    "Symbol label of each constellation point (or phase change).",
                )
                .default_value("gray"),
                param(
                    "rotation_deg",
                    int(0, 315),
                    "Resolves the coherent M-fold phase ambiguity (a multiple of 360/M; OQPSK: \
                     90 inverts I, 180 both rails, 270 Q; differential modes: 0 only).",
                )
                .default_value(0)
                .hot(),
                param("iq_swap", boolean(), "Swap I and Q before de-mapping.")
                    .default_value(false)
                    .hot(),
                param(
                    "loop_bandwidth",
                    float(1e-4, 1.0, ""),
                    "Tracker loop bandwidth on liquid symtrack's scale (carrier and timing \
                     loops at 0.001x, AGC and equaliser at 0.02x); the OQPSK loops use the \
                     same carrier law.",
                )
                .default_value(0.2)
                .hot(),
                param(
                    "max_offset_hz",
                    float(0.0, 1e6, "Hz"),
                    "Largest carrier offset the coarse estimator removes; absent: \
                     symbol_rate/4; 0: off.",
                ),
            ],
            true,
        ),
        descriptor(
            "css_demod",
            "iq",
            "Chirp-spread-spectrum (LoRa) demodulator: dechirp + FFT, de-mapped inside the block \
             to one soft item per BIT (SF, or SF-2 in reduced-rate symbols).",
            vec![PortSpec::new("in", Iq)],
            vec![PortSpec::new("out", Soft)],
            vec![
                param("spreading_factor", int(5, 12), "SF: bits per chirp symbol.").required(),
                param(
                    "bandwidth_hz",
                    float(7.8e3, 1.6e6, "Hz"),
                    "Chirp bandwidth.",
                )
                .required(),
                param(
                    "ldro",
                    boolean(),
                    "Low-data-rate optimisation (SF-2 bits per symbol).",
                )
                .default_value(false),
                param("sync_word", hex(8), "Network sync word."),
                param(
                    "header",
                    one_of(&["explicit", "implicit"]),
                    "Explicit header (its first 8 symbols at SF-2).",
                )
                .default_value("explicit"),
            ],
            false,
        ),
        descriptor(
            "ssb_demod",
            "iq",
            "Single-sideband demodulator with an estimated (not hand-set) carrier.",
            vec![PortSpec::new("in", Iq)],
            vec![PortSpec::new("out", Real)],
            vec![
                param("sideband", one_of(&["usb", "lsb"]), "Which sideband.").required(),
                param(
                    "carrier",
                    one_of(&["estimate", "raster", "fixed"]),
                    "Suppressed-carrier estimate: measured, snapped to raster_hz, or as tuned.",
                )
                .default_value("estimate"),
                param(
                    "raster_hz",
                    float(0.0, 1e5, "Hz"),
                    "Channel raster for `raster`.",
                ),
                param(
                    "clarifier_hz",
                    float(-5e3, 5e3, "Hz"),
                    "Residual offset (refined from the output).",
                )
                .default_value(0.0)
                .hot(),
                param("bandwidth_hz", float(100.0, 1e4, "Hz"), "Audio bandwidth.")
                    .default_value(2_400.0),
                param("output_rate_hz", float(1e3, 96e3, "Hz"), "Output rate."),
            ],
            false,
        ),
        descriptor(
            "cw_demod",
            "iq",
            "CW (on-off keyed carrier): an audible tone, or its envelope for Morse decoding.",
            vec![PortSpec::new("in", Iq)],
            vec![PortSpec::new("out", Real)],
            vec![
                param(
                    "output",
                    one_of(&["tone", "envelope"]),
                    "Audible beat tone, or keying envelope.",
                )
                .default_value("tone"),
                param("tone_hz", float(100.0, 3e3, "Hz"), "Beat-tone pitch.")
                    .default_value(700.0)
                    .hot(),
                param("bandwidth_hz", float(10.0, 3e3, "Hz"), "Filter bandwidth.")
                    .default_value(500.0)
                    .hot(),
                param("output_rate_hz", float(1e2, 96e3, "Hz"), "Output rate."),
            ],
            false,
        ),
    ]
}
