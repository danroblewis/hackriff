//! AWARE-036, T-030: C14 must never trust a harmonic of the symbol rate on high-index FSK.
//!
//! T-013 found C14 trusting 33.6 kBd (7 × the true 4.8 kBd) on 13 of 20 rectangular 2-FSK
//! bursts with h = 4. Rectangular FSK with a large index has sharp IF steps, so the lines and the
//! run-length seed land on high harmonics; the transition fit then fits the clock at T/7 and
//! only common run-count factors 2..5 were divided out, so a prime factor ≥ 7 passed every guard
//! (all transitions on the sub-lattice, odd-run fraction ≈ 67 %).
//!
//! - `t030_rect_fsk_h4_never_trusts_a_harmonic` (named regression): h = 4 at 4.8 kBd.
//! - `t030_rect_fsk_index_sweep` (always on): rectangular 2-FSK, h ∈ {0.5, 1, 2, 3, 4, 6},
//!   4.8 and 38.4 kBd, 12/20/30 dB, 8-bit quantised: 0 trusted-and-wrong; trusted fraction per h
//!   reported.
//! - `t030_rect_fsk_index_sweep_full` (`#[ignore]`): the same with more trials.

mod blind_support;

use blind_support::*;
use hk_dsp::synth::Rng;

const AWARE_036: &str = "AWARE-036";

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    n: usize,
    trusted: usize,
    trusted_wrong: usize,
    best_ok: usize,
}

/// One rectangular 2-FSK burst (32-bit 0101 preamble + random bits) through the product chain.
fn run_one(chain: &mut Chain, rate: f64, h: f64, snr: f64, seed: u64, t: &mut Tally) {
    let dev = 0.5 * h * rate;
    // ≥ 10 samples per Carson bandwidth so the extraction can take 6 per OBW99.
    let fs = (10.0 * (h + 2.0) * rate).max(250e3);
    let mut rng = Rng::new(seed);
    let sig = gen_fsk(&mut rng, rate, dev, 400, fs, None, 32);
    let obw = clean_obw(&sig, fs);
    let cfo = (rng.unit() - 0.5) * 0.1 * obw;
    let pad = if fs < 5e5 { 4e-3 } else { 2e-3 };
    let e = embed(&mut rng, &sig, fs, snr, obw, pad, cfo);
    let o = chain.run_embedded(&e, obw);
    t.n += 1;
    t.best_ok += usize::from(o.best_rate().is_some_and(|r| (r / rate - 1.0).abs() < 0.01));
    if let Some(r) = o.trusted_rate() {
        t.trusted += 1;
        if (r / rate - 1.0).abs() >= 0.01 {
            t.trusted_wrong += 1;
            eprintln!(
                "[{AWARE_036}] TRUSTED-WRONG rect FSK {rate} Bd h {h} {snr} dB seed {seed}: \
                 {r:.0} Bd ({:.2}×) {}",
                r / rate,
                describe(&o)
            );
        }
    }
    if std::env::var("HK_T011_VERBOSE").is_ok() {
        eprintln!(
            "[{AWARE_036}] rect FSK {rate} Bd h {h} {snr} dB seed {seed}: {}",
            describe(&o)
        );
    }
}

fn sweep(hs: &[f64], rates: &[f64], snrs: &[f64], trials: u64) -> Vec<(f64, f64, f64, Tally)> {
    let mut chain = Chain::default();
    let mut rows = Vec::new();
    for (ri, &rate) in rates.iter().enumerate() {
        for (hi, &h) in hs.iter().enumerate() {
            for &snr in snrs {
                let mut t = Tally::default();
                for k in 0..trials {
                    let seed =
                        30_000 + 10_000 * ri as u64 + 1_000 * hi as u64 + 10 * snr as u64 + k;
                    run_one(&mut chain, rate, h, snr, seed, &mut t);
                }
                eprintln!(
                    "[{AWARE_036}] rect FSK {rate:7} Bd h {h:3} {snr:4} dB: trusted {}/{} \
                     trusted&wrong {} best<1% {}",
                    t.trusted, t.n, t.trusted_wrong, t.best_ok
                );
                rows.push((rate, h, snr, t));
            }
        }
    }
    // Trusted fraction per h (all rates and SNRs).
    for &h in hs {
        let (mut n, mut tr, mut tw) = (0, 0, 0);
        for (_, hh, _, t) in &rows {
            if *hh == h {
                n += t.n;
                tr += t.trusted;
                tw += t.trusted_wrong;
            }
        }
        eprintln!(
            "[{AWARE_036}] rect FSK h {h:3}: trusted {tr}/{n} ({:.0} %), trusted&wrong {tw}",
            100.0 * tr as f64 / n as f64
        );
    }
    rows
}

fn trusted_wrong(rows: &[(f64, f64, f64, Tally)]) -> usize {
    rows.iter().map(|r| r.3.trusted_wrong).sum()
}

/// T-030 regression: rectangular 2-FSK h = 4 at 4.8 kBd (the T-013 sensor's modulation) was
/// trusted at 7 × the rate. It must never be trusted at a harmonic.
#[test]
fn t030_rect_fsk_h4_never_trusts_a_harmonic() {
    let rows = sweep(&[4.0], &[4800.0], &[12.0, 20.0, 30.0], 6);
    assert_eq!(
        trusted_wrong(&rows),
        0,
        "[{AWARE_036}] T-030: harmonic rate trusted on rectangular FSK h = 4"
    );
}

#[test]
fn t030_rect_fsk_index_sweep() {
    let rows = sweep(
        &[0.5, 1.0, 2.0, 3.0, 4.0, 6.0],
        &[4800.0, 38_400.0],
        &[12.0, 20.0, 30.0],
        2,
    );
    assert_eq!(trusted_wrong(&rows), 0, "[{AWARE_036}] trusted and wrong");
}

#[test]
#[ignore = "720 runs: cargo test --release -p hk-estimate --test blind_fsk_high_index -- --ignored"]
fn t030_rect_fsk_index_sweep_full() {
    let rows = sweep(
        &[0.5, 1.0, 2.0, 3.0, 4.0, 6.0],
        &[4800.0, 38_400.0],
        &[12.0, 16.0, 20.0, 25.0, 30.0],
        12,
    );
    assert_eq!(trusted_wrong(&rows), 0, "[{AWARE_036}] trusted and wrong");
}
