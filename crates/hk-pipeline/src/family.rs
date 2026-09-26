//! Family vocabulary and ranked explanations (T-039, C15 → C17). The module:
//! - maps what the pipeline measured or decoded to the band-plan **service families**
//!   hk-context's priors understand;
//! - ranks the **top-k explanations** of each emitter;
//! - sets the emitter's `known_status` (`known` / `unexpected-here` / `unknown`), with a `prior_ref`
//!   and a status-history entry authored `prior`, from the best explanation backed by demodulator,
//!   decoder or classifier evidence (never by shape alone).
//!
//! # Evidence → service family
//!
//! Evidence comes in three kinds ([`Evidence`]):
//! - **Labels.** Demodulator modes (hk-demod [`AnalogMode::as_str`](hk_demod::AnalogMode::as_str),
//!   [`FSK_FAMILY`](hk_demod::fsk::FSK_FAMILY)) and modulation labels (hk-estimate blind families:
//!   `fsk`, `ook`, `bpsk`, `qpsk`). These are the families the chains' record writers store on an
//!   emitter.
//! - **Decoders.** Built-in decoder ids and plugin manifest ids (e.g. `readsb`, `hk-rds`). A
//!   chain stores them with [`record_decoder_evidence`] (see "Where it is wired").
//! - **Occupancy.** Bandwidth, duty cycle and symbol rate, from a closed Track.
//!
//! Every mapping carries a confidence. Evidence that is unmapped, or below [`MIN_CONFIDENCE`],
//! never sets a status. A name that is already a service family (`fm-broadcast`, `adsb`, `ism`,
//! …; [`hk_context::is_service_family`]) passes through at confidence 1.
//!
//! | evidence | kind | service family | confidence | why |
//! |---|---|---|---|---|
//! | `wfm` | demod mode | `fm-broadcast` | 0.9 | 200 kHz wideband FM is the broadcast service (47 CFR 73 subpart B) |
//! | `hk-rds`, `rds` | decoder | `fm-broadcast` | 0.99 | RDS rides only on FM broadcast |
//! | `readsb`, `dump1090` | decoder | `adsb` | 0.99 | CRC-checked Mode S / ADS-B frames |
//! | `ais-catcher` | decoder | `ais` | 0.95 | AIS frames |
//! | `rtl_433`, `rtl-433` | decoder | `ism` | 0.9 | a Part 15 sensor protocol decoded |
//! | `aptdec` | decoder | `noaa-apt` | 0.9 | APT imagery lines |
//! | `p25-tsbk`, `dmr-csbk`, `nxdn-cac` | decoder | `public-safety` | 0.97 | a CRC-valid trunked control channel; nothing else transmits one (T-546) |
//! | `flex` | decoder | `paging` | 0.97 | FLEX frames whose sync and BCH(31,21)-checked frame information word both hold; FLEX is a paging protocol and nothing else (T-950) |
//! | continuous, OBW 106–400 kHz | occupancy | `fm-broadcast` | 0.6 | [`WIDEBAND_FM_OBW_HZ`] |
//! | `nbfm`, `nfm` | demod mode | — | — | land mobile, amateur, marine, public safety and FRS/GMRS share it |
//! | `am` | demod mode | — | — | aviation, AM broadcast, CB and amateur share it |
//! | `ssb`, `cw` | demod mode | — | — | amateur and HF utility share it |
//! | `2fsk`, `fsk`, `gfsk`, `ook`, `bpsk`, `qpsk` | modulation | — | — | a modulation names no service |
//! | symbol rate alone | occupancy | — | — | no service is identified by rate alone today |
//!
//! # Ranked explanations
//!
//! [`rank_explanations`] builds one candidate per service family ([`Explanation`]):
//! - **Evidence candidates.** Each mapped piece of family evidence contributes
//!   `mapping confidence × classification confidence`. The pieces for one service combine as a
//!   noisy OR into `evidence_confidence`.
//! - **Allocation-only candidates.** Every service the band plan expects at the emitter's extent
//!   is a candidate, with no signal evidence and a base score of [`ALLOCATION_ONLY_SCORE`]. These
//!   are suggestions ("what is supposed to be here") and never set a status — **and since T-990
//!   they are never rank 1 either** (see "An explanation needs measured support" below).
//! - **Centre.** Allocation and raster checks use the emitter's centre and bandwidth **refined by
//!   output analysis** when a chain stored one (T-070, [`crate::refine`]; raster evidence then
//!   says `center_source: refined`), else the detected ones. Nothing is snapped to a raster: an
//!   off-raster refined centre is flagged `off-raster`.
//! - **Score.**
//!   `score = base × allocation fit × raster fit × support fit`. The base is the evidence
//!   confidence, or [`ALLOCATION_ONLY_SCORE`]. Allocation fit is 1 for `known`, 0.8 with no
//!   allocation data and 0.7 for `unexpected-here`. Support fit discounts an allocation-only
//!   candidate the emitter's own occupancy contradicts (T-990) and is 1 otherwise. Raster fit is
//!   0.8 for an off-raster centre and 1 otherwise. Rasters
//!   ([`CHANNEL_RASTERS`]) are keyed by region and apply only when the band table is wholly that
//!   region ([`BandTable::region`]). Candidates are ranked by score, and the top [`TOP_K`] are
//!   kept.
//! - **Evidence and flags.** Each candidate lists its evidence: the families, the band-plan
//!   verdict with its `prior_ref`, and the raster fit. Its flags are `off-raster`,
//!   `off-allocation`, `no-allocation-data`, `allocation-only`, `shape-only` (occupancy evidence
//!   only), `low-confidence`, and `contradicts-user-status` / `contradicts-decoder-status` (its
//!   band-plan verdict differs from a status a user or decoder set).
//!
//! # An explanation needs measured support (T-990)
//!
//! The band plan says what is **allocated** here. On its own that explains nothing: the identical
//! suggestion is offered to an empty channel, and an explanation that does not discriminate is a
//! map of the band rather than evidence (the T-545 wording, applied to the allocation-only path
//! it was written about). The explorer met the consequence directly — an airband with nothing on
//! the air, 52 noise rows in 8 minutes, every one of them topped by "Aviation voice (VHF AM)",
//! and DC and integer-MHz spurs given "Aviation voice", "Amateur radio" and "AIS" with nothing
//! measured behind them. Three rules follow, and they are about ranking and disclosure, not about
//! deleting suggestions: every candidate the band plan offers is still listed.
//!
//! 1. **Rank 1 belongs to a measurement.** If no candidate has `evidence_confidence > 0`, an
//!    [`UNIDENTIFIED`] explanation is inserted at rank 1 (flag `no-measured-support`), stating
//!    that nothing measured names a service and listing what the band plan allocates here. This
//!    also answers the other half of the finding — a band whose allocation table has no row got
//!    *no explanation at all* (460–470 MHz), where it now says so in words.
//! 2. **Every candidate discloses what the measurement says about it.** [`SERVICE_SHAPES`] states
//!    the occupied-bandwidth window a real emission of each allocation service falls in, derived
//!    from the service's own channelisation. Each candidate carries an
//!    [`ExplanationEvidence::Occupancy`] row with the measured width and duty, the expected
//!    window and a [`Support`] verdict. An allocation-only candidate the measurement
//!    **contradicts** is flagged `measurement-contradicts` and discounted by `FIT_CONTRADICTED`:
//!    230 kHz of occupied bandwidth is not a 25 kHz aviation voice channel. Services with no
//!    bandwidth signature (amateur; Part 15, which spans a 20 kHz remote and a 20 MHz WLAN
//!    channel) state none, and read `not-discriminating` rather than pretending to a test.
//! 3. **A receiver artefact is explained as one — but only where the mechanism was measured.**
//!    [`artefact_verdict`] reads the repository for a standing `artifact-of` relation (T-219,
//!    with its arithmetic) or for an **established** mechanism on a majority of the row's newest
//!    detections ([`Repository::emitter_receiver_artefact_share`]): an IQ image, an
//!    intermodulation product, a hit in a *measured* spur mask, or a retune-settled LO-relative
//!    verdict. Such a row gets exactly one explanation, [`RECEIVER_ARTEFACT`], and **no** service
//!    candidate: nothing was on the air, so no allocation applies to it.
//!
//!    A bare **coincidence** between the emission's frequency and one of the receiver's own
//!    numbers — DC at the tuned centre, `n × 10 MHz`, `n × fs`, a comb tooth — is *not* a verdict
//!    and never has been able to be one here. 120.000 MHz is a 10 MHz multiple and a valid 25 kHz
//!    airband channel; a dwell centres the radio on what it is listening to, so a real emission
//!    being demodulated is at the tuned centre too (AWARE-042's real 12.5 kHz channels are `dc`
//!    on 100 % of their detections); and the retune test that would settle DC is unreachable per
//!    emitter, because a DC line under a wide retune lands at a different frequency and becomes a
//!    different emitter. So a coincidence is **said and not claimed**: [`coincidence_note`] puts
//!    the mechanism and its count on the rank-1 row, in words, with the allocations still ranked
//!    below. Deciding it belongs where the detection is admitted (T-948's per-device, per-rate DC
//!    + spur mask) — and a row that mask lists carries `spur-map`, which *is* established here.
//!
//!    Two further limits, from the first T-990 review: `clipped` and `compressed` never count (a
//!    real station strong enough to clip the ADC on one high-gain tune is still a real station,
//!    and an artefact label would override what was measured); and a row carrying demodulator,
//!    decoder or classifier evidence keeps it at rank 1 and shows the artefact only as an
//!    alternative at [`ARTEFACT_ALTERNATIVE_SCORE`], so the status that evidence earned stands.
//!    The verdict is derived on every call and never written down, so it is revocable in both
//!    directions with nothing to un-tag.
//! 4. **The `unidentified` row carries the raster verdict it displaced.** The client reads
//!    `explanations[0]` for the off-raster chip and the channel-raster readout, so rank 1 copies
//!    the `off-raster` flag and the [`ExplanationEvidence::Raster`] entry of the best candidate
//!    below it. A centre off its channel raster is flagged, never dropped and never snapped.
//!
//! Neither pseudo-service carries non-shape evidence, so neither can set a status.
//!
//! **Status from the best status-backed candidate.** Occupancy (shape) evidence ranks and
//! suggests, but never sets a status: a continuous 150–400 kHz carrier anywhere is not "FM
//! broadcast" until a demodulator, decoder or classifier says so. Each candidate carries
//! `status_evidence_confidence`, the noisy OR of its non-shape evidence only. The best-scoring
//! candidate whose `status_evidence_confidence` is at least [`MIN_CONFIDENCE`] sets the status
//! and `prior_ref` from its band-plan verdict, even when a weaker candidate scores above it.
//! With none, the status is `unknown`. As in the repository, a status authored by a user or
//! decoder is never overridden, and an unchanged verdict is not repeated.
//!
//! **Where explanations are kept.** They are stored as an Emitter Annotation (author
//! `classifier`, `author_ref` [`FAMILY_MAP_VERSION`], `metadata.explanations`, no content,
//! class `metadata-only`). A new row is written only when the ranking changes, superseding the
//! previous one. [`explanations`] reads them, and `/api/inventory` serves them per row.
//!
//! **Decision: unmapped FSK in an ISM band keeps status `unknown`.** A bare `2fsk` at 915 MHz or
//! 433.92 MHz does not say "Part 15 device". It could be amateur, telemetry or LMR data sharing
//! the band, and `known` would hide exactly the unknown sensors AWARE-036 is about (docs/07 §3).
//! Its explanations still list the ISM / Part 15 allocation as an allocation-only suggestion. A
//! status of `known` via Part 15 comes from a protocol decode instead: a decoder that maps to
//! `ism` (e.g. `rtl_433`), or a CRC-valid framing ground truth, which the framed-burst writer
//! records as a decoder-authored status.
//!
//! # Where it is wired
//!
//! - **Closed tracks.** [`crate::TrackInventory`] adds a Classification from [`track_family`]
//!   when the track's occupancy maps, records the sighting, then runs [`explain_emitter`].
//! - **Chain writers.** After the analog (WFM) and FSK record writers write an emitter,
//!   `Inventory::chain_emitter` runs [`explain_emitter`].
//! - **Decoder chains (plugin decoders, T-037b).** When a decoder produced a valid decode for an
//!   emitter, the chain calls
//!   `family::record_decoder_evidence(&mut repo, emitter, decoder_id, confidence, t)` with the
//!   decoder's manifest id (e.g. `readsb`, `rtl_433`), then `Inventory::chain_emitter` as the other
//!   chains do. The first call stores a Classification (family = the decoder id, `model_version`
//!   `decoder:<id>`) when the id maps to a service; the second re-ranks. Decoder ids that map to
//!   nothing store nothing.
//! - **Status or classification changes.** A user reclassification ([`reclassify`]) or a
//!   user/decoder status change ([`set_status`]) re-ranks at once instead of waiting for the next
//!   sighting. A decoder-set status written inside a chain (the FSK framed-burst writer) is
//!   re-ranked by the `chain_emitter` call that follows it.
//!
//! # Legal guardrail
//!
//! A family, prior or explanation only sets `known_status`, `prior_ref`, the Classification row
//! and a metadata-only Annotation. It never feeds `class::classify_emitter`, the source class,
//! identity gating or content decisions. Restricted classes (paging and cellular bands, T-027) and
//! `metadata-only` stay exactly as restrictive with a mapped family. Explanations carry service
//! labels, band-plan references and measured offsets: no content and no identity.

use std::collections::BTreeMap;

use hk_context::{BandTable, Region, match_known_status};
// T-218: a user reclassification is written at the user arbitration rank (ADR-0016 §2).
use hk_detect::TrackSummary;
use hk_detect::track::HopSetSummary;
use hk_detect::track::inventory::SUSPECT_FRACTION;
use hk_model::ReceiverArtefactShare;
use hk_model::classify::{ArbRank, Stage};
use hk_model::relate::{ArtifactKind, RelationKind};
use hk_model::{
    Annotation, AnnotationAuthor, AnnotationId, AnnotationKind, AnnotationTarget, Classification,
    ContentClass, EmitterId, KnownStatus, KnownStatusChange, KnownStatusPrior, PriorVerdict,
    RepoError, Repository, StatusAuthor, Timestamp,
};
use serde::{Deserialize, Serialize};

/// `Classification.model_version` of families this module assigns, and `author_ref` of its
/// explanation annotations.
pub const FAMILY_MAP_VERSION: &str = "hk-pipeline/family-map@1";

/// Smallest evidence confidence that sets a status.
pub const MIN_CONFIDENCE: f64 = 0.5;

/// Explanations kept per emitter.
pub const TOP_K: usize = 5;

/// Base score of a candidate the band plan suggests without any signal evidence.
pub const ALLOCATION_ONLY_SCORE: f64 = 0.2;

const FIT_NO_DATA: f64 = 0.8;
const FIT_UNEXPECTED: f64 = 0.7;
const FIT_OFF_RASTER: f64 = 0.8;
/// Allocation fit of a suggestion the emitter's own occupancy contradicts (T-990).
const FIT_CONTRADICTED: f64 = 0.25;

/// The honest top explanation of a row nothing was measured *about*: the service name for
/// [`service_label`] and [`Explanation::service`] (T-990).
pub const UNIDENTIFIED: &str = "unidentified";
/// The explanation of a row the receiver made: the service name for [`service_label`] and
/// [`Explanation::service`] (T-990).
pub const RECEIVER_ARTEFACT: &str = "receiver-artefact";
/// Score of the [`RECEIVER_ARTEFACT`] candidate when it ranks as an alternative beside measured
/// evidence: above a bare allocation ([`ALLOCATION_ONLY_SCORE`]), because a receiver mechanism was
/// measured on the row's detections, and below anything a demodulator, decoder or classifier said.
pub const ARTEFACT_ALTERNATIVE_SCORE: f64 = 0.3;

/// Occupied bandwidth of a continuous emission read as broadcast FM, Hz — derived from the
/// service, not from any capture.
///
/// The FM stereo multiplex (47 CFR 73.322; ITU-R BS.450) carries L+R at 0–15 kHz, the pilot at
/// 19 kHz, the L−R DSB-SC subcarrier at 23–53 kHz and optional RDS at 57 kHz, with a maximum
/// deviation of ±75 kHz that is reached only at full modulation. Carson's rule `2(Δf + f_m)` at
/// full deviation gives `2(75 + 57) = 264 kHz` with RDS and `2(75 + 53) = 256 kHz` without. As
/// deviation falls the bandwidth tends to the narrowband-FM limit `2·f_m`, and the highest
/// component a stereo station always carries is the 53 kHz subcarrier top, so it cannot fall
/// below `2 × 53 = 106 kHz` and still be stereo broadcast FM. OBW99 also measures under Carson,
/// which counts tails that the 99 % point excludes. Hence 106 kHz: the narrowband limit of the
/// stereo multiplex. The upper bound keeps headroom above Carson for OBW99 on a strong carrier;
/// it is permissive, and it is not what this constant got wrong.
///
/// **The lower bound was 150 kHz until T-316**, taken from "the detector's OBW99 … 333 kHz on the
/// 101.3 MHz fixture station (unverified beyond that fixture)". That 333 kHz was never a
/// measurement of broadcast FM: the emitter spanned 101.1366–101.4694 MHz, its upper edge inside
/// a 75 kHz shelf of raised *noise* at 101.428–101.503 MHz that the detector was reporting as
/// signal. With those false alarms gone the same station measures 136 kHz, and T-289 measured it
/// independently at 128 kHz on a different capture by a different method — both excluded by the
/// old bound, so a correctly measured FM station no longer matched its own prior. (The 99.6999 MHz
/// station of that capture measures 70 kHz, but it sits 1.10 MHz off centre, outside the 875 kHz
/// half-width of the baseband filter, so its width is the filter's and says nothing about FM.)
///
/// The error runs both ways: too high and a correctly measured station is never suggested as FM
/// broadcast (the T-316 bug); too low and narrower continuous services begin reading as broadcast
/// FM on width alone. A continuous emission in this band carries only 0.6 confidence: it is shape
/// evidence, not a demodulation.
pub const WIDEBAND_FM_OBW_HZ: [f64; 2] = [106e3, 400e3];

/// Smallest duty cycle counted as continuous.
pub const CONTINUOUS_DUTY: f64 = 0.9;

/// A service's channel raster inside a frequency range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelRaster {
    /// Allocation-table region the raster belongs to.
    pub region: Region,
    /// Service family.
    pub service: &'static str,
    /// Range the raster applies to, Hz.
    pub range_hz: [f64; 2],
    /// Channel spacing, Hz.
    pub raster_hz: f64,
    /// Channel centres at `k · raster_hz + offset_hz`.
    pub offset_hz: f64,
    /// Largest centre offset still on the raster, Hz.
    pub tolerance_hz: f64,
    /// Source.
    pub source: &'static str,
}

/// Channel rasters. US FM broadcast: 200 kHz channels on odd tenths of a MHz, 87.9–107.9 MHz (47 CFR
/// 73.201, channels 200–300; unverified this session). The 20 kHz tolerance is 10 % of the
/// raster; the detector's centre error on the fixture station is about 3 kHz. Every entry is keyed
/// by its region; other regions' rasters (e.g. 100 kHz FM steps in ITU Region 1) are future work.
pub const CHANNEL_RASTERS: &[ChannelRaster] = &[
    ChannelRaster {
        region: Region::Us,
        service: "fm-broadcast",
        range_hz: [87.8e6, 108.0e6],
        raster_hz: 200e3,
        offset_hz: 100e3,
        tolerance_hz: 20e3,
        source: "47 CFR 73.201 (US FM channels, 200 kHz on odd tenths)",
    },
    // T-979: US UHF television, channels 14-36, 6 MHz each, centres at 473 MHz + 6 MHz·k. The
    // tolerance is ~40 ppm at 600 MHz: wide enough for any receiver clock error (the explorer's
    // was 4 ppm), tight enough that a station genuinely off the raster is flagged, not snapped.
    ChannelRaster {
        region: Region::Us,
        service: "tv-broadcast",
        range_hz: [470e6, 608e6],
        raster_hz: 6e6,
        offset_hz: 5e6,
        tolerance_hz: 25e3,
        source: "47 CFR 73.603(a), 73.699 Fig. 1 (US TV channels 14-36, 6 MHz)",
    },
];

/// Kind of a vocabulary entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceKind {
    /// A demodulator mode.
    DemodMode,
    /// A modulation label.
    Modulation,
    /// A decoder id.
    Decoder,
}

/// One row of the vocabulary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VocabEntry {
    /// Evidence label, lower case.
    pub label: &'static str,
    /// Kind.
    pub kind: EvidenceKind,
    /// Service family, or `None` (unmapped).
    pub service: Option<&'static str>,
    /// Mapping confidence (0 when unmapped).
    pub confidence: f64,
    /// Why.
    pub note: &'static str,
}

const fn entry(
    label: &'static str,
    kind: EvidenceKind,
    service: Option<&'static str>,
    confidence: f64,
    note: &'static str,
) -> VocabEntry {
    VocabEntry {
        label,
        kind,
        service,
        confidence,
        note,
    }
}

use EvidenceKind::{Decoder, DemodMode, Modulation};

const SHARED_NBFM: &str =
    "NBFM is shared by land mobile, amateur, marine, public safety and FRS/GMRS";
const SHARED_AM: &str = "AM voice is shared by aviation, AM broadcast, CB and amateur";
const SHARED_HF: &str = "SSB/CW are shared by amateur and HF utility services";
const MODULATION_ONLY: &str = "a modulation names no service; a Part 15 / ISM status needs a \
     protocol decode (e.g. rtl_433) or a CRC-valid framing ground truth";
const RDS_NOTE: &str = "RDS rides only on FM broadcast";
/// Why a measured 8VSB pilot identifies television and nothing else (T-979).
const ATSC_NOTE: &str = "a 5.381 MHz flat band with a CW pilot on its lower edge is ATSC 1.0 \
     8VSB (A/53 Part 2 §5.1.2); no other service emits that pair";
const ADSB_NOTE: &str = "CRC-checked Mode S / ADS-B frames";
const ISM_NOTE: &str = "a Part 15 sensor protocol decoded";
/// Why a decoded trunking control channel is public-safety / land-mobile evidence, and why it is
/// *measured* evidence rather than an allocation row.
///
/// T-545 found the only explanation on a fully decoded P25 control channel scoring
/// `evidence_confidence: 0` with the flag `allocation-only` — the identical suggestion the
/// analogue FM neighbours got, and the one an empty channel would get, because 851–869 MHz is a
/// public-safety allocation. **An explanation that does not discriminate is not evidence; it is a
/// map of the band.** What the run actually held and spent nothing of: a continuous narrowband
/// emission on the 12.5 kHz LMR raster, four-level, frame-synced, with CRC-valid trunking blocks.
/// Nothing else transmits a CRC-valid trunked control channel, so the decode names the service.
const TRUNK_CC_NOTE: &str = "CRC-valid trunking control blocks: a continuous narrowband four-level emission on the LMR \
     raster whose frame sync and check both hold. Trunked LMR is public safety and land mobile";

/// Why a FLEX decode is paging evidence (T-950): the frame sync and the BCH-checked frame
/// information word are the protocol's own, and FLEX carries pages and nothing else. The
/// allocation (929–932 MHz paging, 47 CFR 22 / 24 / 90) then *agrees* or *disagrees* — it never
/// decided the decoder.
const FLEX_NOTE: &str = "FLEX frames decoded: sync-1 and a BCH(31,21)-checked frame information word. \
     FLEX is a paging protocol";
/// T-977: what a frame sync without a check actually says.
const TRUNK_SYNC_NOTE: &str = "frame sync at the expected spacing with no CRC-valid control block: the air interface is \
     recognised, the channel is not a control channel (a voice or data channel of the same system \
     looks exactly like this)";

/// The vocabulary (see the module table).
pub const VOCABULARY: &[VocabEntry] = &[
    entry(
        "wfm",
        DemodMode,
        Some("fm-broadcast"),
        0.9,
        "200 kHz wideband FM is the broadcast service (47 CFR 73 subpart B)",
    ),
    // T-979. `Modulation` is the nearest kind, but this label is never a bare modulation call:
    // `hk_estimate::atsc` writes it only when a 5.381 MHz flat band AND a CW pilot on its lower
    // edge were both measured, which is a positive identification of ATSC 1.0 and of nothing
    // else. It is therefore status-setting evidence, not shape.
    entry(
        "atsc-8vsb",
        Modulation,
        Some("tv-broadcast"),
        0.97,
        ATSC_NOTE,
    ),
    entry("nbfm", DemodMode, None, 0.0, SHARED_NBFM),
    entry("nfm", DemodMode, None, 0.0, SHARED_NBFM),
    entry("am", DemodMode, None, 0.0, SHARED_AM),
    entry("ssb", DemodMode, None, 0.0, SHARED_HF),
    entry("cw", DemodMode, None, 0.0, SHARED_HF),
    entry("unknown", DemodMode, None, 0.0, "no family"),
    entry("2fsk", Modulation, None, 0.0, MODULATION_ONLY),
    entry("fsk", Modulation, None, 0.0, MODULATION_ONLY),
    entry("gfsk", Modulation, None, 0.0, MODULATION_ONLY),
    entry("ook", Modulation, None, 0.0, MODULATION_ONLY),
    entry("bpsk", Modulation, None, 0.0, MODULATION_ONLY),
    entry("qpsk", Modulation, None, 0.0, MODULATION_ONLY),
    entry("hk-rds", Decoder, Some("fm-broadcast"), 0.99, RDS_NOTE),
    entry("rds", Decoder, Some("fm-broadcast"), 0.99, RDS_NOTE),
    entry("readsb", Decoder, Some("adsb"), 0.99, ADSB_NOTE),
    entry("dump1090", Decoder, Some("adsb"), 0.99, ADSB_NOTE),
    entry("ais-catcher", Decoder, Some("ais"), 0.95, "AIS frames"),
    entry("rtl_433", Decoder, Some("ism"), 0.9, ISM_NOTE),
    entry("rtl-433", Decoder, Some("ism"), 0.9, ISM_NOTE),
    entry(
        "aptdec",
        Decoder,
        Some("noaa-apt"),
        0.9,
        "APT imagery lines",
    ),
    // T-546: the trunking chain's own decoders. The id names the framing that CRC-checked, so a
    // reader can tell which air interface said so, and every one of them is a *decode*, never a
    // band-plan lookup.
    entry(
        "p25-tsbk",
        Decoder,
        Some("public-safety"),
        0.97,
        TRUNK_CC_NOTE,
    ),
    entry(
        "dmr-csbk",
        Decoder,
        Some("public-safety"),
        0.97,
        TRUNK_CC_NOTE,
    ),
    entry(
        "nxdn-cac",
        Decoder,
        Some("public-safety"),
        0.97,
        TRUNK_CC_NOTE,
    ),
    entry("flex", Decoder, Some("paging"), 0.97, FLEX_NOTE),
    // T-977: frame sync at the expected spacing with NO CRC-valid control block. The same service
    // family, at less certainty, under its own id — a P25 voice or data channel carries P25's
    // 48-bit frame sync and no TSBK, so this is what "P25-like" is measured as.
    entry(
        "p25-frame-sync",
        Decoder,
        Some("public-safety"),
        0.8,
        TRUNK_SYNC_NOTE,
    ),
    entry(
        "dmr-frame-sync",
        Decoder,
        Some("public-safety"),
        0.8,
        TRUNK_SYNC_NOTE,
    ),
    entry(
        "nxdn-frame-sync",
        Decoder,
        Some("public-safety"),
        0.8,
        TRUNK_SYNC_NOTE,
    ),
];

/// Service family names that pass through unchanged (each one accepted by
/// [`hk_context::is_service_family`]; checked by a test), with their canonical ranking key.
const SERVICE_PASSTHROUGH: &[(&str, &str)] = &[
    ("fm-broadcast", "fm-broadcast"),
    ("adsb", "adsb"),
    ("mode-s", "adsb"),
    ("ais", "ais"),
    ("noaa-apt", "noaa-apt"),
    ("noaa-wx", "noaa-wx"),
    ("noaa-weather", "noaa-wx"),
    ("amateur", "amateur"),
    ("ham", "amateur"),
    ("aviation-voice", "aviation-voice"),
    ("aviation-am", "aviation-voice"),
    ("aviation-vhf-comm", "aviation-voice"),
    ("gnss", "gnss"),
    ("cellular", "cellular"),
    ("lte", "cellular"),
    ("public-safety", "public-safety"),
    ("p25", "public-safety"),
    ("dmr", "public-safety"),
    ("ism", "ism"),
    ("fsk-ism", "ism"),
    ("ook-ism", "ism"),
    ("lora", "lora"),
    // T-953: the 929-932 MHz paging allocation, and the frequency-hopping *behaviour* the
    // detector measures directly. `flex`/`pocsag` name the service because nothing else carries
    // those air interfaces; a bare `2fsk` at 929 MHz still names nothing (the rule above).
    // `flex` is not here: since T-950 it is the FLEX decoder's own vocabulary entry (a
    // BCH-checked frame decode, `FLEX_NOTE`), and a name is either a service or a label, never
    // both.
    ("paging", "paging"),
    ("pocsag", "paging"),
    ("pager", "paging"),
    ("fhss", "fhss"),
    // T-979: UHF television and the Part 74 low power auxiliary use that shares its channels.
    ("tv-broadcast", "tv-broadcast"),
    ("atsc", "tv-broadcast"),
    ("dtv", "tv-broadcast"),
    ("wireless-mic", "wireless-mic"),
    ("low-power-auxiliary", "wireless-mic"),
];

/// Canonical services the band plan can suggest as allocation-only candidates.
const ALLOCATION_SERVICES: &[&str] = &[
    "fm-broadcast",
    "aviation-voice",
    "adsb",
    "ais",
    "noaa-apt",
    "noaa-wx",
    "amateur",
    "gnss",
    "cellular",
    "public-safety",
    "ism",
    "paging",
    "tv-broadcast",
    "wireless-mic",
];

/// What a real emission of a service **measures like**: the occupied-bandwidth window the
/// service's own channelisation implies, and whether it is continuous. Derived from the
/// allocation and its channel plan, never from a capture.
///
/// T-990's rule, and why the table exists: the band plan says what is *allocated* here, and an
/// allocation on its own explains nothing. An explanation has to be consistent with what was
/// measured, so every allocation-only suggestion is checked against the emitter's own occupancy
/// and says, in its evidence, whether the measurement supports it, contradicts it, or cannot
/// discriminate. A 200 kHz carrier inside 118–137 MHz is not aviation voice however clearly the
/// band is allocated to it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ServiceShape {
    service: &'static str,
    /// Occupied-bandwidth window a real emission of the service falls in, Hz. `None` where the
    /// service has **no** bandwidth signature, so no width can support or contradict it.
    obw_hz: Option<[f64; 2]>,
    /// The service is a continuous emission (duty at least [`CONTINUOUS_DUTY`]).
    continuous: bool,
    /// Where the window comes from.
    why: &'static str,
}

const fn shape(
    service: &'static str,
    obw_hz: Option<[f64; 2]>,
    continuous: bool,
    why: &'static str,
) -> ServiceShape {
    ServiceShape {
        service,
        obw_hz,
        continuous,
        why,
    }
}

/// The measurable shape of each [`ALLOCATION_SERVICES`] entry (a test checks the two lists agree).
const SERVICE_SHAPES: &[ServiceShape] = &[
    shape(
        "fm-broadcast",
        Some(WIDEBAND_FM_OBW_HZ),
        true,
        "the stereo multiplex between its narrowband limit and Carson's rule (WIDEBAND_FM_OBW_HZ)",
    ),
    shape(
        "aviation-voice",
        Some([4e3, 25e3]),
        false,
        "DSB-AM voice (ICAO Annex 10 A3E) on a 25 kHz raster — 8.33 kHz in Europe; the emission \
         is ~6-8 kHz wide and cannot exceed its channel",
    ),
    shape(
        "adsb",
        Some([0.5e6, 20e6]),
        false,
        "1090 MHz Mode S pulse-position modulation at 1 Mbit/s: a megahertz-class main lobe",
    ),
    shape(
        "ais",
        Some([6e3, 30e3]),
        false,
        "GMSK 9600 Bd in a 25 kHz maritime channel (ITU-R M.1371)",
    ),
    shape(
        "noaa-apt",
        Some([15e3, 60e3]),
        false,
        "APT: a 2.4 kHz subcarrier on an FM carrier at ~17 kHz deviation, ~40 kHz occupied",
    ),
    shape(
        "noaa-wx",
        Some([8e3, 25e3]),
        false,
        "NBFM voice in a 25 kHz NOAA weather-radio channel",
    ),
    shape(
        "amateur",
        None,
        false,
        "amateur emissions run from a 100 Hz CW carrier to megahertz-wide ATV: no width supports \
         or contradicts the allocation",
    ),
    shape(
        "gnss",
        Some([1e6, 30e6]),
        true,
        "BPSK(1) and wider codes: megahertz-class, continuous, and below the noise floor",
    ),
    shape(
        "cellular",
        Some([1e6, 25e6]),
        false,
        "LTE/NR channel bandwidths run 1.4-20 MHz",
    ),
    shape(
        "public-safety",
        Some([4e3, 30e3]),
        false,
        "narrowband land mobile on a 12.5 / 25 kHz raster",
    ),
    shape(
        "ism",
        None,
        false,
        "Part 15 covers a 20 kHz OOK remote, a 500 kHz LoRa chirp and a 20 MHz WLAN channel: no \
         width supports or contradicts the allocation",
    ),
    // T-950/T-953: 929-932 MHz paging. FLEX (1600/3200/6400 bit/s, 2- or 4-level FSK at +/-4.8 kHz)
    // and POCSAG (512-2400 bit/s 2-FSK at +/-4.5 kHz) sit in 25 kHz channels; a paging transmitter
    // keys up per batch, so it is not required to be continuous.
    shape(
        "paging",
        Some([4e3, 30e3]),
        false,
        "FLEX / POCSAG FSK at +/-4.5-4.8 kHz deviation in a 25 kHz paging channel (47 CFR Part \
         22 Subpart E, 90.494): ~10-20 kHz occupied",
    ),
    // T-979: the two services the 470-608 MHz rows added. Both are strongly discriminating, and
    // against each other: a 6 MHz DTV channel is not a wireless microphone, and a 200 kHz mic is
    // not a television station. That is exactly the discrimination T-990 exists to disclose, and
    // it is why the fragments the explorer saw inside channels 29/30 could never be either.
    shape(
        "tv-broadcast",
        Some([4.5e6, 6.5e6]),
        true,
        "8VSB occupies the 5.381 MHz Nyquist band of its 6 MHz channel (ATSC A/53 Part 2 §5.1.2; \
         47 CFR 73.603 for the channel), and a DTV transmitter is on air continuously",
    ),
    shape(
        "wireless-mic",
        Some([10e3, 200e3]),
        false,
        "47 CFR 74.861(e): a low power auxiliary station in the TV bands is authorised 200 kHz, \
         and an analogue mic at +/-15-75 kHz deviation occupies a fraction of it",
    ),
];

/// Whether an emitter's measured occupancy supports a service (T-990).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Support {
    /// The measurement falls inside the shape the service must have.
    Supports,
    /// The measurement falls outside it: the allocation cannot explain this emission.
    Contradicts,
    /// The service has no bandwidth signature, so no width can decide either way.
    NotDiscriminating,
    /// Nothing was measured.
    Unmeasured,
}

impl Support {
    /// The stored text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supports => "measurement-supports",
            Self::Contradicts => "measurement-contradicts",
            Self::NotDiscriminating => "measurement-not-discriminating",
            Self::Unmeasured => "measurement-unavailable",
        }
    }
}

/// Reads `occupancy` against `service`'s shape. Services with no entry are not allocation
/// candidates, so they cannot be reached from the ranking; they read `NotDiscriminating`.
fn support_for(service: &str, occupancy: &Occupancy) -> (Support, Option<ServiceShape>) {
    let Some(sh) = SERVICE_SHAPES
        .iter()
        .copied()
        .find(|s| s.service == service)
    else {
        return (Support::NotDiscriminating, None);
    };
    let Some([lo, hi]) = sh.obw_hz else {
        return (Support::NotDiscriminating, Some(sh));
    };
    if !(occupancy.bandwidth_hz.is_finite() && occupancy.bandwidth_hz > 0.0) {
        return (Support::Unmeasured, Some(sh));
    }
    let width_ok = (lo..=hi).contains(&occupancy.bandwidth_hz);
    let duty_ok = !sh.continuous || occupancy.duty_cycle.is_none_or(|d| d >= CONTINUOUS_DUTY);
    let verdict = if width_ok && duty_ok {
        Support::Supports
    } else {
        Support::Contradicts
    };
    (verdict, Some(sh))
}

/// Human label of a canonical service family.
pub fn service_label(service: &str) -> &'static str {
    match service {
        "fm-broadcast" => "FM broadcast",
        "aviation-voice" => "Aviation voice (VHF AM)",
        "adsb" => "ADS-B / Mode S",
        "ais" => "AIS",
        "noaa-apt" => "NOAA APT weather satellite",
        "noaa-wx" => "NOAA weather radio",
        "amateur" => "Amateur radio",
        "gnss" => "GNSS",
        "cellular" => "Cellular",
        "public-safety" => "Public safety / land mobile",
        "ism" => "ISM / Part 15 device",
        "lora" => "LoRa (Part 15)",
        "paging" => "Paging (929-932 MHz)",
        "fhss" => "Frequency-hopping system (Part 15 §15.247)",
        "tv-broadcast" => "TV broadcast",
        "wireless-mic" => "Wireless microphone (Part 74)",
        UNIDENTIFIED => "Unidentified emission",
        RECEIVER_ARTEFACT => "Receiver artefact",
        _ => "Other service",
    }
}

/// Measured occupancy of an emission.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Occupancy {
    /// Occupied bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Fraction of time on air, 0–1.
    pub duty_cycle: Option<f64>,
    /// Symbol rate, Bd (maps nothing on its own today).
    pub symbol_rate_hz: Option<f64>,
}

/// A measured frequency-hopping set (T-953): what the tracker linked, as evidence.
///
/// A [`HopSetSummary`] only exists after the tracker's
/// own gate has held — ≥ 3 channels with ≥ 2 links each, ≥ 10 hops, over half of each member's
/// bursts linked, every member above `min_channel_snr_db`, and the periodic-emitter veto passed
/// (`hk_detect::track`, §7). That gate *is* the measurement of hopping, so nothing is re-gated
/// here.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HopSet {
    /// Member channels.
    pub channels: usize,
    /// Hops linked.
    pub hops: u64,
    /// Channel raster, Hz, when one was fitted.
    pub raster_hz: Option<f64>,
    /// Mean dwell, s.
    pub dwell_s: Option<f64>,
}

/// Evidence about an emitter's family.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Evidence<'a> {
    /// A demodulator mode or modulation label (or a service family name).
    Label(&'a str),
    /// A decoder id.
    Decoder(&'a str),
    /// Occupancy.
    Occupancy(Occupancy),
    /// A measured hop set (T-953).
    HopSet(HopSet),
}

/// The outcome of mapping evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct FamilyCall {
    /// The canonical service family, when mapped.
    pub service: Option<&'static str>,
    /// Mapping confidence, 0–1.
    pub confidence: f64,
    /// Why, in words.
    pub reason: String,
}

impl FamilyCall {
    fn unmapped(reason: impl Into<String>) -> Self {
        Self {
            service: None,
            confidence: 0.0,
            reason: reason.into(),
        }
    }

    /// The service family, when mapped at or above [`MIN_CONFIDENCE`].
    pub fn confident_service(&self) -> Option<&'static str> {
        self.service.filter(|_| self.confidence >= MIN_CONFIDENCE)
    }

    /// The Classification to record at `t`, when [`Self::confident_service`] is set.
    pub fn classification(&self, t: Timestamp) -> Option<Classification> {
        self.confident_service().map(|family| Classification {
            t,
            family: family.into(),
            confidence: self.confidence,
            open_set_score: 1.0 - self.confidence,
            model_version: FAMILY_MAP_VERSION.into(),
        })
    }
}

/// The vocabulary entry for `label` (trimmed, case-insensitive).
pub fn lookup(label: &str) -> Option<&'static VocabEntry> {
    let l = label.trim();
    VOCABULARY.iter().find(|e| e.label.eq_ignore_ascii_case(l))
}

/// Maps a label, a decoder id or a service family name.
fn map_name(name: &str) -> FamilyCall {
    let n = name.trim();
    if let Some(&(_, canonical)) = SERVICE_PASSTHROUGH.iter().find(|(s, _)| *s == n) {
        return FamilyCall {
            service: Some(canonical),
            confidence: 1.0,
            reason: format!("{n} is a service family"),
        };
    }
    match lookup(n) {
        Some(e) => FamilyCall {
            service: e.service,
            confidence: e.confidence,
            reason: match e.service {
                Some(s) => format!("{n} → {s} ({:.2}: {})", e.confidence, e.note),
                None => format!("no service mapping for family {n:?} ({})", e.note),
            },
        },
        None => FamilyCall::unmapped(format!("no service mapping for family {n:?}")),
    }
}

/// Maps one piece of evidence to a service family (see the module table).
pub fn service_family(evidence: &Evidence<'_>) -> FamilyCall {
    match evidence {
        Evidence::Label(l) | Evidence::Decoder(l) => map_name(l),
        // T-953: the detector linked dwells across channels, which is a measurement of hopping
        // and of nothing else. It suggests `fhss` — a *behaviour*, not a system: what hops here
        // could be a Part 15 §15.247 device, a cordless phone, a telemetry link or a radar. It is
        // shape evidence, so it ranks and never sets a status (see the module docs).
        Evidence::HopSet(h) => FamilyCall {
            service: Some("fhss"),
            confidence: HOP_SET_CONFIDENCE,
            reason: format!(
                "{} channels linked by {} hops{}{}: a frequency-hopping emission",
                h.channels,
                h.hops,
                h.raster_hz
                    .map(|r| format!(" on a {:.1} kHz raster", r / 1e3))
                    .unwrap_or_default(),
                h.dwell_s
                    .map(|d| format!(", {:.1} ms dwell", d * 1e3))
                    .unwrap_or_default(),
            ),
        },
        Evidence::Occupancy(o) => {
            let continuous = o.duty_cycle.is_some_and(|d| d >= CONTINUOUS_DUTY);
            let [lo, hi] = WIDEBAND_FM_OBW_HZ;
            if continuous && o.bandwidth_hz >= lo && o.bandwidth_hz <= hi {
                FamilyCall {
                    service: Some("fm-broadcast"),
                    confidence: 0.6,
                    reason: format!(
                        "continuous {:.0} kHz emission: the broadcast-FM channel shape",
                        o.bandwidth_hz / 1e3
                    ),
                }
            } else {
                FamilyCall::unmapped(format!(
                    "occupancy {:.1} kHz, duty {:?}, symbol rate {:?} maps no service",
                    o.bandwidth_hz / 1e3,
                    o.duty_cycle,
                    o.symbol_rate_hz
                ))
            }
        }
    }
}

/// Confidence of the `fhss` suggestion a measured hop set carries (T-953).
///
/// Not 1.0, and not a measurement of *what* is hopping. The tracker's link rule can in principle
/// chain independent emitters whose bursts abut — which is exactly what its channel count, link
/// fraction, SNR floor and periodic veto bound, and why they are not re-applied here — and
/// "something hops across these channels" identifies no system. It is well above
/// [`MIN_CONFIDENCE`] because the hopping itself was measured, and it is *shape* evidence, so it
/// can rank an explanation and can never set a `known_status`.
pub const HOP_SET_CONFIDENCE: f64 = 0.8;

/// The family evidence of a measured hop set (T-953): `fhss`, as a suggestion.
pub fn hop_set_family(h: &HopSetSummary) -> FamilyCall {
    service_family(&Evidence::HopSet(HopSet {
        channels: h.channels_hz.len(),
        hops: h.hops,
        raster_hz: h.raster_hz,
        dwell_s: h.dwell_s,
    }))
}

/// The family evidence of a closed channel track: its occupancy. A mostly suspect track (spur,
/// image, IMD, clipping) or a hop-set member maps nothing.
pub fn track_family(summary: &TrackSummary) -> FamilyCall {
    if summary.suspect_fraction > SUSPECT_FRACTION || summary.hop_set.is_some() {
        return FamilyCall::unmapped("suspect or hop-set track");
    }
    let duty = summary.track.timing.duty_cycle.or_else(|| {
        (summary.observed_s > 0.0).then(|| (summary.on_time_s / summary.observed_s).min(1.0))
    });
    service_family(&Evidence::Occupancy(Occupancy {
        bandwidth_hz: summary.track.bandwidth_hz,
        duty_cycle: duty,
        symbol_rate_hz: None,
    }))
}

/// The known-signal prior with the family vocabulary in front of hk-context's matcher: a stored
/// family (a demod mode, a decoder id or a service family) is mapped first; unmapped or
/// low-confidence families are `unknown`.
#[derive(Clone, Copy, Debug)]
pub struct FamilyPrior<'a> {
    table: &'a BandTable,
}

impl<'a> FamilyPrior<'a> {
    /// A prior over `table`.
    pub fn new(table: &'a BandTable) -> Self {
        Self { table }
    }
}

impl KnownStatusPrior for FamilyPrior<'_> {
    fn verdict(&self, family: &str, f_center_hz: f64, bandwidth_hz: f64) -> PriorVerdict {
        let call = map_name(family);
        let Some(service) = call.confident_service() else {
            return PriorVerdict {
                status: KnownStatus::Unknown,
                prior_ref: None,
                reason: if call.service.is_some() {
                    format!("family evidence too weak: {}", call.reason)
                } else {
                    call.reason
                },
            };
        };
        let m = match_known_status(self.table, service, f_center_hz, bandwidth_hz);
        PriorVerdict {
            status: m.status,
            prior_ref: m.prior_ref,
            reason: format!("{} [{}]", m.reason, call.reason),
        }
    }
}

/// One piece of family evidence held by an emitter.
#[derive(Clone, Debug, PartialEq)]
pub struct FamilyEvidence {
    /// Family as stored (demod mode, decoder id, service family).
    pub family: String,
    /// The classifier's confidence in it, 0–1.
    pub confidence: f64,
    /// Who classified (`Classification.model_version`).
    pub model_version: String,
}

/// Evidence behind one explanation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ExplanationEvidence {
    /// A stored family mapped to this service.
    Family {
        /// Family as stored.
        family: String,
        /// Classifier.
        model_version: String,
        /// Classifier confidence.
        confidence: f64,
        /// Vocabulary mapping confidence.
        mapping_confidence: f64,
    },
    /// The band-plan verdict for this service at the emitter's extent.
    BandPlan {
        /// Verdict.
        status: KnownStatus,
        /// Deciding allocation row.
        prior_ref: Option<String>,
        /// Why.
        reason: String,
    },
    /// A pilot tone measured from the signal that identifies a channelised service (T-979).
    ///
    /// This is the evidence an ATSC 8VSB explanation rests on: a CW line on the lower edge of the
    /// emission's own 5.381 MHz flat band, which the standard puts 309.440 559 kHz above the
    /// 6 MHz channel's lower edge. The measurement comes first; [`Self::nominal_hz`] and the ppm
    /// exist only when the measured channel landed on a known grid.
    Pilot {
        /// Standard the pilot belongs to, e.g. `atsc-8vsb`.
        standard: String,
        /// Measured pilot frequency, Hz.
        measured_hz: f64,
        /// One-sigma uncertainty of the measurement, Hz.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sigma_hz: Option<f64>,
        /// Where the standard puts the pilot of the channel the measurement landed on, Hz.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nominal_hz: Option<f64>,
        /// Measured minus nominal, Hz.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset_hz: Option<f64>,
        /// Receiver clock error that offset implies, ppm (C05 offset convention).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ppm: Option<f64>,
        /// Channel number on the grid, when it landed on one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<u32>,
        /// Pilot over the emission's own plateau, dB.
        excess_db: f64,
        /// Source of the nominal offset.
        source: String,
    },
    /// T-990: how the emitter's own measured occupancy reads against the shape a real emission
    /// of this service has to have. This is what stops a band-plan allocation from explaining,
    /// by itself, an emission whose measurements contradict it.
    Occupancy {
        /// Measured occupied bandwidth, Hz.
        bandwidth_hz: f64,
        /// Measured duty cycle, when one was measured.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duty_cycle: Option<f64>,
        /// The occupied-bandwidth window the service implies, Hz; absent where it has none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_obw_hz: Option<[f64; 2]>,
        /// The verdict.
        support: Support,
        /// Where the window comes from, and what the measurement did to it.
        reason: String,
    },
    /// T-990: the row is the receiver's own artefact, so no service explains it.
    Artefact {
        /// What said so: an `artifact-of` relation, or an emitter tag.
        source: String,
        /// Why, in words.
        reason: String,
    },
    /// Fit of the emitter's centre to the service's channel raster.
    Raster {
        /// Channel spacing, Hz.
        raster_hz: f64,
        /// Nearest channel centre, Hz.
        nearest_channel_hz: f64,
        /// Centre minus nearest channel, Hz.
        offset_hz: f64,
        /// Tolerance, Hz.
        tolerance_hz: f64,
        /// Within tolerance.
        on_raster: bool,
        /// Raster source.
        source: String,
        /// Which emitter centre was checked: [`CENTER_REFINED`] (output analysis, T-070) or
        /// [`CENTER_DETECTED`].
        #[serde(default = "detected_center")]
        center_source: String,
    },
}

/// Metadata key under which [`crate::atsc`] stores a measured channel pilot (T-979).
pub const PILOT_METADATA_KEY: &str = "pilot";

/// The service a [`PILOT_FINGERPRINT_KEY`] measurement is evidence for.
pub const PILOT_SERVICE: &str = "tv-broadcast";

/// The [`ExplanationEvidence::Pilot`] row in a pilot annotation's `metadata`, if it carries one.
///
/// Reading it back here is what makes an ATSC explanation *cite the pilot it was measured from*
/// instead of asserting a family and leaving the reader to trust it.
pub fn pilot_evidence(metadata: &serde_json::Value) -> Option<ExplanationEvidence> {
    let p = metadata.get(PILOT_METADATA_KEY)?;
    let f = |k: &str| p.get(k).and_then(serde_json::Value::as_f64);
    Some(ExplanationEvidence::Pilot {
        standard: p.get("standard")?.as_str()?.to_owned(),
        measured_hz: f("measured_hz")?,
        sigma_hz: f("sigma_hz"),
        nominal_hz: f("nominal_hz"),
        offset_hz: f("offset_hz"),
        ppm: f("ppm"),
        channel: p
            .get("channel")
            .and_then(serde_json::Value::as_u64)
            .map(|c| c as u32),
        excess_db: f("excess_db")?,
        source: p
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("ATSC A/53 Part 2 §5.1.2")
            .to_owned(),
    })
}

/// `Raster.center_source` of the emitter's detected centre.
pub const CENTER_DETECTED: &str = "detected";
/// `Raster.center_source` of a centre refined by output analysis (T-070).
pub const CENTER_REFINED: &str = "refined";

fn detected_center() -> String {
    CENTER_DETECTED.to_owned()
}

/// One ranked explanation of an emitter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    /// 1-based rank.
    pub rank: u32,
    /// Canonical service family, e.g. `fm-broadcast`.
    pub service: String,
    /// Human label, e.g. `FM broadcast`.
    pub label: String,
    /// Ranking score, 0–1.
    pub score: f64,
    /// Combined signal evidence for the service, 0–1 (0 = allocation-only).
    pub evidence_confidence: f64,
    /// Combined non-shape (demodulator, decoder, classifier) evidence, 0–1. Only this can set a
    /// status; 0 for a shape-only or allocation-only candidate.
    #[serde(default)]
    pub status_evidence_confidence: f64,
    /// Band-plan verdict for the service here.
    pub status: KnownStatus,
    /// Band-plan row, when one decided the verdict.
    pub prior_ref: Option<String>,
    /// Flags: `off-raster`, `off-allocation`, `no-allocation-data`, `allocation-only`,
    /// `shape-only`, `low-confidence`.
    pub flags: Vec<String>,
    /// Evidence.
    pub evidence: Vec<ExplanationEvidence>,
}

impl Explanation {
    /// Carries `flag`.
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }
}

/// The raster fit of `service` at `f_center_hz` in `region`, when a raster of that region applies
/// there. `None` region (no single-region band table) applies no raster.
pub fn raster_fit(
    region: Option<Region>,
    service: &str,
    f_center_hz: f64,
) -> Option<ExplanationEvidence> {
    let region = region?;
    let r = CHANNEL_RASTERS.iter().find(|r| {
        r.region == region
            && r.service == service
            && f_center_hz >= r.range_hz[0]
            && f_center_hz <= r.range_hz[1]
    })?;
    let k = ((f_center_hz - r.offset_hz) / r.raster_hz).round();
    let nearest = k * r.raster_hz + r.offset_hz;
    let offset = f_center_hz - nearest;
    Some(ExplanationEvidence::Raster {
        raster_hz: r.raster_hz,
        nearest_channel_hz: nearest,
        offset_hz: offset,
        tolerance_hz: r.tolerance_hz,
        on_raster: offset.abs() <= r.tolerance_hz,
        source: r.source.into(),
        center_source: detected_center(),
    })
}

/// Ranks the explanations of an emitter at `f_center_hz` / `bandwidth_hz` holding `evidence` (see
/// the module docs). At most [`TOP_K`], best first.
pub fn rank_explanations(
    table: &BandTable,
    evidence: &[FamilyEvidence],
    f_center_hz: f64,
    bandwidth_hz: f64,
) -> Vec<Explanation> {
    let mut out = rank_all(table, evidence, f_center_hz, bandwidth_hz, None, None, None);
    out.truncate(TOP_K);
    out
}

/// Evidence written by this module's occupancy mapping: shape, never status.
fn is_shape_evidence(model_version: &str) -> bool {
    model_version == FAMILY_MAP_VERSION
}

#[derive(Default)]
struct Slot {
    /// Probability no evidence supports the service.
    miss: f64,
    /// Probability no non-shape evidence supports it.
    status_miss: f64,
    rows: Vec<ExplanationEvidence>,
    any_evidence: bool,
    shape_only: bool,
}

/// Every candidate, ranked, 1-based ranks, untruncated.
///
/// `duty_cycle` is the emitter's measured duty, when one was measured; `artefact` is the verdict
/// that the row is the receiver's own (T-990). An artefact short-circuits the whole ranking: no
/// service is offered for something that was never on the air.
fn rank_all(
    table: &BandTable,
    evidence: &[FamilyEvidence],
    f_center_hz: f64,
    bandwidth_hz: f64,
    duty_cycle: Option<f64>,
    artefact: Option<&ArtefactVerdict>,
    coincidence: Option<&ExplanationEvidence>,
) -> Vec<Explanation> {
    // T-990 review: the short-circuit is only for a row nothing measured has named. A
    // demodulator lock, a decode or a classifier row is proof the signal was on the air, and the
    // artefact verdict never overrides what was measured (vision step 4) -- there it ranks as one
    // more alternative, disclosed, below the evidence.
    let measured_on_air = evidence
        .iter()
        .any(|e| !is_shape_evidence(&e.model_version) && map_name(&e.family).service.is_some());
    if let Some(a) = artefact {
        if !measured_on_air {
            return vec![artefact_explanation(a)];
        }
    }
    let occupancy = Occupancy {
        bandwidth_hz,
        duty_cycle,
        symbol_rate_hz: None,
    };
    let new_slot = || Slot {
        miss: 1.0,
        status_miss: 1.0,
        shape_only: true,
        ..Slot::default()
    };
    let mut by_service: BTreeMap<&'static str, Slot> = BTreeMap::new();
    for ev in evidence {
        let call = map_name(&ev.family);
        let Some(service) = call.service else {
            continue;
        };
        let c = (call.confidence * ev.confidence).clamp(0.0, 1.0);
        let slot = by_service.entry(service).or_insert_with(new_slot);
        slot.any_evidence = true;
        slot.miss *= 1.0 - c;
        if !is_shape_evidence(&ev.model_version) {
            slot.status_miss *= 1.0 - c;
            slot.shape_only = false;
        }
        slot.rows.push(ExplanationEvidence::Family {
            family: ev.family.clone(),
            model_version: ev.model_version.clone(),
            confidence: ev.confidence,
            mapping_confidence: call.confidence,
        });
    }
    for &service in ALLOCATION_SERVICES {
        if match_known_status(table, service, f_center_hz, bandwidth_hz).status
            == KnownStatus::Known
        {
            by_service.entry(service).or_insert_with(new_slot);
        }
    }
    let no_rows = table
        .overlapping_center(f_center_hz, bandwidth_hz)
        .is_empty();
    let region = table.region();
    let mut out: Vec<Explanation> = by_service
        .into_iter()
        .map(|(service, slot)| {
            let evidence_confidence = 1.0 - slot.miss;
            let status_evidence_confidence = 1.0 - slot.status_miss;
            let mut ev = slot.rows;
            let m = match_known_status(table, service, f_center_hz, bandwidth_hz);
            let mut flags = Vec::new();
            let allocation_only = !(slot.any_evidence && evidence_confidence > 0.0);
            let base = if allocation_only {
                flags.push("allocation-only".to_owned());
                ALLOCATION_ONLY_SCORE
            } else {
                if slot.shape_only {
                    flags.push("shape-only".to_owned());
                }
                if evidence_confidence < MIN_CONFIDENCE {
                    flags.push("low-confidence".to_owned());
                }
                evidence_confidence
            };
            // T-990: what the emitter's own occupancy says about this suggestion. Disclosed for
            // every candidate; it discounts only the ones the band plan alone put forward, since
            // an evidence-backed candidate already rests on a measurement.
            let (support, sh) = support_for(service, &occupancy);
            let measured = measured_support(service, &occupancy, support, sh.as_ref());
            let support_fit = if allocation_only && support == Support::Contradicts {
                flags.push(Support::Contradicts.as_str().to_owned());
                FIT_CONTRADICTED
            } else {
                if allocation_only && support == Support::Unmeasured {
                    flags.push(Support::Unmeasured.as_str().to_owned());
                }
                1.0
            };
            ev.push(measured);
            let alloc = match m.status {
                KnownStatus::Known => 1.0,
                KnownStatus::UnexpectedHere => {
                    flags.push("off-allocation".to_owned());
                    FIT_UNEXPECTED
                }
                _ => {
                    if no_rows {
                        flags.push("no-allocation-data".to_owned());
                    }
                    FIT_NO_DATA
                }
            };
            ev.push(ExplanationEvidence::BandPlan {
                status: m.status,
                prior_ref: m.prior_ref.clone(),
                reason: m.reason,
            });
            let mut raster = 1.0;
            if let Some(fit) = raster_fit(region, service, f_center_hz) {
                if let ExplanationEvidence::Raster {
                    on_raster: false, ..
                } = fit
                {
                    flags.push("off-raster".to_owned());
                    raster = FIT_OFF_RASTER;
                }
                ev.push(fit);
            }
            Explanation {
                rank: 0,
                service: service.to_owned(),
                label: service_label(service).to_owned(),
                score: base * alloc * raster * support_fit,
                evidence_confidence,
                status_evidence_confidence,
                status: m.status,
                prior_ref: m.prior_ref,
                flags,
                evidence: ev,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.service.cmp(&b.service))
    });
    // T-990: the rank-1 slot belongs to a measurement. A band-plan allocation with nothing
    // measured behind it is a suggestion about the band, not an explanation of this emission --
    // it is the same suggestion an empty channel gets -- so when nothing here rests on signal
    // evidence, the honest top explanation is that the emission is unidentified, and the
    // allocations rank below it as what the band plan expects.
    if let Some(a) = artefact {
        // Reached only with measured evidence above (the no-evidence case short-circuited), so
        // this is an alternative and never the answer: above a bare allocation, because a
        // receiver mechanism was actually measured on these detections, and below anything a
        // demodulator, decoder or classifier said.
        let mut alt = artefact_explanation(a);
        alt.score = ARTEFACT_ALTERNATIVE_SCORE;
        alt.rank = 0;
        out.push(alt);
        out.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.service.cmp(&b.service))
        });
    }
    if !out.iter().any(|x| x.evidence_confidence > 0.0) {
        out.insert(
            0,
            unidentified_explanation(&occupancy, &out, no_rows, coincidence),
        );
    }
    for (i, e) in out.iter_mut().enumerate() {
        e.rank = i as u32 + 1;
    }
    out
}

/// The occupancy evidence row of one candidate (T-990).
fn measured_support(
    service: &str,
    occupancy: &Occupancy,
    support: Support,
    shape: Option<&ServiceShape>,
) -> ExplanationEvidence {
    let expected = shape.and_then(|s| s.obw_hz);
    let why = shape.map_or("no shape is recorded for this service", |s| s.why);
    let reason = match support {
        Support::Supports => format!(
            "measured {:.1} kHz, inside the {:.1}-{:.1} kHz a {service} emission occupies ({why})",
            occupancy.bandwidth_hz / 1e3,
            expected.map_or(0.0, |e| e[0]) / 1e3,
            expected.map_or(0.0, |e| e[1]) / 1e3,
        ),
        Support::Contradicts => format!(
            "measured {:.1} kHz at duty {:?}: outside the {:.1}-{:.1} kHz a {service} emission \
             occupies ({why}), so the allocation does not explain this emission",
            occupancy.bandwidth_hz / 1e3,
            occupancy.duty_cycle,
            expected.map_or(0.0, |e| e[0]) / 1e3,
            expected.map_or(0.0, |e| e[1]) / 1e3,
        ),
        Support::NotDiscriminating => format!("{service}: {why}"),
        Support::Unmeasured => format!("no occupied bandwidth measured, so {service} is untested"),
    };
    ExplanationEvidence::Occupancy {
        bandwidth_hz: occupancy.bandwidth_hz,
        duty_cycle: occupancy.duty_cycle,
        expected_obw_hz: expected,
        support,
        reason,
    }
}

/// The rank-1 explanation of a row no measurement identifies (T-990).
fn unidentified_explanation(
    occupancy: &Occupancy,
    below: &[Explanation],
    no_allocation_rows: bool,
    coincidence: Option<&ExplanationEvidence>,
) -> Explanation {
    let considered: Vec<&str> = below.iter().map(|x| x.service.as_str()).collect();
    let reason = if no_allocation_rows {
        "nothing measured about this emission names a service, and the band plan has no \
         allocation row here"
            .to_owned()
    } else if considered.is_empty() {
        "nothing measured about this emission names a service".to_owned()
    } else {
        format!(
            "nothing measured about this emission names a service; the band plan allocates \
             {} here, which is what is expected in this band rather than what was measured",
            considered.join(", ")
        )
    };
    // T-990 review, finding 2: the raster verdict travels with the rank-1 row. The client reads
    // `explanations[0]` for the "off raster" chip and the Channel raster line, and a centre that
    // misses its channel raster is exactly the mismatch the product says to flag rather than snap
    // (CLAUDE.md, vision step 4) -- losing it because an honest "unidentified" moved into rank 1
    // would be this change quietly deleting a finding. The verdict is copied from the
    // best-ranked candidate that has one, which is the row the client used to show.
    let mut flags = vec!["no-measured-support".to_owned()];
    let mut evidence = vec![ExplanationEvidence::Occupancy {
        bandwidth_hz: occupancy.bandwidth_hz,
        duty_cycle: occupancy.duty_cycle,
        expected_obw_hz: None,
        support: Support::Unmeasured,
        reason,
    }];
    if let Some((raster, from)) = below.iter().find_map(|x| {
        x.evidence
            .iter()
            .find(|e| matches!(e, ExplanationEvidence::Raster { .. }))
            .map(|e| (e.clone(), x))
    }) {
        if from.has_flag("off-raster") {
            flags.push("off-raster".to_owned());
        }
        evidence.push(raster);
    }
    // T-990: a frequency coincidence with one of the receiver's own numbers, said out loud on the
    // row a person actually reads, without being claimed as a verdict (see `artefact_verdict`).
    if let Some(ExplanationEvidence::Artefact { source, reason }) = coincidence {
        flags.push(source.clone());
        evidence.push(ExplanationEvidence::Artefact {
            source: source.clone(),
            reason: reason.clone(),
        });
    }
    Explanation {
        rank: 0,
        service: UNIDENTIFIED.to_owned(),
        label: service_label(UNIDENTIFIED).to_owned(),
        // Above ALLOCATION_ONLY_SCORE by construction: an honest "not identified" outranks a
        // suggestion with nothing behind it.
        score: 1.0,
        evidence_confidence: 0.0,
        status_evidence_confidence: 0.0,
        status: KnownStatus::Unknown,
        prior_ref: None,
        flags,
        evidence,
    }
}

/// The one explanation of a receiver artefact (T-990).
fn artefact_explanation(a: &ArtefactVerdict) -> Explanation {
    Explanation {
        rank: 1,
        service: RECEIVER_ARTEFACT.to_owned(),
        label: service_label(RECEIVER_ARTEFACT).to_owned(),
        score: 1.0,
        evidence_confidence: 0.0,
        status_evidence_confidence: 0.0,
        status: KnownStatus::Unknown,
        prior_ref: None,
        flags: vec![RECEIVER_ARTEFACT.to_owned()],
        evidence: vec![ExplanationEvidence::Artefact {
            source: a.source.clone(),
            reason: a.reason.clone(),
        }],
    }
}

/// Why a row is the receiver's own artefact (T-990).
#[derive(Clone, Debug, PartialEq)]
pub struct ArtefactVerdict {
    /// What said so.
    pub source: String,
    /// Why, in words.
    pub reason: String,
}

/// Share of an emitter's newest linked detections a receiver mechanism must explain before the
/// row is called the receiver's own. A majority: "mostly the receiver", the same reading of
/// "mostly" as [`hk_detect::track::inventory::SUSPECT_FRACTION`], applied to a strictly narrower
/// set of flags.
pub const RECEIVER_ARTEFACT_FRACTION: f64 = 0.5;

/// `emitter`'s receiver-artefact verdict, recomputed from what the repository holds. Two ways in,
/// and both require the mechanism to have been **measured against something**:
///
/// 1. A standing `artifact-of` relation (T-219's overlap resolver, with the arithmetic in its
///    reason) — the mechanism was resolved against a named source.
/// 2. A majority of the newest linked detections explained by an **established** mechanism
///    ([`Repository::emitter_receiver_artefact_share`]): an IQ image, an intermodulation product,
///    a hit in a **measured** spur mask, or a retune-settled LO-relative verdict.
///
/// **What is deliberately not a verdict, and why (the T-990 reviews).** A bare *coincidence*
/// between the emission's frequency and one of the receiver's own numbers proves nothing, in two
/// different ways, and neither can be repaired at this layer:
///
/// - A reference harmonic (`n × 10 MHz`), a clock harmonic (`n × fs`) and a comb tooth sit at a
///   **fixed absolute frequency**. 120.000 MHz is a 10 MHz multiple *and* a valid 25 kHz airband
///   channel. Nothing distinguishes the two by position, and no retune can: neither the mark nor
///   a real emission there moves.
/// - DC sits at the **tuned centre**, which does move — but a dwell centres the radio on what it
///   is listening to, so a real emission being demodulated is at the tuned centre too. Measured,
///   not argued: AWARE-042's synthetic 12.5 kHz raster at 446 MHz has every one of its real
///   channels flagged `dc` on 100 % of its detections, because the renderer tunes to each channel
///   in turn. And "the mark followed the LO" cannot be tested per emitter, because the DC rule
///   only marks a detection within 15 kHz of the centre ([`hk_detect::DcRule`]) — a DC line under
///   a larger retune lands at a different frequency and becomes a *different* emitter, so one
///   row's DC detections can never span a retune wide enough to prove anything.
///
/// So a coincidence is **disclosed and never asserted**: [`coincidence_note`] puts it on the
/// rank-1 explanation in words, with the mechanism named and counted, and the band-plan
/// allocations stay ranked below it. Deciding it needs the check made where the detection is
/// admitted — T-948's per-device, per-rate DC + spur mask — and once a row *is* mask-listed its
/// detections carry `spur-map`, which is established evidence and does reach a verdict here.
///
/// **Two further things this is not** (first T-990 review):
///
/// - **It is not "the measurement was untrustworthy".** `clipped` and `compressed` are excluded
///   entirely, so a real station strong enough to drive the ADC into clipping on one high-gain
///   tune is never relabelled the receiver's own.
/// - **It is not durable.** Nothing is written and nothing is tagged; the answer is derived from
///   the current detections every time it is asked, so a line that stops being flagged stops
///   being an artefact — revocable in both directions, like a detected end (ADR-0017).
pub fn artefact_verdict(
    repo: &Repository,
    emitter: EmitterId,
) -> Result<Option<ArtefactVerdict>, RepoError> {
    Ok(artefact_and_coincidence(repo, emitter)?.0)
}

/// [`artefact_verdict`] and, when there is no verdict, the coincidence worth *saying*
/// ([`coincidence_note`]) — from **one** pass over the row's detections.
///
/// One call, because this runs on the control thread for every explained emitter: the relation
/// lookup is an indexed read of a table that is empty for almost every row, and the census is one
/// index-only query capped at [`hk_model::MAX_EVIDENCE_DETECTIONS`] rows with no join and no JSON
/// extract (the tuning centre it used to fetch is not needed — see [`artefact_verdict`]).
pub fn artefact_and_coincidence(
    repo: &Repository,
    emitter: EmitterId,
) -> Result<(Option<ArtefactVerdict>, Option<ExplanationEvidence>), RepoError> {
    let id = repo.live_emitter_id(emitter)?;
    if let Some(r) = repo
        .emitter_relations(id)?
        .into_iter()
        .find(|r| r.active && r.kind == RelationKind::ArtifactOf)
    {
        return Ok((
            Some(ArtefactVerdict {
                source: format!(
                    "artifact-of:{}",
                    r.artifact.map_or("unspecified", ArtifactKind::as_str)
                ),
                reason: r.reason,
            }),
            None,
        ));
    }
    let share = repo.emitter_receiver_artefact_share(id)?;
    Ok(match artefact_from_share(&share) {
        Some(v) => (Some(v), None),
        None => (None, coincidence_note(&share)),
    })
}

/// The verdict a detection-flag census supports, if any. Split out so it can be read (and tested)
/// without a repository.
pub fn artefact_from_share(share: &ReceiverArtefactShare) -> Option<ArtefactVerdict> {
    if share.detections == 0 || share.established_fraction() <= RECEIVER_ARTEFACT_FRACTION {
        return None;
    }
    let mechanism = share
        .established_reason
        .clone()
        .unwrap_or_else(|| "spur".to_owned());
    Some(ArtefactVerdict {
        source: format!("receiver-mechanism:{mechanism}"),
        reason: format!(
            "{} of this row's {} newest detections are the receiver's own: the mechanism \
             ({mechanism}) was measured against something — a mirror, a strong carrier, a \
             measured spur mask or a retune — not inferred from the frequency alone. Clipping \
             and compression are not counted, so this is not merely a measurement it could not \
             be trusted on",
            share.established, share.detections,
        ),
    })
}

/// A frequency coincidence with one of the receiver's own numbers, to be **said** on the rank-1
/// explanation without being claimed as a verdict (see [`artefact_verdict`] for why it cannot be
/// one). `None` unless a majority of the row's detections carry it.
pub fn coincidence_note(share: &ReceiverArtefactShare) -> Option<ExplanationEvidence> {
    if share.detections == 0 || share.coincidence_fraction() <= RECEIVER_ARTEFACT_FRACTION {
        return None;
    }
    let mechanism = share
        .coincidence_reason
        .clone()
        .unwrap_or_else(|| "unspecified spur".to_owned());
    Some(ExplanationEvidence::Artefact {
        source: format!("receiver-coincidence:{mechanism}"),
        reason: format!(
            "{} of this row's {} newest detections coincide with one of the receiver's own \
             numbers ({mechanism}). That is a suspicion, not a verdict: a real emission sits on a \
             10 MHz multiple or at the tuned centre of the dwell listening to it just as readily, \
             and no retune separates the two. Deciding it needs the receiver's own measured DC / \
             spur mask (T-948); until then this row is unidentified, not explained away",
            share.coincidence, share.detections,
        ),
    })
}

/// The status, `prior_ref` and reason `ranked` (best first, untruncated) supports: the verdict of
/// the best candidate with `status_evidence_confidence >= MIN_CONFIDENCE`, else `unknown`.
fn status_from(ranked: &[Explanation]) -> (KnownStatus, Option<String>, String) {
    match ranked
        .iter()
        .find(|x| x.status_evidence_confidence >= MIN_CONFIDENCE)
    {
        Some(best) => {
            let why = best
                .evidence
                .iter()
                .find_map(|ev| match ev {
                    ExplanationEvidence::BandPlan { reason, .. } => Some(reason.as_str()),
                    _ => None,
                })
                .unwrap_or_default();
            (
                best.status,
                best.prior_ref.clone(),
                format!(
                    "best evidence-backed explanation {} (rank {}, score {:.2}, evidence {:.2}): {why}",
                    best.label, best.rank, best.score, best.status_evidence_confidence
                ),
            )
        }
        None => (
            KnownStatus::Unknown,
            None,
            format!(
                "no explanation backed by demodulator, decoder or classifier evidence >= \
                 {MIN_CONFIDENCE:.2} (shape evidence only suggests)"
            ),
        ),
    }
}

/// Flags candidates whose band-plan verdict differs from a status a user or decoder set.
fn flag_status_conflicts(ranked: &mut [Explanation], current: Option<&KnownStatusChange>) {
    let Some(cur) = current else {
        return;
    };
    let flag = match cur.author {
        StatusAuthor::User => "contradicts-user-status",
        StatusAuthor::Decoder => "contradicts-decoder-status",
        _ => return,
    };
    for x in ranked.iter_mut().filter(|x| x.status != cur.status) {
        x.flags.push(flag.to_owned());
    }
}

/// `model_version` prefix of decoder evidence.
/// Shared with hk-model's arbitration rank, which ranks these rows as decoder evidence (T-211).
pub const DECODER_EVIDENCE_PREFIX: &str = hk_model::classify::DECODER_RULES_PREFIX;

/// The Classification recording that decoder `decoder_id` (a built-in decoder id or plugin
/// manifest id) produced a valid decode at `t` with `confidence` (0–1): family = the id (lower
/// case), `model_version` = `decoder:<id>`. `None` when the id maps to no service (see the module
/// table), so unmapped decoders never become family evidence.
pub fn decoder_evidence(decoder_id: &str, confidence: f64, t: Timestamp) -> Option<Classification> {
    let id = decoder_id.trim().to_ascii_lowercase();
    let call = service_family(&Evidence::Decoder(&id));
    call.confident_service()?;
    let confidence = confidence.clamp(0.0, 1.0);
    Some(Classification {
        t,
        family: id.clone(),
        confidence,
        open_set_score: 1.0 - confidence,
        model_version: format!("{DECODER_EVIDENCE_PREFIX}{id}"),
    })
}

/// The **service family** a decoder's CRC-valid decode evidences, at `t`, or `None` when the id
/// maps to no service at or above [`MIN_CONFIDENCE`].
///
/// The same call as [`FamilyCall::classification`] — the service, at the mapping's own confidence
/// — but stamped `decoder:<id>`, so [`hk_model::classify::ArbRank`] ranks it as **decoder
/// evidence** (ADR-0016 §2, rank 1: "a CRC-valid decode is ground truth") rather than at the
/// classifier's rank 3.
///
/// T-961: the rank is the whole point. A decode's family used to be written at rank 3 with
/// `model_version` [`FAMILY_MAP_VERSION`], which ties with the classifier — and "latest among
/// equals" then hands the family to whichever wrote last. On 106.1 MHz that was the classifier's
/// `unknown` at confidence 0.999, standing on an emitter whose RDS PI had been decoded CRC-valid:
/// a decode that confirmed the entry and did not name it. Rank 1 is what makes the decode
/// *supersede* the classifier instead of racing it, and it is why the row is no longer
/// [shape-only evidence](is_shape_evidence) for the status either.
///
/// It differs from [`decoder_evidence`] only in the `family` string: that one records the decoder
/// id itself as the family (what `synth`'s framing decoders have no service name for), this one
/// the service the id maps to, which is what the decoder chains have always written and what the
/// inventory's `family` filter matches on.
pub fn decoder_service_evidence(decoder_id: &str, t: Timestamp) -> Option<Classification> {
    let id = decoder_id.trim().to_ascii_lowercase();
    let call = service_family(&Evidence::Decoder(&id));
    let family = call.confident_service()?;
    Some(Classification {
        t,
        family: family.to_owned(),
        confidence: call.confidence,
        open_set_score: 1.0 - call.confidence,
        model_version: format!("{DECODER_EVIDENCE_PREFIX}{id}"),
    })
}

/// Stores [`decoder_evidence`] on `emitter` (its live id) and returns it; `Ok(None)` stores
/// nothing. **Call contract (decoder chains, T-037b `chains/plugin.rs`):** after a valid decode for
/// `emitter`, call this, then `Inventory::chain_emitter(repo, track, emitter)`, which re-ranks the
/// explanations and may set the status. This writes no content, identity or class.
pub fn record_decoder_evidence(
    repo: &mut Repository,
    emitter: EmitterId,
    decoder_id: &str,
    confidence: f64,
    t: Timestamp,
) -> Result<Option<Classification>, RepoError> {
    let Some(c) = decoder_evidence(decoder_id, confidence, t) else {
        return Ok(None);
    };
    let id = repo.live_emitter_id(emitter)?;
    repo.append_classification(id, &c)?;
    Ok(Some(c))
}

/// A **user** reclassification: appends `classification` to `emitter` at the user arbitration rank
/// and re-ranks at once.
///
/// The rank matters (T-218, from the T-211 review). This used to write through
/// `append_classification`, whose rows carry no stage and derive rank 3 — the classifier's rank.
/// A user's explicit call would then have been overtaken by the next decoder row, the next
/// lock-verified chain label, or even a later feature-tree row: "latest wins among equals". The
/// user is the highest authority on an emitter's family (ADR-0016 §2, rank 0), so the row is
/// written at [`ArbRank::User`] with [`Stage::User`] and nothing outranks it afterwards.
///
/// The label stays in the caller's vocabulary (`wfm`, `adsb`, a service family): a user's call is
/// an assertion, not a measured distribution, so it is stored as the legacy row shape with its
/// stage and rank set. A UI that has a full `hk_model::classify::Classification` to offer (a
/// re-run of the classifier the user accepted, T-207) writes it with
/// `Repository::record_classification` instead, which is what T-205's dataset export reads as a
/// user-labelled training sample.
pub fn reclassify(
    repo: &mut Repository,
    table: &BandTable,
    emitter: EmitterId,
    classification: &Classification,
) -> Result<Explained, RepoError> {
    let id = repo.live_emitter_id(emitter)?;
    repo.append_classification_ranked(id, classification, Stage::User, ArbRank::User)?;
    explain_emitter(repo, table, id)
}

/// Appends a known-status change (typically authored by a user or decoder) and re-ranks at once,
/// so explanations contradicting it are flagged without waiting for the next sighting.
pub fn set_status(
    repo: &mut Repository,
    table: &BandTable,
    change: &KnownStatusChange,
) -> Result<Explained, RepoError> {
    let id = repo.live_emitter_id(change.emitter_id)?;
    repo.append_known_status(&KnownStatusChange {
        emitter_id: id,
        ..change.clone()
    })?;
    explain_emitter(repo, table, id)
}

/// The outcome of [`explain_emitter`].
#[derive(Clone, Debug, PartialEq)]
pub struct Explained {
    /// The live emitter.
    pub emitter_id: EmitterId,
    /// Ranked explanations.
    pub explanations: Vec<Explanation>,
    /// A new explanations annotation was written.
    pub written: Option<AnnotationId>,
    /// The status appended, if any.
    pub status_appended: Option<KnownStatus>,
}

fn latest_explanations(repo: &Repository, id: EmitterId) -> Result<Option<Annotation>, RepoError> {
    Ok(repo
        .annotations_for(&AnnotationTarget::Emitter(id))?
        .into_iter()
        .rfind(|a| a.author == AnnotationAuthor::Classifier && a.author_ref == FAMILY_MAP_VERSION))
}

/// An emitter's latest ranked explanations (empty when none were written).
pub fn explanations(repo: &Repository, emitter: EmitterId) -> Result<Vec<Explanation>, RepoError> {
    let id = repo.live_emitter_id(emitter)?;
    Ok(match latest_explanations(repo, id)? {
        Some(a) => serde_json::from_value(a.metadata["explanations"].clone())?,
        None => Vec::new(),
    })
}

/// Ranks `emitter`'s explanations from its stored families (every classification, else its
/// fingerprint family), stores them when they changed, and sets the status from the best
/// status-backed explanation (see the module docs).
pub fn explain_emitter(
    repo: &mut Repository,
    table: &BandTable,
    emitter: EmitterId,
) -> Result<Explained, RepoError> {
    let id = repo.live_emitter_id(emitter)?;
    let e = repo.emitter(id)?;
    let known = |f: &str| !f.trim().is_empty() && !f.eq_ignore_ascii_case("unknown");
    // Best confidence per (family, classifier): shape and non-shape evidence for one family stay
    // separate pieces.
    let mut best: BTreeMap<(String, String), FamilyEvidence> = BTreeMap::new();
    for c in e.classifications.iter().filter(|c| known(&c.family)) {
        let slot = best
            .entry((c.family.clone(), c.model_version.clone()))
            .or_insert(FamilyEvidence {
                family: c.family.clone(),
                confidence: 0.0,
                model_version: c.model_version.clone(),
            });
        slot.confidence = slot.confidence.max(c.confidence.clamp(0.0, 1.0));
    }
    if best.is_empty() {
        if let Some(f) = e.fingerprint.get("family").and_then(|v| v.as_str()) {
            if known(f) {
                best.insert(
                    (f.to_owned(), "fingerprint".into()),
                    FamilyEvidence {
                        family: f.to_owned(),
                        confidence: 1.0,
                        model_version: "fingerprint".into(),
                    },
                );
            }
        }
    }
    let evidence: Vec<FamilyEvidence> = best.into_values().collect();
    // T-070: a centre refined by output analysis is where the emission is; the detected values
    // stay on the emitter.
    let refined = repo.refined_tuning(id)?;
    let (f_center, bandwidth) = refined
        .as_ref()
        .map_or((e.f_center_hz, e.bandwidth_hz), |r| {
            (r.center_hz, r.bandwidth_hz)
        });
    // T-990: the measured occupancy and the receiver-artefact verdict the ranking has to read.
    // Duty comes from the emitter's own fingerprint when a measurement folded one in.
    let duty = e
        .fingerprint
        .get("duty_cycle")
        .and_then(serde_json::Value::as_f64)
        .filter(|d| d.is_finite() && (0.0..=1.0).contains(d));
    let (artefact, coincidence) = artefact_and_coincidence(repo, id)?;
    let mut all = rank_all(
        table,
        &evidence,
        f_center,
        bandwidth,
        duty,
        artefact.as_ref(),
        coincidence.as_ref(),
    );
    // T-979: the explanation cites the pilot the family was measured from.
    if let Some(pilot) = crate::atsc::pilot_evidence(repo, id)? {
        if let Some(c) = all.iter_mut().find(|c| c.service == PILOT_SERVICE) {
            c.evidence.insert(0, pilot);
        }
    }
    if refined.is_some() {
        for ev in all.iter_mut().flat_map(|x| x.evidence.iter_mut()) {
            if let ExplanationEvidence::Raster { center_source, .. } = ev {
                *center_source = CENTER_REFINED.to_owned();
            }
        }
    }
    let verdict = status_from(&all);
    let cur = repo.known_status_history(id)?.pop();
    let mut ranked = all;
    ranked.truncate(TOP_K);
    flag_status_conflicts(&mut ranked, cur.as_ref());

    let prev = latest_explanations(repo, id)?;
    let value = serde_json::to_value(&ranked)?;
    let changed = match &prev {
        Some(a) => a.metadata.get("explanations") != Some(&value),
        None => !ranked.is_empty(),
    };
    let mut written = None;
    if changed {
        let top = ranked.first();
        let a = Annotation {
            id: AnnotationId::new(),
            target: AnnotationTarget::Emitter(id),
            author: AnnotationAuthor::Classifier,
            author_ref: FAMILY_MAP_VERSION.into(),
            kind: AnnotationKind::Label,
            value: format!(
                "explanations/{}",
                top.map_or("none", |t| t.service.as_str())
            ),
            metadata: serde_json::json!({ "explanations": value, "top_k": TOP_K }),
            content: None,
            confidence: top.map_or(0.0, |t| t.score),
            supersedes: prev.as_ref().map(|a| a.id),
            content_class: ContentClass::MetadataOnly,
            t: e.last_seen,
            exported: false,
        };
        repo.insert_annotation(&a)?;
        written = Some(a.id);
    }

    let mut status_appended = None;
    if !evidence.is_empty() {
        let (status, prior_ref, reason) = verdict;
        let skip = cur.is_some_and(|cur| {
            let overridable = matches!(
                cur.author,
                StatusAuthor::System | StatusAuthor::Clusterer | StatusAuthor::Prior
            );
            let same = cur.status == status && (prior_ref.is_none() || cur.prior_ref == prior_ref);
            !overridable || same
        });
        if !skip {
            repo.append_known_status(&KnownStatusChange {
                emitter_id: id,
                status,
                prior_ref,
                reason,
                t: e.last_seen,
                author: StatusAuthor::Prior,
            })?;
            status_appended = Some(status);
        }
    }
    Ok(Explained {
        emitter_id: id,
        explanations: ranked,
        written,
        status_appended,
    })
}

#[cfg(test)]
mod tests {
    //! Pure ranking and mapping tests over synthetic evidence, plus repository component tests
    //! that address emitters only by the id the write returned (never by a frequency lookup).

    use super::*;
    use hk_context::{Region, is_service_family};
    use hk_demod::AnalogMode;
    use hk_demod::fsk::FSK_FAMILY;
    use hk_model::detection::SpurReason;
    use hk_model::{
        Detection, DetectionFlags, DetectionId, Fingerprint, FreqRange, LinkTarget, PlanRegion,
        ScanPlan, ScanPlanId, ScanPolicy, Schedule, Sighting, SpurMask, SpurMaskId, Survey,
        SurveyId, SurveyState, TimeRange, TimingFeatures, Track, TrackId, TrackState,
    };

    const FM_ROW: &str = "us-47cfr2106-compact:fm-broadcast";
    const AIRBAND_ROW: &str = "us-47cfr2106-compact:aviation-vhf-comm";

    fn table() -> BandTable {
        BandTable::bundled(Region::Us).unwrap()
    }

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    fn ev(family: &str, confidence: f64, model_version: &str) -> FamilyEvidence {
        FamilyEvidence {
            family: family.into(),
            confidence,
            model_version: model_version.into(),
        }
    }

    /// One open survey the test's detections can reference (the schema needs one).
    fn test_survey(repo: &mut Repository) -> SurveyId {
        let plan = ScanPlan {
            id: ScanPlanId::new(),
            version: 1,
            name: "t990".into(),
            created_at: t(0),
            regions: vec![PlanRegion {
                freq: FreqRange::new(1e6, 6e9),
                priority: 1.0,
                revisit_ns: None,
            }],
            policy: ScanPolicy::SweepThenDwell,
            gain_table: vec![],
            schedule: Schedule::Cron {
                expr: "* * * * *".into(),
            },
            extra: serde_json::Value::Null,
        };
        repo.insert_scan_plan(&plan).unwrap();
        let survey = Survey {
            id: SurveyId::new(),
            plan_id: plan.id,
            plan_version: plan.version,
            device_id: "mock:t990".into(),
            state: SurveyState::Open,
            t_start: t(0),
            t_end: None,
            summary: None,
        };
        repo.insert_survey(&survey).unwrap();
        survey.id
    }

    /// A track-backed emitter at `f`/`bw` whose linked detections carry `flags`, so the
    /// repository-derived receiver-artefact verdict (T-990) has real measurements to read.
    /// `family` is stored as a classification unless it is `"unknown"`.
    fn emitter_with_detections(
        repo: &mut Repository,
        f: f64,
        bw: f64,
        family: &str,
        flags: &[DetectionFlags],
    ) -> EmitterId {
        emitter_measured_at(repo, f, bw, family, &[f], flags)
    }

    /// As [`emitter_with_detections`], with the tuning centres the detections were measured
    /// under (cycled): the receiver-artefact verdict reads the LO, so a test that means "the
    /// receiver's own line, seen wherever the radio was pointed" has to say where it was pointed.
    fn emitter_measured_at(
        repo: &mut Repository,
        f: f64,
        bw: f64,
        family: &str,
        centres: &[f64],
        flags: &[DetectionFlags],
    ) -> EmitterId {
        let survey_id = test_survey(repo);
        let track_id = TrackId::new();
        let seen = TimeRange::new(t(0), t(1));
        let id = repo
            .record_sighting(
                &Sighting {
                    source: LinkTarget::Track(track_id),
                    seen,
                    count: flags.len() as u64,
                    f_center_hz: f,
                    bandwidth_hz: bw,
                    fingerprint: Some(Fingerprint::new(f, bw)),
                    identity: None,
                    context: None,
                    classification: (family != "unknown").then(|| Classification {
                        t: t(1),
                        family: family.into(),
                        confidence: 0.9,
                        open_set_score: 0.1,
                        model_version: "test@1".into(),
                    }),
                    tags: Vec::new(),
                },
                None,
            )
            .unwrap()
            .emitter_id;
        let provs: Vec<_> = centres
            .iter()
            .map(|c| {
                repo.intern_provenance(
                    &serde_json::from_value(serde_json::json!({
                        "device_id": "mock:t990",
                        "tune": {"center_hz": c, "sample_rate_hz": 2.4e6, "lna_db": 16.0,
                                 "vga_db": 20.0, "amp_on": false, "bandwidth_hz": 1.8e6},
                        "overload": false, "quantisation_limited": false,
                        "clock_source": "internal", "clock_locked": true,
                        "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
                    }))
                    .unwrap(),
                )
                .unwrap()
            })
            .collect();
        let dets: Vec<Detection> = flags
            .iter()
            .enumerate()
            .map(|(i, fl)| Detection {
                id: DetectionId::new(),
                survey_id,
                time: TimeRange::new(t(i as i64), t(i as i64 + 1)),
                f_center_hz: f,
                obw_hz: bw,
                xdb_bandwidth_hz: Some(bw),
                xdb_level_db: Some(-3.0),
                snr_peak_db: 12.0,
                snr_mean_db: 9.0,
                peak_level_dbfs: -30.0,
                peak_level_dbm: None,
                sk: None,
                clip_count: 0,
                detector_version: "test@1".into(),
                provenance_ref: provs[i % provs.len()],
                flags: *fl,
            })
            .collect();
        repo.insert_detections(&dets).unwrap();
        repo.upsert_track(&Track {
            id: track_id,
            state: TrackState::Closed,
            split_from: None,
            time: seen,
            f_center_hz: f,
            bandwidth_hz: bw,
            detection_count: dets.len() as u64,
            timing: TimingFeatures::default(),
            updated_at: seen.end,
        })
        .unwrap();
        let ids: Vec<DetectionId> = dets.iter().map(|d| d.id).collect();
        repo.link_detections_to_track(track_id, &ids, seen.end)
            .unwrap();
        id
    }

    /// The receiver's own DC/LO-leakage line.
    fn dc_flag() -> DetectionFlags {
        DetectionFlags {
            spur_candidate: true,
            spur_reason: Some(SpurReason::Dc),
            ..DetectionFlags::default()
        }
    }

    /// A measurement that could not be trusted, on a signal that was genuinely on the air.
    fn clipped_flag() -> DetectionFlags {
        DetectionFlags {
            clipped: true,
            compressed: true,
            ..DetectionFlags::default()
        }
    }

    fn sighting(f: f64, bw: f64, family: &str) -> Sighting {
        Sighting {
            source: LinkTarget::Track(TrackId::new()),
            seen: TimeRange::new(t(0), t(1)),
            count: 1,
            f_center_hz: f,
            bandwidth_hz: bw,
            fingerprint: Some(Fingerprint::new(f, bw)),
            identity: None,
            context: None,
            classification: Some(Classification {
                t: t(1),
                family: family.into(),
                confidence: 0.9,
                open_set_score: 0.1,
                model_version: "test@1".into(),
            }),
            tags: Vec::new(),
        }
    }

    #[test]
    fn vocabulary_covers_demod_names_and_maps_into_service_families() {
        for m in AnalogMode::ALL {
            assert!(lookup(m.as_str()).is_some(), "{m:?} not in the vocabulary");
        }
        assert!(lookup(FSK_FAMILY).is_some());
        for e in VOCABULARY {
            assert_eq!(e.label, e.label.to_lowercase());
            assert!(!is_service_family(e.label), "{} shadows a service", e.label);
            match e.service {
                Some(s) => {
                    assert!(is_service_family(s), "{} → {s}", e.label);
                    assert!(e.confidence >= MIN_CONFIDENCE && e.confidence <= 1.0);
                }
                None => assert_eq!(e.confidence, 0.0),
            }
        }
        for (s, canonical) in SERVICE_PASSTHROUGH {
            assert!(is_service_family(s), "{s}");
            // `lora` and `fhss` are evidence-only services: something has to be measured for
            // them to be named, so the band plan never suggests them on its own (T-953).
            assert!(
                ALLOCATION_SERVICES.contains(canonical) || matches!(*canonical, "lora" | "fhss"),
                "{canonical}"
            );
            assert_ne!(service_label(canonical), "Other service");
        }
        let wfm = service_family(&Evidence::Label("wfm"));
        assert_eq!(wfm.confident_service(), Some("fm-broadcast"));
        let adsb = service_family(&Evidence::Decoder("readsb"));
        assert_eq!(adsb.confident_service(), Some("adsb"));
        assert_eq!(service_family(&Evidence::Label("2fsk")).service, None);
        assert_eq!(service_family(&Evidence::Label("nbfm")).service, None);
        assert_eq!(
            service_family(&Evidence::Label("carrier-pigeon")).service,
            None
        );
        let c = wfm.classification(t(3)).unwrap();
        assert_eq!(c.family, "fm-broadcast");
        assert_eq!(c.model_version, FAMILY_MAP_VERSION);
    }

    #[test]
    fn occupancy_maps_only_continuous_wideband_fm_shapes() {
        let occ = |bw: f64, duty: Option<f64>| {
            service_family(&Evidence::Occupancy(Occupancy {
                bandwidth_hz: bw,
                duty_cycle: duty,
                symbol_rate_hz: None,
            }))
        };
        let fm = occ(333e3, Some(1.0));
        assert_eq!(fm.confident_service(), Some("fm-broadcast"));
        assert!((fm.confidence - 0.6).abs() < 1e-9);
        assert_eq!(occ(333e3, Some(0.2)).service, None, "bursty");
        assert_eq!(occ(333e3, None).service, None, "duty unknown");
        assert_eq!(occ(2.0e6, Some(1.0)).service, None, "too wide");
        assert_eq!(occ(14e3, Some(1.0)).service, None, "narrow fragment");
    }

    /// T-953: a ~25 kHz FSK burst at 929.6 MHz gets the paging allocation as a ranked
    /// **explanation** and nothing more. Before T-953 the compact table held no row between
    /// 894 MHz and 960 MHz, so the explorer's FLEX pager emitters came back with `explanations: []`
    /// — not a wrong suggestion, no suggestion at all.
    #[test]
    fn t953_a_pager_burst_at_929_6_mhz_is_explained_by_the_paging_allocation_and_not_identified() {
        // The evidence a blind run actually holds there: a modulation label, which names no
        // service (the module's rule, unchanged).
        let r = rank_explanations(
            &table(),
            &[ev("2fsk", 0.9, "fsk-demod@1")],
            929.6125e6,
            25e3,
        );
        let paging = r
            .iter()
            .find(|e| e.service == "paging")
            .unwrap_or_else(|| panic!("no paging explanation in {r:#?}"));
        assert!(paging.rank <= TOP_K as u32, "rank {}", paging.rank);
        assert_eq!(paging.label, "Paging (929-932 MHz)");
        assert_eq!(paging.status, KnownStatus::Known, "the band expects paging");
        assert_eq!(
            paging.prior_ref.as_deref(),
            Some("us-47cfr2106-compact:paging-929")
        );
        // A suggestion, never an identification: no signal evidence backs it, and nothing that
        // can set a status does.
        assert!(paging.has_flag("allocation-only"), "{:?}", paging.flags);
        assert!(paging.evidence_confidence < 1e-9);
        assert!(paging.status_evidence_confidence < MIN_CONFIDENCE);
        let (status, _, _) = status_from(&r);
        assert_eq!(
            status,
            KnownStatus::Unknown,
            "a band-plan row must never identify an emitter"
        );
    }

    /// T-953: the same allocation, once something *decodes* the service, is what promotes it —
    /// the decoder arbitrates, the band plan only agrees.
    #[test]
    fn t953_a_decoded_pager_is_identified_by_the_decode_and_agreed_with_by_the_band_plan() {
        let r = rank_explanations(
            &table(),
            &[ev("flex", 0.95, "decoder:flex")],
            929.6125e6,
            25e3,
        );
        let top = &r[0];
        assert_eq!(top.service, "paging");
        assert!(!top.has_flag("allocation-only"), "{:?}", top.flags);
        assert!(top.status_evidence_confidence >= MIN_CONFIDENCE);
        assert_eq!(status_from(&r).0, KnownStatus::Known);
    }

    /// T-953: a measured hop set suggests `fhss`, above the band's own allocation-only row, and
    /// still sets no status — the hopping was measured, the system was not identified.
    #[test]
    fn t953_a_hopping_population_in_the_ism_band_is_explained_as_frequency_hopping() {
        let call = service_family(&Evidence::HopSet(HopSet {
            channels: 25,
            hops: 312,
            raster_hz: Some(400e3),
            dwell_s: Some(0.4e-3),
        }));
        assert_eq!(call.confident_service(), Some("fhss"));
        assert!(call.reason.contains("frequency-hopping"), "{}", call.reason);
        let r = rank_explanations(
            &table(),
            &[ev("fhss", HOP_SET_CONFIDENCE, FAMILY_MAP_VERSION)],
            915e6,
            12e6,
        );
        let top = &r[0];
        assert_eq!(top.service, "fhss", "{r:#?}");
        assert_eq!(top.label, "Frequency-hopping system (Part 15 §15.247)");
        assert!(top.has_flag("shape-only"), "{:?}", top.flags);
        assert!(top.evidence_confidence > ALLOCATION_ONLY_SCORE);
        // The Part 15 band itself is still suggested beside it.
        assert!(r.iter().any(|e| e.service == "ism"), "{r:#?}");
        // Shape evidence ranks and suggests; it never sets a status.
        assert_eq!(status_from(&r).0, KnownStatus::Unknown);
    }

    #[test]
    fn prior_maps_demod_families_before_the_band_plan() {
        let p = |family: &str, f: f64| FamilyPrior::new(&table()).verdict(family, f, 230e3);
        assert_eq!(p("wfm", 101.3e6).status, KnownStatus::Known);
        let u = p("wfm", 120.5e6);
        assert_eq!(u.status, KnownStatus::UnexpectedHere);
        assert_eq!(u.prior_ref.as_deref(), Some(AIRBAND_ROW));
        assert_eq!(p(FSK_FAMILY, 915e6).status, KnownStatus::Unknown);
    }

    /// AWARE-053: WFM evidence on the FM raster ranks FM broadcast first, `known`, on raster.
    #[test]
    fn wfm_on_the_fm_raster_ranks_fm_broadcast_first() {
        let r = rank_explanations(&table(), &[ev("wfm", 0.8, "mode@1")], 101.3023e6, 230e3);
        let top = &r[0];
        assert_eq!((top.rank, top.service.as_str()), (1, "fm-broadcast"));
        assert_eq!(top.label, "FM broadcast");
        assert_eq!(top.status, KnownStatus::Known);
        assert_eq!(top.prior_ref.as_deref(), Some(FM_ROW));
        assert!((top.evidence_confidence - 0.72).abs() < 1e-9);
        assert!(top.flags.is_empty(), "{:?}", top.flags);
        assert!(top.evidence.iter().any(|e| matches!(
            e,
            ExplanationEvidence::Raster {
                on_raster: true,
                ..
            }
        )));
    }

    /// A station 47 kHz off its nearest channel (101.453 MHz; 150 kHz above the 101.3 MHz channel
    /// lands 47 kHz below 101.5 MHz) keeps FM broadcast, flagged off-raster and shape-only.
    #[test]
    fn a_station_47_khz_off_its_nearest_channel_is_flagged_off_raster() {
        let r = rank_explanations(
            &table(),
            &[ev("fm-broadcast", 0.6, FAMILY_MAP_VERSION)],
            101.453e6,
            333e3,
        );
        let fm = r.iter().find(|e| e.service == "fm-broadcast").unwrap();
        assert_eq!(fm.rank, 1);
        assert!(
            fm.has_flag("off-raster") && fm.has_flag("shape-only"),
            "{fm:?}"
        );
        let offset = fm
            .evidence
            .iter()
            .find_map(|e| match e {
                ExplanationEvidence::Raster { offset_hz, .. } => Some(*offset_hz),
                _ => None,
            })
            .unwrap();
        assert!((offset + 47e3).abs() < 1.0, "{offset}");
        assert!((fm.score - 0.48).abs() < 1e-9);
    }

    /// AWARE-053: occupancy FM evidence in the airband ranks FM broadcast first (off-allocation),
    /// with the aviation allocation as an allocation-only alternative.
    #[test]
    fn fm_shape_in_the_airband_is_off_allocation_with_aviation_as_alternative() {
        let r = rank_explanations(
            &table(),
            &[ev("fm-broadcast", 0.6, FAMILY_MAP_VERSION)],
            120.503e6,
            333e3,
        );
        assert_eq!(r[0].service, "fm-broadcast");
        assert_eq!(r[0].status, KnownStatus::UnexpectedHere);
        assert_eq!(r[0].prior_ref.as_deref(), Some(AIRBAND_ROW));
        assert!(r[0].has_flag("off-allocation"));
        assert!(
            !r[0].has_flag("off-raster"),
            "no FM raster outside the band"
        );
        let av = r.iter().find(|e| e.service == "aviation-voice").unwrap();
        assert_eq!(av.rank, 2);
        assert!(av.has_flag("allocation-only"));
        assert_eq!(av.status, KnownStatus::Known);
    }

    /// T-054 item 2: shape evidence alone ranks and suggests, never sets a status. A wide
    /// continuous carrier at 162 MHz stays `unknown`, with FM broadcast a `shape-only` suggestion.
    #[test]
    fn shape_only_evidence_ranks_but_never_sets_a_status() {
        let mut repo = Repository::open_in_memory().unwrap();
        let table = table();
        let mut s = sighting(162.0e6, 333e3, "fm-broadcast");
        s.classification.as_mut().unwrap().model_version = FAMILY_MAP_VERSION.into();
        s.classification.as_mut().unwrap().confidence = 0.6;
        let r = repo.record_sighting(&s, None).unwrap();
        let x = explain_emitter(&mut repo, &table, r.emitter_id).unwrap();
        let fm = x
            .explanations
            .iter()
            .find(|e| e.service == "fm-broadcast")
            .unwrap();
        assert!(
            fm.has_flag("shape-only") && fm.has_flag("off-allocation"),
            "{fm:?}"
        );
        assert!(fm.evidence_confidence >= MIN_CONFIDENCE);
        assert_eq!(fm.status_evidence_confidence, 0.0);
        assert_eq!(x.status_appended, None);
        let e = repo.emitter(r.emitter_id).unwrap();
        assert_eq!(e.known_status, KnownStatus::Unknown);
        let last = repo
            .known_status_history(r.emitter_id)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(last.prior_ref, None);

        // A WFM demodulation on the same emitter is status evidence.
        let wfm = Classification {
            t: t(2),
            family: "wfm".into(),
            confidence: 0.8,
            open_set_score: 0.2,
            model_version: "mode-rules@1".into(),
        };
        let x = reclassify(&mut repo, &table, r.emitter_id, &wfm).unwrap();
        assert_eq!(x.status_appended, Some(KnownStatus::UnexpectedHere));
        let fm = &x.explanations[0];
        assert_eq!(fm.service, "fm-broadcast");
        assert!(!fm.has_flag("shape-only"));
        assert!((fm.status_evidence_confidence - 0.72).abs() < 1e-9);
        assert_eq!(
            repo.emitter(r.emitter_id).unwrap().known_status,
            KnownStatus::UnexpectedHere
        );

        // T-218 (T-211 review): a user's call is written at the user arbitration rank, so nothing
        // written later takes the emitter's family away from it — not even a CRC-valid decode,
        // which outranks every other producer (ADR-0016 §2).
        let current = repo.current_classification(r.emitter_id).unwrap().unwrap();
        assert_eq!(current.stage, Stage::User);
        assert_eq!(current.arb_rank, ArbRank::User);
        assert_eq!(current.classification.family, "wfm");
        record_decoder_evidence(&mut repo, r.emitter_id, "readsb", 0.95, t(3)).unwrap();
        let after = repo.current_classification(r.emitter_id).unwrap().unwrap();
        assert_eq!(
            after.classification.family, "wfm",
            "a later decoder row must not overrule the user"
        );
        assert_eq!(after.arb_rank, ArbRank::User);
        // The decoder row is still history, and still the latest.
        let latest = repo.latest_classification(r.emitter_id).unwrap().unwrap();
        assert_eq!(latest.classification.family, "readsb");
        assert_eq!(latest.arb_rank, ArbRank::Decoder);
    }

    /// T-054 item 3: the status comes from the best candidate backed by enough evidence, not from
    /// a weak top candidate. At 120.5 MHz a 0.45 aviation-voice classification (on allocation,
    /// score 0.45) outscores 0.6 WFM evidence (off allocation, 0.6 × 0.7 = 0.42), yet the WFM
    /// candidate decides: `unexpected-here`.
    #[test]
    fn status_comes_from_the_best_candidate_above_min_confidence() {
        let wfm_conf = 0.6 / 0.9;
        let r = rank_all(
            &table(),
            &[
                ev("aviation-voice", 0.45, "clf@1"),
                ev("wfm", wfm_conf, "mode@1"),
            ],
            120.5e6,
            230e3,
            None,
            None,
            None,
        );
        assert_eq!(r[0].service, "aviation-voice");
        assert_eq!(r[0].status, KnownStatus::Known);
        assert!(r[0].has_flag("low-confidence"));
        assert_eq!(r[1].service, "fm-broadcast");
        assert!(r[1].status_evidence_confidence >= MIN_CONFIDENCE);
        let (status, prior_ref, reason) = status_from(&r);
        assert_eq!(status, KnownStatus::UnexpectedHere, "{reason}");
        assert_eq!(prior_ref.as_deref(), Some(AIRBAND_ROW));

        let none = rank_all(
            &table(),
            &[ev("aviation-voice", 0.45, "clf@1")],
            120.5e6,
            230e3,
            None,
            None,
            None,
        );
        assert_eq!(status_from(&none).0, KnownStatus::Unknown);
    }

    /// T-054 item 4: rasters are keyed by region. A band table that is not a US table (here one
    /// with no region) applies no US raster, even to a station 47 kHz off the US FM raster.
    #[test]
    fn a_non_us_table_applies_no_us_raster() {
        let us = rank_explanations(&table(), &[ev("wfm", 0.8, "mode@1")], 101.453e6, 230e3);
        assert!(us[0].has_flag("off-raster"));
        assert_eq!(raster_fit(None, "fm-broadcast", 101.453e6), None);
        assert!(raster_fit(Some(Region::Us), "fm-broadcast", 101.453e6).is_some());
        let other = BandTable::default();
        assert_eq!(other.region(), None);
        let r = rank_explanations(&other, &[ev("wfm", 0.8, "mode@1")], 101.453e6, 230e3);
        let fm = r.iter().find(|e| e.service == "fm-broadcast").unwrap();
        assert!(!fm.has_flag("off-raster"), "{fm:?}");
        assert!(
            !fm.evidence
                .iter()
                .any(|e| matches!(e, ExplanationEvidence::Raster { .. }))
        );
    }

    /// T-054 item 5: a decoder id reaches the mapping through `record_decoder_evidence`; unmapped
    /// ids store nothing.
    #[test]
    fn decoder_evidence_reaches_the_mapping_and_sets_status_after_chain_emitter() {
        use crate::{Inventory, TrackInventory};
        assert_eq!(decoder_evidence("some-unknown-plugin", 1.0, t(1)), None);
        let c = decoder_evidence("ReadSB", 0.95, t(1)).unwrap();
        assert_eq!(
            (c.family.as_str(), c.model_version.as_str()),
            ("readsb", "decoder:readsb")
        );

        let mut repo = Repository::open_in_memory().unwrap();
        let r = repo
            .record_sighting(&sighting(1090e6, 50e3, "unknown"), None)
            .unwrap();
        assert_eq!(
            record_decoder_evidence(&mut repo, r.emitter_id, "nope", 1.0, t(2)).unwrap(),
            None
        );
        let stored = record_decoder_evidence(&mut repo, r.emitter_id, "readsb", 0.95, t(2))
            .unwrap()
            .unwrap();
        assert_eq!(stored.family, "readsb");
        let mut inv = TrackInventory::default();
        inv.chain_emitter(&mut repo, None, r.emitter_id).unwrap();
        let x = explanations(&repo, r.emitter_id).unwrap();
        assert_eq!(x[0].service, "adsb", "{x:?}");
        assert!(x[0].status_evidence_confidence >= MIN_CONFIDENCE);
        let e = repo.emitter(r.emitter_id).unwrap();
        assert_eq!(e.known_status, KnownStatus::Known);
    }

    /// T-054 item 5: a user status change re-ranks at once, flagging contradicting explanations.
    #[test]
    fn a_user_status_change_re_ranks_at_once() {
        let mut repo = Repository::open_in_memory().unwrap();
        let table = table();
        let r = repo
            .record_sighting(&sighting(120.5e6, 230e3, "wfm"), None)
            .unwrap();
        explain_emitter(&mut repo, &table, r.emitter_id).unwrap();
        let x = set_status(
            &mut repo,
            &table,
            &KnownStatusChange {
                emitter_id: r.emitter_id,
                status: KnownStatus::Known,
                prior_ref: None,
                reason: "user: my own transmitter".into(),
                t: t(2),
                author: StatusAuthor::User,
            },
        )
        .unwrap();
        assert!(x.written.is_some(), "re-ranked and stored at once");
        assert_eq!(x.status_appended, None);
        let stored = explanations(&repo, r.emitter_id).unwrap();
        let fm = stored.iter().find(|e| e.service == "fm-broadcast").unwrap();
        assert!(fm.has_flag("contradicts-user-status"), "{fm:?}");
        let av = stored
            .iter()
            .find(|e| e.service == "aviation-voice")
            .unwrap();
        assert!(!av.has_flag("contradicts-user-status"));
        assert_eq!(
            repo.emitter(r.emitter_id).unwrap().known_status,
            KnownStatus::Known
        );
    }

    // ---- T-990: an explanation needs measured evidence consistent with it ----

    /// Every service the band plan may suggest has a shape entry, so no allocation can be offered
    /// with nothing said about whether the measurement backs it.
    #[test]
    fn t990_every_allocation_service_states_its_measurable_shape() {
        for service in ALLOCATION_SERVICES {
            let sh = SERVICE_SHAPES
                .iter()
                .find(|s| s.service == *service)
                .unwrap_or_else(|| panic!("no shape entry for the allocation service {service}"));
            if let Some([lo, hi]) = sh.obw_hz {
                assert!(lo > 0.0 && hi > lo, "{service}: {:?}", sh.obw_hz);
            }
            assert!(
                !sh.why.is_empty(),
                "{service} states no source for its shape"
            );
        }
        for sh in SERVICE_SHAPES {
            assert!(
                ALLOCATION_SERVICES.contains(&sh.service),
                "{} has a shape but is not an allocation service",
                sh.service
            );
        }
    }

    /// The explorer's airband finding (T-990, journal 2026-09-25 window 3), as a unit: an empty
    /// band produced 52 noise rows in 8 minutes and **every one of them** was topped by "Aviation
    /// voice (VHF AM)". The row here is what such a noise candidate looks like — 8 kHz of energy
    /// at 120.5 MHz, no demodulation, no decode, no classifier — and 8 kHz is a width aviation
    /// voice really could have, so the shape check alone would still offer it.
    ///
    /// What must not happen is that the offer *explains* the row. The top explanation is that the
    /// emission is unidentified; the aviation allocation ranks below it as what the band plan
    /// expects here, flagged `allocation-only`, and it sets no status.
    #[test]
    fn t990_noise_in_an_allocated_band_is_unidentified_before_it_is_the_allocation() {
        let mut repo = Repository::open_in_memory().unwrap();
        let r = repo
            .record_sighting(&sighting(120.5e6, 8e3, "unknown"), None)
            .unwrap();
        let x = explain_emitter(&mut repo, &table(), r.emitter_id).unwrap();
        let top = &x.explanations[0];
        assert_eq!(top.service, UNIDENTIFIED, "{:?}", x.explanations);
        assert_eq!(top.rank, 1);
        assert_eq!(top.status, KnownStatus::Unknown);
        let av = x
            .explanations
            .iter()
            .find(|e| e.service == "aviation-voice")
            .unwrap_or_else(|| panic!("{:?}", x.explanations));
        assert!(av.rank > 1, "{av:?}");
        assert!(av.has_flag("allocation-only"), "{av:?}");
        // The width is consistent with the allocation, and the explanation says so — being
        // consistent is not the same as being evidence.
        assert!(
            av.evidence.iter().any(|e| matches!(
                e,
                ExplanationEvidence::Occupancy {
                    support: Support::Supports,
                    ..
                }
            )),
            "{av:?}"
        );
        assert_eq!(x.status_appended, None);
        assert_eq!(
            repo.emitter(r.emitter_id).unwrap().known_status,
            KnownStatus::Unknown
        );
    }

    /// T-990 guard for the blind acceptance suite: inserting an honest rank-1 pushes every
    /// allocation-only suggestion down one place, and `blind::reasonable_services` asks for the
    /// sensible service inside the **top 3**. This pins the two shapes that suite exercises with
    /// no family evidence — an ISM burst at 915 MHz and an NBFM burst at 446 MHz — so a later
    /// widening of the allocation list cannot push them out of the window silently.
    #[test]
    fn t990_the_sensible_allocation_stays_within_the_blind_suite_top_three() {
        let table = table();
        for (f_hz, bw_hz, want) in [(915e6, 40e3, "ism"), (446e6, 12e3, "amateur")] {
            let ranked = rank_explanations(&table, &[], f_hz, bw_hz);
            assert_eq!(ranked[0].service, UNIDENTIFIED, "{ranked:?}");
            let rank = ranked
                .iter()
                .find(|e| e.service == want)
                .unwrap_or_else(|| panic!("{want} missing at {f_hz:e}: {ranked:?}"))
                .rank;
            assert!(
                rank <= 3,
                "{want} fell to rank {rank} at {:.3} MHz, outside the blind suite's top 3: \
                 {ranked:?}",
                f_hz / 1e6,
            );
        }
    }

    /// The other half of the explorer's finding: in 460-470 MHz **no explanation appeared at
    /// all**, because the compact allocation table has no row there and nothing was measured, so
    /// the ranking was empty and no annotation was written. A row now always says something, and
    /// what it says is that it is unidentified and that the band plan has nothing here — which is
    /// a different statement from "the band plan expects aviation voice".
    #[test]
    fn t990_a_band_with_no_allocation_row_still_gets_an_explanation() {
        let table = table();
        let ranked = rank_explanations(&table, &[], 465.0e6, 12e3);
        let top = ranked.first().unwrap_or_else(|| panic!("no explanation"));
        assert_eq!(top.service, UNIDENTIFIED, "{ranked:?}");
        assert!(top.has_flag("no-measured-support"), "{top:?}");
        assert!(
            matches!(
                top.evidence.first(),
                Some(ExplanationEvidence::Occupancy { reason, .. })
                    if reason.contains("no allocation row here")
            ),
            "{top:?}"
        );
    }

    /// An allocation the row's own occupancy contradicts is flagged and discounted: 230 kHz of
    /// occupied bandwidth is not a 25 kHz aviation voice channel, however clearly 118–137 MHz is
    /// allocated to aviation voice.
    #[test]
    fn t990_an_allocation_the_measurement_contradicts_is_flagged_and_discounted() {
        let table = table();
        let wide = rank_explanations(&table, &[], 120.5e6, 230e3);
        let av = wide
            .iter()
            .find(|e| e.service == "aviation-voice")
            .unwrap_or_else(|| panic!("{wide:?}"));
        assert!(av.has_flag(Support::Contradicts.as_str()), "{av:?}");
        let narrow = rank_explanations(&table, &[], 120.5e6, 8e3);
        let ok = narrow
            .iter()
            .find(|e| e.service == "aviation-voice")
            .unwrap_or_else(|| panic!("{narrow:?}"));
        assert!(!ok.has_flag(Support::Contradicts.as_str()), "{ok:?}");
        assert!(
            av.score < ok.score,
            "a contradicted allocation must not score like a consistent one: {av:?} vs {ok:?}"
        );
        // Both are still disclosed, with the arithmetic behind the verdict.
        for e in [av, ok] {
            assert!(
                e.evidence
                    .iter()
                    .any(|x| matches!(x, ExplanationEvidence::Occupancy { .. })),
                "{e:?}"
            );
        }
    }

    /// T-990 review, finding 2. The client reads `explanations[0].flags` for the "off raster"
    /// chip and `explanations[0].evidence` for the Channel raster line. An honest `unidentified`
    /// at rank 1 must therefore carry the raster verdict of the candidate it displaced, or a
    /// station sitting off its channel raster silently loses the flag the product exists to
    /// raise ("Mismatches are interesting… flagged, not snapped").
    #[test]
    fn t990_the_unidentified_row_carries_the_raster_verdict_it_displaced() {
        let table = table();
        // An unmodulated carrier 47 kHz off the 200 kHz FM raster, with no family evidence.
        let ranked = rank_explanations(&table, &[], 98.147e6, 180e3);
        let top = &ranked[0];
        assert_eq!(top.service, UNIDENTIFIED, "{ranked:?}");
        let fm = ranked
            .iter()
            .find(|e| e.service == "fm-broadcast")
            .unwrap_or_else(|| panic!("{ranked:?}"));
        assert!(fm.has_flag("off-raster"), "{fm:?}");
        assert!(
            top.has_flag("off-raster"),
            "the off-raster flag was lost when unidentified took rank 1: {top:?}"
        );
        let raster = top
            .evidence
            .iter()
            .find(|e| matches!(e, ExplanationEvidence::Raster { .. }))
            .unwrap_or_else(|| panic!("no raster evidence on the rank-1 row: {top:?}"));
        assert!(
            matches!(
                raster,
                ExplanationEvidence::Raster {
                    on_raster: false,
                    ..
                }
            ),
            "{raster:?}"
        );

        // On the raster, the flag is absent and the readout still has its numbers.
        let on = rank_explanations(&table, &[], 98.1e6, 180e3);
        assert_eq!(on[0].service, UNIDENTIFIED, "{on:?}");
        assert!(!on[0].has_flag("off-raster"), "{:?}", on[0]);
        assert!(
            on[0].evidence.iter().any(|e| matches!(
                e,
                ExplanationEvidence::Raster {
                    on_raster: true,
                    ..
                }
            )),
            "{:?}",
            on[0]
        );
    }

    /// T-990 / T-948: the DC point in the airband, built from detections **the DC rule could
    /// actually produce** — a 2 kHz mark within `DcRule`'s 15 kHz window of the one tuned centre
    /// (a wider retune moves the DC line to a different frequency, where it is a different
    /// emitter, so a DC row measured under two far-apart tunings is a fixture the pipeline can
    /// never record).
    ///
    /// The ticket's requirement is "**not 'Aviation voice'**", and that is met unconditionally:
    /// the band-plan allocation is never the explanation of a row nothing measured has named. The
    /// DC coincidence itself is **said out loud** — mechanism and count, on the rank-1 row — but
    /// not claimed as a verdict, because at this layer it cannot be told from a real emission the
    /// radio is dwelled on (see [`artefact_verdict`]).
    #[test]
    fn t990_a_dc_spike_in_the_airband_is_never_explained_as_aviation_voice() {
        let mut repo = Repository::open_in_memory().unwrap();
        let dc = emitter_measured_at(
            &mut repo,
            120.5e6,
            2e3,
            "unknown",
            &[120.5e6],
            &[dc_flag(); 6],
        );
        let x = explain_emitter(&mut repo, &table(), dc).unwrap();
        let top = &x.explanations[0];
        assert_eq!(top.service, UNIDENTIFIED, "{:?}", x.explanations);
        assert_ne!(top.service, "aviation-voice");
        assert!(
            top.has_flag("receiver-coincidence:dc"),
            "the DC mechanism was not named on the row a person reads: {top:?}"
        );
        assert!(
            matches!(
                top.evidence.iter().find(|e| matches!(e, ExplanationEvidence::Artefact { .. })),
                Some(ExplanationEvidence::Artefact { reason, .. }) if reason.contains("6 of this row's 6")
            ),
            "{top:?}"
        );
        // The allocation is still offered, ranked below, and still sets no status.
        assert!(
            x.explanations
                .iter()
                .any(|e| e.service == "aviation-voice" && e.rank > 1),
            "{:?}",
            x.explanations
        );
        assert_eq!(x.status_appended, None);
    }

    /// T-990 / T-948: the same DC point once the receiver's **measured** spur mask lists it. The
    /// ticket's clause is "DC/spur-**masked** rows get the artefact explanation instead (T-948)":
    /// a mask entry is a measurement of the receiver (a terminated-input capture), not a
    /// coincidence, so it does reach a verdict — one explanation, and no service offered.
    #[test]
    fn t990_a_spur_masked_row_is_explained_as_a_receiver_artefact() {
        let mut repo = Repository::open_in_memory().unwrap();
        // The mask is a measurement of the receiver: a terminated-input capture, stored.
        let mask = SpurMask {
            id: SpurMaskId::new(),
            supersedes: None,
            device_id: "mock:t990".into(),
            measured_at: t(0),
            rules: Vec::new(),
        };
        repo.insert_spur_mask(&mask).unwrap();
        let masked = DetectionFlags {
            spur_candidate: true,
            spur_reason: Some(SpurReason::SpurMap { mask: mask.id }),
            ..DetectionFlags::default()
        };
        let id = emitter_measured_at(&mut repo, 120.5e6, 2e3, "unknown", &[120.5e6], &[masked; 4]);
        let x = explain_emitter(&mut repo, &table(), id).unwrap();
        assert_eq!(x.explanations.len(), 1, "{:?}", x.explanations);
        assert_eq!(x.explanations[0].service, RECEIVER_ARTEFACT);
        assert_eq!(x.explanations[0].label, "Receiver artefact");
        assert!(
            !x.explanations.iter().any(|e| e.service == "aviation-voice"),
            "{:?}",
            x.explanations
        );
        assert_eq!(x.status_appended, None);
    }

    /// T-990 second review, finding 2. A **fixed absolute frequency** that happens to be one of
    /// the receiver's own numbers proves nothing: 120.000 MHz is a 10 MHz multiple *and* a valid
    /// 25 kHz airband channel, and no retune separates the two because neither moves. A real
    /// narrowband emission there, flagged `ref-harmonic` on every detection under two dwells
    /// 400 kHz apart, must keep its allocation — the previous rule made it "Receiver artefact"
    /// and dropped the aviation suggestion entirely.
    #[test]
    fn t990_a_real_emission_on_a_10_mhz_multiple_is_not_the_receivers_own() {
        let mut repo = Repository::open_in_memory().unwrap();
        let refh = DetectionFlags {
            spur_candidate: true,
            spur_reason: Some(SpurReason::RefHarmonic),
            ..DetectionFlags::default()
        };
        let id = emitter_measured_at(
            &mut repo,
            120.0e6,
            8e3,
            "unknown",
            &[119.8e6, 120.2e6],
            &[refh; 8],
        );
        assert_eq!(artefact_verdict(&repo, id).unwrap(), None);
        let x = explain_emitter(&mut repo, &table(), id).unwrap();
        assert!(
            !x.explanations
                .iter()
                .any(|e| e.service == RECEIVER_ARTEFACT),
            "a real 25 kHz airband channel on a 10 MHz multiple was called the receiver's own: \
             {:?}",
            x.explanations
        );
        assert!(
            x.explanations.iter().any(|e| e.service == "aviation-voice"),
            "the aviation allocation was dropped: {:?}",
            x.explanations
        );
        // The suspicion is still disclosed, by name, on the row a person reads.
        assert!(
            x.explanations[0].has_flag("receiver-coincidence:ref-harmonic"),
            "{:?}",
            x.explanations[0]
        );
    }

    /// T-990, from the AWARE-042 red the first review's finding 4 uncovered. A dwell centres the
    /// radio on what it is listening to, so a real channel being demodulated is flagged `dc` on
    /// every detection. In AWARE-042's synthetic 12.5 kHz raster at 446 MHz every real channel
    /// was being explained "Receiver artefact"; the allocation must survive.
    #[test]
    fn t990_a_dwelled_on_real_channel_is_not_a_receiver_artefact() {
        let mut repo = Repository::open_in_memory().unwrap();
        let real = emitter_measured_at(
            &mut repo,
            446.06875e6,
            8e3,
            "unknown",
            &[446.06875e6],
            &[dc_flag(); 8],
        );
        assert_eq!(artefact_verdict(&repo, real).unwrap(), None);
        let x = explain_emitter(&mut repo, &table(), real).unwrap();
        assert!(
            !x.explanations
                .iter()
                .any(|e| e.service == RECEIVER_ARTEFACT),
            "{:?}",
            x.explanations
        );
        assert!(
            x.explanations.iter().any(|e| e.service == "amateur"),
            "the 70 cm allocation is still offered: {:?}",
            x.explanations
        );
    }

    /// A mechanism that was **measured against something** — here an IQ image, a mirror measured
    /// stronger with a correlated shape — is a verdict: it is a measurement about the receive
    /// chain, not a coincidence of numbers.
    #[test]
    fn t990_a_measured_mechanism_is_a_verdict() {
        let mut repo = Repository::open_in_memory().unwrap();
        let image = DetectionFlags {
            image_candidate: true,
            ..DetectionFlags::default()
        };
        let id = emitter_measured_at(&mut repo, 120.5e6, 8e3, "unknown", &[120.9e6], &[image; 4]);
        let v = artefact_verdict(&repo, id).unwrap().expect("a verdict");
        assert_eq!(v.source, "receiver-mechanism:image");
        assert_eq!(
            explain_emitter(&mut repo, &table(), id)
                .unwrap()
                .explanations[0]
                .service,
            RECEIVER_ARTEFACT
        );
    }

    /// T-990 review, finding 1a. A strong real station that drove the ADC into clipping on one
    /// high-gain tune is **still a real station**. `clipped` and `compressed` say the measurement
    /// could not be trusted, not that the receiver invented the signal, so they never reach the
    /// artefact verdict — only a spur, image or intermod rule does.
    #[test]
    fn t990_clipping_and_compression_alone_never_make_a_row_a_receiver_artefact() {
        let mut repo = Repository::open_in_memory().unwrap();
        let id = emitter_with_detections(&mut repo, 120.5e6, 8e3, "unknown", &[clipped_flag(); 4]);
        assert_eq!(artefact_verdict(&repo, id).unwrap(), None);
        let x = explain_emitter(&mut repo, &table(), id).unwrap();
        assert!(
            !x.explanations
                .iter()
                .any(|e| e.service == RECEIVER_ARTEFACT),
            "an all-clipped real station was relabelled the receiver's own: {:?}",
            x.explanations
        );
        assert_eq!(x.explanations[0].service, UNIDENTIFIED, "{x:?}");
    }

    /// T-990 review, finding 1b. A demodulator lock, a decode or a classifier row is proof the
    /// signal **was on the air**, and the artefact verdict never overrides what was measured
    /// (vision step 4: the database never overrides the measurement, and neither does a receiver
    /// heuristic). The verdict is still disclosed — it ranks as one more alternative, above a
    /// bare allocation and below the evidence — and the status the evidence earned stands.
    #[test]
    fn t990_an_artefact_verdict_never_overrides_measured_evidence() {
        let mut repo = Repository::open_in_memory().unwrap();
        // A real broadcast station that also clipped, and that a spur rule flagged on most of its
        // detections: the worst case the review names.
        let image = DetectionFlags {
            image_candidate: true,
            ..DetectionFlags::default()
        };
        let id = emitter_measured_at(
            &mut repo,
            98.5e6,
            180e3,
            "wfm",
            &[101.0e6],
            &[image, image, image, DetectionFlags::default()],
        );
        assert!(artefact_verdict(&repo, id).unwrap().is_some());
        let x = explain_emitter(&mut repo, &table(), id).unwrap();
        assert_eq!(x.explanations[0].service, "fm-broadcast", "{x:?}");
        assert_eq!(x.status_appended, Some(KnownStatus::Known), "{x:?}");
        assert_eq!(
            repo.emitter(id).unwrap().known_status,
            KnownStatus::Known,
            "the artefact verdict reset a status the measurement earned"
        );
        let alt = x
            .explanations
            .iter()
            .find(|e| e.service == RECEIVER_ARTEFACT)
            .unwrap_or_else(|| panic!("the verdict was hidden rather than ranked: {x:?}"));
        assert!(alt.rank > 1, "{alt:?}");
        assert_eq!(alt.score, ARTEFACT_ALTERNATIVE_SCORE);
    }

    /// T-990 review, finding 1c. The verdict is derived from the current detections, never
    /// written down, so it is revocable: a row that stops being flagged stops being an artefact
    /// on the next explain. Nothing has to un-tag it, because nothing tagged it.
    #[test]
    fn t990_the_artefact_verdict_is_revocable() {
        let mut repo = Repository::open_in_memory().unwrap();
        let image = DetectionFlags {
            image_candidate: true,
            ..DetectionFlags::default()
        };
        let id = emitter_measured_at(&mut repo, 120.5e6, 8e3, "unknown", &[120.9e6], &[image; 4]);
        assert_eq!(
            explain_emitter(&mut repo, &table(), id)
                .unwrap()
                .explanations[0]
                .service,
            RECEIVER_ARTEFACT
        );
        // The receiver retunes; the same emission is measured off the tuned centre, clean.
        let same = emitter_with_detections(
            &mut repo,
            120.5e6,
            8e3,
            "unknown",
            &[DetectionFlags::default(); 6],
        );
        assert_eq!(same, id, "the second sighting resolved to another emitter");
        assert_eq!(artefact_verdict(&repo, id).unwrap(), None);
        let after = explain_emitter(&mut repo, &table(), id).unwrap();
        assert_eq!(after.explanations[0].service, UNIDENTIFIED, "{after:?}");
        assert!(
            after
                .explanations
                .iter()
                .any(|e| e.service == "aviation-voice"),
            "{after:?}"
        );
    }

    /// Decision (module docs): unmapped FSK in an ISM band keeps `unknown`, with ISM / Part 15 as
    /// an allocation-only suggestion; a protocol decode mapped to `ism` is `known` via Part 15.
    ///
    /// **T-990 moved the suggestion off rank 1.** It used to *be* rank 1, which is the shape the
    /// explorer met in an empty airband: 52 noise rows in 8 minutes, every one of them topped by
    /// "Aviation voice (VHF AM)" with nothing measured behind it. The ISM allocation here is
    /// still offered, still flagged `allocation-only`, and still sets no status -- it now ranks
    /// under an honest `unidentified`, because a 40 kHz FSK burst at 915 MHz has not been
    /// identified as a Part 15 device; the band is merely allocated to them.
    #[test]
    fn unmapped_fsk_in_ism_keeps_unknown_with_an_ism_suggestion() {
        let mut repo = Repository::open_in_memory().unwrap();
        let table = table();
        let r = repo
            .record_sighting(&sighting(915e6, 40e3, FSK_FAMILY), None)
            .unwrap();
        let x = explain_emitter(&mut repo, &table, r.emitter_id).unwrap();
        assert_eq!(x.status_appended, None);
        assert_eq!(
            repo.emitter(r.emitter_id).unwrap().known_status,
            KnownStatus::Unknown
        );
        let top = &x.explanations[0];
        assert_eq!(top.service, UNIDENTIFIED, "{top:?}");
        assert!(top.has_flag("no-measured-support"), "{top:?}");
        let ism = x
            .explanations
            .iter()
            .find(|e| e.service == "ism")
            .unwrap_or_else(|| panic!("{:?}", x.explanations));
        assert!(ism.has_flag("allocation-only"), "{ism:?}");
        // Part 15 has no bandwidth signature, so a width can neither back it nor rule it out.
        assert!(
            !ism.has_flag(Support::Contradicts.as_str())
                && ism.evidence.iter().any(|e| matches!(
                    e,
                    ExplanationEvidence::Occupancy {
                        support: Support::NotDiscriminating,
                        ..
                    }
                )),
            "{ism:?}"
        );
        assert_eq!(explanations(&repo, r.emitter_id).unwrap(), x.explanations);

        let rtl = rank_explanations(&table, &[ev("rtl_433", 1.0, "plugin")], 433.92e6, 40e3);
        assert_eq!(rtl[0].service, "ism");
        assert_eq!(rtl[0].status, KnownStatus::Known);
        assert_eq!(
            rtl[0].prior_ref.as_deref(),
            Some("us-47cfr2106-compact:ism-433-part15")
        );
    }

    #[test]
    fn status_follows_the_top_explanation_once_and_never_overrides_a_user() {
        let mut repo = Repository::open_in_memory().unwrap();
        let table = table();
        let r = repo
            .record_sighting(&sighting(120.5e6, 230e3, "wfm"), None)
            .unwrap();
        let x = explain_emitter(&mut repo, &table, r.emitter_id).unwrap();
        assert_eq!(x.status_appended, Some(KnownStatus::UnexpectedHere));
        assert!(x.written.is_some());
        let again = explain_emitter(&mut repo, &table, r.emitter_id).unwrap();
        assert_eq!((again.status_appended, again.written), (None, None));
        let h = repo.known_status_history(r.emitter_id).unwrap();
        assert_eq!(h.len(), 2, "{h:?}");
        assert_eq!(h[1].author, StatusAuthor::Prior);
        assert_eq!(h[1].prior_ref.as_deref(), Some(AIRBAND_ROW));

        repo.append_known_status(&KnownStatusChange {
            emitter_id: r.emitter_id,
            status: KnownStatus::Known,
            prior_ref: None,
            reason: "user: my own transmitter".into(),
            t: t(2),
            author: StatusAuthor::User,
        })
        .unwrap();
        let x = explain_emitter(&mut repo, &table, r.emitter_id).unwrap();
        assert_eq!(x.status_appended, None);
        assert_eq!(
            repo.emitter(r.emitter_id).unwrap().known_status,
            KnownStatus::Known
        );
    }
}
