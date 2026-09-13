//! AWARE-036 (unknown burst triage), T-011 C14 synthetic sweep: the S5 §3.3 family/rate sweep
//! and §3.5 synthetic negatives through the product chain (extraction → C13 → normalise → C14),
//! 8-bit quantised in real HackRF ADC noise (3.21 LSB rms).
//!
//! Cases (S5 `synth_sweep.py`): OOK 4 kbit/s NRZ (200 sym), 2-FSK 9.6 kBd h = 1 (400 sym),
//! GFSK 50 kBd h = 0.5 BT 0.5 (500 sym), BPSK and QPSK 25 kBd α = 0.35 (500 sym). SNR is
//! in-band over the clean OBW99; CFO uniform ±5 % of OBW.
//!
//! - `blind_sweep_subset` (always on, 60 runs at 6/15/20/30 dB): 0 trusted-and-wrong, 0 wrong
//!   labels, and at ≥ 20 dB every run right, trusted and labelled.
//! - `blind_constants_against_t010_obw`: the OBW-scaled S5 constants against T-010's OBW99.
//! - `blind_sweep_full` (`#[ignore]`, 900 runs, run with `--release --ignored`): 0 trusted and
//!   wrong, 0 wrong labels, and the S5 §3.3 floors (see `FLOORS`).
//! - `blind_negatives`: noise, CW, NBFM voice, LoRa-like chirp at 10/20/30 dB: never a trusted
//!   rate, never a label.

mod blind_support;

use blind_support::*;
use hk_dsp::synth::Rng;
use hk_estimate::Family;
use num_complex::Complex32;

const AWARE_036: &str = "AWARE-036";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Case {
    Ook4k,
    Fsk9k6H1,
    Gfsk50kH05,
    Bpsk25k,
    Qpsk25k,
}

const CASES: [Case; 5] = [
    Case::Ook4k,
    Case::Fsk9k6H1,
    Case::Gfsk50kH05,
    Case::Bpsk25k,
    Case::Qpsk25k,
];

impl Case {
    fn fs(self) -> f64 {
        if self == Case::Gfsk50kH05 { 1e6 } else { 250e3 }
    }

    fn family(self) -> Family {
        match self {
            Case::Ook4k => Family::Ook,
            Case::Fsk9k6H1 | Case::Gfsk50kH05 => Family::Fsk,
            Case::Bpsk25k => Family::Bpsk,
            Case::Qpsk25k => Family::Qpsk,
        }
    }

    /// (signal, rate, deviation)
    fn generate(self, rng: &mut Rng) -> (Vec<Complex32>, f64, Option<f64>) {
        let fs = self.fs();
        match self {
            Case::Ook4k => (gen_ook(rng, 4000.0, 200, fs), 4000.0, None),
            Case::Fsk9k6H1 => (
                gen_fsk(rng, 9600.0, 4800.0, 400, fs, None, 32),
                9600.0,
                Some(4800.0),
            ),
            Case::Gfsk50kH05 => (
                gen_fsk(rng, 50e3, 12.5e3, 500, fs, Some(0.5), 32),
                50e3,
                Some(12.5e3),
            ),
            Case::Bpsk25k => {
                let (s, r) = gen_bpsk_c(rng, 25e3, 500, fs);
                (s, r, None)
            }
            Case::Qpsk25k => {
                let (s, r) = gen_qpsk(rng, 25e3, 500, fs);
                (s, r, None)
            }
        }
    }

    fn clean_obw(self) -> f64 {
        let mut rng = Rng::new(1);
        let (s, _, _) = self.generate(&mut rng);
        clean_obw(&s, self.fs())
    }
}

#[derive(Debug, Default, Clone)]
struct Tally {
    n: usize,
    family_ok: usize,
    wrong_label: usize,
    rate_ok: usize,
    trusted: usize,
    trusted_wrong: usize,
    dev_n: usize,
    dev_ok: usize,
    cost_us: u64,
}

fn run_one(chain: &mut Chain, case: Case, snr: f64, obw: f64, seed: u64, t: &mut Tally) {
    let mut rng = Rng::new(seed);
    let (sig, rate, dev) = case.generate(&mut rng);
    let cfo = (rng.unit() - 0.5) * 0.1 * obw;
    let pad = if case.fs() < 5e5 { 4e-3 } else { 2e-3 };
    let e = embed(&mut rng, &sig, case.fs(), snr, obw, pad, cfo);
    let o = chain.run_embedded(&e, obw);
    let best_ok = o.best_rate().is_some_and(|r| (r / rate - 1.0).abs() < 0.01);
    let trusted = o.trusted_rate();
    t.n += 1;
    t.rate_ok += usize::from(best_ok);
    if let Some(r) = trusted {
        t.trusted += 1;
        if (r / rate - 1.0).abs() >= 0.01 {
            t.trusted_wrong += 1;
            eprintln!(
                "[{AWARE_036}] TRUSTED-WRONG {case:?} {snr} dB seed {seed}: {}",
                describe(&o)
            );
        }
    }
    let fam = o.family();
    if fam == case.family() {
        t.family_ok += 1;
    } else if fam != Family::Unknown {
        t.wrong_label += 1;
        eprintln!(
            "[{AWARE_036}] WRONG-LABEL {case:?} {snr} dB seed {seed}: {}",
            describe(&o)
        );
    }
    if let (Some(d), Some(truth)) = (o.deviation(), dev) {
        t.dev_n += 1;
        t.dev_ok += usize::from((d / truth - 1.0).abs() < 0.10);
    }
    t.cost_us += o.sym.as_ref().map_or(0, |s| s.cost_us);
    if std::env::var("HK_T011_VERBOSE").is_ok() {
        eprintln!(
            "[{AWARE_036}] {case:?} {snr} dB seed {seed}: {}",
            describe(&o)
        );
    }
}

fn sweep(snrs: &[f64], trials: u64) -> Vec<(Case, f64, Tally)> {
    let mut chain = Chain::default();
    let mut rows = Vec::new();
    for (ci, case) in CASES.into_iter().enumerate() {
        let obw = case.clean_obw();
        for &snr in snrs {
            let mut t = Tally::default();
            for k in 0..trials {
                let seed = 10_000 * (ci as u64 + 1) + (snr as u64) * 100 + k;
                run_one(&mut chain, case, snr, obw, seed, &mut t);
            }
            eprintln!(
                "[{AWARE_036}] {case:?} (OBW {obw:.0}) {snr:4} dB: family {}/{} wrong {} rate<1% {} \
                 trusted {} t&w {} dev<10% {}/{} C14 {:.2} ms",
                t.family_ok,
                t.n,
                t.wrong_label,
                t.rate_ok,
                t.trusted,
                t.trusted_wrong,
                t.dev_ok,
                t.dev_n,
                t.cost_us as f64 / 1e3 / t.n as f64
            );
            rows.push((case, snr, t));
        }
    }
    rows
}

fn totals(rows: &[(Case, f64, Tally)]) -> Tally {
    let mut all = Tally::default();
    for (_, _, t) in rows {
        all.n += t.n;
        all.trusted_wrong += t.trusted_wrong;
        all.wrong_label += t.wrong_label;
        all.trusted += t.trusted;
        all.dev_n += t.dev_n;
        all.dev_ok += t.dev_ok;
        all.cost_us += t.cost_us;
    }
    all
}

#[test]
fn blind_sweep_subset() {
    let rows = sweep(&[6.0, 15.0, 20.0, 30.0], 3);
    let all = totals(&rows);
    eprintln!(
        "[{AWARE_036}] subset: {} runs, trusted {}, trusted&wrong {}, wrong labels {}, dev<10% {}/{}",
        all.n, all.trusted, all.trusted_wrong, all.wrong_label, all.dev_ok, all.dev_n
    );
    assert_eq!(all.trusted_wrong, 0, "[{AWARE_036}] trusted and wrong");
    assert_eq!(all.wrong_label, 0, "[{AWARE_036}] wrong family labels");
    for (case, snr, t) in &rows {
        if *snr >= 20.0 {
            assert!(
                t.family_ok == t.n && t.rate_ok == t.n && t.trusted == t.n,
                "[{AWARE_036}] {case:?} at {snr} dB: {t:?}"
            );
        }
    }
    assert_eq!(all.dev_ok, all.dev_n, "[{AWARE_036}] deviation within 10 %");
}

/// (case, min SNR, family ≥, rate within 1 % ≥, trusted ≥) per 20-trial cell, from S5 §3.3.
const FLOORS: [(Case, f64, f64, f64, f64); 7] = [
    (Case::Ook4k, 20.0, 1.0, 1.0, 1.0),
    (Case::Fsk9k6H1, 20.0, 1.0, 1.0, 1.0),
    (Case::Gfsk50kH05, 20.0, 1.0, 1.0, 1.0),
    (Case::Bpsk25k, 15.0, 1.0, 1.0, 0.85),
    (Case::Bpsk25k, 20.0, 1.0, 1.0, 1.0),
    (Case::Qpsk25k, 15.0, 1.0, 1.0, 0.75),
    (Case::Qpsk25k, 20.0, 1.0, 1.0, 1.0),
];

#[test]
#[ignore = "900 runs: cargo test --release -p hk-estimate --test blind_synthetic -- --ignored"]
fn blind_sweep_full() {
    let rows = sweep(&[0.0, 3.0, 6.0, 9.0, 12.0, 15.0, 20.0, 25.0, 30.0], 20);
    let all = totals(&rows);
    eprintln!(
        "[{AWARE_036}] full sweep: {} runs, trusted {}, trusted&wrong {}, wrong labels {}, dev<10% \
         {}/{}, C14 mean {:.2} ms",
        all.n,
        all.trusted,
        all.trusted_wrong,
        all.wrong_label,
        all.dev_ok,
        all.dev_n,
        all.cost_us as f64 / 1e3 / all.n as f64
    );
    assert!(all.n >= 900);
    assert_eq!(all.trusted_wrong, 0, "[{AWARE_036}] trusted and wrong");
    assert_eq!(all.wrong_label, 0, "[{AWARE_036}] wrong family labels");
    for (case, min_snr, fam, rate, trusted) in FLOORS {
        for (c, snr, t) in &rows {
            if *c == case && *snr >= min_snr {
                let f = |k: usize| k as f64 / t.n as f64;
                assert!(
                    f(t.family_ok) >= fam && f(t.rate_ok) >= rate && f(t.trusted) >= trusted,
                    "[{AWARE_036}] {case:?} at {snr} dB below the S5 floor: {t:?}"
                );
            }
        }
    }
    assert!(
        all.dev_ok as f64 >= 0.95 * all.dev_n as f64,
        "[{AWARE_036}] deviation within 10 %: {}/{}",
        all.dev_ok,
        all.dev_n
    );
}

/// The OBW-scaled S5 constants against T-010's OBW99 (S5's OBW99 zeroed negative PSD bins and
/// read wider): C13 OBW99 tracks the clean OBW99, the rate bound 1.2·OBW covers Rs, and the
/// delay-multiply D = round(fs/(2·OBW)) and the IF smoothing stay a usable fraction of T.
#[test]
fn blind_constants_against_t010_obw() {
    let mut chain = Chain::default();
    for case in CASES {
        let clean = case.clean_obw();
        for snr in [12.0, 20.0, 30.0] {
            let mut rng = Rng::new(900 + snr as u64);
            let (sig, rate, _) = case.generate(&mut rng);
            let pad = if case.fs() < 5e5 { 4e-3 } else { 2e-3 };
            let e = embed(&mut rng, &sig, case.fs(), snr, clean, pad, 0.0);
            let o = chain.run_embedded(&e, clean);
            let Some(obw) = o.params.obw99_hz.value() else {
                eprintln!("[{AWARE_036}] {case:?} {snr} dB: C13 OBW99 below its floor");
                continue;
            };
            let fs = o.sym.as_ref().unwrap().sample_rate_hz;
            let t = fs / rate;
            let d = (fs / obw / 2.0).round().max(1.0);
            eprintln!(
                "[{AWARE_036}] {case:?} {snr} dB: OBW99 {obw:.0} / clean {clean:.0} = {:.3}, \
                 1.2·OBW/Rs {:.2}, fs/OBW {:.2}, D/T {:.2}",
                obw / clean,
                1.2 * obw / rate,
                fs / obw,
                d / t
            );
            // NRZ OOK's sinc² tails vanish into the noise: its OBW99 reads ~0.6× clean at 12 dB.
            assert!(
                (0.5..=1.3).contains(&(obw / clean)),
                "[{AWARE_036}] {case:?} {snr} dB: OBW99 {obw:.0} vs clean {clean:.0}"
            );
            assert!(
                1.2 * obw >= 1.1 * rate,
                "[{AWARE_036}] {case:?}: rate bound"
            );
            assert!(
                (0.05..=0.6).contains(&(d / t)),
                "[{AWARE_036}] {case:?}: D/T {:.2}",
                d / t
            );
        }
    }
}

#[test]
fn blind_negatives() {
    let fs = 1e6;
    let mut chain = Chain::default();
    let mut labelled = 0;
    let mut trusted = 0;
    let mut n = 0;
    for kind in ["noise", "cw", "nbfm_voice", "lora_chirp"] {
        for snr in [10.0, 20.0, 30.0] {
            for k in 0..3u64 {
                let mut rng = Rng::new(700 + k * 10 + snr as u64);
                let (sig, obw) = match kind {
                    "noise" => (vec![Complex32::default(); 20_000], 20e3),
                    "cw" => (gen_cw(20_000, 1000.0, fs), 2e3),
                    "nbfm_voice" => (gen_nbfm_voice(&mut rng, 0.1, fs, 2500.0), 8e3),
                    _ => (gen_chirp(125e3, 7, 8, fs), 125e3),
                };
                let e = embed(&mut rng, &sig, fs, snr, obw, 4e-3, 0.0);
                let o = chain.run_embedded(&e, obw.max(20e3));
                n += 1;
                let bad_label = o.family() != Family::Unknown;
                let bad_rate = o.trusted_rate().is_some();
                labelled += usize::from(bad_label);
                trusted += usize::from(bad_rate);
                if bad_label || bad_rate || std::env::var("HK_T011_VERBOSE").is_ok() {
                    eprintln!(
                        "[{AWARE_036}] negative {kind} {snr} dB #{k}: {}",
                        describe(&o)
                    );
                }
            }
        }
    }
    eprintln!("[{AWARE_036}] negatives: {n} runs, labelled {labelled}, trusted {trusted}");
    assert_eq!(labelled, 0, "[{AWARE_036}] negatives labelled");
    assert_eq!(trusted, 0, "[{AWARE_036}] negatives with a trusted rate");
}
