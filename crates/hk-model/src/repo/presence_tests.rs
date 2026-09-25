//! T-262 (ADR-0017 stage TM-5): presence intervals close after an idle gap, and a returning
//! signal revives on the **same** emitter instead of minting a duplicate row.
//!
//! The headline case is the user's own, measured by T-250 on their staging database: the reported
//! "Confirmed duplicates" at 99.8148 / 99.8151 MHz are **one** FM station that stopped and came
//! back, whose second appearance became a second inventory row.

use super::Repository;
use crate::cluster::*;
use crate::*;

fn t(sec: f64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + (sec * 1e9) as i64)
}

fn tr(a: f64, b: f64) -> TimeRange {
    TimeRange::new(t(a), t(b))
}

fn repo() -> Repository {
    Repository::open_in_memory().unwrap()
}

/// A broadcast-FM fingerprint: a family, and the median burst length that is a statistic of the
/// window the producer watched, not of the emission.
fn fm_fp(f: f64, bw: f64, burst_s: f64) -> Fingerprint {
    Fingerprint {
        family: Some("wfm".into()),
        burst_length_s: Some(burst_s),
        ..Fingerprint::new(f, bw)
    }
}

/// A stored closed track and the sighting offered for it.
fn track_sighting(repo: &mut Repository, fp: Fingerprint, seen: TimeRange, count: u64) -> Sighting {
    let track = Track {
        id: TrackId::new(),
        state: TrackState::Closed,
        split_from: None,
        time: seen,
        f_center_hz: fp.f_center_hz,
        bandwidth_hz: fp.bandwidth_hz,
        detection_count: count,
        timing: TimingFeatures::default(),
        updated_at: seen.end,
    };
    repo.upsert_track(&track).unwrap();
    let mut s = Sighting::track(&track, fp);
    s.count = count;
    s
}

fn listed(repo: &Repository) -> usize {
    repo.query_inventory(&InventoryQuery::default())
        .unwrap()
        .entries
        .len()
}

/// **The user's duplicate pair.** Two appearances of one FM station, 305 s and 59 s long with a
/// 72 s silence between them, at 99.8148 and 99.8151 MHz (492 Hz apart, band overlap 100 %). The
/// only thing separating them was a median burst length of 0.68 s versus 0.37 s — a statistic of
/// how long each was watched (19 observations against 6).
///
/// One emitter, two presence intervals, two source rows. Not two rows.
#[test]
fn the_station_that_stopped_and_came_back_revives_on_one_emitter() {
    let mut r = repo();
    let first = fm_fp(99.8148e6, 377.5e3, 0.68);
    let second = fm_fp(99.8151e6, 377.6e3, 0.37);

    // The mechanism, in one assertion: compared as if watched over the same window these two are
    // a different emitter; compared across the silence they are the same one.
    let tol = Tolerances::default();
    let strict = first.compare(&second, &tol);
    assert!(!strict.within, "{strict:?}");
    assert_eq!(strict.worst, Some("burst_length"));
    assert!(strict.worst_error > 1.8, "{strict:?}");
    assert!(first.compare_across_silence(&second, &tol).within);

    let a = track_sighting(&mut r, first, tr(94.0, 399.0), 19);
    let a = r.record_sighting(&a, None).unwrap();
    assert!(a.created);

    let b = track_sighting(&mut r, second, tr(471.0, 530.0), 6);
    let b = r.record_sighting(&b, None).unwrap();
    assert_eq!(
        b.emitter_id, a.emitter_id,
        "the returning station revives its own emitter"
    );
    assert!(!b.created, "and does not mint a second row");
    assert_eq!(
        listed(&r),
        1,
        "one inventory row, not the reported duplicate pair"
    );

    // Two source rows on that one emitter — revival appends, it never overwrites.
    assert_eq!(r.observation_spans(a.emitter_id).unwrap().len(), 2);

    // Even at the most conservative idle gap the 72 s silence closes the first interval.
    let gap = IdleGap::conservative();
    let iv = r
        .presence_intervals(a.emitter_id, gap, t(530.0), &Watched::unrecorded())
        .unwrap();
    assert_eq!(iv.len(), 2, "{iv:?}");
    assert_eq!(iv[0].time, tr(94.0, 399.0));
    assert_eq!(iv[1].time, tr(471.0, 530.0));
    assert_eq!(iv[0].duration_s(), 305.0);
    assert_eq!(iv[1].duration_s(), 59.0);
    let silence = iv[1].time.start.as_unix_nanos() - iv[0].time.end.as_unix_nanos();
    assert_eq!(silence, 72_000_000_000, "72 s > the 60 s gap");
    assert!(
        !iv[0].open,
        "the first interval closed when evidence stopped"
    );
    assert!(iv[1].open, "the second is open at the live edge");

    // And the hull is exactly the thing that must never be shown as a duration: 436 s of hull
    // over 364 s actually on air.
    let e = r.emitter(a.emitter_id).unwrap();
    assert_eq!(e.seen(), tr(94.0, 530.0));
    let on_air: f64 = iv.iter().map(PresenceInterval::duration_s).sum();
    assert_eq!(on_air, 364.0);
    assert_eq!(e.seen().duration_ns(), 436_000_000_000);
}

/// Liveness through a view window: live at the edge, then `ended` with the moment it stopped,
/// then `absent` once the window has moved past it entirely.
#[test]
fn liveness_reads_live_then_ended_then_absent() {
    let mut r = repo();
    let s = track_sighting(&mut r, fm_fp(99.8148e6, 377.5e3, 0.68), tr(94.0, 399.0), 19);
    let id = r.record_sighting(&s, None).unwrap().emitter_id;
    let gap = IdleGap::conservative();

    let live = r.presence(id, tr(0.0, 399.0), gap, t(399.0)).unwrap();
    assert_eq!(live.liveness, Liveness::Live);
    assert_eq!(live.ended_t, None);
    assert_eq!(live.on_air_s, 305.0);
    assert_eq!(live.intervals, 1);

    // Four minutes later: the interval is in the window but closed. "Ended 4 minutes ago" is the
    // sentence the product previously could not say.
    let ended = r.presence(id, tr(0.0, 639.0), gap, t(639.0)).unwrap();
    assert_eq!(ended.liveness, Liveness::Ended);
    assert_eq!(ended.ended_t, Some(t(399.0)));

    // A window past it entirely: absent, and nothing was deleted to make that true.
    let absent = r.presence(id, tr(500.0, 639.0), gap, t(639.0)).unwrap();
    assert_eq!(absent.liveness, Liveness::Absent);
    assert_eq!(absent.intervals, 0);
    assert_eq!(absent.on_air_s, 0.0);
    assert_eq!(r.observation_spans(id).unwrap().len(), 1, "still on record");
}

/// The relaxation is narrow: it drops only the three window statistics. Anything that describes
/// the emission still separates two emitters across a silence.
#[test]
fn two_emissions_that_differ_beyond_the_window_statistics_stay_two_emitters() {
    let mut r = repo();
    let mut first = fm_fp(915.0e6, 36e3, 0.68);
    first.symbol_rate_hz = Some(4800.0);
    let mut second = fm_fp(915.0e6, 36e3, 0.37);
    second.symbol_rate_hz = Some(9600.0);

    let a = track_sighting(&mut r, first, tr(0.0, 100.0), 19);
    let a = r.record_sighting(&a, None).unwrap();
    let b = track_sighting(&mut r, second, tr(500.0, 600.0), 6);
    let b = r.record_sighting(&b, None).unwrap();

    assert_ne!(
        b.emitter_id, a.emitter_id,
        "a different symbol rate is a different emitter"
    );
    assert!(b.created);
    assert_eq!(listed(&r), 2);
}

/// While the intervals **do** overlap, the window statistics are comparable and still separate
/// two emissions sharing a channel — the ISM case (T-254). Nothing was widened.
#[test]
fn while_the_intervals_overlap_the_window_statistics_still_separate_two_emissions() {
    let mut r = repo();
    let a = track_sighting(&mut r, fm_fp(915.0e6, 36e3, 0.68), tr(0.0, 100.0), 19);
    let a = r.record_sighting(&a, None).unwrap();
    // Same channel, same bandwidth, same family, overlapping minutes, different burst length.
    let b = track_sighting(&mut r, fm_fp(915.0e6, 36e3, 0.37), tr(50.0, 150.0), 6);
    let b = r.record_sighting(&b, None).unwrap();

    assert_ne!(b.emitter_id, a.emitter_id);
    assert!(b.created);
    assert_eq!(listed(&r), 2);
}

/// Overlapping source rows of one emitter normalise into a single disjoint interval (docs/07
/// §2.11's "overlapping observations not counted twice", given a name).
#[test]
fn overlapping_source_rows_normalise_into_one_interval() {
    let mut r = repo();
    let fp = fm_fp(101.3e6, 200e3, 0.5);
    let a = track_sighting(&mut r, fp.clone(), tr(0.0, 60.0), 10);
    let id = r.record_sighting(&a, None).unwrap().emitter_id;
    // A second source (another track of the same emission) covering overlapping minutes.
    let b = track_sighting(&mut r, fp, tr(30.0, 90.0), 10);
    let b = r.record_sighting(&b, None).unwrap();
    assert_eq!(b.emitter_id, id);

    assert_eq!(r.observation_spans(id).unwrap().len(), 2, "two source rows");
    let iv = r
        .presence_intervals(
            id,
            IdleGap::from_revisit_s(1.0),
            t(90.0),
            &Watched::unrecorded(),
        )
        .unwrap();
    assert_eq!(iv.len(), 1, "one interval: {iv:?}");
    assert_eq!(iv[0].time, tr(0.0, 90.0));
    assert_eq!(iv[0].sources, 2);
    assert_eq!(
        iv[0].duration_s(),
        90.0,
        "not 120 s - the overlap is not counted twice"
    );
}

/// Nothing in the presence derivation reads the sighting count (ADR-0017 §5). Two emitters with
/// identical intervals and wildly different lifetime counts are identically live and identically
/// ranked; the one with 582,500 sightings and no recent interval is not live at all.
#[test]
fn liveness_and_ranking_never_read_the_sighting_count() {
    let mut r = repo();
    let quiet = track_sighting(&mut r, fm_fp(88.1e6, 200e3, 0.5), tr(0.0, 60.0), 38);
    let quiet = r.record_sighting(&quiet, None).unwrap().emitter_id;
    let chatty = track_sighting(&mut r, fm_fp(97.7e6, 200e3, 0.5), tr(0.0, 60.0), 582_500);
    let chatty = r.record_sighting(&chatty, None).unwrap().emitter_id;

    assert_eq!(r.emitter(quiet).unwrap().count, 38);
    assert_eq!(r.emitter(chatty).unwrap().count, 582_500);

    let gap = IdleGap::conservative();
    let window = tr(0.0, 60.0);
    let a = r.presence(quiet, window, gap, t(60.0)).unwrap();
    let b = r.presence(chatty, window, gap, t(60.0)).unwrap();
    assert_eq!(a.liveness, b.liveness);
    assert_eq!(a.on_air_s, b.on_air_s);
    assert_eq!(a.intervals, b.intervals);

    // Ranking input is in-window on-air time, so a huge lifetime count cannot outrank a signal
    // that is actually transmitting.
    let later = tr(600.0, 660.0);
    let stale = r.presence(chatty, later, gap, t(660.0)).unwrap();
    assert_eq!(stale.liveness, Liveness::Absent);
    assert_eq!(stale.on_air_s, 0.0);
}

/// T-251 (ADR-0017 TM-6), through the repository: the user's own stopped-and-returned FM station
/// reads `ended` with a decayed confidence while it is silent, and is fully confident again once
/// it returns — with **both** presence intervals still on record. Nothing was deleted to make
/// either reading true, and no decay state was stored between the two reads.
#[test]
fn a_stopped_candidate_decays_and_a_returning_one_is_confident_again() {
    let mut r = repo();
    let first = track_sighting(&mut r, fm_fp(99.8148e6, 377.5e3, 0.68), tr(94.0, 399.0), 19);
    let id = r.record_sighting(&first, None).unwrap().emitter_id;
    let gap = IdleGap::conservative(); // 60 s: the API's own reading

    // On the air: confident by definition, and silence is zero.
    let live = r.presence(id, tr(0.0, 399.0), gap, t(399.0)).unwrap();
    assert_eq!(live.liveness, Liveness::Live);
    assert_eq!(live.confidence, 1.0);
    assert_eq!(live.silence_s, Some(0.0));

    // Silent inside the window: still listed, still carrying its interval, ranked lower.
    let ended = r.presence(id, tr(0.0, 639.0), gap, t(639.0)).unwrap();
    assert_eq!(ended.liveness, Liveness::Ended);
    assert_eq!(ended.silence_s, Some(240.0));
    assert!(ended.confidence < live.confidence, "{ended:?}");
    assert!(
        (ended.confidence - confidence_after_silence(240.0, gap)).abs() < 1e-12,
        "the row decays by the stated law and nothing else: {ended:?}"
    );
    assert_eq!(ended.intervals, 1, "not expired - it happened");

    // The station returns (T-262: same emitter, new interval). Confidence recovers with no
    // revival step, because it was never stored.
    let second = track_sighting(&mut r, fm_fp(99.8151e6, 377.6e3, 0.37), tr(471.0, 530.0), 6);
    let second = r.record_sighting(&second, None).unwrap();
    assert_eq!(second.emitter_id, id, "revived, not duplicated");
    let back = r.presence(id, tr(0.0, 530.0), gap, t(530.0)).unwrap();
    assert_eq!(back.liveness, Liveness::Live);
    assert_eq!(back.confidence, 1.0);
    assert_eq!(back.intervals, 2, "both events kept");
    assert_eq!(listed(&r), 1);

    // Decay touched nothing on disk: the observation ledger still holds both source rows, and the
    // whole reading re-derives from them under a different gap.
    assert_eq!(r.observation_spans(id).unwrap().len(), 2);
}

/// Two rows with the same in-window on-air time, one live and one that stopped inside the window:
/// `on_air_s` ranks them equal, confidence does not. This is the only case decay owns.
#[test]
fn confidence_separates_two_rows_that_on_air_time_ranks_equal() {
    let mut r = repo();
    let stopped = track_sighting(&mut r, fm_fp(88.1e6, 200e3, 0.5), tr(0.0, 100.0), 10);
    let stopped = r.record_sighting(&stopped, None).unwrap().emitter_id;
    let live = track_sighting(&mut r, fm_fp(97.7e6, 200e3, 0.5), tr(300.0, 400.0), 10);
    let live = r.record_sighting(&live, None).unwrap().emitter_id;

    let gap = IdleGap::conservative();
    let window = tr(0.0, 400.0);
    let a = r.presence(stopped, window, gap, t(400.0)).unwrap();
    let b = r.presence(live, window, gap, t(400.0)).unwrap();

    assert_eq!(a.on_air_s, b.on_air_s, "100 s each");
    assert_eq!((a.liveness, b.liveness), (Liveness::Ended, Liveness::Live));
    assert!(a.confidence < b.confidence, "{a:?} vs {b:?}");
    // Both are still listed; decay is a rank, not a filter.
    assert_eq!(listed(&r), 2);
}

/// Migration 0012 adds an index and nothing else: `emitter_observation`'s columns are exactly
/// migration 0001's, and no `closed_at` / `close_reason` / decay column appeared.
#[test]
fn migration_0012_is_index_only() {
    let r = repo();
    let indexes: Vec<String> = r
        .conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'emitter_observation' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        indexes.iter().any(|n| n == "idx_emitter_observation_time"),
        "{indexes:?}"
    );

    let columns: Vec<String> = r
        .conn
        .prepare("SELECT name FROM pragma_table_info('emitter_observation')")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        columns,
        [
            "source_kind",
            "source_id",
            "emitter_id",
            "count",
            "t_start",
            "t_end",
            "measurement",
            "f_center",
            // T-940 (migration 0021): the silence an open track's tracker has OBSERVED since the
            // row's measured end — a measurement the fold compares against the idle gap, not a
            // closure decision. No `closed_at` / `close_reason` / decay column exists.
            "live_silence_ns",
        ],
        "0012 is index-only: closure stays derived, never stored"
    );
}

/// T-940: the pipeline's live-follow report keeps an on-air emitter `live` between sightings, and
/// ending the follow hands the row back to the closed-source reading — through the repository,
/// so the column, the upsert and the fold are one path.
#[test]
fn a_followed_track_reads_live_between_reports_and_closes_on_its_own_silence() {
    let mut r = repo();
    let s = track_sighting(&mut r, fm_fp(101.3e6, 180e3, 1.0), tr(0.0, 10.0), 10);
    let id = r.record_sighting(&s, None).unwrap().emitter_id;
    let track = match s.source {
        LinkTarget::Track(t) => t,
        ref other => panic!("a track sighting names a track: {other:?}"),
    };
    let gap = IdleGap::continuous();
    let read = |r: &Repository, now: f64, w: &Watched| {
        let iv = r.presence_intervals(id, gap, t(now), w).unwrap();
        presence_in_window(&iv, tr(0.0, now), gap).liveness
    };
    // The wall clock alone ends it 4 s after the sighting's row was written: the defect.
    assert_eq!(read(&r, 14.0, &Watched::unrecorded()), Liveness::Ended);

    // The pipeline follows the track: measured to 12 s, no silence observed.
    r.follow_track(id, track, tr(0.0, 12.0), 0).unwrap();
    assert_eq!(read(&r, 16.0, &Watched::unrecorded()), Liveness::Live);
    let spans = r.observation_spans(id).unwrap();
    assert_eq!(
        spans.len(),
        1,
        "the report is the track's own row: {spans:?}"
    );
    assert_eq!(spans[0].time, tr(0.0, 12.0));
    assert_eq!(spans[0].count, 10, "a report counts no sighting");

    // The tracker observes 2 s of silence: ended at the measured end, not at the report.
    r.follow_track(id, track, tr(0.0, 12.0), 2_000_000_000)
        .unwrap();
    let iv = r
        .presence_intervals(id, gap, t(16.0), &Watched::unrecorded())
        .unwrap();
    assert!(!iv[0].open);
    assert_eq!(iv[0].time.end, t(12.0));

    // Stopping the follow returns the row to the closed reading: watched coverage decides.
    r.follow_track(id, track, tr(0.0, 12.0), 0).unwrap();
    r.stop_following_track(track).unwrap();
    let tuned_away = Watched::recorded(t(0.0), &[tr(0.0, 12.5)]);
    assert_eq!(
        read(&r, 300.0, &tuned_away),
        Liveness::Live,
        "unobserved is not quiet"
    );
    assert_eq!(
        read(&r, 300.0, &Watched::recorded(t(0.0), &[tr(0.0, 300.0)])),
        Liveness::Ended
    );
}

/// T-940: a report for a track with no ledger row yet (its entry came from a chain) files a
/// `track-live` row on the entry it names, and a run start releases every report a killed run left
/// behind.
#[test]
fn a_report_for_an_unrowed_track_files_a_live_row_and_a_run_start_releases_it() {
    let mut r = repo();
    let s = track_sighting(&mut r, fm_fp(101.3e6, 180e3, 1.0), tr(0.0, 10.0), 10);
    let id = r.record_sighting(&s, None).unwrap().emitter_id;
    let other = TrackId::new();
    r.follow_track(id, other, tr(5.0, 20.0), 0).unwrap();
    let spans = r.observation_spans(id).unwrap();
    assert_eq!(spans.len(), 2, "{spans:?}");
    assert!(
        spans
            .iter()
            .any(|s| s.time == tr(5.0, 20.0) && s.count == 0)
    );
    // Filed as a `track-live` row, never a `track` row: the track's own sightings must still
    // resolve by clustering (with the same-emission discount), not replay onto this entry.
    let kind: String = r
        .conn
        .query_row(
            "SELECT source_kind FROM emitter_observation WHERE t_start = ?1",
            [tr(5.0, 20.0).start.as_unix_nanos()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(kind, super::presence::TRACK_LIVE_SOURCE);
    // And recurrence does not count it as an appearance.
    assert_eq!(r.emitter_recurrence(id, 5).unwrap().appearances, 1);
    assert_eq!(r.stop_following_all().unwrap(), 1);
    assert!(
        r.observation_spans(id)
            .unwrap()
            .iter()
            .all(|s| s.live_silence_ns.is_none())
    );
}
