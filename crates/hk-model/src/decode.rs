//! Demodulation (docs/07 §2.14), Decode / Message (§2.15) and Bitstream (§2.16).
//!
//! All three are **append-only interpretations**: re-running a demodulator or decoder writes a
//! new row with its own version, and the measurement it ran on is unchanged. Outputs point back
//! at what produced them (`Decode.demodulation_ref`, `Bitstream.demodulation_ref`,
//! `Recording.trigger`), so producer rows never need updating.
//!
//! Content gating (ADR-0004): see [`crate::content`]. A Decode keeps metadata and content in
//! separate fields so that a gated class can still record the metadata.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cluster::InventoryIdentity;
use crate::content::ContentClass;
use crate::emitter::DecodedIdentity;
use crate::ids::{
    BitstreamId, DecodeId, DemodulationId, DetectionId, EmitterId, ProvenanceId, RecordingId,
};
use crate::region::TimeRange;
use crate::time::Timestamp;

/// Parameters estimated from the signal (C13/C14); never picked by hand.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EstimatedParams {
    /// Symbol rate, Bd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_rate_hz: Option<f64>,
    /// Frequency deviation, Hz (FM/FSK).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deviation_hz: Option<f64>,
    /// Carrier frequency offset, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cfo_hz: Option<f64>,
    /// Modulation order (2 for BPSK/2FSK, 4 for QPSK/4FSK...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_order: Option<u32>,
    /// Pulse-shaping roll-off, 0–1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll_off: Option<f64>,
    /// Bandwidth used for the channel filter, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_hz: Option<f64>,
    /// Measured stereo pilot frequency, Hz (WFM: nominally 19 kHz, on the receiver clock), when
    /// the pilot locked (T-037b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pilot_hz: Option<f64>,
    /// Sub-audible squelch signalling (CTCSS tone / DCS code) measured blind from the FM
    /// discriminator output (T-988, SIGNAL-090). `None` when nobody looked (not an FM channel, or
    /// too little on-air audio); a look that found nothing is `Some` with `kind: none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subaudible: Option<Subaudible>,
}

/// What a sub-audible squelch analysis concluded (T-988).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubaudibleKind {
    /// Too little on-air audio analysed yet to say anything.
    Measuring,
    /// A sub-audible line matched a standard (EIA/TIA) CTCSS tone within the stated tolerance.
    Ctcss,
    /// A clean sub-audible line off every standard tone (reported raw, never snapped).
    Tone,
    /// A DCS (CDCSS) code word stream.
    Dcs,
    /// Analysed and found no sub-audible signalling — "no tone", not "never looked".
    None,
}

/// One sub-audible line (T-988).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubaudibleTone {
    /// Measured frequency, Hz (to about 0.1 Hz).
    pub measured_hz: f64,
    /// Line power over the median of the 55–270 Hz band, dB.
    pub snr_db: f64,
    /// Nearest standard CTCSS tone within `tolerance_hz`, Hz; `None` off the table.
    #[serde(default)]
    pub table_hz: Option<f64>,
    /// `measured_hz − table_hz`, Hz.
    #[serde(default)]
    pub delta_hz: Option<f64>,
    /// Snap tolerance applied, Hz.
    pub tolerance_hz: f64,
}

/// A decoded DCS code (T-988).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DcsCode {
    /// Three octal digits, e.g. `023`.
    pub code: String,
    /// `normal` or `inverted`.
    pub polarity: String,
    /// Valid code words received at the chosen bit phase.
    pub words: u32,
    /// The same bit stream read as other standard codes (DCS aliasing: `023` normal is
    /// bit-for-bit `047` inverted), e.g. `["047I"]`. The stream alone cannot tell them apart.
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Sub-audible squelch signalling measured on an FM channel (T-988): a CTCSS tone, a DCS code,
/// or an explicit "none". Measured blind from the discriminator, never looked up.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subaudible {
    /// The conclusion.
    pub kind: SubaudibleKind,
    /// On-air (squelch-open) audio analysed, s.
    pub analysed_s: f64,
    /// Sub-audible lines, strongest first: the tone (`ctcss`/`tone`), then a second tone when
    /// two are present. Empty otherwise.
    #[serde(default)]
    pub tones: Vec<SubaudibleTone>,
    /// The DCS code (`kind: dcs`).
    #[serde(default)]
    pub dcs: Option<DcsCode>,
    /// Why nothing was reported (`kind: none`), e.g. a harmonic comb rather than a tone.
    #[serde(default)]
    pub reason: Option<String>,
    /// Detector id and version.
    pub detector: String,
}

/// A demodulation session on a channel (docs/07 §2.14).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Demodulation {
    /// Id.
    pub id: DemodulationId,
    /// Emitter demodulated, if resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitter_ref: Option<EmitterId>,
    /// Detection the channel was derived from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection_ref: Option<DetectionId>,
    /// Recording replayed (offline demodulation), if any. `None` for live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_ref: Option<RecordingId>,
    /// Mode or family, e.g. `wfm`, `2fsk`, `ook`.
    pub mode: String,
    /// Estimated parameters.
    pub params: EstimatedParams,
    /// Lock quality, 0–1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_quality: Option<f64>,
    /// Error vector magnitude, dB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evm_db: Option<f64>,
    /// Session time span.
    pub time: TimeRange,
    /// Demodulator id and version, e.g. `hk-demod/fsk@0.1.0`.
    pub demod_version: String,
}

/// Frame check result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CrcStatus {
    /// Check passed: the frame is ground truth.
    Valid,
    /// Check failed.
    Invalid,
    /// The frame format has no check.
    NoCrc,
    /// Check not yet known (inferred framing, C21).
    Unknown,
    /// The check passed only after FEC corrected bits that the frame's own check alone could not
    /// vouch for (T-210: RDS burst correction while block-synced). The frame is usable, but it is
    /// never CRC-valid evidence: confirm-by-decode, lifecycle and CRC-valid rates count `valid`
    /// only.
    Corrected,
}

/// A decode's evidence for the labelled-capture dataset export (T-205, ADR-0016 §7): the CRC
/// status and the demodulation it decoded, without identity or content. **Only [`CrcStatus::Valid`]
/// counts as decoder-validated ground truth** — a future bounded-correction status (T-210) is a
/// distinct variant and this contract never treats it as evidence, by construction: a reader
/// matches on `Valid` explicitly rather than `!= Invalid`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodeEvidence {
    /// The decode.
    pub decode_id: DecodeId,
    /// The demodulation it decoded (names the emitter and the modulation `mode`).
    pub demodulation_id: DemodulationId,
    /// Frame check.
    pub crc_status: CrcStatus,
    /// Frame time.
    pub t: Timestamp,
}

/// Where a decode came from, when it was not an ordinary decoder's output (docs/07 §2.15 delta,
/// ADR-0015 §5.5 + ADR-0022 §11.3). Absent on every decoder-, plugin- and recipe-produced row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DecodeProvenance {
    /// Decoded by a pipeline decoder synthesis found (MAUTO), over the job's **hold-out** window.
    ///
    /// The numbers are what `ConfirmPolicy.synthesized` read, so a confirmation's arithmetic is
    /// reconstructible from the stored row rather than only from the job (ADR-0022 §11.3).
    Synthesized {
        /// The analyze job, `a<n>`.
        job_id: String,
        /// Decoded on hold-out (always `true`: search-window frames are never stored).
        holdout: bool,
        /// Hold-out `evidence_bits` (the rank currency, **not** the confirm key).
        evidence_bits: f64,
        /// Hypotheses the job charged to look-elsewhere, all stages.
        hypotheses: u64,
        /// The confirm key: analytic-null hold-out bits, each net of its own stage's `L_j`.
        analytic_holdout_bits: f64,
        /// `width × differences − L_check`; `None` when the prefix carried no check.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        check_bits: Option<f64>,
        /// `L_check`; `None` when an inherited charge was never recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        l_check: Option<f64>,
        /// Whether the check was searched (open search or a discovered template).
        check_searched: bool,
        /// How the template's **check** was priced (ADR-0022 §5.1), or `None` for an open search:
        ///
        /// - `template-fixed` — a builtin or user template fixed the whole check before the data
        ///   was seen, so `L_check = 0`. The two author kinds are one value: the ADR prices them
        ///   identically and the result does not record which authored the template.
        /// - `discovered` — a template an earlier search found, inheriting that search's
        ///   look-elsewhere as `L_check` (the laundering rule).
        /// - `searched` — a template whose check was still searched in this job.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        template_provenance: Option<String>,
    },
}

/// Structured output from a decoder plugin or bit-framing inference (docs/07 §2.15).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decode {
    /// Id.
    pub id: DecodeId,
    /// Demodulation decoded, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demodulation_ref: Option<DemodulationId>,
    /// Recording decoded (replay), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_ref: Option<RecordingId>,
    /// Decoder id, e.g. `readsb`, `hk-rds`, `hk-infer`.
    pub decoder_id: String,
    /// Decoder version.
    pub decoder_version: String,
    /// Frame model name, e.g. `adsb-df17`, `rds-group-0a`, or an inferred-framing id.
    pub frame_model: String,
    /// Metadata fields: frame type, addresses and other non-content identifiers, lengths, timing,
    /// CRC detail. Always stored and streamed, whatever `content_class` says.
    pub metadata: Value,
    /// Content fields: payload, message text, voice/audio references. `None` when the frame has
    /// no content or it was withheld. The repository refuses `Some` unless `content_class`
    /// permits content (`RepoError::GatedContent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    /// Frame check.
    pub crc_status: CrcStatus,
    /// Identity named by the frame, if any (metadata).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<DecodedIdentity>,
    /// Content gating class, chosen explicitly by the decoder (fail closed to
    /// [`ContentClass::FAIL_CLOSED`] when unknown).
    pub content_class: ContentClass,
    /// Frame time.
    pub t: Timestamp,
    /// Non-decoder provenance (a synthesized pipeline's decode); `None` for every ordinary row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<DecodeProvenance>,
}

/// Label written in place of a decode label (`frame_model`, `decoder_id`, `decoder_version`) that
/// names a withheld identifier (T-036).
pub const WITHHELD_LABEL: &str = "withheld";

/// A decode as it leaves the repository, gated for an [`crate::IdentityAccess`] (T-036; rules in
/// [`crate::cluster`]).
///
/// When the gate withholds the row's detail, `decode.identity` is `None`, `decode.metadata` is
/// `{}`, `decode.content` is `None` (a content-permitting row whose identity is withheld) and any
/// label naming the identity, a metadata value or a content value reads [`WITHHELD_LABEL`]. The
/// class, CRC status, time and references are unchanged.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodeView {
    /// The decode, gated.
    pub decode: Decode,
    /// Its identity as shown: clear, withheld (scheme only) or none.
    pub identity: InventoryIdentity,
    /// Non-empty stored metadata was withheld.
    pub metadata_withheld: bool,
    /// Stored content was withheld.
    pub content_withheld: bool,
    /// At least one label was replaced by [`WITHHELD_LABEL`].
    pub labels_withheld: bool,
}

/// A backend-rendered summary of a decoded identity for a list row (T-967): the stable,
/// human-readable label the identity's decoder **voted** over its latest committed session, and
/// how much of that session's evidence agreed with it. Thin-client rule (CLAUDE.md): the UI
/// renders `label` as given and never parses a decode's raw fields to build one itself.
///
/// Served only for schemes whose built-in decoder writes an explicit per-session summary row —
/// today RDS (`rds-pi`, see [`rds_session_summary`]). Every other scheme has none, and is served
/// no summary rather than a guess from similarly-named fields: a per-decoder declared label field
/// is its own contract, not something this module infers from key names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecodeIdentitySummary {
    /// The decoder's own session label, trimmed — for RDS the **most frequent** complete PS frame
    /// of the session (the vote `hk_demod::RdsReport::ps` takes), never the latest fragment: a
    /// station scrolling song/artist text through PS sends many fragments, one of which is always
    /// the newest.
    pub label: String,
    /// The fraction, 0–1, of the session's complete PS frames that read exactly `label` — the
    /// label's own vote share (the same figure the decoder's Label annotation carries as its
    /// confidence). `None` when the summary row recorded no frame counts. Deliberately **not** the
    /// PI vote share (`pi_share`), which is confidence in the code, not in the name.
    pub label_share: Option<f64>,
}

/// The built-in RDS decoder's id (`hk_demod::RDS_DECODER_ID`; `hk-model` cannot depend on
/// `hk-demod`, and `hk-demod`'s T-967 test writes rows through the real writer and reads them
/// back here, which pins the two together).
pub(crate) const RDS_DECODER_ID: &str = "hk-rds";

/// The frame model of the RDS decoder's once-per-session summary row (`hk_demod::rds_decodes`):
/// the accepted PI's vote, the most frequent PS frame (`ps`) and every PS frame's count
/// (`ps_frames`, most frequent first). The per-frame rows (`rds-group-0-ps-frame`) are the raw
/// fragments and are never read for the summary.
pub(crate) const RDS_SUMMARY_FRAME_MODEL: &str = "rds-pi";

/// The [`DecodeIdentitySummary`] an RDS session summary row states, or `None` when `d` is not one
/// (another decoder, a per-frame row) or voted no PS. Reads `ps` and `ps_frames` exactly as
/// `hk_demod::rds_decodes` writes them.
pub(crate) fn rds_session_summary(d: &Decode) -> Option<DecodeIdentitySummary> {
    if d.decoder_id != RDS_DECODER_ID || d.frame_model != RDS_SUMMARY_FRAME_MODEL {
        return None;
    }
    let raw = d.metadata.get("ps")?.as_str()?;
    let label = raw.trim();
    if label.is_empty() {
        return None;
    }
    // `ps_frames`: `[[text, count], …]`. The label's share is its own count over all counts.
    let label_share = d
        .metadata
        .get("ps_frames")
        .and_then(Value::as_array)
        .and_then(|frames| {
            let (mut total, mut mine) = (0u64, None);
            for f in frames {
                let n = f.get(1)?.as_u64()?;
                total += n;
                if f.get(0)?.as_str()? == raw {
                    mine = Some(n);
                }
            }
            let mine = mine?;
            (total > 0).then(|| mine as f64 / total as f64)
        });
    Some(DecodeIdentitySummary {
        label: label.to_owned(),
        label_share,
    })
}

/// What a bitstream carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BitstreamPayload {
    /// Hard bits.
    HardBits,
    /// Soft symbols.
    SoftSymbols,
    /// Framed messages (newline-delimited JSON on stream-output, ADR-0004).
    Messages,
}

/// Framing metadata. The ADR-0004 stream header carries the same information.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Framing {
    /// Payload kind.
    pub payload: BitstreamPayload,
    /// Bits per symbol, for symbol payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bits_per_symbol: Option<u32>,
    /// Symbol rate, Bd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_rate_hz: Option<f64>,
    /// Message schema id, for message payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_id: Option<String>,
    /// Sync word / preamble, hex, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_word_hex: Option<String>,
}

/// Where the bits are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum BitstreamTransport {
    /// A stored file (Recording-like, under quota). Stored bits are content.
    Stored {
        /// Path relative to the device data directory.
        uri: String,
    },
    /// A live stream on stream-output (C24); the row is its descriptor (metadata).
    Live {
        /// Endpoint, e.g. `unix:///run/hackriff/bits-<id>.sock`.
        endpoint: String,
    },
}

/// A bit/soft-symbol artifact or live stream descriptor (docs/07 §2.16).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bitstream {
    /// Id.
    pub id: BitstreamId,
    /// Emitter, if resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitter_ref: Option<EmitterId>,
    /// Producing demodulation, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demodulation_ref: Option<DemodulationId>,
    /// Framing.
    pub framing: Framing,
    /// Stored or live.
    pub transport: BitstreamTransport,
    /// Time span; `end` equals `start` while a live stream is open.
    pub time: TimeRange,
    /// Source trust record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_ref: Option<ProvenanceId>,
    /// Content gating class. The repository refuses a `Stored` bitstream whose class does not
    /// permit content; a `Live` descriptor is accepted and C24 gates the stream itself.
    pub content_class: ContentClass,
}
