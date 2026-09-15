//! Symbol blocks (T-086): timing recovery, decisions and line decoding. The biphase
//! max-contrast timing ports hk-demod `rds::demod`; Gardner/Mueller–Müller are classic
//! interpolating loops over the same matched-filter statistic (ADR-0011 §1.6).

use hk_recipe::PortType::{Bits, Iq, Real, Soft};
use hk_recipe::{BlockDescriptor, PortSpec};

use crate::Registry;
use crate::blocks::iq::common;
use crate::schema::{ParamExt, boolean, descriptor, float, one_of, param};

mod clock;
mod line;

/// Pinned descriptors of this group.
pub fn planned() -> Vec<BlockDescriptor> {
    vec![
        descriptor(
            "clock_recovery",
            "symbol",
            "Symbol timing recovery to one soft value per symbol (positive = 1).",
            vec![PortSpec::any_of("in", &[Iq, Real])],
            vec![
                PortSpec::new("out", Soft),
                PortSpec::new("timing_error", Real).diagnostic(),
            ],
            vec![
                param(
                    "symbol_rate_bd",
                    float(0.01, 10e6, "Bd"),
                    "Nominal symbol rate (tracked within max_deviation_ppm).",
                )
                .required(),
                param(
                    "pulse",
                    one_of(&["nrz", "biphase", "rrc"]),
                    "Pulse shape of the matched filter.",
                )
                .default_value("nrz"),
                param(
                    "algorithm",
                    one_of(&["gardner", "mueller-muller", "max-contrast"]),
                    "Timing error detector.",
                )
                .default_value("gardner"),
                param(
                    "soft_from",
                    one_of(&["in-phase", "magnitude"]),
                    "For iq input: soft value from the I axis or the envelope.",
                )
                .default_value("in-phase"),
                param(
                    "loop_bandwidth",
                    float(1e-6, 0.25, ""),
                    "Normalised loop bandwidth.",
                )
                .default_value(0.01)
                .hot(),
                param(
                    "max_deviation_ppm",
                    float(0.0, 20_000.0, "ppm"),
                    "Largest rate error tracked.",
                )
                .default_value(500.0),
            ],
            true,
        ),
        descriptor(
            "slicer",
            "symbol",
            "Hard decision: bit = soft > threshold (xor invert).",
            vec![PortSpec::new("in", Soft)],
            vec![PortSpec::new("out", Bits)],
            vec![
                param("threshold", float(-1e9, 1e9, ""), "Decision threshold.")
                    .default_value(0.0)
                    .hot(),
                param("invert", boolean(), "Complement every bit.")
                    .default_value(false)
                    .hot(),
            ],
            true,
        ),
        descriptor(
            "diff_decode",
            "symbol",
            "Differential decoding: out[n] = in[n] xor in[n−1] (xnor: complemented).",
            vec![PortSpec::new("in", Bits)],
            vec![PortSpec::new("out", Bits)],
            vec![
                param("mode", one_of(&["xor", "xnor"]), "Combination.")
                    .default_value("xor")
                    .hot(),
            ],
            true,
        ),
        descriptor(
            "nrzi",
            "symbol",
            "NRZI decoding: a level change encodes one value, no change the other (the first bit after a reset is the reference and emits nothing).",
            vec![PortSpec::new("in", Bits)],
            vec![PortSpec::new("out", Bits)],
            vec![
                param(
                    "mode",
                    one_of(&["transition-is-0", "transition-is-1"]),
                    "Which value a transition encodes (HDLC/AX.25/USB: transition-is-0).",
                )
                .default_value("transition-is-0")
                .hot(),
            ],
            true,
        ),
        descriptor(
            "manchester",
            "symbol",
            "Manchester (chip-pair) decoding at half the chip rate, with automatic pair alignment from the violation rate.",
            vec![PortSpec::any_of("in", &[Soft, Bits])],
            vec![PortSpec::new("out", Bits)],
            vec![
                param(
                    "convention",
                    one_of(&["thomas", "ieee"]),
                    "thomas: 1 = high→low (G. E. Thomas); ieee: 1 = low→high (IEEE 802.3).",
                )
                .default_value("thomas")
                .hot(),
                param(
                    "align",
                    one_of(&["auto", "fixed"]),
                    "auto: re-pair chips when the other alignment has clearly fewer violations; fixed: pairs start at the first chip after a reset.",
                )
                .default_value("auto")
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
    add("clock_recovery", clock::build);
    add("slicer", line::build_slicer);
    add("diff_decode", line::build_diff);
    add("nrzi", line::build_nrzi);
    add("manchester", line::build_manchester);
}
