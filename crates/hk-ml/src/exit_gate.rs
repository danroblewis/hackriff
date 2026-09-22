//! ADR-0016 §7's **ML exit-gate row**, as a checkable predicate instead of a sentence (T-366).
//!
//! §7's M3 exit-gate table carries one ML row — *"shadow changes no Classification row; each
//! `active` family has enable evidence; zero ring sample drops with ML on"* — and until this
//! module existed, **nothing anywhere asserted it**: `tests/e2e/tests/acceptance/m3_*.rs` held no
//! ML assertion at all, so the row passed because no one checked it. That is the shape T-206's two
//! vacuous gate dimensions already cost this project once.
//!
//! # What this is, and what it deliberately is not
//!
//! It is a **pure function over observables** — the `(model, consumer)` modes in force, the
//! `Classification` rows a run persisted, and the run's lost-sample count — returning the
//! violations of the §7 row. It holds no policy of its own: every rule below is a transcription of
//! a clause of that row, and the enforcement *at the point of mutation* stays where it already is
//! ([`crate::host::ModelHost::set_mode`] refuses `active` without evidence;
//! [`crate::host::ModelHost::decide`] refuses anything but `active`). This is the **gate-level**
//! statement: whatever the system ended up in, does it satisfy the row?
//!
//! It is **not** a test that passes today because ML is off. With the host dormant (T-363) the
//! modes list is empty and no row names a model, so the gate is satisfied — but it is satisfied
//! *by a measurement over a real run*, and the same predicate goes red the day an ML-attributed
//! `Classification` row appears without an `active` model with evidence behind it. That is the
//! property T-366 asked for: a check that **fails the day ML becomes active without its
//! evidence**, rather than one that is true because nothing is on.
//!
//! **Non-vacuity is proved the T-287/T-297 way, not argued.** Every violation below is constructed
//! in this module's own tests and caught, including through a real [`crate::host::ModelHost`]
//! forced to `active` with no enable evidence ([`MlGateSnapshot::from_host`]), which is the one
//! path that can reach `active` without evidence today. The acceptance half
//! (`tests/e2e/tests/acceptance/m3_ml.rs`) does the same over a real run's rows.

use crate::MlMode;
use crate::host::{ModelHost, model_key};
use std::fmt;

/// One `(model, consumer)` mode in force, with the evidence ADR-0016 §4.6 requires of `active`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeInForce {
    /// `id@version` ([`model_key`]).
    pub model: String,
    /// The consumer the mode is scoped to.
    pub consumer: String,
    /// The mode.
    pub mode: MlMode,
    /// Whether ADR-0016 §4.6's requirements were overridden to reach it.
    pub forced: bool,
    /// Reference to the §4.6 enable evidence on the manifest, if any.
    pub enable_evidence: Option<String>,
}

/// A persisted `Classification` row **attributed to a model**: one whose
/// `provenance.ml` names a model, or whose stage is the DL stage.
///
/// A row that names no model but claims the DL stage is as much a violation as one naming a shadow
/// model: either way a model changed a `Classification` and the gate cannot tell which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MlAttributedRow {
    /// What the row is about, for the failure message (an emitter or detection id).
    pub subject: String,
    /// `provenance.ml.id` as stored — `id@version#sha8` — if the row names a model.
    pub model: Option<String>,
    /// The row's stage, as persisted (`dl`, `feature-tree`, …).
    pub stage: String,
}

/// The state of the ML runtime at the end of a run, in the terms ADR-0016 §7's row is stated over.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MlGateSnapshot {
    /// Every `(model, consumer)` mode in force. Empty means no model is loaded in any mode.
    pub modes: Vec<ModeInForce>,
    /// Every ML-attributed `Classification` row the run persisted.
    pub rows: Vec<MlAttributedRow>,
    /// Shadow records written during the run.
    pub shadow_records: u64,
    /// Samples the always-on ring lost during the run.
    pub lost_samples: u64,
}

/// A violation of ADR-0016 §7's ML row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateViolation {
    /// Clause 1: a `Classification` row is attributed to a model that is not `active` for anyone —
    /// which is precisely "shadow changed a Classification row" when that model is in shadow, and
    /// is no better when it is off or was never loaded.
    ClassificationFromANonActiveModel {
        /// The offending row's subject.
        subject: String,
        /// The model it names, or its stage when it names none.
        model: String,
        /// The mode that model is in, `None` when no mode is in force for it at all.
        mode: Option<MlMode>,
    },
    /// Clause 2: an `active` mode without the §4.6 enable evidence behind it — including one
    /// reached by `force`, which is audited rather than refused and so can exist.
    ActiveWithoutEnableEvidence {
        /// `id@version`.
        model: String,
        /// The consumer.
        consumer: String,
        /// Whether it got there by `force`.
        forced: bool,
    },
    /// Clause 3: the ring lost samples on a run with ML on.
    SamplesLostWithMlOn {
        /// Samples lost.
        lost: u64,
        /// Why ML counts as on for this run.
        why: String,
    },
}

impl fmt::Display for GateViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateViolation::ClassificationFromANonActiveModel {
                subject,
                model,
                mode,
            } => write!(
                f,
                "ADR-0016 §7 (shadow changes no Classification row): {subject} carries a \
                 classification attributed to {model}, which is in {} mode — only an `active` \
                 model with enable evidence may decide anything",
                mode.map_or("no", MlMode::as_str),
            ),
            GateViolation::ActiveWithoutEnableEvidence {
                model,
                consumer,
                forced,
            } => write!(
                f,
                "ADR-0016 §7 (each active family has enable evidence): {model} is active for \
                 {consumer}{} with no §4.6 enable evidence on its manifest",
                if *forced { " by force" } else { "" },
            ),
            GateViolation::SamplesLostWithMlOn { lost, why } => write!(
                f,
                "ADR-0016 §7 (zero ring sample drops with ML on): the ring lost {lost} samples on \
                 a run with ML on ({why})",
            ),
        }
    }
}

/// The `id@version` key of a model reference that may carry a `#sha8` suffix.
///
/// A mode is held under [`model_key`] (`id@version`); a `Classification` row stores the full
/// `id@version#sha8`. Two spellings of one model, so the gate compares the key.
fn key_of(model: &str) -> &str {
    model.split('#').next().unwrap_or(model)
}

impl MlGateSnapshot {
    /// Whether ML was on for this run at all, and why — the antecedent of clause 3.
    ///
    /// On means *anything* ran or could have: a mode above `off`, a shadow record written, or a
    /// classification attributed to a model. `None` means ML was off, which is the honest state
    /// while the host is dormant (T-363) and is reported as such rather than counted as a pass.
    pub fn ml_on(&self) -> Option<String> {
        let live: Vec<_> = self
            .modes
            .iter()
            .filter(|m| m.mode != MlMode::Off)
            .map(|m| format!("{} {} for {}", m.model, m.mode.as_str(), m.consumer))
            .collect();
        if !live.is_empty() {
            return Some(live.join(", "));
        }
        if self.shadow_records > 0 {
            return Some(format!("{} shadow records", self.shadow_records));
        }
        if !self.rows.is_empty() {
            return Some(format!(
                "{} ML-attributed classification rows",
                self.rows.len()
            ));
        }
        None
    }

    /// Every violation of ADR-0016 §7's ML row in this state. Empty is the gate passing.
    pub fn check(&self) -> Vec<GateViolation> {
        let mut out = Vec::new();

        // Clause 1: shadow changes no Classification row. Stated over what was persisted rather
        // than over what shadow was asked to do, because the row is about the outcome: a
        // classification may only be attributed to a model that is `active` for some consumer.
        for row in &self.rows {
            let named = row.model.as_deref();
            let mode = named.and_then(|m| {
                self.modes
                    .iter()
                    .filter(|e| e.model == key_of(m))
                    .map(|e| e.mode)
                    .max_by_key(|m| match m {
                        MlMode::Active => 2,
                        MlMode::Shadow => 1,
                        MlMode::Off => 0,
                    })
            });
            if mode != Some(MlMode::Active) {
                out.push(GateViolation::ClassificationFromANonActiveModel {
                    subject: row.subject.clone(),
                    model: named
                        .map_or_else(|| format!("no model, stage {}", row.stage), str::to_owned),
                    mode,
                });
            }
        }

        // Clause 2: each `active` family has enable evidence.
        for m in &self.modes {
            if m.mode == MlMode::Active && (m.enable_evidence.is_none() || m.forced) {
                out.push(GateViolation::ActiveWithoutEnableEvidence {
                    model: m.model.clone(),
                    consumer: m.consumer.clone(),
                    forced: m.forced,
                });
            }
        }

        // Clause 3: zero ring sample drops with ML on.
        if self.lost_samples > 0 {
            if let Some(why) = self.ml_on() {
                out.push(GateViolation::SamplesLostWithMlOn {
                    lost: self.lost_samples,
                    why,
                });
            }
        }

        out
    }

    /// One line for the gate's log, naming what was measured **and what was not exercised**, so a
    /// green ML row is never mistaken for evidence that ML ran.
    pub fn summary(&self) -> String {
        format!(
            "ML: {}; modes {}, ML-attributed classification rows {}, shadow records {}, lost \
             samples {}",
            self.ml_on().map_or_else(
                || "off (clause 3 not exercised)".to_owned(),
                |w| format!("on — {w}")
            ),
            self.modes.len(),
            self.rows.len(),
            self.shadow_records,
            self.lost_samples,
        )
    }

    /// The snapshot a **live host** is in: its modes, the evidence on each loaded manifest and its
    /// shadow-record counter, plus the rows and lost samples the caller measured elsewhere.
    ///
    /// This is the seam the wiring change takes: once a host exists in the pipeline, the
    /// acceptance gate builds its snapshot here instead of asserting the dormant state, and every
    /// clause above starts biting on real modes.
    pub fn from_host(host: &ModelHost, rows: Vec<MlAttributedRow>, lost_samples: u64) -> Self {
        let evidence: Vec<(String, Option<String>)> = host
            .loaded()
            .iter()
            .map(|m| (model_key(&m.model), m.enable_evidence.clone()))
            .collect();
        let modes = host
            .modes()
            .into_iter()
            .map(|(model, consumer, entry)| ModeInForce {
                enable_evidence: evidence
                    .iter()
                    .find(|(k, _)| *k == model)
                    .and_then(|(_, e)| e.clone()),
                model,
                consumer: consumer.to_string(),
                mode: entry.mode,
                forced: entry.forced,
            })
            .collect();
        Self {
            modes,
            rows,
            shadow_records: host.stats().shadow_records,
            lost_samples,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(subject: &str, model: Option<&str>) -> MlAttributedRow {
        MlAttributedRow {
            subject: subject.to_owned(),
            model: model.map(str::to_owned),
            stage: "dl".to_owned(),
        }
    }

    fn mode(mode: MlMode, evidence: Option<&str>, forced: bool) -> ModeInForce {
        ModeInForce {
            model: "amc-fsk@1.0.0".to_owned(),
            consumer: "hk-classify/dl".to_owned(),
            mode,
            forced,
            enable_evidence: evidence.map(str::to_owned),
        }
    }

    /// The state the system is in today (T-363: the host is dormant), measured rather than
    /// declared: nothing loaded, no ML-attributed row, no sample lost. The gate passes — and
    /// [`MlGateSnapshot::summary`] says clause 3 was not exercised rather than claiming it held.
    #[test]
    fn the_dormant_state_passes_and_says_that_clause_three_was_not_exercised() {
        let snap = MlGateSnapshot::default();
        assert_eq!(snap.check(), []);
        assert_eq!(snap.ml_on(), None);
        assert!(
            snap.summary().contains("not exercised"),
            "{}",
            snap.summary()
        );
    }

    /// **Non-vacuity, clause 1.** A model running in shadow, and a classification attributed to
    /// it: the state ADR-0016 §6 forbids in as many words ("shadow never writes a Classification
    /// row"). The gate catches it.
    #[test]
    fn a_classification_from_a_shadow_model_is_caught() {
        let snap = MlGateSnapshot {
            modes: vec![mode(MlMode::Shadow, None, false)],
            rows: vec![row("emitter-7", Some("amc-fsk@1.0.0#1a2b3c4d"))],
            shadow_records: 12,
            lost_samples: 0,
        };
        assert_eq!(
            snap.check(),
            [GateViolation::ClassificationFromANonActiveModel {
                subject: "emitter-7".to_owned(),
                model: "amc-fsk@1.0.0#1a2b3c4d".to_owned(),
                mode: Some(MlMode::Shadow),
            }]
        );
        assert!(snap.check()[0].to_string().contains("shadow"));

        // The mirror: the same row with the model `active` **and** its evidence is not a
        // violation, so the rule is about shadow deciding and not about ML existing.
        let ok = MlGateSnapshot {
            modes: vec![mode(MlMode::Active, Some("dev/amc-eval@1"), false)],
            ..snap
        };
        assert_eq!(ok.check(), []);
    }

    /// A row that names no model but claims the DL stage is caught too: a model changed a
    /// classification and the gate cannot even say which one.
    #[test]
    fn a_dl_row_naming_no_model_is_caught() {
        let snap = MlGateSnapshot {
            rows: vec![row("emitter-9", None)],
            ..MlGateSnapshot::default()
        };
        let v = snap.check();
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].to_string().contains("no model, stage dl"), "{v:?}");
    }

    /// **Non-vacuity, clause 2**, both ways it can happen: an `active` mode whose manifest carries
    /// no evidence, and one that reached `active` by `force` (which the host audits rather than
    /// refuses, so the gate is the thing that has to notice).
    #[test]
    fn an_active_model_without_enable_evidence_is_caught_however_it_got_there() {
        for (evidence, forced) in [(None, false), (Some("dev/amc-eval@1"), true)] {
            let snap = MlGateSnapshot {
                modes: vec![mode(MlMode::Active, evidence, forced)],
                ..MlGateSnapshot::default()
            };
            assert_eq!(
                snap.check(),
                [GateViolation::ActiveWithoutEnableEvidence {
                    model: "amc-fsk@1.0.0".to_owned(),
                    consumer: "hk-classify/dl".to_owned(),
                    forced,
                }],
                "evidence={evidence:?} forced={forced}"
            );
        }
        // Evidence, not forced: allowed.
        let ok = MlGateSnapshot {
            modes: vec![mode(MlMode::Active, Some("dev/amc-eval@1"), false)],
            ..MlGateSnapshot::default()
        };
        assert_eq!(ok.check(), []);
    }

    /// **Non-vacuity, clause 3.** Lost samples are a violation *when ML is on* and are not this
    /// row's business when it is off — the run's own suites assert loss-free capture either way,
    /// and pretending this row covers it would be the vacuity the ticket is about.
    #[test]
    fn samples_lost_are_a_violation_only_with_ml_on() {
        let off = MlGateSnapshot {
            lost_samples: 4096,
            ..MlGateSnapshot::default()
        };
        assert_eq!(off.check(), []);

        for on in [
            MlGateSnapshot {
                modes: vec![mode(MlMode::Shadow, None, false)],
                lost_samples: 4096,
                ..MlGateSnapshot::default()
            },
            MlGateSnapshot {
                shadow_records: 1,
                lost_samples: 4096,
                ..MlGateSnapshot::default()
            },
        ] {
            let v = on.check();
            assert!(
                v.contains(&GateViolation::SamplesLostWithMlOn {
                    lost: 4096,
                    why: on.ml_on().unwrap(),
                }),
                "{v:?}"
            );
        }
    }

    /// A mode in force for one consumer does not license a row for another model: the match is on
    /// the model key, with the `#sha8` a `Classification` row carries stripped.
    #[test]
    fn the_sha_suffix_of_a_persisted_row_still_matches_its_mode() {
        let snap = MlGateSnapshot {
            modes: vec![mode(MlMode::Active, Some("dev/amc-eval@1"), false)],
            rows: vec![
                row("emitter-1", Some("amc-fsk@1.0.0#deadbeef")),
                row("emitter-2", Some("amc-psk@1.0.0#deadbeef")),
            ],
            ..MlGateSnapshot::default()
        };
        let v = snap.check();
        assert_eq!(v.len(), 1, "only the unmodelled row violates: {v:?}");
        assert!(v[0].to_string().contains("amc-psk"), "{v:?}");
    }
}
