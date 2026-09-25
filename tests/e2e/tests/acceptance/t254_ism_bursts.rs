//! T-254: the 902-928 MHz short-burst playground, blind, through the mock SDR **and the IQ ring**.
//!
//! **What is under test.** CLAUDE.md invariant 1, in the band the invariant itself names as its
//! canonical playground: *"a signal is a time-frequency region, not a persistent carrier … such
//! ephemeral emissions are first-class — never forced into a 'steady emitter parked on one
//! frequency' shape"*. The 902-928 MHz US ISM band is where rtl_433 sensors, remotes and LoRa put
//! short bursts everywhere, so it is where a detector that only models carriers fails visibly.
//!
//! The scene (`lora_ism_burst`, shared with T-255) carries three species, and the whole point is
//! what separates them:
//!
//! | species | on air | what it is here |
//! |---|---|---|
//! | `cw` at 902.82 MHz | the whole recording | the **control**: the one thing that really is a carrier |
//! | `fsk-burst` at 902.94 MHz | 6 x 16.7 ms, 0.20-1.22 s, then silent | the rtl_433-shaped **repeating burster that stops** |
//! | `lora-packet` at 903.10 MHz | one 132 ms packet at 2.00 s | the **ephemeral one-off** |
//!
//! Three claims, one run:
//!
//! 1. [`ism_bursts_are_time_frequency_regions_with_a_real_time_extent`] — the bursters are
//!    detected with a bounded time extent and the carrier is not. Both directions are asserted,
//!    because "everything has a time extent" and "everything is a carrier" are both passable by a
//!    system that models only one shape.
//! 2. [`each_ism_burster_is_one_emitter_not_a_scatter_of_skirt_fragments`] — one physical emitter,
//!    one inventory row (T-250's 82-candidates-around-one-signal failure, in the burst band).
//! 3. [`ephemeral_ism_emissions_are_catalogued_as_past_events_not_live_candidates`] — the one-off
//!    and the stopped burster are rows on `GET /api/events` with their own timespans (T-264,
//!    ADR-0017 TM-8), and by the run's live edge they read `ended`, not `live`, while the carrier
//!    reads `live`. Since **T-591** it also asserts that `/api/events` and `/api/inventory` give
//!    **every** emitter in the window the same liveness — this scene is where the disagreement was
//!    measured (3 of 3 events `open: true` beside rows reading `ended`), and the fix was one
//!    derivation rather than two constants that match.
//!
//! **Through the IQ ring, in both senses the ticket means.** The samples reach detection through
//! the ring (`cfg.iq_buffer.enabled = Some(true)`; a lossless replay leaves it off by default),
//! and the ring's tune journal is what the API reads to decide an emitter's idle gap
//! (`hk_api::coverage::ObservedCoverage`, T-410/ADR-0019 §3): a continuous dwell on one centre is
//! a measured revisit period of one STFT frame, not "unknown". Without the ring wired, every
//! liveness answer falls back to the conservative 60 s gap and nothing inside a 6 s recording can
//! ever read `ended` — which is exactly why claim 3 needs it.
//!
//! **Blind.** The generator's spreading factor, symbol rate, sensor id, payload and every
//! emission's box live in the fixture's annotations, which `blind_replay`/`blind_config` strip and
//! seal before the mock SDR opens the recording. No frequency in this file is ever handed to the
//! run: truth is opened after the run, only to assert. Every threshold below was fixed, with its
//! arithmetic, before the suite was first run.

use std::sync::{Arc, Mutex, OnceLock};

use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_e2e::{Fixture, SynthRequest};
use hk_model::{
    Detection, FreqRange, IdleGap, Liveness, Region, TimeRange, Timestamp, Watched,
    presence_in_window,
};
use hk_store::iqbuffer::IqBufferConfig;
use serde_json::{Value, json};

use crate::blind::{BlindSource, assert_truth_found, blind_config, blind_replay, recording_start};
use crate::common::*;

const T254: &str = "T-254";
const AWARE_053: &str = "AWARE-053";

// ---------------------------------------------------------------------------------------------
// The scene. Shape only: what is on the air, and when, is the fixture's business.
// ---------------------------------------------------------------------------------------------

/// Recording length, s. Long enough that each burster is followed by silence it can be seen to
/// have stopped in, at 500 kSps ci8 (6 MB through the ring).
const DURATION_S: f64 = 6.0;

/// The scene: the shared 902-928 MHz generator, with the burst schedule this suite needs.
///
/// Six sensor bursts in the first fifth of the recording and one LoRa packet a second later, both
/// followed by silence. Nothing here tells the run anything — the parameters shape the *air*, and
/// the annotations they produce are stripped before the device opens the recording.
fn scene() -> SynthRequest {
    SynthRequest::new("lora_ism_burst")
        .seed(254)
        .param("duration_s", DURATION_S)
        // The rtl_433-shaped sensor: a short burst every 200 ms, six times, then quiet.
        .param("n_fsk_bursts", 6)
        .param("first_fsk_s", 0.2)
        .param("fsk_period_s", 0.2)
        // The one-off: a single packet, well after the sensor has gone quiet.
        .param("n_packets", 1)
        .param("first_packet_s", 2.0)
}

// ---------------------------------------------------------------------------------------------
// A-priori thresholds. Fixed with their derivations before the suite was run.
// ---------------------------------------------------------------------------------------------

/// Fraction of the recording a **bursty** emitter's detections may collectively occupy.
///
/// Truth: the sensor is on air for 6 x 16.7 ms = 100 ms of 6 s (1.7 %), and even a detector that
/// joined all six bursts across their 183 ms silences would draw one 1.02 s box (17 %). The bound
/// is twice that hull, so it passes any reasonable joining and still fails the one reading this
/// test exists to catch: a burst modelled as a carrier that happens to persist (100 %).
const MAX_EPHEMERAL_COVERAGE: f64 = 0.34;

/// Fraction of the recording the **carrier** control's detections must occupy.
///
/// The CW is on air for all of it, so the only question is detector dropout. Two thirds leaves
/// generous room for that and still cannot be reached by anything ephemeral in this scene, whose
/// widest possible hull is 17 %. Without this the first bound could be satisfied by a detector
/// that simply sees very little.
const MIN_ONGOING_COVERAGE: f64 = 0.66;

/// Seconds a detection may sit outside a truth span and still be attributed to it.
///
/// The detection resolution here is one STFT frame (~1 ms at 500 kSps / 512 bins) and a box may
/// be grown by the tracker's `max_transition_gap_s`; 50 ms is a few tens of frames, far short of
/// the 183 ms silence between bursts, so it can never merge two truth spans by itself.
const TIME_SLACK_S: f64 = 0.05;

/// Seconds after a burster's last truth emission by which its presence must have ended.
///
/// This is the end detector's own latency and nothing else: an interval closes after one
/// [`IdleGap`] of observed silence, and a band dwelled on continuously takes the
/// [`IdleGap::continuous`] floor of 1 s (ADR-0019 §3). One gap plus one slack is the earliest a
/// correct implementation can answer and the latest a correct one may take.
const END_LATENCY_S: f64 = 1.0 + TIME_SLACK_S;

/// Inventory rows a single physical emitter may produce: one.
///
/// Not a tolerance. T-250 found 82 candidates around one FM signal, and the rule it restored is
/// the one asserted here — near-duplicate detections of one emission merge into one emitter.
const ROWS_PER_EMITTER: usize = 1;

/// Detections allowed in a window a real capture's analysis pass **measured** as silent.
///
/// Carried over unchanged from `fm_band_2026_09_15.rs`, which derives it from the detector's
/// design (per-cell `Pfa` 1e-6, a seed plus three connected frames; `hk-detect`'s
/// `false_alarm.rs` asserts <= 2 boxes per 1.02 MHz*h of ideal noise, and this window is ~1000x
/// smaller an exposure). It is not re-derived here, and it is not tightened to today's output.
const SILENT_WINDOW_MAX_DETECTIONS: usize = 2;

// ---------------------------------------------------------------------------------------------
// The run.
// ---------------------------------------------------------------------------------------------

/// One blind run of the scene, shared by the tests below.
pub struct IsmRun {
    /// Data directory (SQLite, tiles).
    pub dir: TempDir,
    /// The private truth, read only after the run, only to assert.
    pub fx: Fixture,
    /// The run's IQ ring, kept alive so the API can read its tune journal.
    pub ring: Arc<hk_pipeline::iqbuffer::IqBufferService>,
}

/// The shared run; `None` when the synthetic generator is unavailable (skip).
fn run() -> Option<&'static IsmRun> {
    static RUN: OnceLock<Option<IsmRun>> = OnceLock::new();
    RUN.get_or_init(|| {
        let out = match SynthRequest::generate(&scene()) {
            Ok(out) => out,
            Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
                eprintln!("SKIP {}: {err}", module_path!());
                return None;
            }
            Err(err) => panic!("synthetic scenario generation failed: {err}"),
        };
        let fx = out.fixture(0).unwrap();
        let mut cfg = blind_config(&fx.meta_path, "t254ism", BlindSource::default(), json!({}));
        // The ticket's "through the IQ ring": a lossless replay leaves the ring off (the recording
        // is already the history), so this run asks for it, sized for what it will actually hold —
        // 6 s at 500 kSps ci8 is 6 MB, and the default quota would size a ring for a 20 MSps device
        // over half an hour and then refuse it for want of free space. Configuration, not truth.
        cfg.cfg.iq_buffer = IqBufferConfig {
            enabled: Some(true),
            retention_s: 60.0,
            max_bytes: Some(64 << 20),
            min_free_bytes: Some(0),
            ..IqBufferConfig::default()
        };
        let dir = cfg.dir;
        let handle = crate::blind::start(cfg.cfg, cfg.replay);
        let ring = handle.iq_buffer();
        // T-178: the ring opens in the background, and until it has, capture is read and not
        // buffered. Waiting here is what makes "through the IQ ring" a fact about the run rather
        // than a hope: without it a fast unpaced replay can finish before the ring exists, the
        // tune journal stays empty, and every liveness answer below silently falls back to the
        // conservative 60 s idle gap — which no 6 s recording can ever clear.
        assert!(
            ring.wait_allocated(std::time::Duration::from_secs(120)),
            "[{T254}] the IQ ring did not finish opening"
        );
        assert!(
            ring.enabled(),
            "[{T254}] the IQ ring is disabled: {:?}",
            ring.status(None, None, 0).reason
        );
        finish(handle);
        let status = ring.status(None, None, 1000);
        assert!(
            !status.segments.is_empty(),
            "[{T254}] the run buffered no IQ: {} samples retained, {} skipped while the ring \
             opened",
            status.samples,
            status.allocation_skipped_samples
        );
        eprintln!(
            "[{T254}] IQ ring: {} segments, {:.3} s retained ({} samples, {} skipped while \
             opening)",
            status.segments.len(),
            status.span_s,
            status.samples,
            status.allocation_skipped_samples
        );
        Some(IsmRun { dir, fx, ring })
    })
    .as_ref()
}

/// The API over a finished run, wired as `hk serve` wires it: the inventory store **and the run's
/// IQ ring**, which is what lets presence read a measured idle gap instead of the conservative
/// fallback.
fn serve(r: &IsmRun) -> Server {
    let state = ApiState {
        inventory: Some(Arc::new(Mutex::new(repo(&r.dir.0)))),
        iq_buffer: Some(Arc::new(hk_cli::control::PipelineIqBuffer(Arc::clone(
            &r.ring,
        )))),
        ..ApiState::default()
    };
    Server::start(
        ServerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            Token::from_config(API_TOKEN).unwrap(),
        ),
        state,
    )
    .unwrap()
}

/// An authenticated `GET` returning parsed JSON; panics on a non-200.
fn get_json(server: &Server, path: &str) -> Value {
    let (status, body) = api_get(server.local_addr(), path);
    assert_eq!(status, 200, "{path}: {}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

/// The recording's own clock: `(start, end)` as Unix seconds.
fn window_s(fx: &Fixture) -> (f64, f64) {
    let t0 = recording_start(fx).as_unix_nanos() as f64 * 1e-9;
    (t0, t0 + DURATION_S)
}

/// Every detection the run wrote.
fn detections(r: &IsmRun) -> Vec<Detection> {
    repo(&r.dir.0)
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
        .expect("detections")
}

/// `[(start_s, end_s)]` of the detections whose centre falls inside `band`, in seconds from the
/// recording start.
///
/// Attribution is by **centre**, as in T-255: a box that has merged two emitters overlaps both
/// bands and would otherwise be counted as evidence about each of them.
fn spans_in(dets: &[Detection], band: FreqRange, t0_ns: i64) -> Vec<(f64, f64)> {
    dets.iter()
        .filter(|d| (band.lo_hz..=band.hi_hz).contains(&d.f_center_hz))
        .map(|d| {
            (
                (d.time.start.as_unix_nanos() - t0_ns) as f64 * 1e-9,
                (d.time.end.as_unix_nanos() - t0_ns) as f64 * 1e-9,
            )
        })
        .collect()
}

/// `[(start, end)]` merged into disjoint intervals, and their total length, s.
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

/// The three species' bands, read from truth after the run: `(label, band, on-air span)`.
///
/// The band is the emission's own occupied bandwidth (a carrier has none, so it gets the
/// harness's [`crate::blind::MIN_CENTER_TOL_HZ`]-shaped floor of +/-10 kHz), which is what makes
/// "one row for this emitter" a question about this emitter rather than about the band.
fn species(fx: &Fixture, kind: &str) -> (FreqRange, f64, f64) {
    let items = fx.of_kind(kind);
    assert!(!items.is_empty(), "[{T254}] the scene has no {kind}");
    let center = items[0].center_hz();
    let bw = items[0].expect_f64("bandwidth_hz").max(20e3);
    let start = items
        .iter()
        .map(|t| t.t_start_s)
        .fold(f64::INFINITY, f64::min);
    let end = items.iter().map(|t| t.t_end_s).fold(0.0, f64::max);
    (FreqRange::centered(center, bw), start, end)
}

// ---------------------------------------------------------------------------------------------
// The suite. One scene, one run, four claims: nextest gives each `#[test]` its own process, so
// four test functions would mean four replays of the same recording (~4 min each). The phases are
// named and each prints what it measured, so a failure still says which claim broke.
// ---------------------------------------------------------------------------------------------

#[test]
fn ism_burst_playground_is_a_catalogue_of_time_frequency_regions() {
    let Some(r) = run() else { return };
    every_ism_emission_is_found_blind_with_a_reasonable_explanation(r);
    ism_bursts_are_time_frequency_regions_with_a_real_time_extent(r);
    each_ism_burster_is_one_emitter_not_a_scatter_of_skirt_fragments(r);
    ephemeral_ism_emissions_are_catalogued_as_past_events_not_live_candidates(r);
    a_stopped_burster_has_ended_in_the_presence_model_too(r);
}

// ---------------------------------------------------------------------------------------------
// 0. The blind ground-truth check: everything on the air is found, with a sensible explanation.
// ---------------------------------------------------------------------------------------------

fn every_ism_emission_is_found_blind_with_a_reasonable_explanation(r: &IsmRun) {
    // The harness's own rule (T-047): every truth emission detected within frequency/extent and
    // time tolerance, and an emitter matching it carrying a reasonable service in its top 3. The
    // truth kinds decide what "reasonable" is (`blind::reasonable_services`); nothing is looked up.
    let found = assert_truth_found(T254, &r.dir.0, &r.fx, 0.0, true);
    eprintln!(
        "[{T254}/{AWARE_053}] {} truth emissions checked",
        found.len()
    );
}

// ---------------------------------------------------------------------------------------------
// 1. A time extent, not a carrier.
// ---------------------------------------------------------------------------------------------

fn ism_bursts_are_time_frequency_regions_with_a_real_time_extent(r: &IsmRun) {
    let dets = detections(r);
    assert!(!dets.is_empty(), "[{T254}] the run produced no detections");
    let t0_ns = recording_start(&r.fx).as_unix_nanos();

    let (sensor, sensor_start, sensor_end) = species(&r.fx, "fsk-burst");
    let (oneoff, oneoff_start, oneoff_end) = species(&r.fx, "lora-packet");
    let (carrier, _, _) = species(&r.fx, "cw");
    assert!(
        (902e6..928e6).contains(&sensor.center_hz()),
        "[{T254}] the scene must sit in 902-928 MHz US ISM: {:.4} MHz",
        sensor.center_hz() / 1e6
    );

    // Each truth burst is found as a region overlapping its own span, not as a share of one long
    // box: the union of what was detected over each burst's span must be non-empty.
    for t in
        r.fx.of_kind("fsk-burst")
            .iter()
            .chain(&r.fx.of_kind("lora-packet"))
    {
        let hits = spans_in(
            &dets,
            FreqRange::centered(t.center_hz(), t.expect_f64("bandwidth_hz").max(20e3)),
            t0_ns,
        )
        .into_iter()
        .filter(|(s, e)| *e >= t.t_start_s - TIME_SLACK_S && *s <= t.t_end_s + TIME_SLACK_S)
        .count();
        eprintln!(
            "[{T254}] {} {:.4} MHz {:.3}..{:.3} s ({:.0} ms): {hits} detections",
            t.kind,
            t.center_hz() / 1e6,
            t.t_start_s,
            t.t_end_s,
            (t.t_end_s - t.t_start_s) * 1e3
        );
        assert!(
            hits > 0,
            "[{T254}] a {:.0} ms {} at {:.4} MHz produced no detection over its own span",
            (t.t_end_s - t.t_start_s) * 1e3,
            t.kind,
            t.center_hz() / 1e6
        );
    }

    // The two directions of invariant 1, measured the same way on the same run.
    let ephemeral = [("sensor", sensor), ("one-off", oneoff)];
    for (label, band) in ephemeral {
        let covered = covered_s(spans_in(&dets, band, t0_ns));
        eprintln!(
            "[{T254}] {label} at {:.4} MHz: detections occupy {:.3} s of {DURATION_S} s ({:.1} %, \
             ceiling {:.0} %)",
            band.center_hz() / 1e6,
            covered,
            100.0 * covered / DURATION_S,
            100.0 * MAX_EPHEMERAL_COVERAGE
        );
        assert!(
            covered <= MAX_EPHEMERAL_COVERAGE * DURATION_S,
            "[{T254}] the {label} at {:.4} MHz is on air for well under a fifth of the recording, \
             but its detections occupy {:.3} s of {DURATION_S} s ({:.0} %): it is being modelled \
             as a carrier, which CLAUDE.md invariant 1 forbids",
            band.center_hz() / 1e6,
            covered,
            100.0 * covered / DURATION_S
        );
    }
    let carried = covered_s(spans_in(&dets, carrier, t0_ns));
    eprintln!(
        "[{T254}] carrier at {:.4} MHz: detections occupy {:.3} s of {DURATION_S} s ({:.1} %)",
        carrier.center_hz() / 1e6,
        carried,
        100.0 * carried / DURATION_S
    );
    assert!(
        carried >= MIN_ONGOING_COVERAGE * DURATION_S,
        "[{T254}] the control carrier is on air throughout, but its detections occupy only \
         {carried:.3} s of {DURATION_S} s: the bound the bursters just cleared means nothing if \
         the detector simply sees very little"
    );

    // And the bursters stop. A detection is a time-frequency *region*, so the last one attributed
    // to an emitter that went quiet must end when it did, within the end detector's own latency.
    for (label, band, last) in [
        ("sensor", sensor, sensor_end),
        ("one-off", oneoff, oneoff_end),
    ] {
        let latest = spans_in(&dets, band, t0_ns)
            .iter()
            .map(|(_, e)| *e)
            .fold(f64::NEG_INFINITY, f64::max);
        eprintln!(
            "[{T254}] {label}: last truth emission ends {last:.3} s, last detection ends \
             {latest:.3} s (ceiling {:.3} s)",
            last + END_LATENCY_S
        );
        assert!(
            latest <= last + END_LATENCY_S,
            "[{T254}] the {label} stopped at {last:.3} s but a detection attributed to it runs to \
             {latest:.3} s, past the {END_LATENCY_S} s the end detector may take"
        );
    }
    eprintln!(
        "[{T254}] sensor on air {sensor_start:.3}..{sensor_end:.3} s, one-off \
         {oneoff_start:.3}..{oneoff_end:.3} s, {} detections in the run",
        dets.len()
    );
}

// ---------------------------------------------------------------------------------------------
// 2. One emitter per burst, not a scatter of skirt fragments.
// ---------------------------------------------------------------------------------------------

fn each_ism_burster_is_one_emitter_not_a_scatter_of_skirt_fragments(r: &IsmRun) {
    let server = serve(r);
    let (t0, t1) = window_s(&r.fx);
    let rows = get_json(
        &server,
        &format!("/api/inventory?limit=500&t0={t0}&t1={t1}"),
    );
    let rows = rows["entries"].as_array().cloned().unwrap_or_default();
    eprintln!("[{T254}] {} inventory rows in the window", rows.len());

    for kind in ["fsk-burst", "cw"] {
        let (band, _, _) = species(&r.fx, kind);
        let mine: Vec<&Value> = rows
            .iter()
            .filter(|row| {
                let f = row["f_center_hz"].as_f64().unwrap_or(f64::NAN);
                (band.lo_hz..=band.hi_hz).contains(&f)
            })
            .collect();
        eprintln!(
            "[{T254}] {kind} at {:.4} MHz (+/-{:.1} kHz): {} inventory rows {:?}",
            band.center_hz() / 1e6,
            (band.hi_hz - band.lo_hz) / 2e3,
            mine.len(),
            mine.iter()
                .map(|r| (
                    r["f_center_hz"].as_f64().unwrap_or(f64::NAN) / 1e6,
                    r["bandwidth_hz"].as_f64().unwrap_or(f64::NAN) / 1e3,
                    r["state"].as_str().unwrap_or("?")
                ))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            mine.len(),
            ROWS_PER_EMITTER,
            "[{T254}] one physical {kind} emitter at {:.4} MHz produced {} inventory rows: \
             near-duplicate detections of one emission must merge into one emitter (T-250)",
            band.center_hz() / 1e6,
            mine.len()
        );
    }

    // The chirp is reported, not asserted: a LoRa up-chirp has no stable frequency, and how many
    // rows its sweep resolves into is T-255's measurement, not this suite's claim.
    let (chirp, _, _) = species(&r.fx, "lora-packet");
    let n = rows
        .iter()
        .filter(|row| {
            let f = row["f_center_hz"].as_f64().unwrap_or(f64::NAN);
            (chirp.lo_hz..=chirp.hi_hz).contains(&f)
        })
        .count();
    eprintln!(
        "[{T254}] one-off chirp at {:.4} MHz: {n} inventory rows (reported; see T-255)",
        chirp.center_hz() / 1e6
    );
}

// ---------------------------------------------------------------------------------------------
// 3. Ephemera are catalogued as past events, not left sitting as live candidates.
// ---------------------------------------------------------------------------------------------

fn ephemeral_ism_emissions_are_catalogued_as_past_events_not_live_candidates(r: &IsmRun) {
    let server = serve(r);
    let (t0, t1) = window_s(&r.fx);

    // --- The catalogue (T-264): one row per presence interval, each with its own timespan. ---
    let cat = get_json(
        &server,
        &format!("/api/events?f_lo=902e6&f_hi=928e6&t0={t0}&t1={t1}&limit=500"),
    );
    let events = cat["events"].as_array().cloned().unwrap_or_default();
    eprintln!(
        "[{T254}/{AWARE_053}] /api/events over 902-928 MHz x {DURATION_S} s: {} events over {} \
         emitters",
        events.len(),
        cat["emitters"].as_array().map_or(0, Vec::len)
    );
    assert!(
        !events.is_empty(),
        "[{T254}] nothing on the air in the burst playground reached the durable catalogue: \
         {cat}"
    );

    for kind in ["fsk-burst", "lora-packet"] {
        let (band, start, end) = species(&r.fx, kind);
        let mine: Vec<&Value> = events
            .iter()
            .filter(|e| {
                let f = e["f_center_hz"].as_f64().unwrap_or(f64::NAN);
                (band.lo_hz..=band.hi_hz).contains(&f)
            })
            .collect();
        eprintln!(
            "[{T254}] {kind} events: {:?}",
            mine.iter()
                .map(|e| (
                    e["t_start_s"].as_f64().unwrap_or(f64::NAN) - t0,
                    e["t_end_s"].as_f64().unwrap_or(f64::NAN) - t0,
                    e["duration_s"].as_f64().unwrap_or(f64::NAN),
                    e["open"].as_bool()
                ))
                .collect::<Vec<_>>()
        );
        assert!(
            !mine.is_empty(),
            "[{T254}] the {kind} on air {start:.3}..{end:.3} s is in no event row: an ephemeral \
             emission must be catalogued as a past event with its own timespan (ADR-0017 TM-8)"
        );
        // Its timespan is the event's own, and it is a timespan, not the window.
        for e in &mine {
            let d = e["duration_s"].as_f64().unwrap_or(f64::NAN);
            assert!(
                d.is_finite() && d > 0.0 && d <= MAX_EPHEMERAL_COVERAGE * DURATION_S,
                "[{T254}] a {kind} event claims {d} s of a {DURATION_S} s window: an ephemeral \
                 emission's event carries its own timespan, not the window's: {e}"
            );
        }
        // And it covers when the emission really was on air.
        let covers = mine.iter().any(|e| {
            let (s, x) = (
                e["t_start_s"].as_f64().unwrap_or(f64::NAN) - t0,
                e["t_end_s"].as_f64().unwrap_or(f64::NAN) - t0,
            );
            s <= start + TIME_SLACK_S && x >= end - TIME_SLACK_S
        });
        assert!(
            covers,
            "[{T254}] no {kind} event covers {start:.3}..{end:.3} s, when it was really on air"
        );
    }

    let open_events = events
        .iter()
        .filter(|e| e["open"].as_bool() == Some(true))
        .count();
    eprintln!(
        "[{T254}] {open_events} of {} events read open=true against a {DURATION_S} s window",
        events.len()
    );

    // --- The live list: by the run's live edge the ephemera have ended and the carrier has not. ---
    let rows = get_json(
        &server,
        &format!("/api/inventory?limit=500&t0={t0}&t1={t1}"),
    );
    let rows = rows["entries"].as_array().cloned().unwrap_or_default();

    // --- T-591: the two surfaces cannot disagree about one emitter's liveness. ---
    //
    // This scene is where the defect was measured: `/api/events` derived each event's `open` with
    // `IdleGap::conservative()` (60 s) while `/api/inventory` derived the same emitter's
    // `liveness` from the gap the **ring** measured (T-410), so in a recording shorter than a
    // minute 3 of 3 events read `open: true` beside rows for the same emitter in the same window
    // reading `ended`. Liveness is a property of the emitter — one interval `[start, end?]`,
    // ongoing until an end is affirmatively detected (ADR-0017/0019) — never a property of the
    // route asked, so there is exactly one derivation now (`ObservedCoverage::track`).
    //
    // Asserted over EVERY emitter the catalogue lists, not a sampled one, and the number compared
    // is printed: a comparison of zero emitters would pass while proving nothing, which is the
    // vacuous-guard failure this project keeps meeting.
    let catalogued = cat["emitters"].as_array().cloned().unwrap_or_default();
    let mut compared = 0usize;
    let mut disagreed: Vec<String> = Vec::new();
    for m in &catalogued {
        let id = m["id"].as_str().unwrap_or_default();
        let Some(row) = rows.iter().find(|r| r["id"].as_str() == Some(id)) else {
            continue;
        };
        compared += 1;
        let (ev, inv) = (
            m["liveness"].as_str().unwrap_or("?"),
            row["presence"]["liveness"].as_str().unwrap_or("?"),
        );
        // The same fact a third way: an emitter reads live exactly when one of its events in this
        // window is still open. This is the literal shape of the measured defect.
        let any_open = events
            .iter()
            .filter(|e| e["emitter_id"].as_str() == Some(id))
            .any(|e| e["open"].as_bool() == Some(true));
        if ev != inv || any_open != (ev == Liveness::Live.as_str()) {
            disagreed.push(format!(
                "{id} at {:.6} MHz: /api/events liveness={ev} (any open event: {any_open}), \
                 /api/inventory presence.liveness={inv}",
                m["f_center_hz"].as_f64().unwrap_or(f64::NAN) / 1e6,
            ));
        }
    }
    eprintln!(
        "[{T254}] T-591: liveness compared across /api/events and /api/inventory for {compared} \
         of {} catalogued emitters; {} disagreed",
        catalogued.len(),
        disagreed.len()
    );
    assert!(
        compared > 0,
        "[{T254}] T-591 compared {compared} emitters — a vacuous comparison proves nothing: \
         catalogue {cat}"
    );
    assert!(
        disagreed.is_empty(),
        "[{T254}] T-591: {} of {compared} emitters read a different liveness on /api/events than \
         on /api/inventory for the same window. Liveness is a property of the emitter, not of the \
         route asked (ADR-0017/0019), and docs/api.md promises the two surfaces cannot disagree. \
         {disagreed:#?}",
        disagreed.len()
    );
    for (kind, want) in [
        ("fsk-burst", Liveness::Ended),
        ("lora-packet", Liveness::Ended),
        ("cw", Liveness::Live),
    ] {
        let (band, _, end) = species(&r.fx, kind);
        let mine: Vec<&Value> = rows
            .iter()
            .filter(|row| {
                let f = row["f_center_hz"].as_f64().unwrap_or(f64::NAN);
                (band.lo_hz..=band.hi_hz).contains(&f)
            })
            .collect();
        let states: Vec<(&str, f64, f64)> = mine
            .iter()
            .map(|row| {
                (
                    row["presence"]["liveness"].as_str().unwrap_or("?"),
                    row["presence"]["on_air_s"].as_f64().unwrap_or(f64::NAN),
                    row["presence"]["confidence"].as_f64().unwrap_or(f64::NAN),
                )
            })
            .collect();
        eprintln!(
            "[{T254}] {kind} (last on air {end:.3} s, live edge {DURATION_S} s): \
             (liveness, on_air_s, confidence) {states:?}, want {}",
            want.as_str()
        );
        assert!(!mine.is_empty(), "[{T254}] no inventory row for the {kind}");
        assert!(
            states.iter().any(|(l, _, _)| *l == want.as_str()),
            "[{T254}] the {kind} reads {states:?} at the run's live edge, not {}: a signal that \
             stopped must not sit in the live list as though it were still transmitting, and one \
             that never stopped must not read ended",
            want.as_str()
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 4. The user's field case: intermittent DATA-like bursts near 100.3 MHz (B0.497, 2026-09-15).
// ---------------------------------------------------------------------------------------------

/// The 100.3 MHz field observation, re-asked now that burst detection is verified above.
///
/// **The observation.** During the 2026-09-15 live demo the user saw short, intermittent,
/// possibly-AM bursts near 100.3 MHz, distinct from the 100.3 WFM station. The planning log
/// (B0.497) records it with three candidate explanations and no verdict: a subcarrier, an
/// adjacent narrowband emitter, or a receiver artefact of the strong station.
///
/// **What the recordings can and cannot answer.** The only real capture of that band is
/// `fixtures/hackrf/capture-2026-09-15-fm-band` (45 s at 100.8 MHz / 2.4 MSps), and its
/// annotation pass measured 100.3 MHz **silent** — the burst was not transmitting while the
/// recording ran (B0.497 follow-up; the fixture states a silence, not a signal). So the honest
/// question this test can ask is not "was the burst found" but its converse, which is the one
/// that matters for a burst detector: **now that ephemeral emissions are first-class, does the
/// burst machinery invent one where the air was measured empty?**
///
/// **Measured, 2026-09-21** (printed by the test, and the reason it is worth keeping): over the
/// 45 s capture the burst path fires **2 detections** at 100.298 and 100.302 MHz, 4.4 and 5.2 dB,
/// each a few milliseconds long — exactly the ephemeral shape the user described, in a window the
/// analysis pass measured as empty — and they mint **one candidate emitter** at 100.3004 MHz,
/// never confirmed. So the field sighting is *not* reproduced from this recording (the burst was
/// not transmitting while it ran), and what sits at 100.3 MHz in the catalogue is the designed
/// false-alarm allowance being spent, not a signal. That is the answer this test pins: the count
/// stays inside the allowance, and nothing there is ever promoted to Confirmed.
///
/// That is the T-316 shape — 866 detections and 5 candidate emitters in a measured-flat band —
/// and it is the failure a burst-friendly detector is most prone to. The bound is the designed
/// false-alarm rate, carried over unchanged from the suite that derived it; nothing here is
/// tuned to what this fixture happens to produce.
#[test]
fn field_case_100p3_mhz_bursts_are_not_invented_where_the_air_was_measured_silent() {
    let Some(meta) = real_fixture_in("fixtures/hackrf/capture-2026-09-15-fm-band", "iq") else {
        eprintln!("SKIP {}: the LFS capture is not fetched", module_path!());
        return;
    };
    let fx = Fixture::load(&meta).unwrap();
    let run = blind_replay(&meta, "t254field", BlindSource::default());
    let dets = repo(&run.dir.0)
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
        .expect("detections");

    // Truth, opened only now: the windows the analysis pass measured as empty.
    let scenario = fx
        .scenario()
        .unwrap_or_else(|| panic!("[{T254}] the fixture has no scenario truth"));
    let absent: Vec<(String, f64, f64)> = scenario
        .get("measured_absent")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("[{T254}] the fixture records no measured_absent windows"))
        .iter()
        .filter(|w| w["assert_silent"].as_bool().unwrap_or(false))
        .map(|w| {
            (
                w["label"].as_str().unwrap_or_default().to_owned(),
                w["f_lo_hz"].as_f64().unwrap(),
                w["f_hi_hz"].as_f64().unwrap(),
            )
        })
        .collect();
    let field = absent
        .iter()
        .find(|(_, lo, hi)| (*lo..=*hi).contains(&100.3e6))
        .unwrap_or_else(|| {
            panic!("[{T254}] the fixture records no measured-silent window covering 100.3 MHz")
        });
    let (label, lo, hi) = (field.0.as_str(), field.1, field.2);

    let t0_ns = recording_start(&fx).as_unix_nanos();
    let here: Vec<&Detection> = dets
        .iter()
        .filter(|d| (lo..=hi).contains(&d.f_center_hz))
        .collect();
    eprintln!(
        "[{T254}] field case: {label} ({:.4}-{:.4} MHz), 45 s of real air measured silent there. \
         {} detections: {:?}",
        lo / 1e6,
        hi / 1e6,
        here.len(),
        here.iter()
            .map(|d| (
                d.f_center_hz / 1e6,
                (d.time.start.as_unix_nanos() - t0_ns) as f64 * 1e-9,
                (d.time.end.as_unix_nanos() - t0_ns) as f64 * 1e-9,
                d.snr_peak_db
            ))
            .collect::<Vec<_>>()
    );
    let rows: Vec<&Value> = run
        .api_rows
        .iter()
        .filter(|row| {
            let f = row["f_center_hz"].as_f64().unwrap_or(f64::NAN);
            (lo..=hi).contains(&f)
        })
        .collect();
    eprintln!(
        "[{T254}] field case: {} inventory rows at 100.3 MHz {:?}",
        rows.len(),
        rows.iter()
            .map(|r| (
                r["f_center_hz"].as_f64().unwrap_or(f64::NAN) / 1e6,
                r["state"].as_str().unwrap_or("?")
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        here.len() <= SILENT_WINDOW_MAX_DETECTIONS,
        "[{T254}] {} detections in {label}, which the analysis pass measured as silent \
         (allowance {SILENT_WINDOW_MAX_DETECTIONS}, the detector's designed false-alarm rate): \
         the burst path is inventing ephemera where there was no signal",
        here.len()
    );
    assert!(
        !rows
            .iter()
            .any(|r| r["state"].as_str() == Some("confirmed")),
        "[{T254}] a Confirmed emitter at 100.3 MHz, where 45 s of real air was measured silent: \
         {rows:?}"
    );
}

/// The model's own answer, beside the API's: with the idle gap the **ring** measured, a stopped
/// burster's presence has ended.
///
/// This is the same question [`ephemeral_ism_emissions_are_catalogued_as_past_events_not_live_candidates`]
/// asks over HTTP, asked directly of `hk_model::presence` so that a disagreement between the two
/// is attributable. `IdleGap::continuous` is not a choice: ADR-0019 §3 defines it as the gap for a
/// band the receiver never looked away from, and this run dwells on one centre for its whole
/// length.
fn a_stopped_burster_has_ended_in_the_presence_model_too(r: &IsmRun) {
    let repo = repo(&r.dir.0);
    let t0 = recording_start(&r.fx);
    let edge = Timestamp::from_unix_nanos(t0.as_unix_nanos() + (DURATION_S * 1e9) as i64);
    let window = TimeRange::new(t0, edge);
    let gap = IdleGap::continuous();

    for (kind, want) in [("fsk-burst", Liveness::Ended), ("cw", Liveness::Live)] {
        let (band, _, end) = species(&r.fx, kind);
        let entries = inventory(
            &repo,
            hk_model::InventoryQuery {
                freq: Some(band),
                ..Default::default()
            },
        );
        assert!(!entries.is_empty(), "[{T254}] no emitter for the {kind}");
        let got: Vec<(Liveness, f64, usize)> = entries
            .iter()
            .map(|e| {
                // Silence as elapsed time: the reading this test was written against (T-940 added
                // the coverage-observed reading, which the served routes take via coverage.rs).
                let intervals = repo
                    .presence_intervals(e.emitter.id, gap, edge, &Watched::unrecorded())
                    .unwrap();
                let p = presence_in_window(&intervals, window, gap);
                (p.liveness, p.on_air_s, intervals.len())
            })
            .collect();
        eprintln!(
            "[{T254}] {kind} (last on air {end:.3} s): presence {got:?} at a {:.0} s idle gap, \
             want {}",
            gap.as_nanos() as f64 * 1e-9,
            want.as_str()
        );
        assert!(
            got.iter().any(|(l, _, _)| *l == want),
            "[{T254}] the {kind} reads {got:?}, not {}",
            want.as_str()
        );
    }
}
