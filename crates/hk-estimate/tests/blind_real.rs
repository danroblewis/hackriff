//! AWARE-036 (unknown burst triage), T-011 C14 on real 8-bit HackRF captures (gated by S5).
//!
//! - **915 MHz FHSS 2-FSK** (`ism_915M_10M_l24g30a1_t42p3_1p2s`): every annotated box →
//!   extraction at ≥ 1.25 Msps (as S5) → C13 → normalise → C14. Truth (`hackriff:truth`): sync
//!   truth rate 100/150 kBd, deviation reference (S5 fixed-rate path), S5 SNR; edge-alias pairs
//!   (one transmission at both ±fs/2) are counted once, by the stronger member, and both
//!   members must agree. Every trusted rate within 1 % of truth (boxes without sync truth:
//!   within 1 % of the emitter's 100 or 150 kBd), every measured deviation within 10 % of the
//!   reference, slivers below 0 dB unknown and untrusted, and the ≥ 20 dB bursts trusted.
//! - **RDS** (`fm_100p8M_2p4M_l32g30a1_t1p5_5s`): test-local FM discriminator → subcarrier
//!   snippet (mixed at a wrong 56.7 kHz, extracted at 6 × the 5 kHz box; C13 x² CFO; OBW99 is
//!   below the C13 floor, so C14 scales with the box width) → C14 on 0.25 s, 0.5 s and 1 s
//!   windows. Truth chip rate = 2·Rs = pilot/8 in the capture clock. Every window's best
//!   candidate within 1 % with the 1187.5 Bd data rate offered as the ½ alternative, no trusted
//!   and wrong window; trusted: every 1 s window and ≥ 85 % of the 0.25 s and 0.5 s windows.
//!   (Subcarrier SNR ≈ 2 dB. With window starts shifted by ¼–¾ window: 16–18 of 19 trusted at
//!   0.25 s, 8–9 of 9 at 0.5 s, 4 of 4 at 1 s; box widths 3.5–5 kHz give the same. The untrusted
//!   windows have their envelope-group line at 11–14 dB, under the 14 dB two-group rule.)
//! - **FM broadcast** (same capture, 20 ms station windows, analog): no trusted rate.

mod blind_support;
mod common;

use blind_support::{Chain, describe};
use common::*;
use hk_dsp::{Ddc, DdcSpec};
use hk_e2e::Fixture;
use hk_estimate::blind::{BlindConfig, BlindEstimator};
use hk_estimate::{Family, FamilyHint, Hints, NoiseReference, SnippetRequest};
use num_complex::Complex32;

const AWARE_036: &str = "AWARE-036";
const ISM: &str = "ism_915M_10M_l24g30a1_t42p3_1p2s";
const FM: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";

#[test]
fn aware_036_blind_rate_and_deviation_915mhz() {
    let (meta, data) = fixture_or_skip!(ISM);
    let fx = Fixture::load(&meta).unwrap();
    let prov = meta_provenance(&meta);
    let center = prov.tune.center_hz;
    let ppm = fx.scenario().and_then(|s| s.f64("/clock/ppm_measured"));
    let iq = read_ci8(&data);
    let mut chain = Chain {
        min_rate_hz: Some(1.25e6),
        ..Default::default()
    };
    let hints = Hints {
        clock_ppm: ppm,
        ..Default::default()
    };

    struct Row {
        index: i64,
        alias_of: Option<i64>,
        snr: f64,
        rate: Option<f64>,
        dev: Option<f64>,
        trusted: Option<f64>,
        family: Family,
        deviation: Option<f64>,
        cost_us: u64,
    }
    let mut rows = Vec::new();
    for b in fx.emissions() {
        let req = SnippetRequest {
            start_index: b.sample_start,
            end_index: b.sample_start + b.sample_count,
            center_offset_hz: b.center_hz() - center,
            bandwidth_hz: b.bandwidth_hz(),
        };
        let o = chain.run(&iq, &prov, &req, &hints);
        let row = Row {
            index: b.f64("burst_index").unwrap_or(-1.0) as i64,
            alias_of: b.f64("alias_of_burst_index").map(|v| v as i64),
            snr: b.expect_f64("snr_db"),
            rate: b.f64("symbol_rate_bd"),
            dev: b.f64("deviation_hz"),
            trusted: o.trusted_rate(),
            family: o.family(),
            deviation: o.deviation(),
            cost_us: o.sym.as_ref().map_or(0, |s| s.cost_us),
        };
        eprintln!(
            "[{AWARE_036}] burst {:2} ({}) S5 snr {:5.1} truth rate {:?} dev {:?} | {}",
            row.index,
            b.kind,
            row.snr,
            row.rate,
            row.dev.map(|d| d.round()),
            describe(&o)
        );
        rows.push(row);
    }
    let costs: Vec<u64> = rows.iter().map(|r| r.cost_us).collect();
    eprintln!(
        "[{AWARE_036}] C14 cost per snippet: mean {:.2} ms, max {:.2} ms",
        costs.iter().sum::<u64>() as f64 / costs.len() as f64 / 1e3,
        *costs.iter().max().unwrap() as f64 / 1e3
    );

    let mut trusted_right = Vec::new();
    let mut dev_checked = 0;
    for r in &rows {
        if let Some(a) = r.alias_of {
            let p = rows.iter().find(|o| o.index == a).expect("alias partner");
            if let (Some(x), Some(y)) = (r.trusted, p.trusted) {
                assert!(
                    (x / y - 1.0).abs() < 0.01,
                    "[{AWARE_036}] alias pair {}/{} disagree: {x:.0} vs {y:.0}",
                    r.index,
                    a
                );
            }
            // Count the pair once: the stronger member.
            if p.snr > r.snr || (p.snr == r.snr && p.index < r.index) {
                continue;
            }
        }
        if let Some(t) = r.trusted {
            match r.rate {
                Some(truth) => assert!(
                    (t / truth - 1.0).abs() < 0.01,
                    "[{AWARE_036}] burst {}: trusted {t:.0} vs truth {truth:.0}",
                    r.index
                ),
                None => assert!(
                    [100e3, 150e3].iter().any(|s| (t / s - 1.0).abs() < 0.01),
                    "[{AWARE_036}] burst {} (no sync truth): trusted {t:.0}",
                    r.index
                ),
            }
            trusted_right.push(r.index);
        }
        if let (Some(d), Some(truth)) = (r.deviation, r.dev) {
            assert!(
                (d / truth - 1.0).abs() < 0.10,
                "[{AWARE_036}] burst {}: deviation {d:.0} vs reference {truth:.0}",
                r.index
            );
            dev_checked += 1;
        }
        if r.rate.is_none() && r.snr < 0.0 {
            assert!(
                r.trusted.is_none() && r.family == Family::Unknown,
                "[{AWARE_036}] sliver {} at {:.1} dB",
                r.index,
                r.snr
            );
        }
    }
    eprintln!(
        "[{AWARE_036}] 915 MHz: {} unique trusted-and-right bursts {:?}, {} deviations checked",
        trusted_right.len(),
        trusted_right,
        dev_checked
    );
    for must in [3, 9] {
        assert!(
            trusted_right.contains(&must) || (must == 9 && trusted_right.contains(&10)),
            "[{AWARE_036}] ≥ 20 dB burst {must} not trusted"
        );
    }
    assert!(dev_checked >= 2, "[{AWARE_036}] deviations checked");
}

/// Detection box of the RDS subcarrier, Hz.
const RDS_BOX_HZ: f64 = 5_000.0;

#[test]
fn aware_036_blind_rds_chip_rate_windows() {
    let (meta, data) = fixture_or_skip!(FM);
    let fx = Fixture::load(&meta).unwrap();
    let fs = fx.sample_rate;
    let prov = meta_provenance(&meta);
    let center = prov.tune.center_hz;
    let station = fx.of_kind("wfm-broadcast")[0];
    let pilot = station.expect_f64("/pilot/frequency_hz");
    let chip = pilot / 8.0; // 2·Rs, Rs = pilot/16 in the capture clock
    let iq = read_ci8(&data);

    let mpx_fs = 240e3;
    let mut ddc = Ddc::new(
        DdcSpec::new(station.center_hz() - center, 220e3).with_output_rate(mpx_fs),
        fs,
    )
    .unwrap();
    let block = ddc.process(common::info(0, &prov), &iq).unwrap();
    let scale = mpx_fs / std::f64::consts::TAU / 150e3;
    let mpx: Vec<Complex32> = block
        .samples
        .windows(2)
        .map(|w| Complex32::new((f64::from((w[1] * w[0].conj()).arg()) * scale) as f32, 0.0))
        .collect();
    let spec = hk_dsp::welch(&mpx, mpx_fs, 0.0, &hk_dsp::WelchConfig::new(16_384)).unwrap();
    let gap = |lo: f64, hi: f64| {
        let bins: Vec<f64> = (0..spec.bins())
            .filter(|&k| (lo..=hi).contains(&spec.bin_offset_hz(k)))
            .map(|k| f64::from(spec.psd[k]))
            .collect();
        bins.iter().sum::<f64>() / bins.len() as f64
    };
    let floor = 0.5 * (gap(53_200.0, 54_400.0) + gap(59_600.0, 61_000.0));
    let mpx_prov = provenance(0.0, mpx_fs);
    // C14 wants samples_per_obw × the box width (BlindConfig::normalise_config docs).
    let mut ex = hk_estimate::SnippetExtractor::new(hk_estimate::SnippetConfig {
        min_rate_hz: BlindConfig::default().samples_per_obw * RDS_BOX_HZ,
        ..Default::default()
    });
    let req = SnippetRequest {
        start_index: 0,
        end_index: mpx.len() as u64,
        center_offset_hz: 56_700.0,
        bandwidth_hz: RDS_BOX_HZ,
    };
    let snip = ex.extract(common::info(0, &mpx_prov), &mpx, &req).unwrap();
    let hints = Hints {
        family: FamilyHint::Dsb,
        noise: Some(NoiseReference {
            density: floor,
            sigma_db: 0.3,
        }),
        ..Default::default()
    };
    let mut c13 = hk_estimate::ParamEstimator::default();
    let ps = c13.estimate(&snip, &hints);
    eprintln!(
        "[{AWARE_036}] RDS C13: obw {:?} cfo {:?} snr ext {:?} box {:?} extent {:?} snippet {} at {:.0}",
        ps.obw99_hz,
        ps.cfo_hz,
        ps.snr_extent_db,
        ps.snr_box_db,
        ps.extent,
        snip.samples.len(),
        snip.sample_rate_hz
    );
    let mut blind = BlindEstimator::new(BlindConfig::default());
    let prepared = blind.prepare(&snip, &ps).expect("prepare RDS");
    eprintln!(
        "[{AWARE_036}] RDS: CFO {:?} (truth {:.2}), OBW99 {:?}, SNR ext {:?} box {:?}, C14 window \
         {} samples at {:.0} Hz, bandwidth {:.0} from {:?}, chip truth {chip:.3}",
        ps.cfo_hz,
        3.0 * pilot - 56_700.0,
        ps.obw99_hz.value(),
        ps.snr_extent_db.value(),
        ps.snr_box_db.value(),
        prepared.samples.len(),
        prepared.sample_rate_hz,
        prepared.obw_hz,
        prepared.obw_source
    );
    let input = prepared.input();
    for w_s in [0.25, 0.5, 1.0] {
        let w = (w_s * prepared.sample_rate_hz) as usize;
        let windows = prepared.samples.len() / w;
        let (mut ok, mut trusted, mut alt, mut cost) = (0, 0, 0, 0u64);
        for k in 0..windows {
            let s = blind.estimate(&input.window(k * w..(k + 1) * w));
            cost += s.cost_us;
            let best = s.best_candidate_bd();
            let right = best.is_some_and(|r| (r / chip - 1.0).abs() < 0.01);
            ok += usize::from(right);
            trusted += usize::from(s.rate_trusted());
            let has_alt = s
                .harmonic_alternatives_bd
                .iter()
                .any(|a| (a / (chip / 2.0) - 1.0).abs() < 0.01);
            alt += usize::from(has_alt);
            if !(right && s.rate_trusted() && has_alt) || k == 0 {
                eprintln!(
                    "[{AWARE_036}] RDS {w_s} s window {k}: rate {:?} best {best:?} alts {:?} fam {:?} \
                     reasons {:?} lines {:?}",
                    s.symbol_rate_bd.value(),
                    s.harmonic_alternatives_bd,
                    s.family,
                    s.reasons,
                    s.lines
                        .iter()
                        .map(|l| (
                            l.freq_hz.map(|f| (f * 10.0).round() / 10.0),
                            l.significance_db.round()
                        ))
                        .collect::<Vec<_>>()
                );
            }
            if s.rate_trusted() {
                assert!(
                    s.symbol_rate_bd
                        .value()
                        .is_some_and(|r| (r / chip - 1.0).abs() < 0.01),
                    "[{AWARE_036}] RDS window {k} trusted and wrong: {:?}",
                    s.symbol_rate_bd
                );
            }
        }
        eprintln!(
            "[{AWARE_036}] RDS {w_s} s: {windows} windows, within 1 % {ok}, trusted {trusted}, ½ \
             alternative {alt}, C14 {:.2} ms/window",
            cost as f64 / 1e3 / windows as f64
        );
        assert!(windows >= 4);
        assert_eq!(
            ok, windows,
            "[{AWARE_036}] RDS {w_s} s: best candidate within 1 %"
        );
        assert_eq!(
            alt, windows,
            "[{AWARE_036}] RDS {w_s} s: 1187.5 Bd not offered"
        );
        // Trust floors (module docs): in some sub-second windows of this segment the envelope
        // line sits under the 14 dB two-group rule; those abstain, they never mislead.
        let floor = if w_s >= 1.0 { 1.0 } else { 0.85 };
        assert!(
            trusted as f64 >= floor * windows as f64,
            "[{AWARE_036}] RDS {w_s} s: {trusted}/{windows} trusted, floor {floor}"
        );
    }
}

#[test]
fn aware_036_blind_fm_broadcast_is_untrusted() {
    let (meta, data) = fixture_or_skip!(FM);
    let fx = Fixture::load(&meta).unwrap();
    let fs = fx.sample_rate;
    let prov = meta_provenance(&meta);
    let center = prov.tune.center_hz;
    let station = fx.of_kind("wfm-broadcast")[0];
    let iq = read_ci8(&data);
    let mut chain = Chain::default();
    let win = (0.02 * fs) as u64;
    let (mut n, mut labelled) = (0, 0);
    for k in 0..24u64 {
        let start = (0.1 * fs) as u64 + k * (0.2 * fs) as u64;
        let req = SnippetRequest {
            start_index: start,
            end_index: start + win,
            center_offset_hz: station.center_hz() - center,
            bandwidth_hz: station.bandwidth_hz(),
        };
        let o = chain.run(&iq, &prov, &req, &Hints::default());
        n += 1;
        labelled += usize::from(o.family() != Family::Unknown);
        eprintln!("[{AWARE_036}] FM window {k}: {}", describe(&o));
        assert!(
            o.trusted_rate().is_none(),
            "[{AWARE_036}] analog FM window {k} got a trusted rate"
        );
    }
    eprintln!("[{AWARE_036}] FM broadcast: {n} windows, 0 trusted, {labelled} labelled");
}
