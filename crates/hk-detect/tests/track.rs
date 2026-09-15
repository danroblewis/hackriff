//! Burst tracking (C10, T-007) acceptance:
//! - AWARE-036 (`fsk_burst_train`, replayed as ci8 through STFT → floor → detector → tracker):
//!   one Track; period within 2 %, duty cycle within 0.02, burst length within 1.5 frames; tone
//!   lobes merged.
//! - AWARE-042 (`occupancy_multi_hour` render windows): one track per active channel whose
//!   on-time, burst length and duty cycle match the schedule.
//! - Hopper (10 channels on a 200 kHz raster, 50 hops/s): one hop set with those channels, the
//!   raster and the hop rate.
//! - Two close emitters interleaved: separate tracks. Tracks and links round-trip through the
//!   repository.
//! - Gain change mid-track: same tracks, a `GainChange` segment recorded, the carrier stays one
//!   burst across the transition and its max-duration splits.
//! - Record-level: a converging fragment merges (recorded, not overwritten); a split-frame fused
//!   box is spread back onto its two tracks.
//! - T-031: a bursty hopper (packets separated by 100–310 ms of silence) forms one hop set;
//!   independent periodic emitters on one raster do not; two emitters sharing a track split
//!   (`TrackSplit`, `split_from`); low-SNR fragments stay tentative (no `Opened`); a merged
//!   track's links are re-pointed to the survivor in the repository.
//! - Real 915 MHz FHSS fixture: whether the bursts form a hop set on the 200 kHz raster (report).
//!
//! The continuous-carrier-over-hours bounded-memory test lives in `track_no_alloc.rs`.

mod common;

use common::*;
use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_detect::track::HopSetSummary;
use hk_detect::{
    BandProfile, BoundaryKind, Candidate, ClipCount, CloseReason, DetectionProfile,
    DetectionRecord, DetectionWriter, Detector, DetectorConfig, DetectorEvent, TrackBatch,
    TrackEvent, TrackSummary, Tracker, TrackerConfig, count_clipped_ci8,
};
use hk_dsp::floor::NoiseFloorTracker;
use hk_dsp::window::WindowKind;
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_e2e::{Fixture, SynthRequest, synth_or_skip};
use hk_model::{
    Detection, DetectionFlags, DetectionId, FreqRange, GainTableEntry, PageRequest, PlanRegion,
    Region, Repository, SampleTime, ScanPlan, ScanPlanId, ScanPolicy, Schedule, SegmentKind,
    Survey, SurveyId, SurveyState, TimeRange, Timestamp, TimingFeatures, Track, TrackFilter,
    TrackId, TrackState,
};
use num_complex::Complex;
use std::time::Instant;

const AWARE_036: &[&str] = &["AWARE-036"];
const AWARE_042: &[&str] = &["AWARE-042"];

// ---- drivers ----

#[derive(Default)]
struct Out {
    records: Vec<DetectionRecord>,
    events: Vec<TrackEvent>,
}

impl Out {
    fn closed(&self) -> Vec<&TrackSummary> {
        self.events
            .iter()
            .filter_map(|e| match e {
                TrackEvent::Closed(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    fn hop_sets_formed(&self) -> Vec<&HopSetSummary> {
        self.events
            .iter()
            .filter_map(|e| match e {
                TrackEvent::HopSetFormed(h) => Some(h),
                _ => None,
            })
            .collect()
    }

    fn hop_sets_closed(&self) -> Vec<&HopSetSummary> {
        self.events
            .iter()
            .filter_map(|e| match e {
                TrackEvent::HopSetClosed(h) => Some(h),
                _ => None,
            })
            .collect()
    }

    fn describe(&self, s: &TrackSummary) -> String {
        format!(
            "track {:.4} MHz bw {:.1} kHz dets {} bursts {} on {:.3} s obs {:.3} s duty {:?} period {:?} len {:?} segs {} hop {:?}",
            s.track.f_center_hz / 1e6,
            s.track.bandwidth_hz / 1e3,
            s.track.detection_count,
            s.burst_count,
            s.on_time_s,
            s.observed_s,
            s.track.timing.duty_cycle,
            s.period,
            s.burst_length.map(|d| (d.mean_s, d.min_s, d.max_s)),
            s.segments,
            s.hop_set,
        )
    }
}

/// One Gamma-frame step: detector, then the tracker takes the frame's events and observes it.
fn step(s: &mut Scene, tr: &mut Tracker, out: &mut Out, profile: &[f32], flags: Discontinuity) {
    s.step_with(profile, flags, ClipCount::NONE, false);
    forward(s, tr, out);
    tr.observe_frame(&s.det, &s.frame, &mut |e| out.events.push(e));
}

fn forward(s: &mut Scene, tr: &mut Tracker, out: &mut Out) {
    for r in s.out.detections.drain(..) {
        tr.push_detection(&r, &mut |e| out.events.push(e));
        out.records.push(r);
    }
    for c in s.out.confirmations.drain(..) {
        tr.confirm(&c);
    }
    s.out.evaluations.clear();
}

fn finish(s: &mut Scene, tr: &mut Tracker, out: &mut Out) {
    s.finish();
    forward(s, tr, out);
    tr.finish(&mut |e| out.events.push(e));
}

/// ci8 IQ → STFT → floor tracker → detector → tracker (one capture).
fn replay_tracked(
    samples: &[Complex<i8>],
    fs: f64,
    prov: ProvenanceHandle,
    chain: &ChainConfig,
    det: &mut Detector,
    tr: &mut Tracker,
) -> Out {
    let welch = WelchConfig {
        fft_len: chain.fft_len,
        overlap: 0,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: true,
    };
    let mut stft = StftProcessor::new(StftConfig::new(welch, chain.averages)).expect("stft");
    let mut floor = NoiseFloorTracker::new(chain.floor).expect("floor");
    let mut out = Out::default();
    let n = samples.len() as u64;
    let mut s = 0u64;
    while s < n {
        let e = (s + 65_536).min(n);
        let header = BlockHeader {
            time: SampleTime {
                sample_index: s,
                host_time: Timestamp::from_unix_nanos((s as f64 * 1e9 / fs).round() as i64),
            },
            provenance: prov.clone(),
            discontinuity: if s == 0 {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
        };
        stft.push(
            InputInfo::from(&header),
            &samples[s as usize..e as usize],
            |frame| {
                let f = floor.update(frame, |_| {});
                let a = frame.t.sample_index as usize;
                let b = (a + frame.sample_count as usize).min(samples.len());
                let clip = ClipCount::new(count_clipped_ci8(&samples[a..b]), (b - a) as u64);
                det.process(frame, f, clip, &mut |ev| {
                    tr.push(&ev, &mut |te| out.events.push(te));
                    if let DetectorEvent::Detection(r) = ev {
                        out.records.push(r);
                    }
                });
                tr.observe_frame(det, frame, &mut |te| out.events.push(te));
            },
        );
        s = e;
    }
    det.finish(&mut |ev| {
        tr.push(&ev, &mut |te| out.events.push(te));
        if let DetectorEvent::Detection(r) = ev {
            out.records.push(r);
        }
    });
    tr.finish(&mut |te| out.events.push(te));
    out
}

// ---- AWARE-036 ----

#[test]
fn aware_036_fsk_burst_train_one_track_with_period_duty_and_burst_length() {
    let synth = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(1)
            .param("snr_db", 20.0)
            .param("duration_s", 3.0)
    );
    let fx = synth.fixture(0).unwrap();
    let iq = to_ci8(&fx.samples().unwrap());
    // 256×4 (2 ms frames) for the duty/length tolerances; 512×5 (1 kHz bins) resolves the two
    // tones and sidelobes as separate boxes, which must merge into one burst each.
    for (fft_len, averages, lobes) in [(256, 4, false), (512, 5, true)] {
        aware_036_run(&fx, &iq, ChainConfig::new(fft_len, averages), lobes);
    }
}

fn aware_036_run(fx: &Fixture, iq: &[Complex<i8>], chain: ChainConfig, lobes: bool) {
    let fs = fx.sample_rate;
    let frame_s = chain.frame_period_s(fs);
    let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).unwrap();
    let mut tr = Tracker::new(TrackerConfig::default());
    let out = replay_tracked(iq, fs, fixture_provenance(fx), &chain, &mut det, &mut tr);
    let bursts = fx.of_kind("fsk-burst");
    let t0 = bursts[0];
    let (f_lo, f_hi) = (t0.f_lo_hz, t0.f_hi_hz);
    let truth_len =
        bursts.iter().map(|b| b.t_end_s - b.t_start_s).sum::<f64>() / bursts.len() as f64;
    let truth_period = 0.12;
    let closed = out.closed();
    for s in &closed {
        eprintln!("AWARE-036 {}", out.describe(s));
    }
    eprintln!(
        "AWARE-036 {}x{}: {} bursts, {} records, stats {:?}",
        chain.fft_len,
        chain.averages,
        bursts.len(),
        out.records.len(),
        tr.stats()
    );
    let near: Vec<_> = closed
        .iter()
        .filter(|s| s.track.f_center_hz >= f_lo - 20e3 && s.track.f_center_hz <= f_hi + 20e3)
        .collect();
    assert_eq!(near.len(), 1, "{AWARE_036:?} tracks near the emitter");
    let t = near[0];
    assert_eq!(
        closed.len(),
        1,
        "{AWARE_036:?} no other tracks (no false alarms)"
    );
    assert_eq!(t.burst_count as usize, bursts.len(), "{AWARE_036:?} bursts");
    if lobes {
        assert!(
            tr.stats().lobe_parts > 0 && t.track.detection_count > t.burst_count,
            "{AWARE_036:?} tone-lobe boxes merged into bursts"
        );
    }
    assert_eq!(
        t.track.detection_count as usize,
        out.records
            .iter()
            .filter(|r| r.f_hi_hz >= f_lo - 20e3 && r.f_lo_hz <= f_hi + 20e3)
            .count(),
        "{AWARE_036:?} every emitter detection links to the one track"
    );
    let p = t.period.expect("period");
    assert!(
        (p.period_s - truth_period).abs() / truth_period <= 0.02,
        "{AWARE_036:?} period {p:?}"
    );
    assert_eq!(t.track.timing.period_s, Some(p.period_s));
    let duty = t.track.timing.duty_cycle.unwrap();
    let truth_duty = truth_len / truth_period;
    let len = t.burst_length.unwrap();
    eprintln!(
        "AWARE-036 {}x{}: duty {duty:.4} vs {truth_duty:.4}; length {:.4} vs {truth_len:.4} s",
        chain.fft_len, chain.averages, len.mean_s
    );
    // Duty and length are frame-quantised: asserted at the 2 ms geometry.
    if !lobes {
        assert!(
            (duty - truth_duty).abs() <= 0.02,
            "{AWARE_036:?} duty {duty:.4} vs {truth_duty:.4}"
        );
    }
    assert!(
        (len.mean_s - truth_len).abs() <= 1.5 * frame_s,
        "{AWARE_036:?} burst length {len:?} vs {truth_len:.4} s (frame {frame_s:.4} s)"
    );
    assert!(t.next_burst_eta.is_some() && t.hop_set.is_none());
}

// ---- AWARE-042 ----

#[test]
fn aware_042_occupancy_windows_per_channel_tracks_match_the_schedule() {
    let synth = synth_or_skip!(
        SynthRequest::new("occupancy_multi_hour")
            .seed(5)
            .param("windows", 6)
    );
    let schedule = synth.file_json("schedule.json").unwrap();
    let channels: Vec<f64> = schedule["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["center_hz"].as_f64().unwrap())
        .collect();
    let spacing = channels[1] - channels[0];
    let chain = ChainConfig::new(128, 10);
    let mut checked = 0;
    for fx in synth.fixtures().unwrap() {
        let fs = fx.sample_rate;
        let frame_s = chain.frame_period_s(fs);
        let iq = to_ci8(&fx.samples().unwrap());
        let window_s = iq.len() as f64 / fs;
        let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).unwrap();
        let mut tr = Tracker::new(TrackerConfig::default());
        let out = replay_tracked(&iq, fs, fixture_provenance(&fx), &chain, &mut det, &mut tr);
        let truth = fx.of_kind("nbfm-burst");
        let name = fx
            .meta_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let closed = out.closed();
        for (c, &ch) in channels.iter().enumerate() {
            let visible: Vec<_> = truth
                .iter()
                .filter(|t| t.f64("channel").map(|x| x as usize) == Some(c))
                .filter(|t| t.t_end_s.min(window_s) - t.t_start_s.max(0.0) >= 4.0 * frame_s)
                .collect();
            let tracks: Vec<_> = closed
                .iter()
                .filter(|s| (s.track.f_center_hz - ch).abs() <= spacing / 2.0)
                .collect();
            for s in &tracks {
                eprintln!("AWARE-042 {name} ch{c}: {}", out.describe(s));
            }
            if visible.is_empty() {
                assert!(
                    tracks.len() <= 1,
                    "{AWARE_042:?} {name} ch{c}: stray tracks"
                );
                continue;
            }
            assert_eq!(tracks.len(), 1, "{AWARE_042:?} {name} ch{c}: one track");
            let t = tracks[0];
            let on: f64 = visible
                .iter()
                .map(|v| v.t_end_s.min(window_s) - v.t_start_s.max(0.0))
                .sum();
            let first = visible
                .iter()
                .map(|v| v.t_start_s.max(0.0))
                .fold(f64::INFINITY, f64::min);
            let longest = visible
                .iter()
                .map(|v| v.t_end_s.min(window_s) - v.t_start_s.max(0.0))
                .fold(0.0, f64::max);
            assert!(
                (t.on_time_s - on).abs() <= 3.0 * frame_s,
                "{AWARE_042:?} {name} ch{c}: on-time {:.4} vs {on:.4}",
                t.on_time_s
            );
            let len = t.burst_length.unwrap();
            assert!(
                (len.max_s - longest).abs() <= 3.0 * frame_s,
                "{AWARE_042:?} {name} ch{c}: burst length {len:?} vs {longest:.4}"
            );
            let span = window_s - first;
            if span >= 20.0 * frame_s {
                let truth_duty = on / span;
                let duty = t.track.timing.duty_cycle.unwrap();
                assert!(
                    (duty - truth_duty).abs() <= 0.05,
                    "{AWARE_042:?} {name} ch{c}: duty {duty:.3} vs {truth_duty:.3}"
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no active channels in the rendered windows");
}

// ---- hopper ----

/// Ten channels on a 200 kHz raster around 915 MHz, 20 ms dwells in random order for 3 s:
/// `(tracker, output, channels, frame period s)`.
fn run_hopper(cfg: TrackerConfig) -> (Tracker, Out, Vec<f64>, f64) {
    let fc = 915e6;
    let mut s = Scene::new(
        DetectorConfig::new(SurveyId::new()),
        GammaFrames::new(BINS, N_AVG, provenance(fc, FS, 24.0), 7),
    );
    let mut tr = Tracker::new(cfg);
    let mut out = Out::default();
    let raster = 200e3;
    let chans: Vec<f64> = (0..10).map(|k| fc - 1.0e6 + k as f64 * raster).collect();
    let profiles: Vec<Vec<f32>> = chans
        .iter()
        .map(|&f| {
            let mut p = flat(BINS);
            add_line(&mut p, s.bin_of(f).round() as usize, 10, 15.0);
            p
        })
        .collect();
    let frame_s = s.src.frame_period_s();
    let dwell = 0.02;
    let mut rng = Rng(11);
    let mut seq = vec![0usize];
    let frames = (3.0 / frame_s) as usize;
    for i in 0..frames {
        let hop = ((i as f64 + 0.5) * frame_s / dwell) as usize;
        while seq.len() <= hop {
            let prev = *seq.last().unwrap();
            seq.push((prev + 1 + (rng.next_u64() % 9) as usize) % 10);
        }
        step(
            &mut s,
            &mut tr,
            &mut out,
            &profiles[seq[hop]],
            Discontinuity::NONE,
        );
    }
    finish(&mut s, &mut tr, &mut out);
    (tr, out, chans, frame_s)
}

#[test]
fn hopper_ten_channels_fifty_hops_per_second_forms_one_hop_set() {
    let (tr, out, chans, frame_s) = run_hopper(TrackerConfig::default());
    let (raster, dwell) = (200e3, 0.02);
    let sets = out.hop_sets_closed();
    eprintln!("hopper: stats {:?}", tr.stats());
    for h in &sets {
        eprintln!("hopper: {h:?}");
    }
    assert_eq!(sets.len(), 1, "one hop set");
    let h = sets[0];
    assert_eq!(h.channels_hz.len(), 10, "channels {:?}", h.channels_hz);
    let bin = FS / BINS as f64;
    for (got, want) in h.channels_hz.iter().zip(&chans) {
        assert!((got - want).abs() <= 1.5 * bin, "{got} vs {want}");
    }
    let rate = h.hop_rate_hz.unwrap();
    assert!((rate - 50.0).abs() / 50.0 <= 0.05, "hop rate {rate}");
    let r = h.raster_hz.unwrap();
    assert!((r - raster).abs() / raster <= 0.01, "raster {r}");
    let dw = h.dwell_s.unwrap();
    assert!((dw - dwell).abs() <= 2.0 * frame_s, "dwell {dw}");
    let closed = out.closed();
    assert_eq!(closed.len(), 10, "one track per channel");
    assert!(
        closed
            .iter()
            .all(|t| t.hop_set == Some(h.id) && t.track.timing.co_occurring == vec![h.id])
    );
    let mut members = h.members.clone();
    members.sort();
    let mut ids: Vec<_> = closed.iter().map(|t| t.track.id).collect();
    ids.sort();
    assert_eq!(members, ids);
    let linked: u64 = closed.iter().map(|t| t.track.detection_count).sum();
    assert_eq!(
        linked as usize,
        out.records.len(),
        "every dwell linked once"
    );
}

// ---- bursty hopper vs independent periodic emitters (T-031) ----

/// Packets 10 bins (≈ 49 kHz) wide at `(start s, length s, channel)` over `duration_s`.
fn run_packets(
    fc: f64,
    chans: &[f64],
    packets: &[(f64, f64, usize)],
    duration_s: f64,
    seed: u64,
) -> (Tracker, Out) {
    run_packets_with(
        TrackerConfig::default(),
        fc,
        chans,
        packets,
        duration_s,
        seed,
    )
}

fn run_packets_with(
    cfg: TrackerConfig,
    fc: f64,
    chans: &[f64],
    packets: &[(f64, f64, usize)],
    duration_s: f64,
    seed: u64,
) -> (Tracker, Out) {
    let mut s = Scene::new(
        DetectorConfig::new(SurveyId::new()),
        GammaFrames::new(BINS, N_AVG, provenance(fc, FS, 24.0), seed),
    );
    let mut tr = Tracker::new(cfg);
    let mut out = Out::default();
    let bins: Vec<usize> = chans
        .iter()
        .map(|&f| s.bin_of(f).round() as usize)
        .collect();
    let frame_s = s.src.frame_period_s();
    for i in 0..(duration_s / frame_s) as usize {
        let t = (i as f64 + 0.5) * frame_s;
        let mut p = flat(BINS);
        for &(t0, len, c) in packets {
            if t >= t0 && t < t0 + len {
                add_line(&mut p, bins[c], 10, 15.0);
            }
        }
        step(&mut s, &mut tr, &mut out, &p, Discontinuity::NONE);
    }
    finish(&mut s, &mut tr, &mut out);
    (tr, out)
}

/// Eight channels on a 200 kHz raster around 915 MHz, 10 ms packets separated by 100–300 ms of
/// silence for 7.5 s: `(channels, packets)`.
fn bursty_packets() -> (Vec<f64>, Vec<(f64, f64, usize)>) {
    let (fc, raster) = (915e6, 200e3);
    let chans: Vec<f64> = (0..8).map(|k| fc - 0.8e6 + k as f64 * raster).collect();
    let mut rng = Rng(3);
    let mut packets = Vec::new();
    let (mut t, mut ch) = (0.05, 0usize);
    while t < 7.5 {
        packets.push((t, 0.01, ch));
        t += 0.01 + 0.1 + (rng.next_u64() % 200) as f64 * 1e-3;
        ch = (ch + 1 + (rng.next_u64() % 7) as usize) % 8;
    }
    (chans, packets)
}

#[test]
fn bursty_hopper_packets_separated_by_silence_form_one_hop_set() {
    let (fc, raster) = (915e6, 200e3);
    let (chans, packets) = bursty_packets();
    let (tr, out) = run_packets(fc, &chans, &packets, 8.0, 13);
    let sets = out.hop_sets_closed();
    eprintln!(
        "bursty hopper: {} packets, {} tracks, stats {:?}",
        packets.len(),
        out.closed().len(),
        tr.stats()
    );
    for h in &sets {
        eprintln!("bursty hopper: {h:?}");
    }
    assert_eq!(sets.len(), 1, "one hop set");
    let h = sets[0];
    assert!(h.channels_hz.len() >= 6, "channels {:?}", h.channels_hz);
    let bin = FS / BINS as f64;
    for got in &h.channels_hz {
        assert!(
            chans.iter().any(|c| (got - c).abs() <= 1.5 * bin),
            "{got} off the raster"
        );
    }
    let r = h.raster_hz.unwrap();
    assert!((r - raster).abs() / raster <= 0.01, "raster {r}");
    let span = packets.last().unwrap().0 - packets[0].0;
    let mean_gap = span / (packets.len() - 1) as f64;
    let rate = h.hop_rate_hz.unwrap();
    assert!((rate * mean_gap - 1.0).abs() <= 0.2, "hop rate {rate}");
    assert!(tr.stats().bursty_hop_links >= 10);
}

// ---- T-064: hop-set scaling (bounded membership, raster refit on channel-set change) ----

/// A tracker config with the T-064 bounds off (the pre-T-064 behaviour).
fn unbounded() -> TrackerConfig {
    let mut cfg = TrackerConfig::default();
    cfg.hop = cfg.hop.without_scaling_bounds();
    cfg
}

/// The documented T-064 tolerance: `HopSetFormed`/`HopSetClosed` carry the same raster (within
/// 0.1 Hz), the same channels, hop rate, hop count and member count as the unbounded tracker.
fn assert_hop_parity(tag: &str, got: &Out, want: &Out) {
    for (kind, a, b) in [
        ("formed", got.hop_sets_formed(), want.hop_sets_formed()),
        ("closed", got.hop_sets_closed(), want.hop_sets_closed()),
    ] {
        assert_eq!(a.len(), b.len(), "{tag}: {kind} hop sets");
        for (x, y) in a.iter().zip(&b) {
            eprintln!(
                "{tag} {kind}: raster {:?} vs {:?}, {} channels, rate {:?}",
                x.raster_hz,
                y.raster_hz,
                x.channels_hz.len(),
                x.hop_rate_hz
            );
            match (x.raster_hz, y.raster_hz) {
                (Some(p), Some(q)) => {
                    assert!((p - q).abs() <= 0.1, "{tag} {kind}: raster {p} vs {q}")
                }
                (p, q) => assert_eq!(p, q, "{tag} {kind}: raster"),
            }
            assert_eq!(x.channels_hz, y.channels_hz, "{tag} {kind}: channels");
            assert_eq!(x.hop_rate_hz, y.hop_rate_hz, "{tag} {kind}: hop rate");
            assert_eq!(x.hops, y.hops, "{tag} {kind}: hops");
            assert_eq!(x.members.len(), y.members.len(), "{tag} {kind}: members");
            assert_eq!(x.dwell_s, y.dwell_s, "{tag} {kind}: dwell");
        }
    }
}

#[test]
fn hop_set_outputs_match_the_unbounded_tracker_on_synthetic_hoppers() {
    let (tr, got, ..) = run_hopper(TrackerConfig::default());
    let (tr_old, want, ..) = run_hopper(unbounded());
    assert!(!got.hop_sets_closed().is_empty());
    assert_hop_parity("hopper", &got, &want);
    eprintln!(
        "hopper: raster fits {} (unbounded {}), pruned {}",
        tr.stats().hop_raster_fits,
        tr_old.stats().hop_raster_fits,
        tr.stats().hop_members_pruned
    );
    assert!(tr.stats().hop_raster_fits <= tr_old.stats().hop_raster_fits);

    let (chans, packets) = bursty_packets();
    let (tr, got) = run_packets(915e6, &chans, &packets, 8.0, 13);
    let (tr_old, want) = run_packets_with(unbounded(), 915e6, &chans, &packets, 8.0, 13);
    assert!(!got.hop_sets_closed().is_empty());
    assert_hop_parity("bursty hopper", &got, &want);
    eprintln!(
        "bursty hopper: raster fits {} (unbounded {}), pruned {}",
        tr.stats().hop_raster_fits,
        tr_old.stats().hop_raster_fits,
        tr.stats().hop_members_pruned
    );
}

/// A long hopper whose channel tracks keep closing and reopening: 12 channels on a 200 kHz
/// raster, 20 ms dwells, alternating 4 s epochs on channels 0–5 and 6–11 for 300 s with a 1 s
/// idle timeout, so each channel's track closes and a new one joins the set every 8 s (about 450
/// tracks). Before T-064 every one stayed a member; now membership stays within a few per
/// channel, raster fits stay near one per second, and the set keeps its raster and hop rate.
#[test]
fn long_hopper_hop_set_membership_and_raster_fits_stay_bounded() {
    let prov = provenance(915e6, FS, 24.0);
    let mut tr = Tracker::new(TrackerConfig {
        idle_timeout_s: 1.0,
        max_idle_timeout_s: 1.0,
        ..TrackerConfig::default()
    });
    let raster = 200e3;
    let chans: Vec<f64> = (0..12).map(|k| 914e6 + k as f64 * raster).collect();
    let (dwell_frames, epoch_dwells, dwells) = (2u64, 200u64, 15_000u64);
    let mut rng = Rng(64);
    let mut events = Vec::new();
    let (mut prev, mut max_members, mut samples) = (usize::MAX, 0usize, 0u64);
    for k in 0..dwells {
        let half = ((k / epoch_dwells) % 2) as usize;
        let c = loop {
            let c = 6 * half + (rng.next_u64() % 6) as usize;
            if c != prev {
                break c;
            }
        };
        prev = c;
        let r = rec(
            &prov,
            k * dwell_frames,
            dwell_frames,
            chans[c],
            20e3,
            CloseReason::Ended,
            false,
        );
        tr.push_detection(&r, &mut |e| events.push(e));
        let mut batch = TrackBatch::new();
        tr.drain_into(&mut batch);
        if k % 50 == 49 {
            let now = tr
                .hop_sets()
                .iter()
                .map(|h| h.members.len())
                .max()
                .unwrap_or(0);
            max_members = max_members.max(now);
            samples += 1;
        }
    }
    let st = tr.stats();
    tr.finish(&mut |e| events.push(e));
    let out = Out {
        records: Vec::new(),
        events,
    };
    let closed = out.hop_sets_closed();
    eprintln!(
        "long hopper: {} tracks opened, max members {max_members} over {samples} samples, stats {st:?}",
        st.tracks_opened
    );
    for h in &closed {
        eprintln!(
            "long hopper: closed set {} channels raster {:?} rate {:?} hops {}",
            h.channels_hz.len(),
            h.raster_hz,
            h.hop_rate_hz,
            h.hops
        );
    }
    assert!(
        st.tracks_opened >= 300,
        "channel tracks reopen ({})",
        st.tracks_opened
    );
    assert!(
        max_members <= 4 * chans.len(),
        "hop-set members bounded: {max_members}"
    );
    assert!(st.hop_members_pruned as usize + 4 * chans.len() >= st.tracks_opened as usize);
    // About one fit per second of stream plus formations (300 s).
    assert!(
        st.hop_raster_fits <= 400,
        "raster fits {}",
        st.hop_raster_fits
    );
    let h = closed
        .iter()
        .max_by_key(|h| h.hops)
        .expect("a closed hop set");
    let r = h.raster_hz.expect("raster");
    assert!((r - raster).abs() / raster <= 0.01, "raster {r}");
    let rate = h.hop_rate_hz.expect("hop rate");
    assert!((rate - 50.0).abs() / 50.0 <= 0.05, "hop rate {rate}");
    let mut distinct: Vec<f64> = Vec::new();
    for &f in &h.channels_hz {
        if distinct.last().is_none_or(|&d| f - d > 50e3) {
            distinct.push(f);
        }
    }
    assert_eq!(distinct.len(), chans.len(), "channels {:?}", h.channels_hz);
    assert!(h.channels_hz.len() <= 4 * chans.len());
}

#[test]
fn independent_periodic_emitters_on_one_raster_do_not_form_a_hop_set() {
    let fc = 915e6;
    let chans: Vec<f64> = (0..3).map(|k| fc + k as f64 * 200e3).collect();
    let periods = [0.1, 0.13, 0.17];
    for n in [2usize, 3] {
        let mut packets = Vec::new();
        for (c, &period) in periods.iter().enumerate().take(n) {
            let mut t = 0.03 + 0.037 * c as f64;
            while t < 5.9 {
                packets.push((t, 0.02, c));
                t += period;
            }
        }
        let (tr, out) = run_packets(fc, &chans, &packets, 6.0, 17 + n as u64);
        let formed = out
            .events
            .iter()
            .filter(|e| matches!(e, TrackEvent::HopSetFormed(_)))
            .count();
        let closed = out.closed();
        for t in &closed {
            eprintln!("independent x{n}: {}", out.describe(t));
        }
        eprintln!("independent x{n}: stats {:?}", tr.stats());
        assert_eq!(formed, 0, "{n} independent emitters: no hop set");
        assert_eq!(closed.len(), n, "one track per emitter");
        assert!(
            closed
                .iter()
                .all(|t| t.hop_set.is_none() && t.period.is_some())
        );
        if n == 2 {
            // (The contiguous rule may link a dwell that happens to start as the other ends.)
            assert_eq!(
                tr.stats().bursty_hop_links,
                0,
                "two channels never link as a bursty hopper"
            );
        }
    }
}

// ---- two close emitters ----

fn test_repo() -> (Repository, SurveyId) {
    seed_repo(Repository::open_in_memory().unwrap())
}

fn seed_repo(mut repo: Repository) -> (Repository, SurveyId) {
    let plan = ScanPlan {
        id: ScanPlanId::new(),
        version: 1,
        name: "t-007".into(),
        created_at: Timestamp::UNIX_EPOCH,
        regions: vec![PlanRegion {
            freq: FreqRange::new(900e6, 930e6),
            priority: 1.0,
            revisit_ns: None,
        }],
        policy: ScanPolicy::SweepThenDwell,
        gain_table: vec![GainTableEntry {
            freq: FreqRange::new(1e6, 6e9),
            lna_db: 24.0,
            vga_db: 20.0,
            amp_on: false,
            antenna_port: None,
        }],
        schedule: Schedule::Cron {
            expr: "0 * * * *".into(),
        },
        extra: serde_json::json!({}),
    };
    let survey = Survey {
        id: SurveyId::new(),
        plan_id: plan.id,
        plan_version: 1,
        device_id: "synthetic:hk-detect-test".into(),
        state: SurveyState::Open,
        t_start: Timestamp::UNIX_EPOCH,
        t_end: None,
        summary: None,
    };
    repo.insert_scan_plan(&plan).unwrap();
    repo.insert_survey(&survey).unwrap();
    (repo, survey.id)
}

#[test]
fn two_close_interleaved_emitters_stay_separate_and_persist() {
    let (mut repo, survey) = test_repo();
    let fc = 915e6;
    let mut s = Scene::new(
        DetectorConfig::new(survey),
        GammaFrames::new(BINS, N_AVG, provenance(fc, FS, 24.0), 21),
    );
    let mut tr = Tracker::new(TrackerConfig::default());
    let mut out = Out::default();
    let (f1, f2) = (fc + 1.0e6, fc + 1.025e6);
    let mut p1 = flat(BINS);
    add_line(&mut p1, s.bin_of(f1).round() as usize, 3, 15.0);
    let mut p2 = flat(BINS);
    add_line(&mut p2, s.bin_of(f2).round() as usize, 3, 15.0);
    let quiet = flat(BINS);
    let frame_s = s.src.frame_period_s();
    let mut writer = DetectionWriter::new(64);
    let mut batch = TrackBatch::new();
    for i in 0..(3.0 / frame_s) as usize {
        let t = (i as f64 + 0.5) * frame_s;
        let ph = t % 0.1;
        let prof = if ph < 0.02 {
            &p1
        } else if (0.05..0.07).contains(&ph) {
            &p2
        } else {
            &quiet
        };
        let before = out.records.len();
        step(&mut s, &mut tr, &mut out, prof, Discontinuity::NONE);
        for r in &out.records[before..] {
            writer.push(&mut repo, r).unwrap();
        }
        if i % 200 == 0 {
            writer.flush(&mut repo).unwrap();
            tr.drain_into(&mut batch);
            batch.write(&mut repo).unwrap();
        }
    }
    let before = out.records.len();
    finish(&mut s, &mut tr, &mut out);
    for r in &out.records[before..] {
        writer.push(&mut repo, r).unwrap();
    }
    writer.flush(&mut repo).unwrap();
    tr.drain_into(&mut batch);
    batch.write(&mut repo).unwrap();

    let closed = out.closed();
    for t in &closed {
        eprintln!("two emitters: {}", out.describe(t));
    }
    assert_eq!(closed.len(), 2, "two tracks");
    for (f, t) in [f1, f2].iter().zip({
        let mut c = closed.clone();
        c.sort_by(|a, b| a.track.f_center_hz.total_cmp(&b.track.f_center_hz));
        c
    }) {
        assert!((t.track.f_center_hz - f).abs() <= FS / BINS as f64);
        assert!((28..=31).contains(&t.burst_count), "{}", t.burst_count);
        let p = t.period.unwrap();
        assert!((p.period_s - 0.1).abs() / 0.1 <= 0.02, "{p:?}");
        assert!(t.hop_set.is_none());
        // Repository: the aggregate and its ordered member links.
        let stored = repo.track(t.track.id).unwrap();
        assert_eq!(
            (stored.id, stored.state, stored.time, stored.detection_count),
            (
                t.track.id,
                TrackState::Closed,
                t.track.time,
                t.track.detection_count
            )
        );
        assert!((stored.f_center_hz - t.track.f_center_hz).abs() < 1e-3);
        let (ps, pt) = (
            stored.timing.period_s.unwrap(),
            t.track.timing.period_s.unwrap(),
        );
        assert!((ps - pt).abs() < 1e-12);
        assert!(
            (stored.timing.duty_cycle.unwrap() - t.track.timing.duty_cycle.unwrap()).abs() < 1e-12
        );
        let members = repo.track_detections(t.track.id).unwrap();
        assert_eq!(members.len() as u64, t.track.detection_count);
        let starts: Vec<Timestamp> = members
            .iter()
            .map(|d| repo.detection(*d).unwrap().time.start)
            .collect();
        assert!(starts.windows(2).all(|w| w[0] <= w[1]));
        for d in &members {
            let r = out.records.iter().find(|r| r.detection.id == *d).unwrap();
            assert!((r.detection.f_center_hz - f).abs() <= 2.0 * FS / BINS as f64);
        }
    }
    assert_eq!(tr.stats().hop_links, 0);
}

// ---- gain change ----

#[test]
fn gain_change_mid_track_keeps_the_track_and_records_a_segment() {
    let fc = 433.92e6;
    let mut cfg = DetectorConfig::new(SurveyId::new());
    cfg.max_duration_s = 0.5;
    let mut s = Scene::new(
        cfg,
        GammaFrames::new(BINS, N_AVG, provenance(fc, FS, 16.0), 5),
    );
    let mut tr = Tracker::new(TrackerConfig::default());
    let mut out = Out::default();
    let (fb, fcar) = (fc + 2.0e6, fc - 1.0e6);
    let mut carrier = flat(BINS);
    add_line(&mut carrier, s.bin_of(fcar).round() as usize, 3, 20.0);
    let mut burst = carrier.clone();
    add_line(&mut burst, s.bin_of(fb).round() as usize, 6, 15.0);
    let frame_s = s.src.frame_period_s();
    let n = (4.0 / frame_s) as usize;
    for i in 0..n {
        let flags = if i == n / 2 {
            s.switch(provenance(fc, FS, 32.0));
            Discontinuity::GAIN_CHANGE
        } else {
            Discontinuity::NONE
        };
        let t = (i as f64 + 0.5) * frame_s;
        // The switch lands mid-burst (t ≈ 2.0 s is inside [2.0, 2.02)).
        let prof = if (t + 0.09) % 0.1 < 0.02 {
            &burst
        } else {
            &carrier
        };
        step(&mut s, &mut tr, &mut out, prof, flags);
    }
    finish(&mut s, &mut tr, &mut out);
    let closed = out.closed();
    for t in &closed {
        eprintln!("gain change: {}", out.describe(t));
    }
    eprintln!("gain change: stats {:?}", tr.stats());
    assert_eq!(closed.len(), 2, "same two tracks across the gain change");
    let car = closed
        .iter()
        .find(|t| (t.track.f_center_hz - fcar).abs() < 20e3)
        .unwrap();
    let bur = closed
        .iter()
        .find(|t| (t.track.f_center_hz - fb).abs() < 20e3)
        .unwrap();
    assert_eq!(
        car.burst_count, 1,
        "carrier: one burst across splits and the transition"
    );
    assert!(car.on_time_s >= 4.0 - 6.0 * frame_s, "{}", car.on_time_s);
    assert!(car.segments >= 1 && bur.segments >= 1);
    assert!((39..=41).contains(&bur.burst_count), "{}", bur.burst_count);
    let p = bur.period.unwrap();
    assert!((p.period_s - 0.1).abs() / 0.1 <= 0.02, "{p:?}");
    let seg_events: Vec<_> = out
        .events
        .iter()
        .filter_map(|e| match e {
            TrackEvent::Segment(b) => Some(b),
            _ => None,
        })
        .collect();
    assert!(
        seg_events
            .iter()
            .any(|b| b.track == car.track.id && b.kind == BoundaryKind::GainChange)
    );
    assert!(
        seg_events
            .iter()
            .any(|b| b.track == bur.track.id && b.kind == BoundaryKind::GainChange)
    );
    assert!(tr.stats().split_continuations >= 6 && tr.stats().transition_continuations >= 1);
    assert!(
        out.records
            .iter()
            .any(|r| r.close == CloseReason::Transition),
        "the gain change closed boxes"
    );
}

// ---- record-level: merge and fused split ----

fn rec(
    prov: &ProvenanceHandle,
    frame0: u64,
    frames: u64,
    fc: f64,
    obw: f64,
    close: CloseReason,
    continues: bool,
) -> DetectionRecord {
    const FRAME_NS: i64 = 10_000_000;
    const SPF: u64 = 1000;
    let bin = 5e3;
    let nb = (obw / bin).ceil().max(1.0) as usize;
    let lo = fc - nb as f64 * bin / 2.0;
    let t0 = frame0 as i64 * FRAME_NS;
    let t1 = (frame0 + frames) as i64 * FRAME_NS;
    DetectionRecord {
        detection: Detection {
            id: DetectionId::new(),
            survey_id: SurveyId::new(),
            time: TimeRange::new(
                Timestamp::from_unix_nanos(t0),
                Timestamp::from_unix_nanos(t1),
            ),
            f_center_hz: fc,
            obw_hz: obw,
            xdb_bandwidth_hz: None,
            xdb_level_db: None,
            snr_peak_db: 20.0,
            snr_mean_db: 15.0,
            peak_level_dbfs: -40.0,
            peak_level_dbm: None,
            sk: None,
            clip_count: 0,
            detector_version: "test".into(),
            provenance_ref: prov.id(),
            flags: DetectionFlags::default(),
        },
        provenance: prov.clone(),
        segment: 1,
        bins: 1000..1000 + nb,
        f_lo_hz: lo,
        f_hi_hz: lo + nb as f64 * bin,
        frames: frame0..frame0 + frames,
        samples: frame0 * SPF..(frame0 + frames) * SPF,
        pixels: nb as u64 * frames,
        close,
        continues,
        candidate: Candidate::Unconfirmed,
        image: None,
        spur_harmonic_hz: None,
        merged_boxes: 1,
        inconclusive: false,
    }
}

#[test]
fn converging_fragment_merges_into_the_older_track() {
    let prov = provenance(915e6, FS, 24.0);
    let (mut repo, survey) = test_repo();
    let mut tr = Tracker::new(TrackerConfig::default());
    let mut ev = Vec::new();
    let mut recs = Vec::new();
    let f0 = 915.5e6;
    for k in 0..80u64 {
        let fb = f0 + (12e3 - 0.25e3 * k as f64).max(0.0);
        // A: steady at f0, bursts at 0, 100, 200 ms …
        // B: opens 12 kHz away (outside ε = 10 kHz) and drifts slowly onto f0 (0.25 kHz per
        // burst, slow enough for its centre estimate to follow), interleaved at +50 ms.
        for (frame0, f) in [(10 * k, f0), (10 * k + 5, fb)] {
            let mut r = rec(&prov, frame0, 2, f, 20e3, CloseReason::Ended, false);
            r.detection.survey_id = survey;
            tr.push_detection(&r, &mut |e| ev.push(e));
            recs.push(r);
        }
    }
    let mut batch = TrackBatch::new();
    tr.drain_into(&mut batch);
    tr.finish(&mut |e| ev.push(e));
    let merged: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            TrackEvent::Merged { from, into, .. } => Some((*from, *into)),
            _ => None,
        })
        .collect();
    let opened: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            TrackEvent::Opened { track, .. } => Some(*track),
            _ => None,
        })
        .collect();
    eprintln!(
        "merge: opened {} merged {merged:?} stats {:?}",
        opened.len(),
        tr.stats()
    );
    assert_eq!(opened.len(), 2);
    assert_eq!(
        merged,
        vec![(opened[1], opened[0])],
        "younger merges into older"
    );
    assert!(
        batch
            .upserts
            .iter()
            .any(|t| t.id == opened[1] && t.state == TrackState::MergedInto(opened[0])),
        "the merge is recorded on the absorbed track"
    );
    let closed: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            TrackEvent::Closed(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].track.id, opened[0]);
    assert_eq!(closed[0].burst_count, 160);

    // Persisted: the survivor's links include the absorbed track's (re-pointed); the absorbed
    // track keeps its own rows (links are append-only).
    assert_eq!(batch.repoints, vec![(opened[1], opened[0])]);
    let mut writer = DetectionWriter::new(64);
    for r in &recs {
        writer.push(&mut repo, r).unwrap();
    }
    writer.flush(&mut repo).unwrap();
    batch.write(&mut repo).unwrap();
    tr.drain_into(&mut batch);
    batch.write(&mut repo).unwrap();
    let survivor = repo.track_detections(opened[0]).unwrap();
    assert_eq!(survivor.len(), 160);
    assert_eq!(survivor.len() as u64, closed[0].track.detection_count);
    let absorbed = repo.track_detections(opened[1]).unwrap();
    assert!(!absorbed.is_empty() && absorbed.iter().all(|d| survivor.contains(d)));
    assert_eq!(
        repo.track(opened[1]).unwrap().state,
        TrackState::MergedInto(opened[0])
    );
}

/// T-035: TrackSummary timing features, segment boundaries and the region query survive
/// tracker → TrackBatch (one transaction) → repository → read (AWARE-036, AWARE-042).
#[test]
fn timing_features_segments_and_region_query_round_trip_through_the_repository() {
    let (mut repo, survey) = test_repo();
    let low = provenance(915e6, FS, 24.0);
    let high = provenance(915e6, FS, 32.0);
    let mut tr = Tracker::new(TrackerConfig::default());
    let mut ev = Vec::new();
    let mut writer = DetectionWriter::new(64);
    let mut batch = TrackBatch::new();
    let f0 = 915.5e6;
    for k in 0..60u64 {
        let prov = if k < 30 { &low } else { &high };
        let mut r = rec(prov, 10 * k, 2, f0, 20e3, CloseReason::Ended, false);
        r.detection.survey_id = survey;
        tr.push_detection(&r, &mut |e| ev.push(e));
        writer.push(&mut repo, &r).unwrap();
        if k % 16 == 15 {
            writer.flush(&mut repo).unwrap();
            tr.drain_into(&mut batch);
            batch.write(&mut repo).unwrap();
        }
    }
    tr.finish(&mut |e| ev.push(e));
    writer.flush(&mut repo).unwrap();
    tr.drain_into(&mut batch);
    batch.write(&mut repo).unwrap();
    assert!(batch.is_empty());

    let closed: Vec<&TrackSummary> = ev
        .iter()
        .filter_map(|e| match e {
            TrackEvent::Closed(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(closed.len(), 1);
    let s = closed[0];
    let (p, bl) = (s.period.unwrap(), s.burst_length.unwrap());
    let stored = repo.track(s.track.id).unwrap();
    let tm = &stored.timing;
    let near = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() <= 1e-9 * b.abs().max(1.0);
    assert!(near(tm.period_s, p.period_s), "{tm:?}");
    assert!(near(tm.period_confidence, p.confidence) && near(tm.period_jitter_s, p.jitter_s));
    let l = tm.burst_length.unwrap();
    assert_eq!(l.count, bl.count);
    assert!(near(Some(l.p50_s), bl.p50_s) && near(Some(l.p90_s), bl.p90_s));
    assert!(near(Some(l.min_s), bl.min_s) && near(Some(l.max_s), bl.max_s));
    assert!(s.segments >= 1);
    assert_eq!(tm.segment_count, s.segments);
    assert_eq!((tm.hop_set, tm.hop_raster_hz), (None, None));
    assert_eq!(stored.detection_count, s.track.detection_count);

    // Boundaries: the Segment events, as rows.
    let events: Vec<(Timestamp, SegmentKind)> = ev
        .iter()
        .filter_map(|e| match e {
            TrackEvent::Segment(b) if b.track == s.track.id => Some((b.at, b.kind.into())),
            _ => None,
        })
        .collect();
    let rows: Vec<(Timestamp, SegmentKind)> = repo
        .track_segments(s.track.id)
        .unwrap()
        .into_iter()
        .map(|x| (x.at, x.kind))
        .collect();
    assert_eq!(rows, events);
    assert!(rows.iter().any(|&(_, k)| k == SegmentKind::GainChange));

    // Region query (AWARE-042).
    let any = TrackFilter::default();
    let region = Region::new(FreqRange::new(f0 - 1e3, f0 + 1e3), s.track.time);
    let page = repo
        .tracks_in_region(&region, &any, PageRequest::default())
        .unwrap();
    assert_eq!(
        page.tracks.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![s.track.id]
    );
    let elsewhere = Region::new(FreqRange::new(f0 + 1e6, f0 + 2e6), s.track.time);
    assert!(
        repo.tracks_in_region(&elsewhere, &any, PageRequest::default())
            .unwrap()
            .tracks
            .is_empty()
    );
}

/// T-035: `TrackBatch::write` is one transaction: a failed link rolls back the upserts written
/// before it, and the batch is kept for a retry.
#[test]
fn track_batch_write_rolls_back_on_failure_and_keeps_the_batch() {
    let (mut repo, survey) = test_repo();
    let prov = provenance(915e6, FS, 24.0);
    let mut tr = Tracker::new(TrackerConfig::default());
    let mut recs = Vec::new();
    for k in 0..20u64 {
        let mut r = rec(&prov, 10 * k, 2, 915.5e6, 20e3, CloseReason::Ended, false);
        r.detection.survey_id = survey;
        tr.push_detection(&r, &mut |_| {});
        recs.push(r);
    }
    tr.finish(&mut |_| {});
    let mut batch = TrackBatch::new();
    tr.drain_into(&mut batch);
    let id = batch.upserts[0].id;
    let (tracks, links) = (batch.upserts.len(), batch.links.len());
    let members = batch.links.iter().filter(|l| l.0 == id).count();
    assert!(members > 0);
    // The member detections were never written: the first link fails after the upserts ran.
    assert!(batch.write(&mut repo).is_err());
    assert!(repo.track(id).is_err(), "upserts rolled back");
    assert_eq!((batch.upserts.len(), batch.links.len()), (tracks, links));
    let mut writer = DetectionWriter::new(64);
    for r in &recs {
        writer.push(&mut repo, r).unwrap();
    }
    writer.flush(&mut repo).unwrap();
    assert_eq!(batch.write(&mut repo).unwrap(), (tracks, links));
    assert!(batch.is_empty());
    assert_eq!(repo.track_detections(id).unwrap().len(), members);
}

/// T-035 benchmark: rows/s of the per-call write path (one transaction per upsert and per
/// track's links, as `TrackBatch::write` did before T-035) against one transaction per batch, on
/// a WAL file database.
#[test]
#[ignore = "benchmark: cargo test -p hk-detect --release --test track bench_ -- --ignored --nocapture"]
fn bench_track_batch_write_rows_per_second() {
    const DRAINS: usize = 200;
    const TRACKS: usize = 16;
    const LINKS: usize = 8;
    let dir = std::env::temp_dir().join(format!("hk-detect-bench-{}", TrackId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let (mut repo, survey) = seed_repo(Repository::open(dir.join("bench.db")).unwrap());
    let prov = provenance(915e6, FS, 24.0);
    let mut writer = DetectionWriter::new(1024);
    let mut ids = Vec::new();
    for k in 0..(TRACKS * LINKS) as u64 {
        let f = 915e6 + (k % TRACKS as u64) as f64 * 100e3;
        let mut r = rec(&prov, 3 * k, 2, f, 20e3, CloseReason::Ended, false);
        r.detection.survey_id = survey;
        ids.push(r.detection.id);
        writer.push(&mut repo, &r).unwrap();
    }
    writer.flush(&mut repo).unwrap();
    let make = |drain: usize| {
        let mut b = TrackBatch::new();
        let at = Timestamp::from_unix_nanos(drain as i64 * 1_000_000_000);
        for t in 0..TRACKS {
            let track = Track {
                id: TrackId::new(),
                state: TrackState::Open,
                split_from: None,
                time: TimeRange::new(at, at.saturating_add_nanos(500_000_000)),
                f_center_hz: 915e6 + t as f64 * 100e3,
                bandwidth_hz: 20e3,
                detection_count: LINKS as u64,
                timing: TimingFeatures {
                    period_s: Some(0.1),
                    duty_cycle: Some(0.2),
                    ..TimingFeatures::default()
                },
                updated_at: at,
            };
            for l in 0..LINKS {
                b.links.push((track.id, ids[t * LINKS + l]));
            }
            b.upserts.push(track);
        }
        b.linked_at = at;
        b
    };
    let rows = DRAINS * TRACKS * (1 + LINKS);

    let batches: Vec<TrackBatch> = (0..DRAINS).map(make).collect();
    let start = Instant::now();
    for b in &batches {
        for t in &b.upserts {
            repo.upsert_track(t).unwrap();
        }
        for chunk in b.links.chunks(LINKS) {
            let members: Vec<DetectionId> = chunk.iter().map(|l| l.1).collect();
            repo.link_detections_to_track(chunk[0].0, &members, b.linked_at)
                .unwrap();
        }
    }
    let per_call = rows as f64 / start.elapsed().as_secs_f64();

    let mut batches: Vec<TrackBatch> = (0..DRAINS).map(make).collect();
    let start = Instant::now();
    for b in &mut batches {
        b.write(&mut repo).unwrap();
    }
    let batched = rows as f64 / start.elapsed().as_secs_f64();
    eprintln!(
        "track batch write: {rows} rows in {DRAINS} drains of {TRACKS} tracks + {} links: \
         per-call transactions {per_call:.0} rows/s, one transaction per batch {batched:.0} rows/s \
         ({:.1}x)",
        TRACKS * LINKS,
        batched / per_call
    );
    drop(repo);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn two_emitters_sharing_a_track_split_into_two() {
    let prov = provenance(915e6, FS, 24.0);
    let mut tr = Tracker::new(TrackerConfig::default());
    let mut ev = Vec::new();
    // 8 kHz apart: B's first burst passes A's gate (ε = 10 kHz) and joins A's track.
    let (fa, fb) = (915.5e6, 915.508e6);
    for k in 0..60u64 {
        for (frame0, f) in [(10 * k, fa), (10 * k + 5, fb)] {
            tr.push_detection(
                &rec(&prov, frame0, 2, f, 20e3, CloseReason::Ended, false),
                &mut |e| ev.push(e),
            );
        }
    }
    tr.finish(&mut |e| ev.push(e));
    let mut batch = TrackBatch::new();
    tr.drain_into(&mut batch);
    let opened: Vec<TrackId> = ev
        .iter()
        .filter_map(|e| match e {
            TrackEvent::Opened { track, .. } => Some(*track),
            _ => None,
        })
        .collect();
    let splits: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            TrackEvent::TrackSplit { from, into, .. } => Some((*from, *into)),
            _ => None,
        })
        .collect();
    let closed: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            TrackEvent::Closed(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    eprintln!("split: stats {:?}", tr.stats());
    assert_eq!(opened.len(), 2);
    assert_eq!(splits, vec![(opened[0], opened[1])]);
    assert!(!ev.iter().any(|e| matches!(e, TrackEvent::Merged { .. })));
    assert_eq!(closed.len(), 2);
    let parent = closed.iter().find(|s| s.track.id == opened[0]).unwrap();
    let child = closed.iter().find(|s| s.track.id == opened[1]).unwrap();
    // One track per emitter (the larger cluster in the window stays on the parent).
    let (pf, cf) = (parent.track.f_center_hz, child.track.f_center_hz);
    let near = |x: f64, f: f64| (x - f).abs() < 1e3;
    assert!(
        (near(pf, fa) && near(cf, fb)) || (near(pf, fb) && near(cf, fa)),
        "parent {pf} child {cf}"
    );
    assert_eq!(parent.track.split_from, None);
    assert_eq!(child.track.split_from, Some(opened[0]));
    assert!(child.burst_count >= 45, "{}", child.burst_count);
    // Recorded, not overwritten: the parent's row at the split precedes the child's rows.
    let first = |id| batch.upserts.iter().position(|t| t.id == id).unwrap();
    assert!(first(opened[0]) < first(opened[1]));
}

#[test]
fn low_snr_fragments_stay_tentative_and_are_never_opened() {
    let synth = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(1)
            .param("snr_db", 12.0)
            .param("duration_s", 3.0)
    );
    let fx = synth.fixture(0).unwrap();
    let iq = to_ci8(&fx.samples().unwrap());
    let chain = ChainConfig::new(256, 4);
    let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).unwrap();
    let mut tr = Tracker::new(TrackerConfig::default());
    let out = replay_tracked(
        &iq,
        fx.sample_rate,
        fixture_provenance(&fx),
        &chain,
        &mut det,
        &mut tr,
    );
    let opened = out
        .events
        .iter()
        .filter(|e| matches!(e, TrackEvent::Opened { .. }))
        .count();
    let closed = out.closed();
    for s in &closed {
        eprintln!("12 dB: {}", out.describe(s));
    }
    eprintln!(
        "12 dB: {} records, stats {:?}",
        out.records.len(),
        tr.stats()
    );
    assert!(
        tr.stats().tentative_discarded >= 1,
        "a fragment was held and discarded"
    );
    assert_eq!(opened, 1, "{AWARE_036:?} only the emitter is opened");
    assert_eq!(closed.len(), 1, "{AWARE_036:?} one track");
    assert_eq!(
        closed[0].burst_count as usize,
        fx.of_kind("fsk-burst").len()
    );
}

#[test]
fn fused_split_box_is_spread_back_onto_both_tracks() {
    let prov = provenance(915e6, FS, 24.0);
    let mut tr = Tracker::new(TrackerConfig::default());
    let mut ev = Vec::new();
    let (f1, f2) = (915.2e6, 915.23e6);
    let push = |tr: &mut Tracker, ev: &mut Vec<TrackEvent>, r: DetectionRecord| {
        tr.push_detection(&r, &mut |e| ev.push(e));
    };
    let md = CloseReason::MaxDuration;
    push(&mut tr, &mut ev, rec(&prov, 0, 100, f1, 10e3, md, true));
    // (Staggered key-up: boxes starting in the same frame would be tone lobes of one burst.)
    push(&mut tr, &mut ev, rec(&prov, 10, 90, f2, 10e3, md, true));
    // A bridge on the split frame fuses the two continuations into one wide box.
    push(
        &mut tr,
        &mut ev,
        rec(&prov, 100, 100, 0.5 * (f1 + f2), 45e3, md, true),
    );
    push(
        &mut tr,
        &mut ev,
        rec(&prov, 200, 100, f1, 10e3, CloseReason::Ended, false),
    );
    push(
        &mut tr,
        &mut ev,
        rec(&prov, 200, 100, f2, 10e3, CloseReason::Ended, false),
    );
    // A 1-frame side run at the split skips min duration: dropped.
    push(
        &mut tr,
        &mut ev,
        rec(&prov, 150, 1, 915.6e6, 5e3, CloseReason::Ended, false),
    );
    tr.finish(&mut |e| ev.push(e));
    let closed: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            TrackEvent::Closed(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    eprintln!("fused: stats {:?}", tr.stats());
    assert_eq!(closed.len(), 2);
    assert!(ev.iter().any(|e| matches!(e, TrackEvent::Split { .. })));
    assert_eq!(tr.stats().fused_spread, 1);
    assert_eq!(tr.stats().short_skipped, 1);
    let dets: u64 = closed.iter().map(|s| s.track.detection_count).sum();
    assert_eq!(dets, 5, "each detection links to one track");
    for s in &closed {
        assert_eq!(s.burst_count, 1, "{s:?}");
        let want = if (s.track.f_center_hz - f1).abs() < 5e3 {
            3.0
        } else {
            2.9
        };
        assert!((s.on_time_s - want).abs() < 1e-6, "{}", s.on_time_s);
    }
}

// ---- real 915 MHz fixture (report only) ----

fn load_main_checkout_fixture(name: &str) -> Option<(Fixture, Vec<Complex<i8>>)> {
    let local = hackrf_fixture(name);
    let data = local.with_extension("sigmf-data");
    let is_pointer = std::fs::read(&data)
        .map(|b| b.starts_with(b"version https://git-lfs"))
        .unwrap_or(true);
    let meta = if is_pointer {
        // A worktree without LFS data: read the main checkout's copy.
        let root = hk_e2e::paths::repo_root();
        let git = std::fs::read_to_string(root.join(".git")).ok()?;
        let gitdir = git.strip_prefix("gitdir:")?.trim();
        let main = std::path::Path::new(gitdir)
            .ancestors()
            .nth(3)?
            .to_path_buf();
        main.join(local.strip_prefix(&root).ok()?)
    } else {
        local
    };
    let fx = Fixture::load(&meta).ok()?;
    let bytes = std::fs::read(fx.data_path()).ok()?;
    if bytes.starts_with(b"version https://git-lfs") {
        eprintln!("SKIP {name}: fixture data is not fetched");
        return None;
    }
    let iq = bytes
        .chunks_exact(2)
        .map(|c| Complex::new(c[0] as i8, c[1] as i8))
        .collect();
    Some((fx, iq))
}

#[test]
fn report_915_fhss_bursts_hop_set_on_200_khz_raster() {
    let name = "ism_915M_10M_l24g30a1_t42p3_1p2s";
    let Some((fx, iq)) = load_main_checkout_fixture(name) else {
        eprintln!("SKIP {name}");
        return;
    };
    let mut cfg = DetectorConfig::new(SurveyId::new());
    cfg.band_profiles.push(BandProfile {
        freq: FreqRange::new(902e6, 928e6),
        profile: DetectionProfile::short_burst(),
    });
    let mut det = Detector::new(cfg).unwrap();
    let mut tr = Tracker::new(TrackerConfig::default());
    let chain = ChainConfig::new(1024, 4);
    let out = replay_tracked(
        &iq,
        fx.sample_rate,
        fixture_provenance(&fx),
        &chain,
        &mut det,
        &mut tr,
    );
    let closed = out.closed();
    let hop_sets = out.hop_sets_closed();
    eprintln!(
        "915 report: {} records, {} tracks, {} hop sets, stats {:?}",
        out.records.len(),
        closed.len(),
        hop_sets.len(),
        tr.stats()
    );
    let mut on_raster = 0;
    for s in &closed {
        let off = (s.track.f_center_hz - 915e6).rem_euclid(200e3);
        let resid = off.min(200e3 - off);
        on_raster += usize::from(resid <= 25e3);
        eprintln!(
            "915 report: {} raster residual {:.1} kHz",
            out.describe(s),
            resid / 1e3
        );
    }
    for h in &hop_sets {
        eprintln!("915 report: hop set {h:?}");
        // T-035: the raster estimate is robust to the noisy real centres (T-031 reported 40.3 kHz).
        let r = h.raster_hz.expect("hop-set raster");
        assert!(
            (r - 200e3).abs() / 200e3 <= 0.05,
            "915 hop raster {:.1} kHz, want 200 kHz ± 5 %",
            r / 1e3
        );
    }
    eprintln!(
        "915 report: {on_raster}/{} tracks within 25 kHz of the 200 kHz raster; hop links {}; hop set formed: {}",
        closed.len(),
        tr.stats().hop_links,
        !hop_sets.is_empty()
    );
}

/// T-064 parity on the real 915 MHz FHSS fixture: the bounded tracker's hop sets equal the
/// unbounded (pre-T-064) tracker's within the documented tolerance.
#[test]
fn hop_set_outputs_match_the_unbounded_tracker_on_the_915_fixture() {
    let name = "ism_915M_10M_l24g30a1_t42p3_1p2s";
    let Some((fx, iq)) = load_main_checkout_fixture(name) else {
        eprintln!("SKIP {name}");
        return;
    };
    let run = |tcfg: TrackerConfig| {
        let mut cfg = DetectorConfig::new(SurveyId::new());
        cfg.band_profiles.push(BandProfile {
            freq: FreqRange::new(902e6, 928e6),
            profile: DetectionProfile::short_burst(),
        });
        let mut det = Detector::new(cfg).unwrap();
        let mut tr = Tracker::new(tcfg);
        let chain = ChainConfig::new(1024, 4);
        let out = replay_tracked(
            &iq,
            fx.sample_rate,
            fixture_provenance(&fx),
            &chain,
            &mut det,
            &mut tr,
        );
        (tr.stats(), out)
    };
    let (st, got) = run(TrackerConfig::default());
    let (st_old, want) = run(unbounded());
    assert!(!got.hop_sets_closed().is_empty(), "915 hop set formed");
    assert_hop_parity("915", &got, &want);
    eprintln!(
        "915: raster fits {} (unbounded {}), pruned {}",
        st.hop_raster_fits, st_old.hop_raster_fits, st.hop_members_pruned
    );
}
