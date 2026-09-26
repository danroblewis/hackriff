//! Shared blind-station support for the T-970 FM fixtures (`ota_fm_stations.rs`).
//!
//! Nothing here reads `hackriff:truth` except [`truth_stations`], which the asserting test calls
//! after the run: the station search below sees only the measured spectrum and the capture's
//! `hackriff:provenance`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use hk_classify::{Classifier, ClassifyRequest, SymbolEstimator};
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::{InputInfo, WelchConfig, WindowKind, welch};
use hk_estimate::{Hints, ParamEstimator, SnippetExtractor, SnippetRequest};
use hk_model::classify::Classification;
use hk_model::{Provenance, SampleTime, Timestamp};
use num_complex::{Complex, Complex32};

/// The four committed FM captures, by directory and stem. A fixture whose Git-LFS data is not
/// fetched is skipped (or fails, under `HK_REQUIRE_FIXTURES=1`).
const FM_FIXTURES: &[(&str, &str)] = &[
    (
        "fixtures/hackrf/2026-09-13",
        "fm_100p8M_2p4M_l32g30a1_t1p5_5s",
    ),
    ("fixtures/hackrf/capture-2026-09-15-fm-band", "iq"),
    ("fixtures/hackrf/explorer-2026-09-25", "fm-101p3-pi1694"),
    ("fixtures/hackrf/explorer-2026-09-25", "fm-98p9-piA4FF"),
];

/// One fixture's capture: the samples the search runs on, and where they came from.
pub struct FmFixture {
    pub name: String,
    pub meta: PathBuf,
    pub prov: ProvenanceHandle,
    pub fs: f64,
    pub center_hz: f64,
    pub iq: Vec<Complex<i8>>,
}

/// One station found blind, and what the classifier made of it.
pub struct Station {
    pub offset_hz: f64,
    pub bandwidth_hz: f64,
    pub class: Classification,
}

/// A truth station, read only by the assertions.
pub struct TruthStation {
    pub center_hz: f64,
    /// The in-band SNR the fixture's own analysis measured, where it recorded one.
    pub snr_db: Option<f64>,
}

/// A truth annotation that is **not** a broadcast station: an artefact, or an emission the
/// fixture's analysis measured to carry no pilot. Read only by the assertions.
pub struct TruthOther {
    pub center_hz: f64,
    pub kind: String,
}

fn repo_root() -> PathBuf {
    hk_e2e::paths::repo_root()
}

/// Every FM fixture whose data is fetched.
pub fn fm_fixtures() -> Vec<FmFixture> {
    let mut out = Vec::new();
    for (dir, stem) in FM_FIXTURES {
        let meta = repo_root().join(dir).join(format!("{stem}.sigmf-meta"));
        let data = meta.with_extension("sigmf-data");
        let fetched = std::fs::metadata(&data).is_ok_and(|m| m.len() > 4096);
        if !meta.is_file() || !fetched {
            if std::env::var("HK_REQUIRE_FIXTURES").is_ok_and(|v| v == "1") {
                panic!("{stem}: fixture data not fetched (git lfs pull)");
            }
            eprintln!("SKIP {stem}: fixture data is not fetched (git lfs pull)");
            continue;
        }
        let prov = provenance(&meta);
        let fs = prov.get().tune.sample_rate_hz;
        let center_hz = prov.get().tune.center_hz;
        // Two seconds is plenty for a continuous emission and keeps the 19 kHz line's Welch
        // average honest (the pilot is a steady tone, so the segment count only reduces variance).
        let want = (2.0 * fs) as usize;
        let iq = read_ci8(&data, want);
        out.push(FmFixture {
            name: (*stem).to_owned(),
            meta,
            prov,
            fs,
            center_hz,
            iq,
        });
    }
    out
}

fn provenance(meta: &Path) -> ProvenanceHandle {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(meta).unwrap()).unwrap();
    let p: Provenance = serde_json::from_value(v["global"]["hackriff:provenance"].clone()).unwrap();
    ProvenanceHandle::new(p)
}

fn read_ci8(path: &Path, max: usize) -> Vec<Complex<i8>> {
    std::fs::read(path)
        .expect("read .sigmf-data")
        .chunks_exact(2)
        .take(max)
        .map(|c| Complex::new(c[0] as i8, c[1] as i8))
        .collect()
}

fn to_cf32(iq: &[Complex<i8>]) -> Vec<Complex32> {
    iq.iter()
        .map(|s| Complex32::new(f32::from(s.re) / 127.0, f32::from(s.im) / 127.0))
        .collect()
}

fn info(prov: &ProvenanceHandle) -> InputInfo<'_> {
    InputInfo {
        time: SampleTime {
            sample_index: 0,
            host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
        },
        discontinuity: Discontinuity::STREAM_START,
        dropped_before: 0,
        provenance: prov,
    }
}

/// A broadcast FM emission occupies 120-200 kHz inside its 200 kHz channel. The search accepts a
/// wider range than that so a real station measured through a truncating baseband filter, or one
/// whose skirts overlap a neighbour, is still found; what it will not accept is a line or a
/// band-wide blob.
const MIN_STATION_HZ: f64 = 60e3;
const MAX_STATION_HZ: f64 = 340e3;
/// A direct-conversion front end always puts its LO leakage and ADC offset at offset zero; the
/// product's own detector refuses such a box (`hk_detect::rules::spur_decision`).
const DC_GUARD_HZ: f64 = 30e3;
/// How far a peak must stand over the capture's own noise floor to be a candidate, dB.
const PEAK_OVER_FLOOR_DB: f64 = 4.0;

/// Every station-shaped emission in the capture, found blind from the measured spectrum.
///
/// Local maxima of a smoothed mean periodogram that stand [`PEAK_OVER_FLOOR_DB`] over the
/// capture's own median floor, each grown outwards to the **valley** between it and its
/// neighbours — which is what separates two adjacent stations, where a single -20 dB contour from
/// the strongest bin swallows both. Nothing here reads a channel raster or a truth annotation.
pub fn stations(fx: &FmFixture) -> Vec<Station> {
    let cf = to_cf32(&fx.iq);
    let cfg = WelchConfig {
        fft_len: 2048,
        overlap: 1024,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: false,
    };
    let s = welch(&cf, fx.fs, 0.0, &cfg).expect("welch");
    let raw: Vec<f64> = s.psd.iter().map(|v| f64::from(*v)).collect();
    let bin = s.bin_width_hz();
    // ~10 kHz of smoothing: enough to walk a station's shoulder without tripping on its noise.
    let half = ((5e3 / bin) as usize).max(1);
    let psd: Vec<f64> = (0..raw.len())
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + half).min(raw.len() - 1);
            raw[lo..=hi].iter().sum::<f64>() / (hi - lo + 1) as f64
        })
        .collect();
    let mut sorted = psd.clone();
    sorted.sort_by(f64::total_cmp);
    let floor = sorted[sorted.len() / 2].max(1e-30);
    let threshold = floor * 10f64.powf(PEAK_OVER_FLOOR_DB / 10.0);

    let away = |i: usize| s.bin_offset_hz(i).abs() >= DC_GUARD_HZ;
    let mut peaks: Vec<usize> = (1..psd.len() - 1)
        .filter(|i| away(*i) && psd[*i] >= threshold)
        .filter(|i| psd[*i] >= psd[i - 1] && psd[*i] > psd[i + 1])
        .collect();
    peaks.sort_by(|a, b| psd[*b].total_cmp(&psd[*a]));

    let mut boxes: Vec<(usize, usize)> = Vec::new();
    for peak in peaks {
        if boxes.iter().any(|(lo, hi)| (*lo..=*hi).contains(&peak)) {
            continue;
        }
        // Walk downhill to the valley either side: stop where the spectrum turns back up after
        // falling at least `EDGE_DB` from the peak, or where it reaches the floor.
        const EDGE_DB: f64 = 12.0;
        let edge = psd[peak] / 10f64.powf(EDGE_DB / 10.0);
        let mut lo = peak;
        while lo > 0 && away(lo - 1) && !boxes.iter().any(|(a, b)| (*a..=*b).contains(&(lo - 1))) {
            if psd[lo - 1] > psd[lo] && psd[lo] < edge {
                break;
            }
            if psd[lo - 1] < threshold {
                break;
            }
            lo -= 1;
        }
        let mut hi = peak;
        while hi + 1 < psd.len()
            && away(hi + 1)
            && !boxes.iter().any(|(a, b)| (*a..=*b).contains(&(hi + 1)))
        {
            if psd[hi + 1] > psd[hi] && psd[hi] < edge {
                break;
            }
            if psd[hi + 1] < threshold {
                break;
            }
            hi += 1;
        }
        let width = (hi - lo + 1) as f64 * bin;
        if (MIN_STATION_HZ..=MAX_STATION_HZ).contains(&width) {
            boxes.push((lo, hi));
        }
    }

    boxes
        .into_iter()
        .map(|(lo, hi)| {
            (
                0.5 * (s.bin_offset_hz(lo) + s.bin_offset_hz(hi)),
                ((hi - lo + 1) as f64 * bin).max(2.0 * bin),
            )
        })
        .filter_map(|(offset_hz, bandwidth_hz)| {
            classify_box(fx, offset_hz, bandwidth_hz).map(|class| Station {
                offset_hz,
                bandwidth_hz,
                class,
            })
        })
        .collect()
}

/// The station in `found` nearest `rf_center_hz`, within half a broadcast channel.
pub fn station_on<'a>(
    fx: &FmFixture,
    found: &'a [Station],
    rf_center_hz: f64,
) -> Option<&'a Station> {
    found
        .iter()
        .filter(|st| (fx.center_hz + st.offset_hz - rf_center_hz).abs() <= 100e3)
        .min_by(|a, b| {
            (fx.center_hz + a.offset_hz - rf_center_hz)
                .abs()
                .total_cmp(&(fx.center_hz + b.offset_hz - rf_center_hz).abs())
        })
}

/// Runs one blind box through C13 + C14 and the classifier, exactly as `hk_pipeline::classify`
/// does.
fn classify_box(fx: &FmFixture, offset_hz: f64, bandwidth_hz: f64) -> Option<Classification> {
    let request = SnippetRequest {
        start_index: 0,
        end_index: fx.iq.len() as u64,
        center_offset_hz: offset_hz,
        bandwidth_hz,
    };
    let mut extractor = SnippetExtractor::new(Default::default());
    let snip = extractor.extract(info(&fx.prov), &fx.iq, &request).ok()?;
    let params = ParamEstimator::new(Default::default()).estimate(&snip, &Hints::default());
    let normalised = hk_estimate::normalise::normalise(&snip, &params, &Default::default()).ok()?;
    let window = SymbolEstimator::new().window_from_snippet(&snip, &params);
    let symbols = window.as_ref().map(|w| w.params.clone());
    let mut req = ClassifyRequest::new(
        &normalised.samples,
        normalised.sample_rate_hz,
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
    );
    req.obw_hz = params.obw99_hz.value();
    req.snr_db = params
        .snr_extent_db
        .value()
        .or_else(|| params.snr_box_db.value());
    req.suspect.clipped = params.flags.clipped;
    req.suspect.spur = params.flags.overload;
    req.symbols = symbols.as_ref();
    req.symbol_samples = window.as_ref().map(|w| w.samples.as_slice());
    req.symbol_sample_rate_hz = window.as_ref().map(|w| w.sample_rate_hz);
    Some(Classifier::new().classify(&req))
}

/// The fixture's annotated broadcast stations. **Truth: assertions only.**
pub fn truth_stations(fx: &FmFixture) -> Vec<TruthStation> {
    let f = hk_e2e::Fixture::load(&fx.meta).unwrap();
    f.emissions()
        .into_iter()
        .filter(|t| {
            t.str("kind")
                .is_some_and(|k| k.starts_with("wfm-broadcast"))
        })
        .map(|t| TruthStation {
            center_hz: t.center_hz(),
            snr_db: t.f64("snr_db"),
        })
        .collect()
}

/// The fixture's annotated non-stations: artefacts, and any emission that is not a broadcast
/// station. **Truth: assertions only.**
pub fn non_station_truth(fx: &FmFixture) -> Vec<TruthOther> {
    let f = hk_e2e::Fixture::load(&fx.meta).unwrap();
    f.emissions()
        .into_iter()
        .filter(|t| {
            !t.str("kind")
                .is_some_and(|k| k.starts_with("wfm-broadcast"))
        })
        .chain(f.artefacts())
        .filter_map(|t| {
            let kind = t.str("kind")?.to_owned();
            let center_hz = t.f64("center_hz")?;
            Some(TruthOther { center_hz, kind })
        })
        .collect()
}

/// Classifies one fixed-width window at an absolute frequency (the negative controls). `None`
/// when the window falls outside the capture.
pub fn classify_window(
    fx: &FmFixture,
    rf_center_hz: f64,
    bandwidth_hz: f64,
) -> Option<Classification> {
    let offset = rf_center_hz - fx.center_hz;
    if offset.abs() + bandwidth_hz > 0.45 * fx.fs {
        return None;
    }
    classify_box(fx, offset, bandwidth_hz)
}
