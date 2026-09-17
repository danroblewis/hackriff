//! **T-364: the burst-recall / open-set trade, measured across seed bases rather than once.**
//!
//! ```text
//! cargo run --release -p hk-classify --bin t364-curves -- --bases 8 --out t364-max.json
//! cargo run --release -p hk-classify --bin t364-curves --features cyclic-dims -- --bases 8 --out t364-four.json
//! ```
//!
//! # What this measures, and why it needed its own harness
//!
//! T-328 measured, once, that **the shipped classifier rejects 39.4 % of genuine N/8 bursts from
//! their own class** against 11.9 % of the same classes' full windows, and that fitting the
//! densities over **pooled window lengths** (N, N/2, N/4, N/8 in one fit) nearly removes that at a
//! cost in held-out unknown recall — quoted as `0.9606 → 0.9000` with the shipped single
//! `cyclic_db` dimension and `0.9697 → 0.9242` with T-310/T-328's four-dimension expansion.
//! Those are single draws quoted to four decimals.
//!
//! T-428 then established (ADR-0016 §7.2) that the held-out unknown-recall figure has a **draw
//! standard deviation of 0.0084**: the gate's own draw, re-run at eight seed bases on identical
//! code, gives 0.9343…0.9621. A four-decimal comparison against a 0.90 floor is therefore not a
//! comparison. This binary re-measures **both** sides of the trade — the burst side as well as the
//! open-set side, because a single-draw burst number is no more trustworthy — over N seed bases,
//! and reports a mean and a spread at every point.
//!
//! # Populations (ADR-0016 §7.1 requires every figure to name one)
//!
//! | Figure | Population |
//! |---|---|
//! | `burst_*` / `full_*` | 21 taxonomy classes × gate+{0,5,10,15} dB × [`SEEDS_PER_CELL`] **dev** seeds, disjoint from the fitting seeds: T-328's n = 840. |
//! | `held_out` | **the gate's draw**: [`Class::HELD_OUT`] (11 generators) × 20/25/30 dB × [`HELD_OUT_TRIALS`] = 396, seeds `ACCEPTANCE_SEED_BASE + base…`, the construction of `m3_grid.rs::held_out`, which is the population ADR-0016 §7 row 4 rules on. Base `k` here is `ACCEPTANCE_SEED_BASE + k·100 000`, so base 6 **is** the gate's own seed range and the eight bases are exactly T-428's eight. It is **not** the harness's held-out reading (§7.2 row 2) and not the six-generator subset (§7.2 row 3). |
//! | `held_out_burst` | as `held_out`, scored on truncated records: how the open set behaves on ephemeral emissions, which is where the fix is meant to earn its keep. |
//!
//! A seed **base** moves the evaluation draw only. The fit always uses the shipped protocol's dev
//! seeds, because the question is how the decision rule behaves, not how the fit varies.
//!
//! # Two truncation protocols, because they are different questions
//!
//! - [`Trunc::Both`] — the **emission** is short: the classifier's snippet *and* C14's
//!   symbol-geometry view of it are both 1/8 of the record. This is what the pipeline hands the
//!   classifier for a genuine burst, so it is the product-facing number.
//! - [`Trunc::C14`] — only C14's window is short, as `tests/cyclic_line_window.rs::estimate` does
//!   it, and as T-328's own measurement did. Reported so T-328's 39.4 % can be compared with
//!   something measured the same way.
//!
//! # Arms
//!
//! - `builtin` — the shipped `data/densities-1.json` / `densities-below-gate-1.json`, untouched.
//! - `full` — **both** models refitted here at full window under the shipped protocol. Present to
//!   show the harness reproduces the shipped models, so a difference in a pooled arm is the
//!   protocol and not the harness.
//! - `pooled-c14` — both models refitted over N, N/2, N/4, N/8 **of C14's window**, the snippet
//!   left full length. This is the protocol T-328 priced.
//! - `pooled-both` — both models refitted over N…N/8 of the snippet **and** C14's window together:
//!   the fit sees what a genuinely short emission looks like in every dimension.
//!
//! With `--features cyclic-dims` the four line significances are carried as four dimensions in
//! place of their max (T-310's expansion, refused by T-328) and the same arms are measured over
//! that feature set. That feature is off by default and a default build is unaffected.
//!
//! Nothing here ships and no floor moves: this binary writes a JSON report and prints a table.

use std::collections::BTreeMap;
use std::sync::Mutex;

use hk_classify::density::DensityModel;
use hk_classify::features::{FeatureInput, Features, features};
use hk_classify::harness::{SeedGuard, Split};
use hk_classify::symbols::SymbolEstimator;
use hk_classify::synth::{ACCEPTANCE_SEED_BASE, Class, DEV_SEEDS, SynthConfig, generate};
use hk_classify::thresholds::thresholds_of;
use hk_classify::{Classifier, ClassifyRequest};
use hk_model::Timestamp;
use hk_model::classify::{Classification, UNKNOWN};

/// Dev seeds per class the shipped fitter uses (`bin/fit-densities.rs`). Mirrored, not imported,
/// because that binary is not a library.
const FIT_SEEDS_PER_CLASS: u64 = 60;

/// SNR offsets above each family's gate, dB — `fit-densities`'s `ABOVE_GATE`, and the burst grid.
const ABOVE_GATE: [f64; 4] = [0.0, 5.0, 10.0, 15.0];

/// SNR offsets below each family's gate, dB — `fit-densities`'s `BELOW_GATE`.
const BELOW_GATE: [f64; 4] = [-10.0, -7.5, -5.0, -2.5];

/// The three fitting protocols, as `(snippet denominator, C14-window denominator)` pairs.
///
/// **There are two ways to pool window lengths and they are not the same experiment.** T-328
/// truncated C14's window alone (`tests/cyclic_line_window.rs::estimate`), which widens only the
/// symbol-derived dimensions — the ones it identified as the cause. Truncating the classifier's
/// snippet too widens all ~27, which is what a genuinely short emission does to them. Both are
/// measured, because the price differs by far more than the draw spread and quoting one as "the
/// pooled fit" would hide that.
const PROTOCOLS: [(&str, &[(usize, usize)]); 3] = [
    ("full", &[(1, 1)]),
    ("pooled-c14", &[(1, 1), (1, 2), (1, 4), (1, 8)]),
    ("pooled-both", &[(1, 1), (2, 2), (4, 4), (8, 8)]),
];

/// Every `(snippet, C14)` pair any protocol needs, so one generation pass serves all three.
const ALL_PAIRS: [(usize, usize); 7] = [(1, 1), (1, 2), (1, 4), (1, 8), (2, 2), (4, 4), (8, 8)];

/// The burst fraction the capability gap is stated over (T-328).
const BURST_DENOM: usize = 8;

/// Evaluation seeds per (class, SNR) cell. 21 × 4 × 10 = 840, T-328's n.
const SEEDS_PER_CELL: u64 = 10;

/// First evaluation seed: past the fitting seeds, inside [`DEV_SEEDS`].
const EVAL_SEED_START: u64 = DEV_SEEDS.start + FIT_SEEDS_PER_CLASS;

/// Trials per held-out (generator, SNR) cell — `m3_grid.rs::HELD_OUT_TRIALS`.
const HELD_OUT_TRIALS: u32 = 12;

/// Held-out SNRs, dB — `m3_grid.rs::held_out`.
const HELD_OUT_SNRS: [f64; 3] = [20.0, 25.0, 30.0];

/// ADR-0016 §7 row 4.
const UNKNOWN_RECALL_FLOOR: f64 = 0.90;

/// Which views of the record a short burst shortens.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Trunc {
    /// Neither: the full record.
    None,
    /// C14's symbol-geometry window only, as `cyclic_line_window.rs` and T-328 did it.
    C14,
    /// The classifier's snippet and C14's window together: a genuinely short emission.
    Both,
}

impl Trunc {
    fn label(self) -> &'static str {
        match self {
            Trunc::None => "full",
            Trunc::C14 => "burst(C14)",
            Trunc::Both => "burst(both)",
        }
    }
    fn denoms(self, den: usize) -> (usize, usize) {
        match self {
            Trunc::None => (1, 1),
            Trunc::C14 => (1, den),
            Trunc::Both => (den, den),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------------------------

/// One classification of one generated class, truncated per `trunc`.
fn classify_at(
    classifier: &Classifier,
    c14: &mut SymbolEstimator,
    class: Class,
    snr_db: f64,
    seed: u64,
    trunc: Trunc,
) -> Classification {
    let s = generate(class, &SynthConfig::new(snr_db, seed));
    let (dn, dm) = trunc.denoms(BURST_DENOM);
    let n = (s.samples.len() / dn).max(1);
    let m = (s.symbol_samples.len() / dm).max(1);
    let symbols = c14.from_samples(
        &s.symbol_samples[..m],
        s.symbol_sample_rate_hz,
        Some(s.obw_hz),
        Some(snr_db),
    );
    let mut req = ClassifyRequest::new(&s.samples[..n], s.sample_rate_hz, Timestamp::UNIX_EPOCH);
    req.obw_hz = Some(s.obw_hz);
    req.snr_db = Some(snr_db);
    req.symbols = symbols.as_ref();
    req.symbol_samples = Some(&s.symbol_samples[..m]);
    req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);
    classifier.classify(&req)
}

/// Counts for one (arm, base, truncation) cell.
#[derive(Clone, Copy, Default, Debug)]
struct Counts {
    n: u32,
    unknown: u32,
    right_family: u32,
    right_class: u32,
}

impl Counts {
    fn frac(v: u32, n: u32) -> f64 {
        if n == 0 {
            0.0
        } else {
            f64::from(v) / f64::from(n)
        }
    }
    fn unknown_rate(&self) -> f64 {
        Self::frac(self.unknown, self.n)
    }
    fn family_rate(&self) -> f64 {
        Self::frac(self.right_family, self.n)
    }
    fn class_rate(&self) -> f64 {
        Self::frac(self.right_class, self.n)
    }
}

/// The taxonomy arm: how often a class's **own** snippet comes back `unknown`, overall and per SNR.
fn taxonomy_recall(
    classifier: &Classifier,
    guard: &Mutex<SeedGuard>,
    base: u64,
    trunc: Trunc,
) -> (Counts, BTreeMap<String, Counts>) {
    let mut c14 = SymbolEstimator::new();
    let mut total = Counts::default();
    let mut per_snr: BTreeMap<String, Counts> = BTreeMap::new();
    for class in Class::TAXONOMY {
        let family = class.family().expect("a taxonomy class has a family");
        let gate = thresholds_of(family)
            .and_then(|t| t.snr_gate_db)
            .unwrap_or(10.0);
        for step in ABOVE_GATE {
            let cell = per_snr.entry(format!("gate{step:+.0}")).or_default();
            for i in 0..SEEDS_PER_CELL {
                let seed = EVAL_SEED_START + base * SEEDS_PER_CELL + i;
                guard.lock().expect("seed guard").require(seed);
                let c = classify_at(classifier, &mut c14, *class, gate + step, seed, trunc);
                let right_family = c.family == family;
                let right_class = c.class.as_ref().map(|k| k.label.as_str()) == Some(class.label());
                for t in [&mut total, &mut *cell] {
                    t.n += 1;
                    t.unknown += u32::from(c.family == UNKNOWN);
                    t.right_family += u32::from(right_family);
                    t.right_class += u32::from(right_class);
                }
            }
        }
    }
    (total, per_snr)
}

/// The open-set arm: `m3_grid.rs::held_out`, re-run at an arbitrary seed base and truncation.
///
/// Reproduces the gate's predicate exactly — `family == unknown || open_set_score >= 0.5`,
/// ADR-0016 §7 row 4 — and the gate's population, 11 generators × 3 SNRs × 12 trials = 396.
fn held_out_recall(
    classifier: &Classifier,
    seed_base: u64,
    trunc: Trunc,
) -> (Counts, BTreeMap<String, Counts>) {
    let mut c14 = SymbolEstimator::new();
    let mut seed = ACCEPTANCE_SEED_BASE + seed_base;
    let mut total = Counts::default();
    let mut per_snr: BTreeMap<String, Counts> = BTreeMap::new();
    for class in Class::HELD_OUT {
        for snr in HELD_OUT_SNRS {
            let cell = per_snr.entry(format!("{snr:.0} dB")).or_default();
            for _ in 0..HELD_OUT_TRIALS {
                seed += 1;
                let c = classify_at(classifier, &mut c14, *class, snr, seed, trunc);
                let hit = c.family == UNKNOWN || c.open_set_score >= 0.5;
                for t in [&mut total, &mut *cell] {
                    t.n += 1;
                    t.unknown += u32::from(hit);
                }
            }
        }
    }
    (total, per_snr)
}

// ---------------------------------------------------------------------------------------------
// Fitting
// ---------------------------------------------------------------------------------------------

/// One labelled fitting row, tagged with the `(snippet, C14)` truncation it was measured at.
type Row = ((usize, usize), (String, String, Features));

/// Measures every [`ALL_PAIRS`] truncation of the fitting grid once, in parallel over the classes.
///
/// One generation pass serves all three protocols, because generating the waveform dominates and
/// the three fits differ only in which truncations they keep.
fn fit_rows(guard: &Mutex<SeedGuard>, steps: &[f64], threads: usize) -> Vec<Row> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let out: Mutex<Vec<Row>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                let mut c14 = SymbolEstimator::new();
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(class) = Class::TAXONOMY.get(i) else {
                        return;
                    };
                    let family = class.family().expect("a taxonomy class has a family");
                    let gate = thresholds_of(family)
                        .and_then(|t| t.snr_gate_db)
                        .unwrap_or(10.0);
                    let mut mine: Vec<Row> = Vec::new();
                    for step in steps {
                        let snr = gate + step;
                        for seed in DEV_SEEDS.start..(DEV_SEEDS.start + FIT_SEEDS_PER_CLASS) {
                            guard.lock().expect("seed guard").require(seed);
                            let s = generate(*class, &SynthConfig::new(snr, seed));
                            for (dn, dm) in ALL_PAIRS {
                                let n = (s.samples.len() / dn).max(1);
                                let m = (s.symbol_samples.len() / dm).max(1);
                                let symbols = c14.from_samples(
                                    &s.symbol_samples[..m],
                                    s.symbol_sample_rate_hz,
                                    Some(s.obw_hz),
                                    Some(snr),
                                );
                                let f = features(&FeatureInput {
                                    samples: &s.samples[..n],
                                    sample_rate_hz: s.sample_rate_hz,
                                    obw_hz: Some(s.obw_hz),
                                    snr_db: Some(snr),
                                    symbols: symbols.as_ref(),
                                });
                                mine.push((
                                    (dn, dm),
                                    (class.label().to_owned(), family.to_owned(), f),
                                ));
                            }
                        }
                    }
                    out.lock().expect("rows").extend(mine);
                }
            });
        }
    });
    out.into_inner().expect("rows")
}

/// Fits one model from the rows belonging to one protocol.
fn fit_from(rows: &[Row], pairs: &[(usize, usize)], label: &str) -> DensityModel {
    let labelled: Vec<(String, String, Features)> = rows
        .iter()
        .filter(|(p, _)| pairs.contains(p))
        .map(|(_, r)| r.clone())
        .collect();
    assert!(!labelled.is_empty(), "{label}: no rows for {pairs:?}");
    let model = DensityModel::fit(
        &labelled,
        &format!(
            "T-364 {label}: dev seeds {}..{}, (snippet, C14) truncations {pairs:?}",
            DEV_SEEDS.start,
            DEV_SEEDS.start + FIT_SEEDS_PER_CLASS
        ),
    );
    model.validate().expect("fitted model is valid");
    model
}

// ---------------------------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------------------------

/// Mean, sd and range of a set of draws of one quantity.
struct Spread {
    mean: f64,
    sd: f64,
    min: f64,
    max: f64,
    draws: Vec<f64>,
}

impl Spread {
    fn of(draws: Vec<f64>) -> Self {
        let n = draws.len().max(1) as f64;
        let mean = draws.iter().sum::<f64>() / n;
        let sd = if draws.len() > 1 {
            (draws.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
        } else {
            f64::NAN
        };
        Self {
            mean,
            sd,
            min: draws.iter().copied().fold(f64::INFINITY, f64::min),
            max: draws.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            draws,
        }
    }
    /// Standard error of the mean over the bases.
    fn sem(&self) -> f64 {
        self.sd / (self.draws.len() as f64).sqrt()
    }
    fn cell(&self) -> String {
        format!(
            "{:.4} ±{:.4} [{:.4},{:.4}]",
            self.mean, self.sd, self.min, self.max
        )
    }
    fn json(&self) -> String {
        let num = |x: f64| {
            if x.is_finite() {
                format!("{x:.6}")
            } else {
                "null".into()
            }
        };
        format!(
            "{{\"mean\":{},\"sd\":{},\"sem\":{},\"min\":{},\"max\":{},\"draws\":[{}]}}",
            num(self.mean),
            num(self.sd),
            num(self.sem()),
            num(self.min),
            num(self.max),
            self.draws
                .iter()
                .map(|d| num(*d))
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

/// Everything measured for one arm.
struct Arm {
    name: &'static str,
    fitted_on: String,
    /// Keyed by [`Trunc::label`].
    unknown: BTreeMap<&'static str, Spread>,
    family: BTreeMap<&'static str, Spread>,
    class: BTreeMap<&'static str, Spread>,
    unknown_by_snr: BTreeMap<&'static str, BTreeMap<String, Spread>>,
    held_out: BTreeMap<&'static str, Spread>,
    held_by_snr: BTreeMap<&'static str, BTreeMap<String, Spread>>,
}

fn spread_by_key(
    rows: &[BTreeMap<String, Counts>],
    f: fn(&Counts) -> f64,
) -> BTreeMap<String, Spread> {
    let mut keys: Vec<String> = Vec::new();
    for r in rows {
        for k in r.keys() {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    keys.into_iter()
        .map(|k| {
            let draws = rows.iter().filter_map(|r| r.get(&k)).map(f).collect();
            (k, Spread::of(draws))
        })
        .collect()
}

/// One base's worth of every cell.
type BaseRow = Vec<(
    Trunc,
    Counts,
    BTreeMap<String, Counts>,
    Counts,
    BTreeMap<String, Counts>,
)>;

fn measure_arm(
    name: &'static str,
    classifier: &Classifier,
    guard: &Mutex<SeedGuard>,
    bases: u64,
    threads: usize,
) -> Arm {
    let truncs = [Trunc::None, Trunc::C14, Trunc::Both];
    let next = std::sync::atomic::AtomicU64::new(0);
    let rows: Mutex<Vec<(u64, BaseRow)>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                loop {
                    let base = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if base >= bases {
                        return;
                    }
                    let mut row: BaseRow = Vec::new();
                    for t in truncs {
                        let (c, by) = taxonomy_recall(classifier, guard, base, t);
                        let (h, hby) = held_out_recall(classifier, (base + 1) * 100_000, t);
                        row.push((t, c, by, h, hby));
                    }
                    eprintln!(
                        "  {name} base {base}: {}",
                        row.iter()
                            .map(|(t, c, _, h, _)| format!(
                                "{} unknown {:.4} / held-out {}/{} {:.4}",
                                t.label(),
                                c.unknown_rate(),
                                h.unknown,
                                h.n,
                                h.unknown_rate()
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    );
                    rows.lock().expect("rows").push((base, row));
                }
            });
        }
    });
    let mut rows = rows.into_inner().expect("rows");
    rows.sort_by_key(|(b, _)| *b);
    let rows: Vec<BaseRow> = rows.into_iter().map(|(_, r)| r).collect();

    let mut arm = Arm {
        name,
        fitted_on: classifier.model().fitted_on.clone(),
        unknown: BTreeMap::new(),
        family: BTreeMap::new(),
        class: BTreeMap::new(),
        unknown_by_snr: BTreeMap::new(),
        held_out: BTreeMap::new(),
        held_by_snr: BTreeMap::new(),
    };
    for (i, t) in truncs.iter().enumerate() {
        let cs: Vec<Counts> = rows.iter().map(|r| r[i].1).collect();
        let by: Vec<BTreeMap<String, Counts>> = rows.iter().map(|r| r[i].2.clone()).collect();
        let hs: Vec<Counts> = rows.iter().map(|r| r[i].3).collect();
        let hby: Vec<BTreeMap<String, Counts>> = rows.iter().map(|r| r[i].4.clone()).collect();
        let sp = |v: &[Counts], f: fn(&Counts) -> f64| Spread::of(v.iter().map(f).collect());
        arm.unknown.insert(t.label(), sp(&cs, Counts::unknown_rate));
        arm.family.insert(t.label(), sp(&cs, Counts::family_rate));
        arm.class.insert(t.label(), sp(&cs, Counts::class_rate));
        arm.unknown_by_snr
            .insert(t.label(), spread_by_key(&by, Counts::unknown_rate));
        arm.held_out
            .insert(t.label(), sp(&hs, Counts::unknown_rate));
        arm.held_by_snr
            .insert(t.label(), spread_by_key(&hby, Counts::unknown_rate));
    }
    arm
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let bases: u64 = arg("--bases").and_then(|v| v.parse().ok()).unwrap_or(8);
    let threads: usize = arg("--threads").and_then(|v| v.parse().ok()).unwrap_or(6);
    let out = arg("--out");

    let dims = if cfg!(feature = "cyclic-dims") {
        "four cyclic dimensions (T-310/T-328 expansion)"
    } else {
        "single `cyclic_db` max (shipped)"
    };
    eprintln!("T-364 curves: {bases} seed bases, {threads} threads, feature set = {dims}");

    // Fitting and the taxonomy arm touch only dev seeds. The held-out arm is on acceptance seeds
    // by construction (it is the gate's own draw) and is not guarded here.
    let guard = Mutex::new(SeedGuard::new(Split::Dev));

    let t0 = std::time::Instant::now();
    eprintln!("measuring the fitting grid at every truncation …");
    let above = fit_rows(&guard, &ABOVE_GATE, threads);
    let below = fit_rows(&guard, &BELOW_GATE, threads);
    eprintln!(
        "fitting grid measured in {:.0} s ({} above-gate + {} below-gate rows)",
        t0.elapsed().as_secs_f64(),
        above.len(),
        below.len()
    );
    let fitted: Vec<(&'static str, Classifier)> = PROTOCOLS
        .iter()
        .map(|(name, pairs)| {
            eprintln!("fitting `{name}` over {pairs:?} …");
            (
                *name,
                Classifier::with_models(
                    fit_from(&above, pairs, "above-gate"),
                    fit_from(&below, pairs, "below-gate"),
                ),
            )
        })
        .collect();

    let mut arms = Vec::new();
    // The shipped densities are only meaningful for the default feature set: with `cyclic-dims`
    // on, `cyclic_db` is never measured, so the builtin model would score on an absent dimension.
    if !cfg!(feature = "cyclic-dims") {
        arms.push(measure_arm(
            "builtin",
            &Classifier::new(),
            &guard,
            bases,
            threads,
        ));
    }
    for (name, c) in &fitted {
        arms.push(measure_arm(name, c, &guard, bases, threads));
    }

    let truncs = ["full", "burst(C14)", "burst(both)"];
    println!("\n=== T-364: the trade over {bases} seed bases — mean ±sd [min, max] ===");
    println!("feature set: {dims}\n");

    println!("-- a class's OWN snippet called `unknown` (the capability side; lower is better) --");
    println!(
        "{:<9} {:^26} {:^26} {:^26}",
        "arm", truncs[0], truncs[1], truncs[2]
    );
    for a in &arms {
        println!(
            "{:<9} {:^26} {:^26} {:^26}",
            a.name,
            a.unknown[truncs[0]].cell(),
            a.unknown[truncs[1]].cell(),
            a.unknown[truncs[2]].cell()
        );
    }

    println!(
        "\n-- held-out unknown recall, the gate's draw (the open-set side; floor {UNKNOWN_RECALL_FLOOR}) --"
    );
    println!(
        "{:<9} {:^26} {:^26} {:^26}",
        "arm", truncs[0], truncs[1], truncs[2]
    );
    for a in &arms {
        println!(
            "{:<9} {:^26} {:^26} {:^26}",
            a.name,
            a.held_out[truncs[0]].cell(),
            a.held_out[truncs[1]].cell(),
            a.held_out[truncs[2]].cell()
        );
    }

    println!(
        "\n-- distinguishability from the {UNKNOWN_RECALL_FLOOR} floor, full-window held-out --"
    );
    println!(
        "{:<9} {:>8} {:>8} {:>8} {:>10} {:>28}",
        "arm", "mean", "sd", "sem", "(m-f)/sem", "verdict"
    );
    for a in &arms {
        let s = &a.held_out["full"];
        let z = (s.mean - UNKNOWN_RECALL_FLOOR) / s.sem();
        let verdict = if !z.is_finite() {
            "one draw: not measurable"
        } else if z >= 2.0 {
            "above the floor"
        } else if z <= -2.0 {
            "below the floor"
        } else {
            "NOT distinguishable"
        };
        println!(
            "{:<9} {:>8.4} {:>8.4} {:>8.4} {:>10.2} {:>28}",
            a.name,
            s.mean,
            s.sd,
            s.sem(),
            z,
            verdict
        );
    }

    println!("\n-- known-side accuracy (the rest of the price) --");
    println!(
        "{:<9} {:^26} {:^26} {:^26}",
        "arm", "full right family", "full right class", "burst(both) right family"
    );
    for a in &arms {
        println!(
            "{:<9} {:^26} {:^26} {:^26}",
            a.name,
            a.family["full"].cell(),
            a.class["full"].cell(),
            a.family["burst(both)"].cell()
        );
    }

    for a in &arms {
        for t in truncs {
            println!(
                "\n{} / {t} — own snippet -> `unknown`, by SNR offset:",
                a.name
            );
            for (k, s) in &a.unknown_by_snr[t] {
                println!("  {k:<10} {}", s.cell());
            }
            println!("{} / {t} — held-out recall, by SNR:", a.name);
            for (k, s) in &a.held_by_snr[t] {
                println!("  {k:<10} {}", s.cell());
            }
        }
    }

    if let Some(path) = out {
        let by = |m: &BTreeMap<&'static str, Spread>| {
            m.iter()
                .map(|(k, s)| format!("\"{k}\":{}", s.json()))
                .collect::<Vec<_>>()
                .join(",")
        };
        let by2 = |m: &BTreeMap<&'static str, BTreeMap<String, Spread>>| {
            m.iter()
                .map(|(k, inner)| {
                    let rows = inner
                        .iter()
                        .map(|(k2, s)| format!("\"{k2}\":{}", s.json()))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("\"{k}\":{{{rows}}}")
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        let arms_json: Vec<String> = arms
            .iter()
            .map(|a| {
                format!(
                    "{{\"arm\":\"{}\",\"fitted_on\":{},\"unknown\":{{{}}},\"family\":{{{}}},\
                     \"class\":{{{}}},\"held_out\":{{{}}},\"unknown_by_snr\":{{{}}},\
                     \"held_by_snr\":{{{}}}}}",
                    a.name,
                    serde_json::to_string(&a.fitted_on).expect("string"),
                    by(&a.unknown),
                    by(&a.family),
                    by(&a.class),
                    by(&a.held_out),
                    by2(&a.unknown_by_snr),
                    by2(&a.held_by_snr),
                )
            })
            .collect();
        let doc = format!(
            "{{\"task\":\"T-364\",\"bases\":{bases},\"feature_set\":\"{dims}\",\
             \"floor\":{UNKNOWN_RECALL_FLOOR},\"arms\":[{}]}}",
            arms_json.join(",")
        );
        std::fs::write(&path, doc).expect("write report");
        eprintln!("wrote {path}");
    }
}
