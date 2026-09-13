//! Content classification for restricted-content gating (ADR-0004; docs/07 §2.12, §2.15, §2.16).
//!
//! Every object that can carry *content* (a Recording's samples, a Decode's fields, a Bitstream,
//! an Annotation value) carries a [`ContentClass`]. The model only records the class. The
//! stream-output contract (C24, ADR-0004) is the provisional enforcement point, and recording
//! (C25) must also refuse to retain gated content.
//!
//! **Legal guardrail area** (CLAUDE.md, docs/04 §1.3). Metadata (that a signal exists, its
//! frequency, timing, parameters, and non-content identifiers such as a transmitter id) always
//! flows. What is gated is the *content*. The variants and [`ContentClass::permits_content`]
//! encode the ADR-0004 default and are **provisional** pending the legal-guardrail confirmation
//! with the user. Changing them is a core-interface change.

use serde::{Deserialize, Serialize};

/// What may be done with the content of a signal, decode or recording.
///
/// There is deliberately no `Default`: the producer (decoder plugin, recorder, prior lookup) must
/// choose a class explicitly, so nothing becomes `Unrestricted` by omission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentClass {
    /// No known restriction on content, e.g. broadcast FM/RDS, ADS-B, AIS, amateur telemetry,
    /// ISM sensor payloads, radiosondes, unknown bursts with no restricted-service prior.
    /// Content may be stored, streamed and displayed.
    Unrestricted,
    /// Someone else's encrypted or otherwise protected traffic. Detect and label **metadata
    /// only**: never store or stream the payload, and never attempt to circumvent its security
    /// (CLAUDE.md legal guardrails).
    MetadataOnly,
    /// Cellular service content (voice, SMS, user data), restricted in the US even when
    /// unencrypted (ECPA; docs/04 §1.3). Metadata such as cell/broadcast-channel identifiers
    /// may flow; content is dropped before any Recording or egress.
    RestrictedCellular,
    /// Common-carrier paging message content (POCSAG/FLEX pager messages), restricted in the US
    /// (docs/04 §1.3). Message-rate and address metadata may flow; message text is dropped.
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

    /// Whether the *content* (payload, message text, audio, IQ carrying that content) may be
    /// retained in a Recording or passed to stream-output. Metadata is never gated.
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
}
