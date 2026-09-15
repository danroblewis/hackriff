//! Arbitration rank for an emitter's current family (ADR-0016 §2). **Core interface.**
//!
//! The current family is the classification with the lowest rank, latest among equals:
//! `ORDER BY coalesce(arb_rank, <derived>) ASC, classification_id DESC`.
//!
//! | Rank | [`ArbRank`] | Stages |
//! |---|---|---|
//! | 0 | `User` | `user` (explicit reclassify) |
//! | 1 | `Decoder` | `decoder` (CRC-valid decode) |
//! | 2 | `LockVerified` | `verifier`, or `chain` with a demod lock (pilot/RDS/clock lock) |
//! | 3 | `Classifier` | `feature-tree`, `dl`, or `chain` without a recorded lock |
//! | 4 | `TrackShape` | `track-shape` (occupancy only, T-183) |
//!
//! **Pre-M3 rows** (NULL `stage`/`arb_rank`) are ranked by [`ArbRank::legacy`]:
//! `model_version` starting `decoder:` → decoder (1); `input_kind = 'track'` → track shape (4);
//! anything else → `chain` at rank 3. ADR-0016 names the legacy stage (`chain`) but not its rank:
//! legacy chain rows record no lock, so they are **not** treated as lock-verified (conservative;
//! a chain writer that knows it is locked writes rank 2 explicitly through
//! `Repository::record_classification`). This keeps T-183 (track shape below everything) and
//! today's latest-wins among demodulator/classifier rows; the only ordering change for existing
//! writers is that a decoder row now outranks a chain row written after it, as the ADR requires.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::Stage;

/// `model_version` prefix of decoder evidence (`hk-pipeline::family::DECODER_EVIDENCE_PREFIX`).
pub const DECODER_RULES_PREFIX: &str = "decoder:";

/// `input_kind` of a track (occupancy shape) input.
const TRACK_INPUT_KIND: &str = "track";

/// Arbitration rank; lower wins. Serialises as its integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum ArbRank {
    /// 0: a user's explicit reclassification.
    User = 0,
    /// 1: a CRC-valid decode.
    Decoder = 1,
    /// 2: post-sync evidence (verifier, or a chain label with demod lock).
    LockVerified = 2,
    /// 3: pre-sync evidence (feature tree, DL, unlocked chain label).
    Classifier = 3,
    /// 4: track occupancy shape.
    TrackShape = 4,
}

impl ArbRank {
    /// Every rank, best first.
    pub const ALL: [ArbRank; 5] = [
        ArbRank::User,
        ArbRank::Decoder,
        ArbRank::LockVerified,
        ArbRank::Classifier,
        ArbRank::TrackShape,
    ];

    /// The stored integer.
    pub const fn value(self) -> u8 {
        self as u8
    }

    /// The rank stored as `v`.
    pub fn from_value(v: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|r| i64::from(r.value()) == v)
    }

    /// Whether a row of `stage` may carry this rank.
    pub fn allows(self, stage: Stage) -> bool {
        matches!(
            (stage, self),
            (Stage::User, ArbRank::User)
                | (Stage::Decoder, ArbRank::Decoder)
                | (Stage::Verifier, ArbRank::LockVerified)
                | (Stage::Chain, ArbRank::LockVerified | ArbRank::Classifier)
                | (Stage::FeatureTree | Stage::Dl, ArbRank::Classifier)
                | (Stage::TrackShape, ArbRank::TrackShape)
        )
    }

    /// The rank of `stage` when no lock is recorded (a `chain` row ranks as a classifier).
    pub fn for_stage(stage: Stage) -> Self {
        match stage {
            Stage::User => ArbRank::User,
            Stage::Decoder => ArbRank::Decoder,
            Stage::Verifier => ArbRank::LockVerified,
            Stage::Chain | Stage::FeatureTree | Stage::Dl => ArbRank::Classifier,
            Stage::TrackShape => ArbRank::TrackShape,
        }
    }

    /// Stage and rank of a pre-M3 row from its legacy columns (see the module docs). Must agree
    /// with the repository's SQL derivation (tested there).
    pub fn legacy(model_version: &str, input_kind: Option<&str>) -> (Stage, ArbRank) {
        if model_version.starts_with(DECODER_RULES_PREFIX) {
            (Stage::Decoder, ArbRank::Decoder)
        } else if input_kind == Some(TRACK_INPUT_KIND) {
            (Stage::TrackShape, ArbRank::TrackShape)
        } else {
            (Stage::Chain, ArbRank::Classifier)
        }
    }
}

impl Serialize for ArbRank {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(self.value())
    }
}

impl<'de> Deserialize<'de> for ArbRank {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = i64::deserialize(d)?;
        Self::from_value(v)
            .ok_or_else(|| serde::de::Error::custom(format!("arb_rank {v} not 0..=4")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAGES: [Stage; 7] = [
        Stage::FeatureTree,
        Stage::Verifier,
        Stage::Dl,
        Stage::Decoder,
        Stage::User,
        Stage::Chain,
        Stage::TrackShape,
    ];

    #[test]
    fn ranks_are_ordered_as_the_adr() {
        assert_eq!(
            ArbRank::ALL.map(ArbRank::value),
            [0, 1, 2, 3, 4],
            "user > decoder > lock-verified > classifier > track shape"
        );
        for w in ArbRank::ALL.windows(2) {
            assert!(w[0] < w[1]);
        }
        for r in ArbRank::ALL {
            assert_eq!(ArbRank::from_value(i64::from(r.value())), Some(r));
            assert_eq!(serde_json::to_string(&r).unwrap(), r.value().to_string());
            let back: ArbRank = serde_json::from_str(&r.value().to_string()).unwrap();
            assert_eq!(back, r);
        }
        assert_eq!(ArbRank::from_value(5), None);
        assert_eq!(ArbRank::from_value(-1), None);
        assert!(serde_json::from_str::<ArbRank>("7").is_err());
    }

    #[test]
    fn each_stage_allows_its_default_rank_and_only_chain_may_be_lock_verified() {
        for s in STAGES {
            assert!(ArbRank::for_stage(s).allows(s), "{s:?}");
            let allowed: Vec<_> = ArbRank::ALL.into_iter().filter(|r| r.allows(s)).collect();
            let expected = match s {
                Stage::Chain => vec![ArbRank::LockVerified, ArbRank::Classifier],
                _ => vec![ArbRank::for_stage(s)],
            };
            assert_eq!(allowed, expected, "{s:?}");
        }
    }

    #[test]
    fn legacy_rows_derive_decoder_track_and_chain() {
        use ArbRank as R;
        for (mv, kind, want) in [
            ("decoder:readsb", None, (Stage::Decoder, R::Decoder)),
            (
                "decoder:readsb",
                Some("track"),
                (Stage::Decoder, R::Decoder),
            ),
            (
                "hk-pipeline/family-map@1",
                Some("track"),
                (Stage::TrackShape, R::TrackShape),
            ),
            (
                "hk-demod/mode-rules@1",
                Some("demodulation"),
                (Stage::Chain, R::Classifier),
            ),
            (
                "hk-demod/c20-fsk@0.1.0",
                Some("decode"),
                (Stage::Chain, R::Classifier),
            ),
            ("mode-rules@1", None, (Stage::Chain, R::Classifier)),
            ("Decoder:x", None, (Stage::Chain, R::Classifier)),
        ] {
            assert_eq!(ArbRank::legacy(mv, kind), want, "{mv} {kind:?}");
        }
    }
}
