//! The C15 cascade's first stage: one normalised snippet in, one [`Classification`] out
//! (ADR-0016 §4).
//!
//! ```text
//! features@1 ─▶ tree gates ─▶ class-conditional densities ─▶ χ² open set ─▶ prior fusion
//!      │                                                              └▶ within-family class
//!      └▶ the broadcast-FM pilot rule ([`crate::fm`]) ─▶ a likelihood, beside the gate
//! ```
//!
//! What the stage guarantees, and what the tests below check:
//! - **Gates bound the claim.** A family whose SNR gate the measurement does not reach contributes
//!   no likelihood mass at all: its share goes to `unknown` with reason `low_snr`, never to a
//!   neighbouring family. "Not measured" is not "ruled out" (ADR-0016 §8).
//! - **Unknown is a real outcome.** It wins when nothing is plausible (the χ² open-set score),
//!   when no family passes a gate, and when the evidence is too flat to report one
//!   ([`crate::thresholds`] `min_confidence`, the entropy rule).
//! - **Priors rank, never veto** ([`crate::fuse`]).
//! - **Nothing is certain.** The posterior is capped at `MAX_CONFIDENCE`, and an abstention at
//!   `MAX_UNKNOWN_CONFIDENCE` — "I could not measure this" is not a near-certain claim, and the
//!   residual above the cap goes to the families the tree left admissible (T-970).
//! - **A measurement may stand beside a gate.** The pre-classification rule of [`crate::fm`]
//!   measures the FM multiplex directly and supplies a likelihood, so a broadcast station whose
//!   in-band SNR the densities cannot be trusted at is still named. It is evidence, not a verdict:
//!   it enters the likelihood and is fused with the C17 prior like everything else.
//!
//! The verifier (T-200) and the per-family DL stage (T-204) re-rank *within* what this stage
//! produced; neither may add a family it did not score.

use hk_estimate::blind::SymbolParameters;
use hk_model::Timestamp;
use hk_model::classify::{
    ClassCall, ClassFlag, ClassProvenance, Classification, Coarse, HK_MOD_V1, LabelP,
    MAX_UNKNOWN_CONFIDENCE, Stage, SuspectFlags, TaxonomyRef, UNKNOWN, entropy_norm,
};
use hk_model::emitter::LinkTarget;
use num_complex::Complex32;

use crate::density::DensityModel;
use crate::features::{FeatureInput, Features, features};
use crate::fm::{self, WfmCall};
use crate::fuse::{FamilyPriorSet, fuse};
use crate::openset::open_set_score;
use crate::thresholds::{
    ABSTAIN_ENTROPY, ABSTAIN_TOP_SHARE, FEATURES_VERSION, RULES_VERSION, THRESHOLDS_VERSION,
    passes_gate, thresholds_of,
};
use crate::tree::{admissible, class_guess, coarse_hint};

/// One classification request: a normalised snippet plus what C13/C14 measured about it.
#[derive(Clone, Debug)]
pub struct ClassifyRequest<'a> {
    /// Normalised snippet (CFO-corrected, unit mean power, cut to the extent).
    pub samples: &'a [Complex32],
    /// Sample rate of `samples`, Hz.
    pub sample_rate_hz: f64,
    /// C13 OBW99, Hz.
    pub obw_hz: Option<f64>,
    /// C13 in-band SNR of the analysed extent, dB. Without it every gated family abstains.
    pub snr_db: Option<f64>,
    /// C14 symbol estimate, when it ran.
    pub symbols: Option<&'a SymbolParameters>,
    /// The same emission at C14's geometry ([`crate::symbols::SYMBOL_SAMPLES_PER_OBW`]), for the
    /// post-sync verifier (T-200). `None` — or a missing clock lock — and the verifier does not
    /// run, leaving the feature tree's within-family ranking exactly as it was.
    pub symbol_samples: Option<&'a [Complex32]>,
    /// Sample rate of [`ClassifyRequest::symbol_samples`], Hz.
    pub symbol_sample_rate_hz: Option<f64>,
    /// When the classification was produced.
    pub t: Timestamp,
    /// The observation it ran on (track, detection, demodulation, decode).
    pub input: Option<LinkTarget>,
    /// Detection provenance: clipped, IMD, image, spur.
    pub suspect: SuspectFlags,
    /// Power mode it ran in, when the caller tracks one.
    pub power_mode: Option<String>,
    /// C17 prior for this extent (T-212 supplies it; `None` = no reference data).
    pub prior: Option<FamilyPriorSet>,
}

impl<'a> ClassifyRequest<'a> {
    /// A request over `samples` with nothing else measured.
    pub fn new(samples: &'a [Complex32], sample_rate_hz: f64, t: Timestamp) -> Self {
        Self {
            samples,
            sample_rate_hz,
            obw_hz: None,
            snr_db: None,
            symbols: None,
            symbol_samples: None,
            symbol_sample_rate_hz: None,
            t,
            input: None,
            suspect: SuspectFlags::default(),
            power_mode: None,
            prior: None,
        }
    }
}

/// The feature-tree classifier.
#[derive(Clone, Debug)]
pub struct Classifier {
    model: DensityModel,
    /// Densities fitted below each family's gate. Used only to size the `unknown` mass owed to a
    /// family the SNR gate held back, never to claim one.
    below_gate: DensityModel,
}

impl Default for Classifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Classifier {
    /// A classifier over the shipped densities.
    pub fn new() -> Self {
        Self {
            model: DensityModel::builtin().clone(),
            below_gate: DensityModel::builtin_below_gate().clone(),
        }
    }

    /// A classifier over a model the caller fitted (the dev-grid fitter, T-204's experiments).
    /// The shipped below-gate densities are kept: they only ever move mass to `unknown`.
    pub fn with_model(model: DensityModel) -> Self {
        Self {
            model,
            below_gate: DensityModel::builtin_below_gate().clone(),
        }
    }

    /// A classifier over **both** models the caller fitted (T-364).
    ///
    /// `bin/fit-densities` fits two models under one protocol, and an experiment that changes the
    /// protocol has to change both or it measures a hybrid of the old and the new. Prefer
    /// [`Classifier::with_model`] when only the claiming model is under test; the below-gate model
    /// still cannot make the classifier more confident, only move mass to `unknown`.
    pub fn with_models(model: DensityModel, below_gate: DensityModel) -> Self {
        Self { model, below_gate }
    }

    /// The densities it scores with.
    pub fn model(&self) -> &DensityModel {
        &self.model
    }

    /// The broadcast-FM pre-classification rule ([`crate::fm`]), run on the request's own samples.
    ///
    /// Bounded by construction: it measures nothing unless C13 reports a station-shaped occupied
    /// bandwidth ([`fm::STATION_OBW_HZ`]) at a rate that can carry a 57 kHz subcarrier
    /// ([`fm::MIN_MPX_RATE_HZ`]), so the extra transform is paid on FM-shaped boxes and on nothing
    /// else. Every outcome leaves a machine reason, so a row says whether the rule looked.
    fn wfm_rule(
        &self,
        request: &ClassifyRequest<'_>,
        reasons: &mut Vec<String>,
    ) -> Option<WfmCall> {
        let obw = request.obw_hz?;
        if !(fm::STATION_OBW_HZ.0..=fm::STATION_OBW_HZ.1).contains(&obw)
            || request.sample_rate_hz < fm::MIN_MPX_RATE_HZ
        {
            return None;
        }
        let Some(ev) = fm::mpx_evidence(request.samples, request.sample_rate_hz) else {
            push_reason(reasons, "mpx_not_measurable");
            return None;
        };
        match fm::wfm_rule(Some(obw), &ev) {
            Some(call) => {
                push_reason(reasons, call.reason);
                Some(call)
            }
            None => {
                push_reason(reasons, "no_fm_pilot");
                None
            }
        }
    }

    /// Classifies one normalised snippet.
    pub fn classify(&self, request: &ClassifyRequest<'_>) -> Classification {
        let f = features(&FeatureInput {
            samples: request.samples,
            sample_rate_hz: request.sample_rate_hz,
            obw_hz: request.obw_hz,
            snr_db: request.snr_db,
            symbols: request.symbols,
        });
        self.classify_features(&f, request)
    }

    /// Classifies an already-computed feature vector (the fitter and the evaluation harness reuse
    /// the features they measured).
    pub fn classify_features(&self, f: &Features, request: &ClassifyRequest<'_>) -> Classification {
        let tax = &HK_MOD_V1;
        let mut reasons: Vec<String> = f.reasons.clone();
        let mut flags: Vec<ClassFlag> = Vec::new();

        // 1. Gates and tree exclusions decide which families may be scored at all.
        let rules = admissible(f);
        let mut gated = false;
        let mut scored: Vec<(String, crate::density::FamilyScore)> = Vec::new();
        // How plausibly a family held back by its SNR gate could explain this snippet, read from
        // the below-gate densities. It becomes `unknown` mass below, never a claim.
        let mut gated_plausibility = 0.0_f64;
        for rule in &rules {
            if !rule.allowed {
                if let Some(r) = rule.reason {
                    push_reason(&mut reasons, r);
                }
                continue;
            }
            if passes_gate(rule.family, request.snr_db) {
                match self.model.score(rule.family, f) {
                    Some(s) => scored.push((rule.family.to_owned(), s)),
                    None => push_reason(&mut reasons, "too_few_features"),
                }
                continue;
            }
            gated = true;
            push_reason(
                &mut reasons,
                if request.snr_db.is_some() {
                    "low_snr"
                } else {
                    "no_snr"
                },
            );
            if let Some(s) = self.below_gate.score(rule.family, f) {
                gated_plausibility = gated_plausibility.max(s.plausibility);
            }
        }
        if gated {
            flags.push(ClassFlag::BelowGate);
        }
        if request.suspect.any() {
            flags.push(ClassFlag::SuspectInput);
        }

        // 1b. **The pre-classification rule** (T-970): a 19 kHz stereo pilot with its subcarriers
        // suppressed is a broadcast FM multiplex and nothing else in `hk-mod@1` produces one, so
        // the measurement decides on its own — including where the SNR gate above held `analog`
        // back, because a tone's detectability is set by the integration time and not by the
        // in-band SNR of the whole 200 kHz channel (see [`crate::fm`]). It reads only the samples
        // and C13's occupied bandwidth; it cannot reach a band plan.
        let wfm = self.wfm_rule(request, &mut reasons);

        // 2. Open set: how far the snippet is from every family that was allowed to explain it
        // (the calibrated plausibility). The evidence-only distribution ranks what is left.
        let open_set = open_set_score(scored.iter().map(|(_, s)| s.plausibility));

        // A family held back by its SNR gate has not been ruled out — it was **not measured**, and
        // ADR-0016 §2 puts its share on `unknown`, not on its neighbours. Dropping it and
        // renormalising over the survivors is what makes a low-SNR FSK burst come back as
        // `analog`: FSK is gated out at 20 dB while analog, gated at 10, is still standing and
        // collects the whole distribution.
        //
        // So the `unknown` mass is at least the plausibility of the best family the gate held back:
        // if a gated family could genuinely have produced this snippet, no survivor may be claimed,
        // because the one measurement that would have separated them is the one we were not allowed
        // to make. `plausibility` is the right currency for that comparison — a dev-calibrated χ²
        // tail, so it is directly comparable with the open-set score, unlike `evidence`, which is
        // `exp(-K_REF*m/2)` and underflows to exactly 0 for any poor fit.
        //
        // The plausibility comes from the **below-gate** densities, which are fitted where the
        // family is gated (`fit-densities` writes both files). The shipped at-gate densities cannot
        // answer this: scoring a family below its own gate is an extrapolation, and a genuine 2-FSK
        // burst 10 dB under the FSK gate scored its own family at plausibility 0.000 — so no mass
        // moved to `unknown` and the constant-envelope `analog` classes, which that waveform really
        // does resemble at that SNR, claimed it instead (a 0.65 wrong-label rate in that bin).
        //
        // An AM carrier at the same SNR is not plausible FSK, contributes nothing, and is still
        // reported as analog. This is only ever a `max`, so a gated family can move mass to
        // `unknown` but can never make a claim.
        let open_set = open_set.max(gated_plausibility);
        let known_mass = 1.0 - open_set;
        let total: f64 = scored.iter().map(|(_, s)| s.evidence).sum();
        let likelihood: Vec<LabelP> = tax
            .families
            .iter()
            .map(|fam| LabelP {
                label: fam.name.to_owned(),
                p: match scored.iter().find(|(l, _)| l == fam.name) {
                    Some((_, s)) if total > 0.0 => known_mass * s.evidence / total,
                    _ => 0.0,
                },
            })
            .chain(std::iter::once(LabelP {
                label: UNKNOWN.to_owned(),
                p: open_set,
            }))
            .collect();

        // The rule's claim enters as **likelihood**, not as a verdict: it goes where the cascade's
        // evidence goes and is fused with the C17 prior through exactly the same path, rather than
        // short-circuiting it as a hard-coded family would. What it is not is *arguable by the band
        // plan*: [`rule_likelihood`] leaves the residual on `unknown`, so the runner-up known
        // family is 0 and `fuse` finds the evidence dominant, tempering any prior that disagrees
        // back. A prior explains a measured station; it never relabels one.
        let likelihood = match &wfm {
            Some(call) => rule_likelihood(&likelihood, call),
            None => likelihood,
        };
        let open_set = match &wfm {
            Some(call) => 1.0 - call.confidence,
            None => open_set,
        };
        let known_mass = 1.0 - open_set;

        // 3. Fusion with the C17 prior (a prior can reorder, never veto or reach `unknown`).
        let fused = fuse(&likelihood, open_set, request.prior.as_ref());
        let mut posterior = fused.posterior;
        flags.extend(fused.flags);

        // 4. Report or abstain.
        let top = top_known(&posterior);
        let l_top = top_known(&likelihood).map_or(0.0, |(_, p)| p);
        let l_share = if known_mass > 0.0 {
            l_top / known_mass
        } else {
            0.0
        };
        let entropy_of_likelihood = entropy_norm(&likelihood, tax.families.len() + 1);
        let abstain = match &top {
            // The rule measured the multiplex; the abstention rules below all ask whether the
            // *density cascade* said enough, which is a question the rule has already answered —
            // **for `analog`**. If fusion put some other family on top, the rule is not what is
            // being reported and the ordinary rules apply again.
            Some((label, _)) if wfm.is_some() && label == "analog" => None,
            None => Some("no_family_scored"),
            Some((label, p)) => {
                let t = thresholds_of(label);
                if scored.is_empty() {
                    Some("no_family_scored")
                } else if *p < t.map_or(0.6, |t| t.min_confidence) {
                    Some("low_confidence")
                } else if open_set > t.map_or(0.5, |t| t.open_set_max) {
                    Some("open_set")
                } else if scored
                    .iter()
                    .find(|(l, _)| l == label)
                    .is_some_and(|(_, s)| 1.0 - s.plausibility > t.map_or(0.5, |t| t.open_set_max))
                {
                    // **The open-set score is a maximum over families; the claim is not** (T-248).
                    //
                    // `open_set = 1 − max_c L_c` (ADR-0016 §4.4) asks "is *any* known family
                    // plausible?", while the family reported is the one with the highest
                    // **evidence**. Those are different families more often than it sounds, and the
                    // arm above then tests the claimed family's threshold against a number some
                    // other family produced.
                    //
                    // Measured: the held-out 8-level FSK is claimed `fsk` with confidence 0.997 and
                    // a reported open set of 0.000, while `fsk`'s own best class scores it at
                    // plausibility **0.152** — m 2.187 against `4fsk`'s dev m_p95 of 1.709, i.e.
                    // already outside the envelope of its own claimed family. The 0.000 came from a
                    // different family fitting loosely; `fsk` won the ranking on evidence. 13 of
                    // those 15 snippets abstain once the question is asked of the family actually
                    // being named.
                    //
                    // So the family being claimed must itself be plausible, at the same threshold
                    // §4.4 already states. This can only ever *withhold* a claim — it adds no
                    // family, moves no mass and cannot raise a confidence — so it is a tightening
                    // of the abstention rule, not a new decision procedure. `open_set_score` keeps
                    // the ADR's definition unchanged; this is one more condition under which
                    // unknown wins, alongside the gate and entropy conditions §4.4 already lists.
                    Some("open_set_family")
                } else if l_share < ABSTAIN_TOP_SHARE && entropy_of_likelihood > ABSTAIN_ENTROPY {
                    Some("ambiguous")
                } else {
                    None
                }
            }
        };
        if let Some(reason) = abstain {
            push_reason(&mut reasons, reason);
            force_unknown(&mut posterior);
        }
        // **An abstention is not a confident claim** (T-970). `unknown` used to come out of the
        // cap at `MAX_CONFIDENCE` whenever no family scored at all, so a row the cascade could not
        // measure read `unknown 0.999` — "100 % unk" on the explorer's screen — which states more
        // certainty about the world than a row that names a family. The residual belongs to the
        // families the tree left **admissible**: those are precisely the ones that were not ruled
        // out and could not be measured, which is what an abstention means (ADR-0016 §2).
        if cap_unknown(&mut posterior, &rules) {
            push_reason(&mut reasons, "unknown_capped");
        }

        let (family, confidence) = posterior
            .iter()
            .max_by(|a, b| a.p.total_cmp(&b.p).then_with(|| b.label.cmp(&a.label)))
            .map(|lp| (lp.label.clone(), lp.p))
            .unwrap_or_else(|| (UNKNOWN.to_owned(), 1.0));

        // 5. Within-family class, only above the family's class gate.
        let snr_gate_db = thresholds_of(&family)
            .and_then(|t| t.snr_gate_db)
            .unwrap_or(0.0);
        let class_gate = thresholds_of(&family).map_or(0.0, |t| t.class_gate_db);
        let class = if let Some(call) = wfm_class(&family, wfm.as_ref()) {
            call
        } else if wfm.is_some() && family != "analog" {
            // The rule fired but something else is being reported: say so, and name no class.
            push_reason(&mut reasons, "wfm_rule_not_reported");
            None
        } else if family == UNKNOWN
            || !request
                .snr_db
                .is_some_and(|s| s >= snr_gate_db + class_gate)
        {
            if family != UNKNOWN {
                push_reason(&mut reasons, "below_class_gate");
            }
            None
        } else {
            class_guess(
                &family,
                f,
                &self.model,
                request.symbols.and_then(|s| s.mod_index_h.value()),
            )
            .map(|g| ClassCall {
                label: g.label,
                p: g.p,
                dist: g.dist,
                stage: Stage::FeatureTree,
            })
        };

        let coarse = if family == UNKNOWN {
            coarse_hint(f)
        } else {
            tax.coarse_of(&family).unwrap_or(Coarse::Unknown)
        };

        let mut out = Classification {
            schema: hk_model::classify::CLASSIFICATION_SCHEMA,
            t: request.t,
            taxonomy: TaxonomyRef::current(),
            input: request.input,
            coarse,
            entropy_norm: entropy_norm(&posterior, tax.families.len() + 1),
            likelihood,
            posterior,
            prior: request.prior.as_ref().map(FamilyPriorSet::to_use),
            family,
            confidence,
            class,
            open_set_score: open_set,
            stage: Stage::FeatureTree,
            provenance: ClassProvenance {
                rules: RULES_VERSION.to_owned(),
                features_version: FEATURES_VERSION,
                features_ref: None,
                ml: None,
                snr_db: request.snr_db,
                snr_gate_db,
                gated,
                thresholds: THRESHOLDS_VERSION.to_owned(),
                suspect: request.suspect,
                power_mode: request.power_mode.clone(),
            },
            flags,
            reasons,
        };

        // 6. The post-sync verifier (T-200), the cascade's next stage. It re-ranks the within-family
        // candidates above and may do nothing else: it never changes the family, never adds a class
        // the tree did not offer, and never turns an abstention into a claim (see [`crate::verify`]).
        // Without the symbol-geometry view, or without a C14 clock lock, it does not run at all.
        //
        // **A skip is recorded, not swallowed (T-589).** The outcome used to be discarded here, so
        // a classification whose verifier ran and agreed and one whose verifier never ran carried
        // exactly the same reasons — and C14 not locking on a whole modulation class (genuine
        // 8-PSK, 0 of 12 snippets) was therefore invisible in production. Every path now leaves a
        // machine reason: `verifier_confirmed` / `verifier_reranked` from [`crate::verify::verify`]
        // itself, or the [`SkipReason`](crate::verify::SkipReason) code from here.
        let verified = if let (Some(samples), Some(rate)) =
            (request.symbol_samples, request.symbol_sample_rate_hz)
        {
            crate::verify::verify(
                &mut out,
                &crate::verify::VerifyInput {
                    samples,
                    sample_rate_hz: rate,
                    symbols: request.symbols,
                    snr_db: request.snr_db,
                },
            )
        } else {
            crate::verify::VerifyOutcome::Skipped(crate::verify::SkipReason::NoSymbolView)
        };
        if let crate::verify::VerifyOutcome::Skipped(why) = verified {
            push_reason(&mut out.reasons, why.as_str());
        }
        out
    }
}

/// The class a [`WfmCall`] names — **only when the family being reported is the one it belongs
/// to** (T-970 review).
///
/// A `Classification` whose class is not a class of its family is invalid
/// ([`Classification::validate`]: "class not in family"), and the family reported is the one
/// fusion and the abstention rules settled on, not the one the rule proposed. Binding the class to
/// the rule instead of to the family made an invalid row constructible, so the class follows the
/// family here and the caller records why it dropped the name. Returns `None` when the rule did
/// not fire, and `Some(None)` is not representable — a fired rule on another family is the
/// caller's branch, so it can leave a reason.
fn wfm_class(family: &str, call: Option<&WfmCall>) -> Option<Option<ClassCall>> {
    let call = call?;
    (family == "analog").then(|| {
        // The pilot is not evidence that the emission is *some* analog mode — it is what makes it
        // wideband broadcast FM rather than AM, SSB or NBFM. The class gate exists because the
        // within-family densities need SNR; this name does not come from them.
        Some(ClassCall {
            label: "wfm".to_owned(),
            p: call.confidence,
            dist: Vec::new(),
            stage: Stage::FeatureTree,
        })
    })
}

/// Redistributes `likelihood` so `analog` carries the rule's confidence and **`unknown` carries
/// all of the residual**.
///
/// The residual is not evidence for the other families, and spreading it over them was a defect
/// (T-970 review): a 19 kHz pilot says nothing whatever about whether an emission is FSK or PSK,
/// so the alternative to "this is a broadcast multiplex" is "the pilot measurement misled me and I
/// do not know what this is" — which is what `unknown` means. Putting it there is both the honest
/// statement and the one that keeps the measurement safe from the band plan:
/// [`crate::fuse::fuse`] tempers a prior back whenever the evidence is dominant, and dominance is
/// read off the **runner-up known family**, which is now 0. A prior can therefore explain a
/// measured station but never relabel one, which is the product rule that the database is never a
/// source of truth and never overrides what was measured.
///
/// Spreading it proportionally instead put up to `1 − confidence` on one family, and a pilot-only
/// call ([`fm::WFM_PILOT_CONFIDENCE`]) is 0.90 : 0.10 — a 9:1 ratio, *inside* the 10:1
/// [`crate::thresholds::EVIDENCE_DOMINANCE_RATIO`] a prior may reorder within. The reported family
/// could then be moved off `analog` while the class stayed `wfm`, which
/// [`Classification::validate`] rejects outright.
fn rule_likelihood(likelihood: &[LabelP], call: &WfmCall) -> Vec<LabelP> {
    let share = 1.0 - call.confidence;
    likelihood
        .iter()
        .map(|lp| LabelP {
            label: lp.label.clone(),
            p: match lp.label.as_str() {
                "analog" => call.confidence,
                UNKNOWN => share,
                _ => 0.0,
            },
        })
        .collect()
}

/// Holds an abstention's posterior at [`MAX_UNKNOWN_CONFIDENCE`], moving the excess to the
/// families `rules` left admissible (or, when the tree admitted none, to every family: "we could
/// measure nothing" is maximal ignorance, not certainty). Returns whether it had to.
fn cap_unknown(posterior: &mut [LabelP], rules: &[crate::tree::Admissibility]) -> bool {
    let Some(u) = posterior.iter().position(|lp| lp.label == UNKNOWN) else {
        return false;
    };
    if posterior[u].p <= MAX_UNKNOWN_CONFIDENCE || posterior.iter().any(|lp| lp.p > posterior[u].p)
    {
        return false;
    }
    let admitted: Vec<usize> = (0..posterior.len())
        .filter(|i| *i != u)
        .filter(|i| {
            rules.is_empty()
                || !rules.iter().any(|r| r.family == posterior[*i].label)
                || rules
                    .iter()
                    .any(|r| r.family == posterior[*i].label && r.allowed)
        })
        .collect();
    let targets: Vec<usize> = if admitted.is_empty() {
        (0..posterior.len()).filter(|i| *i != u).collect()
    } else {
        admitted
    };
    if targets.is_empty() {
        return false;
    }
    let excess = posterior[u].p - MAX_UNKNOWN_CONFIDENCE;
    posterior[u].p = MAX_UNKNOWN_CONFIDENCE;
    let weight: f64 = targets.iter().map(|i| posterior[*i].p).sum();
    for i in &targets {
        posterior[*i].p += if weight > 0.0 {
            excess * posterior[*i].p / weight
        } else {
            excess / targets.len() as f64
        };
    }
    crate::fuse::normalise_dist(posterior);
    true
}

fn push_reason(reasons: &mut Vec<String>, reason: &str) {
    if !reasons.iter().any(|r| r == reason) {
        reasons.push(reason.to_owned());
    }
}

fn top_known(dist: &[LabelP]) -> Option<(String, f64)> {
    dist.iter()
        .filter(|lp| lp.label != UNKNOWN)
        .max_by(|a, b| a.p.total_cmp(&b.p).then_with(|| b.label.cmp(&a.label)))
        .filter(|lp| lp.p > 0.0)
        .map(|lp| (lp.label.clone(), lp.p))
}

/// Raises `unknown` above every family so the row *reports* an abstention, keeping the relative
/// order of the families below it (the ranking still seeds MAUTO, ADR-0016 §8).
fn force_unknown(posterior: &mut [LabelP]) {
    let top = posterior
        .iter()
        .filter(|lp| lp.label != UNKNOWN)
        .map(|lp| lp.p)
        .fold(0.0_f64, f64::max);
    if let Some(u) = posterior.iter_mut().find(|lp| lp.label == UNKNOWN) {
        u.p = u.p.max(top + 0.05);
    }
    crate::fuse::cap_and_normalise(posterior);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{Class, SynthConfig, generate};

    fn classify(class: Class, snr_db: f64, seed: u64) -> Classification {
        let s = generate(class, &SynthConfig::new(snr_db, seed));
        let mut req = ClassifyRequest::new(
            &s.samples,
            s.sample_rate_hz,
            Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
        );
        req.obw_hz = Some(s.obw_hz);
        req.snr_db = Some(snr_db);
        Classifier::new().classify(&req)
    }

    #[test]
    fn every_classification_satisfies_the_contract() {
        for class in Class::TAXONOMY.iter().chain(Class::HELD_OUT) {
            for snr in [0.0, 12.0, 25.0] {
                let c = classify(*class, snr, 1_000_123);
                c.validate()
                    .unwrap_or_else(|e| panic!("{} at {snr} dB: {e}", class.label()));
                assert!(c.confidence <= hk_model::classify::MAX_CONFIDENCE);
                assert_eq!(c.stage, Stage::FeatureTree);
                assert_eq!(c.provenance.rules, RULES_VERSION);
                assert_eq!(c.likelihood.len(), c.posterior.len());
            }
        }
    }

    #[test]
    fn below_the_gate_the_mass_goes_to_unknown_not_to_a_neighbour() {
        // 5 dB is below every gate: nothing may be claimed.
        for class in [Class::Fsk2, Class::Bpsk, Class::Ook] {
            let c = classify(class, 5.0, 1_000_321);
            assert_eq!(
                c.family,
                UNKNOWN,
                "{} at 5 dB: {:?}",
                class.label(),
                c.top(3)
            );
            assert!(c.flags.contains(&ClassFlag::BelowGate));
            assert!(c.reasons.iter().any(|r| r == "low_snr"));
            assert!(c.provenance.gated);
        }
        // Without a measured SNR, a gated family cannot be claimed either.
        let s = generate(Class::Fsk2, &SynthConfig::new(30.0, 1_000_322));
        let req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
        let c = Classifier::new().classify(&req);
        assert_eq!(c.family, UNKNOWN);
        assert!(c.reasons.iter().any(|r| r == "no_snr"));
    }

    #[test]
    fn an_out_of_taxonomy_generator_is_unknown_not_the_nearest_family() {
        let mut unknowns = 0;
        let mut n = 0;
        let mut adr_unknowns = 0;
        let mut adr_n = 0;
        let mut wrong_family = Vec::new();
        for class in Class::HELD_OUT {
            let adr = Class::ADR_HELD_OUT.contains(class);
            for seed in 0..6 {
                let c = classify(*class, 25.0, crate::synth::ACCEPTANCE_SEED_BASE + seed);
                n += 1;
                adr_n += i32::from(adr);
                if c.family == UNKNOWN || c.open_set_score >= 0.5 {
                    unknowns += 1;
                    adr_unknowns += i32::from(adr);
                } else if class.nearest_family() != Some(c.family.as_str()) {
                    wrong_family.push((class.label(), c.family.clone()));
                }
            }
        }
        // ADR-0016 §7's floor, over the population §7 names (`Class::ADR_HELD_OUT`).
        let adr_recall = f64::from(adr_unknowns) / f64::from(adr_n);
        assert!(
            adr_recall >= 0.80,
            "held-out unknown recall {adr_recall:.2}"
        );

        // **The property that holds over every generator, T-244's five included, and the one the
        // name of this test claims:** an out-of-taxonomy emission is never given a family that is
        // not its own. Where it is not abstained on, it is recognised as the family it genuinely
        // belongs to — an unlisted analog mode as `analog`, an unlisted constellation as
        // `psk-qam` — which is generalisation, not a confident wrong label.
        assert!(wrong_family.is_empty(), "{wrong_family:?}");
        eprintln!(
            "[T-244] abstention over all {n} held-out snippets {:.2} (ADR-0016 §7's six: {adr_recall:.2}); wrong family: 0",
            f64::from(unknowns) / f64::from(n)
        );
    }

    #[test]
    fn a_short_snippet_abstains_with_a_reason_rather_than_guessing() {
        let c = Classifier::new().classify(&ClassifyRequest::new(
            &[Complex32::new(0.5, 0.5); 32],
            1e6,
            Timestamp::UNIX_EPOCH,
        ));
        assert_eq!(c.family, UNKNOWN);
        assert!(
            c.reasons.iter().any(|r| r == "too_short"),
            "{:?}",
            c.reasons
        );
        c.validate().unwrap();
    }

    /// A **mono-programme stereo station**: an FM carrier with 0–15 kHz audio and a 19 kHz pilot,
    /// and nothing at 38 or 57 kHz. It is a real and common case — a talk station, or any mono
    /// programme on a stereo transmitter that leaves its pilot on — and it is the one the
    /// pilot-only ceiling [`fm::WFM_PILOT_CONFIDENCE`] exists for. The dev grid cannot generate it
    /// ([`crate::synth`] couples the pilot to the L−R subcarrier), so it is built here.
    fn mono_programme_with_pilot(fs: f64, n: usize, snr_db: f64) -> Vec<Complex32> {
        use hk_dsp::synth::Rng;
        use std::f64::consts::TAU;
        let mut rng = Rng::new(0x9704_0001);
        let mut phase = 0.0_f64;
        let mut out = Vec::with_capacity(n);
        // Noise is not decoration here. A noiseless carrier's discriminator floor is f32 rounding
        // residue shaped by the modulation itself, which reads as energy in the 24-53 kHz and the
        // 57 kHz bands and turns this into a stereo-plus-RDS station (measured: `stereo_db` +22.1,
        // `rds_p_fa` 3e-35, so the rule claimed `wfm_pilot_rds` at 0.97 and the case under test
        // was never reached). A real thermal floor is what leaves the pilot as the only thing
        // present.
        let sigma = (10f64.powf(-snr_db / 10.0) / 2.0).sqrt();
        for i in 0..n {
            let t = i as f64 / fs;
            let audio = 0.45 * (TAU * 997.0 * t).sin()
                + 0.30 * (TAU * 4_310.0 * t).sin()
                + 0.16 * (TAU * 11_030.0 * t).sin();
            let mpx = 60e3 * audio + 5.4e3 * (TAU * 19_000.0 * t).sin();
            // Wrapped, so the phase stays inside f64's precise range over a long record.
            phase = (phase + TAU * mpx / fs).rem_euclid(TAU);
            let (ni, nq) = rng.gaussian_pair();
            out.push(Complex32::new(
                (phase.cos() + sigma * ni) as f32,
                (phase.sin() + sigma * nq) as f32,
            ));
        }
        out
    }

    /// A prior that puts almost all of its allocation mass on one family.
    fn hostile_prior(family: &str) -> crate::fuse::FamilyPriorSet {
        let dist = HK_MOD_V1
            .families
            .iter()
            .map(|f| LabelP {
                label: f.name.to_owned(),
                p: if f.name == family {
                    1.0 - 0.001 * (HK_MOD_V1.families.len() - 1) as f64
                } else {
                    0.001
                },
            })
            .collect();
        let p = crate::fuse::FamilyPriorSet {
            prior_ref: "test:hostile".to_owned(),
            lambda: [0.1, 0.9, 0.0, 0.0],
            dist,
        };
        p.validate().unwrap();
        p
    }

    /// **The class follows the family, never the rule** (T-970 review).
    ///
    /// The reported family is what fusion and the abstention rules settled on, and a
    /// `Classification` whose class is not a class of its family is invalid. Binding the `wfm`
    /// name to the rule rather than to the family made an invalid row constructible, so this holds
    /// the guard directly: every family in the taxonomy, plus `unknown`.
    #[test]
    fn a_wfm_class_is_only_ever_reported_on_the_analog_family() {
        let call = crate::fm::WfmCall {
            confidence: crate::fm::WFM_PILOT_CONFIDENCE,
            reason: "wfm_pilot",
        };
        assert_eq!(wfm_class("analog", None), None, "no rule, no class");
        let named = wfm_class("analog", Some(&call)).flatten().expect("named");
        assert_eq!(named.label, "wfm");
        for family in HK_MOD_V1
            .families
            .iter()
            .map(|f| f.name)
            .chain(std::iter::once(UNKNOWN))
        {
            if family == "analog" {
                continue;
            }
            assert_eq!(
                wfm_class(family, Some(&call)),
                None,
                "the rule named wfm while reporting {family}"
            );
        }
    }

    /// The whole path under an adversarial prior, on the waveform that reaches the pilot-only
    /// ceiling: the row must be **valid**, and the band plan must not be able to relabel a
    /// measured station.
    ///
    /// The features are taken from a genuine 2-FSK snippet while the samples are the pilot-bearing
    /// multiplex, which is the sharpest input the public `classify_features` entry point admits:
    /// it puts the density cascade's confidence on `fsk` at the same time as the rule measures the
    /// pilot, so the rule's residual and the prior are both pushing the family off `analog` at
    /// once. With the defect re-injected, the measured likelihood here is `analog` 0.900 against
    /// `fsk` 0.100 — a 9:1 ratio *inside* the 10:1 the prior may reorder within — and the row came
    /// back `fsk` carrying class `wfm`: `prior on fsk produced an invalid row: invalid
    /// classification: class "wfm" not in family fsk`.
    #[test]
    fn a_prior_can_explain_a_measured_station_but_never_relabel_one() {
        let fs = 600e3;
        let samples = mono_programme_with_pilot(fs, 300_000, 25.0);
        let fsk = generate(Class::Fsk2, &SynthConfig::new(30.0, 1_000_970));
        let fsk_features = features(&FeatureInput {
            samples: &fsk.samples,
            sample_rate_hz: fsk.sample_rate_hz,
            obw_hz: Some(fsk.obw_hz),
            snr_db: Some(30.0),
            symbols: None,
        });

        let mut req = ClassifyRequest::new(&samples, fs, Timestamp::UNIX_EPOCH);
        req.obw_hz = Some(160e3);
        req.snr_db = Some(30.0);
        let classifier = Classifier::new();

        // The rule has to fire on this waveform, or the test proves nothing.
        let plain = classifier.classify_features(&fsk_features, &req);
        assert!(
            plain.reasons.iter().any(|r| r.starts_with("wfm_pilot")),
            "the pilot rule did not fire: {:?}",
            plain.reasons
        );

        for family in ["fsk", "psk-qam", "ook-ask", "ofdm"] {
            let mut hostile = req.clone();
            hostile.prior = Some(hostile_prior(family));
            let c = classifier.classify_features(&fsk_features, &hostile);
            c.validate()
                .unwrap_or_else(|e| panic!("prior on {family} produced an invalid row: {e}"));
            assert_eq!(
                c.family,
                "analog",
                "a prior on {family} relabelled a measured station: {:?}",
                c.top(3)
            );
            assert_eq!(
                c.class.as_ref().map(|k| k.label.as_str()),
                Some("wfm"),
                "prior on {family}"
            );
        }
    }

    #[test]
    fn a_suspect_input_is_flagged_but_still_classified() {
        let s = generate(Class::Fsk2, &SynthConfig::new(25.0, 1_000_555));
        let mut req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
        req.obw_hz = Some(s.obw_hz);
        req.snr_db = Some(25.0);
        req.suspect.clipped = true;
        let c = Classifier::new().classify(&req);
        assert!(c.flags.contains(&ClassFlag::SuspectInput));
        assert!(c.provenance.suspect.clipped);
        c.validate().unwrap();
    }
}
