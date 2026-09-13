//! Egress gating (ADR-0004; docs/stream-contract.md §6). **Legal guardrail.**
//!
//! This module is the single enforcement point for restricted content leaving hackriff on a
//! stream. The publisher calls it for every record before any byte is produced:
//!
//! - **Messages:** the record's class is clamped to the stream header's class ([`clamp`]).
//!   Content is serialised only if [`message_content_permitted`]: the effective class permits
//!   content, and `own-key-decrypted` content only goes out on a stream whose header class is
//!   also `own-key-decrypted`. Otherwise `content` is omitted and `gated: true` is set. Metadata
//!   always flows.
//! - **Binary records:** if the kind's payload is content ([`StreamKind::payload_is_content`]) and
//!   the header class does not permit content, the payload is withheld: a header-only record
//!   with [`RecordFlags::GATED`](super::RecordFlags::GATED) is emitted and the publisher returns
//!   an error so the misrouted producer notices.
//! - **Message metadata:** when a message's effective class forbids content, everything outside
//!   `content` (metadata, frame model/label, identity, decoder) is reduced to the publisher's
//!   [`MetadataPolicy`](super::MetadataPolicy) ([`super::policy`]). A messages publisher whose
//!   header class forbids content cannot be created without a policy.
//! - **Spectrum:** a waterfall whose row rate reaches the symbol rate is a non-coherent
//!   demodulator (POCSAG, voice spectrograms). Under a class that forbids content a spectrum
//!   stream is refused at creation unless its row rate is at most
//!   [`GATED_SPECTRUM_MAX_ROW_RATE_HZ`] ([`spectrum_stream_permitted`]) and it declares
//!   `fft_size` and `datatype`. The declared rate is then **enforced per row**: a token bucket
//!   over wall-clock arrival and one over the rows' `t` spacing (burst
//!   [`GATED_SPECTRUM_BURST_ROWS`]), plus a payload cap of `fft_size` x element size. Excess rows
//!   are withheld and reported by a counted `GATED` drop marker.
//! - **Transports:** `own-key-decrypted` streams are local-only: every consumer is subscribed
//!   with a [`Locality`](super::Locality) derived from its writer type, and a `Remote` consumer
//!   (TCP, a WebSocket bridge) is refused ([`remote_transport_permitted`]).
//! - A missing or unknown class, wherever it is parsed (headers, plugin output, raw JSON), is
//!   [`ContentClass::FAIL_CLOSED`].
//!
//! The internal plugin data plane ([`super::DecoderFeed`]) is deliberately *not* gated: decoders
//! must see samples to decode them; their output is clamped by the plugin host and gated again
//! here on the way out.

use hk_model::ContentClass;

use super::header::StreamKind;

/// Largest spectrum row rate (rows per second, the header's `sample_rate_hz`) allowed on a stream
/// whose class forbids content. A ~30 fps survey waterfall fits; a waterfall fast enough to
/// resolve symbols does not.
pub const GATED_SPECTRUM_MAX_ROW_RATE_HZ: f64 = 50.0;

/// Token-bucket depth (rows) of the per-row spectrum rate enforcement: absorbs scheduling jitter
/// of a producer running at its declared rate without letting a burst resolve symbols.
pub const GATED_SPECTRUM_BURST_ROWS: f64 = 2.0;

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

/// Whether a message whose effective (clamped) class is `effective` may carry its `content` on a
/// stream whose header class is `stream`. Own-key content needs an own-key stream.
pub const fn message_content_permitted(stream: ContentClass, effective: ContentClass) -> bool {
    effective.permits_content()
        && (!matches!(effective, ContentClass::OwnKeyDecrypted)
            || matches!(stream, ContentClass::OwnKeyDecrypted))
}

/// Whether a binary record of `kind` under the header class `class` may carry its payload.
pub const fn binary_payload_permitted(kind: StreamKind, class: ContentClass) -> bool {
    !kind.payload_is_content() || class.permits_content()
}

/// Whether a spectrum stream with row rate `row_rate_hz` may exist under `class`. A missing rate
/// under a content-forbidding class fails closed.
pub fn spectrum_stream_permitted(class: ContentClass, row_rate_hz: Option<f64>) -> bool {
    class.permits_content()
        || matches!(row_rate_hz, Some(r) if r.is_finite() && r > 0.0 && r <= GATED_SPECTRUM_MAX_ROW_RATE_HZ)
}

/// Whether a stream of `class` may be served on a remote transport (TCP, WebSocket bridge).
pub const fn remote_transport_permitted(class: ContentClass) -> bool {
    !matches!(class, ContentClass::OwnKeyDecrypted)
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

    #[test]
    fn own_key_content_needs_an_own_key_stream() {
        for &stream in ContentClass::ALL {
            for &record in ContentClass::ALL {
                let eff = clamp(stream, record);
                let permitted = message_content_permitted(stream, eff);
                let expected = match eff {
                    ContentClass::Unrestricted => true,
                    ContentClass::OwnKeyDecrypted => stream == ContentClass::OwnKeyDecrypted,
                    _ => false,
                };
                assert_eq!(permitted, expected, "stream {stream:?} record {record:?}");
            }
        }
        assert!(!remote_transport_permitted(ContentClass::OwnKeyDecrypted));
        assert!(remote_transport_permitted(ContentClass::RestrictedPaging));
    }

    #[test]
    fn spectrum_row_rate_cap() {
        let gated = ContentClass::RestrictedPaging;
        assert!(spectrum_stream_permitted(gated, Some(30.0)));
        assert!(spectrum_stream_permitted(gated, Some(50.0)));
        assert!(!spectrum_stream_permitted(gated, Some(50.1)));
        assert!(!spectrum_stream_permitted(gated, None));
        assert!(!spectrum_stream_permitted(gated, Some(f64::NAN)));
        assert!(spectrum_stream_permitted(
            ContentClass::Unrestricted,
            Some(1e6)
        ));
        assert!(spectrum_stream_permitted(ContentClass::Unrestricted, None));
    }
}
