//! Runs the T-213 evaluation harness against the classical classifier and writes
//! `report.json` + `report.md` (ADR-0016 §7). This is the tool T-199's follow-up (T-230), the DL
//! stage (T-204) and the M3 exit gate (T-206) all measure themselves with.
//!
//! ```text
//! cargo run -p hk-classify --bin eval-harness -- [small|full] [out-dir]
//! ```
//!
//! `small` (the default) is for iteration; `full` is the size to trust before a gate or an enable
//! decision. See [`hk_classify::harness::GridSize`] for what each covers and their measured wall
//! times.

use std::env;
use std::path::PathBuf;
use std::time::Instant;

use hk_classify::harness::{GridSize, Harness};
use hk_classify::{Classifier, ClassifyRequest, SymbolEstimator};
use hk_model::Timestamp;

fn main() {
    let mut args = env::args().skip(1);
    let grid = match args.next().as_deref() {
        Some("full") => GridSize::Full,
        Some("small") | None => GridSize::Small,
        Some(other) => {
            eprintln!("unknown grid {other:?}, expected small|full");
            std::process::exit(2);
        }
    };
    let out = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&out).expect("create output dir");

    let started = Instant::now();
    let classifier = Classifier::new();
    // One C14 estimator for the whole run, so its FFT plans are cached across snippets (T-238).
    let mut c14 = SymbolEstimator::new();
    let mut harness = Harness::new(grid);
    harness
        .run_synthetic(|s| {
            let symbols = c14.from_samples(
                s.symbol_samples,
                s.symbol_sample_rate_hz,
                s.obw_hz,
                s.snr_db,
            );
            let mut req = ClassifyRequest::new(s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
            req.obw_hz = s.obw_hz;
            req.snr_db = s.snr_db;
            req.symbols = symbols.as_ref();
            classifier.classify(&req)
        })
        .expect("seed separation held");
    let report = harness.finish();
    let elapsed = started.elapsed();

    let json_path = out.join("report.json");
    let md_path = out.join("report.md");
    std::fs::write(
        &json_path,
        serde_json::to_string_pretty(&report).expect("serialise report"),
    )
    .expect("write report.json");
    std::fs::write(&md_path, report.markdown()).expect("write report.md");

    eprintln!(
        "[T-213] {:?} grid: {} snippets in {:.2}s ({} family rows, {} class rows) -> {}, {}",
        grid,
        report.summary.n_total,
        elapsed.as_secs_f64(),
        report.rows.len(),
        report.class_rows.len(),
        json_path.display(),
        md_path.display(),
    );
    eprintln!(
        "[T-213] known top-1 {:.3}, top-2 {:.3}, wrong-label (at gate) {:.3}, held-out unknown recall {:.3}",
        report.summary.known_top1_at_gate_plus5,
        report.summary.known_top2_at_gate_plus5,
        report.summary.wrong_label_rate_at_gate,
        report.summary.held_out_unknown_recall,
    );
}
