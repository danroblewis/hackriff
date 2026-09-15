//! T-185: the RDS recipe (`recipes/rds.recipe.json`) on real air, at the recipe graph (the
//! pipeline's channel DDC + `Graph`, no scheduler): weak-signal robustness and block-sync
//! false-lock behaviour.
//!
//! - **Weak signal.** The real `fm_100p8M_2p4M_l32g30a1_t1p5_5s` capture (station RF SNR 18 dB)
//!   is degraded at run time with seeded AWGN to the 14 dB the live demo received, looped with
//!   fresh noise every pass (a discontinuity at each seam, as the looping mock SDR marks it).
//! - **No RDS, no lock.** Complex noise, and an FM broadcast multiplex with a pilot but no RDS,
//!   never lock block sync and surface no group.
//! - **Clean capture.** Block sync locks within the first second.
//!
//! The station's channel is found blindly (strongest 200 kHz of the Welch spectrum); the truth
//! (PI, PS frames) is read only for assertions.
//!
//! SNR is measured as the handoff's reference did: `(P in ±100 kHz − N0·B) / (N0·B)`, N0 the
//! median bin of a 4096-bin Welch PSD of the whole capture window.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use common::*;
use hk_blocks::{ChunkFlags, ChunkMeta, Input, PortInfo, PortSlice, PortVec, Registry};
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::welch::{WelchConfig, welch};
use hk_dsp::{Ddc, DdcSpec, InputInfo};
use hk_model::{CrcStatus, Provenance, SampleTime, Timestamp};
use hk_pipeline::recipes::graph::{Graph, Src};
use hk_pipeline::recipes::runtime::parse_recipe;
use hk_recipe::PortType;
use num_complex::Complex32;
use serde_json::Value;

const NAME: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
const TAG: &str = "SIGNAL-062/T-185 rds real air";
const CHUNK: usize = 1 << 16;

/// What one run of the recipe produced.
#[derive(Debug, Default)]
struct Report {
    secs: f64,
    crc_ok: u64,
    crc_bad: u64,
    /// Group frames out of `fields` carrying a layer tree although CRC-invalid.
    invalid_with_fields: u64,
    /// Valid groups' PI counts.
    pi: BTreeMap<u16, u64>,
    /// Assembled PS strings (`ps` text node).
    ps: Vec<String>,
    radiotext: Vec<String>,
    acquisitions: f64,
    /// Acquisitions (seen per chunk) that lost lock again without one CRC-valid group.
    barren_locks: u64,
    /// Diagnostic only (the crc block corrects nothing): groups whose every block is valid or
    /// fixed by a unique ≤2-bit burst (one differential-decoding channel-bit error), and those
    /// with a corrected block whose PI is not the valid groups' PI.
    burst2_groups: u64,
    burst2_pi_mismatch: u64,
    /// Seconds of input before block sync first locked.
    first_lock_s: Option<f64>,
    status: serde_json::Map<String, Value>,
}

impl Report {
    fn crc_rate(&self) -> f64 {
        self.crc_ok as f64 / (self.crc_ok + self.crc_bad).max(1) as f64
    }

    fn top_pi(&self) -> Option<u16> {
        self.pi.iter().max_by_key(|(_, c)| **c).map(|(p, _)| *p)
    }

    fn line(&self) -> String {
        format!(
            "{:.1} s: CRC-valid groups {}/{} = {:.3}; invalid groups with fields {}; PI {:?} \
             {:?}; PS {:?}; RT {}; sync acquisitions {}; barren locks {}; first lock {:?} s; \
             [diag ≤2-bit burst: groups {}, corrected PI mismatches {}]",
            self.secs,
            self.crc_ok,
            self.crc_ok + self.crc_bad,
            self.crc_rate(),
            self.invalid_with_fields,
            self.top_pi().map(|p| format!("{p:04X}")),
            self.pi,
            self.ps,
            self.radiotext.len(),
            self.acquisitions,
            self.barren_locks,
            self.first_lock_s,
            self.burst2_groups,
            self.burst2_pi_mismatch,
        )
    }
}

fn recipe_doc() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../recipes/rds.recipe.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Merges `{node_id: {param: value}, "input": {...}}` into the recipe (diagnostics only).
fn patch(doc: &mut Value, patch: &Value) {
    let Some(p) = patch.as_object() else { return };
    for (k, v) in p {
        if k == "input" {
            for (ik, iv) in v.as_object().unwrap() {
                doc["input"][ik] = iv.clone();
            }
            continue;
        }
        let node = doc["nodes"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|n| n["id"] == *k)
            .unwrap_or_else(|| panic!("patch names no node {k}"));
        for (pk, pv) in v.as_object().unwrap() {
            node["params"][pk] = pv.clone();
        }
    }
}

fn text_of(info: &hk_blocks::FrameInfo, path: &str) -> Option<String> {
    let tree = info.layers.as_ref()?;
    tree.nodes
        .iter()
        .find(|n| n.path == path)?
        .value
        .as_ref()?
        .as_str()
        .map(str::to_owned)
}

/// Runs the RDS recipe over `passes` of IQ at `fs`, the channel `offset_hz` from the capture
/// centre. Each pass starts with a discontinuity.
fn run_recipe(
    passes: &mut dyn Iterator<Item = Vec<Complex32>>,
    fs: f64,
    offset_hz: f64,
    doc: Value,
) -> Report {
    let recipe = Arc::new(parse_recipe(doc).unwrap());
    let rate = recipe.input.sample_rate_hz.unwrap();
    let bw = recipe.input.bandwidth_hz.unwrap();
    let max_items = (CHUNK as f64 * rate / fs).ceil() as usize + 1024;
    let info = PortInfo {
        ty: PortType::Iq,
        rate_hz: rate,
        max_items,
        hold_items: 0,
    };
    let (mut g, _) = Graph::build(recipe, &Registry::builtin(), info).unwrap();
    let ids: Vec<String> = g.node_status().into_iter().map(|(id, _, _)| id).collect();
    let pos = |id: &str| ids.iter().position(|x| x == id).unwrap();
    let (p_sync, p_group, p_ps, p_rt) = (pos("sync"), pos("group"), pos("ps"), pos("rt"));
    let bursts = burst_syndromes();
    let mut burst_pis: Vec<u16> = Vec::new();
    let (mut locked, mut valid_in_lock) = (false, 0u64);
    let prov = provenance();
    let mut rep = Report::default();
    let (mut idx, mut out_items) = (0u64, 0u64);
    for iq in passes {
        let mut ddc = Ddc::new(DdcSpec::new(offset_hz, bw).with_output_rate(rate), fs).unwrap();
        assert_eq!(ddc.output_rate_hz(), rate, "DDC {fs} → {rate} S/s");
        for (k, c) in iq.chunks(CHUNK).enumerate() {
            let t = SampleTime {
                sample_index: idx,
                host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
            };
            let info = InputInfo {
                time: t,
                discontinuity: if k == 0 {
                    Discontinuity::STREAM_START
                } else {
                    Discontinuity::NONE
                },
                dropped_before: 0,
                provenance: &prov,
            };
            let block = ddc.process(info, c).unwrap();
            let meta = ChunkMeta {
                index: out_items,
                source_index: block.header.time.source_index as f64,
                source_per_item: block.header.time.source_per_output as f64,
                rate_hz: rate,
                channel: 0,
                flags: if k == 0 {
                    ChunkFlags::DISCONTINUITY
                } else {
                    ChunkFlags::NONE
                },
            };
            out_items += block.samples.len() as u64;
            idx += c.len() as u64;
            g.process(Input {
                meta,
                data: PortSlice::Iq(block.samples),
            })
            .unwrap();
            let frames = |pos: usize| match g.output(Src::Node { pos, port: 0 }).map(|o| &o.data) {
                Some(PortVec::Frames(f)) => Some(f),
                _ => None,
            };
            if let Some(f) = frames(p_sync) {
                for fr in f.iter() {
                    if let Some((pi, corrected)) = burst2_group(fr.bytes, &bursts) {
                        rep.burst2_groups += 1;
                        if corrected {
                            burst_pis.push(pi);
                        }
                    }
                }
            }
            if let Some(f) = frames(p_group) {
                for fr in f.iter() {
                    match fr.info.check {
                        CrcStatus::Valid => {
                            valid_in_lock += 1;
                            rep.crc_ok += 1;
                            let pi = u16::from_be_bytes([fr.bytes[0], fr.bytes[1]]);
                            *rep.pi.entry(pi).or_default() += 1;
                        }
                        _ => {
                            rep.crc_bad += 1;
                            if fr.info.layers.is_some() {
                                rep.invalid_with_fields += 1;
                            }
                        }
                    }
                }
            }
            if let Some(f) = frames(p_ps) {
                rep.ps
                    .extend(f.iter().filter_map(|fr| text_of(fr.info, "ps.text")));
            }
            if let Some(f) = frames(p_rt) {
                rep.radiotext
                    .extend(f.iter().filter_map(|fr| text_of(fr.info, "radiotext.text")));
            }
            let now = g
                .node_status()
                .into_iter()
                .any(|(id, _, st)| id == "sync" && st.lock == hk_blocks::Lock::Locked);
            if now && rep.first_lock_s.is_none() {
                rep.first_lock_s = Some(idx as f64 / fs);
            }
            if locked && !now && valid_in_lock == 0 {
                rep.barren_locks += 1;
            }
            if now && !locked {
                valid_in_lock = 0;
            }
            locked = now;
        }
    }
    rep.secs = idx as f64 / fs;
    let top = rep.top_pi();
    rep.burst2_pi_mismatch = burst_pis.iter().filter(|&&p| Some(p) != top).count() as u64;
    for (id, _, st) in g.node_status() {
        st.to_metadata(&id, &mut rep.status);
    }
    rep.acquisitions = rep
        .status
        .get("sync.acquisitions")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    rep
}

/// Syndromes of the ≤2-bit bursts in a 26-bit block: (error word, syndrome).
fn burst_syndromes() -> Vec<(u32, u16)> {
    use hk_demod::rds::block::syndrome;
    let mut v: Vec<(u32, u16)> = (0..26)
        .map(|i| 1u32 << i)
        .collect::<Vec<_>>()
        .into_iter()
        .map(|e| (e, syndrome(e)))
        .collect();
    v.extend((0..25).map(|i| 0b11u32 << i).map(|e| (e, syndrome(e))));
    v
}

/// A 104-bit synced group whose blocks are all valid or uniquely burst-correctable: (PI, any
/// block corrected).
fn burst2_group(bytes: &[u8], bursts: &[(u32, u16)]) -> Option<(u16, bool)> {
    use hk_demod::rds::Offset;
    use hk_demod::rds::block::syndrome;
    let bits = |from: usize, n: usize| -> u32 {
        (from..from + n).fold(0u32, |a, i| {
            (a << 1) | u32::from((bytes[i / 8] >> (7 - i % 8)) & 1)
        })
    };
    let fits = |s: u16, slot: usize| Offset::from_syndrome(s).is_some_and(|o| o.fits_slot(slot));
    let (mut pi, mut corrected) = (0u16, false);
    for slot in 0..4 {
        let mut w = bits(26 * slot, 26);
        let s = syndrome(w);
        if !fits(s, slot) {
            let mut fixes = bursts.iter().filter(|(_, bs)| fits(s ^ bs, slot));
            let (e, _) = fixes.next()?;
            if fixes.next().is_some() {
                return None;
            }
            w ^= e;
            corrected = true;
        }
        if slot == 0 {
            pi = (w >> 10) as u16;
        }
    }
    Some((pi, corrected))
}

/// The capture's recorded provenance (the meta file is committed, not LFS).
fn provenance() -> ProvenanceHandle {
    let meta = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta"
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(meta).unwrap()).unwrap();
    let prov: Provenance =
        serde_json::from_value(v["global"]["hackriff:provenance"].clone()).unwrap();
    ProvenanceHandle::new(prov)
}

/// (SNR dB, N0 per Hz, signal power, channel offset Hz): the strongest 200 kHz channel.
fn measure(iq: &[Complex32], fs: f64, offset_hz: Option<f64>) -> (f64, f64, f64, f64) {
    let n = iq.len().min(4 << 20);
    let s = welch(
        &iq[..n],
        fs,
        0.0,
        &WelchConfig {
            holds: false,
            spectral_kurtosis: false,
            ..WelchConfig::new(4096)
        },
    )
    .unwrap();
    let bins = s.psd.len();
    let df = fs / bins as f64;
    let f = |i: usize| (i as f64 - (bins / 2) as f64) * df;
    let mut sorted = s.psd.clone();
    sorted.sort_by(f32::total_cmp);
    let n0 = f64::from(sorted[bins / 2]);
    let half = (100e3 / df).round() as usize;
    let band = |c: usize| -> f64 {
        s.psd[c - half..=c + half]
            .iter()
            .map(|&p| f64::from(p))
            .sum::<f64>()
            * df
    };
    let centre = match offset_hz {
        Some(o) => (o / df).round() as usize + bins / 2,
        None => {
            // The strongest 200 kHz, then the power centroid above the floor (FM is symmetric
            // about its carrier; a box maximum is flat across tens of kHz).
            let mut c = (half..bins - half)
                .max_by(|&a, &b| band(a).total_cmp(&band(b)))
                .unwrap();
            for _ in 0..3 {
                let (mut w, mut m) = (0.0, 0.0);
                for i in c - half..=c + half {
                    let p = (f64::from(s.psd[i]) - n0).max(0.0);
                    w += p;
                    m += p * i as f64;
                }
                c = ((m / w).round() as usize).clamp(half, bins - half - 1);
            }
            c
        }
    };
    let b = (2 * half + 1) as f64 * df;
    let ps = band(centre);
    let sig = ps - n0 * b;
    (10.0 * (sig / (n0 * b)).log10(), n0, sig, f(centre))
}

/// xorshift64* → Box–Muller.
struct Gauss(u64);

impl Gauss {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        ((self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    fn pair(&mut self, sigma: f64) -> Complex32 {
        let (u, v) = (self.uniform(), self.uniform());
        let r = sigma * (-2.0 * u.ln()).sqrt();
        let th = std::f64::consts::TAU * v;
        Complex32::new((r * th.cos()) as f32, (r * th.sin()) as f32)
    }
}

/// Adds complex AWGN of total variance `var`.
fn add_noise(iq: &[Complex32], var: f64, rng: &mut Gauss) -> Vec<Complex32> {
    let sigma = (var / 2.0).sqrt();
    iq.iter().map(|&z| z + rng.pair(sigma)).collect()
}

fn load(meta: &Path) -> (Vec<Complex32>, f64, hk_e2e::TruthItem) {
    let fx = hk_e2e::Fixture::load(meta).unwrap();
    let iq = fx
        .samples()
        .unwrap()
        .into_iter()
        .map(|s| Complex32::new(s.re, s.im))
        .collect();
    let truth = fx
        .of_kind("wfm-broadcast")
        .first()
        .map(|t| (*t).clone())
        .expect("truth station");
    (iq, fx.sample_rate, truth)
}

/// Weak-signal run: the capture degraded to `snr_db`, `passes` passes with fresh noise.
fn weak(
    iq: &[Complex32],
    fs: f64,
    snr_db: f64,
    passes: usize,
    seed: u64,
    doc: Value,
) -> (Report, f64, f64) {
    let (snr0, n0, sig, offset) = measure(iq, fs, None);
    let b = (2.0 * (100e3 / (fs / 4096.0)).round() + 1.0) * fs / 4096.0;
    let n0_target = sig / (b * 10f64.powf(snr_db / 10.0));
    let var = (n0_target - n0).max(0.0) * fs;
    let mut rng = Gauss(seed | 1);
    let first = add_noise(iq, var, &mut rng);
    let (snr, ..) = measure(&first, fs, Some(offset));
    eprintln!("[{TAG}] channel {offset:.0} Hz from centre; SNR {snr0:.1} dB → {snr:.1} dB");
    let mut first = Some(first);
    let mut it = (0..passes).map(|_| first.take().unwrap_or_else(|| add_noise(iq, var, &mut rng)));
    (run_recipe(&mut it, fs, offset, doc), snr, offset)
}

fn truth_ps_frames(truth: &hk_e2e::TruthItem) -> Vec<String> {
    truth
        .get("/rds/ps_frames")
        .and_then(Value::as_object)
        .expect("truth PS frames")
        .keys()
        .cloned()
        .collect()
}

fn truth_pi(truth: &hk_e2e::TruthItem) -> u16 {
    u16::from_str_radix(truth.str("/rds/pi_hex").expect("truth PI"), 16).unwrap()
}

/// Env-driven diagnostic (not part of the suite): T185_IQ (raw ci8 paths, comma-separated,
/// contiguous) or the fixture; T185_SNR_DB, T185_PASSES, T185_SEED, T185_SECS, T185_PATCH (recipe
/// patch JSON).
#[test]
#[ignore]
fn t185_rds_diag() {
    let env_f = |k: &str, d: f64| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let mut doc = recipe_doc();
    if let Ok(p) = std::env::var("T185_PATCH") {
        patch(&mut doc, &serde_json::from_str(&p).unwrap());
    }
    let fs = 2.4e6;
    if std::env::var("T185_NOISE").is_ok() {
        let mut rng = Gauss(env_f("T185_SEED", 185.0) as u64);
        let mut it = (0..10).map(|_| {
            (0..(2.5 * fs) as usize)
                .map(|_| rng.pair(0.1))
                .collect::<Vec<_>>()
        });
        let r = run_recipe(&mut it, fs, 0.0, doc);
        eprintln!("RESULT noise {}", r.line());
        return;
    }
    if let Ok(paths) = std::env::var("T185_IQ") {
        let mut raw = Vec::new();
        for p in paths.split(',') {
            raw.extend(std::fs::read(p).unwrap());
        }
        let n = (raw.len() / 2).min((env_f("T185_SECS", 1e9) * fs) as usize);
        let iq: Vec<Complex32> = (0..n)
            .map(|i| {
                Complex32::new(
                    raw[2 * i] as i8 as f32 / 127.0,
                    raw[2 * i + 1] as i8 as f32 / 127.0,
                )
            })
            .collect();
        drop(raw);
        let (snr, _, _, offset) = measure(&iq, fs, None);
        eprintln!("live: channel {offset:.0} Hz, SNR {snr:.1} dB");
        let r = run_recipe(&mut std::iter::once(iq), fs, offset, doc);
        eprintln!("RESULT live {}", r.line());
        eprintln!("STATUS {}", Value::Object(r.status));
        return;
    }
    let meta = real_fixture(NAME).unwrap();
    let (iq, fs, _) = load(&meta);
    let (r, snr, _) = weak(
        &iq,
        fs,
        env_f("T185_SNR_DB", 99.0),
        env_f("T185_PASSES", 5.0) as usize,
        env_f("T185_SEED", 185.0) as u64,
        doc,
    );
    eprintln!("RESULT fixture @ {snr:.1} dB {}", r.line());
    eprintln!("STATUS {}", Value::Object(r.status));
}

/// Weak signal: the capture at 14 dB RF SNR (the live demo's), 25 s with fresh noise per pass.
/// A-priori target: PI decoded, a complete PS name, and no CRC-invalid group surfaced as fields.
#[test]
fn signal_062_rds_recipe_weak_signal_14db_decodes_pi_and_complete_ps_without_invalid_fields() {
    let Some(meta) = real_fixture(NAME) else {
        return;
    };
    let (iq, fs, truth) = load(&meta);
    let (r, snr, _) = weak(&iq, fs, 14.0, 5, 185, recipe_doc());
    eprintln!("[{TAG}] RESULT 14 dB: {}", r.line());
    assert!((snr - 14.0).abs() < 0.3, "[{TAG}] degraded SNR {snr:.2} dB");
    assert!(r.secs >= 24.9, "[{TAG}] {} s", r.secs);
    assert_eq!(r.top_pi(), Some(truth_pi(&truth)), "[{TAG}] PI {:?}", r.pi);
    assert_eq!(
        r.pi.len(),
        1,
        "[{TAG}] one PI among CRC-valid groups: {:?}",
        r.pi
    );
    let known = truth_ps_frames(&truth);
    assert!(!r.ps.is_empty(), "[{TAG}] no complete PS at 14 dB");
    assert!(
        r.ps.iter()
            .all(|p| p.chars().count() == 8 && known.contains(p)),
        "[{TAG}] PS {:?} not all complete names in {known:?}",
        r.ps
    );
    assert_eq!(
        r.invalid_with_fields, 0,
        "[{TAG}] CRC-invalid groups surfaced as fields"
    );
    assert!(
        r.crc_bad > 0,
        "[{TAG}] the test must exercise invalid groups"
    );
    // Regression floor (measured 0.368 with seed 185; the hard-decision limit, see T-185).
    assert!(
        r.crc_rate() >= 0.3,
        "[{TAG}] CRC-valid rate {:.3}",
        r.crc_rate()
    );
}

/// The clean capture: block sync locks within half a second, every lock yields CRC-valid
/// groups, and invalid groups carry no fields.
#[test]
fn signal_062_rds_recipe_clean_capture_locks_fast_without_barren_locks() {
    let Some(meta) = real_fixture(NAME) else {
        return;
    };
    let (iq, fs, truth) = load(&meta);
    let (_, _, _, offset) = measure(&iq, fs, None);
    let r = run_recipe(&mut std::iter::once(iq), fs, offset, recipe_doc());
    eprintln!("[{TAG}] RESULT clean: {}", r.line());
    assert!(
        r.first_lock_s.is_some_and(|t| t <= 0.5),
        "[{TAG}] first lock {:?}",
        r.first_lock_s
    );
    assert_eq!(r.acquisitions, 1.0, "[{TAG}] one acquisition over 5 s");
    assert_eq!(r.barren_locks, 0);
    assert!(
        r.crc_rate() >= 0.7,
        "[{TAG}] CRC-valid rate {:.3}",
        r.crc_rate()
    );
    assert_eq!(r.top_pi(), Some(truth_pi(&truth)));
    assert_eq!(r.invalid_with_fields, 0);
}

/// FM broadcast multiplex at `fs`: mono tones, a 19 kHz pilot (7.5 kHz deviation) and an L−R
/// tone on 38 kHz, no 57 kHz subcarrier; 75 kHz scale, 30 dB CNR.
fn fm_without_rds(secs: f64, fs: f64, rng: &mut Gauss) -> Vec<Complex32> {
    use std::f64::consts::TAU;
    let mut phase = 0.0f64;
    (0..(secs * fs) as usize)
        .map(|i| {
            let t = i as f64 / fs;
            let mono = 0.15 * (TAU * 400.0 * t).sin() + 0.1 * (TAU * 2_500.0 * t).sin();
            let lr = 0.1 * (TAU * 1_700.0 * t).sin() * (TAU * 38_000.0 * t).cos();
            let mpx = mono + lr + 0.1 * (TAU * 19_000.0 * t).cos();
            phase += TAU * 75e3 * mpx / fs;
            Complex32::from_polar(1.0, phase as f32) + rng.pair(10f64.powf(-3.0 / 2.0))
        })
        .collect()
}

/// No RDS, no lock: complex noise and an FM multiplex without RDS never acquire block sync,
/// emit no group and surface no field.
#[test]
fn signal_062_rds_recipe_never_locks_on_noise_or_fm_without_rds() {
    // The capture's rate: the DDC takes its input rate from the recorded provenance.
    let fs = 2.4e6;
    let mut rng = Gauss(185);
    let mut noise = (0..4).map(|_| (0..(2.5 * fs) as usize).map(|_| rng.pair(0.1)).collect());
    let r = run_recipe(&mut noise, fs, 0.0, recipe_doc());
    eprintln!("[{TAG}] RESULT noise: {}", r.line());
    let mut fm = (0..4).map(|_| fm_without_rds(2.5, fs, &mut rng));
    let f = run_recipe(&mut fm, fs, 0.0, recipe_doc());
    let stages: Vec<String> = f
        .status
        .iter()
        .filter(|(k, _)| k.starts_with("rds57") || k.starts_with("fm."))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    eprintln!("[{TAG}] RESULT FM without RDS: {} {stages:?}", f.line());
    assert_eq!(
        f.status.get("rds57.pilot_locked").and_then(Value::as_f64),
        Some(1.0),
        "[{TAG}] the FM multiplex is demodulated (pilot locked)"
    );
    for (what, r) in [("noise", &r), ("FM without RDS", &f)] {
        assert_eq!(r.acquisitions, 0.0, "[{TAG}] {what}: sync acquired");
        assert!(r.first_lock_s.is_none(), "[{TAG}] {what}: sync locked");
        assert_eq!(r.crc_ok + r.crc_bad, 0, "[{TAG}] {what}: groups emitted");
        assert!(
            r.ps.is_empty() && r.radiotext.is_empty(),
            "[{TAG}] {what}: text emitted"
        );
    }
}
