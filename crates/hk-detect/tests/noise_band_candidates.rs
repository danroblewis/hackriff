//! T-990: **how many candidates a noise-only band makes per minute**, measured through the real
//! chain and bounded by [`NOISE_BAND_MAX_CANDIDATES_PER_MIN`].
//!
//! `false_alarm.rs` bounds the detector's false **boxes** per MHz·hour, which is the S4 quantity
//! and the right one for a CFAR. It is not the quantity the explorer counted. What reaches a
//! person is a *candidate* — a confirmed track, catalogued as an inventory row and handed an
//! explanation — and in an empty airband (118–128.7 MHz, nothing on the air) the app produced 52
//! of them in 8 minutes at 4.5–8 dB, each labelled "Aviation voice (VHF AM)". Nothing in the
//! suite stated a rate at that level, so nothing could go red when one appeared.
//!
//! The two tests here are the two halves of that:
//!
//! 1. **The rate.** Synthetic noise-only band → 8-bit IQ → STFT → the running
//!    [`NoiseFloorTracker`] → [`Detector`] at the default profile → [`Tracker`]; count the tracks
//!    that confirm, over several minutes of capture time, and check the 95 % upper bound on the
//!    rate (`Gamma⁻¹(k+1, 0.95) / exposure`, as the S4 suite reports it) is inside the stated
//!    bound. The floor tracker is the *running estimate*, not the true floor, on purpose: an
//!    estimator that lags a shaped or drifting floor is how a threshold quietly moves.
//! 2. **Self-cleaning.** A stream of blips shorter than [`DetectionProfile::min_frames`] never
//!    becomes a candidate however many of them there are, while the same line held past the
//!    duration test does. The second half is what keeps the first from passing by deafness.
//!
//! Both are deterministic: the noise is a seeded stream and the exposure is simulated capture
//! time, so neither the wall clock nor machine load can change the verdict (docs/10 §3.6).

mod common;

use common::*;
use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_detect::{
    ClipCount, Detector, DetectorConfig, DetectorEvent, NOISE_BAND_MAX_CANDIDATES_PER_MIN,
    TrackEvent, Tracker, TrackerConfig, count_clipped_ci8,
};
use hk_dsp::floor::{FloorConfig, NoiseFloorTracker, gamma};
use hk_dsp::window::WindowKind;
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_model::{SampleTime, SurveyId, Timestamp};
use num_complex::Complex;

/// The explorer's geometry: a HackRF at 2.4 Msps with 4096-bin, 8-average frames (13.65 ms).
const FS: f64 = 2.4e6;
const FFT: usize = 4096;
const AVG: usize = 8;
/// Capture time per worker, s.
const SEGMENT_S: f64 = 60.0;
/// Workers, each an independent noise stream. Four segments certify 0.75/min at 95 % with no
/// observations, which is inside [`NOISE_BAND_MAX_CANDIDATES_PER_MIN`].
const SEGMENTS: usize = 4;

/// 95 % upper bound on a Poisson rate given `k` events, in events per unit exposure.
fn upper95(k: u64, exposure: f64) -> f64 {
    gamma::inverse_lower(k as f64 + 1.0, 0.95) / exposure
}

/// Box–Muller, from the shared SplitMix64.
fn gauss(rng: &mut Rng) -> f64 {
    let u1 = rng.unit();
    let u2 = rng.unit();
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

#[derive(Default, Clone, Copy, Debug)]
struct Counts {
    boxes: u64,
    candidates: u64,
}

/// One noise-only segment: `seconds` of 8-bit complex Gaussian noise at `sigma` LSBs rms per
/// component, through the whole chain. Retunes across the airband every `retune_s` so the floor
/// tracker is re-converging as often as a scanning receiver makes it.
fn noise_segment(seed: u64, sigma: f64, seconds: f64, retune_s: f64) -> Counts {
    let centres: [f64; 5] = [118.5e6, 120.9e6, 123.3e6, 125.7e6, 128.1e6];
    let mut prov: ProvenanceHandle = provenance(centres[0], FS, 24.0);
    let mut cur = 0usize;
    let welch = WelchConfig {
        fft_len: FFT,
        overlap: 0,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: true,
    };
    let mut stft = StftProcessor::new(StftConfig::new(welch, AVG)).expect("stft");
    let mut floor = NoiseFloorTracker::new(FloorConfig::default()).expect("floor");
    let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).expect("detector");
    let mut tr = Tracker::new(TrackerConfig::default());
    let mut rng = Rng(seed);
    let total = (FS * seconds) as u64;
    let mut n = Counts::default();
    let mut buf: Vec<Complex<i8>> = Vec::with_capacity(65_536);
    let mut s = 0u64;
    while s < total {
        let take = 65_536usize.min((total - s) as usize);
        buf.clear();
        for _ in 0..take {
            let re = (gauss(&mut rng) * sigma).round().clamp(-127.0, 127.0) as i8;
            let im = (gauss(&mut rng) * sigma).round().clamp(-127.0, 127.0) as i8;
            buf.push(Complex::new(re, im));
        }
        let want = ((s as f64 / FS) / retune_s) as usize % centres.len();
        let retuned = want != cur;
        if retuned {
            cur = want;
            prov = provenance(centres[cur], FS, 24.0);
        }
        let header = BlockHeader {
            time: SampleTime {
                sample_index: s,
                host_time: Timestamp::from_unix_nanos((s as f64 * 1e9 / FS).round() as i64),
            },
            provenance: prov.clone(),
            discontinuity: if s == 0 || retuned {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
        };
        stft.push(InputInfo::from(&header), &buf, |frame| {
            let f = floor.update(frame, |_| {});
            let clip = ClipCount::new(count_clipped_ci8(&buf), buf.len() as u64);
            det.process(frame, f, clip, &mut |e| {
                if matches!(e, DetectorEvent::Detection(_)) {
                    n.boxes += 1;
                }
                tr.push(&e, &mut |te| {
                    if matches!(te, TrackEvent::Opened { .. }) {
                        n.candidates += 1;
                    }
                });
            });
            tr.observe_frame(&det, frame, &mut |te| {
                if matches!(te, TrackEvent::Opened { .. }) {
                    n.candidates += 1;
                }
            });
        });
        s += take as u64;
    }
    det.finish(&mut |e| {
        if matches!(e, DetectorEvent::Detection(_)) {
            n.boxes += 1;
        }
        tr.push(&e, &mut |te| {
            if matches!(te, TrackEvent::Opened { .. }) {
                n.candidates += 1;
            }
        });
    });
    tr.finish(&mut |te| {
        if matches!(te, TrackEvent::Opened { .. }) {
            n.candidates += 1;
        }
    });
    n
}

/// **The bound.** Four independent minutes of a noise-only airband, at two noise levels: one well
/// above the quantiser (sigma 8 LSB, a normally-gained receiver) and one down in it (sigma 1 LSB,
/// the quantisation-limited case a stock indoor antenna on a quiet band actually gives, and the
/// regime T-237 found the detector's worst behaviour in).
#[test]
fn t990_a_noise_only_band_stays_inside_the_stated_candidate_rate() {
    for sigma in [8.0f64, 1.0] {
        let mut total = Counts::default();
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..SEGMENTS)
                .map(|i| {
                    scope.spawn(move || noise_segment(0x51_0000 + i as u64, sigma, SEGMENT_S, 12.0))
                })
                .collect();
            for h in handles {
                let c = h.join().expect("segment");
                total.boxes += c.boxes;
                total.candidates += c.candidates;
            }
        });
        let exposure_min = SEGMENTS as f64 * SEGMENT_S / 60.0;
        let bound = upper95(total.candidates, exposure_min);
        eprintln!(
            "[T-990] sigma {sigma} LSB: {} candidates and {} boxes in {exposure_min:.2} min of \
             noise-only band at {:.1} Msps -> {bound:.3} candidates/min (95 % upper bound), \
             stated bound {NOISE_BAND_MAX_CANDIDATES_PER_MIN}",
            total.candidates,
            total.boxes,
            FS / 1e6,
        );
        assert!(
            bound <= NOISE_BAND_MAX_CANDIDATES_PER_MIN,
            "[T-990] a noise-only band produced {} candidates in {exposure_min:.2} min \
             ({bound:.3}/min at the 95 % upper bound, sigma {sigma} LSB), past the stated bound \
             of {NOISE_BAND_MAX_CANDIDATES_PER_MIN}/min. The explorer measured ~6.5/min on air in \
             an empty airband and every one of them was handed a band-plan explanation; this is \
             the guard for that. Either the threshold moved or the floor estimate stopped \
             tracking -- do not raise the bound.",
            total.candidates,
        );
    }
}

/// **Self-cleaning.** A blip shorter than the profile's minimum duration never becomes a
/// candidate, however strong and however often it repeats — and the same line held past that
/// duration does, so the gate is a duration test and not deafness.
#[test]
fn t990_short_blips_expire_without_becoming_candidates() {
    let min_frames = DetectorConfig::new(SurveyId::new()).profile.min_frames as usize;
    assert!(min_frames >= 2, "the duration test is what this test tests");
    let run = |on: usize, off: usize, repeats: usize| {
        let prov = provenance(120.5e6, common::FS, 24.0);
        let mut src = GammaFrames::new(BINS, N_AVG, prov, 0x99);
        let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).expect("detector");
        let mut tr = Tracker::new(TrackerConfig::default());
        let mut frame = src.empty_frame();
        let mut floor = floor_frame(&frame, &flat(BINS), 0);
        let quiet = flat(BINS);
        let mut loud = flat(BINS);
        // 20 dB: far above the threshold, so nothing here turns on the SNR.
        add_line(&mut loud, BINS / 2 + 137, 5, 20.0);
        let mut boxes = 0u64;
        let mut candidates = 0u64;
        let mut first = true;
        {
            let mut step = |profile: &[f32],
                            src: &mut GammaFrames,
                            det: &mut Detector,
                            tr: &mut Tracker,
                            boxes: &mut u64,
                            candidates: &mut u64| {
                let flags = if first {
                    first = false;
                    Discontinuity::STREAM_START
                } else {
                    Discontinuity::NONE
                };
                src.fill(&mut frame, profile, flags);
                refresh_floor(&mut floor, &frame, 0, false);
                det.process(&frame, &floor, ClipCount::NONE, &mut |e| {
                    if matches!(e, DetectorEvent::Detection(_)) {
                        *boxes += 1;
                    }
                    tr.push(&e, &mut |te| {
                        if matches!(te, TrackEvent::Opened { .. }) {
                            *candidates += 1;
                        }
                    });
                });
                tr.observe_frame(det, &frame, &mut |te| {
                    if matches!(te, TrackEvent::Opened { .. }) {
                        *candidates += 1;
                    }
                });
            };
            for _ in 0..repeats {
                for _ in 0..on {
                    step(
                        &loud,
                        &mut src,
                        &mut det,
                        &mut tr,
                        &mut boxes,
                        &mut candidates,
                    );
                }
                for _ in 0..off {
                    step(
                        &quiet,
                        &mut src,
                        &mut det,
                        &mut tr,
                        &mut boxes,
                        &mut candidates,
                    );
                }
            }
        }
        det.finish(&mut |e| {
            if matches!(e, DetectorEvent::Detection(_)) {
                boxes += 1;
            }
            tr.push(&e, &mut |te| {
                if matches!(te, TrackEvent::Opened { .. }) {
                    candidates += 1;
                }
            });
        });
        tr.finish(&mut |te| {
            if matches!(te, TrackEvent::Opened { .. }) {
                candidates += 1;
            }
        });
        (boxes, candidates)
    };

    // Blips one frame short of the duration test, repeated 40 times, separated by more than the
    // gap merge so nothing is stitched into a long enough component.
    let (boxes, candidates) = run(min_frames - 1, 8, 40);
    assert_eq!(
        (boxes, candidates),
        (0, 0),
        "[T-990] 40 blips of {} frames (the profile needs {min_frames}) produced {boxes} boxes \
         and {candidates} candidates: a sub-threshold-duration blip must expire, not accumulate \
         into a candidate the inventory then explains.",
        min_frames - 1,
    );

    // The same line, held one frame past the test, twice: that is a candidate.
    let (boxes, candidates) = run(min_frames + 1, 8, 2);
    assert!(
        boxes > 0 && candidates > 0,
        "[T-990] a line held {} frames twice produced {boxes} boxes and {candidates} \
         candidates -- the duration gate above is deafness, not discrimination.",
        min_frames + 1,
    );
}
