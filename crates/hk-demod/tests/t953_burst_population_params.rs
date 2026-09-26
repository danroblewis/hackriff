//! T-953: **an emitter whose bursts agree on a symbol clock gets parameters**, even when no
//! framing model fits and C14 never trusted the rate.
//!
//! The explorer, on live air in San Francisco on 2026-09-25, watched `/ws/open/bits` publish a
//! symbol rate for 585 pager bursts — clustered hard at 4800 / 9600 / 19200 / 100 000 Bd — while
//! every emitter those bursts belonged to carried `estimated_params: null`. The two numbers came
//! from the *same* `FskSymbols`: the tap prints `lock.tracked_rate_bd` unconditionally
//! (`hk_pipeline::chains::taps::prepare`), while T-614 withholds it from storage unless something
//! independent of the trial agrees — a CRC, a sync prior, a learned sync word, or a trusted C14
//! clock. A real emitter that no framing model fits has none of those, on any of its bursts, ever.
//!
//! T-953 adds the evidence the explorer was reading by eye: **the population**. Below the trust
//! floor the receiver picks, per burst, the best-locking entry of a fixed standard-rate list; if
//! there is no symbol clock, nothing ties one burst's winner to the next one's. The same entry
//! winning burst after burst, with the tracked rates inside the loop's own jitter, is a
//! measurement of the emitter's clock (`hk_demod::fsk::consensus`).
//!
//! # The scene
//!
//! One recording, three emissions, each on its own frequency, each run through exactly the chain
//! the pipeline's `fsk-bursts` chain runs (`FskReceiver` with the standard-rate table, then
//! `infer_framing`, then `write_framed_bursts`), read back as stored docs/07 `Demodulation` rows:
//!
//! | emission | what it is |
//! |---|---|
//! | **`Pager`**, 4800 Bd ±4.8 kHz, **no preamble and no sync word**, random payload | the live-air case: a real symbol clock no framing model can confirm |
//! | **`ToneFm`**, 1 kHz tone ±2.5 kHz | the T-614 control: analogue FM must still claim nothing |
//! | **`VoiceFm`**, wandering multi-tone ±2.5 kHz | the harder T-614 control: non-periodic bits |
//!
//! # Blind
//!
//! The run is told nothing about the pager: `DemodPriors` carry the standard-rate table and
//! nothing else, no sync prior, no cluster prior, the receiver runs below the C14 trust floor
//! (`use_trusted: false`, the regime the standard-rate table exists for and the one the explorer's
//! bursts were in), and the boxes are the emissions' extents as a detector would give them. `PAGER_RATE_BD` and `PAGER_DEVIATION_HZ` appear **only** in the
//! assertions, after the run.
//!
//! # Red before T-953
//!
//! Every pager burst read `AlphabetEvidence::Abstained("unconfirmed standard-rate trial")`, so
//! `write_framed_bursts` wrote **no Demodulation row at all** for the pager — `stored.len() == 0`,
//! and the emitter's latest params `None`. Both controls are unchanged by T-953 and are asserted
//! here so the new rule cannot be bought with the old one.

mod common;

use std::f64::consts::TAU;

use common::*;
use hk_demod::fsk::{
    DemodPriors, FramedRecordContext, FskBurst, FskReceiver, FskReceiverConfig, rate_consensus,
    write_framed_bursts,
};
use hk_dsp::synth::{Rng, complex_noise};
use hk_estimate::SnippetRequest;
use hk_estimate::framing::{FramingConfig, infer_framing};
use hk_model::{EstimatedParams, Repository};
use num_complex::Complex32;

const T953: &str = "T-953";
const FS: f64 = 250_000.0;
/// 929.6125 MHz: inside the 929-930 MHz private-paging allocation (47 CFR 90.494).
const CENTER: f64 = 929_612_500.0;
const SPAN_S: f64 = 2.4;

/// Hidden truth. Read by the assertions only — never by the run.
///
/// 4800 Bd: one of the rates the explorer's 585 live pager bursts clustered on, and a member of
/// [`STANDARD_RATES_BD`](hk_demod::fsk::STANDARD_RATES_BD) — so the receiver's trial space can
/// reach it, and a wrong answer here would be T-953's fault rather than the rate table's. (The
/// FLEX rates 1600/3200/6400 Bd are **not** in that table: a 1600 Bd version of this scene locks
/// at 4800 Bd, the 3rd harmonic, on every burst. The consensus must then abstain rather than
/// store the harmonic — that is its own test below,
/// `a_1600_bd_pager_population_stores_1600_bd_or_abstains_never_its_harmonic`.)
const PAGER_RATE_BD: f64 = 4800.0;
/// Hidden truth: ±4.8 kHz, a FLEX-shaped deviation in a 25 kHz channel.
const PAGER_DEVIATION_HZ: f64 = 4800.0;

/// Pager burst amplitude, against a noise variance of 1e-4 — deliberately weaker than the
/// analogue controls, so the emission sits **below the C14 trust floor** (spike S5 §5) and the
/// receiver falls back to the standard-rate table. A clean 20 dB 2-FSK burst is trusted by C14 and
/// would take the T-614 path that already works; the live-air case this ticket is about is the one
/// C14 will not vouch for.
const PAGER_AMP: f64 = 0.02;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Pager,
    ToneFm,
    VoiceFm,
}

struct Burst {
    kind: Kind,
    start: usize,
    end: usize,
    offset_hz: f64,
    bw_hz: f64,
}

/// A wandering audio waveform: three drifting tones, the shape of speech well enough that no one
/// symbol clock describes it (the T-614 scene's voice control).
fn voice_audio(rng: &mut Rng, n: usize) -> Vec<f64> {
    let mut out = vec![0.0; n];
    let comps: Vec<(f64, f64, f64)> = (0..3)
        .map(|_| {
            (
                300.0 + 2200.0 * rng.unit(),
                0.5 + 3.0 * rng.unit(),
                TAU * rng.unit(),
            )
        })
        .collect();
    for (base, wobble_hz, ph0) in comps {
        let mut ph = ph0;
        for (k, o) in out.iter_mut().enumerate() {
            let t = k as f64 / FS;
            let f = base * (1.0 + 0.35 * (TAU * wobble_hz * t).sin());
            ph += TAU * f / FS;
            *o += (0.5 + 0.5 * (TAU * 0.7 * wobble_hz * t + ph0).sin()) * ph.sin();
        }
    }
    let peak = out.iter().fold(0.0f64, |m, v| m.max(v.abs())).max(1e-9);
    out.iter_mut().for_each(|v| *v /= peak);
    out
}

fn add_fm(x: &mut [Complex32], start: usize, offset_hz: f64, amp: f64, freq_hz: &[f64]) {
    let mut ph = 0.0f64;
    for (k, f) in freq_hz.iter().enumerate() {
        ph += TAU * (offset_hz + f) / FS;
        x[start + k] += Complex32::new((amp * ph.cos()) as f32, (amp * ph.sin()) as f32);
    }
}

fn scene() -> (Vec<Complex32>, Vec<Burst>) {
    let n = (SPAN_S * FS) as usize;
    let mut rng = Rng::new(953);
    let mut x = complex_noise(&mut rng, n, 1e-4);
    let amp = 0.1; // ~20 dB over the noise in the channel
    let mut bursts = Vec::new();

    // Analogue FM controls: tone at -40 kHz, voice at +45 kHz, ±2.5 kHz (the T-614 scene).
    let dev = 2500.0;
    for (kind, offset) in [(Kind::ToneFm, -40_000.0), (Kind::VoiceFm, 45_000.0)] {
        let mut t = 0.05;
        while t + 0.15 < SPAN_S {
            let dur = 0.12 + 0.2 * rng.unit();
            let (s, e) = (
                (t * FS) as usize,
                (((t + dur).min(SPAN_S - 0.01)) * FS) as usize,
            );
            let audio: Vec<f64> = match kind {
                Kind::ToneFm => {
                    let ph0 = TAU * rng.unit();
                    (0..e - s)
                        .map(|k| (TAU * 1000.0 * k as f64 / FS + ph0).sin())
                        .collect()
                }
                _ => voice_audio(&mut rng, e - s),
            };
            let f: Vec<f64> = audio.iter().map(|a| dev * a).collect();
            add_fm(&mut x, s, offset, amp, &f);
            bursts.push(Burst {
                kind,
                start: s,
                end: e,
                offset_hz: offset,
                bw_hz: 2.0 * (dev + 3000.0),
            });
            t += dur + 0.08 + 0.1 * rng.unit();
        }
    }

    // The pager: 2-FSK at +5 kHz, one clock, and **nothing a framing model can learn** — no
    // preamble, no sync word, a fresh random payload every burst.
    let (rate, fdev, offset) = (PAGER_RATE_BD, PAGER_DEVIATION_HZ, 5_000.0);
    let amp = PAGER_AMP;
    let sps = FS / rate;
    let mut t = 0.03;
    while t + 0.12 < SPAN_S {
        let bits: Vec<u8> = (0..160).map(|_| (rng.next_u64() & 1) as u8).collect();
        let len = (bits.len() as f64 * sps) as usize;
        let s = (t * FS) as usize;
        let f: Vec<f64> = (0..len)
            .map(|k| {
                let b = bits[((k as f64 / sps) as usize).min(bits.len() - 1)];
                if b == 1 { fdev } else { -fdev }
            })
            .collect();
        add_fm(&mut x, s, offset, amp, &f);
        bursts.push(Burst {
            kind: Kind::Pager,
            start: s,
            end: s + len,
            offset_hz: offset,
            bw_hz: 2.0 * fdev + rate,
        });
        t += len as f64 / FS + 0.09 + 0.04 * rng.unit();
    }
    (x, bursts)
}

fn claims_alphabet(p: &EstimatedParams) -> bool {
    p.symbol_rate_hz.is_some() || p.mod_order.is_some()
}

/// The bursts of one emission, demodulated exactly as the chain does.
fn demodulate(x: &[Complex32], mine: &[&Burst]) -> Vec<FskBurst> {
    let prov = provenance(CENTER, FS);
    // Below the C14 trust floor, which is the regime this ticket is about and the regime the
    // standard-rate table exists for: C14 does not vouch for the rate, so every burst's seed is an
    // unconfirmed standard-rate trial (`SeedSource::StandardRate`) — exactly what the explorer's
    // live pager bursts carried. `use_trusted: false` is the receiver's own switch for it; a clean
    // synthetic burst is otherwise trusted by C14 at any SNR the demodulator can lock at, and
    // would take the T-614 path that already worked.
    let mut rx = FskReceiver::new(FskReceiverConfig {
        use_trusted: false,
        ..FskReceiverConfig::default()
    });
    // Exactly the priors the pipeline's fsk-bursts chain runs with: the standard-rate table and
    // nothing else. No sync prior, no cluster prior — the run is told nothing.
    let priors = DemodPriors {
        standard_rates: true,
        ..Default::default()
    };
    mine.iter()
        .map(|b| {
            let req = SnippetRequest {
                start_index: b.start as u64,
                end_index: b.end as u64,
                center_offset_hz: b.offset_hz,
                bandwidth_hz: b.bw_hz,
            };
            rx.run(info(0, &prov), x, &req, &priors).unwrap()
        })
        .collect()
}

#[test]
fn a_pager_population_that_no_framing_fits_still_stores_its_symbol_rate_and_alphabet() {
    let (x, bursts) = scene();
    let mut repo = Repository::open_in_memory().unwrap();
    let mut report = String::new();
    let mut failures: Vec<String> = Vec::new();

    for kind in [Kind::Pager, Kind::ToneFm, Kind::VoiceFm] {
        let mine: Vec<&Burst> = bursts.iter().filter(|b| b.kind == kind).collect();
        let out = demodulate(&x, &mine);
        let consensus = rate_consensus(&out);
        let bits: Vec<&[u8]> = out.iter().map(FskBurst::bits).collect();
        let framing = infer_framing(&bits, &FramingConfig::default());
        let w = write_framed_bursts(&mut repo, &out, &framing, &FramedRecordContext::default())
            .unwrap();
        let stored: Vec<EstimatedParams> = w
            .demodulation_ids
            .iter()
            .map(|(_, id)| repo.demodulation(*id).unwrap().params)
            .collect();
        let claims = stored.iter().filter(|p| claims_alphabet(p)).count();
        let latest = repo.latest_demodulation_for_emitter(w.emitter_id).unwrap();
        for (i, b) in out.iter().enumerate() {
            report.push_str(&format!(
                "[{T953}]   {kind:?} burst {i}: seed {} trusted {} bits {} -> {:?}\n",
                b.seed.source.as_str(),
                b.seed.c14_trusted,
                b.bits().len(),
                b.alphabet_evidence(),
            ));
        }
        report.push_str(&format!(
            "[{T953}] {kind:?}: {} bursts, consensus {:?}, {} stored demodulations, {claims} \
             alphabet claims; emitter's latest params {:?}\n",
            out.len(),
            consensus.map(|c| c.evidence()),
            stored.len(),
            latest.as_ref().map(|d| &d.params),
        ));

        match kind {
            Kind::Pager => {
                if out.len() < 8 {
                    failures.push(format!(
                        "Pager: only {} bursts — nothing to count",
                        out.len()
                    ));
                }
                // Nothing framed: this is the live-air case, and the whole point of the test.
                if framing.model.sync.is_some() {
                    failures.push(
                        "Pager: framing learned a sync word — the scene is no longer the \
                         unframed case T-953 is about"
                            .into(),
                    );
                }
                // The population measured the clock…
                let Some(c) = consensus else {
                    failures.push("Pager: no consensus over a single-clock population".into());
                    continue;
                };
                let err = (c.rate_bd / PAGER_RATE_BD - 1.0).abs();
                if err > 0.02 {
                    failures.push(format!(
                        "Pager: consensus {:.1} Bd is {:.1} % off the true {PAGER_RATE_BD} Bd",
                        c.rate_bd,
                        100.0 * err
                    ));
                }
                // …and the measurement reached storage, which is what the inventory serves.
                if stored.len() * 2 < out.len() {
                    failures.push(format!(
                        "Pager: {} of {} bursts stored a Demodulation row (want at least half) — \
                         this was 0 before T-953",
                        stored.len(),
                        out.len()
                    ));
                }
                let rates: Vec<f64> = stored.iter().filter_map(|p| p.symbol_rate_hz).collect();
                if rates.len() * 2 < out.len() {
                    failures.push(format!(
                        "Pager: only {} of {} stored rows carry a symbol rate",
                        rates.len(),
                        out.len()
                    ));
                }
                for r in &rates {
                    if (r / PAGER_RATE_BD - 1.0).abs() > 0.02 {
                        failures.push(format!(
                            "Pager: stored rate {r:.1} Bd is not the true {PAGER_RATE_BD} Bd"
                        ));
                        break;
                    }
                }
                if stored.iter().filter(|p| p.mod_order == Some(2)).count() != stored.len() {
                    failures.push("Pager: a stored row is missing mod_order 2".into());
                }
                // The FSK levels, within a quarter of the truth (the deviation is measured at
                // settled symbols through a channel filter, not read off the generator).
                let devs: Vec<f64> = stored.iter().filter_map(|p| p.deviation_hz).collect();
                if devs.is_empty() {
                    failures.push("Pager: no stored row carries a deviation".into());
                }
                if let Some(bad) = devs
                    .iter()
                    .find(|d| (*d / PAGER_DEVIATION_HZ - 1.0).abs() > 0.25)
                {
                    failures.push(format!(
                        "Pager: stored deviation {bad:.0} Hz is not the true \
                         {PAGER_DEVIATION_HZ} Hz"
                    ));
                }
                // The burst duration the population agreed on, against the true 33.3 ms
                // (160 symbols at 4800 Bd).
                let true_s = 160.0 / PAGER_RATE_BD;
                if (c.burst_duration_s / true_s - 1.0).abs() > 0.1 {
                    failures.push(format!(
                        "Pager: consensus burst duration {:.1} ms is not the true {:.1} ms",
                        c.burst_duration_s * 1e3,
                        true_s * 1e3
                    ));
                }
                // And the emitter's own row — what `/api/inventory` serves as `estimated_params`.
                match latest.as_ref().map(|d| &d.params) {
                    Some(p) if claims_alphabet(p) => {}
                    other => failures.push(format!(
                        "Pager: the emitter's latest Demodulation carries no alphabet: {other:?}"
                    )),
                }
            }
            // T-614 stands: analogue FM has no symbol alphabet, and a population of it is still a
            // population of analogue FM. The new rule may not buy the old one back.
            _ => {
                if out.len() < 4 {
                    failures.push(format!(
                        "{kind:?}: only {} bursts — nothing to count",
                        out.len()
                    ));
                }
                if claims > 0 {
                    failures.push(format!(
                        "{kind:?}: {claims} stored symbol-rate/mod_order claims on ANALOGUE FM \
                         (want 0); consensus {:?}",
                        consensus.map(|c| c.evidence())
                    ));
                }
            }
        }
    }
    eprint!("{report}");
    assert!(failures.is_empty(), "{}\n{report}", failures.join("\n"));
}

/// A pager population alone: `levels` are the FSK tones (Hz, about the carrier), one symbol clock
/// at `rate` Bd, no preamble, no sync word, a fresh random payload per burst — the FLEX shape,
/// whose 1600 Bd is **not** in the standard-rate table.
fn pager_only_scene(rate: f64, levels: &[f64], seed: u64) -> (Vec<Complex32>, Vec<Burst>) {
    let n = (SPAN_S * FS) as usize;
    let mut rng = Rng::new(seed);
    let mut x = complex_noise(&mut rng, n, 1e-4);
    let offset = 5_000.0;
    let sps = FS / rate;
    let outer = levels.iter().fold(0.0f64, |m, l| m.max(l.abs()));
    let mut bursts = Vec::new();
    let mut t = 0.03;
    while t + 0.12 < SPAN_S {
        let syms: Vec<usize> = (0..160)
            .map(|_| (rng.next_u64() % levels.len() as u64) as usize)
            .collect();
        let len = (syms.len() as f64 * sps) as usize;
        let s = (t * FS) as usize;
        if s + len >= n {
            break;
        }
        let f: Vec<f64> = (0..len)
            .map(|k| levels[syms[((k as f64 / sps) as usize).min(syms.len() - 1)]])
            .collect();
        add_fm(&mut x, s, offset, PAGER_AMP, &f);
        bursts.push(Burst {
            kind: Kind::Pager,
            start: s,
            end: s + len,
            offset_hz: offset,
            bw_hz: 2.0 * outer + rate,
        });
        t += len as f64 / FS + 0.09 + 0.04 * rng.unit();
    }
    (x, bursts)
}

/// **A harmonic lock is not a measured rate** (T-953 review). A true 1600 Bd FLEX-shaped
/// population demodulated below the C14 trust floor locks, burst after burst, on the 4800 Bd
/// table entry — the 3rd harmonic — and those bursts agree with each other to ±0.01 %, so a
/// consensus over agreement alone stores `symbol_rate_hz: 4800` and a 4800 Bd fingerprint that
/// entity resolution then matches on. At `k×` oversampling the bits come out in runs of ≈ `k`, so
/// single-symbol runs all but vanish (random NRZ has ≈ half its runs one symbol long); the
/// consensus must refuse such a burst exactly as the receiver's harmonic check does.
///
/// The emitter must store **1600 Bd or no rate at all** — never 4800 — for 2-FSK and for FLEX's
/// 4-level alphabet. Red before the single-run gate: the 2-FSK population stored 4800 Bd.
#[test]
fn a_1600_bd_pager_population_stores_1600_bd_or_abstains_never_its_harmonic() {
    const TRUE_RATE_BD: f64 = 1600.0; // hidden truth, read by the assertions only
    let cases: [(&str, &[f64], u64); 2] = [
        ("2-FSK ±4.8 kHz", &[-4800.0, 4800.0], 9531),
        (
            "4-FSK ±4.8/±1.6 kHz",
            &[-4800.0, -1600.0, 1600.0, 4800.0],
            9532,
        ),
    ];
    let mut failures = Vec::new();
    let mut report = String::new();
    for (name, levels, seed) in cases {
        let (x, bursts) = pager_only_scene(TRUE_RATE_BD, levels, seed);
        let mine: Vec<&Burst> = bursts.iter().collect();
        let out = demodulate(&x, &mine);
        let consensus = rate_consensus(&out);
        let bits: Vec<&[u8]> = out.iter().map(FskBurst::bits).collect();
        let framing = infer_framing(&bits, &FramingConfig::default());
        let mut repo = Repository::open_in_memory().unwrap();
        let w = write_framed_bursts(&mut repo, &out, &framing, &FramedRecordContext::default())
            .unwrap();
        let stored: Vec<EstimatedParams> = w
            .demodulation_ids
            .iter()
            .map(|(_, id)| repo.demodulation(*id).unwrap().params)
            .collect();
        let tracked: Vec<String> = out
            .iter()
            .filter_map(|b| b.symbols.as_ref())
            .map(|s| format!("{:.0}", s.lock.tracked_rate_bd))
            .collect();
        let fp_rate = repo.emitter(w.emitter_id).unwrap().fingerprint["symbol_rate_hz"].as_f64();
        report.push_str(&format!(
            "[{T953}] {name}: {} bursts, tracked {tracked:?}, consensus {:?}, {} stored rows, \
             fingerprint rate {fp_rate:?}\n",
            out.len(),
            consensus.map(|c| c.evidence()),
            stored.len(),
        ));
        if out.len() < 8 {
            failures.push(format!("{name}: only {} bursts", out.len()));
        }
        let wrong = |r: f64| (r / TRUE_RATE_BD - 1.0).abs() > 0.02;
        if let Some(c) = consensus.filter(|c| wrong(c.rate_bd)) {
            failures.push(format!(
                "{name}: the population 'agreed' on {:.1} Bd, not the true {TRUE_RATE_BD} Bd",
                c.rate_bd
            ));
        }
        if let Some(r) = stored
            .iter()
            .filter_map(|p| p.symbol_rate_hz)
            .find(|r| wrong(*r))
        {
            failures.push(format!(
                "{name}: a Demodulation row stored {r:.1} Bd for a {TRUE_RATE_BD} Bd emitter"
            ));
        }
        if let Some(r) = fp_rate.filter(|r| wrong(*r)) {
            failures.push(format!(
                "{name}: the emitter's fingerprint carries {r:.1} Bd for a {TRUE_RATE_BD} Bd \
                 emitter"
            ));
        }
    }
    eprint!("{report}");
    assert!(failures.is_empty(), "{}\n{report}", failures.join("\n"));
}
