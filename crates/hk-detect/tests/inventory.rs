//! Tracks into the signal inventory (T-018) acceptance:
//! - AWARE-036: `fsk_burst_train` replayed as two sessions a day apart (STFT → floor → detector →
//!   tracker → `record_track_event`) → one Emitter, count = bursts of both sessions, first/last
//!   seen spanning both, `known_status: unknown` from the clusterer; replaying a session's events
//!   adds nothing; T-013's CRC ground truth then makes it `known`.

mod common;

use common::*;
use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_detect::track::inventory::{record_track_event, track_fingerprint};
use hk_detect::{
    ClipCount, Detector, DetectorConfig, DetectorEvent, TrackEvent, TrackSummary, Tracker,
    TrackerConfig, count_clipped_ci8,
};
use hk_dsp::floor::NoiseFloorTracker;
use hk_dsp::window::WindowKind;
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{
    Assignment, FreqRange, InventoryQuery, KnownStatus, KnownStatusChange, Repository, SampleTime,
    StatusAuthor, SurveyId, TimeRange, Timestamp,
};
use num_complex::Complex;

const AWARE_036: &str = "AWARE-036";
const DAY_NS: i64 = 86_400 * 1_000_000_000;

/// Replays ci8 samples through the detection chain with host time starting at `t0_ns`.
fn replay_tracked(
    samples: &[Complex<i8>],
    fs: f64,
    prov: ProvenanceHandle,
    chain: &ChainConfig,
    t0_ns: i64,
) -> Vec<TrackEvent> {
    let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).unwrap();
    let mut tr = Tracker::new(TrackerConfig::default());
    let welch = WelchConfig {
        fft_len: chain.fft_len,
        overlap: 0,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: true,
    };
    let mut stft = StftProcessor::new(StftConfig::new(welch, chain.averages)).expect("stft");
    let mut floor = NoiseFloorTracker::new(chain.floor).expect("floor");
    let mut events = Vec::new();
    let n = samples.len() as u64;
    let mut s = 0u64;
    while s < n {
        let e = (s + 65_536).min(n);
        let header = BlockHeader {
            time: SampleTime {
                sample_index: s,
                host_time: Timestamp::from_unix_nanos(t0_ns + (s as f64 * 1e9 / fs).round() as i64),
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
                det.process(frame, f, clip, &mut |ev: DetectorEvent| {
                    tr.push(&ev, &mut |te| events.push(te));
                });
                tr.observe_frame(&det, frame, &mut |te| events.push(te));
            },
        );
        s = e;
    }
    det.finish(&mut |ev| tr.push(&ev, &mut |te| events.push(te)));
    tr.finish(&mut |te| events.push(te));
    events
}

fn closed(events: &[TrackEvent]) -> Vec<&TrackSummary> {
    events
        .iter()
        .filter_map(|e| match e {
            TrackEvent::Closed(s) => Some(s),
            _ => None,
        })
        .collect()
}

#[test]
fn aware_036_two_sessions_of_fsk_burst_train_are_one_emitter() {
    let day1 = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(1)
            .param("snr_db", 20.0)
            .param("duration_s", 3.0)
    );
    let day2 = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(2)
            .param("snr_db", 20.0)
            .param("duration_s", 3.0)
    );
    let mut repo = Repository::open_in_memory().unwrap();
    let mut sessions = Vec::new();
    for (day, synth) in [(1, &day1), (2, &day2)] {
        let fx = synth.fixture(0).unwrap();
        let iq = to_ci8(&fx.samples().unwrap());
        let events = replay_tracked(
            &iq,
            fx.sample_rate,
            fixture_provenance(&fx),
            &ChainConfig::new(256, 4),
            day * DAY_NS,
        );
        let truth = fx.of_kind("fsk-burst");
        let band = (truth[0].f_lo_hz - 20e3, truth[0].f_hi_hz + 20e3);
        let mut resolutions = Vec::new();
        for ev in &events {
            if let Some(r) = record_track_event(&mut repo, ev, None).unwrap() {
                if let TrackEvent::Closed(s) = ev {
                    resolutions.push((s.track.id, r));
                }
            }
        }
        for s in closed(&events) {
            eprintln!(
                "{AWARE_036} day {day}: track {:.1} Hz bw {:.0} Hz bursts {} fp {:?}",
                s.track.f_center_hz,
                s.track.bandwidth_hz,
                s.burst_count,
                track_fingerprint(s)
            );
        }
        eprintln!("{AWARE_036} day {day}: {resolutions:?}");
        sessions.push((events, resolutions, band, truth.len() as u64));
    }

    // The burst-train track of each session: the near track carrying every truth burst. The
    // detector/tracker may add a one-off fragment beside it (seed 2 at 256×4: one 2 kHz box
    // ~18 kHz above the train); clustering must keep such a fragment out of this emitter.
    let resolution = |res: &[(hk_model::TrackId, hk_model::Resolution)], s: &TrackSummary| {
        res.iter()
            .find(|(id, _)| *id == s.track.id)
            .map(|(_, r)| r.clone())
            .expect("track recorded")
    };
    let mut trains = Vec::new();
    let mut fragments = Vec::new();
    for (events, res, band, n_truth) in &sessions {
        let near: Vec<&TrackSummary> = closed(events)
            .into_iter()
            .filter(|s| s.track.f_center_hz >= band.0 && s.track.f_center_hz <= band.1)
            .collect();
        let (train, rest): (Vec<_>, Vec<_>) =
            near.into_iter().partition(|s| s.burst_count == *n_truth);
        assert_eq!(
            train.len(),
            1,
            "{AWARE_036}: one burst-train track per session"
        );
        trains.push((train[0].clone(), resolution(res, train[0])));
        fragments.extend(rest.into_iter().map(|s| resolution(res, s)));
    }
    let (t1, r1) = &trains[0];
    let (t2, r2) = &trains[1];
    assert!(r1.created, "{AWARE_036}: first session creates the emitter");
    assert!(
        matches!(r2.assignment, Assignment::Fingerprint { .. }),
        "{AWARE_036}: second session matched by fingerprint: {r2:?}"
    );
    let id = r1.emitter_id;
    assert_eq!(r2.emitter_id, id);
    for f in &fragments {
        assert_ne!(f.emitter_id, id, "{AWARE_036}: fragment kept apart: {f:?}");
    }

    let (band1, band2) = (sessions[0].2, sessions[1].2);
    let page = repo
        .query_inventory(&InventoryQuery {
            freq: Some(FreqRange::new(band1.0.min(band2.0), band1.1.max(band2.1))),
            ..Default::default()
        })
        .unwrap();
    let trains_in_band: Vec<_> = page
        .entries
        .iter()
        .filter(|e| e.emitter.count > 2)
        .collect();
    assert_eq!(trains_in_band.len(), 1, "{AWARE_036}: one Emitter");
    let e = &trains_in_band[0].emitter;
    assert_eq!(e.id, id);
    let (t1, t2) = ([t1], [t2]);
    let ev1 = &sessions[0].0;
    assert_eq!(e.count, t1[0].burst_count + t2[0].burst_count);
    assert_eq!(
        e.seen(),
        TimeRange::new(t1[0].track.time.start, t2[0].track.time.end)
    );
    assert!(e.seen().duration_ns() > DAY_NS / 2);
    assert_eq!(e.known_status, KnownStatus::Unknown);
    assert_eq!(
        repo.known_status_history(id).unwrap()[0].author,
        StatusAuthor::Clusterer
    );

    // Replaying a session's tracker output counts nothing twice.
    for ev in ev1 {
        if let Some(r) = record_track_event(&mut repo, ev, None).unwrap() {
            assert_eq!((r.assignment, r.count_added), (Assignment::Replay, 0));
        }
    }
    assert_eq!(repo.emitter(id).unwrap().count, e.count);

    // T-013 ground truth (CRC-valid inferred framing) appends `known`.
    repo.append_known_status(&KnownStatusChange {
        emitter_id: id,
        status: KnownStatus::Known,
        prior_ref: None,
        reason: "CRC-valid inferred framing (T-013)".into(),
        t: t2[0].track.time.end,
        author: StatusAuthor::Decoder,
    })
    .unwrap();
    let known = repo
        .query_inventory(&InventoryQuery {
            status: vec![KnownStatus::Known],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(known.entries.len(), 1);
    assert_eq!(known.entries[0].emitter.id, id);
}
