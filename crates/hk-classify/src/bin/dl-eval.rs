//! Measures the per-family DL stage against the classical cascade on the **dev holdout**, and
//! applies ADR-0016 §4.6's enable rule (T-204).
//!
//! ```text
//! cargo run -p hk-classify --release --features dl-mlp --bin dl-eval -- <models-dir> <out-dir>
//! ```
//!
//! `models-dir` holds `<family>/{model.json,manifest.json}` as the trainer wrote them.
//!
//! # What is compared, and against what rule
//!
//! Both stages are scored on the **same snippets**: dev-holdout waveforms whose truth family the
//! classical cascade actually named (the only snippets a within-family model ever sees in
//! production). Classical class accuracy is its `ClassCall`; a classical abstention counts as not
//! correct, and is reported separately so the two effects can be told apart.
//!
//! The **a-priori** enable rule, fixed in ADR-0016 §4.6 before any measurement:
//! - class accuracy of DL − classical **≥ 5 points at every SNR bin at or above the family gate**;
//! - bootstrap 95 % CI lower bound of that difference **> 0**;
//! - open-set AUROC **not lower by more than 0.02**;
//! - false-known rate on held-out classes **≤ classical**.
//!
//! Anything short of all four leaves the family in shadow. Nothing here tunes a threshold to make
//! a family pass: the rule is read, not fitted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hk_classify::dl::DlStage;
use hk_classify::harness::{SeedGuard, Split};
use hk_classify::symbols::SymbolEstimator;
use hk_classify::synth::{Class, SynthConfig, generate};
use hk_classify::thresholds::thresholds_of;
use hk_classify::{Classifier, ClassifyRequest};
use hk_ml::{MlMode, MlProvider, ModelManifest};
use hk_model::Timestamp;
use serde::Serialize;

/// Must match `dl-train-export`'s holdout range: the model was calibrated on these seeds and
/// trained on the earlier ones, so scoring here is out-of-sample for the weights.
const HOLDOUT_BASE: u64 = 300;
const OFFSETS: [f64; 4] = [0.0, 5.0, 10.0, 15.0];

/// ADR-0016 §4.6, a-priori.
const MARGIN_POINTS: f64 = 0.05;
const MAX_AUROC_LOSS: f64 = 0.02;

/// One snippet's paired outcome: what each stage said about the same waveform.
#[derive(Clone, Copy, Debug)]
struct Outcome {
    /// The classical cascade named the right class.
    classical: bool,
    /// The model's argmax was the right class.
    dl: bool,
    /// As `dl`, but only counted when the model's open set accepted the input.
    dl_gated: bool,
    /// The classical cascade named no class at all (below its class gate).
    classical_abstained: bool,
}

#[derive(Clone, Debug, Serialize)]
struct BinResult {
    family: String,
    offset_db: f64,
    n: usize,
    classical_top1: f64,
    classical_abstained: f64,
    dl_top1: f64,
    dl_top1_gated: f64,
    delta: f64,
    ci_lo: f64,
    ci_hi: f64,
}

#[derive(Clone, Debug, Serialize)]
struct FamilyResult {
    family: String,
    model: String,
    n_id: usize,
    n_ood: usize,
    classical_auroc: f64,
    dl_auroc: f64,
    classical_false_known: f64,
    dl_false_known: f64,
    worst_delta: f64,
    worst_ci_lo: f64,
    /// The worst-bin gain over bins where the classical stage was **above its class gate** (it
    /// named a class in at least half the snippets). Below the class gate the cascade abstains by
    /// design, so a gain there is a gain over a deliberate silence, not over a decision.
    worst_delta_above_class_gate: f64,
    enable: bool,
    why: Vec<String>,
    notes: Vec<String>,
}

#[derive(Serialize)]
struct Report {
    rule: String,
    margin_points: f64,
    bins: Vec<BinResult>,
    families: Vec<FamilyResult>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let models_dir = args.next().map(PathBuf::from).unwrap_or_else(|| {
        eprintln!("usage: dl-eval <models-dir> <out-dir>");
        std::process::exit(2);
    });
    let out = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&out).expect("create output dir");

    let mut stage = DlStage::new();
    let mut manifests: BTreeMap<String, ModelManifest> = BTreeMap::new();
    for entry in std::fs::read_dir(&models_dir)
        .expect("read models dir")
        .flatten()
    {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some((family, manifest, model)) = load(&dir) else {
            continue;
        };
        stage
            .add(&family, model, MlMode::Shadow)
            .expect("a shadow model loads");
        manifests.insert(family, manifest);
    }
    assert!(
        !stage.is_empty(),
        "no models loaded from {}",
        models_dir.display()
    );

    let classifier = Classifier::new();
    let mut c14 = SymbolEstimator::new();
    // Dev seeds only: the holdout is dev, and the acceptance grid is never touched here.
    let mut guard = SeedGuard::new(Split::Dev);

    // Per (family, offset): paired outcomes, one per snippet the stage actually ran on.
    let mut paired: BTreeMap<(String, i64), Vec<Outcome>> = BTreeMap::new();
    // Per family: in-distribution and out-of-distribution open-set scores, both stages.
    let mut id_scores: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    let mut ood_scores: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();

    let n_seeds: u64 = std::env::var("HK_DL_EVAL_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);

    for class in Class::TAXONOMY.iter().chain(Class::HELD_OUT) {
        let truth_family = class.family();
        let routed = truth_family.or_else(|| class.nearest_family());
        let Some(routed) = routed else { continue };
        if !stage.families().any(|f| f == routed) {
            continue;
        }
        let gate = thresholds_of(routed)
            .and_then(|t| t.snr_gate_db)
            .unwrap_or(10.0);
        for offset in OFFSETS {
            let snr = gate + offset;
            for seed in HOLDOUT_BASE..(HOLDOUT_BASE + n_seeds) {
                guard.require(seed);
                let s = generate(*class, &SynthConfig::new(snr, seed));
                let symbols = c14.from_samples(
                    &s.symbol_samples,
                    s.symbol_sample_rate_hz,
                    Some(s.obw_hz),
                    Some(snr),
                );
                let mut req =
                    ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
                req.obw_hz = Some(s.obw_hz);
                req.snr_db = Some(snr);
                req.symbols = symbols.as_ref();
                // T-200's post-sync verifier is part of the classical cascade, and it re-ranks
                // exactly what this stage would. Comparing against a cascade with the verifier
                // switched off would credit the model with gains the classical side already makes.
                req.symbol_samples = Some(&s.symbol_samples);
                req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);
                let c = classifier.classify(&req);
                // The model is scored on every snippet routed to this family, whatever the
                // classical cascade decided. Conditioning on the cascade having *accepted* the
                // input — which is what `observe` does, correctly, in production — makes the
                // classical false-known rate 1.0 by construction, because an accepted
                // out-of-taxonomy input is precisely a classical false known. The first version of
                // this evaluation did that and reported exactly that meaningless 1.000.
                let Some(p) = stage.score(
                    routed,
                    &s.samples,
                    s.sample_rate_hz,
                    Some(s.obw_hz),
                    Timestamp::UNIX_EPOCH,
                ) else {
                    continue;
                };
                let dl_unknown = f64::from(p.unknown_score);
                let Some((dl_label, _)) = p.top() else {
                    continue;
                };

                match truth_family {
                    // In-distribution: a taxonomy class belonging to this family.
                    Some(truth) if truth == routed => {
                        id_scores
                            .entry(routed.to_owned())
                            .or_default()
                            .push((c.open_set_score, dl_unknown));
                        // The class comparison, unlike the open set, stays conditioned on what
                        // happens in production: the stage only ever runs on a snippet the
                        // cascade named this family for.
                        if c.family == routed {
                            let classical_correct =
                                c.class.as_ref().is_some_and(|cc| cc.label == class.label());
                            let classical_abstained = c.class.is_none();
                            let dl_correct = dl_label == class.label();
                            paired
                                .entry((routed.to_owned(), offset as i64))
                                .or_default()
                                .push(Outcome {
                                    classical: classical_correct,
                                    dl: dl_correct,
                                    dl_gated: dl_correct && dl_unknown < 0.5,
                                    classical_abstained,
                                });
                        }
                    }
                    // Out of taxonomy: an open-set negative for the family it most resembles.
                    None => {
                        ood_scores
                            .entry(routed.to_owned())
                            .or_default()
                            .push((c.open_set_score, dl_unknown));
                    }
                    Some(_) => {}
                }
            }
        }
    }

    let mut bins: Vec<BinResult> = Vec::new();
    for ((family, offset), rows) in &paired {
        let n = rows.len();
        let classical = mean(rows.iter().map(|r| f64::from(u8::from(r.classical))));
        let dl = mean(rows.iter().map(|r| f64::from(u8::from(r.dl))));
        let dl_gated = mean(rows.iter().map(|r| f64::from(u8::from(r.dl_gated))));
        let abstained = mean(
            rows.iter()
                .map(|r| f64::from(u8::from(r.classical_abstained))),
        );
        let (ci_lo, ci_hi) = bootstrap_delta(rows);
        bins.push(BinResult {
            family: family.clone(),
            offset_db: *offset as f64,
            n,
            classical_top1: classical,
            classical_abstained: abstained,
            dl_top1: dl,
            dl_top1_gated: dl_gated,
            delta: dl - classical,
            ci_lo,
            ci_hi,
        });
    }
    bins.sort_by(|a, b| {
        (a.family.clone(), a.offset_db as i64).cmp(&(b.family.clone(), b.offset_db as i64))
    });

    let mut families: Vec<FamilyResult> = Vec::new();
    for family in manifests.keys() {
        let id = id_scores.get(family).cloned().unwrap_or_default();
        let ood = ood_scores.get(family).cloned().unwrap_or_default();
        let classical_auroc = auroc(id.iter().map(|s| s.0), ood.iter().map(|s| s.0));
        let dl_auroc = auroc(id.iter().map(|s| s.1), ood.iter().map(|s| s.1));
        // "False known": an out-of-taxonomy input the stage would have accepted as one of its
        // classes at the operating point (0.5 for both scores, by construction).
        let classical_fk = mean(ood.iter().map(|s| f64::from(u8::from(s.0 < 0.5))));
        let dl_fk = mean(ood.iter().map(|s| f64::from(u8::from(s.1 < 0.5))));

        let mine: Vec<&BinResult> = bins.iter().filter(|b| &b.family == family).collect();
        let worst_delta = mine.iter().map(|b| b.delta).fold(f64::INFINITY, f64::min);
        let worst_ci_lo = mine.iter().map(|b| b.ci_lo).fold(f64::INFINITY, f64::min);
        // Bins where the classical stage actually named a class. Below its class gate it abstains
        // on purpose, and "beating" a deliberate abstention is not evidence that the model is
        // better at telling the classes apart.
        let deciding: Vec<&&BinResult> = mine
            .iter()
            .filter(|b| b.classical_abstained < 0.5)
            .collect();
        let worst_delta_above_class_gate = deciding
            .iter()
            .map(|b| b.delta)
            .fold(f64::INFINITY, f64::min);
        let mut notes = Vec::new();
        if deciding.len() < mine.len() {
            notes.push(format!(
                "{} of {} bins are below the class gate, where the classical stage abstains by \
                 design; over the {} bins where it decides, the worst-bin gain is {:+.3}",
                mine.len() - deciding.len(),
                mine.len(),
                deciding.len(),
                worst_delta_above_class_gate
            ));
        }
        if ood.is_empty() {
            notes.push(
                "no out-of-taxonomy generator routes to this family, so the open set is unmeasured \
                 here (a gap in the held-out set, not a result)"
                    .into(),
            );
        }
        let mut why = Vec::new();
        if mine.is_empty() {
            why.push("no in-distribution bins measured".into());
        }
        if worst_delta < MARGIN_POINTS {
            why.push(format!(
                "worst-bin gain {:+.3} < the a-priori margin {MARGIN_POINTS:.2}",
                worst_delta
            ));
        }
        if worst_ci_lo <= 0.0 {
            why.push(format!(
                "bootstrap 95 % CI lower bound {worst_ci_lo:+.3} is not > 0"
            ));
        }
        // An **unmeasured** criterion is not a satisfied one. Without an out-of-taxonomy generator
        // routed to this family there is no open-set evidence at all, and a NaN comparison quietly
        // evaluates to false — which would let a family be enabled precisely where nothing checked
        // that the model can say "not mine". That is the failure this whole stage must not have.
        if classical_auroc.is_nan() || dl_auroc.is_nan() || classical_fk.is_nan() || dl_fk.is_nan()
        {
            why.push(
                "the open set is unmeasured here (no out-of-taxonomy generator routes to this \
                 family): an unmeasured criterion cannot satisfy the enable rule"
                    .into(),
            );
        } else {
            if dl_auroc + MAX_AUROC_LOSS < classical_auroc {
                why.push(format!(
                    "open-set AUROC {dl_auroc:.3} is more than {MAX_AUROC_LOSS} below classical {classical_auroc:.3}"
                ));
            }
            if dl_fk > classical_fk {
                why.push(format!(
                    "false-known {dl_fk:.3} exceeds classical {classical_fk:.3}"
                ));
            }
        }
        families.push(FamilyResult {
            family: family.clone(),
            model: manifests[family].model.to_string(),
            n_id: id.len(),
            n_ood: ood.len(),
            classical_auroc,
            dl_auroc,
            classical_false_known: classical_fk,
            dl_false_known: dl_fk,
            worst_delta,
            worst_ci_lo,
            worst_delta_above_class_gate,
            enable: why.is_empty(),
            why,
            notes,
        });
    }

    let report = Report {
        rule:
            "ADR-0016 §4.6: DL − classical ≥ 5 points at every SNR bin ≥ gate, bootstrap 95 % CI \
               lower bound > 0, open-set AUROC not lower by > 0.02, false-known ≤ classical"
                .into(),
        margin_points: MARGIN_POINTS,
        bins,
        families,
    };

    println!("| family | SNR offset | n | classical | DL | DL (gated) | Δ | 95 % CI |");
    println!("|---|---:|---:|---:|---:|---:|---:|---|");
    for b in &report.bins {
        println!(
            "| {} | +{:.0} dB | {} | {:.3} | {:.3} | {:.3} | {:+.3} | [{:+.3}, {:+.3}] |",
            b.family,
            b.offset_db,
            b.n,
            b.classical_top1,
            b.dl_top1,
            b.dl_top1_gated,
            b.delta,
            b.ci_lo,
            b.ci_hi
        );
    }
    println!();
    for f in &report.families {
        println!(
            "{:<9} {}  (AUROC classical {:.3} / DL {:.3}; false-known {:.3} / {:.3}; worst Δ \
             {:+.3}, above class gate {:+.3}){}{}",
            f.family,
            if f.enable { "ENABLE" } else { "SHADOW" },
            f.classical_auroc,
            f.dl_auroc,
            f.classical_false_known,
            f.dl_false_known,
            f.worst_delta,
            f.worst_delta_above_class_gate,
            if f.why.is_empty() {
                String::new()
            } else {
                format!("\n          because: {}", f.why.join("; "))
            },
            if f.notes.is_empty() {
                String::new()
            } else {
                format!("\n          note: {}", f.notes.join("\n          note: "))
            }
        );
    }

    let path = out.join("dl-eval.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&report).expect("serialise"),
    )
    .expect("write report");
    println!("\nwrote {}", path.display());
}

fn load(dir: &Path) -> Option<(String, ModelManifest, Box<dyn hk_ml::LoadedModel>)> {
    let manifest: ModelManifest =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).ok()?).ok()?;
    let bytes = std::fs::read(dir.join("model.json")).ok()?;
    let family = manifest.family.clone()?;
    let model = hk_ml::mlp::MlpProvider
        .load(&manifest, &bytes)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    Some((family, manifest, model))
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let mut n = 0usize;
    let mut sum = 0.0;
    for v in values {
        sum += v;
        n += 1;
    }
    if n == 0 { f64::NAN } else { sum / n as f64 }
}

/// Percentile bootstrap (1000 paired resamples, fixed seed) of `mean(dl) − mean(classical)`.
fn bootstrap_delta(rows: &[Outcome]) -> (f64, f64) {
    if rows.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut deltas: Vec<f64> = (0..1000)
        .map(|_| {
            let mut d = 0.0;
            for _ in 0..rows.len() {
                let r = rows[(next() % rows.len() as u64) as usize];
                d += f64::from(u8::from(r.dl)) - f64::from(u8::from(r.classical));
            }
            d / rows.len() as f64
        })
        .collect();
    deltas.sort_by(f64::total_cmp);
    (deltas[25], deltas[974])
}

/// `P(score_ood > score_id)`, ties counted as half: the probability that the open-set score ranks
/// an out-of-taxonomy input above an in-distribution one.
fn auroc(id: impl Iterator<Item = f64>, ood: impl Iterator<Item = f64>) -> f64 {
    let id: Vec<f64> = id.collect();
    let ood: Vec<f64> = ood.collect();
    if id.is_empty() || ood.is_empty() {
        return f64::NAN;
    }
    let mut wins = 0.0;
    for o in &ood {
        for i in &id {
            wins += match o.partial_cmp(i) {
                Some(std::cmp::Ordering::Greater) => 1.0,
                Some(std::cmp::Ordering::Equal) => 0.5,
                _ => 0.0,
            };
        }
    }
    wins / (id.len() * ood.len()) as f64
}
