//! Adversarial C13 cases (AWARE-036): clipped input, very short bursts, a box across the band
//! edge, a box overlapping the burst edge in time and frequency, and a box beyond the samples.

mod common;

use common::*;
use hk_dsp::synth::{Rng, quantize_ci8};
use hk_estimate::{FamilyHint, Hints, Method, Reason};

const AWARE_036: &str = "AWARE-036";
const FS: f64 = 1e6;
const RATE: f64 = 9_600.0;

fn fsk(seed: u64, bits: usize) -> Vec<num_complex::Complex32> {
    let mut rng = Rng::new(seed);
    cpfsk(&random_bits(&mut rng, bits), FS, RATE, 4_800.0)
}

fn fsk_hint() -> Hints {
    Hints {
        family: FamilyHint::Fsk { levels: 2 },
        ..Default::default()
    }
}

#[test]
fn clipped_burst_withholds_snr_but_keeps_frequency() {
    let sig = fsk(1, 400);
    let obw = obw99_reference(&sig, FS);
    // 58 dB in-band SNR on −40 dBFS noise puts the burst above full scale (amplitude ≈ 1.13:
    // I and Q hard-limit over much of each cycle).
    let scene = Scene::new(&sig, FS, 200_000.0, 58.0, obw, 8_000, 8_000, 3);
    let amp = scene.signal_power.sqrt();
    assert!(amp > 1.0, "scene must clip ({amp})");
    let (ci8, clipped) = quantize_ci8(&scene.iq);
    assert!(clipped > 0);
    let (snip, ps) = run(&ci8, FS, &scene.request(500.0, obw), &fsk_hint());
    eprintln!(
        "[{AWARE_036}] clipped: fraction {:.3}, flags {:?}, snr {:?}, obw {:?}, cfo {:?}",
        snip.clip_fraction(),
        ps.flags,
        ps.snr_box_db,
        ps.obw99_hz.value(),
        ps.cfo_hz.value()
    );
    assert!(
        ps.flags.clipped && ps.flags.clip_fraction > 1e-4,
        "[{AWARE_036}] clip flag"
    );
    assert_eq!(ps.snr_box_db.reason(), Some(Reason::Clipped));
    assert_eq!(ps.snr_extent_db.reason(), Some(Reason::Clipped));
    // Clipping costs the amplitudes (flag, no SNR); the frequency survives within 3 % of Rs
    // (hard limiting distorts the instantaneous frequency within each cycle).
    let rf = ps.rf_center_hz.value().expect("frequency still measured");
    assert!(
        (rf - CENTER_HZ - 200_000.0).abs() <= 0.03 * RATE,
        "[{AWARE_036}] clipped rf error {:.1} Hz",
        rf - CENTER_HZ - 200_000.0
    );
}

#[test]
fn very_short_bursts_abstain_or_widen_uncertainty() {
    let long = fsk(2, 400);
    let obw = obw99_reference(&long, FS);
    // 1.5 ms: ~115 snippet samples at ~77 kS/s → too few for a PSD.
    let tiny = &long[..1_500];
    let scene = Scene::new(tiny, FS, -150_000.0, 25.0, obw, 8_000, 8_000, 4);
    let (snip, ps) = run(&scene.iq, FS, &scene.request(0.0, obw), &fsk_hint());
    eprintln!(
        "[{AWARE_036}] tiny burst ({} box samples): {:?}",
        snip.box_range.len(),
        ps.obw99_hz
    );
    assert!(snip.box_range.len() < 128);
    for e in [ps.obw99_hz, ps.snr_box_db, ps.cfo_hz, ps.rf_center_hz] {
        assert_eq!(
            e.reason(),
            Some(Reason::TooShort),
            "[{AWARE_036}] tiny burst: {e:?}"
        );
    }

    // 6 ms (~460 snippet samples): measured, with a wider CFO uncertainty than the long burst.
    let short = &long[..6_000];
    let s_scene = Scene::new(short, FS, -150_000.0, 25.0, obw, 8_000, 8_000, 5);
    let (_, s_ps) = run(&s_scene.iq, FS, &s_scene.request(0.0, obw), &fsk_hint());
    let l_scene = Scene::new(&long, FS, -150_000.0, 25.0, obw, 8_000, 8_000, 5);
    let (_, l_ps) = run(&l_scene.iq, FS, &l_scene.request(0.0, obw), &fsk_hint());
    eprintln!(
        "[{AWARE_036}] short burst: obw {:?} cfo {:?}; long: obw {:?} cfo {:?}",
        s_ps.obw99_hz, s_ps.cfo_hz, l_ps.obw99_hz, l_ps.cfo_hz
    );
    let s_obw = s_ps.obw99_hz.value().expect("short OBW");
    assert!(
        (s_obw - obw).abs() / obw < 0.35,
        "[{AWARE_036}] short OBW {s_obw} vs {obw}"
    );
    let (s_sig, l_sig) = (s_ps.cfo_hz.sigma().unwrap(), l_ps.cfo_hz.sigma().unwrap());
    assert!(
        s_sig > l_sig,
        "[{AWARE_036}] σ short {s_sig} <= long {l_sig}"
    );
    let s_cfo = s_ps.cfo_hz.value().unwrap();
    assert!(
        s_cfo.abs() <= 3.0 * s_sig + 0.01 * RATE,
        "[{AWARE_036}] short CFO {s_cfo} ± {s_sig}"
    );
}

#[test]
fn box_across_nyquist_is_extracted_wrapped() {
    let sig = fsk(3, 400);
    let obw = obw99_reference(&sig, FS);
    // Emission centred 3 kHz below +fs/2: its top 7 kHz alias to −fs/2. A DC-centred detector
    // sees two boxes, one ending at +fs/2 and one starting at −fs/2.
    let offset = FS / 2.0 - 3_000.0;
    let scene = Scene::new(&sig, FS, offset, 25.0, obw, 8_000, 8_000, 6);
    let upper_bw = obw / 2.0 + 3_000.0;
    let lower_bw = obw / 2.0 - 3_000.0;
    for (name, center, bw) in [
        ("upper half box", FS / 2.0 - upper_bw / 2.0, upper_bw),
        ("lower alias box", -FS / 2.0 + lower_bw / 2.0, lower_bw),
    ] {
        let req = hk_estimate::SnippetRequest {
            start_index: scene.start as u64,
            end_index: (scene.start + scene.len) as u64,
            center_offset_hz: center,
            bandwidth_hz: bw,
        };
        let (snip, ps) = run(&scene.iq, FS, &req, &fsk_hint());
        let rf = ps.rf_center_hz.value().expect("rf centre");
        let truth = CENTER_HZ + offset;
        let err = (rf - truth + FS / 2.0).rem_euclid(FS) - FS / 2.0;
        eprintln!(
            "[{AWARE_036}] edge {name}: flags {:?}, obw {:?} (ref {obw:.0}), rf err {err:.1} Hz",
            snip.flags,
            ps.obw99_hz.value()
        );
        assert!(
            ps.flags.nyquist_wrapped && ps.flags.edge,
            "[{AWARE_036}] {name}: flags"
        );
        assert!(
            err.abs() <= 0.01 * RATE,
            "[{AWARE_036}] {name}: rf err {err}"
        );
        let o = ps.obw99_hz.value().expect("obw");
        assert!(
            (o - obw).abs() / obw <= 0.15,
            "[{AWARE_036}] {name}: OBW {o}"
        );
    }
}

#[test]
fn box_starting_mid_burst_rejects_the_dirty_pad() {
    let sig = fsk(4, 800);
    let obw = obw99_reference(&sig, FS);
    let scene = Scene::new(&sig, FS, 80_000.0, 20.0, obw, 8_000, 8_000, 7);
    let mid = scene.start + scene.len / 2;
    let req = hk_estimate::SnippetRequest {
        start_index: mid as u64,
        end_index: (scene.start + scene.len) as u64,
        center_offset_hz: 80_000.0,
        bandwidth_hz: obw,
    };
    let (_, ps) = run(&scene.iq, FS, &req, &fsk_hint());
    let n0 = ps.noise_density.value().unwrap();
    let ext = ps.extent.expect("extent");
    let snr_true = scene.snr_db_in(obw);
    eprintln!(
        "[{AWARE_036}] mid-burst box: pads rejected {}, N0 {:+.2} dB ({:?}), extent {:?}, \
         snr box {:?} ext {:?} (true {snr_true:.2})",
        ps.flags.pads_rejected,
        db(n0 / scene.n0()),
        ps.noise_density.method(),
        ext,
        ps.snr_box_db.value(),
        ps.snr_extent_db.value()
    );
    assert!(
        ps.flags.pads_rejected >= 1,
        "[{AWARE_036}] the pre-pad carries the burst"
    );
    assert_eq!(ps.noise_density.method(), Method::NoisePad);
    assert!(
        db(n0 / scene.n0()).abs() < 0.5,
        "[{AWARE_036}] N0 from the clean pad"
    );
    assert!(
        ext.truncated_start && !ext.truncated_end,
        "[{AWARE_036}] extent flags {ext:?}"
    );
    let e = ps.snr_extent_db.value().unwrap() - snr_true;
    assert!(
        (-1.5..=1.0).contains(&e),
        "[{AWARE_036}] extent SNR err {e:.2}"
    );

    // Frequency box offset by 40 % of its width: the guard still holds the emission.
    let shifted = scene.request(0.4 * obw, obw);
    let (_, ps) = run(&scene.iq, FS, &shifted, &fsk_hint());
    let rf = ps.rf_center_hz.value().unwrap();
    assert!(
        (rf - CENTER_HZ - 80_000.0).abs() <= 0.01 * RATE,
        "[{AWARE_036}] shifted box rf err {}",
        rf - CENTER_HZ - 80_000.0
    );
}

#[test]
fn box_beyond_the_samples_is_flagged() {
    let sig = fsk(5, 400);
    let obw = obw99_reference(&sig, FS);
    let scene = Scene::new(&sig, FS, 0.25e6, 25.0, obw, 300, 8_000, 8);
    // Box starts before the first sample; almost no pre-pad exists.
    let req = hk_estimate::SnippetRequest {
        start_index: 0,
        end_index: (scene.start + scene.len) as u64,
        center_offset_hz: 0.25e6,
        bandwidth_hz: obw,
    };
    let prov = provenance(CENTER_HZ, FS);
    let mut ex = hk_estimate::SnippetExtractor::new(Default::default());
    // Stream indices start at 1000: the request's start (0) is before the data.
    let snip = ex.extract(
        info(1000, &prov),
        &scene.iq,
        &hk_estimate::SnippetRequest {
            start_index: req.start_index,
            end_index: req.end_index + 1000,
            ..req
        },
    );
    let snip = snip.expect("partial overlap still extracts");
    let mut est = hk_estimate::ParamEstimator::new(Default::default());
    let ps = est.estimate(&snip, &fsk_hint());
    eprintln!(
        "[{AWARE_036}] box beyond samples: {:?} noise {:?}",
        snip.flags,
        ps.noise_density.method()
    );
    assert!(snip.flags.box_truncated && snip.flags.pre_pad_short);
    assert!(ps.flags.box_truncated);
    assert!(ps.rf_center_hz.is_measured(), "{:?}", ps.rf_center_hz);
    // Entirely outside: an error, not a guess.
    let err = ex.extract(
        info(1000, &prov),
        &scene.iq,
        &hk_estimate::SnippetRequest {
            start_index: 0,
            end_index: 500,
            ..req
        },
    );
    assert!(matches!(
        err,
        Err(hk_estimate::EstimateError::OutOfRange { .. })
    ));
}
