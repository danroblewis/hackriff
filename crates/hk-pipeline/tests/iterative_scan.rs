//! T-406 / `docs/16` §7 step 6: the iterative scan's records reach the rasteriser with **the
//! dwell's own shape**, and the three coverage states stay apart in the accumulation.
//!
//! This is the crux the ticket names, and it is one chain rather than three assertions in three
//! crates: **scheduler → observation recorder → observation log → coverage spans → coverage grid**.
//! Each link is the production one; nothing here hand-writes a record or a span.
//!
//! What it proves, in order:
//!
//! 1. An iterative scan emits **region-dwell** steps, so the recorder writes **one dwell record per
//!    step** with that step's own analysed band and its own settled interval — and **no sweep
//!    record at all**. A sweep record aggregates a whole pass, and rasterising one would claim *the
//!    whole band, the whole time*.
//! 2. Rasterised on a time–frequency grid, those records draw a **diagonal**: at the instant the
//!    tune was on hop *k*, hop *k*'s band is `Observed` and the other hops' bands are `Unobserved`.
//!    That is "observed and quiet" (a finding) kept apart from "the sweep has not reached here"
//!    (no claim), per cell, which is the honesty rule the whole feature rests on.
//! 3. `Observed` carries a **positive sampling fact and no claim about energy**: a step that heard
//!    nothing is still observed. Coverage never says "quiet"; the level is a separate measurement.
//! 4. Past the record horizon the answer is the **fourth state** — rows with no surviving record are
//!    `unknown` (*we no longer know whether we looked*), not grey.
//! 5. The DC notch stays honest: a notched dwell contributes two bands, not one wide one.

use hk_core::SourceCapabilities;
use hk_core::scheduler::observe::{ObservationRecorder, StepObservation, WindowRule};
use hk_core::scheduler::{
    IterativeScan, Purpose, ScanBudget, ScheduleStep, Scheduler, SchedulerConfig, SyntheticClock,
    is_scan_step,
};
use hk_model::attention::observation::{ObservationRecord, Reason, Tier};
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_store::coverage::{Coverage, Device, grid_over};
use hk_store::observation::{
    DEFAULT_MAX_AGE_NS, DEFAULT_MAX_BYTES, MAX_RECORD_LIMIT, ObservationLogConfig,
    ObservationStore, RecordQuery,
};

/// 2026-09-13T00:00:00Z.
const T0_NS: i64 = 1_789_257_600_000_000_000;
/// The front end the records name (T-378): a real `DeviceInfo::device_id` spelling.
const DEVICE: &str = "hackrf:0000000000000000a06063c8234e925f";
/// A dwell inside the 10–30 s range the user asked for.
const DWELL_S: f64 = 10.0;
/// The band the scan walks: three 15 MHz windows at 20 Msps.
const LO_HZ: f64 = 100e6;
const HI_HZ: f64 = 145e6;

const RULE: WindowRule = WindowRule {
    fft_bins: 4096,
    dc_half_hz: 15e3,
    enbw_bins: 1.5,
};

fn applied(step: &ScheduleStep) -> StepObservation {
    StepObservation {
        step: *step,
        center_hz: step.center_hz,
        rate_hz: step.rate_hz,
        // Settled at the start: a retune settle gap would only shorten `observed`, and this test is
        // about the shape of the record, not the length of the gap.
        settled: Some(step.t_start),
        end: step.t_end(),
        dropped_samples: 0,
        overload: false,
    }
}

struct Scan {
    steps: Vec<ScheduleStep>,
    records: Vec<ObservationRecord>,
    hops: usize,
    budget: ScanBudget,
}

/// Runs `passes` whole passes of an iterative scan through the real scheduler and the real
/// recorder, and returns the steps and the records they produced.
fn run_scan(passes: usize) -> Scan {
    let scan = IterativeScan::from_seconds(DWELL_S).unwrap();
    assert!(
        scan.recommended(),
        "the test runs the policy the user named"
    );
    let plan = scan.plan_over(
        "iterative scan",
        FreqRange::new(LO_HZ, HI_HZ),
        Timestamp::from_unix_nanos(T0_NS),
    );
    // The plan carries its own dwell: nothing outside it has to be told the policy.
    let cfg = SchedulerConfig::from_plan(&plan).unwrap();
    assert_eq!(cfg.region_dwell_ns, (DWELL_S * 1e9) as i64);

    let caps = SourceCapabilities::hackrf_one();
    let mut sched = Scheduler::new(
        &plan,
        cfg,
        &caps,
        SyntheticClock::new(Timestamp::from_unix_nanos(T0_NS)),
    )
    .unwrap();
    let hops = sched.plan().hops.len();
    assert!(hops >= 3, "the scan walks {hops} windows; wanted >= 3");
    let budget = ScanBudget::of(sched.plan());

    let mut steps = Vec::new();
    sched.run_synthetic(passes * hops, &mut steps);
    let mut rec = ObservationRecorder::new(sched.plan(), RULE, None, None, Some(DEVICE.into()));
    let mut records = Vec::new();
    for st in &steps {
        rec.begin(st, &mut |r| records.push(r));
        rec.observe(&applied(st), &mut |r| records.push(r));
    }
    rec.flush(&mut |r| records.push(r));
    for r in &records {
        r.validate().unwrap();
    }
    Scan {
        steps,
        records,
        hops,
        budget,
    }
}

/// Writes `records` to a real observation log under `dir` and reads them back through the query
/// the coverage route uses, so the spans come out of the store rather than out of a Vec.
fn through_the_log(
    dir: &std::path::Path,
    records: &[ObservationRecord],
    freq: FreqRange,
    window: TimeRange,
) -> hk_store::RecordSpans {
    let store = ObservationStore::open(ObservationLogConfig::new(dir)).unwrap();
    for r in records {
        store.append(r);
    }
    store.flush();
    let page = store.query(&RecordQuery {
        freq,
        span: window,
        tier: None,
        cursor: 0,
        limit: MAX_RECORD_LIMIT,
    });
    hk_store::spans_from_records(&page.records, &page.geometries, freq)
}

/// **The crux.** One record per step, with its true band and interval — and no sweep record.
#[test]
fn every_iterative_scan_step_writes_its_own_record_with_its_own_band_and_interval() {
    let scan = run_scan(2);
    assert_eq!(scan.steps.len(), 2 * scan.hops);

    // 1. Every step is a region dwell, at the scheduled-plan tier: preemptible by a lease or a
    //    user tune, and never the interactive tier. The scan does not own the radio.
    for st in &scan.steps {
        assert!(
            matches!(st.purpose, Purpose::RegionDwell { .. }),
            "an iterative scan emits region dwells, got {:?}",
            st.purpose
        );
        assert_eq!(st.purpose.reason().tier(), Tier::ScheduledPlan);
    }

    // 2. One dwell record per step, in step order — and NOT ONE SWEEP RECORD. This is the line
    //    `docs/16` §7 step 6 draws: a sweep record spans a pass, and the rasteriser cannot tell a
    //    dwell pattern from a smear once it has one.
    let dwells: Vec<_> = scan
        .records
        .iter()
        .filter_map(|r| match r {
            ObservationRecord::Dwell(d) => Some(d),
            _ => None,
        })
        .collect();
    assert!(
        !scan
            .records
            .iter()
            .any(|r| matches!(r, ObservationRecord::Sweep(_))),
        "an iterative scan writes no aggregated sweep record"
    );
    assert_eq!(
        dwells.len(),
        scan.steps.len(),
        "one record per step, not one per pass"
    );

    // 3. Each record's band and interval are ITS STEP'S, not the pass's.
    let pass = TimeRange::new(
        scan.steps[0].t_start,
        scan.steps[scan.steps.len() - 1].t_end(),
    );
    for (d, st) in dwells.iter().zip(&scan.steps) {
        assert!(is_scan_step(d), "reason {:?} is not a scan step", d.reason);
        assert!(matches!(d.reason, Reason::RegionDwell { .. }));
        assert_eq!(d.seq, st.seq);
        assert_eq!(d.planned, TimeRange::new(st.t_start, st.t_end()));
        assert_eq!(d.observed, TimeRange::new(st.t_start, st.t_end()));
        assert_eq!(d.window, RULE.window(st.center_hz, st.rate_hz));
        // The interval is a step, not a pass: this is the quantity that would be wrong if several
        // steps were folded into one record.
        assert!(
            d.observed.duration_ns() < pass.duration_ns(),
            "a record covering the whole pass would erase the dwell pattern"
        );
        assert_eq!(d.observed.duration_ns(), (DWELL_S * 1e9) as i64);
        assert_eq!(d.device_id.as_deref(), Some(DEVICE));
    }

    // 4. And the steps really do MOVE: consecutive steps in a pass are different windows, which is
    //    what "step the tune to the next region" means.
    let centres: Vec<f64> = scan.steps[..scan.hops]
        .iter()
        .map(|s| s.center_hz)
        .collect();
    for w in centres.windows(2) {
        assert!(
            (w[0] - w[1]).abs() > 1e6,
            "the tune did not step: {centres:?}"
        );
    }

    // 5. The budget is the "what it catches" statement as data, and it is this plan's, measured.
    assert_eq!(scan.budget.steps, scan.hops);
    assert_eq!(
        scan.budget.pass_ns,
        (DWELL_S * 1e9) as i64 * scan.hops as i64
    );
    assert!(
        (scan.budget.duty() - 1.0 / scan.hops as f64).abs() < 1e-12,
        "each band is listened to 1 step in {}",
        scan.hops
    );
}

/// **The honesty rule, per cell.** Rasterised, one pass of the scan is a diagonal: observed where
/// the tune was, unobserved where it had not reached — and the two are different values, not two
/// shades of one.
#[test]
fn the_scan_rasterises_as_a_diagonal_so_cleared_spectrum_is_not_unreached_spectrum() {
    let scan = run_scan(1);
    let dir = tempdir("t406-diagonal");
    let window = TimeRange::new(
        scan.steps[0].t_start,
        scan.steps[scan.steps.len() - 1].t_end(),
    );
    // Probe each window 1 MHz off its own centre: inside its own hop's analysed extent, outside
    // every other hop's (the windows are 20 MHz wide on a 15 MHz raster, so they overlap at the
    // seams and only the cores are exclusive), and clear of the DC notch.
    let probes: Vec<f64> = scan.steps[..scan.hops]
        .iter()
        .map(|s| s.center_hz + 1e6)
        .collect();
    let band = FreqRange::new(
        probes.iter().copied().fold(f64::INFINITY, f64::min) - 12e6,
        probes.iter().copied().fold(f64::NEG_INFINITY, f64::max) + 12e6,
    );

    let read = through_the_log(&dir, &scan.records, band, window);
    assert_eq!(
        read.named,
        read.spans.len(),
        "every span names the front end that looked (T-378)"
    );
    assert!(!read.spans.is_empty());

    // One row per step: the rows partition the window exactly, and the steps are equal length, so
    // row k IS step k.
    let grid = grid_over(
        &read.spans,
        &Device::Id(DEVICE.into()),
        band,
        window,
        scan.hops,
        512,
    );
    assert_eq!(grid.nt, scan.hops);

    for (k, st) in scan.steps[..scan.hops].iter().enumerate() {
        // Mid-step, so a boundary rounding cannot decide the answer.
        let t = Timestamp::from_unix_nanos(st.t_start.as_unix_nanos() + st.duration_ns / 2);
        for (j, f) in probes.iter().enumerate() {
            let cell = grid.at_tf(t, *f).expect("probe inside the grid");
            if j == k {
                let s = cell
                    .sampled()
                    .unwrap_or_else(|| panic!("hop {j} unobserved during its own step {k}"));
                // Observed is a POSITIVE fact about sampling and says nothing about energy: a step
                // that heard nothing is still observed, and that is the finding the feature is for.
                assert!(s.observed_ns > 0);
                assert!(s.duty > 0.99, "the whole row was sampled: duty {}", s.duty);
                assert_eq!(s.spans, 1);
                assert!((s.center_hz - st.center_hz).abs() < 1.0);
            } else {
                assert_eq!(
                    *cell,
                    Coverage::Unobserved,
                    "hop {j} must be unobserved during step {k}: the sweep was elsewhere, and \
                     calling that quiet would invent an absence-of-signal finding"
                );
            }
        }
    }

    // Collapsed on time the answer flips, and that is the point of keeping the time axis: over the
    // whole pass every probe WAS observed. A one-row grid says "this band was sampled somewhere in
    // the window"; the diagonal above says "sampled then".
    let column = grid_over(
        &read.spans,
        &Device::Id(DEVICE.into()),
        band,
        window,
        1,
        512,
    );
    for f in &probes {
        assert!(
            column.at(*f).is_some_and(Coverage::is_observed),
            "the pass did clear {f} Hz, taken over the whole window"
        );
    }
}

/// **The fourth state.** Rows before the oldest surviving record are `unknown` — *we no longer know
/// whether we looked* — and never grey. An observed cell is never re-labelled.
#[test]
fn rows_before_the_record_horizon_are_unknown_and_not_unobserved() {
    let scan = run_scan(1);
    let dir = tempdir("t406-horizon");
    let first = scan.steps[0].t_start;
    let last = scan.steps[scan.steps.len() - 1].t_end();
    // A window reaching back before the scan: nothing looked there, and no record says either way.
    let window = TimeRange::new(
        Timestamp::from_unix_nanos(first.as_unix_nanos() - 60 * 1_000_000_000),
        last,
    );
    let band = FreqRange::new(LO_HZ - 12e6, HI_HZ + 12e6);
    let read = through_the_log(&dir, &scan.records, band, window);
    let grid = grid_over(
        &read.spans,
        &Device::Id(DEVICE.into()),
        band,
        window,
        16,
        256,
    );

    let unknown = grid.unknown_rows_before(first);
    assert!(
        unknown > 0,
        "the rows before the oldest record are beyond the horizon"
    );
    assert!(unknown < grid.nt, "not the whole grid");
    // Those rows hold no observation, and it is the CALLER's horizon — not the fold — that decides
    // they are `unknown`: `Coverage` stays deliberately two-variant (`docs/16` §5.4).
    for t in 0..unknown {
        for f in 0..grid.nf {
            assert_eq!(grid.cell(t, f), Some(&Coverage::Unobserved));
        }
    }
    // And a row inside the scan still holds real observation, so the horizon never swallows a
    // measurement: a surviving measurement is itself proof we looked.
    assert!(grid.observed_cells() > 0);
}

/// The DC notch survives the chain: a notched dwell contributes **two** bands, so the notch stays
/// honestly unobserved instead of being papered over by the window around it.
#[test]
fn a_notched_dwell_reaches_the_rasteriser_as_two_bands() {
    let scan = run_scan(1);
    let dir = tempdir("t406-notch");
    let window = TimeRange::new(
        scan.steps[0].t_start,
        scan.steps[scan.steps.len() - 1].t_end(),
    );
    let band = FreqRange::new(LO_HZ - 12e6, HI_HZ + 12e6);
    let read = through_the_log(&dir, &scan.records, band, window);
    // One record per step, two spans each: below the notch and above it.
    assert_eq!(
        read.spans.len(),
        2 * scan.steps.len(),
        "each notched dwell is two positive claims, never one wide one"
    );
    for s in &read.spans {
        assert!(
            !(s.freq.lo_hz < s.center_hz && s.center_hz < s.freq.hi_hz),
            "a span must not straddle the notched centre: {s:?}"
        );
    }
}

/// `docs/16` §5.4's retention arithmetic, **measured rather than assumed** — it is marked unverified
/// there, and it is what T-406's dwell retention rests on.
///
/// It also states the finding the section did not: **which bound binds depends on the policy.**
#[test]
fn a_dwell_records_line_cost_decides_which_retention_bound_binds() {
    let scan = run_scan(1);
    let line = scan
        .records
        .iter()
        .filter(|r| matches!(r, ObservationRecord::Dwell(_)))
        .map(|r| hk_store::observation::segment::encode_line(r).len())
        .max()
        .expect("the scan wrote dwell records");
    // A realistic line, not a minimal one: survey-less but device-named, with a DC notch.
    assert!(
        (300..=1200).contains(&line),
        "a dwell line measured {line} B, outside the range docs/16 §5.4 assumed"
    );

    // Dwelling: one line per step. At the 10 s floor of the user's range, a day is:
    let lines_per_day = (24.0 * 3600.0 / DWELL_S) as u64;
    let bytes_per_day = lines_per_day * line as u64;
    let days_to_fill = DEFAULT_MAX_BYTES / bytes_per_day;
    let age_days = DEFAULT_MAX_AGE_NS / (24 * 3_600_000_000_000);
    assert!(
        days_to_fill > age_days as u64,
        "under the iterative scan the AGE must bind first ({age_days} d) — the byte quota holds \
         {days_to_fill} d at {bytes_per_day} B/day"
    );
    // And the age is long enough to be worth having: `docs/16` §5.4 wants the coverage record to
    // outlive the pyramid it explains, and 30 days did not.
    assert!(age_days >= 180, "{age_days} days");
    eprintln!(
        "[T-406] dwell line {line} B; at a {DWELL_S} s dwell that is {bytes_per_day} B/day, so the \
         {} GiB quota holds ~{days_to_fill} days and the {age_days}-day age binds first. \
         A 50 ms-hop sweep writes ~1.5k hop visits per aggregated record and is ~2 orders of \
         magnitude denser per day, so there the QUOTA binds — raising the age alone would not have \
         moved that horizon.",
        DEFAULT_MAX_BYTES / (1024 * 1024 * 1024)
    );
}

/// A fresh directory under the system temp dir, removed when the test process ends.
fn tempdir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}
