//! Content classification for optional content gating (ADR-0004; docs/07 §2.12, §2.15, §2.16).
//!
//! Every object that can carry *content* (a Recording's samples, a Decode's content fields, a
//! stored Bitstream, an Annotation's content) carries a [`ContentClass`].
//!
//! # Gating is off by default (T-143)
//!
//! Legality is the user's concern, not the software's. By default **nothing is gated**: every
//! class permits content ([`ContentClass::permits_content`] is `true`), identities are shown in
//! clear, and recordings, audio, chains and streams flow regardless of band. Classes are still
//! derived and reported, as information only. The rules below apply only when gating is
//! explicitly enabled: `HK_CONTENT_GATING=1` in the environment, or [`set_content_gating`]. This
//! opt-in path is deliberately untested.
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
//! (Migration 0005 removed the schema CHECKs that used to repeat these rules.)
//!
//! # Fail closed
//!
//! There is no `Default`, no serde default and no fallback to [`ContentClass::Unrestricted`].
//! Producers must choose a class. When a class is missing or unrecognised, use
//! [`ContentClass::FAIL_CLOSED`] (`MetadataOnly`) via [`ContentClass::parse_fail_closed`].

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// 0 = not yet read from the environment, 1 = off, 2 = on.
static CONTENT_GATING: AtomicU8 = AtomicU8::new(0);

/// Whether content gating is enabled (default **off**; opt in with `HK_CONTENT_GATING=1` or
/// [`set_content_gating`]). When off, no class withholds or refuses anything.
pub fn content_gating_enabled() -> bool {
    match CONTENT_GATING.load(Ordering::Relaxed) {
        0 => {
            let on = std::env::var("HK_CONTENT_GATING")
                .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "on" | "yes"));
            CONTENT_GATING.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
        v => v == 2,
    }
}

/// Enables or disables content gating for this process (overrides `HK_CONTENT_GATING`).
pub fn set_content_gating(enabled: bool) {
    CONTENT_GATING.store(if enabled { 2 } else { 1 }, Ordering::Relaxed);
}

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
    /// retained or passed to stream-output. Metadata is never gated. Always `true` unless
    /// [`content_gating_enabled`].
    pub fn permits_content(self) -> bool {
        if !content_gating_enabled() {
            return true;
        }
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
    fn serde_names_round_trip() {
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
        for class in ContentClass::ALL {
            let name = serde_json::to_value(class).unwrap();
            assert_eq!(ContentClass::parse_fail_closed(name.as_str()), *class);
        }
    }
}
