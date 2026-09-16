//! The encryption check that stands where a voice path would be opened (C23, T-270).
//!
//! C23 §Methods calls for an "encryption check **before the vocoder**", and the failure mode it
//! guards is not a missing feature but a missing *branch*: an audio path added later that simply
//! never asks. A comment saying "check encryption here" is exactly the kind of thing that is true
//! until someone writes the next function.
//!
//! So the check is a **permit**, not a convention. [`VoicePermit`] has a private field, no
//! `Default`, and no constructor other than [`VoicePermit::open`]. A future vocoder cannot consume
//! a channel's samples for voice without holding one, and cannot obtain one without passing an
//! [`Encryption`] that earned it. Omission is not available as a failure mode: the type has to
//! appear in the signature, and the only way to make one is to ask.
//!
//! # M4 opens no voice path at all
//!
//! There is no `CallAudio` type in the workspace, no column that could hold one, and no vocoder.
//! Nothing here adds one — this is deliberately the check *without* the thing it checks, put in
//! place first so the thing cannot arrive without it. The follower consults it at the point where
//! it has a channel stream in hand, records the answer, and passes no samples to anything.
//!
//! # What earns a permit
//!
//! **Only clear-by-ALGID.** A grant's service-options bit can say `Encrypted` but is never allowed
//! to say `Clear` (see `super::tsbk`), so in practice today *nothing* earns a permit, and that is
//! the correct state for a milestone with no voice-frame decode: the authoritative statement lives
//! in the call's own header, which nothing reads yet.
//!
//! Both refusals are distinguished, because they are different facts about the world — one is
//! "this is protected", the other "nobody told us" — and a person reading a call list deserves to
//! see which. Neither produces audio.
//!
//! Nothing in this module decrypts anything, and nothing in it may.

use hk_model::Encryption;

use super::tsbk::is_algid_evidence;

/// Why a voice path was refused.
///
/// Carries the evidence so the refusal can be recorded as a measurement rather than a shrug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceRefused {
    /// Something said the traffic is protected. Carries the ALGID when one was read.
    Encrypted {
        /// The P25 ALGID, when an ALGID is what said so.
        algid: Option<u8>,
    },
    /// **Nothing said.** Late entry without a header, a grant update, a grant whose encryption bit
    /// was clear but which no ALGID has corroborated. Never a synonym for "clear".
    Unknown,
    /// Something said "clear", but not with the authority to license audio: a grant-time
    /// announcement rather than the call's own ALGID.
    UnauthoritativeClear,
}

impl VoiceRefused {
    /// A short machine reason for a `call_record` or `grant_event` row.
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Encrypted { .. } => "encrypted-no-audio",
            Self::Unknown => "encryption-unknown-no-audio",
            Self::UnauthoritativeClear => "clear-unconfirmed-no-audio",
        }
    }
}

/// Permission to open a voice path on a call.
///
/// Unconstructible except through [`Self::open`]. The private field is the point: a vocoder that
/// takes one of these in its signature cannot be called without the check having happened, so the
/// check cannot be skipped by writing code that forgets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoicePermit(());

impl VoicePermit {
    /// Asks whether a call's encryption state permits a voice path.
    ///
    /// Granted **only** for a call whose own ALGID said clear. `Unknown` fails closed, which is
    /// C23's named late-entry pitfall made structural, and anything encrypted fails closed for the
    /// obvious reason.
    pub fn open(enc: Encryption) -> Result<Self, VoiceRefused> {
        match enc {
            // The one case that earns it: the call's own header named the clear algorithm.
            Encryption::Clear { .. } if is_algid_evidence(enc) => Ok(Self(())),
            // "Clear" from a grant-time announcement is not the call's own statement.
            Encryption::Clear { .. } => Err(VoiceRefused::UnauthoritativeClear),
            Encryption::Encrypted { .. } => Err(VoiceRefused::Encrypted { algid: enc.algid() }),
            Encryption::Unknown => Err(VoiceRefused::Unknown),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trunk::tsbk::algid_encryption;
    use hk_model::{EncryptionEvidence, P25_ALGID_CLEAR};

    /// The gate, in the four states that reach it. The one that matters is `Unknown`: a call joined
    /// in progress must refuse exactly as hard as one known to be encrypted.
    #[test]
    fn only_a_call_whose_own_algid_said_clear_earns_a_voice_path() {
        // Earned: the authoritative statement, and the octet that means clear.
        assert!(VoicePermit::open(algid_encryption(P25_ALGID_CLEAR, None)).is_ok());

        // Refused: encrypted, and the refusal carries which algorithm.
        for algid in [0x81u8, 0x84, 0xAA, 0x17] {
            let refused = VoicePermit::open(algid_encryption(algid, None))
                .expect_err("an algorithm is not a permit");
            assert_eq!(refused, VoiceRefused::Encrypted { algid: Some(algid) });
            assert_eq!(refused.reason(), "encrypted-no-audio");
        }

        // Refused: nothing said. This is the late-entry case and it fails closed.
        assert_eq!(
            VoicePermit::open(Encryption::Unknown),
            Err(VoiceRefused::Unknown)
        );

        // Refused: "clear", but only a grant-time announcement said so.
        assert_eq!(
            VoicePermit::open(Encryption::Clear {
                evidence: EncryptionEvidence::ServiceOptions,
                algid: None,
                key_id: None,
            }),
            Err(VoiceRefused::UnauthoritativeClear)
        );
    }

    /// `Unknown` must never be treated as clear anywhere the gate can see it — the T-266 invariant,
    /// asserted at the one place a voice path could ever be opened.
    #[test]
    fn unknown_is_never_clear_at_the_gate() {
        let unknown = Encryption::Unknown;
        assert!(!unknown.is_clear());
        assert!(VoicePermit::open(unknown).is_err());
    }
}
