//! T-065 / SIGNAL-062: blind synthetic sweep of the analog auto-mode selector.
//!
//! Each case is synthesised here, handed to [`AnalogReceiver`] as IQ plus a detection-style box
//! (centre and width only; the box width is the same for every narrowband signal) and the
//! decision is compared against a truth list the receiver never sees. The confusion matrix is
//! printed (`cargo test -p hk-demod --test signal_062_mode_sweep -- --nocapture`); set
//! `HK_MODE_SWEEP_VERBOSE=1` for one line per case with the features.
//!
//! Scenes:
//! - **Narrowband:** 250 kS/s, 25 kHz box. SNR is signal power over the noise in a 10 kHz
//!   reference bandwidth.
//! - **Wideband:** 1 MS/s, 250 kHz box; SNR over a 200 kHz reference bandwidth.
//!
//! Signals: unmodulated carrier; AM at 10/30/60/100 % depth with a 1 kHz tone and with
//! voice-like audio; NBFM at 2.5 and 5 kHz deviation (tone, voice-like); WFM at 75 kHz with a
//! 19 kHz pilot; **WFM at 75 kHz mono, with no pilot at all** (T-416); a **wide constant-envelope
//! 4-CPFSK data link** (T-416); USB and LSB voice-like; keyed CW; noise only (both scenes).
//! SNR 3/10/20/30 dB. Every case carries a frequency offset from the box centre; half also drift
//! linearly.
//!
//! **The last two are a pair, and they are the T-416 control.** Every mono broadcaster in the
//! world lacks the 19 kHz pilot, so a selector that needs one cannot recognise any of them; and a
//! selector that recognises them by relaxing until anything wide and constant-envelope is WFM has
//! replaced one defect with a worse one. `wfm75k-mono` must read as WFM, and `4fsk-wide` — equally
//! wide, equally constant-envelope, and genuinely not broadcast FM — must not.

mod common;

use std::f64::consts::TAU;

use common::*;
use hk_demod::{AnalogMode, AnalogReceiver, ModeDecision};
use hk_dsp::synth::{Rng, complex_noise};
use hk_estimate::SnippetRequest;
use num_complex::Complex32;

const SECS: f64 = 0.5;
const SNRS: [f64; 4] = [3.0, 10.0, 20.0, 30.0];

/// T-416 control: symbol rate of the wide data link, Bd. Comparable to its own occupied
/// bandwidth, as a digital emission's is — which is exactly why its instantaneous frequency
/// cannot fit inside a broadcast multiplex.
const FSK_BAUD: f64 = 80e3;
/// Its four frequency levels, Hz.
const FSK_LEVELS_HZ: [f64; 4] = [-54e3, -18e3, 18e3, 54e3];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Signal {
    Carrier,
    CwKeyed,
    AmTone(u32),
    AmVoice(u32),
    NbfmTone(f64),
    NbfmVoice(f64),
    Wfm,
    /// T-416: the same 75 kHz-deviation broadcast station, **mono** — no 19 kHz pilot.
    WfmMono,
    /// T-416: a wide constant-envelope 4-level CPFSK data link. Not broadcast FM, and the control
    /// that recognising mono FM did not turn into "anything wide and constant-envelope is FM".
    FskWide,
    Usb,
    Lsb,
    NoiseNarrow,
    NoiseWide,
}

impl Signal {
    const ALL: [Signal; 21] = [
        Signal::Carrier,
        Signal::CwKeyed,
        Signal::AmTone(10),
        Signal::AmTone(30),
        Signal::AmTone(60),
        Signal::AmTone(100),
        Signal::AmVoice(10),
        Signal::AmVoice(30),
        Signal::AmVoice(60),
        Signal::AmVoice(100),
        Signal::NbfmTone(2.5e3),
        Signal::NbfmTone(5e3),
        Signal::NbfmVoice(2.5e3),
        Signal::NbfmVoice(5e3),
        Signal::Wfm,
        Signal::WfmMono,
        Signal::FskWide,
        Signal::Usb,
        Signal::Lsb,
        Signal::NoiseNarrow,
        Signal::NoiseWide,
    ];

    fn name(self) -> String {
        match self {
            Signal::Carrier => "carrier".into(),
            Signal::CwKeyed => "cw-keyed".into(),
            Signal::AmTone(m) => format!("am{m}-tone"),
            Signal::AmVoice(m) => format!("am{m}-voice"),
            Signal::NbfmTone(d) => format!("nbfm{:.1}k-tone", d / 1e3),
            Signal::NbfmVoice(d) => format!("nbfm{:.1}k-voice", d / 1e3),
            Signal::Wfm => "wfm75k-pilot".into(),
            Signal::WfmMono => "wfm75k-mono".into(),
            Signal::FskWide => "4fsk-wide".into(),
            Signal::Usb => "usb-voice".into(),
            Signal::Lsb => "lsb-voice".into(),
            Signal::NoiseNarrow => "noise-25k".into(),
            Signal::NoiseWide => "noise-250k".into(),
        }
    }

    /// Truth row of the confusion matrix.
    fn row(self) -> &'static str {
        match self {
            Signal::Carrier => "carrier",
            Signal::CwKeyed => "cw-keyed",
            Signal::AmTone(10) | Signal::AmVoice(10) => "am-10%",
            Signal::AmTone(30) | Signal::AmVoice(30) => "am-30%",
            Signal::AmTone(60) | Signal::AmVoice(60) => "am-60%",
            Signal::AmTone(_) | Signal::AmVoice(_) => "am-100%",
            Signal::NbfmTone(_) | Signal::NbfmVoice(_) => "nbfm",
            Signal::Wfm => "wfm",
            Signal::WfmMono => "wfm-mono",
            Signal::FskWide => "fsk-wide",
            Signal::Usb | Signal::Lsb => "ssb",
            Signal::NoiseNarrow | Signal::NoiseWide => "noise",
        }
    }

    /// The mode a correct selector reports (private to the test).
    fn truth(self) -> AnalogMode {
        match self {
            Signal::Carrier | Signal::CwKeyed => AnalogMode::Cw,
            Signal::AmTone(_) | Signal::AmVoice(_) => AnalogMode::Am,
            Signal::NbfmTone(_) | Signal::NbfmVoice(_) => AnalogMode::Nbfm,
            Signal::Wfm | Signal::WfmMono => AnalogMode::Wfm,
            Signal::Usb | Signal::Lsb => AnalogMode::Ssb,
            // No analog mode describes a data link; the selector must abstain, not guess.
            Signal::FskWide | Signal::NoiseNarrow | Signal::NoiseWide => AnalogMode::Unknown,
        }
    }

    fn wide(self) -> bool {
        matches!(
            self,
            Signal::Wfm | Signal::WfmMono | Signal::FskWide | Signal::NoiseWide
        )
    }
}

const ROWS: [&str; 12] = [
    "carrier", "cw-keyed", "am-10%", "am-30%", "am-60%", "am-100%", "nbfm", "wfm", "wfm-mono",
    "fsk-wide", "ssb", "noise",
];

#[derive(Clone, Debug)]
struct Case {
    signal: Signal,
    snr_db: f64,
    drift: bool,
    seed: u64,
}

struct Scene {
    fs: f64,
    iq: Vec<Complex32>,
    request: SnippetRequest,
}

// ------------------------------------------------------------------------ synthesis

/// Voice-like audio at `fs`: a gliding pitch (120–180 Hz) whose 300–3400 Hz harmonics are
/// shaped by three formants, a 4 Hz syllabic envelope, peak-normalised and soft-compressed like
/// a transmitter's speech processor. Returns the real signal and its analytic (one-sided) twin,
/// both with peak magnitude 1.
fn voice(fs: f64, n: usize, rng: &mut Rng) -> (Vec<f64>, Vec<(f64, f64)>) {
    let phases: Vec<f64> = (0..32).map(|_| TAU * rng.unit()).collect();
    let formant = |f: f64| {
        [(600.0, 250.0), (1400.0, 350.0), (2500.0, 400.0)]
            .iter()
            .map(|&(c, w)| (-((f - c) / w).powi(2)).exp())
            .sum::<f64>()
            + 0.08
    };
    let mut pitch_phase = 0.0;
    let mut re = Vec::with_capacity(n);
    let mut an = Vec::with_capacity(n);
    for k in 0..n {
        let t = k as f64 / fs;
        let f0 = 150.0 + 30.0 * (TAU * 1.7 * t).sin();
        pitch_phase += f0 / fs;
        let env = 0.15 + 0.85 * (0.5 - 0.5 * (TAU * 4.0 * t).cos());
        let (mut r, mut i) = (0.0, 0.0);
        for (h, ph) in phases.iter().enumerate() {
            let f = (h + 1) as f64 * f0;
            if !(300.0..=3400.0).contains(&f) {
                continue;
            }
            let a = formant(f);
            let arg = TAU * (h + 1) as f64 * pitch_phase + ph;
            r += a * arg.cos();
            i += a * arg.sin();
        }
        re.push(env * r);
        an.push((env * r, env * i));
    }
    let peak = re.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let apeak = an.iter().fold(0.0f64, |m, (r, i)| m.max(r.hypot(*i)));
    let g = 2.0;
    let re = re
        .iter()
        .map(|v| (g * v / peak).tanh() / g.tanh())
        .collect();
    let an = an.iter().map(|(r, i)| (r / apeak, i / apeak)).collect();
    (re, an)
}

/// Morse-like keying (40 ms units, 5 ms raised-cosine edges), 0–1.
fn keying(t: f64) -> f64 {
    const PATTERN: [u8; 16] = [1, 1, 1, 0, 1, 0, 0, 0, 1, 1, 1, 0, 1, 0, 0, 0];
    let unit = 0.040;
    let edge = 0.005;
    let at = |k: i64| PATTERN[k.rem_euclid(16) as usize] as f64;
    let u = (t / unit).floor() as i64;
    let frac = t - u as f64 * unit;
    let (cur, prev) = (at(u), at(u - 1));
    if frac < edge && cur != prev {
        let w = 0.5 - 0.5 * (std::f64::consts::PI * frac / edge).cos();
        prev + (cur - prev) * w
    } else {
        cur
    }
}

fn scene(case: &Case) -> Scene {
    let sig = case.signal;
    let (fs, box_bw, ref_bw, box_centre, offset, drift) = if sig.wide() {
        (1e6, 250e3, 200e3, 150e3, 9.3e3, 1_000.0)
    } else {
        (250e3, 25e3, 10e3, -40e3, 730.0, 100.0)
    };
    let drift = if case.drift { drift } else { 0.0 };
    let n = (fs * SECS) as usize;
    let mut rng = Rng::new(case.seed);
    // Energy centre of the emission relative to its carrier (the detection box centres there).
    let energy_centre = match sig {
        Signal::Usb => 1_850.0,
        Signal::Lsb => -1_850.0,
        _ => 0.0,
    };
    let carrier0 = box_centre + offset - energy_centre;
    let (vre, van) = match sig {
        Signal::AmVoice(_)
        | Signal::NbfmVoice(_)
        | Signal::Wfm
        | Signal::WfmMono
        | Signal::Usb
        | Signal::Lsb => voice(fs, n, &mut rng),
        _ => (Vec::new(), Vec::new()),
    };
    // T-416: the data link's 4-level symbol stream, drawn once so drift and SNR variants of the
    // same seed carry the same bits.
    let fsk = matches!(sig, Signal::FskWide)
        .then(|| {
            let symbols = (n as f64 / (fs / FSK_BAUD)).ceil() as usize + 2;
            (0..symbols)
                .map(|_| FSK_LEVELS_HZ[(rng.unit() * 4.0) as usize % 4])
                .collect::<Vec<f64>>()
        })
        .unwrap_or_default();
    let a = 0.1;
    let mut theta = TAU * rng.unit();
    let mut fm_phase = 0.0;
    let mut clean = Vec::with_capacity(n);
    for k in 0..n {
        let t = k as f64 / fs;
        let fc = carrier0 + drift * t;
        theta += TAU * fc / fs;
        let tone = (TAU * 1_000.0 * t).cos();
        let (amp, extra, iq_rot) = match sig {
            Signal::Carrier => (a, 0.0, None),
            Signal::CwKeyed => (a * keying(t), 0.0, None),
            Signal::AmTone(m) => (a * (1.0 + f64::from(m) / 100.0 * tone), 0.0, None),
            Signal::AmVoice(m) => (a * (1.0 + f64::from(m) / 100.0 * vre[k]), 0.0, None),
            Signal::NbfmTone(dev) => {
                fm_phase += TAU * dev * tone / fs;
                (a, fm_phase, None)
            }
            Signal::NbfmVoice(dev) => {
                fm_phase += TAU * dev * vre[k] / fs;
                (a, fm_phase, None)
            }
            Signal::Wfm => {
                let mpx = 0.9 * vre[k] + 0.1 * (TAU * 19_000.0 * t).cos();
                fm_phase += TAU * 75e3 * mpx / fs;
                (a, fm_phase, None)
            }
            Signal::WfmMono => {
                // A mono station: the same programme, the same peak deviation, no pilot and no
                // subcarrier of any kind. There is nothing here but audio.
                fm_phase += TAU * 75e3 * vre[k] / fs;
                (a, fm_phase, None)
            }
            Signal::FskWide => {
                // Phase-continuous 4-CPFSK: constant envelope and about as wide as the broadcast
                // station, but its modulating signal is a symbol stream at FSK_BAUD, which no
                // broadcast baseband has room for.
                let sym = fsk[((t * FSK_BAUD) as usize).min(fsk.len() - 1)];
                fm_phase += TAU * sym / fs;
                (a, fm_phase, None)
            }
            Signal::Usb => (a, 0.0, Some(van[k])),
            Signal::Lsb => (a, 0.0, Some((van[k].0, -van[k].1))),
            Signal::NoiseNarrow | Signal::NoiseWide => (0.0, 0.0, None),
        };
        let ph = theta + extra;
        let (c, s) = (ph.cos(), ph.sin());
        let (re, im) = match iq_rot {
            Some((r, i)) => (amp * (r * c - i * s), amp * (r * s + i * c)),
            None => (amp * c, amp * s),
        };
        clean.push(Complex32::new(re as f32, im as f32));
    }
    let p_sig = if matches!(sig, Signal::NoiseNarrow | Signal::NoiseWide) {
        a * a
    } else {
        clean.iter().map(|v| f64::from(v.norm_sqr())).sum::<f64>() / n as f64
    };
    let variance = p_sig * fs / (ref_bw * 10f64.powf(case.snr_db / 10.0));
    let mut iq = complex_noise(&mut rng, n, variance);
    for (x, c) in iq.iter_mut().zip(&clean) {
        *x += c;
    }
    Scene {
        fs,
        iq,
        request: SnippetRequest {
            start_index: 0,
            end_index: n as u64,
            center_offset_hz: box_centre,
            bandwidth_hz: box_bw,
        },
    }
}

/// The selector under test sees only IQ, provenance and the box.
fn classify(s: &Scene) -> ModeDecision {
    let prov = provenance(100e6, s.fs);
    AnalogReceiver::default()
        .run(info(0, &prov), &s.iq, &s.request)
        .expect("receiver runs")
        .mode
}

// ------------------------------------------------------------------------ sweep

struct Outcome {
    case: Case,
    decision: ModeDecision,
}

fn sweep() -> Vec<Outcome> {
    let mut cases = Vec::new();
    let mut seed = 6_500;
    for signal in Signal::ALL {
        for snr_db in SNRS {
            for drift in [false, true] {
                seed += 1;
                cases.push(Case {
                    signal,
                    snr_db,
                    drift,
                    seed,
                });
            }
        }
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let chunk = cases.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = cases
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|case| Outcome {
                            decision: classify(&scene(case)),
                            case: case.clone(),
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("sweep thread"))
            .collect()
    })
}

fn col(m: AnalogMode) -> usize {
    AnalogMode::ALL.iter().position(|&x| x == m).unwrap()
}

fn print_matrix(title: &str, outcomes: &[&Outcome]) {
    eprintln!("[{SIGNAL_062}] {title}");
    let head: Vec<String> = AnalogMode::ALL
        .iter()
        .map(|m| format!("{:>8}", m.as_str()))
        .collect();
    eprintln!("  {:<10}{}  correct", "truth", head.join(""));
    let (mut ok_all, mut n_all) = (0, 0);
    for row in ROWS {
        let mut counts = [0usize; 6];
        let (mut ok, mut n) = (0, 0);
        for o in outcomes.iter().filter(|o| o.case.signal.row() == row) {
            counts[col(o.decision.mode)] += 1;
            n += 1;
            ok += usize::from(o.decision.mode == o.case.signal.truth());
        }
        if n == 0 {
            continue;
        }
        ok_all += ok;
        n_all += n;
        let cells: Vec<String> = counts.iter().map(|c| format!("{c:>8}")).collect();
        eprintln!("  {:<10}{}  {ok}/{n}", row, cells.join(""));
    }
    let wrong = outcomes
        .iter()
        .filter(|o| {
            o.decision.mode != o.case.signal.truth() && o.decision.mode != AnalogMode::Unknown
        })
        .count();
    eprintln!("  total correct {ok_all}/{n_all}, wrong (not unknown) {wrong}");
}

fn report(outcomes: &[Outcome]) {
    if std::env::var("HK_MODE_SWEEP_VERBOSE").is_ok_and(|v| v == "1") {
        for o in outcomes {
            let f = &o.decision.features;
            let r = |v: Option<f64>, k: f64| v.map(|x| (x * k).round() / k);
            eprintln!(
                "  {:<16} {:>4.0} dB {:<6} -> {:<7} {:.2} | obw {:?} env {:?} sym {:?} sq {:?} | \
                 line {:?} Hz {:?} dB frac {:?} | t {:?} bal {:?} depth {:?}/{:?} sb/c {:?} off {:?} \
                 | pilot {:?} baseband {:?} | {:?}",
                o.case.signal.name(),
                o.case.snr_db,
                if o.case.drift { "drift" } else { "offset" },
                o.decision.mode.as_str(),
                o.decision.confidence,
                r(f.obw99_hz, 1.0),
                r(f.envelope_variation, 1e3),
                r(f.symmetry, 1e2),
                r(f.square_line_db, 1.0),
                r(f.line_offset_hz, 1.0),
                r(f.line_snr_db, 1.0),
                r(f.line_fraction, 1e2),
                r(f.inphase_sideband_t, 1e1),
                r(f.iq_balance, 1e2),
                r(f.am_depth, 1e3),
                r(f.am_depth_floor, 1e3),
                r(f.sideband_to_carrier_db, 1e1),
                r(f.keyed_off_fraction, 1e2),
                f.pilot.map(|p| p.found),
                r(f.baseband_fraction, 1e3),
                o.decision.reason,
            );
        }
    }
    let all: Vec<&Outcome> = outcomes.iter().collect();
    print_matrix(
        "mode confusion, all SNR (3/10/20/30 dB), offset + drift",
        &all,
    );
    let hi: Vec<&Outcome> = outcomes.iter().filter(|o| o.case.snr_db >= 10.0).collect();
    print_matrix("mode confusion, SNR >= 10 dB", &hi);
}

#[test]
fn signal_062_mode_sweep_blind_confusion() {
    let outcomes = sweep();
    report(&outcomes);
    let mut failures = Vec::new();
    for o in &outcomes {
        let (sig, snr, got) = (o.case.signal, o.case.snr_db, o.decision.mode);
        let label = format!(
            "{} @ {snr} dB{}: {} ({:.2})",
            sig.name(),
            if o.case.drift { " drift" } else { "" },
            got.as_str(),
            o.decision.confidence
        );
        let must_be_truth = match sig {
            // A pure carrier is carrier/CW at every SNR, never AM.
            Signal::Carrier | Signal::NoiseNarrow | Signal::NoiseWide => true,
            Signal::AmTone(m) | Signal::AmVoice(m) => m >= 30 && snr >= 10.0,
            Signal::CwKeyed | Signal::NbfmTone(_) | Signal::NbfmVoice(_) | Signal::Wfm => {
                snr >= 10.0
            }
            // T-416. The mono station must be recognised without a pilot — but only once its
            // whole 53 kHz baseband is above the noise. The pilot is a narrowband feature and can
            // be found at 10 dB; a mono station has no such feature, so what identifies it is the
            // *shape* of its whole multiplex, and at 10 dB over 200 kHz the discriminator's own
            // f²-rising noise fills the part above the programme channel. Below that it abstains,
            // which is the honest answer and not a guess. (10 dB CNR is also about where a
            // wideband FM receiver reaches threshold and starts clicking.)
            Signal::WfmMono => snr >= 20.0,
            // The data link must never be called WFM, at any SNR: being wide and
            // constant-envelope is not evidence of broadcast FM.
            Signal::FskWide => true,
            Signal::Usb | Signal::Lsb => false,
        };
        // AM shallower than the depth the sideband test reports it could detect is, to the
        // selector, a carrier (10 % speech peaks ≈ 5.6 % tone-equivalent depth).
        let below_floor = matches!(sig, Signal::AmTone(10) | Signal::AmVoice(10))
            && got == AnalogMode::Cw
            && o.decision
                .features
                .am_depth_floor
                .is_some_and(|floor| floor > 0.056);
        if must_be_truth && got != sig.truth() {
            failures.push(label);
        } else if got != sig.truth() && got != AnalogMode::Unknown && snr >= 10.0 && !below_floor {
            // Above 10 dB an ambiguous case abstains; it never guesses another mode.
            failures.push(format!("forced guess: {label}"));
        }
    }
    assert!(
        failures.is_empty(),
        "[{SIGNAL_062}] {} failures:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}
