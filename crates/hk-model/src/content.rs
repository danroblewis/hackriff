//! Content classification for restricted-content gating (ADR-0004; docs/07 §2.12, §2.15, §2.16).
//!
//! Every object that can carry *content* (a Recording's samples, a Decode's content fields, a
//! stored Bitstream, an Annotation's content) carries a [`ContentClass`].
//!
//! **Legal guardrail area** (CLAUDE.md, docs/04 §1.3). The variants and
//! [`ContentClass::permits_content`] encode the ADR-0004 default and are **provisional** pending
//! the legal-guardrail confirmation with the user. Changing them is a core-interface change.
//!
//! # Metadata always flows; content is gated
//!
//! *Metadata* is everything that describes a transmission without disclosing what it says: that
//! a signal exists, its frequency, bandwidth, timing, estimated parameters, frame type, CRC
//! result, lengths, and non-content identifiers (a transmitter/cell/pager address). Metadata is
//! never gated: it is stored, indexed and streamed whatever the class.
//!
//! *Content* is the message itself: payload bytes, message text, voice/audio, and IQ or bits that
//! carry them. Objects keep the two apart (`Decode::metadata` vs `Decode::content`,
//! `Annotation::metadata` vs `Annotation::content`), and the repository enforces the class:
//!
//! - `insert_decode` / `insert_annotation` refuse `content: Some(..)` unless the class permits
//!   content; the metadata-only form is accepted.
//! - `insert_recording` refuses any class that does not permit content (IQ and audio are content).
//! - `insert_bitstream` refuses a *stored* bitstream under such a class; a *live* descriptor is
//!   metadata, and stream-output (C24) gates the stream itself.
//!
//! The schema repeats these rules as CHECK constraints for writers that bypass the repository.
//!
//! # Fail closed
//!
//! There is no `Default`, no serde default and no fallback to [`ContentClass::Unrestricted`].
//! Producers must choose a class. When a class is missing or unrecognised, use
//! [`ContentClass::FAIL_CLOSED`] (`MetadataOnly`) via [`ContentClass::parse_fail_closed`].

use serde::{Deserialize, Serialize};

/// What may be done with the content of a signal, decode or recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentClass {
    /// No known restriction on content, e.g. broadcast FM/RDS, ADS-B, AIS, amateur telemetry,
    /// ISM sensor payloads, radiosondes. Content may be stored, streamed and displayed. Must be
    /// chosen **positively** by a producer that knows the service (decoder, band prior); never
    /// assumed for an unclassified signal.
    Unrestricted,
    /// Someone else's encrypted or otherwise protected traffic, and the fail-closed class for
    /// anything unclassified. Detect and label **metadata only**: never store or stream the
    /// payload, and never attempt to circumvent its security (CLAUDE.md legal guardrails).
    MetadataOnly,
    /// Cellular service content (voice, SMS, user data), restricted in the US even when
    /// unencrypted (ECPA; docs/04 §1.3). Metadata such as cell/broadcast-channel identifiers
    /// flows; content is refused.
    RestrictedCellular,
    /// Common-carrier paging message content (POCSAG/FLEX pager messages), restricted in the US
    /// (docs/04 §1.3). Message-rate and address metadata flows; message text is refused.
    RestrictedPaging,
    /// The user's **own** traffic, decrypted with the user's own key. Permitted for any research
    /// purpose (CLAUDE.md legal guardrails). Key-source provenance belongs to the decoder stage
    /// (docs/07 §6 open question). Content may be stored and streamed locally.
    OwnKeyDecrypted,
}

impl ContentClass {
    /// Every variant, for exhaustive tests and UI listings.
    pub const ALL: &'static [ContentClass] = &[
        ContentClass::Unrestricted,
        ContentClass::MetadataOnly,
        ContentClass::RestrictedCellular,
        ContentClass::RestrictedPaging,
        ContentClass::OwnKeyDecrypted,
    ];

    /// The class to use when a classification is missing or unrecognised. `MetadataOnly` is the
    /// most restrictive class that still lets metadata flow: content is refused.
    pub const FAIL_CLOSED: ContentClass = ContentClass::MetadataOnly;

    /// Whether the *content* (payload, message text, audio, IQ carrying that content) may be
    /// retained or passed to stream-output. Metadata is never gated.
    ///
    /// ADR-0004 default, provisional.
    pub const fn permits_content(self) -> bool {
        match self {
            ContentClass::Unrestricted | ContentClass::OwnKeyDecrypted => true,
            ContentClass::MetadataOnly
            | ContentClass::RestrictedCellular
            | ContentClass::RestrictedPaging => false,
        }
    }

    /// Parses a class name such as `"restricted-paging"`, **failing closed**: `None`, an empty
    /// string or an unknown name gives [`Self::FAIL_CLOSED`]. For plugin manifests, decoder output
    /// and feeds where the class arrives as free text.
    pub fn parse_fail_closed(name: Option<&str>) -> ContentClass {
        name.and_then(|n| serde_json::from_value(serde_json::Value::String(n.to_owned())).ok())
            .unwrap_or(Self::FAIL_CLOSED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_names_and_gating() {
        let names: Vec<String> = ContentClass::ALL
            .iter()
            .map(|c| serde_json::to_string(c).unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "\"unrestricted\"",
                "\"metadata-only\"",
                "\"restricted-cellular\"",
                "\"restricted-paging\"",
                "\"own-key-decrypted\""
            ]
        );
        let permitted: Vec<_> = ContentClass::ALL
            .iter()
            .filter(|c| c.permits_content())
            .collect();
        assert_eq!(
            permitted,
            [&ContentClass::Unrestricted, &ContentClass::OwnKeyDecrypted]
        );
    }

    #[test]
    fn missing_or_unknown_classes_fail_closed() {
        assert!(!ContentClass::FAIL_CLOSED.permits_content());
        for missing in [None, Some(""), Some("unclassified"), Some("Unrestricted")] {
            assert_eq!(
                ContentClass::parse_fail_closed(missing),
                ContentClass::MetadataOnly,
                "{missing:?}"
            );
        }
        for class in ContentClass::ALL {
            let name = serde_json::to_value(class).unwrap();
            assert_eq!(ContentClass::parse_fail_closed(name.as_str()), *class);
        }
        // A missing class in JSON is an error, never a silent default.
        assert!(serde_json::from_str::<ContentClass>("null").is_err());
    }
}
