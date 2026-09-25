//! The decoder-synthesis evidence vocabulary (ADR-0015 §1.1, §2.1, §13.1; T-848 = MAUTO M-1).
//!
//! These are the types a **block** emits and the **search** scores: the closed stage ladder
//! [`Stage`], the closed metric set [`MetricId`], the dependence group [`GroupId`] and one
//! [`Evidence`] summary, collected per evaluation window into an [`EvidenceSet`].
//!
//! **Why here and not in `hk-synth`.** ADR-0015 §9 puts `Block::evidence(&self, out: &mut
//! EvidenceSet)` in `hk-blocks` and the scoring in `hk-synth`, and `hk-synth` depends on
//! `hk-blocks` (the search drives blocks through `run_window`). A type both sides name must
//! therefore live below both, exactly as ADR-0016's contract types live in
//! [`crate::classify`] and `hk-classify` re-exports them. `hk-synth` re-exports every item
//! here, so `hk_synth::Stage` is the name the ADR uses. (Recorded in ADR-0015 §16, delta D1.)
//!
//! This module holds data plus the **analytic** nulls ([`null`], T-853): no calibration and no
//! scoring. A `bits` value in an [`Evidence`] is what the emitting block computed against its
//! own null — the closed-form tail for an analytic metric ([`MetricId::is_analytic`]), and
//! **0.0** for a calibrated one, which `hk-synth` scores against the block's calibration table
//! (a block cannot read a table: `hk-synth` owns them). The engine (not the block) subtracts the
//! look-elsewhere cost (ADR-0015 §2.2).

use serde::{Deserialize, Serialize};

pub mod null;

/// The closed synthesis stage ladder, S0–S6 (ADR-0015 §1.1). Adding a stage is a contract change.
///
/// Serialised as `"S0"` … `"S6"`, including as a map key (a candidate's `choices`, a template's
/// `evidence_targets`). Ordered by depth, so `stage_reached` compares naturally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Stage {
    /// Channel: runtime DDC, `mix`, `lowpass`, `resample`.
    S0,
    /// Demodulation: `fm_demod`, `am_demod`, `fsk_demod`, `msk_demod`, `psk_demod`, `subcarrier`,
    /// `stereo_decode`.
    S1,
    /// Clock recovery: `clock_recovery`.
    S2,
    /// Bits: `slicer`, `diff_decode`, `nrzi`, `manchester`.
    S3,
    /// Framing: `sync_search`, `deframe`, `assemble`, `interleave`.
    S4,
    /// Check: `crc`, `bch`, `parity`, `checksum`.
    S5,
    /// Fields: `fields`, `text`.
    S6,
}

impl Stage {
    /// Every stage, shallowest first.
    pub const ALL: [Stage; 7] = [
        Stage::S0,
        Stage::S1,
        Stage::S2,
        Stage::S3,
        Stage::S4,
        Stage::S5,
        Stage::S6,
    ];

    /// Depth, 0–6.
    pub const fn index(self) -> u8 {
        self as u8
    }

    /// The stage at `depth`, if 0–6.
    pub const fn from_index(depth: u8) -> Option<Stage> {
        match depth {
            0 => Some(Stage::S0),
            1 => Some(Stage::S1),
            2 => Some(Stage::S2),
            3 => Some(Stage::S3),
            4 => Some(Stage::S4),
            5 => Some(Stage::S5),
            6 => Some(Stage::S6),
            _ => None,
        }
    }

    /// The wire name, `"S0"` … `"S6"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Stage::S0 => "S0",
            Stage::S1 => "S1",
            Stage::S2 => "S2",
            Stage::S3 => "S3",
            Stage::S4 => "S4",
            Stage::S5 => "S5",
            Stage::S6 => "S6",
        }
    }
}

/// The closed evidence-metric set (ADR-0015 §2.1). Adding a metric is a contract change; §13
/// adds none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricId {
    /// In-band SNR vs adjacent guard bands (S0), or soft-symbol SNR (S2–S3). Calibrated null.
    Snr,
    /// Discriminator or envelope bimodality (S1). Calibrated null.
    Bimodality,
    /// FSK/FM offset-to-deviation ratio (S1). Calibrated null.
    OffsetRatio,
    /// Pilot or subcarrier lock (S1). Calibrated null.
    PilotLock,
    /// Eye openness (S2). Calibrated null.
    EyeOpen,
    /// Timing-error variance (S2). Calibrated null.
    TimingVar,
    /// Error-vector magnitude of the soft symbols (S3). Calibrated null.
    Evm,
    /// Line-code violation rate, e.g. Manchester (S3). Calibrated null.
    LineViolations,
    /// Bit-structure sanity: neither constant nor all-toggle (S3). Calibrated null.
    BitStructure,
    /// Sync hits in excess of chance (S4). **Analytic** null.
    SyncExcess,
    /// Inter-sync regularity (S4). **Analytic** null.
    SyncRegularity,
    /// Distinct valid frames × check width (S5). **Analytic** null.
    CheckDistinctValid,
    /// Field-map fit share (S6). **Analytic** null.
    FieldFit,
    /// Identity recurrence across frames (S6). **Analytic** null.
    IdentityRecurrence,
    /// Template plausibility ranges met by measured frames (S6). **Analytic** null.
    Plausibility,
}

impl MetricId {
    /// Every metric, in declaration order.
    pub const ALL: [MetricId; 15] = [
        MetricId::Snr,
        MetricId::Bimodality,
        MetricId::OffsetRatio,
        MetricId::PilotLock,
        MetricId::EyeOpen,
        MetricId::TimingVar,
        MetricId::Evm,
        MetricId::LineViolations,
        MetricId::BitStructure,
        MetricId::SyncExcess,
        MetricId::SyncRegularity,
        MetricId::CheckDistinctValid,
        MetricId::FieldFit,
        MetricId::IdentityRecurrence,
        MetricId::Plausibility,
    ];

    /// Whether the metric's null is **analytic** (ADR-0015 §2.2's first list). Only analytic-null
    /// bits may pay toward a confirm (ADR-0022 §2.1); calibrated bits order and rank the search.
    pub const fn is_analytic(self) -> bool {
        matches!(
            self,
            MetricId::SyncExcess
                | MetricId::SyncRegularity
                | MetricId::CheckDistinctValid
                | MetricId::FieldFit
                | MetricId::IdentityRecurrence
                | MetricId::Plausibility
        )
    }

    /// Whether the metric's analytic null is one **ADR-0022 §2.1's table lists as contributing**
    /// to `analytic_holdout_bits` — the confirm key.
    ///
    /// This is deliberately **narrower** than [`Self::is_analytic`], which answers a different
    /// question (does this metric have a closed-form tail, so no calibration table is consulted).
    /// §2.1's table names exactly `check_distinct_valid`, `sync_excess`, `field_fit` and
    /// `identity_recurrence`; `sync_regularity` and `plausibility` are not in it, so they rank,
    /// prune and report like any other metric but **never pay for an irreversible confirm**. On a
    /// one-way door a metric the ADR did not price is charged as if it were calibrated.
    pub const fn pays_for_confirm(self) -> bool {
        matches!(
            self,
            MetricId::SyncExcess
                | MetricId::CheckDistinctValid
                | MetricId::FieldFit
                | MetricId::IdentityRecurrence
        )
    }

    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            MetricId::Snr => "snr",
            MetricId::Bimodality => "bimodality",
            MetricId::OffsetRatio => "offset_ratio",
            MetricId::PilotLock => "pilot_lock",
            MetricId::EyeOpen => "eye_open",
            MetricId::TimingVar => "timing_var",
            MetricId::Evm => "evm",
            MetricId::LineViolations => "line_violations",
            MetricId::BitStructure => "bit_structure",
            MetricId::SyncExcess => "sync_excess",
            MetricId::SyncRegularity => "sync_regularity",
            MetricId::CheckDistinctValid => "check_distinct_valid",
            MetricId::FieldFit => "field_fit",
            MetricId::IdentityRecurrence => "identity_recurrence",
            MetricId::Plausibility => "plausibility",
        }
    }
}

/// A declared dependence group (ADR-0015 §13.1). Within a group a stage scores the **maximum**
/// of its metrics' bits; only distinct groups sum.
///
/// Closed, like [`MetricId`]: the named groups are the ones §13.1's measurement declared, and a
/// block that wants a new one ships the correlation matrix that justifies it and amends the
/// contract. [`GroupId::Undeclared`] is the default and is **one group per (block, stage)** —
/// undeclared is not independent.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum GroupId {
    /// No partition declared: every metric the block publishes at the stage is one group.
    #[default]
    Undeclared,
    /// S1 `bimodality` (singleton).
    DemodShape,
    /// S1 `pilot_lock` + `offset_ratio` (unmeasured, so one group by the default rule).
    Pilot,
    /// S2–S3 `snr` + `evm` + `timing_var` (ρ 0.989 / 0.52).
    SoftQuality,
    /// S2 `eye_open` (singleton).
    Eye,
    /// S3 `line_violations` + `bit_structure` (ρ 0.706).
    BitShape,
}

/// One evidence summary (ADR-0015 §2.1, with §13.1's `group`).
///
/// Emitted by a block between chunks, covering everything since the engine's last `reset()`.
/// `Copy` and fixed-size so emitting it never allocates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    /// The ladder stage this evidence speaks for.
    pub stage: Stage,
    /// Which metric.
    pub metric: MetricId,
    /// Its declared dependence group.
    #[serde(default)]
    pub group: GroupId,
    /// The metric in its natural unit.
    pub raw: f32,
    /// Support: symbols, bursts or distinct frames.
    pub n: u32,
    /// Significance against the block's null for that `n`, in bits. Look-elsewhere is **not**
    /// subtracted here; the engine does that.
    pub bits: f32,
    /// `1 − e^(−bits/8)` ∈ [0, 1], for display only ([`quality_from_bits`]).
    pub quality: f32,
}

impl Evidence {
    /// An evidence record with `quality` derived from `bits`.
    pub fn new(
        stage: Stage,
        metric: MetricId,
        group: GroupId,
        raw: f32,
        n: u32,
        bits: f32,
    ) -> Self {
        Self {
            stage,
            metric,
            group,
            raw,
            n,
            bits,
            quality: quality_from_bits(bits),
        }
    }
}

/// The display quality of a significance: `1 − e^(−bits/8)`, clamped to [0, 1] (ADR-0015 §2.1).
/// Non-finite or negative bits read as 0.
pub fn quality_from_bits(bits: f32) -> f32 {
    if !bits.is_finite() || bits <= 0.0 {
        return 0.0;
    }
    (1.0 - (-bits / 8.0).exp()).clamp(0.0, 1.0)
}

/// Most entries a block may emit per `evidence()` call (ADR-0015 §2.1: "≤ 4 entries").
pub const EVIDENCE_SET_CAPACITY: usize = 4;

/// The fixed-capacity collector passed to `Block::evidence` (ADR-0015 §2.1). No allocation: a
/// fifth entry is refused, not grown into.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EvidenceSet {
    entries: [Option<Evidence>; EVIDENCE_SET_CAPACITY],
    len: usize,
}

/// The block tried to emit more than [`EVIDENCE_SET_CAPACITY`] entries in one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvidenceSetFull;

impl std::fmt::Display for EvidenceSetFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a block may emit at most {EVIDENCE_SET_CAPACITY} evidence entries per call"
        )
    }
}

impl std::error::Error for EvidenceSetFull {}

impl EvidenceSet {
    /// An empty set.
    pub const fn new() -> Self {
        Self {
            entries: [None; EVIDENCE_SET_CAPACITY],
            len: 0,
        }
    }

    /// Adds one entry, or refuses when the set is full.
    pub fn push(&mut self, e: Evidence) -> Result<(), EvidenceSetFull> {
        if self.len == EVIDENCE_SET_CAPACITY {
            return Err(EvidenceSetFull);
        }
        self.entries[self.len] = Some(e);
        self.len += 1;
        Ok(())
    }

    /// Empties the set for the next call.
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// Number of entries.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether no entry was emitted.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The entries, in emission order.
    pub fn iter(&self) -> impl Iterator<Item = &Evidence> {
        self.entries[..self.len].iter().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_serialises_as_s_n_including_as_a_map_key() {
        assert_eq!(serde_json::to_string(&Stage::S4).unwrap(), "\"S4\"");
        let m: std::collections::BTreeMap<Stage, &str> = [(Stage::S3, "nrzi")].into();
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"S3":"nrzi"}"#);
        for s in Stage::ALL {
            assert_eq!(Stage::from_index(s.index()), Some(s));
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{}\"", s.as_str())
            );
        }
        assert!(Stage::S0 < Stage::S6);
        assert_eq!(Stage::from_index(7), None);
    }

    #[test]
    fn metric_wire_names_match_the_adr_list() {
        let names: Vec<String> = MetricId::ALL
            .iter()
            .map(|m| {
                serde_json::to_value(m)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            names,
            [
                "snr",
                "bimodality",
                "offset_ratio",
                "pilot_lock",
                "eye_open",
                "timing_var",
                "evm",
                "line_violations",
                "bit_structure",
                "sync_excess",
                "sync_regularity",
                "check_distinct_valid",
                "field_fit",
                "identity_recurrence",
                "plausibility"
            ]
        );
        for m in MetricId::ALL {
            assert_eq!(serde_json::to_value(m).unwrap(), m.as_str());
        }
        // The S4–S6 metrics, and only those, have analytic nulls.
        let analytic: Vec<_> = MetricId::ALL.iter().filter(|m| m.is_analytic()).collect();
        assert_eq!(analytic.len(), 6);
    }

    /// T-884 item 7. ADR-0022 §2.1's table lists four metrics as contributing to the confirm key,
    /// and `sync_regularity`/`plausibility` are not among them — they have closed-form nulls (so
    /// [`MetricId::is_analytic`], which decides whether a calibration table is consulted, holds)
    /// but they may not pay for an irreversible confirm.
    #[test]
    fn t884_only_adr0022_2_1s_four_metrics_pay_for_a_confirm() {
        let pays: Vec<&str> = MetricId::ALL
            .iter()
            .filter(|m| m.pays_for_confirm())
            .map(|m| m.as_str())
            .collect();
        assert_eq!(
            pays,
            [
                "sync_excess",
                "check_distinct_valid",
                "field_fit",
                "identity_recurrence"
            ]
        );
        // Every payer has an analytic null; not every analytic null pays.
        for m in MetricId::ALL {
            assert!(!m.pays_for_confirm() || m.is_analytic(), "{m:?}");
        }
        assert!(MetricId::SyncRegularity.is_analytic());
        assert!(!MetricId::SyncRegularity.pays_for_confirm());
        assert!(MetricId::Plausibility.is_analytic());
        assert!(!MetricId::Plausibility.pays_for_confirm());
    }

    #[test]
    fn quality_is_one_minus_e_to_the_minus_bits_over_8() {
        assert_eq!(quality_from_bits(0.0), 0.0);
        assert_eq!(quality_from_bits(-3.0), 0.0);
        assert_eq!(quality_from_bits(f32::NAN), 0.0);
        assert!((quality_from_bits(8.0) - (1.0 - (-1.0f32).exp())).abs() < 1e-6);
        assert!(quality_from_bits(1e6) <= 1.0);
    }

    #[test]
    fn evidence_set_refuses_a_fifth_entry_and_group_defaults_to_undeclared() {
        let mut set = EvidenceSet::new();
        let e = Evidence::new(Stage::S2, MetricId::EyeOpen, GroupId::Eye, 0.8, 112, 3.0);
        for _ in 0..EVIDENCE_SET_CAPACITY {
            set.push(e).unwrap();
        }
        assert_eq!(set.push(e), Err(EvidenceSetFull));
        assert_eq!(set.iter().count(), EVIDENCE_SET_CAPACITY);
        set.clear();
        assert!(set.is_empty());

        let parsed: Evidence = serde_json::from_str(
            r#"{"stage":"S2","metric":"snr","raw":9.1,"n":112,"bits":4.0,"quality":0.39}"#,
        )
        .unwrap();
        assert_eq!(parsed.group, GroupId::Undeclared);
    }
}
