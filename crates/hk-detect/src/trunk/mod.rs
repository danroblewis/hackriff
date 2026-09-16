//! Trunking control-channel hunting (C23, T-267).
//!
//! Two stages that must not be conflated:
//!
//! 1. **Candidacy** ([`raster`]): a channel is continuously occupied and lands on the LMR raster.
//!    Cheap, spectral, and *not evidence of a control channel* — continuous data emitters look
//!    exactly like this, which is C23's named false-CC pitfall.
//! 2. **Confirmation** ([`confirm`]): the channel's demodulated symbols carry a known frame sync
//!    **and** CRC-valid blocks.
//!
//! The types enforce the order. See [`confirm`] for how a [`confirm::ConfirmedCc`] is made
//! unconstructible without evidence.

//! 3. **Decode** ([`tsbk`], T-268): what the confirmed control channel *said* — the band plan its
//!    identifier updates announce, and the channel numbers its grants carry. Naming a protocol
//!    lives here, not in [`confirm`], and mapping a channel number refuses rather than guesses.

//! 4. **The encryption check** ([`voice`], T-270): the permit a voice path must hold. Separate from
//!    [`tsbk`] because it is not a decode — it is the gate that decode feeds, and it is a type so
//!    that a later vocoder cannot reach audio by forgetting to ask.

//! 5. **A second protocol, and an honest refusal for the rest** ([`dmr`], [`support`], T-271). DMR
//!    Tier III CSBKs decode alongside P25's TSBKs — and its grants produce **no frequency**, because
//!    no channel-parameter announcement could be corroborated and a plausible guess is the C23
//!    pitfall. [`support`] states, for every trunking family C23 names, what this build does and
//!    why: Capacity Plus and NXDN Type-D have no dedicated control channel at all, and are
//!    **reported unsupported** rather than producing a silence indistinguishable from a quiet band.

//! 6. **A third protocol, and a refusal the specification itself states** ([`nxdn`], T-345). NXDN
//!    Type-C CACs decode alongside P25's TSBKs and DMR's CSBKs — descrambled, deinterleaved,
//!    depunctured, Viterbi decoded and CRC checked against the published air interface — and their
//!    channel assignments produce **no frequency**, because the air interface carries a channel
//!    *number* and defines no mapping from one to hertz. That mapping is configured in the radio,
//!    so a grant resolves to nothing until somebody supplies one, and nothing here supplies a
//!    default.

pub mod confirm;
pub mod dmr;
pub mod nxdn;
pub mod raster;
pub mod support;
pub mod tsbk;
pub mod voice;

pub use confirm::{
    BLOCK_BYTES, BLOCK_DIBITS, CC_FRAMINGS, CONFIRMED_FALSE_ALARM_MAX, CcCandidate,
    CcConfirmConfig, CcConfirmer, CcEvidence, CcFraming, ConfirmedCc, FRAME_DIBITS, MIN_CC_FCO,
    MIN_CRC_VALID, MIN_SYNC_HITS, P25_FRAME_SYNC_DIBITS, SYNC_FALSE_ALARM_FLOOR,
    SYNC_TOLERANCE_DIBITS, ScanOutcome,
};
pub use dmr::{
    CSBK_BYTES, CSBK_CRC_MASK, CSBKO_BTV_GRANT, CSBKO_C_ALOHA, CSBKO_GRANTS, CSBKO_P_CLEAR,
    CSBKO_P_GRANT, CSBKO_PD_GRANT, CSBKO_TD_GRANT, CSBKO_TIER3, Csbk, CsbkScan,
    DMR_BS_DATA_SYNC_DIBITS, DMR_BS_VOICE_SYNC_DIBITS, DmrGrant, DmrPrivacyHeader, DmrResolved,
    MAX_CSBK_PER_WINDOW, MIN_DMR_CSBKS, csbk_crc, csbk_crc_ok, csbko_name, dmr_pi_encryption,
    dmr_protocol_of, is_voice_grant, scan_csbks,
};
pub use nxdn::{
    Cac, CacScan, MAX_CAC_PER_WINDOW, MIN_NXDN_CACS, MSG_DCALL_ASSGN, MSG_DCALL_ASSGN_DUP,
    MSG_VCALL_ASSGN, MSG_VCALL_ASSGN_DUP, NXDN_ASSIGNMENTS, NXDN_CAC_BITS, NXDN_CHANNEL_MAX,
    NXDN_CHANNEL_NULL, NXDN_FSW_DIBITS, NXDN_L3_BYTES, NXDN_SCRAMBLED_DIBITS,
    NXDN_SYNC_TOLERANCE_DIBITS, NXDN_TYPE_C, NxdnAssignment, NxdnFrame, NxdnLich, NxdnResolved,
    decode_frame, is_voice_assignment, message_type_name, nxdn_protocol_of, scan_cacs,
};
pub use raster::{LMR_RASTERS_HZ, RASTER_TOLERANCE_HZ, RasterFit, best_lmr_raster, fit_raster};
pub use support::{
    SupportLevel, TRUNK_SUPPORT, TrunkSupport, support_for, support_json, unsupported,
    unsupported_text,
};
pub use tsbk::{
    ChannelMap, Grant, IDEN_MAX_AGE_S, IdenUp, MAX_TSBK_PER_WINDOW, MIN_IDEN_AGREEMENTS,
    OP_GRP_VCH_GRANT, OP_GRP_VCH_GRANT_UPDATE, OP_IDEN_UP, P25_ALGIDS, Resolved, SVC_ENCRYPTED,
    ServiceOptions, TSBK_BYTES, Tsbk, TsbkScan, Unmapped, algid_encryption, algid_name,
    is_algid_evidence, protocol_of, scan_blocks,
};
pub use voice::{VoicePermit, VoiceRefused};
