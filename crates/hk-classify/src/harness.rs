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
//! **Blind, always.** [`Harness::run_synthetic`] hands the classifier closure a [`Snippet`] — what
//! a receiver could measure about the waveform and nothing else — never a label. Truth is supplied
//! once, up front (by which grid cell is being generated), and used only when
//! [`crate::eval::EvalReport::record`] scores the answer afterwards (docs/10 §3.2: never look a
//! frequency, or a class, up and then tune to it).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;

use num_complex::Complex32;
use serde::{Deserialize, Serialize};

use hk_model::CrcStatus;
use hk_model::classify::{Classification, TaxonomyRef, UNKNOWN};

use crate::density::DENSITY_VERSION;
use crate::eval::EvalReport;
use crate::synth::{
    ACCEPTANCE_SEED_BASE, Class, DEV_SEEDS, SynthConfig, generate, open_set_families,
};
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

// ---------------------------------------------------------------------------------------------
// Open-set coverage: which families actually had a negative, and which only look like they did.
// ---------------------------------------------------------------------------------------------

/// One family's **open-set coverage**: how many out-of-taxonomy negatives were routed to it
/// ([`Class::probes_family`]) and what the classifier did with them.
///
/// The rates are [`Option`] on purpose. A family no negative reached has *no* false-known rate, and
/// writing `0.0` there — which is what the aggregate figures did before T-244 — publishes a perfect
/// score for something that was never run. `None` serialises as `null` and cannot be read as a
/// pass.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FamilyCoverage {
    /// `hk-mod@1` family.
    pub family: String,
    /// Out-of-taxonomy snippets routed to this family. Zero means **unmeasured**.
    pub n_ood: usize,
    /// The held-out generators that produced them (labels), for tracing a number to a waveform.
    pub generators: Vec<String>,
    /// Share of them the classifier abstained on, or `None` when nothing was measured. Scored by
    /// the same rule as [`Summary::held_out_unknown_recall`] (`family == "unknown"`), so the rows
    /// here decompose that aggregate rather than restating it differently.
    pub unknown_recall: Option<f64>,
    /// Share given *some* family instead, or `None` when nothing was measured.
    pub false_known_rate: Option<f64>,
}

impl FamilyCoverage {
    /// A family nothing was measured for: the shape a coverage gap takes in the report.
    pub fn unmeasured(family: &str) -> Self {
        Self {
            family: family.to_owned(),
            n_ood: 0,
            generators: Vec::new(),
            unknown_recall: None,
            false_known_rate: None,
        }
    }

    /// Whether any out-of-taxonomy snippet actually reached this family.
    pub fn measured(&self) -> bool {
        self.n_ood > 0
    }
}

/// Families whose open set no out-of-taxonomy generator reached: the gate would otherwise report
/// coverage it never tested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageGap {
    /// The unmeasured families, sorted.
    pub families: Vec<String>,
}

impl fmt::Display for CoverageGap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no out-of-taxonomy generator reached {}: their open set (unknown recall, false-known \
             rate, AUROC) is UNMEASURED, not good — add a generator whose `probes_family` names \
             each of them (ADR-0016 §7, T-244)",
            self.families.join(", ")
        )
    }
}

impl std::error::Error for CoverageGap {}

/// Running tally of one family's out-of-taxonomy negatives.
#[derive(Clone, Debug, Default)]
struct OodTally {
    n: usize,
    unknown: usize,
    generators: BTreeSet<&'static str>,
}

// ---------------------------------------------------------------------------------------------
// Verifier run-rate: how often the post-sync stage actually ran, and why it did not (T-597).
// ---------------------------------------------------------------------------------------------

/// The outcome bucket for a classification that carries **no** verifier reason at all. Every
/// classifier path records one since T-589, so a non-zero count here is a defect, not a category.
pub const VERIFIER_UNSTATED: &str = "verifier_unstated";
/// The outcome bucket for a classification that carries **more than one** verifier reason: the
/// stage cannot have both run and skipped, so this too is a defect, never a category.
pub const VERIFIER_MULTIPLE: &str = "verifier_multiple";

/// Upstream abstention reasons the classifier records ([`crate::classifier`]'s step 4), used to
/// break `verifier_abstained_upstream` down by why the family was withheld.
const UPSTREAM_ABSTENTIONS: &[&str] = &[
    "no_family_scored",
    "low_confidence",
    "open_set",
    "open_set_family",
    "ambiguous",
];

/// Every verifier outcome code a classification can carry: the two "ran" codes and every
/// [`SkipReason`](crate::verify::SkipReason).
pub fn verifier_outcome_codes() -> Vec<&'static str> {
    let mut codes = vec![
        crate::verify::VERIFIER_CONFIRMED,
        crate::verify::VERIFIER_RERANKED,
    ];
    codes.extend(crate::verify::SkipReason::ALL.iter().map(|r| r.as_str()));
    codes
}

/// The one verifier outcome `c` records, or [`VERIFIER_UNSTATED`] / [`VERIFIER_MULTIPLE`] when it
/// records none or several — both of which a report surfaces rather than drops.
pub fn verifier_outcome(c: &Classification) -> &'static str {
    let codes = verifier_outcome_codes();
    let mut found = c
        .reasons
        .iter()
        .filter_map(|r| codes.iter().copied().find(|code| *code == r.as_str()));
    match (found.next(), found.next()) {
        (None, _) => VERIFIER_UNSTATED,
        (Some(one), None) => one,
        (Some(_), Some(_)) => VERIFIER_MULTIPLE,
    }
}

/// One `source × family × class × SNR bin` row of **verifier run-rate** (T-597).
///
/// A post-sync stage that never fires is indistinguishable, in the accuracy rows, from one that
/// fires and agrees: both leave the tree's call standing. This row makes the difference a count.
/// `outcomes` holds every classification in the cell under exactly one code — the two "ran" codes,
/// a [`SkipReason`](crate::verify::SkipReason) code, or the defect buckets [`VERIFIER_UNSTATED`] /
/// [`VERIFIER_MULTIPLE`] — so its values always **sum to `n`** ([`VerifierRow::accounted`]). A
/// falling `ran` is the signal: a change that makes the tree more decisive silently removes the
/// verifier from the shipped path, and this is where that shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifierRow {
    /// Source, as [`Row::source`].
    pub source: String,
    /// Truth family (`unknown` for a held-out generator).
    pub family: String,
    /// Truth class (or held-out generator label; `-` when the truth has no class).
    pub class: String,
    /// SNR bin, dB, at the same convention as [`Row::snr_bin_db`].
    pub snr_bin_db: i64,
    /// Classifications in the cell.
    pub n: usize,
    /// How many of them the verifier actually ran on (`verifier_confirmed` + `verifier_reranked`).
    pub ran: usize,
    /// Every outcome code seen, with its count. Sums to `n`.
    pub outcomes: BTreeMap<String, usize>,
    /// `verifier_abstained_upstream` broken down by the classifier's own abstention reason
    /// (`low_confidence`, `open_set`, …; `other` for none of those). Sums to that outcome's count.
    pub abstained_because: BTreeMap<String, usize>,
}

impl VerifierRow {
    fn empty(source: &str, family: &str, class: &str, snr_bin_db: i64) -> Self {
        Self {
            source: source.to_owned(),
            family: family.to_owned(),
            class: class.to_owned(),
            snr_bin_db,
            n: 0,
            ran: 0,
            outcomes: BTreeMap::new(),
            abstained_because: BTreeMap::new(),
        }
    }

    /// Adds one classification to the row.
    fn add(&mut self, c: &Classification) {
        let code = verifier_outcome(c);
        self.n += 1;
        if code == crate::verify::VERIFIER_CONFIRMED || code == crate::verify::VERIFIER_RERANKED {
            self.ran += 1;
        }
        *self.outcomes.entry(code.to_owned()).or_default() += 1;
        if code == crate::verify::SkipReason::Abstained.as_str() {
            let why = c
                .reasons
                .iter()
                .find(|r| UPSTREAM_ABSTENTIONS.contains(&r.as_str()))
                .map_or("other", |r| r.as_str());
            *self.abstained_because.entry(why.to_owned()).or_default() += 1;
        }
    }

    /// Folds another row's counts into this one (for per-family totals).
    fn merge(&mut self, other: &VerifierRow) {
        self.n += other.n;
        self.ran += other.ran;
        for (k, v) in &other.outcomes {
            *self.outcomes.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &other.abstained_because {
            *self.abstained_because.entry(k.clone()).or_default() += v;
        }
    }

    /// Share of the cell the verifier ran on, or `None` for an empty cell (never `0.0`: an empty
    /// cell is unmeasured, not a stage that never runs).
    pub fn run_rate(&self) -> Option<f64> {
        (self.n > 0).then(|| self.ran as f64 / self.n as f64)
    }

    /// Count recorded under `code`.
    pub fn count(&self, code: &str) -> usize {
        self.outcomes.get(code).copied().unwrap_or(0)
    }

    /// Whether the parts account for the whole: every classification under exactly one stated
    /// outcome, the outcomes summing to `n`, `ran` equal to the two "ran" codes, and the
    /// upstream-abstention breakdown summing to its outcome.
    pub fn accounted(&self) -> bool {
        self.outcomes.values().sum::<usize>() == self.n
            && self.count(VERIFIER_UNSTATED) == 0
            && self.count(VERIFIER_MULTIPLE) == 0
            && self.ran
                == self.count(crate::verify::VERIFIER_CONFIRMED)
                    + self.count(crate::verify::VERIFIER_RERANKED)
            && self.abstained_because.values().sum::<usize>()
                == self.count(crate::verify::SkipReason::Abstained.as_str())
    }
}

/// A verifier run-rate table whose parts do not account for the whole — how a skipped stage hides.
#[derive(Clone, Debug, PartialEq)]
pub struct VerifierUnaccounted {
    /// The offending rows, as `source/family/class@bin`.
    pub rows: Vec<String>,
}

impl fmt::Display for VerifierUnaccounted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "verifier outcomes do not account for every classification in {}: a classification \
             with no verifier reason, or with two, reads like a stage that ran (T-589/T-597)",
            self.rows.join(", ")
        )
    }
}

impl std::error::Error for VerifierUnaccounted {}

/// Running verifier tally, keyed `(source, family, class, bin)`.
#[derive(Clone, Debug, Default)]
struct VerifierTally {
    cells: BTreeMap<(String, String, String, i64), VerifierRow>,
}

impl VerifierTally {
    fn record(&mut self, source: &str, family: &str, class: &str, snr_db: f64, c: &Classification) {
        // The same binning as `EvalReport` at the harness's 5 dB, so a verifier row lines up with
        // the accuracy row for the same cell.
        let bin = (snr_db / 5.0).floor() as i64 * 5;
        self.cells
            .entry((source.to_owned(), family.to_owned(), class.to_owned(), bin))
            .or_insert_with(|| VerifierRow::empty(source, family, class, bin))
            .add(c);
    }

    fn rows(&self) -> Vec<VerifierRow> {
        self.cells.values().cloned().collect()
    }
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
    /// Families with **no** out-of-taxonomy negative in this run. The two figures above are
    /// averages over whatever negatives existed, so they say nothing at all about these families;
    /// a non-empty list here means the run cannot support an open-set claim about them
    /// ([`Report::require_open_set_coverage`]).
    pub unmeasured_open_set_families: Vec<String>,
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
    /// Per-family open-set coverage: one row for **every** measurable family, including those no
    /// negative reached (`n_ood: 0`, rates `null`).
    pub coverage: Vec<FamilyCoverage>,
    /// Verifier run-rate per `source × family × class × SNR bin` (T-597): how often the post-sync
    /// stage ran and why it did not. Empty for a report built from an [`EvalReport`] alone.
    #[serde(default)]
    pub verifier: Vec<VerifierRow>,
    /// Convenience summary, computed from `rows`/`class_rows` by the same rules every time.
    pub summary: Summary,
}

impl Report {
    /// Builds a report from an [`EvalReport`]'s accumulated cells, `run`'s metadata and the
    /// per-family open-set coverage the run accumulated.
    ///
    /// Every family in [`open_set_families`] gets a row whether or not `coverage` mentions it, so
    /// a caller that measured nothing produces a report that says so — the check
    /// ([`Report::require_open_set_coverage`]) fails closed rather than passing by omission.
    pub fn build(eval: &EvalReport, run: RunMeta, coverage: Vec<FamilyCoverage>) -> Self {
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

        // One row per measurable family, taken from what was measured where that exists and marked
        // unmeasured where it does not. Anything the caller supplied for a family outside the list
        // is kept rather than dropped, so an extra measurement can never disappear silently.
        let mut supplied: BTreeMap<String, FamilyCoverage> = coverage
            .into_iter()
            .map(|c| (c.family.clone(), c))
            .collect();
        let mut coverage: Vec<FamilyCoverage> = open_set_families()
            .into_iter()
            .map(|f| {
                supplied
                    .remove(f)
                    .unwrap_or_else(|| FamilyCoverage::unmeasured(f))
            })
            .collect();
        coverage.extend(supplied.into_values());
        let unmeasured_open_set_families: Vec<String> = coverage
            .iter()
            .filter(|c| !c.measured())
            .map(|c| c.family.clone())
            .collect();

        let summary = Summary {
            n_total,
            known_top1_at_gate_plus5: strong.top1_rate(),
            known_top2_at_gate_plus5: strong.top2_rate(),
            wrong_label_rate_at_gate: at_gate.wrong_rate(),
            wrong_label_rate_overall: all_bins.wrong_rate(),
            worst_bin_wrong_rate,
            held_out_unknown_recall: held_out.unknown_rate(),
            held_out_false_known_rate: held_out.wrong_rate(),
            unmeasured_open_set_families,
        };

        Self {
            run,
            rows,
            class_rows,
            coverage,
            verifier: Vec::new(),
            summary,
        }
    }

    /// Verifier run-rate totals for `family` over `source`, one row per SNR bin (class `*`), in
    /// bin order. The per-family, per-SNR view the run-rate is watched at.
    pub fn verifier_by_snr(&self, source: &str, family: &str) -> Vec<VerifierRow> {
        let mut bins: BTreeMap<i64, VerifierRow> = BTreeMap::new();
        for r in self
            .verifier
            .iter()
            .filter(|r| r.source == source && r.family == family)
        {
            bins.entry(r.snr_bin_db)
                .or_insert_with(|| VerifierRow::empty(source, family, "*", r.snr_bin_db))
                .merge(r);
        }
        bins.into_values().collect()
    }

    /// **Fails when the verifier table does not account for every classification** (T-597): a row
    /// whose outcomes do not sum to its `n`, or that holds a classification stating no verifier
    /// outcome, or two.
    pub fn require_verifier_accounted(&self) -> Result<(), VerifierUnaccounted> {
        let rows: Vec<String> = self
            .verifier
            .iter()
            .filter(|r| !r.accounted())
            .map(|r| format!("{}/{}/{}@{}", r.source, r.family, r.class, r.snr_bin_db))
            .collect();
        if rows.is_empty() {
            Ok(())
        } else {
            Err(VerifierUnaccounted { rows })
        }
    }

    /// **Fails when a family's open set was never exercised** (T-244).
    ///
    /// The aggregate held-out figures are averages over whatever negatives happened to exist. If no
    /// generator routes to a family, that family contributes nothing to them, and the aggregate
    /// still reads like a result — which is how `analog` and `psk-qam` came to have an
    /// unknown-recall number the M3 gate would have published without ever testing for it. A gate
    /// run calls this and refuses to report rather than averaging over a hole.
    pub fn require_open_set_coverage(&self) -> Result<(), CoverageGap> {
        let families: Vec<String> = self
            .coverage
            .iter()
            .filter(|c| !c.measured())
            .map(|c| c.family.clone())
            .collect();
        if families.is_empty() {
            Ok(())
        } else {
            Err(CoverageGap { families })
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
             | held-out unknown recall (harness draw, abstention-only — NOT the ADR-0016 §7.1 gate \
             figure) | {:.3} |\n\
             | held-out false-known rate (harness draw) | {:.3} |\n\
             | families with an UNMEASURED open set | {} |\n\n\
             ## Open-set coverage, per family\n\n\
             The two held-out figures above are averages over whatever negatives existed. A family \
             with no negative contributes nothing to them and is **unmeasured**, never good.\n\n\
             They are also **not the M3 gate's figure**: this draw uses held-out seeds continuing \
             the `synthetic-acceptance` sequence and scores `family == \"unknown\"` alone, while \
             the gate (`m3_grid.rs::m3_unknown_recall_and_false_known_rate`) uses its own seed \
             range and also counts `open_set_score >= 0.5`. Draw-to-draw spread of this quantity \
             is sd ~0.008 over 396 snippets, so the two readings differ by less than noise and \
             neither substitutes for the other — quote the gate's line, per ADR-0016 §7.2.\n\n\
             | family | OOD n | unknown recall | false-known | generators |\n\
             |---|---:|---:|---:|---|\n",
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
            self.summary.unmeasured_open_set_families.len(),
        );
        for c in &self.coverage {
            match (c.unknown_recall, c.false_known_rate) {
                (Some(recall), Some(false_known)) => out.push_str(&format!(
                    "| {} | {} | {recall:.3} | {false_known:.3} | {} |\n",
                    c.family,
                    c.n_ood,
                    c.generators.join(", ")
                )),
                _ => out.push_str(&format!(
                    "| {} | 0 | UNMEASURED | UNMEASURED | none |\n",
                    c.family
                )),
            }
        }
        if !self.verifier.is_empty() {
            let codes = verifier_outcome_codes();
            out.push_str(
                "\n## Post-sync verifier run-rate (T-597)\n\n\
                 How often the verifier actually ran, per truth family and SNR bin, and why it did \
                 not. A stage that never fires reads exactly like one that fires and agrees in the \
                 accuracy rows below; a **falling run count** here is the regression signal. The \
                 outcome columns sum to n.\n\n\
                 | source | family | SNR bin dB | n | ran | run rate |",
            );
            for code in &codes {
                out.push_str(&format!(" {} |", code.trim_start_matches("verifier_")));
            }
            out.push_str(" unstated/multiple | abstained because |\n|---|---|---:|---:|---:|---:|");
            for _ in &codes {
                out.push_str("---:|");
            }
            out.push_str("---:|---|\n");
            let mut keys: Vec<(String, String)> = self
                .verifier
                .iter()
                .map(|r| (r.source.clone(), r.family.clone()))
                .collect();
            keys.sort();
            keys.dedup();
            for (source, family) in keys {
                for r in self.verifier_by_snr(&source, &family) {
                    out.push_str(&format!(
                        "| {} | {} | {} | {} | {} | {:.2} |",
                        r.source,
                        r.family,
                        r.snr_bin_db,
                        r.n,
                        r.ran,
                        r.run_rate().unwrap_or(0.0)
                    ));
                    for code in &codes {
                        out.push_str(&format!(" {} |", r.count(code)));
                    }
                    let because: Vec<String> = r
                        .abstained_because
                        .iter()
                        .map(|(k, v)| format!("{k} {v}"))
                        .collect();
                    out.push_str(&format!(
                        " {} | {} |\n",
                        r.count(VERIFIER_UNSTATED) + r.count(VERIFIER_MULTIPLE),
                        because.join(", ")
                    ));
                }
            }
        }
        out.push_str(
            "\n## Family rows\n\n\
             | source | family | SNR bin dB | n | top-1 | top-2 | unknown | wrong |\n\
             |---|---|---:|---:|---:|---:|---:|---:|\n",
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

/// One blind snippet handed to a classification closure: everything a receiver could have measured
/// about the waveform, and deliberately nothing else. There is no label here, and there never may
/// be one — that is what makes a harness run blind.
///
/// It carries **two views of the same emission** because the cascade's two measuring stages need
/// different geometries: the classifier's own snippet at [`crate::synth::SAMPLES_PER_OBW`], and the
/// C14 symbol view at [`crate::symbols::SYMBOL_SAMPLES_PER_OBW`] (T-238). Production is the same
/// shape: `BlindEstimator::prepare` re-normalises a snippet rather than reusing the classifier's.
#[derive(Clone, Copy, Debug)]
pub struct Snippet<'a> {
    /// Normalised samples at the classifier's analysis geometry.
    pub samples: &'a [Complex32],
    /// Sample rate of [`Snippet::samples`], Hz.
    pub sample_rate_hz: f64,
    /// The same emission at C14's geometry, for blind symbol estimation.
    pub symbol_samples: &'a [Complex32],
    /// Sample rate of [`Snippet::symbol_samples`], Hz.
    pub symbol_sample_rate_hz: f64,
    /// Measured OBW99, Hz.
    pub obw_hz: Option<f64>,
    /// Measured in-band SNR, dB.
    pub snr_db: Option<f64>,
}

/// A blind evaluation run over the synthetic acceptance grid: generates every taxonomy class at
/// its family's gate plus the grid's SNR offsets, and every held-out (out-of-taxonomy) generator,
/// classifies each snippet through a caller-supplied closure that never sees the truth, and
/// scores the result. T-199's sweep, T-204's DL-stage evaluation and T-206's exit gate share this
/// runner so their numbers are measured the same way.
pub struct Harness {
    grid: GridSize,
    guard: SeedGuard,
    report: EvalReport,
    open_set: BTreeMap<&'static str, OodTally>,
    verifier: VerifierTally,
}

impl Harness {
    /// A new run at `grid`'s size, scoring with 5 dB SNR bins.
    pub fn new(grid: GridSize) -> Self {
        Self {
            grid,
            guard: SeedGuard::new(Split::Acceptance),
            report: EvalReport::new(5.0),
            open_set: BTreeMap::new(),
            verifier: VerifierTally::default(),
        }
    }

    /// Runs the synthetic grid. `classify` receives a [`Snippet`] — never a label — and returns the
    /// [`Classification`] it produced.
    ///
    /// Fails (without scoring anything more) the instant a seed outside the acceptance range
    /// would be used — which cannot happen with this module's own generator, but can if a caller
    /// swaps in seeds of their own (e.g. a caller stitching in extra cells).
    pub fn run_synthetic<F>(&mut self, mut classify: F) -> Result<(), SeedSplitViolation>
    where
        F: FnMut(&Snippet<'_>) -> Classification,
    {
        /// The blind view of one generated waveform: everything but its class.
        fn blind(s: &crate::synth::SynthSignal, snr: f64) -> Snippet<'_> {
            Snippet {
                samples: &s.samples,
                sample_rate_hz: s.sample_rate_hz,
                symbol_samples: &s.symbol_samples,
                symbol_sample_rate_hz: s.symbol_sample_rate_hz,
                obw_hz: Some(s.obw_hz),
                snr_db: Some(snr),
            }
        }
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
                    let c = classify(&blind(&s, snr));
                    self.report.record_with_class(
                        "synthetic-acceptance",
                        Some(family),
                        Some(class.label()),
                        offset,
                        &c,
                    );
                    self.verifier
                        .record("synthetic-acceptance", family, class.label(), offset, &c);
                }
            }
        }
        for class in Class::HELD_OUT {
            // Which family's boundary this negative tests. `probes_family` is total over the
            // held-out set by construction (`crate::synth`), so a new generator cannot be added
            // without saying what it is a negative for.
            let probed = class
                .probes_family()
                .expect("every held-out generator probes a family");
            for &offset in [0.0_f64, 5.0, 10.0].iter() {
                let snr = 20.0 + offset;
                for _ in 0..self.grid.trials() {
                    seed += 1;
                    self.guard.check(seed)?;
                    let s = generate(*class, &SynthConfig::new(snr, seed));
                    let c = classify(&blind(&s, snr));
                    self.report.record("held-out", None, offset, &c);
                    self.verifier
                        .record("held-out", UNKNOWN, class.label(), offset, &c);
                    // Scored by the same rule as the aggregate (`family == unknown`), so these
                    // rows decompose the headline figure instead of restating it differently.
                    let tally = self.open_set.entry(probed).or_default();
                    tally.n += 1;
                    if c.family == UNKNOWN {
                        tally.unknown += 1;
                    }
                    tally.generators.insert(class.label());
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
        self.verifier.record(
            &format!("ota:{}", truth.session),
            &truth.label,
            "-",
            snr_db,
            c,
        );
    }

    /// The [`EvalReport`] accumulated so far (for a caller that wants T-199-style access
    /// alongside the harness, e.g. `worst_bin_wrong_rate`).
    pub fn eval(&self) -> &EvalReport {
        &self.report
    }

    /// Finishes the run: a serialisable [`Report`] with reproducibility metadata and per-family
    /// open-set coverage.
    pub fn finish(self) -> Report {
        let run = RunMeta::capture(self.grid, &self.guard);
        let coverage: Vec<FamilyCoverage> = self
            .open_set
            .iter()
            .map(|(family, t)| FamilyCoverage {
                family: (*family).to_owned(),
                n_ood: t.n,
                generators: t.generators.iter().map(|g| (*g).to_owned()).collect(),
                unknown_recall: Some(t.unknown as f64 / t.n as f64),
                false_known_rate: Some((t.n - t.unknown) as f64 / t.n as f64),
            })
            .collect();
        let mut report = Report::build(&self.report, run, coverage);
        report.verifier = self.verifier.rows();
        report
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
                features_version: FEATURES_VERSION,
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
        let report = Report::build(&r, run, vec![]);

        let json = serde_json::to_string_pretty(&report).expect("serialise");
        let back: Report = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back, report);

        let md = report.markdown();
        assert!(md.contains("## Summary"));
        assert!(md.contains("| source | family | SNR bin dB |"));
    }

    // -- Verifier run-rate (T-597) ---------------------------------------------------------------

    /// A classification stating no verifier outcome, or two, is a defect bucket that fails the
    /// check — never silently dropped from the count, which is how a skipped stage hides.
    #[test]
    fn a_classification_with_no_or_two_verifier_outcomes_is_counted_and_fails_the_check() {
        let mut ran = stub("psk-qam", &[("psk-qam", 0.9), (UNKNOWN, 0.1)]);
        ran.reasons = vec![crate::verify::VERIFIER_CONFIRMED.to_owned()];
        let mut skipped = ran.clone();
        skipped.reasons = vec![
            "below_class_gate".to_owned(),
            crate::verify::SkipReason::NoClassCall.as_str().to_owned(),
        ];
        let mut abstained = stub(UNKNOWN, &[(UNKNOWN, 0.9), ("fsk", 0.1)]);
        abstained.reasons = vec![
            "low_confidence".to_owned(),
            crate::verify::SkipReason::Abstained.as_str().to_owned(),
        ];
        assert_eq!(verifier_outcome(&ran), "verifier_confirmed");
        assert_eq!(verifier_outcome(&skipped), "verifier_no_class_call");

        let mut tally = VerifierTally::default();
        for c in [&ran, &skipped, &abstained] {
            tally.record("t", "psk-qam", "qpsk", 20.0, c);
        }
        let row = &tally.rows()[0];
        assert_eq!((row.n, row.ran), (3, 1));
        assert_eq!(row.run_rate(), Some(1.0 / 3.0));
        assert_eq!(row.abstained_because.get("low_confidence"), Some(&1));
        assert!(row.accounted(), "{row:?}");

        let mut silent = ran.clone();
        silent.reasons.clear();
        let mut both = ran.clone();
        both.reasons.push(
            crate::verify::SkipReason::SingleCandidate
                .as_str()
                .to_owned(),
        );
        assert_eq!(verifier_outcome(&silent), VERIFIER_UNSTATED);
        assert_eq!(verifier_outcome(&both), VERIFIER_MULTIPLE);
        tally.record("t", "psk-qam", "qpsk", 20.0, &silent);
        tally.record("t", "psk-qam", "qpsk", 20.0, &both);
        let row = &tally.rows()[0];
        assert_eq!(row.n, 5);
        assert_eq!(
            row.outcomes.values().sum::<usize>(),
            5,
            "parts must sum to n"
        );
        assert!(!row.accounted());

        let r = EvalReport::new(5.0);
        let run = RunMeta::capture(GridSize::Small, &SeedGuard::new(Split::Acceptance));
        let mut report = Report::build(&r, run, vec![]);
        report.verifier = tally.rows();
        let err = report.require_verifier_accounted().unwrap_err();
        assert_eq!(err.rows, vec!["t/psk-qam/qpsk@20".to_owned()]);
        let md = report.markdown();
        assert!(md.contains("## Post-sync verifier run-rate"), "{md}");
        let json = serde_json::to_string(&report).expect("serialise");
        let back: Report = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back, report);
    }

    // -- Open-set coverage (T-244) -------------------------------------------------------------

    /// **The defect this exists to prevent.** A family no out-of-taxonomy negative reached has no
    /// false-known rate and no unknown recall. Reporting `0.0` and `1.0` for it — which is what an
    /// average over an empty set, or an absent row, amounts to — publishes a perfect score for a
    /// test that never ran.
    #[test]
    fn a_family_with_no_negatives_is_flagged_unmeasured_and_fails_the_check() {
        let mut r = EvalReport::new(5.0);
        r.record(
            "held-out",
            None,
            20.0,
            &stub(UNKNOWN, &[(UNKNOWN, 0.9), ("fsk", 0.1)]),
        );
        let run = RunMeta::capture(GridSize::Small, &SeedGuard::new(Split::Acceptance));
        let measured_one = vec![FamilyCoverage {
            family: "fsk".to_owned(),
            n_ood: 1,
            generators: vec!["held-out:8fsk".to_owned()],
            unknown_recall: Some(1.0),
            false_known_rate: Some(0.0),
        }];
        let report = Report::build(&r, run.clone(), measured_one);

        // Every measurable family gets a row, whether or not it was measured: a gap is stated, not
        // left out.
        let listed: Vec<&str> = report.coverage.iter().map(|c| c.family.as_str()).collect();
        assert_eq!(listed, open_set_families());

        let gap = report
            .require_open_set_coverage()
            .expect_err("families with no negative must fail the check");
        assert!(gap.families.contains(&"analog".to_owned()), "{gap}");
        assert!(!gap.families.contains(&"fsk".to_owned()), "{gap}");
        assert_eq!(gap.families, report.summary.unmeasured_open_set_families);
        assert!(gap.to_string().contains("UNMEASURED"));

        // `null`, never `0.0`: the JSON cannot be read as a passing rate.
        let analog = report
            .coverage
            .iter()
            .find(|c| c.family == "analog")
            .expect("listed");
        assert_eq!((analog.n_ood, analog.unknown_recall), (0, None));
        assert_eq!(analog.false_known_rate, None);
        let json = serde_json::to_string(&report).expect("serialise");
        assert!(json.contains("\"unmeasured_open_set_families\":[\"analog\""));
        assert!(report.markdown().contains("| analog | 0 | UNMEASURED"));

        // A report built with no coverage at all fails closed, rather than passing by omission.
        let nothing = Report::build(&r, run, vec![]);
        assert_eq!(
            nothing
                .require_open_set_coverage()
                .unwrap_err()
                .families
                .len(),
            open_set_families().len()
        );
    }

    /// `code=count` pairs, short codes, zeros omitted: the shape the T-597 table is pinned in.
    fn breakdown(r: &VerifierRow) -> String {
        r.outcomes
            .iter()
            .filter(|(_, n)| **n > 0)
            .map(|(k, n)| format!("{}={n}", k.trim_start_matches("verifier_")))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// **How often the post-sync verifier actually runs, reported where a regression would be
    /// seen** (T-597). Shares the small-grid run below rather than paying for a second one.
    ///
    /// A stage that never fires is indistinguishable, in accuracy numbers, from one that fires and
    /// agrees. T-589 found the psk-qam verifier could not run on genuine 8-PSK at all (12 of 12)
    /// and nobody was told; T-321 and T-594 are the same shape. What this pins is the whole outcome
    /// breakdown, per truth family and SNR bin, for the two families with a likelihood model. A
    /// **falling `ran`** is the regression it exists to catch: a change that makes the tree more
    /// decisive silently removes the verifier from the shipped path (how T-246's fix came to be
    /// exercised only at the likelihood level). Any other move changes *why* the stage does not
    /// run and must be re-measured and explained, not just re-pinned. Counts, never wall-clock, and
    /// the parts must account for the whole.
    fn assert_verifier_run_rate(report: &Report) {
        // -- The parts account for the whole ------------------------------------------------------
        report
            .require_verifier_accounted()
            .expect("every classification states exactly one verifier outcome");
        // Every accuracy cell has a verifier row of the same size, and nothing else does: the run-rate
        // table covers exactly the classifications that were scored, no more and no fewer.
        let mut accuracy: BTreeMap<(String, String, i64), usize> = BTreeMap::new();
        for r in &report.rows {
            *accuracy
                .entry((r.source.clone(), r.family.clone(), r.snr_bin_db))
                .or_default() += r.n;
        }
        let mut verifier: BTreeMap<(String, String, i64), usize> = BTreeMap::new();
        for r in &report.verifier {
            assert_eq!(
                r.outcomes.values().sum::<usize>(),
                r.n,
                "{}/{}/{}@{}: outcomes {:?} do not sum to n",
                r.source,
                r.family,
                r.class,
                r.snr_bin_db,
                r.outcomes
            );
            *verifier
                .entry((r.source.clone(), r.family.clone(), r.snr_bin_db))
                .or_default() += r.n;
        }
        assert_eq!(
            verifier, accuracy,
            "verifier rows must cover exactly the scored classifications"
        );
        assert_eq!(
            verifier.values().sum::<usize>(),
            report.summary.n_total,
            "the verifier table must account for every classification in the run"
        );

        // -- The rate itself, per family and SNR, pinned ------------------------------------------
        //
        // (family, SNR bin relative to the family's gate, n, ran, outcome breakdown). Measured
        // 2026-09-23 on the small grid. Read it as: at the family gate the class gate withholds a class
        // call (`no_class_call`); five dB above it the tree is decisive (`single_candidate`) and the
        // stage does not run. On clean psk-qam it ran on 0 of 30 — correct behaviour (T-589), and the
        // exact number a more decisive tree would push further from the shipped path unnoticed —
        // until T-590 returned the QAM orders to its hypothesis set (3 of 30, below).
        const EXPECT: &[(&str, i64, usize, usize, &str)] = &[
            ("psk-qam", -5, 10, 0, "abstained_upstream=10"),
            ("psk-qam", 0, 10, 0, "no_class_call=10"),
            // T-590 re-pinned `ran 0, geometry=3` → `ran 3, confirmed=1 reranked=2`, and it is a
            // RISE, not a re-labelling of the same skips. Those three were the two `qam16` snippets
            // and one `qam64` whose tree prior split between the two QAM orders: with the orders
            // declined (T-422) fewer than two hypotheses were scored, and that surfaced as
            // `geometry`. With them restored the stage runs on all three; both `qam16` are reranked
            // and both land on the truth (class top-1 1.000, wrong 0.000 in this cell), and the
            // `qam64` is confirmed.
            (
                "psk-qam",
                5,
                10,
                3,
                "confirmed=1 no_clock_lock=1 reranked=2 single_candidate=6",
            ),
            ("fsk", -5, 8, 0, "abstained_upstream=8"),
            ("fsk", 0, 8, 0, "no_class_call=8"),
            // T-852 re-pinned `confirmed=1` → `reranked=1`: the run count is unchanged (1 of 8).
            // Widening the `2fsk` generator's modulation index (h up to 5, `synth::waveform`)
            // redrew which waveform each acceptance seed produces, so the one snippet the stage
            // runs on here is a different 2-FSK emission, and the verifier re-ranks its class
            // instead of agreeing.
            // T-888 re-pinned `reranked=1` → `confirmed=1`, run count again unchanged (1 of 8).
            // `blind_qpsk`/`blind_bpsk` are now absent where C14 cannot look past a lower-order
            // line, so the refitted fsk densities no longer carry the veto-floor spike those rows
            // read (0.200 / 0.300 exactly); the tree's own top class for that snippet is now the
            // one the verifier agrees with, rather than one it had to re-rank.
            (
                "fsk",
                5,
                8,
                1,
                "abstained_upstream=1 confirmed=1 single_candidate=6",
            ),
        ];
        let mut got: Vec<String> = Vec::new();
        let mut wrong: Vec<String> = Vec::new();
        for family in ["psk-qam", "fsk"] {
            let rows = report.verifier_by_snr("synthetic-acceptance", family);
            assert!(
                !rows.is_empty(),
                "{family}: no verifier rows — the stage's own family was never measured"
            );
            for r in rows {
                let line = format!(
                    "{family} {:>3} dB: n {:>2}, ran {} ({:.2}), {}",
                    r.snr_bin_db,
                    r.n,
                    r.ran,
                    r.run_rate().unwrap_or(0.0),
                    breakdown(&r)
                );
                got.push(line.clone());
                match EXPECT
                    .iter()
                    .find(|(f, bin, ..)| *f == family && *bin == r.snr_bin_db)
                {
                    Some(&(_, _, n, ran, outcomes)) => {
                        if r.ran < ran {
                            wrong.push(format!(
                                "{line}\n    RUN RATE FELL: ran {} < {ran} — the verifier has left the \
                                 shipped path for this cell",
                                r.ran
                            ));
                        } else if (r.n, r.ran, breakdown(&r).as_str()) != (n, ran, outcomes) {
                            wrong.push(format!("{line}\n    was: n {n}, ran {ran}, {outcomes}"));
                        }
                    }
                    None => wrong.push(format!("{line}\n    (no pinned row)")),
                }
            }
        }
        println!("T-597 verifier run-rate, small grid:\n{}", got.join("\n"));
        assert_eq!(
            got.len(),
            EXPECT.len(),
            "every pinned (family, SNR) cell must have been measured: {got:#?}"
        );
        assert!(
            wrong.is_empty(),
            "the post-sync verifier's run-rate moved. A fall means a stage silently left the shipped \
             path; any other move changes why it does not run and must be re-measured and explained \
             before re-pinning:\n{}",
            wrong.join("\n")
        );
    }

    /// The runtime half: a real grid run reaches every family, so the loud check passes because
    /// the negatives exist and not because nothing was looked for.
    #[test]
    fn a_real_harness_run_measures_the_open_set_of_every_family() {
        use crate::{Classifier, ClassifyRequest, SymbolEstimator};

        let classifier = Classifier::new();
        let mut c14 = SymbolEstimator::new();
        let mut harness = Harness::new(GridSize::Small);
        harness
            .run_synthetic(|s| {
                let symbols = c14.from_samples(
                    s.symbol_samples,
                    s.symbol_sample_rate_hz,
                    s.obw_hz,
                    s.snr_db,
                );
                let mut req =
                    ClassifyRequest::new(s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
                req.obw_hz = s.obw_hz;
                req.snr_db = s.snr_db;
                req.symbols = symbols.as_ref();
                req.symbol_samples = Some(s.symbol_samples);
                req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);
                classifier.classify(&req)
            })
            .expect("seed separation held");
        let report = harness.finish();
        assert_verifier_run_rate(&report);

        report
            .require_open_set_coverage()
            .expect("every family must have an out-of-taxonomy negative");
        assert!(report.summary.unmeasured_open_set_families.is_empty());
        for c in &report.coverage {
            assert!(c.measured() && !c.generators.is_empty(), "{}", c.family);
            assert!(c.unknown_recall.is_some(), "{}", c.family);
        }

        // The per-family rows **decompose** the headline figure rather than restating it by some
        // other rule: same snippets, same split.
        let n_ood: usize = report.coverage.iter().map(|c| c.n_ood).sum();
        let n_held_out: usize = report
            .rows
            .iter()
            .filter(|r| r.source == "held-out")
            .map(|r| r.n)
            .sum();
        assert_eq!(n_ood, n_held_out);
        let unknown: f64 = report
            .coverage
            .iter()
            .map(|c| c.unknown_recall.unwrap_or(0.0) * c.n_ood as f64)
            .sum();
        assert!(
            (unknown / n_ood as f64 - report.summary.held_out_unknown_recall).abs() < 1e-9,
            "per-family coverage does not sum to the aggregate"
        );
    }
}
