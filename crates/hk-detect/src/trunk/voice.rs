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
//! to say `Clear` (see `super::tsbk`), so in practice today *nothing* earns a permit. The
//! authoritative statement lives in the call's own voice frames: [`super::ldu`] reads each LDU2's
//! ALGID off the granted channel (T-849), and [`CallHeader::fold`] folds those into the call's
//! encryption state (T-330) before the follower asks [`VoicePermit::open`]. So the first permit
//! comes the only way any can: a call whose own ALGID said clear, through `open`, like everything
//! else. There is no second door for the path that read the ALGID — it produces an
//! [`Encryption`], not a permit.
//!
//! # The call's own header outranks its grant — but never towards clear
//!
//! The grant's service-options bit may only *raise* a call to `Encrypted`; it never lowers one to
//! `Clear`. That asymmetry is what lets the header override: a grant can have said `Unknown` (bit
//! clear, grant update, late entry) or `Encrypted`, and the header's ALGID replaces either —
//! `Unknown` becomes whatever the ALGID says, and a grant-time `Encrypted` becomes the header's
//! `Encrypted` carrying the algorithm and key id the grant could not name.
//!
//! The one pairing the header does **not** override is a grant that announced `Encrypted` and a
//! header that says clear. The two statements contradict, and a contradiction about whether traffic
//! is protected resolves to the safer answer — the same rule [`Encryption::refine`] applies to a
//! key change mid-call, and the repository enforces again on every rewrite. The call stays
//! encrypted, no permit is earned, and [`CallHeader::contradicts_grant`] says so on the record
//! rather than hiding it: a mismatch between what a system announced and what it sent is
//! interesting in itself.
//!
//! Both refusals are distinguished, because they are different facts about the world — one is
//! "this is protected", the other "nobody told us" — and a person reading a call list deserves to
//! see which. Neither produces audio.
//!
//! Nothing in this module decrypts anything, and nothing in it may.

use hk_model::Encryption;

use super::ldu::EncryptionSync;
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

/// What a call's own voice frames said about its encryption, folded against its grant (T-330).
///
/// See the module docs for the rule. Produces an [`Encryption`] — never a [`VoicePermit`]: the
/// permit is still only obtainable from [`VoicePermit::open`], so reading the ALGID adds no path
/// around the check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallHeader {
    /// What the call's LDU2s alone said, folded in stream order with [`Encryption::refine`]: an
    /// algorithm seen once in a call is never walked back by a later clear one. `Unknown` when no
    /// encryption sync decoded.
    pub header: Encryption,
    /// The call's state after the header was applied to the grant's.
    pub call: Encryption,
    /// The grant announced `Encrypted` and the call's own header said clear. The call stays
    /// encrypted; this records that the two disagreed.
    pub contradicts_grant: bool,
    /// How many encryption syncs the fold read.
    pub read: usize,
}

impl CallHeader {
    /// Folds the ALGIDs of `syncs` (one call's LDU2s, in stream order) into `grant`.
    pub fn fold<'a>(
        grant: Encryption,
        syncs: impl IntoIterator<Item = &'a EncryptionSync>,
    ) -> Self {
        let mut header = Encryption::Unknown;
        let mut read = 0;
        for es in syncs {
            header = header.refine(es.encryption());
            read += 1;
        }
        let contradicts_grant = grant.is_encrypted() && header.is_clear();
        // `refine` is exactly the rule: `Unknown` gives way to the header, a grant-time
        // `Encrypted` gives way to the header's `Encrypted` (which names the algorithm), and an
        // `Encrypted` grant is not walked back by a clear header.
        let call = grant.refine(header);
        Self {
            header,
            call,
            contradicts_grant,
            read,
        }
    }

    /// Whether the call's state was decided by its own ALGID.
    pub fn decided_by_algid(&self) -> bool {
        is_algid_evidence(self.call)
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

    fn es(algid: u8, key_id: u16) -> EncryptionSync {
        EncryptionSync {
            mi: [0; 9],
            algid,
            key_id,
        }
    }

    /// T-330: the call's own ALGID overrides what the grant announced, and only then can a permit
    /// be earned — through `open`, the one door.
    #[test]
    fn the_calls_own_algid_overrides_the_grant_and_is_the_only_road_to_a_permit() {
        let so_encrypted = Encryption::from_service_options(true);

        // Grant said nothing (bit clear, update, late entry); the header says clear.
        let h = CallHeader::fold(Encryption::Unknown, &[es(0x80, 0)]);
        assert_eq!(h.call, algid_encryption(0x80, Some(0)));
        assert!(h.decided_by_algid() && !h.contradicts_grant && h.read == 1);
        assert!(VoicePermit::open(h.call).is_ok());

        // Grant said nothing; the header names AES-256 and its key.
        let h = CallHeader::fold(Encryption::Unknown, &[es(0x84, 0x1234)]);
        assert_eq!(h.call.algid(), Some(0x84));
        assert_eq!(h.call.key_id(), Some(0x1234));
        assert_eq!(
            VoicePermit::open(h.call),
            Err(VoiceRefused::Encrypted { algid: Some(0x84) })
        );

        // Grant said encrypted; the header replaces it with the algorithm the grant could not name.
        let h = CallHeader::fold(so_encrypted, &[es(0xAA, 7)]);
        assert!(h.decided_by_algid() && !h.contradicts_grant);
        assert_eq!(h.call.algid(), Some(0xAA));

        // Grant said encrypted; the header says clear. Contradiction: the safer answer stands,
        // and the disagreement is on the record.
        let h = CallHeader::fold(so_encrypted, &[es(0x80, 0)]);
        assert!(h.contradicts_grant);
        assert!(h.header.is_clear());
        assert_eq!(h.call, so_encrypted);
        assert!(VoicePermit::open(h.call).is_err());

        // No encryption sync decoded: the grant's statement stands, untouched.
        let h = CallHeader::fold(Encryption::Unknown, &[]);
        assert_eq!(
            (h.call, h.header, h.read),
            (Encryption::Unknown, Encryption::Unknown, 0)
        );
        assert_eq!(VoicePermit::open(h.call), Err(VoiceRefused::Unknown));
        assert_eq!(CallHeader::fold(so_encrypted, &[]).call, so_encrypted);
    }

    /// An algorithm seen once in a call is never walked back by a later clear LDU2 (a key change
    /// mid-call must not read as "listenable after all"), whichever order the frames arrive in.
    #[test]
    fn one_encrypted_ldu2_in_a_call_keeps_it_encrypted() {
        for frames in [[es(0x80, 0), es(0x84, 5)], [es(0x84, 5), es(0x80, 0)]] {
            let h = CallHeader::fold(Encryption::Unknown, &frames);
            assert_eq!(h.call.algid(), Some(0x84), "{frames:?}");
            assert!(VoicePermit::open(h.call).is_err());
            assert_eq!(h.read, 2);
        }
    }
}
