//! T-469: the catalogue lists what is on disk, and says so when the disk disagrees with the row.

use std::fs;

use hk_model::provenance::{ClockSource, Tune};
use hk_model::recording::RecordingTrigger;
use hk_model::time::TimestampMethod;
use hk_model::{
    BiasTee, ContentClass, ProvenanceId, RecordingId, RetentionClass, TimeRange, Timestamp,
};

use super::*;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hk-store-recordings-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(d.join("recordings")).unwrap();
    d
}

fn t(ns: i64) -> Timestamp {
    Timestamp::from_unix_nanos(ns)
}

fn prov(repo: &mut Repository, center_hz: f64, fs_hz: f64) -> ProvenanceId {
    repo.intern_provenance(&Provenance {
        device_id: "hackrf:deadbeef".into(),
        tune: Tune {
            center_hz,
            sample_rate_hz: fs_hz,
            lna_db: 16.0,
            vga_db: 20.0,
            amp_on: false,
            bandwidth_hz: 0.75 * fs_hz,
        },
        overload: false,
        quantisation_limited: false,
        noise_sigma_lsb: None,
        temperature_c: None,
        antenna_port: Some("ANT".into()),
        bias_tee: BiasTee::Off,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: None,
        capture_artefacts: Vec::new(),
    })
    .unwrap()
}

/// What a fixture puts on disk: the `size_bytes` the row records, the bytes actually written
/// (`None` = no data file), and whether the `.sigmf-meta` sidecar is written.
#[derive(Clone, Copy)]
struct OnDisk(u64, Option<u64>, bool);

/// A whole recording: row and files agree.
const WHOLE: OnDisk = OnDisk(4800, Some(4800), true);

/// Inserts a row and writes the files `disk` describes.
fn record(
    repo: &mut Repository,
    dir: &Path,
    (t0_ns, t1_ns): (i64, i64),
    kind: RecordingKind,
    disk: OnDisk,
) -> RecordingId {
    let OnDisk(size_bytes, on_disk, meta) = disk;
    let id = RecordingId::new();
    let provenance_ref = prov(repo, 101.3e6, 2.4e6);
    let (meta_uri, data_uri) = (
        format!("recordings/{id}.sigmf-meta"),
        format!("recordings/{id}.sigmf-data"),
    );
    if let Some(n) = on_disk {
        fs::write(dir.join(&data_uri), vec![0u8; n as usize]).unwrap();
    }
    if meta {
        fs::write(dir.join(&meta_uri), b"{}").unwrap();
    }
    repo.insert_recording(&Recording {
        id,
        meta_uri,
        data_uri,
        kind,
        time: TimeRange::new(t(t0_ns), t(t1_ns)),
        f_center_hz: 101.3e6,
        sample_rate_hz: 2.4e6,
        trigger: RecordingTrigger::Manual,
        pre_trigger_s: 0.0,
        post_trigger_s: (t1_ns - t0_ns) as f64 / 1e9,
        size_bytes,
        retention_class: RetentionClass::Pinned,
        content_class: ContentClass::Unrestricted,
        provenance_ref,
    })
    .unwrap();
    id
}

fn q(limit: usize) -> RecordingsQuery {
    RecordingsQuery {
        limit,
        ..RecordingsQuery::default()
    }
}

/// A whole recording on disk is complete, carries its provenance, and is the only kind of entry
/// that contributes an available span.
#[test]
fn a_recording_whose_file_matches_the_row_is_complete_and_extends_the_horizon() {
    let dir = tmp("complete");
    let mut repo = Repository::open_in_memory().unwrap();
    let id = record(
        &mut repo,
        &dir,
        (1_000_000_000, 3_000_000_000),
        RecordingKind::IqSnippet,
        WHOLE,
    );

    let cat = catalogue(&repo, &dir, &q(10)).unwrap();
    assert_eq!((cat.matched, cat.omitted), (1, 0));
    let e = &cat.entries[0];
    assert_eq!(e.availability, Availability::Complete);
    assert!(e.availability.available() && e.is_iq() && e.meta_present);
    assert_eq!(e.bytes_on_disk, Some(4800));
    assert_eq!(e.detail, None);
    assert_eq!(e.recording.id, id);
    // Provenance joined from the row: the device that captured it, not just the tuning.
    let p = e.provenance.as_ref().expect("provenance");
    assert_eq!(p.device_id, "hackrf:deadbeef");
    assert_eq!(p.bias_tee, BiasTee::Off);
    assert_eq!(p.tune.bandwidth_hz, 1.8e6);
    // centre ± rate/2, the same window convention the ring's segments use.
    assert_eq!(e.window_hz(), (101.3e6 - 1.2e6, 101.3e6 + 1.2e6));
    assert_eq!(
        cat.spans,
        vec![AvailableSpan {
            t0_ns: 1_000_000_000,
            t1_ns: 3_000_000_000,
            recording: id,
        }]
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The three ways disk and row disagree. None of them is available, and none of them contributes
/// a span — a half-written recording promising audio it cannot deliver is the defect this guards.
#[test]
fn short_long_missing_and_meta_less_recordings_are_never_listed_as_available() {
    let dir = tmp("partial");
    let mut repo = Repository::open_in_memory().unwrap();
    let short = record(
        &mut repo,
        &dir,
        (1_000_000_000, 2_000_000_000),
        RecordingKind::IqSnippet,
        OnDisk(4800, Some(1200), true),
    );
    let long = record(
        &mut repo,
        &dir,
        (2_000_000_000, 3_000_000_000),
        RecordingKind::IqSnippet,
        OnDisk(4800, Some(9600), true),
    );
    let gone = record(
        &mut repo,
        &dir,
        (3_000_000_000, 4_000_000_000),
        RecordingKind::IqSnippet,
        OnDisk(4800, None, true),
    );
    let no_meta = record(
        &mut repo,
        &dir,
        (4_000_000_000, 5_000_000_000),
        RecordingKind::IqSnippet,
        OnDisk(4800, Some(4800), false),
    );

    let cat = catalogue(&repo, &dir, &q(10)).unwrap();
    assert_eq!(cat.matched, 4);
    assert!(cat.spans.is_empty(), "{:?}", cat.spans);
    let by_id = |id: RecordingId| {
        cat.entries
            .iter()
            .find(|e| e.recording.id == id)
            .expect("listed")
    };

    let e = by_id(short);
    assert_eq!(e.availability, Availability::Partial);
    assert_eq!(e.bytes_on_disk, Some(1200));
    assert!(
        e.detail.as_deref().unwrap().contains("1200 of the 4800"),
        "{:?}",
        e.detail
    );

    let e = by_id(long);
    assert_eq!(e.availability, Availability::Partial);
    assert_eq!(e.bytes_on_disk, Some(9600));

    let e = by_id(gone);
    assert_eq!(e.availability, Availability::Missing);
    assert_eq!(e.bytes_on_disk, None);
    assert!(e.meta_present);

    // The samples are all there but nothing says how to read them as SigMF.
    let e = by_id(no_meta);
    assert_eq!(e.availability, Availability::Partial);
    assert_eq!(e.bytes_on_disk, Some(4800));
    assert!(!e.meta_present);
    assert!(!e.availability.available());
    let _ = fs::remove_dir_all(&dir);
}

/// Audio recordings are catalogued but never counted as IQ: they cannot be re-demodulated, so
/// they do not extend the demod/decode horizon.
#[test]
fn audio_recordings_are_listed_but_carry_no_iq_span() {
    let dir = tmp("audio");
    let mut repo = Repository::open_in_memory().unwrap();
    record(
        &mut repo,
        &dir,
        (1_000_000_000, 2_000_000_000),
        RecordingKind::Audio,
        WHOLE,
    );
    let iq = record(
        &mut repo,
        &dir,
        (2_000_000_000, 3_000_000_000),
        RecordingKind::ChannelDecimated,
        WHOLE,
    );

    let cat = catalogue(&repo, &dir, &q(10)).unwrap();
    assert_eq!(cat.entries.len(), 2);
    assert_eq!(cat.spans.len(), 1);
    assert_eq!(cat.spans[0].recording, iq);

    // A kind filter narrows the query itself, so `matched` narrows with it.
    let only_audio = catalogue(
        &repo,
        &dir,
        &RecordingsQuery {
            kind: Some(RecordingKind::Audio),
            ..q(10)
        },
    )
    .unwrap();
    assert_eq!(only_audio.matched, 1);
    assert!(only_audio.spans.is_empty());
    let _ = fs::remove_dir_all(&dir);
}

/// The window is half-open and overlap-based, and a page that omits rows says how many.
#[test]
fn the_window_selects_overlaps_and_a_short_page_reports_what_it_omitted() {
    let dir = tmp("window");
    let mut repo = Repository::open_in_memory().unwrap();
    for k in 0..5i64 {
        record(
            &mut repo,
            &dir,
            (k * 1_000_000_000, (k + 1) * 1_000_000_000),
            RecordingKind::IqSnippet,
            WHOLE,
        );
    }

    // [2 s, 3 s): the recording ENDING at 2 s does not overlap; the one starting at 2 s does.
    let cat = catalogue(
        &repo,
        &dir,
        &RecordingsQuery {
            t0_ns: Some(2_000_000_000),
            t1_ns: Some(3_000_000_000),
            ..q(10)
        },
    )
    .unwrap();
    assert_eq!(cat.matched, 1);
    assert_eq!(
        cat.entries[0].recording.time.start.as_unix_nanos(),
        2_000_000_000
    );

    // Newest first, and the page's own spans only.
    let page = catalogue(&repo, &dir, &q(2)).unwrap();
    assert_eq!((page.matched, page.omitted), (5, 3));
    assert_eq!(page.entries.len(), 2);
    assert_eq!(
        page.entries[0].recording.time.start.as_unix_nanos(),
        4_000_000_000
    );
    assert_eq!(
        page.entries[1].recording.time.start.as_unix_nanos(),
        3_000_000_000
    );
    // Spans are oldest first, and cover exactly what was listed.
    assert_eq!(page.spans.len(), 2);
    assert_eq!(page.spans[0].t0_ns, 3_000_000_000);
    assert_eq!(page.spans[1].t0_ns, 4_000_000_000);
    let _ = fs::remove_dir_all(&dir);
}

/// A `*_uri` that climbs out of the data directory is refused, not followed: the catalogue never
/// stats a path the writer could not have produced.
#[test]
fn a_uri_outside_the_data_directory_is_refused_rather_than_followed() {
    let dir = tmp("escape");
    let mut repo = Repository::open_in_memory().unwrap();
    let id = RecordingId::new();
    let provenance_ref = prov(&mut repo, 101.3e6, 2.4e6);
    repo.insert_recording(&Recording {
        id,
        meta_uri: "../elsewhere.sigmf-meta".into(),
        data_uri: "../elsewhere.sigmf-data".into(),
        kind: RecordingKind::IqSnippet,
        time: TimeRange::new(t(0), t(1_000_000_000)),
        f_center_hz: 101.3e6,
        sample_rate_hz: 2.4e6,
        trigger: RecordingTrigger::Manual,
        pre_trigger_s: 0.0,
        post_trigger_s: 1.0,
        size_bytes: 4800,
        retention_class: RetentionClass::Pinned,
        content_class: ContentClass::Unrestricted,
        provenance_ref,
    })
    .unwrap();
    // The escaping file exists, and is still not read.
    fs::write(dir.join("elsewhere.sigmf-data"), vec![0u8; 4800]).unwrap();

    let cat = catalogue(&repo, &dir, &q(10)).unwrap();
    assert_eq!(cat.entries[0].availability, Availability::Missing);
    assert_eq!(cat.entries[0].bytes_on_disk, None);
    assert!(cat.spans.is_empty());
    let _ = fs::remove_dir_all(&dir);
}

/// An empty catalogue is an empty list, not an error and not a guess.
#[test]
fn no_recordings_is_an_empty_catalogue() {
    let dir = tmp("empty");
    let repo = Repository::open_in_memory().unwrap();
    let cat = catalogue(&repo, &dir, &q(10)).unwrap();
    assert_eq!((cat.matched, cat.omitted), (0, 0));
    assert!(cat.entries.is_empty() && cat.spans.is_empty());
    let _ = fs::remove_dir_all(&dir);
}
