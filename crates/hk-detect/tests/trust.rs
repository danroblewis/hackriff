//! S4 rules 6–7 as pure functions over two capture results: gain step (SNR invariance) and
//! retune. Synthetic integrated spectra with exact levels.

use hk_detect::{
    CaptureEmitter, CaptureResult, CaptureSide, EdgeRule, GainState, GainStepConfig, GainStepSkip,
    GainStepVerdict, Geometry, IntegratedSnapshot, RetuneConfig, RetuneLabel, gain_step, retune,
};
use hk_model::DetectionFlags;
use hk_model::detection::SpurReason;

const FS: f64 = 20e6;
const BINS: usize = 4096;

struct Sig {
    f: f64,
    bw: f64,
    snr_db: f64,
}

fn capture(fc: f64, floor: f64, gain: GainState, sigs: &[Sig]) -> CaptureResult {
    let geometry = Geometry::new(fc, FS, BINS, 15e6, &EdgeRule::default());
    let mut psd = vec![floor; BINS];
    for s in sigs {
        let lo = geometry.bin_at_or_above(s.f - s.bw / 2.0);
        let hi = geometry.bin_at_or_above(s.f + s.bw / 2.0).max(lo + 1);
        for p in &mut psd[lo..hi] {
            *p += floor * 10f64.powf(s.snr_db / 10.0);
        }
    }
    let emitters = sigs
        .iter()
        .map(|s| {
            let lo = geometry.bin_at_or_above(s.f - s.bw / 2.0);
            CaptureEmitter {
                f_lo_hz: s.f - s.bw / 2.0,
                f_hi_hz: s.f + s.bw / 2.0,
                f_center_hz: s.f,
                bandwidth_hz: s.bw,
                peak_excess_dbfs: 10.0 * ((psd[lo] - floor) * geometry.bin_width_hz).log10(),
                spur: false,
                dc: false,
                image: false,
                edge: false,
            }
        })
        .collect();
    CaptureResult {
        center_hz: fc,
        gain,
        quantisation_limited: false,
        clipped: false,
        spectrum: IntegratedSnapshot {
            geometry,
            span_s: 1.0,
            mean_psd: psd.clone(),
            mean_floor: vec![floor; BINS],
            block_psd: vec![psd; 4],
        },
        emitters,
    }
}

fn sig(f: f64, bw: f64, snr_db: f64) -> Sig {
    Sig { f, bw, snr_db }
}

#[test]
fn gain_step_flags_imd_and_compression_and_keeps_real_signals_linear() {
    let low = GainState {
        lna_db: 16.0,
        vga_db: 20.0,
        amp_on: false,
    };
    let high = GainState {
        lna_db: 24.0,
        vga_db: 22.0,
        amp_on: false,
    };
    let stations = [93.3e6, 94.9e6, 96.5e6, 101.3e6];
    let mut a_sigs: Vec<Sig> = stations.iter().map(|&f| sig(f, 150e3, 20.0)).collect();
    a_sigs.push(sig(92.5e6, 60e3, 3.0)); // IM3 product
    a_sigs.push(sig(104.5e6, 150e3, 20.0)); // will compress
    // +10 dB of gain: floor and real signals +10 dB (SNR kept); IM3 +30 dB; compressed +2 dB.
    let mut b_sigs: Vec<Sig> = stations.iter().map(|&f| sig(f, 150e3, 20.0)).collect();
    // IM3 level +30 dB over a floor +10 dB: SNR 3 → 23 dB.
    b_sigs.push(sig(92.5e6, 60e3, 23.0));
    // Compressed: level +2 dB over a floor +10 dB: SNR 20 → 12 dB.
    b_sigs.push(sig(104.5e6, 150e3, 12.0));
    let a = capture(98e6, 1e-9, low, &a_sigs);
    let b = capture(98e6, 1e-8, high, &b_sigs);
    let r = gain_step(&a, &b, &GainStepConfig::default());
    assert_eq!(r.skipped, None);
    assert!((r.nominal_db - 10.0).abs() < 1e-9);
    assert!((r.g_lin_db - 10.0).abs() < 0.1, "G_lin {}", r.g_lin_db);
    assert!((r.delta_floor_db - 10.0).abs() < 1e-6);
    assert!(r.bound_db.abs() < 0.1);
    assert!(r.anchors >= 3);
    let verdict = |f: f64| {
        let i = b.emitters.iter().position(|e| e.f_center_hz == f).unwrap();
        r.rows.iter().find(|row| row.emitter == i).unwrap().verdict
    };
    for f in stations {
        assert_eq!(verdict(f), GainStepVerdict::Linear, "{f}");
    }
    assert_eq!(verdict(92.5e6), GainStepVerdict::SuspectImd);
    assert_eq!(verdict(104.5e6), GainStepVerdict::Compressed);
    let mut flags = DetectionFlags::default();
    GainStepVerdict::SuspectImd.apply(&mut flags);
    GainStepVerdict::Compressed.apply(&mut flags);
    GainStepVerdict::InconclusiveBursty.apply(&mut flags);
    assert!(flags.suspect_imd && flags.compressed && flags.marginal);

    // Never on clipped blocks, and not from a quantisation-limited lower state.
    let mut clipped = b.clone();
    clipped.clipped = true;
    assert_eq!(
        gain_step(&a, &clipped, &GainStepConfig::default()).skipped,
        Some(GainStepSkip::Clipped)
    );
    let mut quant = a.clone();
    quant.quantisation_limited = true;
    let q = gain_step(&quant, &b, &GainStepConfig::default());
    assert_eq!(q.skipped, Some(GainStepSkip::LowerQuantisationLimited));
    assert!(q.rows.is_empty());
}

#[test]
fn gain_step_marks_bursty_emitters_inconclusive() {
    let low = GainState {
        lna_db: 16.0,
        vga_db: 20.0,
        amp_on: false,
    };
    let high = GainState {
        lna_db: 24.0,
        vga_db: 22.0,
        amp_on: false,
    };
    let sigs: Vec<Sig> = [93.3e6, 94.9e6, 96.5e6, 101.3e6]
        .iter()
        .map(|&f| sig(f, 150e3, 20.0))
        .collect();
    let a = capture(98e6, 1e-9, low, &sigs);
    let mut b = capture(98e6, 1e-8, high, &sigs);
    // The 101.3 MHz emitter is off in two of four blocks.
    let g = b.spectrum.geometry;
    let (lo, hi) = (
        g.bin_at_or_above(101.3e6 - 75e3),
        g.bin_at_or_above(101.3e6 + 75e3),
    );
    for blk in &mut b.spectrum.block_psd[..2] {
        for p in &mut blk[lo..hi] {
            *p = 1e-8;
        }
    }
    let r = gain_step(&a, &b, &GainStepConfig::default());
    let i = b
        .emitters
        .iter()
        .position(|e| e.f_center_hz == 101.3e6)
        .unwrap();
    assert_eq!(
        r.rows.iter().find(|row| row.emitter == i).unwrap().verdict,
        GainStepVerdict::InconclusiveBursty
    );
}

#[test]
fn retune_separates_absolute_lo_relative_image_and_unreproduced_emitters() {
    let g = GainState {
        lna_db: 24.0,
        vga_db: 20.0,
        amp_on: false,
    };
    let a = capture(
        98e6,
        1e-9,
        g,
        &[
            sig(95.0e6, 150e3, 20.0),  // station: stays
            sig(98.0e6, 10e3, 25.0),   // DC: moves with the LO
            sig(100.5e6, 100e3, 30.0), // strong real signal
            sig(95.5e6, 100e3, 8.0),   // its image at 2·98 − 100.5
            sig(96.2e6, 20e3, 12.0),   // only in A
        ],
    );
    let b = capture(
        99e6,
        1e-9,
        g,
        &[
            sig(95.0e6, 150e3, 20.0),
            sig(99.0e6, 10e3, 25.0),
            sig(100.5e6, 100e3, 30.0),
            sig(97.5e6, 100e3, 8.0), // image moved by 2Δ
        ],
    );
    let r = retune(&a, &b, &RetuneConfig::default());
    assert!((r.delta_hz - 1e6).abs() < 1e-6);
    let label = |side: CaptureSide, f: f64| {
        r.rows
            .iter()
            .find(|row| row.capture == side && (row.f_center_hz - f).abs() < 1.0)
            .unwrap_or_else(|| panic!("no row for {f}"))
            .label
    };
    assert_eq!(label(CaptureSide::A, 95.0e6), RetuneLabel::Stays);
    assert_eq!(label(CaptureSide::A, 100.5e6), RetuneLabel::Stays);
    assert_eq!(label(CaptureSide::A, 98.0e6), RetuneLabel::MovesWithLo);
    assert_eq!(label(CaptureSide::B, 99.0e6), RetuneLabel::MovesWithLo);
    assert_eq!(label(CaptureSide::A, 95.5e6), RetuneLabel::ImageMoves);
    assert_eq!(label(CaptureSide::A, 96.2e6), RetuneLabel::NotReproduced);

    let mut f = DetectionFlags::default();
    RetuneLabel::MovesWithLo.apply(&mut f);
    assert_eq!(f.spur_reason, Some(SpurReason::LoRelative));
    let mut f = DetectionFlags::default();
    RetuneLabel::ImageMoves.apply(&mut f);
    assert!(f.image_candidate && f.image_retune_confirmed && f.inconsistency().is_none());
    let mut f = DetectionFlags::default();
    RetuneLabel::NotReproduced.apply(&mut f);
    assert!(f.marginal);
}
