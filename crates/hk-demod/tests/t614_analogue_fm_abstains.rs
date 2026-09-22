//! T-614: the FSK burst estimator must **abstain** on analogue FM rather than label it `2fsk`.
//!
//! T-545 found the bursty analogue-FM neighbours of a trunked control channel stored with
//! `estimated_params` reading `mod_order: 2`, a symbol rate and ~2390 Hz deviation. An analogue
//! FM burst has no symbol alphabet; a confident two-level answer for one is worse than none.
//!
//! One scene, three kinds of emission, each on its own frequency, each run through the same chain
//! the pipeline's `fsk-bursts` chain runs (`FskReceiver` with the standard-rate table, then
//! `infer_framing`, then `write_framed_bursts`), and read back as the stored docs/07
//! `Demodulation` / `EstimatedParams`:
//!
//! - **tone-modulated NBFM bursts** (1 kHz tone, ±2.5 kHz — the T-545 scene's neighbours);
//! - **voice-like NBFM bursts** (a wandering multi-tone audio, ±2.5 kHz);
//! - **a real 2-FSK data channel** (4800 Bd, ±4.8 kHz, preamble + sync + random payload).
//!
//! Asserted **with counts**: every analogue burst is counted, none may carry a symbol rate or a
//! `mod_order`, and the digital channel must still carry both — so the test can pass neither on
//! zero analogue bursts nor by the estimator abstaining on everything.

mod common;

use std::f64::consts::TAU;

use common::*;
use hk_demod::fsk::{
    DemodPriors, FramedRecordContext, FskBurst, FskReceiver, FskReceiverConfig, periodic_bits,
    write_framed_bursts,
};
use hk_dsp::synth::{Rng, complex_noise};
use hk_estimate::SnippetRequest;
use hk_estimate::framing::{FramingConfig, infer_framing};
use hk_model::{EstimatedParams, Repository};
use num_complex::Complex32;

const T614: &str = "T-614";
const FS: f64 = 250_000.0;
const CENTER: f64 = 851_000_000.0;
const SPAN_S: f64 = 2.4;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    ToneFm,
    VoiceFm,
    Fsk2,
}

struct Burst {
    kind: Kind,
    start: usize,
    end: usize,
    offset_hz: f64,
    bw_hz: f64,
}

/// A wandering audio waveform: three tones whose frequencies drift and whose mix changes, the
/// shape of speech well enough that no one symbol clock describes it.
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
    let mut rng = Rng::new(614);
    let mut x = complex_noise(&mut rng, n, 1e-4);
    let amp = 0.1; // ~20 dB over the noise in the channel
    let mut bursts = Vec::new();

    // Analogue FM: tone at -40 kHz, voice at +45 kHz. ±2.5 kHz deviation, as T-545's scene.
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

    // Digital 2-FSK at +5 kHz: 4800 Bd, ±4.8 kHz, 32-bit preamble, sync 2DD4, 96 random bits.
    let (rate, fdev, offset) = (4800.0, 4800.0, 5_000.0);
    let sps = FS / rate;
    let sync = 0x2DD4u16;
    let mut t = 0.03;
    while t + 0.05 < SPAN_S {
        let mut bits: Vec<u8> = (0..32).map(|i| (i % 2) as u8).collect();
        bits.extend((0..16).rev().map(|i| ((sync >> i) & 1) as u8));
        bits.extend((0..96).map(|_| (rng.next_u64() & 1) as u8));
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
            kind: Kind::Fsk2,
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

#[test]
fn analogue_fm_bursts_get_no_symbol_rate_or_mod_order_while_the_digital_channel_does() {
    let (x, bursts) = scene();
    let prov = provenance(CENTER, FS);
    let mut rx = FskReceiver::new(FskReceiverConfig::default());
    // Exactly the priors the pipeline's fsk-bursts chain runs with.
    let priors = DemodPriors {
        standard_rates: true,
        ..Default::default()
    };
    let mut repo = Repository::open_in_memory().unwrap();

    let mut report = String::new();
    let mut failures = Vec::new();
    let mut digital_claims = 0usize;
    let mut digital_mod2 = 0usize;
    for kind in [Kind::ToneFm, Kind::VoiceFm, Kind::Fsk2] {
        let mine: Vec<&Burst> = bursts.iter().filter(|b| b.kind == kind).collect();
        let out: Vec<FskBurst> = mine
            .iter()
            .map(|b| {
                let req = SnippetRequest {
                    start_index: b.start as u64,
                    end_index: b.end as u64,
                    center_offset_hz: b.offset_hz,
                    bandwidth_hz: b.bw_hz,
                };
                rx.run(info(0, &prov), &x, &req, &priors).unwrap()
            })
            .collect();
        for (i, b) in out.iter().enumerate() {
            eprintln!(
                "[{T614}]   {kind:?} burst {i}: seed {} trusted {} bits {} periodic {:?} -> {:?}",
                b.seed.source.as_str(),
                b.seed.c14_trusted,
                b.bits().len(),
                periodic_bits(b.bits()),
                b.alphabet_evidence(),
            );
        }
        // The per-burst estimate, as the writer stores it…
        let burst_claims = out
            .iter()
            .filter(|b| claims_alphabet(&b.estimated_params()))
            .count();
        // …and what actually reached storage, as docs/07 Demodulation rows.
        let bits: Vec<&[u8]> = out.iter().map(FskBurst::bits).collect();
        let framing = infer_framing(&bits, &FramingConfig::default());
        let w = write_framed_bursts(&mut repo, &out, &framing, &FramedRecordContext::default())
            .unwrap();
        let stored: Vec<EstimatedParams> = w
            .demodulation_ids
            .iter()
            .map(|(_, id)| repo.demodulation(*id).unwrap().params)
            .collect();
        let stored_claims = stored.iter().filter(|p| claims_alphabet(p)).count();
        let stored_mod2 = stored.iter().filter(|p| p.mod_order == Some(2)).count();
        let latest = repo.latest_demodulation_for_emitter(w.emitter_id).unwrap();
        let line = format!(
            "[{T614}] {kind:?}: {} bursts, {burst_claims} per-burst alphabet claims, {} stored \
             demodulations, {stored_claims} stored alphabet claims ({stored_mod2} mod_order 2); \
             emitter's latest params {:?}\n",
            out.len(),
            stored.len(),
            latest.map(|d| d.params),
        );
        report.push_str(&line);
        match kind {
            Kind::Fsk2 => {
                digital_claims = stored_claims;
                digital_mod2 = stored_mod2;
            }
            _ => {
                if out.len() < 4 {
                    failures.push(format!(
                        "{kind:?}: only {} bursts — nothing to count",
                        out.len()
                    ));
                }
                if burst_claims + stored_claims > 0 {
                    failures.push(format!(
                        "{kind:?}: {burst_claims} per-burst and {stored_claims} stored \
                         symbol-rate/mod_order claims on ANALOGUE FM (want 0)"
                    ));
                }
            }
        }
    }
    eprint!("{report}");
    let n_digital = bursts.iter().filter(|b| b.kind == Kind::Fsk2).count();
    if digital_mod2 * 2 < n_digital || digital_claims < digital_mod2 {
        failures.push(format!(
            "Fsk2: {digital_mod2} of {n_digital} digital bursts stored mod_order 2 (want at least \
             half) — abstaining on everything is not the fix"
        ));
    }
    assert!(failures.is_empty(), "{}\n{report}", failures.join("\n"));
}
