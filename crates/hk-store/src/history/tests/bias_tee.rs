//! T-332: an antenna-port bias-tee switch is a **device provenance step**, and the state rides on
//! the measurement all the way to the per-tile [`ProvenanceSummary`].
//!
//! The DC powers an external LNA, so the floor moves the instant it arrives. T-331 already refuses
//! to *judge* across the switch (the floor context is keyed on it, so every open episode closes
//! `Unknown` and the step can never confirm as a rise) and T-333 already refuses to *compare*
//! across it (baselines are keyed on it). Neither of them tells the operator **what happened**.
//! This does: the step is recorded, so the explain-the-device-first path can name the cause.
//!
//! Three-valued throughout (T-325): `Unknown` is its own state, never `Off`.

use std::fs;

use hk_model::BiasTee;

use super::*;

/// 100 frames at 10 Hz, `bias(k)` the bias-tee state of frame `k`, everything else held fixed so
/// the only thing that can make a step is the tee. Returns the sealed pyramid's summary over the
/// whole span, read back from disk (so it has round-tripped through the tile format).
fn run(tag: &str, bias: impl Fn(i64) -> BiasTee) -> (TempDir, ProvenanceSummary) {
    let dir = TempDir::new(tag);
    let mut p = Pyramid::open(&dir.0, cfg(vec![level(1, 10), level(2, 6)], 16)).unwrap();
    let psd = vec![lin(-100.0); 16];
    let gain = GainState {
        lna_db: 16.0,
        vga_db: 20.0,
        amp_on: false,
    };
    for k in 0..100 {
        let mut f = frame(T0 + k * S / 10, S / 10, 16_000.0, 1000.0, &psd);
        // Gain, calibration, port and mask are byte-identical either side of the switch: a
        // bias-only change is invisible to every other part of the front-end state (T-331).
        f.gain = Some(gain);
        f.bias_tee = bias(k);
        p.ingest(&f).unwrap();
    }
    p.seal_through(ts(T0 + 60 * S)).unwrap();
    drop(p);
    let p = Pyramid::open(&dir.0, cfg(vec![level(1, 10), level(2, 6)], 16)).unwrap();
    let h = query(
        &p,
        (16_000.0, 32_000.0),
        (T0, T0 + 60 * S),
        Resolution::Level(1),
    );
    let prov = h.provenance.clone();
    drop(p);
    (dir, prov)
}

/// The bias-tee steps of a summary as `(t, from, to)`.
fn bias_steps(p: &ProvenanceSummary) -> Vec<(Timestamp, BiasTee, BiasTee)> {
    p.steps
        .iter()
        .filter(|s| s.changed & ProvenanceStep::BIAS_TEE != 0)
        .map(|s| (s.t, s.from.bias_tee, s.to.bias_tee))
        .collect()
}

/// **Property.** Switching the bias tee mid-run makes one provenance step at the switch, carrying
/// the state either side, and the tile records that two bias-tee states contributed.
///
/// **Control.** A run whose bias tee never changes makes **no** bias-tee step at all — so the fix
/// cannot pass by emitting one always — and is not marked mixed.
#[test]
fn bias_tee_switch_is_a_provenance_step_and_an_unchanged_tee_is_not() {
    let switch_at = T0 + 50 * S / 10;
    let (_d, switched) = run("bias-switch", |k| {
        if k < 50 { BiasTee::Off } else { BiasTee::On }
    });
    assert_eq!(switched.frames, 100);
    assert_eq!(
        bias_steps(&switched),
        vec![(ts(switch_at), BiasTee::Off, BiasTee::On)],
        "one step, at the switch, naming both states: {:?}",
        switched.steps
    );
    // A bias-only change is not a gain change: nothing else in the front-end state moved.
    assert_eq!(switched.gain_changes, 0);
    assert!(
        switched
            .steps
            .iter()
            .all(|s| s.changed == ProvenanceStep::BIAS_TEE)
    );
    assert_eq!(
        switched.steps[0].change_names(),
        vec!["bias_tee"],
        "the step names the field it is about"
    );
    // The tile's own state: the first frame's, and a flag that it pooled two receive chains.
    assert_eq!(switched.bias_tee, BiasTee::Off);
    assert!(switched.bias_tee_mixed);

    for held in [BiasTee::Off, BiasTee::On, BiasTee::Unknown] {
        let (_d, steady) = run("bias-steady", |_| held);
        assert_eq!(steady.frames, 100);
        assert!(
            bias_steps(&steady).is_empty(),
            "no switch, no step ({held:?}): {:?}",
            steady.steps
        );
        assert_eq!(steady.bias_tee, held);
        assert!(!steady.bias_tee_mixed, "{held:?}");
    }
}

/// `Unknown` is not `Off` (T-325), one layer down as well: learning the state, and losing it, are
/// both steps, because the frames either side are not comparable. What such a step *means* is the
/// alarm path's business (`hk_context::occupancy::alarm::bias_tee_switch`): it is a change of
/// knowledge, so it never explains a level change away on its own.
#[test]
fn a_transition_to_or_from_unknown_is_its_own_step() {
    // unknown → on → unknown → off: three steps, not one and not zero.
    let (_d, p) = run("bias-unknown", |k| match k {
        k if k < 25 => BiasTee::Unknown,
        k if k < 50 => BiasTee::On,
        k if k < 75 => BiasTee::Unknown,
        _ => BiasTee::Off,
    });
    assert_eq!(
        bias_steps(&p),
        vec![
            (ts(T0 + 25 * S / 10), BiasTee::Unknown, BiasTee::On),
            (ts(T0 + 50 * S / 10), BiasTee::On, BiasTee::Unknown),
            (ts(T0 + 75 * S / 10), BiasTee::Unknown, BiasTee::Off),
        ],
        "{:?}",
        p.steps
    );
    assert_eq!(p.bias_tee, BiasTee::Unknown);
    assert!(p.bias_tee_mixed);

    // Equality on the state itself, not on `powered()`: unknown and off are different states even
    // though neither says the DC is on.
    let a = FrontEndState {
        bias_tee: BiasTee::Unknown,
        ..FrontEndState::default()
    };
    let b = FrontEndState {
        bias_tee: BiasTee::Off,
        ..FrontEndState::default()
    };
    assert_eq!(a.changes(&b), ProvenanceStep::BIAS_TEE);
    assert_eq!(a.changes(&a), 0);
}

/// Tiles written before T-332 carry no bias-tee byte, so they read back `Unknown` — which is what
/// they are. Reading them as `Off` would claim the DC was down, and the measurement comparable with
/// bias-tee-off captures, on no evidence at all (T-325's defect class).
#[test]
fn tiles_written_before_t332_read_as_unknown_never_as_off() {
    let (dir, fresh) = run("bias-old-format", |k| {
        if k < 50 { BiasTee::Off } else { BiasTee::On }
    });
    assert_eq!(fresh.bias_tee, BiasTee::Off);
    assert_eq!(bias_steps(&fresh).len(), 1);

    let p = Pyramid::open(&dir.0, cfg(vec![level(1, 10), level(2, 6)], 16)).unwrap();
    let hist = p.config().histogram;
    let geom = p.geometry().clone();
    for l in 0..=geom.top() {
        let g = geom.levels[l];
        for key in p.sealed_keys(l) {
            let path = p.tile_path(key);
            let tile = codec::decode(&path, &g, usize::from(hist.bins))
                .unwrap()
                .unwrap();
            let (mut buf, mut payload) = (Vec::new(), Vec::new());
            codec::encode_format(
                4,
                &tile,
                true,
                PowerUnit::Dbfs,
                &g,
                &hist,
                (10.0, 90.0),
                Some(3),
                &mut buf,
                &mut payload,
            );
            assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), 4);
            fs::write(&path, &buf).unwrap();
        }
    }
    drop(p);
    let old = Pyramid::open(&dir.0, cfg(vec![level(1, 10), level(2, 6)], 16)).unwrap();
    let h = query(
        &old,
        (16_000.0, 32_000.0),
        (T0, T0 + 60 * S),
        Resolution::Level(1),
    );
    let pr = &h.provenance;
    assert_eq!(pr.frames, 100);
    assert_eq!(pr.bias_tee, BiasTee::Unknown);
    assert!(!pr.bias_tee_mixed);
    // The step record survives (it is a format-2 field), but says nothing about the DC.
    assert_eq!(
        bias_steps(pr),
        vec![(ts(T0 + 50 * S / 10), BiasTee::Unknown, BiasTee::Unknown)],
        "{:?}",
        pr.steps
    );
}
