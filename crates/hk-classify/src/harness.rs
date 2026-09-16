//! T-213 project-wide evaluation harness (ADR-0016 §7): the tool every later classifier task
//! (T-204's DL stage, T-206's M3 exit gate) measures itself with, so their numbers are comparable.
//!
//! This builds on [`crate::eval::EvalReport`] (T-199's per-family, per-SNR scoring) rather than
//! re-implementing it. What this module adds on top:
//!
//! - **Enforced seed separation** ([`SeedGuard`]): T-199's `DEV_SEEDS`/`ACCEPTANCE_SEED_BASE`
//!   ranges were disjoint by construction but only checked with a `const` assertion that the two
//!   *ranges* don't overlap. That doesn't stop a call site from feeding a dev seed into an
//!   accuracy run, or an acceptance seed into a fitting loop, by simple mistake. `SeedGuard` is a
//!   runtime check at the point a seed is actually used, so a mix-up fails the run instead of
//!   silently inflating (or deflating) a number.
//! - **Per-class breakdown** ([`crate::eval::EvalReport::record_with_class`]) alongside the
//!   existing per-family one.
//! - **OTA truth that can only be truth** ([`OtaTruth`]): a CRC-valid decode or an explicit user
//!   label, and nothing else — enforced by an exhaustive match with no wildcard arm, so a future
//!   `CrcStatus` variant cannot become truth by silently falling through.
//! - **A reusable, blind runner** ([`Harness`]) over the taxonomy and held-out grids, in a small
//!   (fast iteration) and a full (gate-run) size.
//! - **A serialisable [`Report`]**: JSON for tooling, Markdown for a human, with reproducibility
//!   metadata (seeds, grid, versions, commit) so a number can be traced back to what produced it.
//!
//! **Blind, always.** [`Harness::run_synthetic`] hands the classifier closure only samples, sample
//! rate, measured OBW and SNR — never a label. Truth is supplied once, up front (by which grid
//! cell is being generated), and used only when [`crate::eval::EvalReport::record`] scores the
//! answer afterwards (docs/10 §3.2: never look a frequency, or a class, up and then tune to it).

use std::collections::HashSet;
use std::fmt;

use num_complex::Complex32;
use serde::{Deserialize, Serialize};

use hk_model::CrcStatus;
use hk_model::classify::{Classification, TaxonomyRef};

use crate::density::DENSITY_VERSION;
use crate::eval::EvalReport;
use crate::synth::{ACCEPTANCE_SEED_BASE, Class, DEV_SEEDS, SynthConfig, generate};
use crate::thresholds::{FEATURES_VERSION, RULES_VERSION, THRESHOLDS_VERSION, thresholds_of};

// ---------------------------------------------------------------------------------------------
// Seed separation, enforced.
// ---------------------------------------------------------------------------------------------

/// Which half of the blind-evaluation split a seed belongs to (ADR-0016 §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Split {
    /// Fits densities and thresholds. Never scored for accuracy.
    Dev,
    /// Scored for accuracy. Never fitted on anything.
    Acceptance,
}

impl Split {
    /// The split `seed` belongs to, or `None` when it is in neither declared range. A seed must
    /// be declared, not guessed: the gap between the ranges is refused rather than assigned to
    /// whichever side is "closer".
    pub fn of(seed: u64) -> Option<Split> {
        if DEV_SEEDS.contains(&seed) {
            Some(Split::Dev)
        } else if seed >= ACCEPTANCE_SEED_BASE {
            Some(Split::Acceptance)
        } else {
            None
        }
    }
}

/// A seed was used where it does not belong: the guard against tuning on the test set firing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedSplitViolation {
    /// The offending seed.
    pub seed: u64,
    /// The split the call site declared it was using.
    pub wanted: Split,
    /// The split the seed actually belongs to (`None`: neither range).
    pub actual: Option<Split>,
}

impl fmt::Display for SeedSplitViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.actual {
            Some(actual) => write!(
                f,
                "seed {} belongs to {actual:?} but was used as {:?} — dev and acceptance seeds \
                 must never mix (ADR-0016 §7)",
                self.seed, self.wanted
            ),
            None => write!(
                f,
                "seed {} is in neither the dev range ({:?}) nor the acceptance range (>= {}) — \
                 declare a seed inside a real range, don't guess",
                self.seed, DEV_SEEDS, ACCEPTANCE_SEED_BASE
            ),
        }
    }
}

impl std::error::Error for SeedSplitViolation {}

/// Enforces that every seed a run touches belongs to the split it declared.
///
/// A fitting routine (`fit-densities`) holds a `Dev` guard; an accuracy sweep holds an
/// `Acceptance` guard. [`SeedGuard::check`] fails — a `Result`, not a comment — the instant a
/// seed from the other range, or from neither, is used. This is the enforcement half of the
/// dev/acceptance split: the seed *ranges* being disjoint (checked at compile time in
/// [`crate::synth`]) only means the split is possible, not that every call site respects it.
#[derive(Clone, Debug)]
pub struct SeedGuard {
    role: Split,
    seen: HashSet<u64>,
}

impl SeedGuard {
    /// A guard that accepts only seeds of `role`.
    pub fn new(role: Split) -> Self {
        Self {
            role,
            seen: HashSet::new(),
        }
    }

    /// The split this guard enforces.
    pub fn role(&self) -> Split {
        self.role
    }

    /// Checks `seed` belongs to this guard's split, recording it if so. Returns the violation
    /// (not a panic) so a harness run can report exactly which seed leaked, rather than silently
    /// mixing it into a report or crashing with no context.
    pub fn check(&mut self, seed: u64) -> Result<(), SeedSplitViolation> {
        match Split::of(seed) {
            Some(actual) if actual == self.role => {
                self.seen.insert(seed);
                Ok(())
            }
            actual => Err(SeedSplitViolation {
                seed,
                wanted: self.role,
                actual,
            }),
        }
    }

    /// [`Self::check`], panicking loudly on failure. For call sites — a `fit-*` binary, a test —
    /// where a violation is a bug that must stop the run immediately, not a `Result` to plumb
    /// through.
    pub fn require(&mut self, seed: u64) {
        if let Err(e) = self.check(seed) {
            panic!("{e}");
        }
    }

    /// Every seed this guard has accepted so far, for a run's reproducibility record.
    pub fn seeds_used(&self) -> &HashSet<u64> {
        &self.seen
    }
}

// ---------------------------------------------------------------------------------------------
// OTA truth: only a CRC-valid decode, or an explicit user label.
// ---------------------------------------------------------------------------------------------

/// The *only* two ways a real capture's answer is known (ADR-0016 §7 / T-205): a decode whose CRC
/// check actually passed, or an explicit user reclassification. Both constructors match their
/// source explicitly with no wildcard arm, so a future [`CrcStatus`] variant — or any other
/// candidate source someone is tempted to wire in later — cannot become truth by silently
/// falling through a `_ =>` arm; it has to be handled here, on purpose.
#[derive(Clone, Debug, PartialEq)]
pub struct OtaTruth {
    /// `hk-mod@1` family (or class) label.
    pub label: String,
    /// Producing session: OTA truth is split by capture session, never by frame (ADR-0016 §7), so
    /// two frames of the same burst can never land on both sides of a split.
    pub session: String,
    /// `true` for a decoder label, `false` for a user label.
    pub from_decoder: bool,
}

impl OtaTruth {
    /// Truth from a decode, if and only if its CRC check actually passed.
    /// [`CrcStatus::Corrected`] (T-210: FEC-corrected bits, never confirm-by-decode evidence) and
    /// every other status are refused, returning `None`.
    pub fn from_decode(
        status: CrcStatus,
        label: impl Into<String>,
        session: impl Into<String>,
    ) -> Option<Self> {
        match status {
            CrcStatus::Valid => Some(Self {
                label: label.into(),
                session: session.into(),
                from_decoder: true,
            }),
            CrcStatus::Corrected | CrcStatus::Invalid | CrcStatus::NoCrc | CrcStatus::Unknown => {
                None
            }
        }
    }

    /// Truth from an explicit user reclassification.
    pub fn from_user(label: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            session: session.into(),
            from_decoder: false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Grid sizes.
// ---------------------------------------------------------------------------------------------

/// How much of the synthetic acceptance grid a run covers.
///
/// Both are measured with `cargo run -p hk-classify --release --bin eval-harness` (2026-09-15,
/// Apple silicon, single-threaded classification): **small ≈ 0.8 s** (162 snippets: 21 taxonomy
/// classes × 3 SNR offsets × 2 trials, plus 6 held-out classes × 3 offsets × 2 trials), **full ≈
/// 7 s** (1476 snippets: 21 × 5 offsets × 12 trials plus 6 × 3 × 12 — the size T-199's
/// `accuracy_sweep.rs` already runs, in a debug build, as part of `just test-crate hk-classify`).
/// Both exclude OTA fixtures, whose cost is one-off Git LFS I/O rather than classification time.
/// Run `small` on every change; run `full` before trusting a number for a gate (T-206) or an
/// enable decision (T-204 §4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GridSize {
    /// Fast iteration: SNR offsets {-5, 0, +5} dB from each family's gate, 2 trials per cell.
    Small,
    /// Gate runs: SNR offsets {-10, -5, 0, +5, +10} dB, 12 trials per cell (T-199's sweep size).
    Full,
}

impl GridSize {
    /// SNR offsets from a family's gate this grid covers, dB.
    pub fn offsets(self) -> &'static [f64] {
        match self {
            GridSize::Small => &[-5.0, 0.0, 5.0],
            GridSize::Full => &[-10.0, -5.0, 0.0, 5.0, 10.0],
        }
    }

    /// Trials per (class, SNR) cell.
    pub fn trials(self) -> u64 {
        match self {
            GridSize::Small => 2,
            GridSize::Full => 12,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Reproducible, serialisable report.
// ---------------------------------------------------------------------------------------------

/// Reproducibility metadata for one harness run: enough to trace a reported number back to what
/// produced it (grid, seeds, code and data versions, commit).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunMeta {
    /// `GridSize` this run used.
    pub grid: GridSize,
    /// The dev seed range (`crate::synth::DEV_SEEDS`), as `"start..end"`.
    pub dev_seeds: String,
    /// The acceptance seed base (`crate::synth::ACCEPTANCE_SEED_BASE`); acceptance seeds are
    /// every value at or above it.
    pub acceptance_seed_base: u64,
    /// Every seed this run actually consumed (sorted): the exact waveforms behind the numbers.
    pub seeds_used: Vec<u64>,
    /// Taxonomy version, e.g. `hk-mod@1`.
    pub taxonomy: String,
    /// Classifier rule-set version, e.g. `hk-classify/tree@1`.
    pub rules_version: String,
    /// `features@N` version.
    pub features_version: u32,
    /// Shipped density-model version.
    pub density_version: u32,
    /// Threshold-set version, e.g. `thresholds@1`.
    pub thresholds_version: String,
    /// The code commit this ran at (`git rev-parse HEAD`), or `"unknown"` when that isn't
    /// available (e.g. no `.git`, or `git` missing) — never a fatal error.
    pub commit: String,
}

impl RunMeta {
    /// Captures the run metadata for `grid`, given the guard that generated its synthetic seeds.
    pub fn capture(grid: GridSize, guard: &SeedGuard) -> Self {
        let mut seeds_used: Vec<u64> = guard.seeds_used().iter().copied().collect();
        seeds_used.sort_unstable();
        Self {
            grid,
            dev_seeds: format!("{}..{}", DEV_SEEDS.start, DEV_SEEDS.end),
            acceptance_seed_base: ACCEPTANCE_SEED_BASE,
            seeds_used,
            taxonomy: TaxonomyRef::current().to_string(),
            rules_version: RULES_VERSION.to_string(),
            features_version: FEATURES_VERSION,
            density_version: DENSITY_VERSION,
            thresholds_version: THRESHOLDS_VERSION.to_string(),
            commit: git_commit(),
        }
    }
}

fn git_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// One `source × family × SNR bin` family-level row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// `synthetic-acceptance`, `held-out`, or `ota:<session>`.
    pub source: String,
    /// Truth family (`unknown` for a held-out generator).
    pub family: String,
    /// SNR bin, dB (or offset from the family's gate, at the caller's convention).
    pub snr_bin_db: i64,
    /// Snippets in the cell.
    pub n: usize,
    /// Top-1 accuracy.
    pub top1: f64,
    /// Top-2 accuracy.
    pub top2: f64,
    /// Abstention (`unknown`) rate.
    pub unknown: f64,
    /// Wrong-label rate (a family was named, and it was the wrong one).
    pub wrong: f64,
}

/// One `source × family × class × SNR bin` within-family class row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClassRow {
    /// Source, as [`Row::source`].
    pub source: String,
    /// Truth family.
    pub family: String,
    /// Truth class within that family.
    pub class: String,
    /// SNR bin, dB.
    pub snr_bin_db: i64,
    /// Snippets in the cell.
    pub n: usize,
    /// Top-1 class accuracy.
    pub top1: f64,
    /// Top-2 class accuracy (within the family's class distribution).
    pub top2: f64,
    /// No class reported (family called, class abstained).
    pub unknown: f64,
    /// A class was reported, and it was the wrong one.
    pub wrong: f64,
}

/// Summary figures a gate check reads directly, computed the same way every time (ADR-0016 §7:
/// never one SNR-averaged number reported alone — this is a convenience index into `rows`/
/// `class_rows`, not a replacement for them).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Snippets scored, across every source.
    pub n_total: usize,
    /// Known families, `synthetic-acceptance` source, at or above gate + 5 dB: top-1 rate.
    pub known_top1_at_gate_plus5: f64,
    /// As above, top-2 rate.
    pub known_top2_at_gate_plus5: f64,
    /// Wrong-label rate over `synthetic-acceptance`, at or above each family's gate.
    pub wrong_label_rate_at_gate: f64,
    /// Wrong-label rate over `synthetic-acceptance`, every bin including below the gate.
    pub wrong_label_rate_overall: f64,
    /// Worst single-bin wrong-label rate over `synthetic-acceptance` cells at or above each
    /// family's gate, with n >= 10 (ADR-0016 §7's "any bin" floor).
    pub worst_bin_wrong_rate: f64,
    /// Unknown (abstention) recall over the `held-out` source: out-of-taxonomy inputs correctly
    /// not given a family.
    pub held_out_unknown_recall: f64,
    /// False-known rate over the `held-out` source: given *some* family instead of abstaining.
    pub held_out_false_known_rate: f64,
}

/// A full evaluation report: reproducibility metadata plus every condition (ADR-0016 §7's blind
/// evaluation protocol), as both JSON (`serde_json::to_string_pretty`) and Markdown
/// ([`Report::markdown`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Reproducibility metadata.
    pub run: RunMeta,
    /// Every family-level `source × family × SNR bin` row.
    pub rows: Vec<Row>,
    /// Every within-family-class `source × family × class × SNR bin` row.
    pub class_rows: Vec<ClassRow>,
    /// Convenience summary, computed from `rows`/`class_rows` by the same rules every time.
    pub summary: Summary,
}

impl Report {
    /// Builds a report from an [`EvalReport`]'s accumulated cells plus `run`'s metadata.
    pub fn build(eval: &EvalReport, run: RunMeta) -> Self {
        let rows: Vec<Row> = eval
            .cells()
            .map(|(source, family, bin, cell)| Row {
                source: source.to_owned(),
                family: family.to_owned(),
                snr_bin_db: bin,
                n: cell.n,
                top1: cell.top1_rate(),
                top2: cell.top2_rate(),
                unknown: cell.unknown_rate(),
                wrong: cell.wrong_rate(),
            })
            .collect();
        let class_rows: Vec<ClassRow> = eval
            .class_cells()
            .map(|(source, family, class, bin, cell)| ClassRow {
                source: source.to_owned(),
                family: family.to_owned(),
                class: class.to_owned(),
                snr_bin_db: bin,
                n: cell.n,
                top1: cell.top1_rate(),
                top2: cell.top2_rate(),
                unknown: cell.unknown_rate(),
                wrong: cell.wrong_rate(),
            })
            .collect();

        let strong = eval.total(Some("synthetic-acceptance"), 5.0);
        let at_gate = eval.total(Some("synthetic-acceptance"), 0.0);
        let all_bins = eval.total(Some("synthetic-acceptance"), f64::NEG_INFINITY);
        let held_out = eval.total(Some("held-out"), f64::NEG_INFINITY);
        let n_total = eval.total(None, f64::NEG_INFINITY).n;
        // The worst single-bin wrong-label rate, restricted to `synthetic-acceptance` at or above
        // each family's gate (ADR-0016 §7's "any bin" floor): [`EvalReport::worst_bin_wrong_rate`]
        // is deliberately source- and bin-agnostic (it also covers `held-out`, which has its own,
        // much looser, false-known floor), so it is not reused here.
        let worst_bin_wrong_rate = eval
            .cells()
            .filter(|(source, _, bin, cell)| {
                *source == "synthetic-acceptance" && *bin >= 0 && cell.n >= 10
            })
            .map(|(_, _, _, cell)| cell.wrong_rate())
            .fold(0.0, f64::max);

        let summary = Summary {
            n_total,
            known_top1_at_gate_plus5: strong.top1_rate(),
            known_top2_at_gate_plus5: strong.top2_rate(),
            wrong_label_rate_at_gate: at_gate.wrong_rate(),
            wrong_label_rate_overall: all_bins.wrong_rate(),
            worst_bin_wrong_rate,
            held_out_unknown_recall: held_out.unknown_rate(),
            held_out_false_known_rate: held_out.wrong_rate(),
        };

        Self {
            run,
            rows,
            class_rows,
            summary,
        }
    }

    /// A short Markdown report: the summary, then the full per-cell tables.
    pub fn markdown(&self) -> String {
        let mut out = format!(
            "# T-213 classification evaluation report\n\n\
             grid: {:?}, commit: {}, taxonomy: {}, rules: {}, thresholds: {}\n\
             dev seeds {}, acceptance seeds >= {}, {} seeds used\n\n\
             ## Summary (n = {})\n\n\
             | metric | value |\n|---|---:|\n\
             | known top-1 at gate+5dB | {:.3} |\n\
             | known top-2 at gate+5dB | {:.3} |\n\
             | wrong-label rate at gate | {:.3} |\n\
             | wrong-label rate overall | {:.3} |\n\
             | worst per-bin wrong-label rate | {:.3} |\n\
             | held-out unknown recall | {:.3} |\n\
             | held-out false-known rate | {:.3} |\n\n\
             ## Family rows\n\n\
             | source | family | SNR bin dB | n | top-1 | top-2 | unknown | wrong |\n\
             |---|---|---:|---:|---:|---:|---:|---:|\n",
            self.run.grid,
            self.run.commit,
            self.run.taxonomy,
            self.run.rules_version,
            self.run.thresholds_version,
            self.run.dev_seeds,
            self.run.acceptance_seed_base,
            self.run.seeds_used.len(),
            self.summary.n_total,
            self.summary.known_top1_at_gate_plus5,
            self.summary.known_top2_at_gate_plus5,
            self.summary.wrong_label_rate_at_gate,
            self.summary.wrong_label_rate_overall,
            self.summary.worst_bin_wrong_rate,
            self.summary.held_out_unknown_recall,
            self.summary.held_out_false_known_rate,
        );
        for r in &self.rows {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {:.2} | {:.2} | {:.2} | {:.2} |\n",
                r.source, r.family, r.snr_bin_db, r.n, r.top1, r.top2, r.unknown, r.wrong
            ));
        }
        if !self.class_rows.is_empty() {
            out.push_str(
                "\n## Class rows\n\n\
                 | source | family | class | SNR bin dB | n | top-1 | top-2 | unknown | wrong |\n\
                 |---|---|---|---:|---:|---:|---:|---:|---:|\n",
            );
            for r in &self.class_rows {
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {:.2} | {:.2} | {:.2} | {:.2} |\n",
                    r.source,
                    r.family,
                    r.class,
                    r.snr_bin_db,
                    r.n,
                    r.top1,
                    r.top2,
                    r.unknown,
                    r.wrong
                ));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// The runner.
// ---------------------------------------------------------------------------------------------

/// A blind evaluation run over the synthetic acceptance grid: generates every taxonomy class at
/// its family's gate plus the grid's SNR offsets, and every held-out (out-of-taxonomy) generator,
/// classifies each snippet through a caller-supplied closure that never sees the truth, and
/// scores the result. T-199's sweep, T-204's DL-stage evaluation and T-206's exit gate share this
/// runner so their numbers are measured the same way.
pub struct Harness {
    grid: GridSize,
    guard: SeedGuard,
    report: EvalReport,
}

impl Harness {
    /// A new run at `grid`'s size, scoring with 5 dB SNR bins.
    pub fn new(grid: GridSize) -> Self {
        Self {
            grid,
            guard: SeedGuard::new(Split::Acceptance),
            report: EvalReport::new(5.0),
        }
    }

    /// Runs the synthetic grid. `classify` receives samples, sample rate, measured OBW and SNR —
    /// never a label — and returns the [`Classification`] it produced.
    ///
    /// Fails (without scoring anything more) the instant a seed outside the acceptance range
    /// would be used — which cannot happen with this module's own generator, but can if a caller
    /// swaps in seeds of their own (e.g. a caller stitching in extra cells).
    pub fn run_synthetic<F>(&mut self, mut classify: F) -> Result<(), SeedSplitViolation>
    where
        F: FnMut(&[Complex32], f64, Option<f64>, Option<f64>) -> Classification,
    {
        let mut seed = ACCEPTANCE_SEED_BASE;
        for class in Class::TAXONOMY {
            let family = class.family().expect("a taxonomy class has a family");
            let gate = thresholds_of(family)
                .and_then(|t| t.snr_gate_db)
                .unwrap_or(10.0);
            for &offset in self.grid.offsets() {
                let snr = gate + offset;
                for _ in 0..self.grid.trials() {
                    seed += 1;
                    self.guard.check(seed)?;
                    let s = generate(*class, &SynthConfig::new(snr, seed));
                    let c = classify(&s.samples, s.sample_rate_hz, Some(s.obw_hz), Some(snr));
                    self.report.record_with_class(
                        "synthetic-acceptance",
                        Some(family),
                        Some(class.label()),
                        offset,
                        &c,
                    );
                }
            }
        }
        for class in Class::HELD_OUT {
            for &offset in [0.0_f64, 5.0, 10.0].iter() {
                let snr = 20.0 + offset;
                for _ in 0..self.grid.trials() {
                    seed += 1;
                    self.guard.check(seed)?;
                    let s = generate(*class, &SynthConfig::new(snr, seed));
                    let c = classify(&s.samples, s.sample_rate_hz, Some(s.obw_hz), Some(snr));
                    self.report.record("held-out", None, offset, &c);
                }
            }
        }
        Ok(())
    }

    /// Scores one OTA-labelled example. The only way to supply truth is an [`OtaTruth`], so an
    /// uncertain decode cannot be scored as if it were confirmed.
    pub fn record_ota(&mut self, truth: &OtaTruth, snr_db: f64, c: &Classification) {
        self.report.record(
            &format!("ota:{}", truth.session),
            Some(&truth.label),
            snr_db,
            c,
        );
    }

    /// The [`EvalReport`] accumulated so far (for a caller that wants T-199-style access
    /// alongside the harness, e.g. `worst_bin_wrong_rate`).
    pub fn eval(&self) -> &EvalReport {
        &self.report
    }

    /// Finishes the run: a serialisable [`Report`] with reproducibility metadata.
    pub fn finish(self) -> Report {
        let run = RunMeta::capture(self.grid, &self.guard);
        Report::build(&self.report, run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::Cell;
    use hk_model::Timestamp;
    use hk_model::classify::{
        CLASSIFICATION_SCHEMA, ClassProvenance, Coarse, HK_MOD_V1, LabelP, Stage, UNKNOWN,
    };

    /// A minimal, hand-built [`Classification`]: enough for [`EvalReport`]'s scoring maths, which
    /// only reads `family` and `posterior`. Not meant to pass [`Classification::validate`] in
    /// general (it skips fields real classifications compute, like open-set score); the harness's
    /// own runner only ever scores classifier-produced rows.
    fn stub(family: &str, posterior: &[(&str, f64)]) -> Classification {
        let coarse = if family == UNKNOWN {
            Coarse::Unknown
        } else {
            HK_MOD_V1.coarse_of(family).expect("known family")
        };
        let posterior: Vec<LabelP> = posterior
            .iter()
            .map(|(l, p)| LabelP {
                label: (*l).to_owned(),
                p: *p,
            })
            .collect();
        let confidence = posterior
            .iter()
            .find(|lp| lp.label == family)
            .map(|lp| lp.p)
            .expect("family present in its own posterior");
        Classification {
            schema: CLASSIFICATION_SCHEMA,
            t: Timestamp::UNIX_EPOCH,
            taxonomy: TaxonomyRef::current(),
            input: None,
            coarse,
            posterior: posterior.clone(),
            likelihood: posterior,
            prior: None,
            family: family.to_owned(),
            confidence,
            class: None,
            open_set_score: 0.0,
            entropy_norm: 0.0,
            stage: Stage::FeatureTree,
            provenance: ClassProvenance {
                rules: "test@0".to_owned(),
                features_version: 1,
                features_ref: None,
                ml: None,
                snr_db: None,
                snr_gate_db: 0.0,
                gated: false,
                thresholds: "test@0".to_owned(),
                suspect: Default::default(),
                power_mode: None,
            },
            flags: vec![],
            reasons: vec![],
        }
    }

    // -- Seed separation ------------------------------------------------------------------

    #[test]
    fn seed_guard_fires_when_a_seed_is_reused_across_splits() {
        let mut dev = SeedGuard::new(Split::Dev);
        dev.require(DEV_SEEDS.start); // fine: a real dev seed against a dev guard.

        // The exact mistake the guard exists to catch: a dev seed presented to an acceptance run
        // (the "acceptance seed appears in fitting" case, mirrored).
        let mut acceptance = SeedGuard::new(Split::Acceptance);
        let err = acceptance
            .check(DEV_SEEDS.start)
            .expect_err("a dev seed must be refused by an acceptance guard");
        assert_eq!(err.seed, DEV_SEEDS.start);
        assert_eq!(err.wanted, Split::Acceptance);
        assert_eq!(err.actual, Some(Split::Dev));

        // An acceptance seed presented to a fitting (dev) guard.
        let err2 = dev.check(ACCEPTANCE_SEED_BASE + 1).unwrap_err();
        assert_eq!(err2.actual, Some(Split::Acceptance));

        // A seed in neither declared range: refused, not guessed.
        let gap_seed = DEV_SEEDS.end; // 600 < ACCEPTANCE_SEED_BASE
        assert!(Split::of(gap_seed).is_none());
        let err3 = dev.check(gap_seed).unwrap_err();
        assert_eq!(err3.actual, None);

        // The guard's own bookkeeping only grew from the one seed that was actually valid.
        assert_eq!(dev.seeds_used().len(), 1);
        assert!(dev.seeds_used().contains(&DEV_SEEDS.start));
    }

    #[test]
    #[should_panic(expected = "must never mix")]
    fn require_panics_loudly_on_a_split_violation() {
        let mut acceptance = SeedGuard::new(Split::Acceptance);
        acceptance.require(DEV_SEEDS.start);
    }

    #[test]
    fn a_run_that_only_ever_uses_its_declared_split_never_fires() {
        let mut acceptance = SeedGuard::new(Split::Acceptance);
        for seed in ACCEPTANCE_SEED_BASE..ACCEPTANCE_SEED_BASE + 50 {
            acceptance.check(seed).expect("in-range acceptance seed");
        }
        assert_eq!(acceptance.seeds_used().len(), 50);
    }

    // -- OTA truth --------------------------------------------------------------------------

    #[test]
    fn ota_truth_only_comes_from_a_crc_valid_decode_or_a_user_label() {
        assert!(OtaTruth::from_decode(CrcStatus::Valid, "fsk", "s1").is_some());
        // The dangerous case this exists to prevent: a corrected frame is usable, but it is not
        // confirm-by-decode evidence (T-210), and must not silently become truth.
        assert!(OtaTruth::from_decode(CrcStatus::Corrected, "fsk", "s1").is_none());
        assert!(OtaTruth::from_decode(CrcStatus::Invalid, "fsk", "s1").is_none());
        assert!(OtaTruth::from_decode(CrcStatus::NoCrc, "fsk", "s1").is_none());
        assert!(OtaTruth::from_decode(CrcStatus::Unknown, "fsk", "s1").is_none());

        let valid = OtaTruth::from_decode(CrcStatus::Valid, "wfm", "session-a").unwrap();
        assert!(valid.from_decoder);
        assert_eq!(valid.label, "wfm");
        assert_eq!(valid.session, "session-a");

        let user = OtaTruth::from_user("2fsk", "session-b");
        assert!(!user.from_decoder);
        assert_eq!(user.label, "2fsk");
    }

    // -- Scoring maths (via EvalReport, reused rather than reimplemented) --------------------

    #[test]
    fn a_known_confusion_matrix_produces_known_top1_top2_wrong_and_abstention_figures() {
        let mut r = EvalReport::new(5.0);
        // Truth is "fsk" for every row below. Deliberately built so top-1, top-2, wrong and
        // abstention land on different rows, so each figure is independently checkable.
        let correct = stub("fsk", &[("fsk", 0.8), ("psk-qam", 0.15), (UNKNOWN, 0.05)]);
        let wrong = stub(
            "analog",
            &[
                ("analog", 0.7),
                ("noise-like", 0.2),
                ("fsk", 0.05),
                (UNKNOWN, 0.05),
            ],
        );
        let abstained = stub(UNKNOWN, &[(UNKNOWN, 0.6), ("analog", 0.3), ("fsk", 0.1)]);

        for _ in 0..3 {
            r.record("t", Some("fsk"), 20.0, &correct);
        }
        r.record("t", Some("fsk"), 20.0, &wrong);
        r.record("t", Some("fsk"), 20.0, &abstained);

        let cell: Cell = r.family("fsk", 0.0);
        assert_eq!(cell.n, 5);
        assert_eq!(cell.top1, 3, "only the 3 correct rows are top-1");
        assert_eq!(
            cell.top2, 3,
            "fsk sits outside the top 2 of both the wrong and the abstained rows"
        );
        assert_eq!(cell.unknown, 1, "exactly the abstained row");
        assert_eq!(cell.wrong, 1, "exactly the row called analog");

        assert!((cell.top1_rate() - 0.6).abs() < 1e-12);
        assert!((cell.top2_rate() - 0.6).abs() < 1e-12);
        assert!((cell.unknown_rate() - 0.2).abs() < 1e-12);
        assert!((cell.wrong_rate() - 0.2).abs() < 1e-12);

        // A held-out row (truth = None) that is given a family at all is a false "known", scored
        // as wrong even though nothing "wrong" was named against a specific truth family.
        let mut h = EvalReport::new(5.0);
        h.record("held-out", None, 20.0, &wrong);
        assert_eq!(h.total(Some("held-out"), f64::NEG_INFINITY).wrong, 1);
        h.record("held-out", None, 20.0, &abstained);
        let t = h.total(Some("held-out"), f64::NEG_INFINITY);
        assert_eq!((t.n, t.wrong, t.unknown), (2, 1, 1));
    }

    #[test]
    fn per_class_scoring_is_independent_of_the_family_call() {
        let mut r = EvalReport::new(5.0);
        // Family correct, class correct.
        let mut c1 = stub("fsk", &[("fsk", 0.9), (UNKNOWN, 0.1)]);
        c1.class = Some(hk_model::classify::ClassCall {
            label: "2fsk".to_owned(),
            p: 0.8,
            dist: vec![],
            stage: Stage::FeatureTree,
        });
        // Family correct, class wrong (a within-family confusion the family-level cell can't see).
        let mut c2 = c1.clone();
        c2.class = Some(hk_model::classify::ClassCall {
            label: "gfsk".to_owned(),
            p: 0.6,
            dist: vec![],
            stage: Stage::FeatureTree,
        });
        // Family correct, class abstained.
        let mut c3 = c1.clone();
        c3.class = None;

        r.record_with_class("t", Some("fsk"), Some("2fsk"), 20.0, &c1);
        r.record_with_class("t", Some("fsk"), Some("2fsk"), 20.0, &c2);
        r.record_with_class("t", Some("fsk"), Some("2fsk"), 20.0, &c3);

        // Family-level: all three are correct (family == "fsk" in every row).
        let family_cell = r.family("fsk", 0.0);
        assert_eq!(family_cell.top1, 3);

        // Class-level: only c1's class call ("2fsk") is correct; c2 is wrong; c3 never reported
        // a class (`class: None`) so it abstains at the class level despite the family being
        // right — the two levels disagree exactly where they should.
        let class_cell = r
            .class_cells()
            .find(|(source, family, class, bin, _)| {
                *source == "t" && *family == "fsk" && *class == "2fsk" && *bin == 20
            })
            .map(|(_, _, _, _, c)| c.clone())
            .expect("the class cell was recorded");
        assert_eq!(
            (
                class_cell.n,
                class_cell.top1,
                class_cell.wrong,
                class_cell.unknown
            ),
            (3, 1, 1, 1)
        );
    }

    // -- Report shape ------------------------------------------------------------------------

    #[test]
    fn report_round_trips_through_json_and_markdown_has_the_summary() {
        let mut r = EvalReport::new(5.0);
        r.record(
            "synthetic-acceptance",
            Some("fsk"),
            5.0,
            &stub("fsk", &[("fsk", 0.9), (UNKNOWN, 0.1)]),
        );
        let guard = SeedGuard::new(Split::Acceptance);
        let run = RunMeta::capture(GridSize::Small, &guard);
        let report = Report::build(&r, run);

        let json = serde_json::to_string_pretty(&report).expect("serialise");
        let back: Report = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back, report);

        let md = report.markdown();
        assert!(md.contains("## Summary"));
        assert!(md.contains("| source | family | SNR bin dB |"));
    }
}
