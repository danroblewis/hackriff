//! T-607: what the liquid-dsp binding actually provides, measured rather than assumed.
//!
//! These tests are the executable form of docs/18 §7.1.1's per-block coverage note. They pin
//! three answers the block tickets are priced against:
//!
//! 1. `psk_demod` (T-609) is an adapter job for its kernels. `symtrack_cccf` + `modemcf` recover
//!    BPSK/QPSK/8PSK/D8PSK/π/4-DQPSK/DBPSK blind to timing, phase and (bounded) carrier offset,
//!    and a coarse CFO handed in through `symtrack_cccf_adjust_frequency` extends the pull-in.
//! 2. Those kernels are chunking-invariant (ADR-0011 §1.6's first owed test), bit for bit.
//! 3. `viterbi` (T-610) and `reed_solomon` (T-611) get NOTHING from liquid: every convolutional
//!    and Reed–Solomon scheme fails to construct, because in liquid they are libfec (LGPL) wrappers.
//!
//! Signals are synthetic and deterministic (a fixed LCG), so every number below is exact on every
//! run and on both targets — the same binary cross-built for aarch64 Linux produced the identical
//! table in an Ubuntu 22.04 arm64 container (docs/18 §7.1.1).

use std::f32::consts::TAU;
use std::ffi::CStr;

use hk_liquid_sys::*;

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }
    fn unif(&mut self) -> f32 {
        (self.next() as f32 + 0.5) / (1u64 << 31) as f32
    }
    fn gauss(&mut self) -> f32 {
        let (u, v) = (self.unif(), self.unif());
        (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
    }
}

fn modem(name: &str) -> i32 {
    scheme_id(liquid_getopt_str2mod, name).unwrap_or_else(|| panic!("liquid has no modem {name:?}"))
}

fn rrc() -> i32 {
    scheme_id(liquid_getopt_str2firfilt, "rrcos").expect("liquid has an RRC prototype")
}

const K: u32 = 2;
const M: u32 = 7;
const BETA: f32 = 0.35;

/// Transmit `n_sym` random symbols of `scheme` through an RRC interpolator (k = 2, with a 0.3
/// sample fractional delay), then a 0.7 rad phase offset, a `cfo` rad/sample carrier offset and
/// AWGN at `es_n0_db`. Returns (transmitted symbols, received samples).
fn transmit(scheme: &str, n_sym: usize, cfo: f32, es_n0_db: f32) -> (Vec<u32>, Vec<Complex32>) {
    let ms = modem(scheme);
    let mut rng = Lcg(0x5eed ^ ms as u64);
    unsafe {
        let tx = modemcf_create(ms);
        let interp = firinterp_crcf_create_prototype(rrc(), K, M, BETA, 0.3);
        assert!(!tx.is_null() && !interp.is_null());
        let bps = modemcf_get_bps(tx);
        let syms: Vec<u32> = (0..n_sym).map(|_| rng.next() % (1 << bps)).collect();
        let mut x = vec![Complex32::new(0.0, 0.0); n_sym];
        for (s, xi) in syms.iter().zip(x.iter_mut()) {
            assert_eq!(modemcf_modulate(tx, *s, xi), LIQUID_OK);
        }
        let mut y = vec![Complex32::new(0.0, 0.0); n_sym * K as usize];
        assert_eq!(
            firinterp_crcf_execute_block(interp, x.as_mut_ptr(), n_sym as u32, y.as_mut_ptr()),
            LIQUID_OK
        );
        let sigma = (10f32.powf(-es_n0_db / 10.0) / (2.0 * K as f32)).sqrt();
        for (n, v) in y.iter_mut().enumerate() {
            *v = *v * Complex32::from_polar(1.0, 0.7 + cfo * n as f32)
                + Complex32::new(sigma * rng.gauss(), sigma * rng.gauss());
        }
        modemcf_destroy(tx);
        firinterp_crcf_destroy(interp);
        (syms, y)
    }
}

/// Run `samples` through `symtrack_cccf` in chunks of `chunk`, optionally pre-loading a coarse
/// CFO estimate. Returns the recovered symbol-rate samples.
fn track(
    scheme: &str,
    samples: &[Complex32],
    chunk: usize,
    coarse_cfo: Option<f32>,
) -> Vec<Complex32> {
    let mut input = samples.to_vec();
    let mut out = Vec::with_capacity(samples.len() / K as usize + 8);
    let mut buf = vec![Complex32::new(0.0, 0.0); 2 * chunk];
    unsafe {
        let q = symtrack_cccf_create(rrc(), K, M, BETA, modem(scheme));
        assert!(!q.is_null());
        assert_eq!(symtrack_cccf_set_bandwidth(q, 0.02), LIQUID_OK);
        if let Some(dphi) = coarse_cfo {
            assert_eq!(symtrack_cccf_adjust_frequency(q, dphi), LIQUID_OK);
        }
        for c in input.chunks_mut(chunk) {
            let mut ny = 0u32;
            assert_eq!(
                symtrack_cccf_execute_block(
                    q,
                    c.as_mut_ptr(),
                    c.len() as u32,
                    buf.as_mut_ptr(),
                    &mut ny
                ),
                LIQUID_OK
            );
            out.extend_from_slice(&buf[..ny as usize]);
        }
        symtrack_cccf_destroy(q);
    }
    out
}

/// Symbol error rate over the second half of `rx`, minimised over the unknown group delay and —
/// for coherent schemes — the M-fold phase ambiguity. Blind: the receiver is never told either.
fn ser(scheme: &str, tx: &[u32], rx: &[Complex32]) -> f64 {
    unsafe {
        let q = modemcf_create(modem(scheme));
        let m_ary = 1usize << modemcf_get_bps(q);
        let start = rx.len() / 2;
        let mut best = 1.0f64;
        for rot in 0..m_ary {
            let r = Complex32::from_polar(1.0, TAU * rot as f32 / m_ary as f32);
            assert_eq!(modemcf_reset(q), LIQUID_OK);
            let dec: Vec<u32> = rx
                .iter()
                .map(|v| {
                    let mut s = 0u32;
                    modemcf_demodulate(q, *v * r, &mut s);
                    s
                })
                .collect();
            for d in 0..64 {
                let (mut n, mut err) = (0usize, 0usize);
                for i in start..rx.len() {
                    if i >= d && i - d < tx.len() {
                        n += 1;
                        err += usize::from(dec[i] != tx[i - d]);
                    }
                }
                if n > 0 {
                    best = best.min(err as f64 / n as f64);
                }
            }
        }
        modemcf_destroy(q);
        best
    }
}

#[test]
fn the_vendored_library_is_the_pinned_release() {
    let v = unsafe { CStr::from_ptr(liquid_libversion()) };
    assert_eq!(v.to_str().unwrap(), "1.8.2");
    assert_eq!(unsafe { liquid_libversion_number() }, 0x01_08_02);
}

#[test]
fn psk_family_recovers_blind_through_symtrack() {
    // (scheme, largest carrier offset in rad/sample it must pull in unaided at k = 2, bw = 0.02).
    // The bound shrinks with constellation order for coherent schemes, and differential ones do
    // not care — which is the measured reason T-609 needs a coarse CFO stage for coherent 8PSK.
    let cases = [
        ("bpsk", 0.02),
        ("qpsk", 0.01),
        ("psk8", 0.005),
        ("dpsk2", 0.02),
        ("dpsk8", 0.02),
        ("pi4dqpsk", 0.02),
    ];
    for (scheme, max_cfo) in cases {
        for cfo in [0.0f32, max_cfo] {
            let (tx, y) = transmit(scheme, 20_000, cfo, 22.0);
            let rx = track(scheme, &y, 1000, None);
            let s = ser(scheme, &tx, &rx);
            assert_eq!(s, 0.0, "{scheme} at cfo {cfo}: SER {s}");
        }
    }
}

#[test]
fn coherent_pull_in_is_bounded_and_a_coarse_cfo_extends_it() {
    // QPSK at 0.02 rad/sample is past the loop's unaided pull-in: it must NOT lock (so a green
    // result above is not the tracker ignoring CFO)…
    let (tx, y) = transmit("qpsk", 20_000, 0.02, 22.0);
    let unaided = ser("qpsk", &tx, &track("qpsk", &y, 1000, None));
    assert!(
        unaided > 0.1,
        "QPSK at 0.02 rad/sample locked unaided (SER {unaided}); the bound moved"
    );
    // …and handing it the coarse estimate (what hk-estimate's CFO estimator provides) locks it.
    let aided = ser("qpsk", &tx, &track("qpsk", &y, 1000, Some(0.02)));
    assert_eq!(aided, 0.0, "QPSK with a coarse CFO hand-off: SER {aided}");
}

#[test]
fn symtrack_output_is_identical_however_the_input_is_chunked() {
    let (_, y) = transmit("qpsk", 8000, 0.005, 20.0);
    let whole = track("qpsk", &y, y.len(), None);
    for chunk in [1000, 37, 1] {
        let split = track("qpsk", &y, chunk, None);
        assert_eq!(
            whole.len(),
            split.len(),
            "chunk {chunk}: symbol count differs"
        );
        assert!(whole == split, "chunk {chunk}: symbols differ");
    }
}

#[test]
fn liquid_ships_no_viterbi_and_no_reed_solomon() {
    // Every convolutional and Reed–Solomon scheme liquid names: all libfec (LGPL-2.1) wrappers,
    // absent from this build. If one of these starts constructing, someone linked libfec — re-read
    // ADR-0010's ledger before letting it stay.
    let absent = [
        "v27", "v29", "v39", "v615", "v27p23", "v27p34", "v27p45", "v27p56", "v27p67", "v27p78",
        "v29p23", "v29p34", "v29p45", "v29p56", "v29p67", "v29p78", "rs8",
    ];
    // What liquid does implement itself: short block codes only.
    let present = [
        "rep3",
        "rep5",
        "h74",
        "h84",
        "h128",
        "g2412",
        "secded2216",
        "secded3932",
        "secded7264",
    ];
    for name in absent {
        let id = scheme_id(liquid_getopt_str2fec, name)
            .unwrap_or_else(|| panic!("liquid lost the name {name}"));
        let q = unsafe { fec_create(id, std::ptr::null_mut()) };
        assert!(q.is_null(), "fec {name} constructed — libfec is linked");
    }
    for name in present {
        let id = scheme_id(liquid_getopt_str2fec, name)
            .unwrap_or_else(|| panic!("liquid lost the name {name}"));
        let q = unsafe { fec_create(id, std::ptr::null_mut()) };
        assert!(!q.is_null(), "fec {name} did not construct");
        assert_eq!(unsafe { fec_destroy(q) }, LIQUID_OK);
    }
}

#[test]
fn oqpsk_is_not_a_liquid_modem() {
    // T-609 needs OQPSK (Zigbee); liquid's linear modem has no such scheme, so that variant is
    // DSP work (a half-symbol I/Q offset ahead of the tracker), not an adapter.
    assert_eq!(scheme_id(liquid_getopt_str2mod, "oqpsk"), None);
    assert!(scheme_id(liquid_getopt_str2mod, "pi4dqpsk").is_some());
    assert!(scheme_id(liquid_getopt_str2mod, "dpsk8").is_some());
}

#[test]
fn every_declared_entry_point_links_and_behaves() {
    // A hand-written declaration with the wrong signature is undefined behaviour, so each one is
    // called at least once with a checkable result. (The pipeline tests above cover the rest.)
    unsafe {
        // modem: soft de-mapping, EVM and phase error on a clean, slightly rotated QPSK point.
        let q = modemcf_create(modem("qpsk"));
        let mut x = Complex32::new(0.0, 0.0);
        assert_eq!(modemcf_modulate(q, 3, &mut x), LIQUID_OK);
        let rotated = x * Complex32::from_polar(1.0, 0.1);
        let (mut s, mut soft) = (0u32, [0u8; 2]);
        assert_eq!(
            modemcf_demodulate_soft(q, rotated, &mut s, soft.as_mut_ptr()),
            LIQUID_OK
        );
        assert_eq!(s, 3);
        assert!(
            soft.iter().all(|&b| b > 200),
            "soft bits for symbol 3 should both be confident 1s: {soft:?}"
        );
        assert!((modemcf_get_demodulator_phase_error(q) - 0.1).abs() < 0.02);
        assert!(modemcf_get_demodulator_evm(q) > 0.0);
        modemcf_destroy(q);

        // symtrack: the remaining setters return LIQUID_OK and leave a usable object.
        let t = symtrack_cccf_create(rrc(), K, M, BETA, modem("bpsk"));
        assert_eq!(symtrack_cccf_set_modscheme(t, modem("qpsk")), LIQUID_OK);
        assert_eq!(symtrack_cccf_set_eq_dd(t), LIQUID_OK);
        assert_eq!(symtrack_cccf_set_eq_off(t), LIQUID_OK);
        assert_eq!(symtrack_cccf_set_eq_cm(t), LIQUID_OK);
        assert_eq!(symtrack_cccf_adjust_phase(t, 0.3), LIQUID_OK);
        assert_eq!(symtrack_cccf_reset(t), LIQUID_OK);
        symtrack_cccf_destroy(t);

        // nco: a PLL driven by its own mix-down phase error locks onto a 0.05 rad/sample tone.
        let n = nco_crcf_create(LIQUID_NCO);
        assert_eq!(nco_crcf_set_frequency(n, 0.0), LIQUID_OK);
        assert_eq!(nco_crcf_set_phase(n, 0.0), LIQUID_OK);
        assert_eq!(nco_crcf_pll_set_bandwidth(n, 0.02), LIQUID_OK);
        for i in 0..4000 {
            let tone = Complex32::from_polar(1.0, 0.05 * i as f32 + 1.0);
            let mut base = Complex32::new(0.0, 0.0);
            assert_eq!(nco_crcf_mix_down(n, tone, &mut base), LIQUID_OK);
            assert_eq!(nco_crcf_pll_step(n, base.arg()), LIQUID_OK);
            assert_eq!(nco_crcf_step(n), LIQUID_OK);
        }
        let f = nco_crcf_get_frequency(n);
        assert!((f - 0.05).abs() < 1e-3, "PLL frequency {f}, expected 0.05");
        assert!(nco_crcf_get_phase(n).is_finite());
        nco_crcf_destroy(n);
    }
}

#[test]
fn unknown_names_are_none_not_a_silent_default() {
    assert_eq!(scheme_id(liquid_getopt_str2mod, "no-such-modem"), None);
    assert_eq!(scheme_id(liquid_getopt_str2fec, "no-such-code"), None);
    assert_eq!(scheme_id(liquid_getopt_str2firfilt, "no-such-filter"), None);
    assert_eq!(scheme_id(liquid_getopt_str2mod, "nul\0inside"), None);
}
