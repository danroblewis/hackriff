//! What this build can and cannot follow, and **why** (C23, T-271).
//!
//! # The failure this module exists to prevent
//!
//! A trunking system whose control traffic a decoder cannot follow produces **no grants**. So does
//! a system with no traffic. So does a system nobody pointed the radio at. Those three are the same
//! picture, and the first one is the only one where the picture is a lie — the system is busy, and
//! the receiver is the thing that is silent.
//!
//! Two of C23's named cases are not merely hard, they are *structurally* outside what a
//! control-channel decoder can do, because there is no dedicated control channel to decode:
//!
//! - **Motorola Capacity Plus** trunks on a **rest channel**: idle radios park on whichever channel
//!   is currently at rest, calls start there, and the rest channel then *moves*. There is no
//!   continuously-transmitting channel to confirm, nothing at 100 % duty for the occupancy sweep to
//!   find, and the thing a follower would lock onto changes frequency while it is being followed.
//! - **NXDN Type-D** is distributed-logic trunking: the trunking decisions are made by each radio's
//!   home repeater and the signalling rides *in the traffic channel's own data stream*, not on a
//!   dedicated channel. It is the digital descendant of LTR, which hides its signalling
//!   sub-audibly under the voice for the same reason.
//!
//! Both must therefore be **reported unsupported, with the reason**, rather than producing nothing.
//! "Unsupported: Capacity Plus uses a rest channel" is a useful answer. Silence is not — and this is
//! the same discipline every M4 task before it applied to a different refusal: T-268 records an
//! unresolvable identifier as `no-iden` rather than guessing a frequency, T-269 records a grant
//! beyond the radio's reach as `grant-outside-window` **with** the frequency it would have used,
//! T-270 records "nothing said" as `Unknown` rather than as `Clear`. A refusal that says which
//! refusal it is, is a measurement. A refusal that says nothing is a bug report against the user.
//!
//! # Levels, and the line between two of them
//!
//! [`SupportLevel::Unsupported`] and [`SupportLevel::Unimplemented`] are deliberately different
//! statements and must not be collapsed:
//!
//! - **Unsupported** — no amount of work *inside this design* reaches it, because the thing a
//!   control-channel decoder needs does not exist on the air.
//! - **Unimplemented** — there **is** a dedicated control channel; this build does not decode it,
//!   and the reason says what is missing.
//!
//! Telling a person "not built yet" when the truth is "cannot be built this way" sends them looking
//! for a setting. Telling them "unsupported" when a later task could simply do the work writes off a
//! capability by accident. So the reason is part of the value, not decoration.
//!
//! # Where a person sees this
//!
//! [`support_json`] is folded into the run summary's counters under `trunking.support` (and so into
//! `/api/status`, which serves the same object), and [`unsupported_text`] is the line the run
//! summary prints. The statement is about **this build's capability**, not about an observation, so
//! it is present in every run — including one that found nothing, which is exactly the run where the
//! distinction matters.

use hk_model::TrunkProtocol;

/// How well this build follows a trunking system's control signalling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupportLevel {
    /// Control messages are decoded: grants, and whatever band plan the protocol announces.
    Decoded,
    /// The air interface is recognised (its frame sync is known and verified), but no control
    /// message is decoded, so **no grant is ever claimed** from it.
    Identified,
    /// A dedicated control channel exists and this build does not decode it. The reason says what
    /// is missing.
    Unimplemented,
    /// **Structurally outside a control-channel decoder**: there is no dedicated control channel to
    /// decode. The reason says why.
    Unsupported,
}

impl SupportLevel {
    /// The stored text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Decoded => "decoded",
            Self::Identified => "identified",
            Self::Unimplemented => "unimplemented",
            Self::Unsupported => "unsupported",
        }
    }

    /// Whether a grant may ever be claimed at this level. Only [`Self::Decoded`] may.
    pub const fn may_claim_a_grant(self) -> bool {
        matches!(self, Self::Decoded)
    }
}

/// One trunking family, and what this build does about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrunkSupport {
    /// The name a person would recognise. Not every entry has a [`TrunkProtocol`] — Capacity Plus
    /// and LTR have none, which is itself part of the point: the model has no variant for a system
    /// that cannot be identified from a control channel.
    pub name: &'static str,
    /// The model protocol this family maps to, when one exists.
    pub protocol: Option<TrunkProtocol>,
    /// What this build does.
    pub level: SupportLevel,
    /// **Why** — the sentence a person reads instead of seeing nothing.
    pub reason: &'static str,
}

/// Every trunking family C23 names, and what this build does about each.
///
/// The table is exhaustive over C23 §Methods' list on purpose: a family that is simply *absent* from
/// a support table is indistinguishable from one nobody thought about, which is the same ambiguity
/// this module exists to close one level up.
pub const TRUNK_SUPPORT: [TrunkSupport; 10] = [
    TrunkSupport {
        name: "P25 Phase 1",
        protocol: Some(TrunkProtocol::P25Phase1),
        level: SupportLevel::Decoded,
        reason: "TSBK decode on a confirmed C4FM control channel; IDEN_UP announces the band plan, \
                 so a grant's channel number resolves to a frequency on the air alone (T-268)",
    },
    TrunkSupport {
        name: "DMR Tier III",
        protocol: Some(TrunkProtocol::DmrTier3),
        level: SupportLevel::Decoded,
        reason: "CSBK decode on a confirmed 4FSK control channel: grants carry a logical physical \
                 channel number, a timeslot, and target and source addresses. The channel number \
                 is NOT resolved to a frequency — no channel-parameter announcement could be \
                 corroborated — so every grant is recorded `unmapped-channel` with that reason \
                 rather than mapped through an assumed band plan",
    },
    TrunkSupport {
        name: "Motorola Capacity Plus",
        protocol: None,
        level: SupportLevel::Unsupported,
        reason: "no dedicated control channel: the system trunks on a REST CHANNEL that idle \
                 radios park on, calls begin there, and the rest channel itself moves between \
                 frequencies. There is nothing continuously transmitting for the occupancy sweep \
                 to find and nothing stationary for a follower to hold, so a control-channel \
                 decoder cannot reach it however well it decodes DMR",
    },
    TrunkSupport {
        name: "NXDN Type-D",
        protocol: None,
        level: SupportLevel::Unsupported,
        reason: "no dedicated control channel: trunking is distributed, decided by each radio's \
                 home repeater, and the signalling rides inside the traffic channel's own data \
                 stream rather than on a channel of its own — the digital descendant of LTR. There \
                 is no control channel to confirm, so there is none to decode",
    },
    TrunkSupport {
        name: "LTR (sub-audible)",
        protocol: None,
        level: SupportLevel::Unsupported,
        reason: "no dedicated control channel: the signalling is carried sub-audibly beneath the \
                 voice on each traffic channel, so it exists only while a call is up and only on \
                 the channel carrying it",
    },
    TrunkSupport {
        name: "NXDN Type-C",
        protocol: Some(TrunkProtocol::NxdnTypeC),
        level: SupportLevel::Decoded,
        reason: "CAC decode on a confirmed outbound RCCH: the 20-bit frame sync, the LICH's parity \
                 and channel type, then the CAC descrambled, deinterleaved, depunctured, Viterbi \
                 decoded and CRC checked, giving channel assignments with their channel number, \
                 call type, source unit and destination group or unit. The channel number is NOT \
                 resolved to a frequency — the air interface carries a 10-bit channel NUMBER and \
                 defines no mapping from one to hertz, and none of its information elements is a \
                 frequency — so every assignment is recorded `unmapped-channel` with that reason \
                 rather than mapped through an assumed base and step. Only the 12.5 kHz / 9600 bps \
                 variant is reachable: the 6.25 kHz variant is 2400 Bd and this milestone's symbol \
                 path produces 4800 Bd only",
    },
    TrunkSupport {
        name: "Motorola SmartNet / SmartZone",
        protocol: Some(TrunkProtocol::SmartNet),
        level: SupportLevel::Unimplemented,
        reason: "its control channel is 3600 bps BINARY FSK, and this milestone's symbol path \
                 produces 4-level dibits only, so there is no symbol stream to decode. Separately: \
                 SmartNet does not announce its band plan on the air — base frequency, spacing and \
                 offset are configured by the listener in every reference decoder — so a decoded \
                 grant's channel number could not be turned into a frequency blind",
    },
    TrunkSupport {
        name: "EDACS",
        protocol: Some(TrunkProtocol::Edacs),
        level: SupportLevel::Unimplemented,
        reason: "its 9600 bps control channel is binary GFSK, which this milestone's 4-level \
                 symbol path does not produce. Separately: a grant names a logical channel number \
                 whose frequency comes from a per-system channel list, not from the air, so a \
                 decoded grant could not be mapped blind",
    },
    TrunkSupport {
        name: "P25 Phase 2",
        protocol: Some(TrunkProtocol::P25Phase2),
        level: SupportLevel::Unimplemented,
        reason: "the control channel of a Phase 2 system is a Phase 1 FDMA channel this build \
                 already decodes; what is missing is the two-slot TDMA traffic side, so a grant's \
                 slot is not attributed and no Phase 2 system is named",
    },
    TrunkSupport {
        name: "MPT1327",
        protocol: Some(TrunkProtocol::Mpt1327),
        level: SupportLevel::Unimplemented,
        reason: "1200 bps FFSK control signalling, which this milestone's 4-level symbol path does \
                 not produce",
    },
];

/// The entry for a protocol, when the table has one.
pub fn support_for(protocol: TrunkProtocol) -> Option<&'static TrunkSupport> {
    TRUNK_SUPPORT.iter().find(|s| s.protocol == Some(protocol))
}

/// The families that are **structurally** outside a control-channel decoder.
pub fn unsupported() -> impl Iterator<Item = &'static TrunkSupport> {
    TRUNK_SUPPORT
        .iter()
        .filter(|s| s.level == SupportLevel::Unsupported)
}

/// The support table as JSON, for the run summary's `trunking.support` and `/api/status`.
///
/// Every entry carries its reason. A consumer that renders only the names still shows *that* a
/// family is unsupported; one that renders the reason shows why, which is the useful half.
pub fn support_json() -> serde_json::Value {
    serde_json::Value::Array(
        TRUNK_SUPPORT
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "protocol": s.protocol.map(TrunkProtocol::as_str),
                    "level": s.level.as_str(),
                    "reason": s.reason,
                })
            })
            .collect(),
    )
}

/// The one-line statement a run summary prints: which families this build cannot follow at all.
///
/// Short by design — the full reasons live in [`support_json`] — but it names them, because a name
/// is what sends someone to the right place and a bare count sends them nowhere.
pub fn unsupported_text() -> String {
    let names: Vec<&str> = unsupported().map(|s| s.name).collect();
    format!(
        "no dedicated control channel, so not followable: {}",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The acceptance criterion this module exists for, as an assertion: the two named cases are
    /// present, are `unsupported` rather than merely missing, and each says **why** — and the why
    /// names the structural fact, not a difficulty.
    #[test]
    fn capacity_plus_and_nxdn_type_d_are_reported_unsupported_with_the_structural_reason() {
        for (name, must_mention) in [
            ("Motorola Capacity Plus", "rest channel"),
            ("NXDN Type-D", "distributed"),
        ] {
            let e = TRUNK_SUPPORT
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| {
                    panic!(
                        "{name} is absent from the support table, which is indistinguishable from \
                         nobody having considered it"
                    )
                });
            assert_eq!(
                e.level,
                SupportLevel::Unsupported,
                "{name} is reported {:?}, but it has no dedicated control channel at all",
                e.level
            );
            assert!(
                e.reason
                    .to_lowercase()
                    .contains("no dedicated control channel"),
                "{name}'s reason does not say the structural fact: {}",
                e.reason
            );
            assert!(
                e.reason.to_lowercase().contains(must_mention),
                "{name}'s reason does not say how it trunks instead ({must_mention}): {}",
                e.reason
            );
            assert!(
                !e.level.may_claim_a_grant(),
                "{name} may not produce a grant"
            );
            // Neither has a `TrunkProtocol`, and that is deliberate: the model names protocols a
            // control channel can announce, and these announce on no control channel.
            assert_eq!(e.protocol, None, "{name} was given a protocol variant");
        }
        // And they are named in the line a person reads.
        let text = unsupported_text();
        assert!(text.contains("Capacity Plus"), "{text}");
        assert!(text.contains("NXDN Type-D"), "{text}");
    }

    /// `unsupported` and `unimplemented` are different claims, and the table may not blur them.
    #[test]
    fn unimplemented_entries_all_name_a_dedicated_control_channel_they_do_not_decode() {
        let unimpl: Vec<_> = TRUNK_SUPPORT
            .iter()
            .filter(|s| s.level == SupportLevel::Unimplemented)
            .collect();
        assert!(!unimpl.is_empty(), "the distinction is not exercised");
        for e in unimpl {
            assert!(
                !e.reason.contains("no dedicated control channel"),
                "{} is `unimplemented` but its reason is a structural one, so it should be \
                 `unsupported`: {}",
                e.name,
                e.reason
            );
            assert!(
                e.reason.len() > 40,
                "{} gives no usable reason: {}",
                e.name,
                e.reason
            );
        }
    }

    /// Every entry says something, and only decoded ones may produce grants.
    #[test]
    fn every_entry_carries_a_reason_and_only_decoded_ones_may_claim_a_grant() {
        for e in &TRUNK_SUPPORT {
            assert!(!e.name.is_empty());
            assert!(e.reason.len() > 40, "{} gives no reason", e.name);
            if e.level != SupportLevel::Decoded {
                assert!(
                    !e.level.may_claim_a_grant(),
                    "{} may claim a grant at level {:?}",
                    e.name,
                    e.level
                );
            }
        }
        // The JSON a person's tooling reads carries the same thing.
        let j = support_json();
        let rows = j.as_array().expect("an array");
        assert_eq!(rows.len(), TRUNK_SUPPORT.len());
        let cap = rows
            .iter()
            .find(|r| r["name"] == "Motorola Capacity Plus")
            .expect("Capacity Plus in the JSON");
        assert_eq!(cap["level"], "unsupported");
        assert!(cap["reason"].as_str().unwrap().contains("rest channel"));
        assert!(cap["protocol"].is_null());
    }

    /// Both protocols this build decodes are in the table as `decoded`, so the table cannot drift
    /// out of step with what the decoder actually does.
    #[test]
    fn the_decoded_protocols_are_the_ones_the_decoders_can_name() {
        for p in [
            TrunkProtocol::P25Phase1,
            TrunkProtocol::DmrTier3,
            TrunkProtocol::NxdnTypeC,
        ] {
            let e = support_for(p).unwrap_or_else(|| panic!("{p:?} is not in the support table"));
            assert_eq!(e.level, SupportLevel::Decoded, "{p:?}");
            assert!(e.level.may_claim_a_grant());
        }
        // And a protocol nothing decodes may not be claimed as decoded.
        for p in [
            TrunkProtocol::SmartNet,
            TrunkProtocol::Edacs,
            TrunkProtocol::Mpt1327,
            TrunkProtocol::P25Phase2,
        ] {
            let e = support_for(p).unwrap_or_else(|| panic!("{p:?} is not in the support table"));
            assert_ne!(e.level, SupportLevel::Decoded, "{p:?}");
        }
    }

    /// The two protocols whose grants carry **no frequency** must say so in the table, because a
    /// row that reads only "decoded" invites the reader to expect a frequency the run never
    /// produces (T-271 for DMR, T-345 for NXDN).
    #[test]
    fn a_decoded_protocol_that_resolves_no_frequency_says_so() {
        for p in [TrunkProtocol::DmrTier3, TrunkProtocol::NxdnTypeC] {
            let e = support_for(p).unwrap();
            assert_eq!(e.level, SupportLevel::Decoded);
            let r = e.reason.to_lowercase();
            assert!(
                r.contains("unmapped-channel"),
                "{p:?} does not say what its grants are recorded as: {}",
                e.reason
            );
            assert!(
                r.contains("not resolved to a frequency"),
                "{p:?} does not say that its channel numbers produce no frequency: {}",
                e.reason
            );
        }
    }
}
