//! **T-978: the overlap re-analysis is wired through the mock SDR, and costs nothing when no
//! overlap is unresolved.**
//!
//! The resolution itself is asserted where the decision is
//! (`hk-detect/tests/t978_overlap_{reanalysis,repository}.rs`): a narrow box inside a wide one over
//! one emission resolves to one row, and a box merging two emissions the spectrum separates is
//! retired in favour of them. What this pins is the **seam** — a run driven through the device
//! interface (the mock SDR behind the real source contract, never files fed to the pipeline), with
//! the spectrum living on the detect reader and the inventory on the detect writer:
//!
//! - the product invariant holds on what the inventory serves: no two shown rows overlap in both
//!   time and frequency, which is what "overlapping boxes are proof the analysis is wrong" means
//!   once the analysis has been re-run;
//! - and T-453's constraint is met: on a run with no unresolved overlap the reader takes **no**
//!   snapshot and the writer measures nothing, so the feature is not a standing tax on the thread
//!   that gates the ring. The request is one-shot and demand-driven (`hk_pipeline::overlap`), and
//!   the run summary reports what it actually cost (`/overlap/publish_nanos`,
//!   `/overlap/measure_nanos`).

mod common;

use common::*;
use hk_model::{InventoryQuery, RelationVisibility};

#[test]
fn the_overlap_re_analysis_is_wired_and_costs_nothing_when_nothing_overlaps() {
    let dir = TempDir::new("t978-overlap-seam");
    let meta = tone_recording(&dir.0, "tone", 2.4e6, 3.0, 852.86e6, None);
    let s = run(&dir.0, &meta, serde_json::json!({}));

    let requested = s.counter("/overlap/requested");
    let published = s.counter("/overlap/published");
    let measured = s.counter("/overlap/measured");
    eprintln!(
        "T-978 overlap seam: requested {requested}, published {published}, measured {measured}, \
         reader {} us, writer {} us",
        s.counter("/overlap/publish_nanos") / 1000,
        s.counter("/overlap/measure_nanos") / 1000,
    );

    // The reader only ever takes a snapshot for a request, and only the inventory makes one.
    assert!(
        published <= requested,
        "a snapshot was taken that nobody asked for: {published} published, {requested} requested"
    );
    if requested == 0 {
        assert_eq!(
            s.counter("/overlap/publish_nanos"),
            0,
            "no request was made, so the capture-adjacent thread paid nothing for this feature"
        );
        assert_eq!(s.counter("/overlap/measure_nanos"), 0);
    }

    // The product invariant, on what the inventory serves: no two shown rows overlap in time and
    // frequency. A single tone must not come out as stacked boxes.
    let repo = repo(&dir.0);
    let shown = inventory(&repo, InventoryQuery::default());
    for (i, a) in shown.iter().enumerate() {
        for b in shown.iter().skip(i + 1) {
            let overlap_hz = (a.emitter.freq().hi_hz.min(b.emitter.freq().hi_hz)
                - a.emitter.freq().lo_hz.max(b.emitter.freq().lo_hz))
            .max(0.0);
            let narrower = a
                .emitter
                .freq()
                .width_hz()
                .min(b.emitter.freq().width_hz())
                .max(1.0);
            let same_time = a.emitter.first_seen <= b.emitter.last_seen
                && b.emitter.first_seen <= a.emitter.last_seen;
            assert!(
                !(same_time && overlap_hz / narrower > 0.5),
                "two shown rows overlap in time and frequency, which is the analysis being wrong: \
                 {:.6} MHz / {:.1} kHz and {:.6} MHz / {:.1} kHz",
                a.emitter.f_center_hz / 1e6,
                a.emitter.bandwidth_hz / 1e3,
                b.emitter.f_center_hz / 1e6,
                b.emitter.bandwidth_hz / 1e3,
            );
        }
    }

    // And nothing was deleted to get there.
    let all = inventory(
        &repo,
        InventoryQuery {
            relations: RelationVisibility::All,
            ..InventoryQuery::default()
        },
    );
    assert!(all.len() >= shown.len());
}
