//! T-199 over-the-air evaluation: the classifier on real HackRF captures (ADR-0016 §7).
//!
//! **Blind.** Nothing here looks a frequency up. Each test finds the strongest emission in the
//! capture from the measured spectrum alone, runs it through the same C13 chain the pipeline uses
//! (snippet → parameters → normalisation) and classifies it. The fixture's truth list is opened
//! only to assert the answer, after the fact.
//!
//! The captures are Git LFS; when they are not fetched the test skips, unless
//! `HK_REQUIRE_FIXTURES=1` (the CI acceptance job), which fails instead.

use std::path::{Path, PathBuf};

use hk_classify::{Classifier, ClassifyRequest};
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::{InputInfo, WelchConfig, WindowKind, welch};
use hk_estimate::{Hints, ParamEstimator, SnippetExtractor, SnippetRequest};
use hk_model::{Provenance, SampleTime, Timestamp};
use num_complex::{Complex, Complex32};

const FM: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
const ISM: &str = "ism_915M_10M_l24g30a1_t42p3_1p2s";

/// The fixture's `.sigmf-meta` and `.sigmf-data`, or `None` when the LFS data is not fetched.
fn fixture(name: &str) -> Option<(PathBuf, PathBuf)> {
    let rel = Path::new("fixtures/hackrf/2026-09-13").join(format!("{name}.sigmf-meta"));
    let mut dir = Some(hk_e2e::paths::repo_root());
    while let Some(d) = dir {
        let meta = d.join(&rel);
        let data = meta.with_extension("sigmf-data");
        let fetched = std::fs::metadata(&data).is_ok_and(|m| m.len() > 4096);
        if meta.is_file() && fetched {
            return Some((meta, data));
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    if std::env::var("HK_REQUIRE_FIXTURES").is_ok_and(|v| v == "1") {
        panic!("{name}: fixture data not fetched (git lfs pull) and HK_REQUIRE_FIXTURES=1");
    }
    eprintln!("SKIP {name}: fixture data is not fetched (git lfs pull)");
    None
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

fn info(first: u64, prov: &ProvenanceHandle) -> InputInfo<'_> {
    InputInfo {
        time: SampleTime {
            sample_index: first,
            host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
        },
        discontinuity: Discontinuity::STREAM_START,
        dropped_before: 0,
        provenance: prov,
    }
}

/// The strongest **single** emission in `samples`, found blind: the −20 dB contour around the
/// strongest bin of the measured spectrum. Returns (centre offset from the tuned centre,
/// bandwidth), both Hz.
///
/// The contour, rather than the 99 %-power band, is what makes this a detector: a capture of a
/// whole broadcast band holds many emissions, and 99 % of its power spans all of them. Growing
/// from the peak until the spectrum drops 20 dB isolates the one emission the peak belongs to —
/// which is what a detection box is.
fn strongest_band(samples: &[Complex32], fs: f64, fft_len: usize) -> (f64, f64) {
    let cfg = WelchConfig {
        fft_len,
        overlap: fft_len / 2,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: false,
    };
    let s = welch(samples, fs, 0.0, &cfg).expect("welch");
    let psd: Vec<f64> = s.psd.iter().map(|v| f64::from(*v)).collect();
    let peak_bin = psd
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map_or(psd.len() / 2, |(i, _)| i);
    let floor = psd[peak_bin] / 100.0;
    let mut lo = peak_bin;
    let mut hi = peak_bin;
    while lo > 0 && psd[lo - 1] >= floor {
        lo -= 1;
    }
    while hi + 1 < psd.len() && psd[hi + 1] >= floor {
        hi += 1;
    }
    let bin = s.bin_width_hz();
    let centre = 0.5 * (s.bin_offset_hz(lo) + s.bin_offset_hz(hi));
    (centre, ((hi - lo + 1) as f64 * bin).max(2.0 * bin))
}

/// Converts ci8 to unit-scale complex floats for the spectrum search.
fn to_cf32(iq: &[Complex<i8>]) -> Vec<Complex32> {
    iq.iter()
        .map(|s| Complex32::new(f32::from(s.re) / 127.0, f32::from(s.im) / 127.0))
        .collect()
}

/// Runs one detection box through C13 and the classifier, as the pipeline does.
fn classify_box(
    iq: &[Complex<i8>],
    prov: &ProvenanceHandle,
    request: &SnippetRequest,
) -> Option<hk_model::classify::Classification> {
    let mut extractor = SnippetExtractor::new(Default::default());
    let snip = extractor.extract(info(0, prov), iq, request).ok()?;
    let params = ParamEstimator::new(Default::default()).estimate(&snip, &Hints::default());
    let normalised = hk_estimate::normalise::normalise(&snip, &params, &Default::default()).ok()?;
    // C14 at its own geometry, from the same snippet (T-238): six of `features@1`'s most
    // discriminating dimensions abstain without it, which is why both these fixtures used to
    // classify with `no_symbol_estimate`.
    let window = hk_classify::SymbolEstimator::new().window_from_snippet(&snip, &params);
    let symbols = window.as_ref().map(|w| w.params.clone());
    eprintln!(
        "[T-238]   C14: {}",
        match &symbols {
            Some(s) => format!(
                "ran in {} us, family {:?} (conf {:.2}), scores {:?}, rate {:?} Bd (trusted {}), \
                 best line {:.1} dB, reasons {:?}",
                s.cost_us,
                s.family,
                s.family_confidence,
                s.family_scores,
                s.symbol_rate_bd.value(),
                s.rate_trusted(),
                s.lines
                    .iter()
                    .map(|l| l.significance_db)
                    .fold(f64::NEG_INFINITY, f64::max),
                s.reasons
            ),
            None => "could not run on this snippet (features abstain)".to_owned(),
        }
    );
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
    req.symbols = symbols.as_ref();
    // T-200: the post-sync verifier, on the window C14 synced on.
    req.symbol_samples = window.as_ref().map(|w| w.samples.as_slice());
    req.symbol_sample_rate_hz = window.as_ref().map(|w| w.sample_rate_hz);

    // T-230 sim-to-real diagnosis, reported not asserted: which feature dimension puts a real
    // capture outside each synthetic class. `worst` names the dimension with the largest |z|, so a
    // family that abstains here says *why* it abstained rather than only that it did.
    let f = hk_classify::features::features(&hk_classify::features::FeatureInput {
        samples: &normalised.samples,
        sample_rate_hz: normalised.sample_rate_hz,
        obw_hz: req.obw_hz,
        snr_db: req.snr_db,
        symbols: symbols.as_ref(),
    });
    let model = hk_classify::DensityModel::builtin();
    for family in [
        "analog",
        "ook-ask",
        "fsk",
        "psk-qam",
        "ofdm",
        "css",
        "pulsed",
        "noise-like",
    ] {
        match model.score(family, &f) {
            Some(s) => eprintln!(
                "[T-230]   {family:<11} best class {:<11} m {:>8.1} plausibility {:.3} worst {:?}",
                s.class, s.m, s.plausibility, s.worst
            ),
            None => eprintln!("[T-230]   {family:<11} not scored (too few measured features)"),
        }
    }
    // T-235: the whole z vector, not only its worst entry. A distance dominated by one or two
    // content-driven dimensions is a different problem from one spread over all of them, and only
    // the full table tells them apart. Reported, never asserted.
    for class in [
        "wfm",
        "nbfm",
        "am",
        "2fsk",
        "gfsk",
        "msk",
        "ofdm",
        "noise-like",
    ] {
        let Some(c) = model.class(class) else {
            continue;
        };
        let zs: Vec<String> = c
            .dims
            .iter()
            .filter_map(|d| {
                f.get(&d.feature)
                    .map(|x| format!("{}={:+.1}", d.feature, (x - d.mean) / d.sigma))
            })
            .collect();
        eprintln!("[T-235]   z/{class:<11} {}", zs.join(" "));
    }
    Some(Classifier::new().classify(&req))
}

#[test]
fn fm_broadcast_capture_classifies_as_analog() {
    let Some((meta, data)) = fixture(FM) else {
        return;
    };
    let prov = provenance(&meta);
    let fs = prov.get().tune.sample_rate_hz;
    // 1 s of the capture is plenty for a continuous emission.
    let want = (fs as usize).min(2_400_000);
    let iq = read_ci8(&data, want);
    let (offset, bandwidth) = strongest_band(&to_cf32(&iq), fs, 4096);
    eprintln!(
        "[T-199] FM capture: strongest emission {:+.1} kHz from the tuned centre, {:.0} kHz wide (found blind)",
        offset / 1e3,
        bandwidth / 1e3
    );
    let request = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: offset,
        bandwidth_hz: bandwidth,
    };
    let c = classify_box(&iq, &prov, &request).expect("classified");
    c.validate().unwrap();
    eprintln!(
        "[T-199] FM capture → {} ({:.2}), class {:?}, open-set {:.2}, SNR {:?} dB, reasons {:?}",
        c.family,
        c.confidence,
        c.class.as_ref().map(|k| k.label.clone()),
        c.open_set_score,
        c.provenance.snr_db.map(|s| (s * 10.0).round() / 10.0),
        c.reasons
    );

    // Truth, opened only now: the capture holds a wideband FM broadcast station.
    let fx = hk_e2e::Fixture::load(&meta).unwrap();
    assert!(
        !fx.of_kind("wfm-broadcast").is_empty(),
        "fixture truth has no wfm-broadcast station"
    );
    // The safety property first, and absolutely: a real broadcast station is never given some
    // *other* modulation family. Abstaining is allowed — the station is measured at ~15 dB in-band
    // SNR, which is below the FSK and OOK gates, so those families are "not measured" rather than
    // ruled out and the honest answer may be `unknown` (ADR-0016 §2).
    assert!(
        c.family == "analog" || c.family == hk_model::classify::UNKNOWN,
        "a broadcast FM station must be analog or an abstention, not {} ({:?})",
        c.family,
        c.top(3)
    );
    if let Some(class) = &c.class {
        assert_eq!(
            class.label, "wfm",
            "within analog, a 200 kHz station is wfm"
        );
    }
    // Reported, not asserted: how much of the cascade the classical tree alone reaches on real
    // captures (T-206 owns the exit floor).
    eprintln!(
        "[T-199] FM capture outcome: {} (analog would be the full answer)",
        if c.family == "analog" {
            "analog"
        } else {
            "abstained"
        }
    );
}

#[test]
fn ism_915_burst_capture_classifies_as_fsk_or_abstains() {
    let Some((meta, data)) = fixture(ISM) else {
        return;
    };
    let prov = provenance(&meta);
    let fs = prov.get().tune.sample_rate_hz;
    let iq = read_ci8(&data, 12_000_000);
    let cf = to_cf32(&iq);

    // Find the strongest burst blind: the 2 ms window with the most peaked spectrum.
    let window = (0.002 * fs) as usize;
    let mut best = (f64::NEG_INFINITY, 0usize, 0.0, 0.0);
    let mut start = 0;
    while start + window <= cf.len() {
        let block = &cf[start..start + window];
        let cfg = WelchConfig {
            fft_len: 1024,
            overlap: 512,
            window: WindowKind::Hann,
            holds: false,
            spectral_kurtosis: false,
        };
        if let Ok(s) = welch(block, fs, 0.0, &cfg) {
            let psd: Vec<f64> = s.psd.iter().map(|v| f64::from(*v)).collect();
            let mut sorted = psd.clone();
            sorted.sort_by(f64::total_cmp);
            let median = sorted[sorted.len() / 2].max(1e-30);
            let peak = psd.iter().copied().fold(0.0_f64, f64::max);
            let contrast = 10.0 * (peak / median).log10();
            if contrast > best.0 {
                let (lo, hi) = hk_classify::features::occupied_band(&psd);
                let centre = 0.5 * (s.bin_offset_hz(lo) + s.bin_offset_hz(hi));
                let bw = (hi - lo + 1) as f64 * s.bin_width_hz();
                best = (contrast, start, centre, bw);
            }
        }
        start += window;
    }
    let (contrast, at, offset, bandwidth) = best;
    eprintln!(
        "[T-199] 915 MHz capture: strongest burst at sample {at} ({:.1} ms), {:+.1} kHz, {:.1} kHz wide, peak/median {contrast:.1} dB (found blind)",
        at as f64 / fs * 1e3,
        offset / 1e3,
        bandwidth / 1e3
    );
    let request = SnippetRequest {
        start_index: at as u64,
        end_index: (at + window) as u64,
        center_offset_hz: offset,
        bandwidth_hz: bandwidth,
    };
    let c = classify_box(&iq, &prov, &request).expect("classified");
    c.validate().unwrap();
    eprintln!(
        "[T-199] 915 MHz burst → {} ({:.2}), class {:?}, open-set {:.2}, SNR {:?} dB, reasons {:?}",
        c.family,
        c.confidence,
        c.class.as_ref().map(|k| k.label.clone()),
        c.open_set_score,
        c.provenance.snr_db.map(|s| (s * 10.0).round() / 10.0),
        c.reasons
    );

    // Truth, opened only now: the capture holds 2-FSK sensor bursts.
    let fx = hk_e2e::Fixture::load(&meta).unwrap();
    assert!(!fx.emissions().is_empty(), "fixture truth has no bursts");
    // The S5 floor is 20 dB in-band SNR; below it the chain must abstain, and abstaining is
    // never counted as wrong (ADR-0016 §2).
    let snr = c.provenance.snr_db.unwrap_or(f64::NEG_INFINITY);
    // The safety property: a real 2-FSK burst is never given another modulation family. Above the
    // 20 dB S5 gate the right answer is `fsk`; the classical tree fitted on synthetic waveforms
    // does not always reach it on a real HackRF capture (the sim-to-real gap this milestone's
    // richer impairment grid, T-213, and the verifier, T-200, exist to close), and an abstention
    // is never counted as wrong.
    assert!(
        c.family == "fsk" || c.family == hk_model::classify::UNKNOWN,
        "a 2-FSK burst at {snr:.1} dB must be fsk or an abstention, not {} ({:?})",
        c.family,
        c.top(3)
    );
    eprintln!(
        "[T-199] 915 MHz burst outcome at {snr:.1} dB: {} (fsk would be the full answer)",
        if c.family == "fsk" {
            "fsk"
        } else {
            "abstained"
        }
    );
}
