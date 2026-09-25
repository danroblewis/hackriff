//! Audio group tests (T-866): each block through the registry (schema-validated params), driven
//! chunk by chunk like the recipe runtime.

use std::f64::consts::TAU;

use hk_recipe::PortType;
use num_complex::Complex32;
use serde_json::json;

use crate::block::{Block, Io, PortInfo};
use crate::blocks::iq::testkit::{Lcg, build};
use crate::buffer::{ChunkFlags, ChunkMeta, Input, Output, PortSlice, PortVec};
use crate::status::Lock;

/// One block driven chunk by chunk, collecting its single output (or its sink frames).
struct Drive {
    block: Box<dyn Block>,
    outputs: Vec<Output>,
    rate: f64,
    index: u64,
    source: f64,
    per: f64,
    /// Emitted `real` items and the metadata of each non-empty output chunk.
    real: Vec<f32>,
    metas: Vec<(ChunkMeta, usize)>,
    /// Sink frames: `(sample_index, discontinuity, samples)`.
    frames: Vec<(u64, bool, Vec<i16>)>,
}

impl Drive {
    fn new(name: &str, params: serde_json::Value, ty: PortType, rate: f64, chunk: usize) -> Self {
        let mut block = build(name, params, ty);
        let info = PortInfo {
            ty,
            rate_hz: rate,
            max_items: chunk,
            hold_items: 0,
        };
        let outs = block.init(&[info]).expect("init");
        Self {
            block,
            outputs: outs.iter().map(Output::for_port).collect(),
            rate,
            index: 0,
            source: 0.0,
            per: 10.0,
            real: Vec::new(),
            metas: Vec::new(),
            frames: Vec::new(),
        }
    }

    /// Feeds one chunk; `skip` source items are lost before it (flagged `DISCONTINUITY`).
    fn feed_after(&mut self, data: PortSlice<'_>, skip: u64) {
        let mut flags = ChunkFlags::NONE;
        if self.index == 0 || skip > 0 {
            flags |= ChunkFlags::DISCONTINUITY;
        }
        self.index += skip;
        self.source += skip as f64 * self.per;
        let meta = ChunkMeta {
            index: self.index,
            source_index: self.source,
            source_per_item: self.per,
            rate_hz: self.rate,
            channel: 0,
            flags,
        };
        self.index += data.len() as u64;
        self.source += data.len() as f64 * self.per;
        for o in &mut self.outputs {
            o.begin_chunk();
        }
        let inputs = [Input { meta, data }];
        self.block
            .process(&mut Io::new(&inputs, &mut self.outputs))
            .expect("process");
        if let Some(o) = self.outputs.first()
            && let PortVec::Real(v) = &o.data
            && !v.is_empty()
        {
            self.real.extend_from_slice(v);
            self.metas.push((o.meta, v.len()));
        }
        if let Some(f) = self.block.audio_frames() {
            for (fr, bytes) in f.iter() {
                let pcm = bytes
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();
                self.frames.push((fr.sample_index, fr.discontinuity, pcm));
            }
            f.clear();
        }
    }

    fn run_real(&mut self, x: &[f32], chunk: usize) {
        for c in x.chunks(chunk) {
            self.feed_after(PortSlice::Real(c), 0);
        }
    }
}

/// Tone power at `f` over total power, dB.
fn tone_db(x: &[f32], fs: f64, f: f64) -> f64 {
    let (mut re, mut im, mut total) = (0.0f64, 0.0f64, 0.0f64);
    for (n, &v) in x.iter().enumerate() {
        let ph = TAU * f * n as f64 / fs;
        re += f64::from(v) * ph.cos();
        im += f64::from(v) * ph.sin();
        total += f64::from(v).powi(2);
    }
    10.0 * ((2.0 * (re * re + im * im) / x.len() as f64) / total.max(1e-30)).log10()
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

/// A discriminator's output: `fm_demod` over FM IQ (1 kHz tone at ±5 kHz, carrier amplitude
/// `a`, or noise only when `a == 0`) plus complex noise of variance 0.01 (noise only) or 0.0005
/// (with a carrier: ≈ 33 dB C/N over the unfiltered band, ≈ 39 dB in a 12 kHz channel), at `fs`.
fn discriminator(n: usize, fs: f64, a: f64, seed: u64) -> Vec<f32> {
    let var = if a > 0.0 { 0.0005 } else { 0.01 };
    let mut rng = Lcg::new(seed);
    let iq: Vec<Complex32> = (0..n)
        .map(|k| {
            let t = k as f64 / fs;
            let ph = 5_000.0 / 1_000.0 * (TAU * 1_000.0 * t).sin();
            Complex32::new((a * ph.cos()) as f32, (a * ph.sin()) as f32) + rng.cnoise(var)
        })
        .collect();
    let mut fm = Drive::new(
        "fm_demod",
        json!({"deviation_hz": 5000.0}),
        PortType::Iq,
        fs,
        4096,
    );
    for c in iq.chunks(4096) {
        fm.feed_after(PortSlice::Iq(c), 0);
    }
    fm.real
}

/// C19 "squelch latency within attack/hang": noise alone keeps the FM noise squelch shut (no
/// items at all), a captured carrier opens it within its attack, and it closes after the hang.
#[test]
fn fm_noise_squelch_opens_on_a_carrier_and_emits_nothing_on_noise() {
    let fs = 48_000.0;
    let noise = discriminator(24_000, fs, 0.0, 1);
    let mut sq = Drive::new("squelch", json!({}), PortType::Real, fs, 960);
    sq.run_real(&noise, 960);
    assert!(
        sq.real.is_empty(),
        "noise alone emits nothing ({} items)",
        sq.real.len()
    );
    assert_eq!(sq.block.status().lock, Lock::Searching);
    let q = sq.block.status().snr_db.unwrap();
    assert!(q < 3.0, "noise quieting {q} dB");

    // A captured carrier, then noise again.
    let tone = discriminator(24_000, fs, 1.0, 2);
    let tail = discriminator(48_000, fs, 0.0, 3);
    let before = sq.index;
    sq.run_real(&tone, 960);
    let (first, _) = sq.metas[0];
    let opened_after = (first.source_index / sq.per) as u64 - before;
    // The out-of-band noise estimate has to fall ~15 dB through a 15 ms one-pole average:
    // about 4 × attack_s.
    assert!(
        (opened_after as f64) < 0.1 * fs,
        "opens within 100 ms of the carrier ({opened_after} items)"
    );
    assert!(
        first.flags.contains(ChunkFlags::DISCONTINUITY),
        "re-open is flagged"
    );
    assert!(
        tone_db(&sq.real[sq.real.len() / 2..], fs, 1_000.0) > -3.0,
        "the audio passes unchanged"
    );
    let emitted_with_carrier = sq.real.len();
    sq.run_real(&tail, 960);
    let hang_items = sq.real.len() - emitted_with_carrier;
    let hang_s = hang_items as f64 / fs;
    assert!(
        (0.45..0.7).contains(&hang_s),
        "closes after the 0.5 s hang (+ attack), stayed open {hang_s} s"
    );
    assert_eq!(sq.block.status().lock, Lock::Searching);
}

/// Every emitted chunk is one contiguous run: its time map places each item at its source time.
#[test]
fn squelch_output_keeps_the_source_time_map() {
    let fs = 48_000.0;
    let mut x = discriminator(12_000, fs, 0.0, 4);
    x.extend(discriminator(24_000, fs, 1.0, 5));
    let mut sq = Drive::new("squelch", json!({"hang_s": 0.0}), PortType::Real, fs, 1000);
    sq.run_real(&x, 1000);
    let mut k = 0;
    for (meta, len) in &sq.metas {
        let at = (meta.source_index / sq.per) as usize;
        assert_eq!(
            &sq.real[k..k + len],
            &x[at..at + len],
            "items are the input at their time"
        );
        k += len;
    }
    assert!(k > 20_000);
}

/// `snr` mode: without a noise reference it stays open (the Listen rule); with one above the
/// level it stays shut; a hot edit switches it.
#[test]
fn snr_squelch_needs_a_noise_reference() {
    let fs = 48_000.0;
    let x: Vec<f32> = (0..9_600)
        .map(|k| (0.1 * (TAU * 700.0 * k as f64 / fs).sin()) as f32)
        .collect();
    let mut open = Drive::new("squelch", json!({"mode": "snr"}), PortType::Real, fs, 960);
    open.run_real(&x, 960);
    assert_eq!(open.real, x, "no reference: everything passes");
    // Tone power −23 dBFS; a noise reference at −10 dBFS keeps it shut, at −40 opens it.
    let mut shut = Drive::new(
        "squelch",
        json!({"mode": "snr", "noise_dbfs": -10.0}),
        PortType::Real,
        fs,
        960,
    );
    shut.run_real(&x, 960);
    assert!(shut.real.is_empty());
    let up = crate::blocks::iq::testkit::update(
        shut.block.as_mut(),
        json!({"mode": "snr", "noise_dbfs": -40.0}),
        PortType::Real,
    );
    assert_eq!(up, crate::block::ParamUpdate::Applied);
    shut.run_real(&x, 960);
    assert!(shut.real.len() > 9_000, "{} items", shut.real.len());
}

/// AGC brings a quiet tone to its target, caps the gain, keeps its gain across a gap, and is a
/// pass-through when disabled.
#[test]
fn agc_reaches_its_target_and_is_capped() {
    let fs = 48_000.0;
    let tone = |a: f64| -> Vec<f32> {
        (0..48_000)
            .map(|k| (a * (TAU * 500.0 * k as f64 / fs).sin()) as f32)
            .collect()
    };
    let mut agc = Drive::new("agc", json!({}), PortType::Real, fs, 960);
    agc.run_real(&tone(0.01), 960);
    let tail = &agc.real[24_000..];
    let peak = tail.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!((peak - 0.5).abs() < 0.05, "peak {peak} ≈ −6 dBFS");
    let gain = agc
        .block
        .status()
        .extra
        .iter()
        .find(|(k, _)| *k == "gain_db")
        .unwrap()
        .1;
    assert!((gain - 34.0).abs() < 1.0, "gain {gain} dB");
    // A gap keeps the gain: the first samples after it are not blasted at +60 dB.
    let x = tone(0.01);
    agc.feed_after(PortSlice::Real(&x[..960]), 10_000);
    let after = &agc.real[agc.real.len() - 960..];
    assert!(after.iter().all(|v| v.abs() < 0.6), "no blast after a gap");

    let mut capped = Drive::new("agc", json!({"max_gain_db": 10.0}), PortType::Real, fs, 960);
    capped.run_real(&tone(0.01), 960);
    assert!((rms(&capped.real[24_000..]) / rms(&tone(0.01)) - 10f64.powf(0.5)).abs() < 0.05);

    let mut off = Drive::new("agc", json!({"enabled": false}), PortType::Real, fs, 960);
    let x = tone(0.01);
    off.run_real(&x, 960);
    assert_eq!(off.real, x);
}

/// De-emphasis is −3 dB at 1/(2πτ) and its τ is a hot edit.
#[test]
fn deemphasis_corner_and_hot_tau() {
    let fs = 240_000.0;
    let tau = 75e-6;
    let fc = 1.0 / (TAU * tau);
    let x: Vec<f32> = (0..48_000)
        .map(|k| (TAU * fc * k as f64 / fs).sin() as f32)
        .collect();
    let mut de = Drive::new(
        "deemphasis",
        json!({"tau_s": tau}),
        PortType::Real,
        fs,
        4096,
    );
    de.run_real(&x, 4096);
    let g = 20.0 * (rms(&de.real[24_000..]) / rms(&x[24_000..])).log10();
    assert!((g + 3.0).abs() < 0.3, "{g} dB at the corner");
    let up = crate::blocks::iq::testkit::update(
        de.block.as_mut(),
        json!({"tau_s": 0.0}),
        PortType::Real,
    );
    assert_eq!(up, crate::block::ParamUpdate::Applied);
    let n = de.real.len();
    de.run_real(&x[..4096], 4096);
    let err = de.real[n..]
        .iter()
        .zip(&x[..4096])
        .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
    assert!(err < 1e-6, "τ = 0 is a pass-through ({err})");
}

/// `audio_out`: 240 kS/s in → 960-sample 48 kS/s `ri16_le` frames; a 1 kHz tone passes, the
/// 19 kHz pilot is rejected; `sample_index` runs 0, 960, … and jumps by the gap's duration
/// after one, with the frame flagged and the cut-short frame dropped.
#[test]
fn audio_out_frames_48k_and_marks_gaps() {
    let fs = 240_000.0;
    let x: Vec<f32> = (0..240_000)
        .map(|k| {
            let t = k as f64 / fs;
            (0.4 * (TAU * 1_000.0 * t).sin() + 0.1 * (TAU * 19_000.0 * t).sin()) as f32
        })
        .collect();
    let mut out = Drive::new("audio_out", json!({}), PortType::Real, fs, 4800);
    assert!(out.outputs.is_empty(), "a sink has no output port");
    out.run_real(&x, 4800);
    let n = out.frames.len();
    assert!((48..=50).contains(&n), "{n} frames for 1 s");
    for (k, (idx, disc, pcm)) in out.frames.iter().enumerate() {
        assert_eq!(pcm.len(), 960);
        assert_eq!(*idx, 960 * k as u64, "contiguous sample_index");
        assert_eq!(*disc, k == 0, "only the stream start is flagged");
    }
    let audio: Vec<f32> = out.frames[5..]
        .iter()
        .flat_map(|(_, _, p)| p.iter().map(|&v| f32::from(v) / 32767.0))
        .collect();
    assert!(
        tone_db(&audio, 48_000.0, 1_000.0) > -0.5,
        "the tone is the audio"
    );
    let pilot = tone_db(&audio, 48_000.0, 19_000.0);
    assert!(pilot < -50.0, "pilot rejected ({pilot} dB)");

    // A 0.5 s gap: the next frame's index jumps by ≈ 24 000 and is flagged.
    let before = out.frames.len();
    let last = out.frames.last().unwrap().0;
    out.feed_after(PortSlice::Real(&x[..4800]), 120_000);
    out.run_real(&x[4800..24_000], 4800);
    let (idx, disc, _) = &out.frames[before];
    assert!(*disc);
    let jump = idx - last;
    assert!(
        (24_000 + 960..24_000 + 960 * 3).contains(&jump),
        "sample_index jump {jump} is the gap"
    );
    let dropped = out
        .block
        .status()
        .extra
        .iter()
        .find(|(k, _)| *k == "partial_dropped")
        .unwrap()
        .1;
    assert!(
        dropped > 0.0 && dropped < 960.0,
        "the cut-short frame is dropped ({dropped})"
    );
}

/// `audio_out` only reduces the rate, and its profile keys have one legal value each.
#[test]
fn audio_out_refuses_what_the_profile_cannot_serve() {
    let mut b = build("audio_out", json!({}), PortType::Real);
    let info = PortInfo {
        ty: PortType::Real,
        rate_hz: 24_000.0,
        max_items: 1024,
        hold_items: 0,
    };
    assert!(matches!(
        b.init(&[info]),
        Err(crate::block::BlockError::Unrealisable(_))
    ));
    let registry = crate::Registry::builtin();
    let maps = std::collections::BTreeMap::new();
    let ctx = crate::registry::BuildCtx {
        field_maps: &maps,
        input_types: &[PortType::Real],
    };
    for bad in [
        json!({"output_rate_hz": 44_100.0}),
        json!({"frame_samples": 480}),
        json!({"datatype": "rf32_le"}),
    ] {
        let p = crate::blocks::iq::testkit::params(bad.clone());
        assert!(registry.build("audio_out", &p, &ctx).is_err(), "{bad}");
    }
}
