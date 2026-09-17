//! T-394: **a line at one frequency in channels holding nothing is the receiver's — test for
//! that, do not keep listing notches.**
//!
//! T-373 excluded the `fs/8192` gain step and T-382 excluded the host's 8 kHz comb and a
//! free-running ~655.75 Hz family; each exclusion revealed the next, a fourth family spaced
//! ~119.95 Hz was left underneath, and every one of the three was a **named frequency from one
//! fixture's provenance**. `hk_estimate::blind::receiver` replaces that strategy with the
//! measurement itself.
//!
//! Every test here runs with the fixture's recorded `capture_artefacts` **stripped**, so nothing
//! the discriminator finds can have come from a record. Four properties, in the order they matter:
//!
//! 1. **The property.** The survey recognises receiver-wide lines with no record naming them —
//!    demonstrated on the ~119.95 Hz family, which nobody has written down, and cross-checked
//!    against the three that had been.
//! 2. **The control, and the whole ticket.** The two stations' genuine 19 kHz pilot and 38 kHz
//!    subcarrier must survive. An over-eager device-local test that eats real subcarriers is worse
//!    than the artefact list it replaces, because it fails silently and in the direction of
//!    finding nothing.
//! 3. **Mutation.** With the discriminator off and no records either, the artefacts come straight
//!    back as the argmax — a regression test that cannot fail when the fix is removed proves
//!    nothing.
//! 4. **The null.** A synthetic capture with no receiver artefact yields **no** lines, so the
//!    survey is not manufacturing notches out of noise.

mod blind_support;
mod common;

use std::collections::BTreeSet;
use std::path::Path;

use hk_core::ProvenanceHandle;
use hk_dsp::synth::Rng;
use hk_estimate::blind::receiver::{ReceiverLines, SurveyConfig};
use hk_estimate::blind::{BlindEstimator, SymbolParameters};
use hk_estimate::{BlindConfig, Hints, SnippetRequest};
use hk_model::Provenance;
use num_complex::Complex;

use blind_support::{Chain, embed, gen_fsk};

/// The FM-band capture all four families were measured in.
const FIXTURE: &str = "fixtures/hackrf/capture-2026-09-15-fm-band";

/// Seconds surveyed and analysed. The survey's native cell is `M/(2·window)` Hz, so this sets how
/// finely a line is pinned (0.5 Hz at `M` = 32 over 2 s).
const SECONDS: f64 = 2.0;

/// Where in the recording. Not 0: the first samples of a capture carry the source's own start
/// transient, and a survey is a statement about a steady receiver state.
const START_S: f64 = 5.0;

/// The 19 kHz stereo pilot as **this receiver** samples it: 19 000 Hz read through a clock T-382
/// measured to run ~6.9 ppm fast is 18 999.87 Hz. Named here as a frequency to *look at*, never
/// looked up to tune to.
const PILOT_HZ: f64 = 18_999.87;

/// The 38 kHz stereo subcarrier, same clock.
const SUBCARRIER_HZ: f64 = 37_999.74;

/// How far a reported line may sit from a probe frequency and still be that line, Hz.
const NEAR_HZ: f64 = 6.0;

fn fixture(seconds: f64, start_s: f64) -> Option<(ProvenanceHandle, Vec<Complex<i8>>)> {
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
    let fs = prov.get().tune.sample_rate_hz;
    let skip = (start_s * fs) as usize * 2;
    let want = (seconds * fs) as usize;
    let bytes = std::fs::read(Path::new(&data)).expect("read .sigmf-data");
    let iq: Vec<Complex<i8>> = bytes[skip..]
        .chunks_exact(2)
        .take(want)
        .map(|c| Complex::new(c[0] as i8, c[1] as i8))
        .collect();
    Some((prov, iq))
}

/// The same capture with **every recorded artefact removed**: what this receiver looked like
/// before anyone measured it. Everything the discriminator reports under this provenance was found
/// by the measurement and by nothing else.
fn blind(prov: &ProvenanceHandle) -> ProvenanceHandle {
    let mut p: Provenance = prov.get().clone();
    p.capture_artefacts.clear();
    ProvenanceHandle::new(p)
}

fn info<'a>(prov: &'a ProvenanceHandle) -> hk_dsp::InputInfo<'a> {
    hk_dsp::InputInfo {
        time: hk_model::SampleTime {
            sample_index: 0,
            host_time: hk_model::Timestamp::from_unix_nanos(1_000_000_000),
        },
        discontinuity: hk_core::Discontinuity::STREAM_START,
        dropped_before: 0,
        provenance: prov,
    }
}

/// Surveys `iq` under `prov` and returns what was measured.
fn survey(
    prov: &ProvenanceHandle,
    iq: &[Complex<i8>],
    cfg: &SurveyConfig,
) -> Option<ReceiverLines> {
    let mut e = BlindEstimator::new(BlindConfig::default());
    e.survey_receiver_lines(info(prov), iq, cfg)
        .then(|| e.receiver_lines().expect("a survey").clone())
}

/// The strongest line of a C14 result, and its frequency.
fn argmax(s: &SymbolParameters) -> Option<(f64, f64)> {
    s.lines
        .iter()
        .filter_map(|l| l.freq_hz.map(|f| (f, l.significance_db)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// Whether `f` is within `NEAR_HZ` of `probe`.
fn near(f: f64, probe: f64) -> bool {
    (f - probe).abs() <= NEAR_HZ
}

/// Whether a record whose independent cell is `native_hz` wide would have **excluded** `f`: the
/// exclusion's own guard, which is the standard T-382 held its own measurement to.
fn excluded(r: &ReceiverLines, f: f64, native_hz: f64) -> bool {
    let g = r.guard_hz(native_hz);
    r.lines
        .iter()
        .any(|l| (f - l.freq_hz).abs() <= l.half_width_hz + g)
}

/// The recorded artefact combs of the *unmodified* fixture, for the cross-check only: T-394's
/// claim is that the measurement would have caught what those records name.
fn recorded_members(p: &Provenance, f_max: f64) -> Vec<f64> {
    let mut v = Vec::new();
    for c in p.cyclic_combs() {
        let top = c.harmonics.map_or((f_max / c.fundamental_hz).floor(), |m| {
            f64::from(m).min((f_max / c.fundamental_hz).floor())
        });
        for n in 1..=(top.max(0.0) as u32) {
            v.push(f64::from(n) * c.fundamental_hz);
        }
    }
    v.sort_by(f64::total_cmp);
    v
}

// -------------------------------------------------------------------------------------------
// 1. The property: recognised without a record naming it.
// -------------------------------------------------------------------------------------------

/// **The receiver's own lines are measured, not looked up.** With the fixture's provenance
/// stripped of every recorded artefact, the survey finds them anyway — including the ~119.95 Hz
/// family nobody has written down, which is therefore an honest blind target.
#[test]
fn t394_receiver_wide_lines_are_measured_without_a_record_naming_them() {
    let Some((prov, iq)) = fixture(SECONDS, START_S) else {
        return;
    };
    let cfg = SurveyConfig::default();
    let blind_prov = blind(&prov);
    assert!(blind_prov.get().capture_artefacts.is_empty());
    let r = survey(&blind_prov, &iq, &cfg).expect("the survey ran");
    eprintln!(
        "[T-394] survey: {} of {} channels hold nothing ({:.0} Hz spacing), band {:.0}..{:.0} Hz \
         at {:.3} Hz resolution; {} lines",
        r.reference_channels,
        r.surveyed_channels,
        r.channel_spacing_hz,
        r.band_hz.0,
        r.band_hz.1,
        r.resolution_hz,
        r.lines.len()
    );
    for l in &r.lines {
        eprintln!(
            "    {:10.2} Hz  {:6.2} dB  band +/-{:.2} Hz",
            l.freq_hz, l.significance_db, l.half_width_hz
        );
    }
    assert!(
        r.reference_channels >= cfg.min_reference_channels,
        "{} reference channels",
        r.reference_channels
    );
    assert!(!r.lines.is_empty(), "the survey found nothing at all");

    // --- The blind target: a family of lines spaced ~119.95 Hz, which no record names. ---
    // Found the way it would be found on an unknown receiver: an arithmetic progression among the
    // measured lines, with nothing consulted about what its spacing ought to be.
    let tol = 4.0 * r.resolution_hz;
    let freqs: Vec<f64> = r.frequencies().collect();
    let at = |f: f64| freqs.iter().any(|&m| (m - f).abs() <= tol);
    let mut families: Vec<(f64, Vec<f64>)> = Vec::new();
    for (i, &a) in freqs.iter().enumerate() {
        for &b in &freqs[i + 1..] {
            let d = b - a;
            if !(20.0..600.0).contains(&d) {
                continue;
            }
            // Only maximal progressions: if `a − d` is also a line, this one starts earlier.
            if at(a - d) {
                continue;
            }
            let mut members = vec![a, b];
            let mut next = b + d;
            while let Some(&m) = freqs.iter().find(|&&m| (m - next).abs() <= tol) {
                members.push(m);
                next = m + d;
            }
            if members.len() >= 3 {
                families.push((d, members));
            }
        }
    }
    families.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.total_cmp(&b.0)));
    for (d, m) in families.iter().take(8) {
        eprintln!(
            "[T-394] measured family: spacing {d:.2} Hz, {} members {:?}",
            m.len(),
            m.iter()
                .map(|v| (v * 100.0).round() / 100.0)
                .collect::<Vec<_>>()
        );
    }
    let mains: Vec<&(f64, Vec<f64>)> = families
        .iter()
        .filter(|(d, _)| (119.0..121.0).contains(d))
        .collect();
    assert!(
        !mains.is_empty(),
        "the ~119.95 Hz family was not measured blind; families found: {:?}",
        families
            .iter()
            .map(|(d, m)| (d, m.len()))
            .collect::<Vec<_>>()
    );
    let recorded = recorded_members(prov.get(), r.band_hz.1);
    for (d, m) in &mains {
        eprintln!(
            "[T-394] the fourth family, caught blind: spacing {d:.2} Hz, {} members {:?}",
            m.len(),
            m.iter()
                .map(|v| (v * 100.0).round() / 100.0)
                .collect::<Vec<_>>()
        );
        // It is genuinely unrecorded: no member of it is a member of any comb the fixture records,
        // so nothing in the provenance could have put it here.
        for &f in m.iter() {
            let clash = recorded.iter().find(|&&g| (g - f).abs() <= NEAR_HZ);
            assert!(
                clash.is_none(),
                "{f} Hz coincides with the recorded comb member {clash:?}, so it is not a blind \
                 find"
            );
        }
    }

    // --- The control, at the survey level: the emissions' own structure is NOT device-local. ---
    for (probe, what) in [
        (PILOT_HZ, "19 kHz pilot"),
        (SUBCARRIER_HZ, "38 kHz subcarrier"),
    ] {
        let hit = freqs.iter().find(|&&f| near(f, probe));
        assert!(
            hit.is_none(),
            "the survey called the {what} device-local ({hit:?} Hz) — it is the stations' own \
             structure, and eating it is worse than the artefact list this replaces"
        );
    }

    // --- The cross-check: it would have caught each of the three families that needed a record.
    //     This is the claim that justifies replacing the strategy, so it is asserted per family
    //     rather than as one count that a single dominant comb could carry. ---
    for c in prov.get().cyclic_combs() {
        let hits: Vec<f64> = freqs
            .iter()
            .copied()
            .filter(|&l| {
                let n = (l / c.fundamental_hz).round();
                n >= 1.0
                    && c.harmonics.is_none_or(|m| n <= f64::from(m))
                    && (l - n * c.fundamental_hz).abs() <= NEAR_HZ + n * c.fundamental_hz * c.drift
            })
            .collect();
        eprintln!(
            "[T-394] cross-check: the recorded {:.3} Hz family was measured blind at {:?}",
            c.fundamental_hz,
            hits.iter()
                .map(|v| (v * 100.0).round() / 100.0)
                .collect::<Vec<_>>()
        );
        assert!(
            hits.len() >= 2,
            "the measurement must recover the recorded {:.3} Hz family without its record: {hits:?}",
            c.fundamental_hz
        );
    }
}

// -------------------------------------------------------------------------------------------
// 2 and 3. The control and the mutation, on the shipped C13 -> C14 chain.
// -------------------------------------------------------------------------------------------

/// A grid of narrowband boxes across the capture's passband, the way T-382 measured its numbers.
///
/// T-382 used 44 boxes of 28 kHz; this is every other one of them (80 kHz apart, spanning
/// 99.94–101.62 MHz), which halves a test that is already the slowest in the crate while keeping
/// the boxes that matter: one on the strong station's centre, where its 38 kHz subcarrier is
/// visible, and two on its shoulders, where the 19 kHz pilot is.
fn grid(center_hz: f64) -> Vec<SnippetRequest> {
    const BOX_HZ: f64 = 28e3;
    const STEP_HZ: f64 = 80e3;
    const N: usize = 22;
    (0..N)
        .map(|i| {
            let off = -0.86e6 + STEP_HZ * i as f64;
            SnippetRequest {
                start_index: 0,
                end_index: 0, // filled by the caller
                center_offset_hz: off + (100.8e6 - center_hz),
                bandwidth_hz: BOX_HZ,
            }
        })
        .collect()
}

struct BoxResult {
    rf_hz: f64,
    argmax_hz: f64,
    cyclic_db: f64,
    excluded: usize,
    native_hz: f64,
}

fn run_grid(
    prov: &ProvenanceHandle,
    iq: &[Complex<i8>],
    lines: &ReceiverLines,
) -> (Vec<BoxResult>, Vec<BoxResult>) {
    let center = prov.get().tune.center_hz;
    // One extraction and one C13 estimate per box, two C14 runs over it: the arms differ only in
    // whether the measured receiver lines are in force, which is what a mutation check means.
    let mut chain = Chain::default();
    let mut on_estimator = BlindEstimator::new(BlindConfig::default());
    on_estimator.set_receiver_lines(Some(lines.clone()));
    let (mut off, mut on) = (Vec::new(), Vec::new());
    for mut req in grid(center) {
        req.end_index = iq.len() as u64;
        let out = chain.run(iq, prov, &req, &Hints::default());
        let rf_hz = center + req.center_offset_hz;
        let row = |s: &SymbolParameters| {
            argmax(s).map(|(f, db)| BoxResult {
                rf_hz,
                argmax_hz: f,
                cyclic_db: db,
                excluded: s.excluded_receiver_hz.len(),
                native_hz: s.sample_rate_hz / s.samples.max(1) as f64,
            })
        };
        let Some(a) = out.sym.as_ref().and_then(&row) else {
            continue;
        };
        let with = on_estimator.estimate_snippet(&out.snip, &out.params);
        let Some(b) = row(&with) else { continue };
        off.push(a);
        on.push(b);
    }
    (off, on)
}

fn median_db(v: &[BoxResult]) -> f64 {
    let mut d: Vec<f64> = v.iter().map(|b| b.cyclic_db).collect();
    d.sort_by(f64::total_cmp);
    d.get(d.len() / 2).copied().unwrap_or(f64::NAN)
}

/// **The control is the whole ticket**, and the mutation is what gives it teeth.
///
/// Over a grid of narrowband boxes across the passband, with every recorded artefact stripped so only the
/// measurement can act:
///
/// - **off** (no survey): the argmax is a receiver artefact in most boxes, which is the state
///   T-373 and T-382 each found and patched with one more named frequency.
/// - **on**: no box's argmax is any measured receiver line, the per-box median `cyclic_db` falls,
///   and the boxes still above 20 dB are the two stations' **genuine 19 kHz pilot and 38 kHz
///   subcarrier** — the emissions' own structure, untouched.
#[test]
fn t394_the_stations_pilot_and_subcarrier_survive_the_discriminator() {
    let Some((prov, iq)) = fixture(SECONDS, START_S) else {
        return;
    };
    let blind_prov = blind(&prov);
    let r = survey(&blind_prov, &iq, &SurveyConfig::default()).expect("the survey ran");

    // --- Mutation: nothing recorded, nothing measured. The artefacts are the answer. ---
    let (off, on) = run_grid(&blind_prov, &iq, &r);
    assert!(off.len() >= 20, "{} boxes analysed", off.len());
    let off_on_line = off
        .iter()
        .filter(|b| excluded(&r, b.argmax_hz, b.native_hz))
        .count();
    eprintln!(
        "[T-394] discriminator OFF (and no records): {} of {} boxes' argmax is a measured \
         receiver line, per-box median cyclic_db {:.2} dB, {} above 20 dB",
        off_on_line,
        off.len(),
        median_db(&off),
        off.iter().filter(|b| b.cyclic_db > 20.0).count()
    );
    assert!(
        off_on_line * 2 > off.len(),
        "the test has no teeth: without the discriminator only {off_on_line} of {} boxes read the \
         receiver",
        off.len()
    );

    // --- On: measured, never listed. ---
    let on_on_line: Vec<&BoxResult> = on
        .iter()
        .filter(|b| excluded(&r, b.argmax_hz, b.native_hz))
        .collect();
    let above20: Vec<&BoxResult> = on.iter().filter(|b| b.cyclic_db > 20.0).collect();
    eprintln!(
        "[T-394] discriminator ON: {} of {} boxes' argmax is a measured receiver line, per-box \
         median cyclic_db {:.2} dB, {} above 20 dB",
        on_on_line.len(),
        on.len(),
        median_db(&on),
        above20.len()
    );
    for b in &on {
        eprintln!(
            "    {:10.4} MHz  argmax {:9.2} Hz  {:6.2} dB  ({} lines excluded, native {:.3} Hz)",
            b.rf_hz / 1e6,
            b.argmax_hz,
            b.cyclic_db,
            b.excluded,
            b.native_hz
        );
    }
    assert!(
        on_on_line.is_empty(),
        "boxes still reading a measured receiver line: {:?}",
        on_on_line
            .iter()
            .map(|b| (b.rf_hz / 1e6, b.argmax_hz, b.cyclic_db))
            .collect::<Vec<_>>()
    );
    assert!(
        median_db(&on) < median_db(&off) - 5.0,
        "the discriminator must move this dimension, not shuffle it: {:.2} -> {:.2} dB",
        median_db(&off),
        median_db(&on)
    );

    // --- THE CONTROL. The stations' genuine 19 kHz pilot and 38 kHz subcarrier must not only
    //     survive, they must be what is *left on top*: the strongest boxes in the grid. ---
    // Three boxes of this grid sit on the strong station: 101.2200 and 101.3800 MHz on its
    // shoulders, where the 19 kHz pilot is, and 101.3000 MHz on its centre, where the 38 kHz
    // subcarrier is. Those three are the real cyclic structure in this capture, so those three
    // must be what is left on top.
    const TOP: usize = 3;
    let mut ranked: Vec<&BoxResult> = on.iter().collect();
    ranked.sort_by(|a, b| b.cyclic_db.total_cmp(&a.cyclic_db));
    for b in ranked.iter().take(8) {
        eprintln!(
            "    rank: {:10.4} MHz  {:9.2} Hz  {:6.2} dB",
            b.rf_hz / 1e6,
            b.argmax_hz,
            b.cyclic_db
        );
    }
    let real: BTreeSet<&str> = ranked
        .iter()
        .take(TOP)
        .map(|b| {
            if near(b.argmax_hz, PILOT_HZ) {
                "pilot"
            } else if near(b.argmax_hz, SUBCARRIER_HZ) {
                "subcarrier"
            } else {
                panic!(
                    "one of the five strongest boxes is neither station subcarrier: {:.4} MHz at \
                     {:.2} Hz, {:.2} dB - the discriminator has left (or created) something that \
                     is neither the receiver nor a real emission",
                    b.rf_hz / 1e6,
                    b.argmax_hz,
                    b.cyclic_db
                )
            }
        })
        .collect();
    eprintln!(
        "[T-394] control: the {} strongest boxes are {:?} at {:?} dB; {} boxes above 20 dB",
        TOP,
        real,
        ranked
            .iter()
            .take(TOP)
            .map(|b| (b.cyclic_db * 100.0).round() / 100.0)
            .collect::<Vec<_>>(),
        above20.len()
    );
    assert!(
        real.contains("pilot") && real.contains("subcarrier"),
        "the stations' genuine 19 kHz pilot and 38 kHz subcarrier must both survive: {real:?}"
    );
    // T-382 measured six boxes of 44 above 20 dB after its three recorded exclusions, and said
    // they were the stations' pilot and subcarrier. The measurement must leave the same handful,
    // not sweep the band clean.
    assert!(
        (3..=6).contains(&above20.len()),
        "T-382 left six boxes of 44 above 20 dB; {} of {} survived here, which is a different \
         answer rather than the same one",
        above20.len(),
        on.len()
    );
    // And they must stand clear of everything else: the strongest box that is NOT a station
    // subcarrier has to be well below the weakest that is, or "the subcarriers survived" is only
    // true by a coin toss.
    let weakest_real = ranked[TOP - 1].cyclic_db;
    let strongest_other = ranked[TOP];
    eprintln!(
        "[T-394] margin: weakest surviving subcarrier {:.2} dB against the next box, {:.4} MHz at \
         {:.2} Hz, {:.2} dB",
        weakest_real,
        strongest_other.rf_hz / 1e6,
        strongest_other.argmax_hz,
        strongest_other.cyclic_db
    );
    assert!(
        weakest_real > strongest_other.cyclic_db + 5.0,
        "the stations' subcarriers must stand clear: {:.2} dB against {:.2} dB",
        weakest_real,
        strongest_other.cyclic_db
    );
}

// -------------------------------------------------------------------------------------------
// 4. The null, and the cost of being wrong.
// -------------------------------------------------------------------------------------------

/// A synthetic capture of `seconds` at `fs`: noise everywhere, one narrowband FSK emission, and
/// optionally a periodic gain step of `period` samples — the artefact T-317 measured, applied
/// **without recording it anywhere**.
fn synthetic_capture(
    fs: f64,
    seconds: f64,
    gain_step: Option<(usize, usize, f64)>,
    seed: u64,
) -> (ProvenanceHandle, Vec<Complex<i8>>) {
    let mut rng = Rng::new(seed);
    let n = (fs * seconds) as usize;
    let rate = 9_600.0;
    let sig = gen_fsk(
        &mut rng,
        rate,
        4_800.0,
        (seconds * rate) as usize,
        fs,
        None,
        16,
    );
    let mut e = embed(&mut rng, &sig, fs, 25.0, 1.25 * rate, 0.0, 0.0);
    e.iq.truncate(n);
    if let Some((period, low, step_db)) = gain_step {
        let g = 10f64.powf(step_db / 20.0);
        for (i, s) in e.iq.iter_mut().enumerate() {
            if i % period < low {
                *s = Complex::new(
                    (f64::from(s.re) * g).round().clamp(-127.0, 127.0) as i8,
                    (f64::from(s.im) * g).round().clamp(-127.0, 127.0) as i8,
                );
            }
        }
    }
    (blind_support::provenance(fs), e.iq)
}

/// **The null.** A capture whose receiver contributes nothing periodic yields **no** measured
/// receiver lines. Without this the survey could be manufacturing notches out of the noise, and
/// every other result here would be meaningless.
#[test]
fn t394_a_clean_capture_yields_no_measured_receiver_lines() {
    let fs = 600_000.0;
    let (prov, iq) = synthetic_capture(fs, 2.0, None, 0x5EED_0394);
    let cfg = SurveyConfig {
        channels: 16,
        ..Default::default()
    };
    let r = survey(&prov, &iq, &cfg);
    match &r {
        None => eprintln!("[T-394] null: the survey abstained on a clean capture"),
        Some(r) => eprintln!(
            "[T-394] null: {} reference channels of {}, {} lines {:?}",
            r.reference_channels,
            r.surveyed_channels,
            r.lines.len(),
            &r.lines[..r.lines.len().min(8)]
        ),
    }
    if let Some(r) = r {
        assert!(
            r.lines.is_empty(),
            "a clean receiver must produce no lines; got {:?}",
            r.lines
        );
    }
}

/// **The same synthetic capture with a real artefact in it, and nothing recorded.** The survey
/// finds the comb it was never told about, and C14 stops reporting it — which is exactly the
/// sequence T-373 needed a provenance record for.
///
/// The genuine emission's own rate is the control: it must still be the answer.
#[test]
fn t394_an_unrecorded_artefact_is_measured_and_the_emissions_rate_survives() {
    let fs = 600_000.0;
    // 4096 samples at 600 kHz puts the comb at 146.484 Hz, far from the 9600 Bd emission.
    let (prov, iq) = synthetic_capture(fs, 2.0, Some((4096, 448, -1.5)), 0x5EED_0395);
    assert!(
        prov.get().capture_artefacts.is_empty(),
        "nothing may be recorded: this is the blind case"
    );
    let comb = fs / 4096.0;
    let cfg = SurveyConfig {
        channels: 16,
        ..Default::default()
    };
    let r = survey(&prov, &iq, &cfg).expect("the survey ran");
    let members: Vec<f64> = r
        .frequencies()
        .filter(|&f| {
            let n = (f / comb).round();
            n >= 1.0 && (f - n * comb).abs() <= 4.0 * r.resolution_hz
        })
        .collect();
    eprintln!(
        "[T-394] unrecorded {comb:.3} Hz comb: {} of {} measured lines are its harmonics {:?}",
        members.len(),
        r.lines.len(),
        &members[..members.len().min(8)]
    );
    assert!(
        members.len() >= 3,
        "the survey must find an unrecorded comb: lines {:?}",
        r.frequencies().collect::<Vec<_>>()
    );

    // The emission's rate is untouched: the notch is at the comb, not across the band.
    let req = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: 0.0,
        bandwidth_hz: 1.6 * 1.25 * 9_600.0,
    };
    let mut chain = Chain::default();
    chain.blind.set_receiver_lines(Some(r.clone()));
    let out = chain.run(&iq, &prov, &req, &Hints::default());
    let s = out.sym.as_ref().expect("C14 ran");
    eprintln!(
        "[T-394] with {} measured lines excluded in band: trusted rate {:?}, best {:?}",
        s.excluded_receiver_hz.len(),
        out.trusted_rate(),
        out.best_rate()
    );
    assert!(
        !s.excluded_receiver_hz.is_empty(),
        "the measured lines must reach the search"
    );
    let got = out
        .trusted_rate()
        .or_else(|| out.best_rate())
        .expect("a rate");
    assert!(
        [0.5, 1.0, 2.0]
            .iter()
            .any(|m| (got / (m * 9_600.0) - 1.0).abs() < 0.02),
        "the exclusion ate the emission's own rate: {got} Bd against 9600 Bd"
    );
}

/// A survey is **device-local physics** (T-259/T-305): it describes one receiver in one state, so
/// it is never applied to a window captured through another device, tune or gain state.
#[test]
fn t394_a_survey_is_never_applied_across_a_device_a_retune_or_a_gain_step() {
    let fs = 600_000.0;
    let (prov, iq) = synthetic_capture(fs, 2.0, Some((4096, 448, -1.5)), 0x5EED_0396);
    let r = survey(
        &prov,
        &iq,
        &SurveyConfig {
            channels: 16,
            ..Default::default()
        },
    )
    .expect("the survey ran");
    let p = prov.get();
    assert!(r.applies_to(p));
    for mutate in [
        (|p: &mut Provenance| p.device_id = "hackrf:another".into()) as fn(&mut Provenance),
        |p: &mut Provenance| p.tune.center_hz += 1e6,
        |p: &mut Provenance| p.tune.sample_rate_hz *= 2.0,
        |p: &mut Provenance| p.tune.lna_db += 8.0,
        |p: &mut Provenance| p.tune.vga_db += 2.0,
        |p: &mut Provenance| p.tune.amp_on = !p.tune.amp_on,
    ] {
        let mut q = p.clone();
        mutate(&mut q);
        assert!(
            !r.applies_to(&q),
            "a survey crossed a receiver-state change"
        );
    }

    // …and the exclusion follows: the same window under a different gain state excludes nothing.
    let mut q = p.clone();
    q.tune.lna_db += 8.0;
    let other = ProvenanceHandle::new(q);
    let req = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: 0.0,
        bandwidth_hz: 1.6 * 1.25 * 9_600.0,
    };
    let mut chain = Chain::default();
    chain.blind.set_receiver_lines(Some(r));
    let out = chain.run(&iq, &other, &req, &Hints::default());
    assert!(
        out.sym
            .as_ref()
            .expect("C14 ran")
            .excluded_receiver_hz
            .is_empty(),
        "a survey from another gain state must not be applied"
    );
}
