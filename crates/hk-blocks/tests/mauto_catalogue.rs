//! T-606 (ADR-0011 §9): the coverage-catalogue rows pin their PORT shapes before any of the
//! blocks exist, so the block tickets (T-608…T-613) code against a fixed contract and the
//! drift test (`implemented_blocks_match_their_pinned_descriptors`) holds each implementation
//! to it.
//!
//! The use-case chains below are the definition of done: each one type-checks end to end
//! against `catalogue::planned()`, which is exactly the claim that §9.2's decision (PSK
//! de-maps inside the block, one `soft` item per bit) leaves every downstream block untouched.
//! Placeholder blocks may warn about their unpinned params; nothing may error.

use hk_blocks::catalogue;
use hk_recipe::PortType::{self, Bits, Frames, Iq, Real, Soft};
use hk_recipe::{BlockDescriptor, Catalogue, Recipe, RecipeError, Resolved};
use serde_json::{Value, json};

/// An output port: name, accepted types, diagnostic.
type Out = (String, Vec<PortType>, bool);

fn port_types(d: &BlockDescriptor) -> (Vec<Vec<PortType>>, Vec<Out>) {
    let ins = d.inputs.iter().map(|p| p.types.clone()).collect();
    let outs = d
        .outputs
        .iter()
        .map(|p| (p.name.clone(), p.types.clone(), p.diagnostic))
        .collect();
    (ins, outs)
}

#[test]
fn every_t606_row_pins_the_port_shape_the_adr_states() {
    let cat = catalogue::planned();
    let main = |t: &[PortType]| vec![("out".to_owned(), t.to_vec(), false)];
    // (name, group, input types, outputs) — ADR-0011 §9.1's table, verbatim.
    let rows: Vec<(&str, &str, Vec<PortType>, Vec<Out>)> = vec![
        (
            "psk_demod",
            "iq",
            vec![Iq],
            vec![
                ("out".into(), vec![Soft], false),
                ("symbols".into(), vec![Iq], true),
                ("timing_error".into(), vec![Real], true),
            ],
        ),
        ("css_demod", "iq", vec![Iq], main(&[Soft])),
        ("ssb_demod", "iq", vec![Iq], main(&[Real])),
        ("cw_demod", "iq", vec![Iq], main(&[Real])),
        ("mlevel_slicer", "symbol", vec![Soft], main(&[Bits])),
        (
            "descramble",
            "symbol",
            vec![Bits, Frames],
            main(&[Bits, Frames]),
        ),
        (
            "bitstuff",
            "symbol",
            vec![Bits, Frames],
            main(&[Bits, Frames]),
        ),
        (
            "codeword_map",
            "symbol",
            vec![Bits, Frames],
            main(&[Bits, Frames]),
        ),
        ("despread", "symbol", vec![Soft, Bits], main(&[Bits])),
        ("equalise", "symbol", vec![Iq], main(&[Iq])),
        ("viterbi", "fec", vec![Soft, Bits], main(&[Bits])),
        ("viterbi_frames", "fec", vec![Frames], main(&[Frames])),
        ("reed_solomon", "fec", vec![Frames], main(&[Frames])),
    ];
    for (name, group, ins, outs) in rows {
        let d = cat
            .descriptor(name)
            .unwrap_or_else(|| panic!("{name}: not in planned()"));
        assert_eq!(d.group, group, "{name}: group");
        let (got_in, got_out) = port_types(d);
        assert_eq!(got_in, vec![ins], "{name}: inputs");
        assert_eq!(got_out, outs, "{name}: outputs");
    }
    // Reserved, deliberately not pinned: its output needs a port type that does not exist
    // (§9.2). A placeholder `iq → soft` here would be a shape the drift test then enforced.
    assert!(cat.descriptor("ofdm_demod").is_none());
}

#[test]
fn fec_is_a_group_with_exactly_two_shapes() {
    let cat = catalogue::planned();
    let mut per_frame = Vec::new();
    let mut streaming = Vec::new();
    for d in cat.iter().filter(|d| d.group == "fec") {
        let (ins, outs) = port_types(d);
        if ins == vec![vec![Frames]] && outs == vec![("out".into(), vec![Frames], false)] {
            per_frame.push(d.name.as_str());
        } else if ins == vec![vec![Soft, Bits]] && outs == vec![("out".into(), vec![Bits], false)] {
            streaming.push(d.name.as_str());
        } else {
            panic!("{}: a third fec shape — ADR-0011 §9.3 must name it", d.name);
        }
    }
    per_frame.sort_unstable();
    assert_eq!(
        per_frame,
        [
            "bch",
            "checksum",
            "crc",
            "parity",
            "reed_solomon",
            "viterbi_frames"
        ]
    );
    assert_eq!(streaming, ["viterbi"]);
}

fn recipe(id: &str, nodes: Value, outputs: Value) -> Recipe {
    serde_json::from_value(json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": id, "version": 1, "name": id,
        "input": { "port": "iq", "sample_rate_hz": 96000, "bandwidth_hz": 48000 },
        "nodes": nodes,
        "outputs": outputs,
        "output_policy": { "content_class": "unrestricted" }
    }))
    .unwrap_or_else(|e| panic!("{id}: parses: {e}"))
}

fn validate(r: &Recipe) -> Resolved {
    let cat = catalogue::planned();
    let resolved = r
        .validate(&cat)
        .unwrap_or_else(|e| panic!("{}: invalid: {e:#?}", r.id));
    for w in &resolved.warnings {
        let i: usize = w
            .path
            .strip_prefix("nodes[")
            .and_then(|p| p.split(']').next())
            .and_then(|i| i.parse().ok())
            .unwrap_or_else(|| panic!("{}: unexpected warning {w:?}", r.id));
        let d = cat.descriptor(&r.nodes[i].block).unwrap();
        assert!(!d.params_pinned, "{}: warning on pinned {}", r.id, d.name);
    }
    resolved
}

fn into(resolved: &Resolved, node: &str) -> PortType {
    resolved.edges.iter().find(|e| e.node == node).unwrap().ty
}

fn sync(word: &str, bits: u32, frame_bits: u32) -> Value {
    json!({ "mode": "sync-word", "sync_word": word, "sync_bits": bits, "max_errors": 2,
            "frame_bits": frame_bits, "include_sync": false })
}

fn frames_out(from: &str) -> Value {
    json!([{ "id": "frames", "kind": "inspector", "from": from }])
}

/// SIGNAL-034 (CCSDS cubesat telemetry): the seven-stage ladder docs/18 §7 finding 3 prices
/// as one investment. PSK de-maps to soft bits, Viterbi consumes them soft, and the ASM sync,
/// de-randomiser and dual-basis RS(255,223) run on frames — the CCSDS 131.0-B order.
#[test]
fn signal_034_ccsds_psk_viterbi_asm_derandomise_rs_type_checks() {
    let r = recipe(
        "ccsds",
        json!([
            { "id": "psk", "block": "psk_demod",
              "params": { "modulation": "bpsk", "symbol_rate_bd": 9600 } },
            { "id": "fec", "block": "viterbi",
              "params": { "constraint_length": 7, "polys": ["0x4F", "0x6D"],
                          "invert": [false, true] } },
            { "id": "asm", "block": "sync_search", "params": sync("0x1ACFFC1D", 32, 8160) },
            { "id": "pn", "block": "descramble",
              "params": { "mode": "additive", "poly": "0x1A9", "init": "0xFF", "offset_bits": 0 } },
            { "id": "rs", "block": "reed_solomon",
              "params": { "n": 255, "k": 223, "poly": "0x187", "fcr": 112, "prim": 11,
                          "dual_basis": true, "depth": 4 } }
        ]),
        json!([
            { "id": "frames", "kind": "inspector", "from": "rs" },
            { "id": "constellation", "kind": "stage", "from": "psk.symbols" }
        ]),
    );
    let res = validate(&r);
    assert_eq!(into(&res, "fec"), Soft, "Viterbi gets soft bits, not hard");
    assert_eq!(into(&res, "asm"), Bits);
    assert_eq!(
        into(&res, "pn"),
        Frames,
        "de-randomise per frame, after the ASM"
    );
    assert_eq!(into(&res, "rs"), Frames);
}

/// SIGNAL-004 (VDL Mode 2): D8PSK is differential and 3 bits per symbol, so the block
/// resolves both inside and emits three soft items per symbol; scrambling, RS and HDLC
/// destuffing are all per burst, i.e. on frames.
#[test]
fn signal_004_vdl2_d8psk_descramble_rs_destuff_type_checks() {
    let r = recipe(
        "vdl2",
        json!([
            { "id": "psk", "block": "psk_demod",
              "params": { "modulation": "d8psk", "symbol_rate_bd": 10500 } },
            { "id": "slice", "block": "slicer" },
            { "id": "sync", "block": "sync_search", "params": sync("0x0", 16, 8192) },
            { "id": "pn", "block": "descramble", "params": { "mode": "additive", "poly": "0x8003" } },
            { "id": "rs", "block": "reed_solomon",
              "params": { "n": 255, "k": 249, "poly": "0x187", "fcr": 120 } },
            { "id": "hdlc", "block": "bitstuff" },
            { "id": "fcs", "block": "crc", "params": { "width": 16, "poly": "0x1021" } }
        ]),
        frames_out("fcs"),
    );
    let res = validate(&r);
    assert_eq!(into(&res, "slice"), Soft);
    assert_eq!(into(&res, "hdlc"), Frames);
    assert_eq!(into(&res, "fcs"), Frames);
}

/// SIGNAL-080 (P25 Phase 1 control channel): C4FM through the existing discriminator and
/// clock, a 4-level decision, frame sync on the 48-bit FS, and the per-frame trellis — the
/// P25 1/2-rate trellis is table-defined, which is why `viterbi_frames` takes `trellis`.
#[test]
fn signal_080_p25_c4fm_mlevel_trellis_type_checks() {
    let r = recipe(
        "p25",
        json!([
            { "id": "fsk", "block": "fsk_demod" },
            { "id": "clock", "block": "clock_recovery", "params": { "symbol_rate_bd": 4800 } },
            { "id": "dibits", "block": "mlevel_slicer", "params": { "levels": 4 } },
            { "id": "fs", "block": "sync_search", "params": sync("0x5575F5FF77FF", 48, 1568) },
            { "id": "trellis", "block": "viterbi_frames",
              "params": { "termination": "terminated" } },
            { "id": "crc", "block": "crc", "params": { "width": 16, "poly": "0x1021" } }
        ]),
        frames_out("crc"),
    );
    let res = validate(&r);
    assert_eq!(into(&res, "dibits"), Soft);
    assert_eq!(into(&res, "fs"), Bits);
    assert_eq!(into(&res, "trellis"), Frames);
}

/// SIGNAL-053 (LoRa): the packet start is found in the chirp domain, so `css_demod` marks the
/// first item of each packet DISCONTINUITY (§1.1's existing flag) and `deframe` cuts the frame
/// from there — the §9.2 burst-boundary convention, with no new port type.
#[test]
fn signal_053_lora_css_deframe_dewhiten_type_checks() {
    let r = recipe(
        "lora",
        json!([
            { "id": "css", "block": "css_demod",
              "params": { "spreading_factor": 7, "bandwidth_hz": 125000 } },
            { "id": "slice", "block": "slicer" },
            { "id": "packet", "block": "deframe", "params": { "frame_bits": 2048 } },
            { "id": "whiten", "block": "descramble",
              "params": { "mode": "additive", "poly": "0x171", "init": "0xFF" } },
            { "id": "crc", "block": "crc", "params": { "width": 16, "poly": "0x1021" } }
        ]),
        frames_out("crc"),
    );
    let res = validate(&r);
    assert_eq!(into(&res, "slice"), Soft);
    assert_eq!(into(&res, "packet"), Bits);
    assert_eq!(into(&res, "whiten"), Frames);
}

/// SIGNAL-054 (Zigbee / 802.15.4): O-QPSK at the chip rate emits one soft item per chip, and
/// the correlator despreads soft chips (better than despreading hard ones) into 4-bit symbols.
#[test]
fn signal_054_zigbee_oqpsk_soft_despread_type_checks() {
    let r = recipe(
        "zigbee",
        json!([
            { "id": "psk", "block": "psk_demod",
              "params": { "modulation": "oqpsk", "symbol_rate_bd": 1000000, "pulse": "half-sine" } },
            { "id": "chips", "block": "despread",
              "params": { "chips_per_symbol": 32, "bit_order": "lsb" } },
            { "id": "sfd", "block": "sync_search", "params": sync("0xE5", 8, 1016) },
            { "id": "fcs", "block": "crc", "params": { "width": 16, "poly": "0x1021" } }
        ]),
        frames_out("fcs"),
    );
    let res = validate(&r);
    assert_eq!(into(&res, "chips"), Soft, "soft chips reach the correlator");
    assert_eq!(into(&res, "sfd"), Bits);
}

fn errors(r: &Recipe) -> Vec<RecipeError> {
    r.validate(&catalogue::planned())
        .err()
        .unwrap_or_else(|| panic!("{}: should not validate", r.id))
}

/// The shapes have teeth: RS straight after the streaming Viterbi (no frame sync between) and
/// a soft stream into the per-frame decoder are both refused, by type.
#[test]
fn mis_ordered_fec_chains_are_refused_by_type() {
    let rs_on_bits = recipe(
        "rs-on-bits",
        json!([
            { "id": "psk", "block": "psk_demod",
              "params": { "modulation": "bpsk", "symbol_rate_bd": 9600 } },
            { "id": "fec", "block": "viterbi" },
            { "id": "rs", "block": "reed_solomon" }
        ]),
        frames_out("rs"),
    );
    assert!(
        errors(&rs_on_bits)
            .iter()
            .any(|e| e.path.contains("nodes[2]")),
        "{:?}",
        errors(&rs_on_bits)
    );
    let soft_into_frames = recipe(
        "soft-into-frames",
        json!([
            { "id": "psk", "block": "psk_demod",
              "params": { "modulation": "bpsk", "symbol_rate_bd": 9600 } },
            { "id": "fec", "block": "viterbi_frames" }
        ]),
        frames_out("fec"),
    );
    assert!(
        errors(&soft_into_frames)
            .iter()
            .any(|e| e.path.contains("nodes[1]")),
        "{:?}",
        errors(&soft_into_frames)
    );
}

/// T-609: `psk_demod` is implemented, so its parameters are pinned and a recipe naming it is
/// checked against them (the other §9.1 rows still only warn).
#[test]
fn psk_demod_is_registered_with_pinned_parameters() {
    let registry = hk_blocks::Registry::builtin();
    let d = registry
        .get("psk_demod")
        .expect("psk_demod is registered")
        .descriptor()
        .clone();
    assert!(d.params_pinned, "psk_demod parameters are pinned");
    let cat = catalogue::planned();
    assert_eq!(cat.descriptor("psk_demod"), Some(&d));
    let bad = recipe(
        "psk-bad-params",
        json!([{ "id": "psk", "block": "psk_demod", "params": { "modulation": "16qam" } }]),
        json!([{ "id": "bits", "kind": "stage", "from": "psk" }]),
    );
    let errs = errors(&bad);
    assert!(
        errs.iter().any(|e| e.path.starts_with("nodes[0].params")),
        "{errs:?}"
    );
}

/// ADR-0011 §9.2: the `symbols` diagnostic port is a constellation tap, never a data path.
/// Tapping it as an output validates (SIGNAL-034's chain does); wiring it into a node is
/// refused, so a symbol-domain port type cannot arrive unreviewed.
#[test]
fn psk_symbols_tap_cannot_be_wired_into_a_node() {
    let r = recipe(
        "psk-symbols-into-node",
        json!([
            { "id": "psk", "block": "psk_demod",
              "params": { "modulation": "qpsk", "symbol_rate_bd": 9600 } },
            { "id": "eq", "block": "lowpass", "params": { "cutoff_hz": 4800 },
              "inputs": { "in": "psk.symbols" } }
        ]),
        json!([{ "id": "s", "kind": "stage", "from": "eq" }]),
    );
    let errs = errors(&r);
    assert!(
        errs.iter()
            .any(|e| e.path == "nodes[1].inputs.in" && e.message.contains("diagnostic")),
        "{errs:?}"
    );
}

/// T-610: both Viterbi shapes are implemented, so their parameters are pinned and a recipe
/// naming them is checked against the schema (an unknown `align`, numeric puncturing masks and
/// a per-frame decoder without its `termination` are all refused before anything is built).
#[test]
fn viterbi_shapes_are_registered_with_pinned_parameters() {
    let registry = hk_blocks::Registry::builtin();
    let cat = catalogue::planned();
    for name in ["viterbi", "viterbi_frames"] {
        let d = registry
            .get(name)
            .unwrap_or_else(|| panic!("{name} is registered"))
            .descriptor()
            .clone();
        assert!(d.params_pinned, "{name} parameters are pinned");
        assert_eq!(cat.descriptor(name), Some(&d));
    }
    let psk = json!({ "id": "psk", "block": "psk_demod",
                      "params": { "modulation": "bpsk", "symbol_rate_bd": 9600 } });
    for (id, node) in [
        (
            "align",
            json!({ "id": "fec", "block": "viterbi",
                    "params": { "constraint_length": 7, "polys": ["0x4F", "0x6D"],
                                "align": "sometimes" } }),
        ),
        (
            "puncture",
            json!({ "id": "fec", "block": "viterbi",
                    "params": { "constraint_length": 7, "polys": ["0x4F", "0x6D"],
                                "puncture": [5, 6] } }),
        ),
    ] {
        let r = recipe(
            id,
            json!([psk.clone(), node]),
            json!([{ "id": "bits", "kind": "stage", "from": "fec" }]),
        );
        let errs = errors(&r);
        assert!(
            errs.iter().any(|e| e.path.starts_with("nodes[1].params")),
            "{id}: {errs:?}"
        );
    }
    let no_termination = recipe(
        "frames-no-termination",
        json!([
            { "id": "fsk", "block": "fsk_demod" },
            { "id": "clock", "block": "clock_recovery", "params": { "symbol_rate_bd": 4800 } },
            { "id": "slice", "block": "slicer" },
            { "id": "fs", "block": "sync_search", "params": sync("0x5575F5FF77FF", 48, 1568) },
            { "id": "trellis", "block": "viterbi_frames",
              "params": { "constraint_length": 7, "polys": ["0x4F", "0x6D"] } }
        ]),
        frames_out("trellis"),
    );
    let errs = errors(&no_termination);
    assert!(
        errs.iter().any(|e| e.path.starts_with("nodes[4].params")),
        "{errs:?}"
    );
}
