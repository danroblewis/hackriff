//! T-255: LoRa up-chirps in 902-928 MHz US ISM, blind, through the mock SDR device.
//!
//! **What is under test.** CLAUDE.md invariant 1: *"a signal is a time-frequency region, not a
//! persistent carrier … they need no carrier and no stable frequency"*. Every other emitter in
//! the acceptance suites sits on a carrier, so nothing so far separates "models a time-frequency
//! region" from "models a carrier that happens to persist". A LoRa chirp sweeps its whole channel
//! every symbol and so has no stable frequency by construction.
//!
//! The scene carries three species so the two axes of the invariant are separable:
//!
//! | species | stable frequency | bounded time extent |
//! |---|---|---|
//! | `cw` | yes | no (whole recording) |
//! | `fsk-burst` | yes | yes (~17 ms) |
//! | `lora-packet` | **no** | yes |
//!
//! Two configurations run, and the difference between them is the point:
//!
//! - **SF9 / 125 kHz** (a real US915 LoRaWAN uplink data rate). Symbol duration 4.096 ms is
//!   *exactly* the pipeline's spectrum frame, so the entire sweep happens inside one frame and the
//!   chirp presents as a 125 kHz-wide burst. The region is right; the diagonal is not merely
//!   un-drawable (ADR-0017 §1.3) — at this frame it is **unobservable**.
//! - **SF12 / 125 kHz** (a private LoRa link; legal in 902-928 under FCC 15.247 digital modulation
//!   and not a LoRaWAN US915 uplink DR). Symbol duration 32.8 ms spans eight frames, stepping
//!   15.6 kHz each, so the sweep *is* resolvable in principle. What the detector does with it is
//!   measured and printed rather than assumed.
//!
//! **Blind.** Spreading factor, bandwidth, coding rate and payload live in the fixture's
//! annotations, which `blind_replay` strips and seals before the device opens the recording. The
//! run is told nothing but where the device says it is tuned. Truth is read after the run, to
//! assert.
//!
//! **What is deferred, and why.** The task also names invariant 5 (region-extending decode:
//! extending a region decodes only the newly-arrived part). That is ADR-0017 stage **TM-10**
//! (`T-265`), which the ADR marks *"Blocked on the analyze engine — `POST /api/analyze` answers
//! 501"*, and MAUTO is unscheduled. There is no bounded-region reader in the tree: no
//! `RegionReader` type, and the two-reader rule (Rule L vs Rule I) is prose in ADR-0017 §6 that
//! nothing enforces. Asserting on it here would mean inventing the contract inside a test, so
//! this suite asserts the chirp-detection half in full and the fixture carries what TM-10 will
//! need — `sweep_polyline`, per-packet ordered symbols, a CRC-valid payload — so the decode half
//! becomes assertions rather than new fixture work.
//! [`t255_chirp_fixture_is_ready_for_region_extending_decode`] states that boundary as a test
//! instead of a comment.

use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{Detection, FreqRange, Region};
use hk_pipeline::config::{PipelineSettings, detection_resolution};

use crate::blind::{BlindSource, blind_replay, recording_start};
use crate::common::*;

const T255: &str = "T-255";

// ---- A-priori thresholds. Fixed with their arithmetic before the suite was run. ----

/// Fraction of a packet's span the detections must collectively account for.
///
/// The emission is continuous for its whole span at 12 dB SNR in its channel. A detector that
/// models a time extent must account for most of it; half is a deliberately loose floor that still
/// fails a detector seeing a chirp as a scatter of unrelated blips.
const MIN_TIME_COVERAGE: f64 = 0.5;

/// Fraction of the channel the chirp's detections must span between them.
///
/// The union of the boxes is what ADR-0017 §1.3 calls the bounding box of the sweep. The emission
/// really does occupy the whole channel over a packet, so the boxes must cover at least half of it
/// or most of the signal was never seen at all.
const MIN_CHIRP_BOX_SPAN: f64 = 0.5;

/// The chirp must account for its channel *somehow*: either its detected centres move across at
/// least this fraction of the channel (the sweep was resolved into a ladder), or its median box is
/// at least this wide (the sweep was hulled into one rectangle). One or the other must hold, since
/// the emission covers the channel; which one holds is the interesting part and is printed.
const MIN_CHIRP_ACCOUNTED: f64 = 0.5;

/// Spread of detected centres for the **steady carrier**, as a fraction of the chirp's channel.
///
/// The control, at 0.05 x 125 kHz = 6.25 kHz — six spectrum bins, so an unmodulated carrier clears
/// it comfortably. Without it, [`MIN_CHIRP_ACCOUNTED`] could be satisfied by a measure that is
/// reading noise rather than frequency.
///
/// **This bound applies to the carrier and to nothing else.** The first version of this test also
/// applied it to the 2-FSK bursts and they failed at 18.9 kHz — correctly, because a 2-FSK
/// emission *has* two frequencies, 2 x 9.6 kHz = 19.2 kHz apart, and the detector resolves both.
/// The threshold was not too tight; it was the wrong quantity for that emitter. The rule a
/// fixed-frequency emitter really obeys is [`burst_centres_within_own_bandwidth`], which is
/// derived below rather than fitted to the number that came out.
const MAX_STABLE_CENTER_SPREAD: f64 = 0.05;

/// A non-swept emitter's detected centres cannot leave the band it occupies.
///
/// This is the correct statement of "fixed frequency" for an emitter with a bandwidth of its own,
/// and it is a bound, not a fit: the scene's 2-FSK burst puts its two tones at +/- 9.6 kHz, so its
/// centres span 19.2 kHz inside an occupied bandwidth of 2 x 9.6 + 4.8 = 24 kHz (Carson). A swept
/// emitter is not excused by it — a chirp's centres spread across its channel *and* its boxes
/// widen, which is what [`MIN_CHIRP_ACCOUNTED`] tests — but nothing that stays put can break it.
fn burst_centres_within_own_bandwidth(spread_hz: f64, own_bandwidth_hz: f64) -> bool {
    spread_hz <= own_bandwidth_hz
}

/// Widest box still attributable to a fixed-frequency emitter, Hz.
///
/// The widest non-chirp emission in the scene is the 24 kHz FSK burst, so this is twice that: wide
/// enough for a real box with skirts, narrow enough to exclude a box that has swallowed the
/// 125 kHz chirp as well. Boxes above it are counted and reported as species merges rather than
/// silently attributed to whichever channel their centre happens to land in — attributing them
/// would make every species look like every other one, which is exactly what the first draft of
/// this test did.
const MAX_FIXED_BOX_HZ: f64 = 50e3;

/// Seconds a detection may sit outside a truth span and still be attributed to it (a frame or two).
const TIME_SLACK_S: f64 = 0.02;

/// A detection with its time expressed in seconds from the recording start.
type Timed<'a> = (f64, f64, &'a Detection);

/// `[(start, end)]` merged into disjoint intervals, and their total length.
fn covered_s(mut spans: Vec<(f64, f64)>) -> f64 {
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut total = 0.0;
    let mut cur: Option<(f64, f64)> = None;
    for (s, e) in spans {
        match &mut cur {
            Some(c) if s <= c.1 => c.1 = c.1.max(e),
            Some(c) => {
                total += c.1 - c.0;
                cur = Some((s, e));
            }
            None => cur = Some((s, e)),
        }
    }
    total + cur.map_or(0.0, |c| c.1 - c.0)
}

fn spread(values: impl Iterator<Item = f64>) -> f64 {
    let (lo, hi) = values.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
        (lo.min(v), hi.max(v))
    });
    if lo.is_finite() { hi - lo } else { 0.0 }
}

fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// The truth `sweep_polyline` as `[(t_s from the recording start, absolute Hz)]`.
///
/// This is the diagonal ADR-0017 §1.3 says the box stands in for, carried in truth by the
/// generator so the cost of the rectangle is measurable rather than merely acknowledged.
fn polyline_points(v: &serde_json::Value) -> Vec<(f64, f64)> {
    v.as_array()
        .expect("sweep_polyline is an array")
        .iter()
        .filter_map(|p| {
            let p = p.as_array()?;
            Some((p.first()?.as_f64()?, p.get(1)?.as_f64()?))
        })
        .collect()
}

/// How much of the channel the emission sweeps **within one analysis frame**, per frame, Hz.
///
/// This is the quantity that decides which of ADR-0017 §1.3's limits applies (T-294). A frame is
/// the smallest time a frame-based detector can tell apart, so a box it draws cannot be narrower
/// than the emission's excursion inside one: where that excursion is the whole channel the
/// rectangle is forced and no ladder exists to resolve, and where it is a fraction the ladder is
/// there to be found. It is read off the truth polyline, so it holds whatever `sf` happens to be.
fn frame_excursions(pts: &[(f64, f64)], frame_s: f64) -> Vec<f64> {
    let Some(&(t0, _)) = pts.first() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let (mut i, mut k) = (0usize, 0usize);
    while i < pts.len() {
        let hi = t0 + (k + 1) as f64 * frame_s;
        let (mut lo_f, mut hi_f) = (f64::INFINITY, f64::NEG_INFINITY);
        let start = i;
        while i < pts.len() && pts[i].0 < hi {
            lo_f = lo_f.min(pts[i].1);
            hi_f = hi_f.max(pts[i].1);
            i += 1;
        }
        // Only whole frames: a part frame at the packet's end under-reports its excursion.
        if i > start && i < pts.len() {
            out.push(hi_f - lo_f);
        }
        k += 1;
    }
    out
}

/// Detections attributed to one emitter: **centre** inside `ch` and box no wider than `max_w`.
///
/// Centre rather than overlap, and capped in width, because a single box that has merged two
/// emitters overlaps both channels and would otherwise be counted as evidence about each of them.
fn attributed<'a>(dets: &'a [Detection], t0_ns: i64, ch: FreqRange, max_w: f64) -> Vec<Timed<'a>> {
    dets.iter()
        .filter(|d| (ch.lo_hz..=ch.hi_hz).contains(&d.f_center_hz) && d.obw_hz <= max_w)
        .map(|d| {
            (
                (d.time.start.as_unix_nanos() - t0_ns) as f64 / 1e9,
                (d.time.end.as_unix_nanos() - t0_ns) as f64 / 1e9,
                d,
            )
        })
        .collect()
}

/// Runs one configuration of the scene blind and asserts invariant 1 on it.
fn chirp_acceptance(req: SynthRequest, tag: &str) {
    let out = match SynthRequest::generate(&req) {
        Ok(out) => out,
        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {}: {err}", module_path!());
            return;
        }
        Err(err) => panic!("synthetic scenario generation failed: {err}"),
    };
    let fx = out.fixture(0).unwrap();
    let run = blind_replay(&fx.meta_path, tag, BlindSource::default());
    let repo = repo(&run.dir.0);
    let dets = repo
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
        .expect("detections");
    assert!(
        !dets.is_empty(),
        "[{T255}/{tag}] the run produced no detections at all"
    );

    // ---- Truth, opened only now, and only to say where to look and what the answer is. ----
    let packets = fx.of_kind("lora-packet");
    let cws = fx.of_kind("cw");
    let bursts = fx.of_kind("fsk-burst");
    assert!(
        packets.len() >= 2 && cws.len() == 1 && !bursts.is_empty(),
        "[{T255}/{tag}] the scene must carry all three species: {} chirps, {} cw, {} bursts",
        packets.len(),
        cws.len(),
        bursts.len()
    );
    let bw = packets[0].expect_f64("bandwidth_hz");
    let sf = packets[0].expect_f64("spreading_factor");
    let t_sym = packets[0].expect_f64("symbol_duration_s");
    let t0_ns = recording_start(&fx).as_unix_nanos();
    let chan = FreqRange::new(packets[0].f_lo_hz, packets[0].f_hi_hz);
    assert!(
        (902e6..928e6).contains(&chan.center_hz()),
        "[{T255}/{tag}] the scene must sit in 902-928 MHz US ISM, not the 2.4 GHz LoRa variant: \
         {:.4} MHz",
        chan.center_hz() / 1e6
    );

    let chirp = attributed(&dets, t0_ns, chan, 2.0 * bw);
    let steady = attributed(
        &dets,
        t0_ns,
        FreqRange::centered(cws[0].center_hz(), MAX_FIXED_BOX_HZ),
        MAX_FIXED_BOX_HZ,
    );
    let bursty = attributed(
        &dets,
        t0_ns,
        FreqRange::centered(bursts[0].center_hz(), MAX_FIXED_BOX_HZ),
        MAX_FIXED_BOX_HZ,
    );
    let merged = dets
        .iter()
        .filter(|d| d.obw_hz > 2.0 * bw.max(MAX_FIXED_BOX_HZ))
        .count();
    eprintln!(
        "[{T255}/{tag}] SF{sf:.0} BW {:.0} kHz at {:.4} MHz, T_sym {:.2} ms: {} detections on the \
         chirp, {} on the steady carrier, {} on the bursts, {merged} too wide to attribute to any \
         one emitter ({} total)",
        bw / 1e3,
        chan.center_hz() / 1e6,
        t_sym * 1e3,
        chirp.len(),
        steady.len(),
        bursty.len(),
        dets.len()
    );

    // ---- 1. The chirp is found, and found as something with a time extent. ----
    for p in &packets {
        let mine: Vec<&Timed<'_>> = chirp
            .iter()
            .filter(|(s, e, _)| *e >= p.t_start_s - TIME_SLACK_S && *s <= p.t_end_s + TIME_SLACK_S)
            .collect();
        let span = p.t_end_s - p.t_start_s;
        let covered = covered_s(
            mine.iter()
                .map(|(s, e, _)| (s.max(p.t_start_s), e.min(p.t_end_s)))
                .collect(),
        );
        eprintln!(
            "[{T255}/{tag}] packet {}: {:.3}..{:.3} s ({:.0} ms), {} detections covering {:.0} ms \
             ({:.0} %), box widths {:.1}..{:.1} kHz",
            p.f64("packet_index").unwrap_or(-1.0),
            p.t_start_s,
            p.t_end_s,
            span * 1e3,
            mine.len(),
            covered * 1e3,
            100.0 * covered / span,
            mine.iter()
                .map(|(_, _, d)| d.obw_hz / 1e3)
                .fold(f64::INFINITY, f64::min),
            mine.iter()
                .map(|(_, _, d)| d.obw_hz / 1e3)
                .fold(0.0, f64::max),
        );
        assert!(
            !mine.is_empty(),
            "[{T255}/{tag}] a {:.0} ms chirp at {:.4} MHz produced no detection at all",
            span * 1e3,
            chan.center_hz() / 1e6
        );
        assert!(
            covered >= MIN_TIME_COVERAGE * span,
            "[{T255}/{tag}] detections account for {:.0} ms of a {:.0} ms chirp \
             ({:.0} %, floor {:.0} %)",
            covered * 1e3,
            span * 1e3,
            100.0 * covered / span,
            100.0 * MIN_TIME_COVERAGE
        );
    }

    // ---- 2. The chirp covers its channel; the fixed-frequency species do not move. ----
    let chirp_spread = spread(chirp.iter().map(|(_, _, d)| d.f_center_hz));
    let chirp_width = median(chirp.iter().map(|(_, _, d)| d.obw_hz).collect());
    let steady_spread = spread(steady.iter().map(|(_, _, d)| d.f_center_hz));
    let burst_spread = spread(bursty.iter().map(|(_, _, d)| d.f_center_hz));
    let resolved = chirp_spread >= MIN_CHIRP_ACCOUNTED * bw;
    eprintln!(
        "[{T255}/{tag}] chirp: centres span {:.1} kHz ({:.2} of the channel), median box \
         {:.1} kHz ({:.2}) -> the sweep was {}. Controls: steady carrier centres span \
         {:.2} kHz, bursts {:.2} kHz.",
        chirp_spread / 1e3,
        chirp_spread / bw,
        chirp_width / 1e3,
        chirp_width / bw,
        if resolved {
            "RESOLVED into a ladder of boxes"
        } else {
            "HULLED into one rectangle (ADR-0017 §1.3)"
        },
        steady_spread / 1e3,
        burst_spread / 1e3
    );
    assert!(
        resolved || chirp_width >= MIN_CHIRP_ACCOUNTED * bw,
        "[{T255}/{tag}] the chirp sweeps {:.0} kHz every {:.1} ms, but its detections neither \
         move ({:.1} kHz of centre spread) nor widen ({:.1} kHz median box): most of the emission \
         was never accounted for",
        bw / 1e3,
        t_sym * 1e3,
        chirp_spread / 1e3,
        chirp_width / 1e3
    );
    assert!(
        !steady.is_empty() && steady_spread <= MAX_STABLE_CENTER_SPREAD * bw,
        "[{T255}/{tag}] the control failed: the steady carrier's {} detections span {:.1} kHz of \
         centre, so this measure is reading something other than frequency",
        steady.len(),
        steady_spread / 1e3
    );

    // ---- 3. The bounding box of the sweep (ADR-0017 §1.3), in the system's own output. ----
    let lo = chirp
        .iter()
        .map(|(_, _, d)| d.freq().lo_hz)
        .fold(f64::INFINITY, f64::min);
    let hi = chirp
        .iter()
        .map(|(_, _, d)| d.freq().hi_hz)
        .fold(f64::NEG_INFINITY, f64::max);
    let frame_s = packets[0].expect_f64("/sweep/instantaneous_frame_s");
    eprintln!(
        "[{T255}/{tag}] bounding box {:.1} kHz ({:.2} of the channel). Truth says the emission \
         occupies {:.1} kHz at a time in a {:.2} ms frame, so the rectangle is {:.1}x wider than \
         the signal; nothing in the stored Detection (one f_center_hz, one obw_hz) can say so.",
        (hi - lo) / 1e3,
        (hi - lo) / bw,
        packets[0].expect_f64("/sweep/instantaneous_bandwidth_hz") / 1e3,
        frame_s * 1e3,
        packets[0].expect_f64("/sweep/box_to_instantaneous_ratio"),
    );
    assert!(
        hi - lo >= MIN_CHIRP_BOX_SPAN * bw,
        "[{T255}/{tag}] the chirp's detections span only {:.1} kHz of a {:.0} kHz sweep",
        (hi - lo) / 1e3,
        bw / 1e3
    );

    // ---- 3b. WHICH limit applies here (T-294), read off the truth polyline. ----
    //
    // ADR-0017 §1.3 used to read as one limit. It is three, and this is the one that decides
    // between the first two: the emission's frequency excursion inside a single analysis frame.
    // The frame is the run's own (`detection_resolution` at the fixture's rate), not a constant.
    let (fft_len, averages) = detection_resolution(fx.sample_rate, &PipelineSettings::default());
    let run_frame_s = (averages * fft_len) as f64 / fx.sample_rate;
    let excursions: Vec<f64> = packets
        .iter()
        .flat_map(|p| {
            frame_excursions(
                &polyline_points(
                    p.get("sweep_polyline")
                        .expect("the sweep the box stands in for"),
                ),
                run_frame_s,
            )
        })
        .collect();
    assert!(
        !excursions.is_empty(),
        "[{T255}/{tag}] the polyline covers no whole analysis frame"
    );
    let in_frame = median(excursions.clone());
    let frames_per_symbol = t_sym / run_frame_s;
    eprintln!(
        "[{T255}/{tag}] analysis frame {:.3} ms ({averages} x {fft_len} bins at {:.0} kS/s), \
         {frames_per_symbol:.2} frames per symbol. The emission sweeps {:.1} kHz ({:.2} of the \
         channel) inside one frame, so a frame-based box cannot be narrower than that; the \
         detector's median box is {:.1} kHz ({:.2}).",
        run_frame_s * 1e3,
        fx.sample_rate / 1e3,
        in_frame / 1e3,
        in_frame / bw,
        chirp_width / 1e3,
        chirp_width / bw,
    );
    // The boundary, from geometry: the excursion inside a frame is about min(1, frame/T_sym) of
    // the channel, so a symbol that fits inside one frame sweeps all of it and one spanning
    // several sweeps a fraction. Sampling the polyline every 1 ms leaves at most a quarter of a
    // fold unseen, hence 0.5 rather than 1.0 on the intra-frame side.
    if frames_per_symbol <= 1.0 {
        assert!(
            in_frame >= 0.5 * bw,
            "[{T255}/{tag}] the whole sweep falls inside one {:.3} ms frame, yet the polyline says \
             it only covers {:.1} kHz of a {:.0} kHz channel there",
            run_frame_s * 1e3,
            in_frame / 1e3,
            bw / 1e3
        );
    } else if frames_per_symbol >= 4.0 {
        assert!(
            in_frame <= 0.3 * bw,
            "[{T255}/{tag}] a symbol spanning {frames_per_symbol:.1} frames should sweep a \
             fraction of the channel in each, but the polyline says {:.2} of it",
            in_frame / bw
        );
    }
    // The consequence, in the system's own output: a box may not be narrower than the emission's
    // excursion inside one frame. At SF9 that *forces* the rectangle — this is an observability
    // limit at this frame, not a drawing one — and at SF12 it leaves the ladder room to exist,
    // which is what the centre spread above then shows.
    assert!(
        chirp_width >= 0.8 * in_frame,
        "[{T255}/{tag}] the median box is {:.1} kHz but the emission sweeps {:.1} kHz inside one \
         {:.3} ms frame: a frame-based box cannot be narrower than what it contains",
        chirp_width / 1e3,
        in_frame / 1e3,
        run_frame_s * 1e3
    );

    // ---- 4. The third species, so "bounded time extent" is not doing the work alone. ----
    let burst_cover = covered_s(bursty.iter().map(|(s, e, _)| (*s, *e)).collect());
    let steady_cover = covered_s(steady.iter().map(|(s, e, _)| (*s, *e)).collect());
    let chirp_cover = covered_s(chirp.iter().map(|(s, e, _)| (*s, *e)).collect());
    let recording_s = fx.n_samples().unwrap() as f64 / fx.sample_rate;
    eprintln!(
        "[{T255}/{tag}] time on air over {recording_s:.2} s: steady carrier {:.0} %, \
         FSK bursts {:.0} % ({} truth bursts), chirp {:.0} % ({} truth packets)",
        100.0 * steady_cover / recording_s,
        100.0 * burst_cover / recording_s,
        bursts.len(),
        100.0 * chirp_cover / recording_s,
        packets.len()
    );
    let burst_bw = bursts[0].bandwidth_hz();
    assert!(
        !bursty.is_empty() && burst_centres_within_own_bandwidth(burst_spread, burst_bw),
        "[{T255}/{tag}] the fixed-frequency bursts must be found and must not wander outside the \
         band they occupy: {} detections, centres spread {:.1} kHz over a {:.1} kHz emission",
        bursty.len(),
        burst_spread / 1e3,
        burst_bw / 1e3
    );
    assert!(
        burst_cover < steady_cover,
        "[{T255}/{tag}] ephemeral bursts cannot be on air longer than a carrier that never stops \
         ({:.0} ms vs {:.0} ms)",
        burst_cover * 1e3,
        steady_cover * 1e3
    );
}

/// SF9 / 125 kHz: a real US915 LoRaWAN uplink data rate, whose symbol is exactly one spectrum
/// frame. The sweep is entirely intra-frame here, so the chirp presents as a wideband burst.
#[test]
fn t255_a_chirp_is_a_region_with_a_time_extent_and_no_stable_frequency() {
    chirp_acceptance(SynthRequest::new("lora_ism_burst").seed(255), "sf9");
}

/// SF12 / 125 kHz: symbol duration 32.8 ms, eight spectrum frames stepping 15.6 kHz each, so the
/// sweep is resolvable in principle. Whether the detector resolves it or hulls it is measured.
#[test]
fn t255_a_slow_chirp_whose_sweep_spans_many_frames() {
    chirp_acceptance(
        SynthRequest::new("lora_ism_burst")
            .seed(255)
            .param("sf", 12)
            .param("payload_bytes", 4)
            .param("duration_s", 1.3)
            .param("first_packet_s", 0.05)
            .param("packet_period_s", 0.65),
        "sf12",
    );
}

/// The invariant-5 boundary, as a test rather than a comment.
///
/// Region-extending decode is ADR-0017 TM-10 (`T-265`), blocked on the analyze engine. What this
/// suite can do now is guarantee the fixture will not be what holds that work up: a bounded-region
/// decoder needs a sweep it can follow, ordered symbols it can check itself against, and a CRC
/// that says whether it got them right. If any of the three disappears from the generator, it
/// fails here rather than inside a task with no reason to suspect the fixture.
#[test]
fn t255_chirp_fixture_is_ready_for_region_extending_decode() {
    let out = synth_or_skip!(SynthRequest::new("lora_ism_burst").seed(255));
    let fx = out.fixture(0).unwrap();
    let packets = fx.of_kind("lora-packet");
    assert!(
        packets.len() >= 2,
        "[{T255}] a region must have something to extend over"
    );
    for p in &packets {
        let polyline = p
            .get("sweep_polyline")
            .and_then(|v| v.as_array())
            .expect("the sweep the bounding box stands in for");
        assert!(
            polyline.len() > 50,
            "[{T255}] a {}-point polyline cannot describe a {:.0} ms sweep",
            polyline.len(),
            (p.t_end_s - p.t_start_s) * 1e3
        );
        // Incremental decode is checked by decoding a prefix and then only the rest, so the
        // symbols have to be per-packet and ordered and the CRC has to cover the payload.
        let symbols = p
            .get("payload_symbols")
            .and_then(|v| v.as_array())
            .expect("per-packet ordered symbol sequence");
        assert!(
            symbols.len() >= 16,
            "[{T255}] {} symbols is too few to split into an already-decoded prefix and a new tail",
            symbols.len()
        );
        assert_eq!(
            p.bool("/frame/crc/valid"),
            Some(true),
            "[{T255}] the packet's CRC must be valid, or a decode cannot self-check"
        );
        assert!(
            !p.str("/frame/payload_hex").unwrap_or_default().is_empty(),
            "[{T255}] the payload is the hidden answer a decode is checked against"
        );
    }
    eprintln!(
        "[{T255}] fixture ready for TM-10 (T-265): {} packets, each with a sweep polyline, an \
         ordered symbol sequence and a CRC-valid payload. The region-extending decode half is \
         NOT asserted here: `POST /api/analyze` answers 501 and no bounded-region reader exists.",
        packets.len()
    );
}
