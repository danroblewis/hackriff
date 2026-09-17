//! T-373: a periodic artefact of the **capture chain** must not be read as the emission's own
//! cyclic structure, and excluding it must not eat a genuine emission that happens to sit on it.
//!
//! T-317 found an 8192-sample gain step in `fixtures/hackrf/capture-2026-09-15-fm-band`: samples
//! 0–895 of every period run 0.431 dB low, stream-wide, in bands holding nothing at all. It puts a
//! comb at `fs/8192` = 292.969 Hz into the amplitude of every channel and reads as a 3.41 ms TDMA
//! frame that is not there — an expert analyst followed it into a wrong answer before a control on
//! the receiver's own CW lines exposed it.
//!
//! Three properties, in the order they matter:
//!
//! 1. **Teeth.** With the exclusion off (`artefact_guard_bins = 0`), C14 reports the comb as this
//!    emission's structure. A regression test that cannot fail when the fix is removed proves
//!    nothing.
//! 2. **The exclusion works, and is derived.** With it on, no reported line is a comb member, and
//!    the excluded frequency comes from the capture's own recorded period against its own sample
//!    rate — so the same artefact excludes 292.969 Hz at 2.4 Msps and 1220.703 Hz at 10 Msps.
//!    Nothing here carries a frequency.
//! 3. **A genuine emission on a comb member is still found**, once it is far enough away to be a
//!    different line at all. The loss window is ±[`ARTEFACT_GUARD_BINS`] periodogram bins — the
//!    record's own resolution — and inside it C14 *abstains visibly*
//!    ([`BlindReason::CaptureArtefact`]) rather than attributing the line to either party.

mod blind_support;
mod common;

use std::path::Path;

use hk_core::ProvenanceHandle;
use hk_dsp::synth::Rng;
use hk_estimate::blind::BlindReason;
use hk_estimate::{BlindConfig, BlindEstimator, Hints, SnippetRequest};
use hk_model::{CaptureArtefact, Provenance};
use num_complex::Complex;

use blind_support::{Chain, embed, gen_fsk};

/// The shipped guard, in native periodogram bins (`BlindConfig::artefact_guard_bins`).
const ARTEFACT_GUARD_BINS: f64 = 4.0;

/// The fixture T-317 measured the gain step in. Its truth annotations are not opened here: this
/// test is about the receiver, not the emissions.
const FIXTURE: &str = "fixtures/hackrf/capture-2026-09-15-fm-band";

/// The narrowband emission T-317 was identifying when it found the artefact. Named as a
/// *frequency to analyse*, not as a truth lookup — any narrowband box in this capture shows the
/// comb, and this is the one whose measurements are on record.
const OSC_HZ: f64 = 100_465_339.0;

fn fixture() -> Option<(ProvenanceHandle, Vec<Complex<i8>>)> {
    let root = hk_e2e::paths::repo_root();
    let meta = root.join(FIXTURE).join("iq.sigmf-meta");
    let data = root.join(FIXTURE).join("iq.sigmf-data");
    if !meta.is_file() || !std::fs::metadata(&data).is_ok_and(|m| m.len() > 4096) {
        if std::env::var(common::REQUIRE_FIXTURES_ENV).is_ok_and(|v| v == "1") {
            panic!("{FIXTURE} data not found (Git LFS not fetched?)");
        }
        eprintln!("SKIP {}: {FIXTURE} data not found", module_path!());
        return None;
    }
    let prov = common::meta_provenance(&meta);
    // 2 s is plenty: the comb resolves at 0.5 Hz and the emission is continuous.
    let want = 2 * (prov.get().tune.sample_rate_hz as usize);
    let iq: Vec<Complex<i8>> = std::fs::read(Path::new(&data))
        .expect("read .sigmf-data")
        .chunks_exact(2)
        .take(want)
        .map(|c| Complex::new(c[0] as i8, c[1] as i8))
        .collect();
    Some((prov, iq))
}

/// Harmonic number of `f` in the comb, and its residual in native bins.
fn comb_offset(f: f64, comb_hz: f64, bin_hz: f64) -> (f64, f64) {
    let n = (f / comb_hz).round();
    (n, (f - n * comb_hz) / bin_hz)
}

/// T-373 property 1 and 2, on the real capture: the shipped cyclic-line search reads the capture
/// chain's own comb as this emission's structure when the exclusion is off, and reads none of it
/// when the exclusion is on.
#[test]
fn t373_the_capture_chains_comb_is_read_as_structure_until_it_is_excluded() {
    let Some((prov, iq)) = fixture() else { return };
    let p = prov.get();
    let fs = p.tune.sample_rate_hz;
    // Derived, never written down: the capture records an 8192-sample period, and this capture
    // runs at 2.4 Msps.
    let combs = p.cyclic_artefacts();
    assert_eq!(
        combs.len(),
        1,
        "the fixture must record exactly the gain step; got {combs:?}"
    );
    let comb_hz = combs[0];
    assert!(
        (comb_hz - fs / 8192.0).abs() < 1e-9,
        "the comb is fs/period, {comb_hz} Hz against {} Hz",
        fs / 8192.0
    );

    let request = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: OSC_HZ - p.tune.center_hz,
        bandwidth_hz: 28e3,
    };

    // --- Mutation check A: with the exclusion disabled the artefact wins. ---
    let mut off = Chain {
        blind: BlindEstimator::new(BlindConfig {
            artefact_guard_bins: 0.0,
            ..Default::default()
        }),
        ..Default::default()
    };
    let out = off.run(&iq, &prov, &request, &Hints::default());
    let s = out.sym.as_ref().expect("C14 ran");
    let bin_hz = s.sample_rate_hz / s.samples.max(1) as f64;
    assert!(
        s.excluded_cyclic_hz.is_empty(),
        "guard 0 excludes nothing: {:?}",
        s.excluded_cyclic_hz
    );
    let on_comb: Vec<_> = s
        .lines
        .iter()
        .filter_map(|l| l.freq_hz)
        .filter(|&f| {
            let (n, d) = comb_offset(f, comb_hz, bin_hz);
            n >= 1.0 && d.abs() <= 1.0
        })
        .collect();
    assert!(
        !on_comb.is_empty(),
        "the test has no teeth: without the exclusion no line landed on the comb. \
         Lines {:?}, comb {comb_hz} Hz, bin {bin_hz} Hz",
        s.lines.iter().map(|l| l.freq_hz).collect::<Vec<_>>()
    );
    // …and it reaches the *rate candidates*, which is what makes it an answer rather than a
    // number in a diagnostic field.
    let cand_on_comb: Vec<_> = s
        .candidates
        .iter()
        .filter(|c| {
            let (n, d) = comb_offset(c.rate_bd, comb_hz, bin_hz);
            n >= 1.0 && d.abs() <= 1.0
        })
        .collect();
    eprintln!(
        "[T-373] exclusion OFF: {} of 4 lines on the comb at up to {:.1} dB {on_comb:?}; \
         {} of {} rate candidates are comb harmonics {:?}",
        on_comb.len(),
        s.lines
            .iter()
            .filter(|l| l.freq_hz.is_some_and(|f| {
                let (n, d) = comb_offset(f, comb_hz, bin_hz);
                n >= 1.0 && d.abs() <= 1.0
            }))
            .map(|l| l.significance_db)
            .fold(f64::NEG_INFINITY, f64::max),
        cand_on_comb.len(),
        s.candidates.len(),
        cand_on_comb
            .iter()
            .map(|c| (c.rate_bd, c.methods))
            .collect::<Vec<_>>()
    );
    assert!(
        !cand_on_comb.is_empty(),
        "the test has no teeth: without the exclusion the comb is not even a rate candidate. \
         Candidates {:?}",
        s.candidates.iter().map(|c| c.rate_bd).collect::<Vec<_>>()
    );
    assert!(
        cand_on_comb.iter().any(|c| c.methods >= 2),
        "the comb should reach two independent methods, which is what makes it convincing: {:?}",
        cand_on_comb
            .iter()
            .map(|c| (c.rate_bd, c.methods))
            .collect::<Vec<_>>()
    );

    // --- With the exclusion on, nothing the search reports is a comb member. ---
    let mut on = Chain::default();
    let out = on.run(&iq, &prov, &request, &Hints::default());
    let s = out.sym.as_ref().expect("C14 ran");
    assert_eq!(
        s.excluded_cyclic_hz,
        vec![comb_hz],
        "the applied exclusion is the capture's own comb"
    );
    for l in &s.lines {
        let Some(f) = l.freq_hz else { continue };
        let (n, d) = comb_offset(f, comb_hz, bin_hz);
        assert!(
            n < 1.0 || d.abs() > ARTEFACT_GUARD_BINS,
            "{:?} still reports the capture artefact: {f} Hz = harmonic {n} ({d:+.2} bins)",
            l.method
        );
    }
    for c in &s.candidates {
        let (n, d) = comb_offset(c.rate_bd, comb_hz, bin_hz);
        assert!(
            n < 1.0 || d.abs() > ARTEFACT_GUARD_BINS,
            "a rate candidate is the capture artefact: {} Bd = harmonic {n} ({d:+.2} bins)",
            c.rate_bd
        );
    }
    // The suppression is reported, not silent: this is how a genuine emission sitting on a comb
    // member stays visible instead of vanishing.
    assert!(
        s.lines.iter().any(|l| l.artefact_suppressed),
        "the artefact outscored what was kept, so at least one line must say so: {:?}",
        s.lines
    );
    assert!(
        s.reasons.contains(&BlindReason::CaptureArtefact),
        "reasons must carry the suppression: {:?}",
        s.reasons
    );
    eprintln!(
        "[T-373] exclusion ON: excluded {:?} Hz, lines {:?}, {} suppressed, reasons {:?}",
        s.excluded_cyclic_hz,
        s.lines
            .iter()
            .map(|l| (
                l.method,
                l.freq_hz,
                (l.significance_db * 10.0).round() / 10.0
            ))
            .collect::<Vec<_>>(),
        s.lines.iter().filter(|l| l.artefact_suppressed).count(),
        s.reasons
    );
}

/// The gain step T-317 measured, applied to a stream: samples `0..low` of every `period` run
/// `step_db` low. Written against the recorded numbers, so the control carries the real artefact
/// and not an idealised one.
fn apply_gain_step(iq: &mut [Complex<i8>], period: usize, low: usize, step_db: f64) {
    let g = 10f64.powf(step_db / 20.0);
    for (i, s) in iq.iter_mut().enumerate() {
        if i % period < low {
            let r = (f64::from(s.re) * g).round().clamp(-127.0, 127.0) as i8;
            let m = (f64::from(s.im) * g).round().clamp(-127.0, 127.0) as i8;
            *s = Complex::new(r, m);
        }
    }
}

/// A synthetic capture at `fs` carrying the recorded gain step, with provenance that records it.
fn artefact_provenance(fs: f64, period_samples: f64) -> ProvenanceHandle {
    let mut p: Provenance = blind_support::provenance(fs).get().clone();
    p.capture_artefacts = vec![CaptureArtefact {
        kind: "periodic-gain-step".into(),
        period_samples: Some(period_samples),
        period_s: None,
        step_db: Some(-0.431),
        measured_by: Some("T-317, applied to a synthetic control by T-373".into()),
        note: None,
    }];
    ProvenanceHandle::new(p)
}

/// T-373's control, and the one that matters: **a genuine emission whose symbol rate lands on a
/// comb harmonic must still be found.** An exclusion that silently eats real structure turns a
/// visible false positive into an invisible false negative.
///
/// The geometry is the fixture's own. 2.4 Msps with an 8192-sample period puts the comb at
/// 292.96875 Hz, and **150 000 Bd is exactly harmonic 512** — not a contrivance: 150 kBd is the
/// rate `blind_real::aware_036_blind_rate_and_deviation_915mhz` measures on the real 915 MHz
/// bursts, and any rate of the form `fs/2^k` lands exactly on this comb.
///
/// The test sweeps the emitter's clock offset and reports where the rate survives. What it asserts
/// is the honest boundary: outside ±`ARTEFACT_GUARD_BINS` native bins — the record's own frequency
/// resolution — the genuine line is a different line and is kept; inside it the two are not
/// resolvable by this transform at all, and C14 abstains with a reason rather than choosing.
#[test]
fn t373_a_genuine_rate_on_a_comb_harmonic_survives_once_it_is_resolvable() {
    // The fixture's capture geometry.
    let fs = 2_400_000.0;
    let period = 8192usize;
    let comb_hz = fs / period as f64;
    let harmonic = 512.0;
    let nominal = harmonic * comb_hz;
    assert!((nominal - 150_000.0).abs() < 1e-9, "{nominal}");

    let prov = artefact_provenance(fs, period as f64);
    let mut rng = Rng::new(0x5EED_0372);
    let nsym = 30_000;
    let dev = 50_000.0;

    let mut survived: Vec<(f64, f64)> = Vec::new();
    let mut suppressed: Vec<f64> = Vec::new();
    let mut bin_hz = f64::NAN;

    for offset_hz in [0.0, 5.0, 10.0, 20.0, 40.0, 80.0] {
        let rate = nominal + offset_hz;
        let sig = gen_fsk(&mut rng, rate, dev, nsym, fs, None, 16);
        let obw = 1.25 * rate;
        let mut e = embed(&mut rng, &sig, fs, 25.0, obw, 0.01, 0.0);
        apply_gain_step(&mut e.iq, period, 896, -0.431);

        let req = SnippetRequest {
            start_index: e.start as u64,
            end_index: (e.start + e.len) as u64,
            center_offset_hz: 0.0,
            bandwidth_hz: 1.6 * obw,
        };
        let mut chain = Chain::default();
        let out = chain.run(&e.iq, &prov, &req, &Hints::default());
        let s = out.sym.as_ref().expect("C14 ran");
        bin_hz = s.sample_rate_hz / s.samples.max(1) as f64;
        let bins = offset_hz / bin_hz;
        let got = out.trusted_rate();
        eprintln!(
            "[T-373] genuine {rate:.3} Bd (harmonic {harmonic} {offset_hz:+.0} Hz = {bins:+.1} \
             bins): trusted {got:?}, best {:?}, excluded {:?}, suppressed {}, reasons {:?}",
            out.best_rate(),
            s.excluded_cyclic_hz,
            s.lines.iter().filter(|l| l.artefact_suppressed).count(),
            s.reasons
        );
        assert_eq!(
            s.excluded_cyclic_hz,
            vec![comb_hz],
            "the control must actually be excluding the comb"
        );
        // Whatever happens, C14 must never report a *wrong* rate: the artefact's harmonic is not
        // this emission's rate, and the emission's own rate is the only right answer.
        if let Some(r) = got {
            // The harmonic alternatives are always ambiguous and always offered (S5 pitfall 6),
            // so ×½ and ×2 of the genuine rate are right answers; anything else is wrong.
            assert!(
                [0.5, 1.0, 2.0]
                    .iter()
                    .any(|m| (r / (m * rate) - 1.0).abs() < 0.01),
                "trusted and wrong: {r} Bd against a genuine {rate} Bd"
            );
            survived.push((offset_hz, r));
        } else {
            suppressed.push(offset_hz);
            assert!(
                s.reasons.contains(&BlindReason::CaptureArtefact)
                    || s.lines.iter().any(|l| l.artefact_suppressed),
                "an abstention inside the notch must say why: reasons {:?}",
                s.reasons
            );
            // Withholding trust is allowed; destroying the evidence is not. Even sitting exactly
            // on a comb member the emission's own rate must survive as a candidate, recovered by
            // the transition fit and the run-length seed, which the notch does not touch.
            let best = out.best_rate().expect("a candidate survives the notch");
            assert!(
                [0.5, 1.0, 2.0]
                    .iter()
                    .any(|m| (best / (m * rate) - 1.0).abs() < 0.01),
                "the exclusion destroyed the emission's evidence: best candidate {best} Bd \
                 against a genuine {rate} Bd"
            );
        }
    }

    eprintln!(
        "[T-373] control: native bin {bin_hz:.2} Hz, guard ±{:.2} Hz; found at offsets {:?} Hz, \
         withheld at {:?} Hz",
        ARTEFACT_GUARD_BINS * bin_hz,
        survived.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
        suppressed
    );
    let resolvable = ARTEFACT_GUARD_BINS * bin_hz;
    for offset_hz in &suppressed {
        assert!(
            *offset_hz <= resolvable,
            "the exclusion ate a genuine rate {offset_hz} Hz off the comb, which is outside the \
             ±{resolvable:.2} Hz the record can resolve — this is the false negative T-373 exists \
             to prevent"
        );
    }
    assert!(
        !survived.is_empty(),
        "no genuine rate near a comb harmonic survived at all"
    );
}

/// The excluded comb is a function of the capture, not a constant: the same recorded 8192-sample
/// period excludes 292.969 Hz in a 2.4 Msps capture and 1220.703 Hz in a 10 Msps one.
#[test]
fn t373_the_excluded_comb_is_derived_from_the_captures_own_rate() {
    let mut rng = Rng::new(0x5EED_0373);
    for (fs, want) in [(2_400_000.0, 292.968_75), (10_000_000.0, 1_220.703_125)] {
        let prov = artefact_provenance(fs, 8192.0);
        let rate = fs / 40.0;
        let sig = gen_fsk(&mut rng, rate, rate / 3.0, 4_000, fs, None, 16);
        let obw = 1.25 * rate;
        let e = embed(&mut rng, &sig, fs, 25.0, obw, 0.01, 0.0);
        let req = SnippetRequest {
            start_index: e.start as u64,
            end_index: (e.start + e.len) as u64,
            center_offset_hz: 0.0,
            bandwidth_hz: 1.6 * obw,
        };
        let mut chain = Chain::default();
        let out = chain.run(&e.iq, &prov, &req, &Hints::default());
        let s = out.sym.as_ref().expect("C14 ran");
        assert_eq!(
            s.excluded_cyclic_hz,
            vec![want],
            "at {fs} Hz the same recorded period must exclude {want} Hz"
        );
    }
}

/// A capture that records no artefact excludes nothing — an unrecorded artefact is never an
/// excluded one, and the default must not quietly notch every capture.
#[test]
fn t373_a_capture_with_no_recorded_artefact_excludes_nothing() {
    let fs = 300_000.0;
    let prov = blind_support::provenance(fs);
    assert!(prov.get().capture_artefacts.is_empty());
    let mut rng = Rng::new(0x5EED_0374);
    let rate = 7_500.0;
    let sig = gen_fsk(&mut rng, rate, 3_000.0, 4_000, fs, None, 16);
    let obw = 1.25 * rate;
    let e = embed(&mut rng, &sig, fs, 25.0, obw, 0.01, 0.0);
    let req = SnippetRequest {
        start_index: e.start as u64,
        end_index: (e.start + e.len) as u64,
        center_offset_hz: 0.0,
        bandwidth_hz: 1.6 * obw,
    };
    let mut chain = Chain::default();
    let out = chain.run(&e.iq, &prov, &req, &Hints::default());
    let s = out.sym.as_ref().expect("C14 ran");
    assert!(
        s.excluded_cyclic_hz.is_empty(),
        "{:?}",
        s.excluded_cyclic_hz
    );
    assert!(!s.lines.iter().any(|l| l.artefact_suppressed));
    assert!(!s.reasons.contains(&BlindReason::CaptureArtefact));
    let got = out
        .trusted_rate()
        .expect("a clean synthetic rate is trusted");
    assert!((got / rate - 1.0).abs() < 0.01, "{got} against {rate}");
}
