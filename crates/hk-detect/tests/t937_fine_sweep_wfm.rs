//! T-937: broadcast FM must come out of the **fine sweep** as one region per station.
//!
//! The explorer's live FM run (2026-09-25, 88–108 MHz at 2.4 Msps) reported 349 candidates for
//! ~19 stations — one station appearing as 13 candidates of 9–47 kHz scattered across its own
//! 230 kHz — while the same stations at 20 Msps came out as single 123–200 kHz detections. The
//! ticket asked which stage splits a WFM emission at the fine-sweep resolution and why the
//! "overlap is an error signal" rule did not clean it up.
//!
//! **Nothing splits it. It is deleted, and the fragments are what survives.** The stage is
//! T-316's *narrow floor-feature* guard ([`hk_detect::step`], "Narrow floor features"). Its width
//! band is derived from the OS-CFAR window and is therefore measured in **bins**: wider than
//! `2G+2` (10) and narrower than `2(G+R)+1` (41). At the detection resolution the pipeline picks
//! for a 2.4 Msps sweep (512 bins, 4.69 kHz) a 180 kHz broadcast-FM station is ~38 bins — inside
//! that band — so every station in the sweep is handed to the guard's power-statistics test. A
//! station carrying real programme audio is noise-like at 2 ms / 4.7 kHz cells, so it reads
//! *floor-like*, and from the 16th frame of the segment (`stat_min_frames`) the guard takes the
//! station's own running mean as the floor there. The station then stops being detected at all;
//! what is left are the sporadic narrow boxes its loudest excursions still make above its own
//! mean — 9–47 kHz, milliseconds long, scattered across its span. T-316 measured and accepted
//! this ("cut from 46 boxes into 68"); at the fine sweep it is total, because the veto latch that
//! would have rescued the station needs a *signal-like* frame and is reset by every retune, and a
//! sweep retunes constantly.
//!
//! **Why overlap-is-an-error never fired.** It cannot: the fragments are a *partition*, not an
//! overlap. `hk_model::relate::bands_compete` requires overlap ≥ `OVERLAP_MIN_FRACTION` (0.6) of
//! **both** bands, and abutting fragments overlap each other by ~0; a 14 kHz fragment against a
//! 230 kHz station box overlaps the wider band by 6 %, which the same rule deliberately refuses to
//! treat as a duplicate so that a subcarrier is never swallowed by its host. Inventory-level
//! overlap resolution can never reassemble a fragmented emission, so the fix has to be where the
//! fragments are made.
//!
//! **The fix** ([`hk_detect::StepGuardConfig::narrow_feature_max_db`]): the guard never acts on a
//! span whose median running mean stands 10 dB or more above the floor reference. Raised noise is
//! a few dB — the case T-316 was built on is a 75 kHz shelf 4.6 dB up, and the synthetic shelves
//! in `false_alarm.rs` reach 8 dB — while a station 10 dB clear of the reference is an emission by
//! the detector's own published standard (`Rules::marginal_snr_db`). Suppressing a non-marginal
//! emission on a statistic that reads a real emission floor-like in up to a third of its frames is
//! the wrong trade in the wrong direction.
//!
//! The scene is four broadcast-FM stations modulated by band-limited noise (what real programme
//! audio looks like to a 2 ms / 4.7 kHz detector cell), replayed as ci8 through the real chain at
//! the pipeline's own fine-sweep geometry, and — as the ticket's control — at the 20 Msps geometry
//! where the explorer saw one detection per station.

mod common;

use std::f64::consts::TAU;

use common::*;
use hk_detect::{Detector, DetectorConfig};
use hk_dsp::synth::{Rng, complex_noise};
use hk_model::SurveyId;
use num_complex::{Complex, Complex32};

/// Noise power per sample, full scale 1 (−40 dBFS).
const NOISE: f64 = 1e-4;
/// Per-bin SNR of the four stations, dB: ordinary strong locals on an 8-bit front end.
const SNRS: [f64; 4] = [18.0, 20.0, 22.0, 24.0];
/// Tuned centre.
const CENTER_HZ: f64 = 98.9e6;
/// Station offsets from the centre. Off the 200 kHz raster and off DC, and far enough apart that
/// no two stations' emissions touch: any merging here would be the scene's fault, not the
/// detector's.
const OFFSETS: [f64; 4] = [-800e3, -400e3, 200e3, 700e3];
/// Carson bandwidth of the stations, Hz (75 kHz deviation, 15 kHz audio).
const STATION_BW_HZ: f64 = 180e3;
/// A detection counts as the station's **region** when it is at least this wide: a third of the
/// station's Carson bandwidth. Below it the box is a **fragment** — the 9–47 kHz shards the
/// explorer counted 349 of.
const REGION_MIN_HZ: f64 = STATION_BW_HZ / 3.0;

/// One WFM station: 75 kHz deviation on band-limited noise, the spectrum of real programme audio
/// at the detector's resolution. Pure tones are the wrong fixture here — a tone-modulated carrier
/// is not noise-like at the bin level and never reaches the guard this test is about.
fn add_wfm(x: &mut [Complex32], fs: f64, offset_hz: f64, snr_db: f64, seed: u64) {
    let mut rng = Rng::new(seed);
    let amp = (NOISE / fs * STATION_BW_HZ * undb(snr_db)).sqrt();
    let a = (-TAU * 15e3 / fs).exp();
    let (mut audio, mut sub) = (0.0f64, 0.0f64);
    let mut phase = 0.0f64;
    for s in x.iter_mut() {
        audio = a * audio + (1.0 - a) * (rng.unit() * 2.0 - 1.0) * 3.0;
        sub = a * sub + (1.0 - a) * (rng.unit() * 2.0 - 1.0) * 3.0;
        let composite = 0.9 * (0.9 * audio).clamp(-1.0, 1.0) + 0.1 * sub.clamp(-1.0, 1.0);
        phase += TAU * (offset_hz + 75e3 * composite) / fs;
        *s += Complex32::new((amp * phase.cos()) as f32, (amp * phase.sin()) as f32);
    }
}

/// A detection reduced to its time–frequency box.
#[derive(Clone, Copy, Debug)]
struct Box {
    t: (f64, f64),
    f: (f64, f64),
}

impl Box {
    fn width_hz(&self) -> f64 {
        self.f.1 - self.f.0
    }

    /// Overlaps the station's own band (its Carson bandwidth, generously).
    fn at(&self, station_hz: f64) -> bool {
        self.f.0 < station_hz + STATION_BW_HZ && self.f.1 > station_hz - STATION_BW_HZ
    }
}

struct Replay {
    label: &'static str,
    fs: f64,
    dur_s: f64,
    boxes: Vec<Box>,
    narrow_guarded_frames: u64,
    frames: u64,
}

/// Replays the scene through STFT → floor tracker → detector at `(fft, avg)`.
fn replay(label: &'static str, fs: f64, fft: usize, avg: usize, dur_s: f64, cap_db: f64) -> Replay {
    let n = (fs * dur_s) as usize;
    let mut rng = Rng::new(0x937);
    let mut x = complex_noise(&mut rng, n, NOISE);
    for (k, off) in OFFSETS.iter().enumerate() {
        add_wfm(&mut x, fs, *off, SNRS[k], 0x937_000 + k as u64);
    }
    let iq: Vec<Complex<i8>> = x
        .iter()
        .map(|s| {
            Complex::new(
                (s.re * 127.0).round().clamp(-128.0, 127.0) as i8,
                (s.im * 127.0).round().clamp(-128.0, 127.0) as i8,
            )
        })
        .collect();
    let mut cfg = DetectorConfig::new(SurveyId::new());
    if let Some(g) = cfg.step_guard.as_mut() {
        g.narrow_feature_max_db = cap_db;
    }
    let mut det = Detector::new(cfg).expect("detector");
    let (out, frames) = replay_ci8(
        &iq,
        fs,
        &[(0, provenance(CENTER_HZ, fs, 16.0))],
        &ChainConfig::new(fft, avg),
        &mut det,
    );
    let boxes = out
        .detections
        .iter()
        .map(|d| Box {
            t: (d.t_start_s(fs), d.t_end_s(fs)),
            f: (d.f_lo_hz, d.f_hi_hz),
        })
        .collect();
    Replay {
        label,
        fs,
        dur_s,
        boxes,
        narrow_guarded_frames: det.stats().narrow_guarded_frames,
        frames,
    }
}

/// Seconds of the run covered by `boxes` (union of their time extents).
fn covered_s(boxes: &[Box]) -> f64 {
    let mut v: Vec<(f64, f64)> = boxes.iter().map(|b| b.t).collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut total = 0.0;
    let mut cur: Option<(f64, f64)> = None;
    for (s, e) in v {
        match cur {
            Some((cs, ce)) if s <= ce => cur = Some((cs, ce.max(e))),
            Some((cs, ce)) => {
                total += ce - cs;
                cur = Some((s, e));
            }
            None => cur = Some((s, e)),
        }
    }
    total + cur.map_or(0.0, |(s, e)| e - s)
}

/// What one station looked like: `(regions, fragments, covered fraction of the run)`.
fn station(r: &Replay, offset_hz: f64) -> (usize, usize, f64) {
    let f = CENTER_HZ + offset_hz;
    let here: Vec<Box> = r.boxes.iter().filter(|b| b.at(f)).copied().collect();
    let regions: Vec<Box> = here
        .iter()
        .filter(|b| b.width_hz() >= REGION_MIN_HZ)
        .copied()
        .collect();
    let fragments = here.len() - regions.len();
    (regions.len(), fragments, covered_s(&regions) / r.dur_s)
}

fn report(r: &Replay) -> String {
    let mut s = format!(
        "{}: {:.1} Msps, {} frames, {} boxes, narrow-guarded frames {}/{}\n",
        r.label,
        r.fs / 1e6,
        r.frames,
        r.boxes.len(),
        r.narrow_guarded_frames,
        r.frames
    );
    for off in OFFSETS {
        let (regions, fragments, cov) = station(r, off);
        s += &format!(
            "  {:.3} MHz: {regions} region(s), {fragments} fragment(s), {:.0} % of the run \
             covered\n",
            (CENTER_HZ + off) / 1e6,
            cov * 100.0
        );
    }
    s
}

/// The product rule, as a list of what breaks it: every station is **one region**, present for
/// the whole run, with no shower of narrow fragments beside it.
///
/// `max_regions` is the number of `max_duration_s` boxes the run is cut into — a region emitted at
/// the duration cap and continuing is still one region, not fragmentation.
fn one_region_per_station(r: &Replay, max_regions: usize) -> Vec<String> {
    let mut failures = Vec::new();
    for off in OFFSETS {
        let f = CENTER_HZ + off;
        let (regions, fragments, cov) = station(r, off);
        if regions == 0 {
            failures.push(format!(
                "{f:.0} Hz: no region at all ({fragments} fragments)"
            ));
            continue;
        }
        if regions > max_regions {
            failures.push(format!(
                "{f:.0} Hz: {regions} regions, more than the {max_regions} the duration cap makes"
            ));
        }
        if fragments > 3 {
            failures.push(format!(
                "{f:.0} Hz: {fragments} narrow fragments beside its region"
            ));
        }
        if cov < 0.9 {
            failures.push(format!(
                "{f:.0} Hz: its region covers only {:.0} % of the run",
                cov * 100.0
            ));
        }
    }
    failures
}

fn assert_one_region_per_station(r: &Replay, max_regions: usize) {
    let failures = one_region_per_station(r, max_regions);
    assert!(failures.is_empty(), "{}\n{failures:#?}", report(r));
}

/// The deliverable: at the 2.4 Msps fine-sweep geometry every station is one region.
#[test]
fn every_fm_station_is_one_region_in_the_fine_sweep() {
    let r = replay(
        "fine sweep (2.4 Msps)",
        2.4e6,
        512,
        10,
        2.0,
        DetectorConfig::new(SurveyId::new())
            .step_guard
            .expect("step guard on by default")
            .narrow_feature_max_db,
    );
    eprintln!("{}", report(&r));
    // 2 s at the 1 s duration cap: two boxes per station, plus one for a cap landing off-phase.
    assert_one_region_per_station(&r, 3);
}

/// The control the ticket names: the same stations at 20 Msps, where the explorer already saw one
/// detection each. A 180 kHz station is ~37 bins there too, so this is not a width-band accident —
/// it is that a continuous dwell eventually earns the veto that a sweep's short segments never do.
#[test]
fn the_same_stations_are_one_region_each_at_20_msps() {
    let r = replay(
        "dwell (20 Msps)",
        20e6,
        4096,
        10,
        0.5,
        DetectorConfig::new(SurveyId::new())
            .step_guard
            .expect("step guard on by default")
            .narrow_feature_max_db,
    );
    eprintln!("{}", report(&r));
    assert_one_region_per_station(&r, 1);
}

/// The attribution, by re-injecting the defect: lift the cap and the narrow floor-feature guard is
/// back on the stations, which is where the explorer's 349 candidates come from. This is what
/// makes the test above a proof rather than a coincidence — the same scene, the same chain and the
/// same rule, with one a-priori bound removed.
///
/// Both halves of the live report reappear at once: a station **swallowed** (its box gone, because
/// the guard made the station its own floor) and a station **shattered** (many boxes where there
/// is one emission, because only its loudest excursions still clear that floor).
#[test]
fn without_the_cap_the_narrow_feature_guard_swallows_the_stations() {
    let r = replay("fine sweep, cap lifted", 2.4e6, 512, 10, 2.0, f64::INFINITY);
    eprintln!("{}", report(&r));
    assert!(
        r.narrow_guarded_frames * 2 > r.frames,
        "with the cap lifted the guard should be acting on most frames: {}",
        report(&r)
    );
    let broken = one_region_per_station(&r, 3);
    assert!(
        broken.len() >= 2,
        "the product rule should break without the cap: {}\n{broken:#?}",
        report(&r)
    );
    let swallowed = OFFSETS
        .into_iter()
        .filter(|&o| station(&r, o).2 < 0.5)
        .count();
    let shattered = OFFSETS
        .into_iter()
        .filter(|&o| station(&r, o).0 > 3)
        .count();
    assert!(
        swallowed >= 1 && shattered >= 1,
        "expected both failure modes the live run showed — a station swallowed and a station \
         shattered — but {swallowed} were swallowed and {shattered} shattered: {}",
        report(&r)
    );
}
