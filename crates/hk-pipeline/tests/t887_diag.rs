//! T-887 scratch diagnostic (not for merge): the device path's `cyclic_db` / `sigma_aa` against
//! the dev grid's.

mod common;

use common::*;
use hk_classify::SymbolEstimator;
use hk_classify::density::DensityModel;
use hk_classify::features::{FeatureInput, features};
use hk_classify::synth::{Class, SynthConfig, generate};
use hk_core::Discontinuity;
use hk_core::{Pacing, Source};
use hk_dsp::stft::InputInfo;
use hk_e2e::SynthRequest;
use hk_estimate::SnippetRequest;
use hk_model::{FreqRange, Region, SampleTime, TimeRange, Timestamp};
use hk_pipeline::classify::measure_box;
use hk_pipeline::open_replay;
use num_complex::Complex32;

fn z(model: &DensityModel, class: &str, name: &str, v: f64) -> String {
    model
        .classes
        .iter()
        .filter(|c| c.class == class)
        .map(|c| {
            let d = c.dims.iter().find(|d| d.feature == name).unwrap();
            format!("{:+.2}", (v - d.mean) / d.sigma)
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn lines(p: &hk_estimate::blind::SymbolParameters) -> String {
    p.lines
        .iter()
        .map(|l| format!("{:?}:{:.1}", l.method, l.significance_db))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn t887_device_rows() {
    let model = DensityModel::builtin();
    let mut c14 = SymbolEstimator::new();
    let pad_env: f64 = std::env::var("T887_PAD_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.002);
    for snr_db in [25.0, 30.0] {
        for seed in 852u64..856 {
            let out = SynthRequest::new("fsk_burst_train")
                .seed(seed)
                .param("snr_db", snr_db)
                .param("duration_s", 1.2)
                .generate()
                .expect("scene synthesises");
            let fixture = out.fixture(0).unwrap();
            let meta = fixture.meta_path.clone();
            let input = TempDir::new("t887");
            let mut source = open_replay(&blind_meta(&meta, &input.0), Pacing::Unpaced, false)
                .unwrap()
                .source;
            let mut iq: Vec<Complex32> = Vec::new();
            let mut block = Vec::new();
            let mut first = None;
            while let Some(h) = source.read_block(&mut block).unwrap() {
                first.get_or_insert_with(|| h.clone());
                iq.extend_from_slice(&block);
            }
            let head = first.unwrap();
            let tune = head.provenance.tune.clone();
            let fs = tune.sample_rate_hz;
            let s0 = head.first_sample();
            let bursts: Vec<(u64, u64, f64, f64)> = fixture
                .emissions()
                .iter()
                .map(|t| {
                    let a = s0 + t.sample_start;
                    (a, a + t.sample_count, t.f_lo_hz, t.f_hi_hz)
                })
                .collect();
            // The pipeline's own detections, grouped per burst as the fsk chain merges them.
            let dir = TempDir::new("t887-run");
            let (cfg, replay, _inp) =
                blind_replay_config(&dir.0, &meta, serde_json::json!({}), Pacing::Unpaced);
            let sum = start(cfg, replay).wait().unwrap();
            assert!(sum.errors.is_empty());
            let repo = repo(&dir.0);
            let all = Region::new(
                FreqRange::new(0.0, 1e12),
                TimeRange::new(
                    Timestamp::from_unix_nanos(i64::MIN / 2),
                    Timestamp::from_unix_nanos(i64::MAX / 2),
                ),
            );
            let mut groups: Vec<Option<SnippetRequest>> = vec![None; bursts.len()];
            for det in repo.detections_in_region(&all).unwrap() {
                let r = SnippetRequest::from_detection(&det, head.time, &tune);
                let (f_lo, f_hi) = (
                    det.f_center_hz - det.obw_hz / 2.0,
                    det.f_center_hz + det.obw_hz / 2.0,
                );
                let Some(k) = bursts.iter().position(|&(a, b, lo, hi)| {
                    r.start_index < b && a < r.end_index && f_lo < hi && lo < f_hi
                }) else {
                    continue;
                };
                let g = groups[k].get_or_insert(r);
                let (lo, hi) = (
                    (tune.center_hz + g.center_offset_hz - g.bandwidth_hz / 2.0).min(f_lo),
                    (tune.center_hz + g.center_offset_hz + g.bandwidth_hz / 2.0).max(f_hi),
                );
                *g = SnippetRequest {
                    start_index: g.start_index.min(r.start_index),
                    end_index: g.end_index.max(r.end_index),
                    center_offset_hz: 0.5 * (lo + hi) - tune.center_hz,
                    bandwidth_hz: (hi - lo).max(1.0),
                };
            }
            for (k, (&(ta, tb, _, _), request)) in bursts.iter().zip(&groups).enumerate() {
                let Some(request) = request.clone() else { continue };
                let (a0, b0) = (request.start_index, request.end_index);
                let pad = (pad_env * fs) as u64;
                let a = a0.saturating_sub(pad).max(s0);
                let b = (b0 + pad).min(s0 + iq.len() as u64);
                let info = InputInfo {
                    time: SampleTime {
                        sample_index: a,
                        host_time: head.time.time_of(a, fs),
                    },
                    discontinuity: Discontinuity::NONE,
                    dropped_before: 0,
                    provenance: &head.provenance,
                };
                let slice = &iq[(a - s0) as usize..(b - s0) as usize];
                let Some((snippet, params)) = measure_box(info, slice, &request) else {
                    eprintln!("[T887] {snr_db} {seed} #{k}: no snippet");
                    continue;
                };
                let window = c14.window_from_snippet(&snippet, &params);
                let (es, ee) = params
                    .extent
                    .map(|e| (e.source_start - ta as f64, e.source_end - tb as f64))
                    .unwrap_or((f64::NAN, f64::NAN));
                let norm =
                    hk_estimate::normalise::normalise(&snippet, &params, &Default::default())
                        .unwrap();
                let obw = params.obw99_hz.value();
                let snr = params
                    .snr_extent_db
                    .value()
                    .or_else(|| params.snr_box_db.value());
                let f = features(&FeatureInput {
                    samples: &norm.samples,
                    sample_rate_hz: norm.sample_rate_hz,
                    obw_hz: obw,
                    snr_db: snr,
                    symbols: window.as_ref().map(|w| &w.params),
                });
                let cyc = f.get("cyclic_db").unwrap_or(f64::NAN);
                let saa = f.get("sigma_aa").unwrap_or(f64::NAN);
                let (wn, wfs, rate, h, ln) = window
                    .as_ref()
                    .map(|w| {
                        (
                            w.samples.len(),
                            w.sample_rate_hz,
                            w.params.symbol_rate_bd.value().unwrap_or(f64::NAN),
                            w.params.mod_index_h.value().unwrap_or(f64::NAN),
                            lines(&w.params),
                        )
                    })
                    .unwrap_or((0, 0.0, f64::NAN, f64::NAN, String::new()));
                // Re-estimate on the window's own samples through the dev grid's entry point.
                let alt = window.as_ref().and_then(|w| {
                    c14.from_samples(&w.samples, w.sample_rate_hz, obw, snr)
                });
                let alt_best = alt
                    .as_ref()
                    .map(|p| {
                        p.lines
                            .iter()
                            .map(|l| l.significance_db)
                            .fold(f64::NEG_INFINITY, f64::max)
                    })
                    .unwrap_or(f64::NAN);
                eprintln!(
                    "[T887] {snr_db} {seed} #{k}: ext err {:+.0}/{:+.0} src samples | snr {:.1} obw {:.0} norm n {} fs {:.0} | win n {} fs {:.0} sym {:.0} rate {:.0} h {:.2} | cyclic_db {:.2} (z {}) from_samples {:.2} | sigma_aa {:.4} (z {}) env_cv {:.4} | {}",
                    es,
                    ee,
                    snr.unwrap_or(f64::NAN),
                    obw.unwrap_or(f64::NAN),
                    norm.samples.len(),
                    norm.sample_rate_hz,
                    wn,
                    wfs,
                    wn as f64 / wfs * rate,
                    rate,
                    h,
                    cyc,
                    z(model, "2fsk", "cyclic_db", cyc),
                    alt_best,
                    saa,
                    z(model, "2fsk", "sigma_aa", saa),
                    f.get("env_cv").unwrap_or(f64::NAN),
                    ln
                );
                if std::env::var_os("T887_DUMP").is_some() {
                    let dir = std::path::Path::new("/tmp/t887");
                    std::fs::create_dir_all(dir).unwrap();
                    let w = |name: String, v: &[Complex32]| {
                        let bytes: Vec<u8> = v
                            .iter()
                            .flat_map(|c| [c.re.to_le_bytes(), c.im.to_le_bytes()].concat())
                            .collect();
                        std::fs::write(dir.join(name), bytes).unwrap();
                    };
                    w(format!("dev_{snr_db}_{seed}_{k}_norm.cf32"), &norm.samples);
                    if let Some(win) = &window {
                        w(format!("dev_{snr_db}_{seed}_{k}_win.cf32"), &win.samples);
                    }
                }
            }
        }
    }
}

#[test]
fn t887_dev_grid_rows() {
    let model = DensityModel::builtin();
    let mut c14 = SymbolEstimator::new();
    for snr_db in [25.0, 30.0] {
        for seed in 0..60u64 {
            let s = generate(Class::Fsk2, &SynthConfig::new(snr_db, seed));
            let symbols = c14.from_samples(
                &s.symbol_samples,
                s.symbol_sample_rate_hz,
                Some(s.obw_hz),
                Some(snr_db),
            );
            let Some(p) = symbols.as_ref() else { continue };
            let h = p.mod_index_h.value().unwrap_or(f64::NAN);
            if !(h > 2.5) {
                continue;
            }
            let f = features(&FeatureInput {
                samples: &s.samples,
                sample_rate_hz: s.sample_rate_hz,
                obw_hz: Some(s.obw_hz),
                snr_db: Some(snr_db),
                symbols: Some(p),
            });
            let rate = p.symbol_rate_bd.value().unwrap_or(f64::NAN);
            let n = s.symbol_samples.len().min(65_536);
            let cyc = f.get("cyclic_db").unwrap_or(f64::NAN);
            let saa = f.get("sigma_aa").unwrap_or(f64::NAN);
            // The same emission's symbol view cut to the device burst's ~112 symbols.
            let cut = ((112.0 / rate) * s.symbol_sample_rate_hz) as usize;
            let short = c14.from_samples(
                &s.symbol_samples[..cut.min(s.symbol_samples.len())],
                s.symbol_sample_rate_hz,
                Some(s.obw_hz),
                Some(snr_db),
            );
            let short_best = short
                .as_ref()
                .map(|p| {
                    p.lines
                        .iter()
                        .map(|l| l.significance_db)
                        .fold(f64::NEG_INFINITY, f64::max)
                })
                .unwrap_or(f64::NAN);
            // And the classifier snippet cut likewise, for sigma_aa.
            let ncut = ((112.0 / rate) * s.sample_rate_hz) as usize;
            let fs_short = features(&FeatureInput {
                samples: &s.samples[..ncut.min(s.samples.len())],
                sample_rate_hz: s.sample_rate_hz,
                obw_hz: Some(s.obw_hz),
                snr_db: Some(snr_db),
                symbols: short.as_ref(),
            });
            eprintln!(
                "[T887-grid] {snr_db} seed {seed}: obw {:.0} norm n {} fs {:.0} | win n {} fs {:.0} sym {:.0} rate {:.0} h {:.2} | cyclic_db {:.2} (z {}) at 112 sym {:.2} | sigma_aa {:.4} (z {}) at 112 sym {:.4} | {}",
                s.obw_hz,
                s.samples.len(),
                s.sample_rate_hz,
                n,
                s.symbol_sample_rate_hz,
                n as f64 / s.symbol_sample_rate_hz * rate,
                rate,
                h,
                cyc,
                z(model, "2fsk", "cyclic_db", cyc),
                short_best,
                saa,
                z(model, "2fsk", "sigma_aa", saa),
                fs_short.get("sigma_aa").unwrap_or(f64::NAN),
                lines(p)
            );
        }
    }
}
