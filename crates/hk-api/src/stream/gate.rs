//! Egress gating (ADR-0004; docs/stream-contract.md §6). **Legal guardrail.**
//!
//! This module is the single enforcement point for restricted content leaving hackriff on a
//! stream. The publisher calls it for every record before any byte is produced:
//!
//! - **Messages:** the record's class is clamped to the stream header's class ([`clamp`]). If the
//!   effective class does not [`ContentClass::permits_content`], `content` is not serialised and
//!   `gated: true` is set. Metadata always flows.
//! - **Binary records:** if the kind's payload is content ([`StreamKind::payload_is_content`]) and
//!   the header class does not permit content, the payload is withheld: a header-only record
//!   with [`RecordFlags::GATED`](super::RecordFlags::GATED) is emitted and the publisher returns
//!   an error so the misrouted producer notices.
//! - A missing or unknown class, wherever it is parsed (headers, plugin output, raw JSON), is
//!   [`ContentClass::FAIL_CLOSED`].
//!
//! The internal plugin data plane ([`super::DecoderFeed`]) is deliberately *not* gated: decoders
//! must see samples to decode them; their output is clamped by the plugin host and gated again
//! here on the way out.

use hk_model::ContentClass;

use super::header::StreamKind;

/// Restrictiveness rank: `unrestricted` (0) < `own-key-decrypted` (1, the user's own content,
/// local use) < classes that forbid content (2).
pub const fn restrictiveness(class: ContentClass) -> u8 {
    match class {
        ContentClass::Unrestricted => 0,
        ContentClass::OwnKeyDecrypted => 1,
        ContentClass::MetadataOnly
        | ContentClass::RestrictedCellular
        | ContentClass::RestrictedPaging => 2,
    }
}

/// Clamps a claimed class to a ceiling: a claim less restrictive than the ceiling becomes the
/// ceiling; an equal or more restrictive claim is kept (a producer may restrict itself further).
pub const fn clamp(ceiling: ContentClass, claimed: ContentClass) -> ContentClass {
    if restrictiveness(claimed) < restrictiveness(ceiling) {
        ceiling
    } else {
        claimed
    }
}

/// Whether a binary record of `kind` under the header class `class` may carry its payload.
pub const fn binary_payload_permitted(kind: StreamKind, class: ContentClass) -> bool {
    !kind.payload_is_content() || class.permits_content()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_never_loosens() {
        for &ceiling in ContentClass::ALL {
            for &claimed in ContentClass::ALL {
                let eff = clamp(ceiling, claimed);
                assert!(restrictiveness(eff) >= restrictiveness(ceiling));
                assert!(restrictiveness(eff) >= restrictiveness(claimed));
                if !ceiling.permits_content() || !claimed.permits_content() {
                    assert!(!eff.permits_content(), "{ceiling:?} {claimed:?}");
                }
            }
        }
        assert_eq!(
            clamp(ContentClass::RestrictedPaging, ContentClass::Unrestricted),
            ContentClass::RestrictedPaging
        );
        assert_eq!(
            clamp(ContentClass::Unrestricted, ContentClass::RestrictedCellular),
            ContentClass::RestrictedCellular
        );
        assert_eq!(
            clamp(ContentClass::OwnKeyDecrypted, ContentClass::Unrestricted),
            ContentClass::OwnKeyDecrypted
        );
    }
}
