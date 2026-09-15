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
//! | continuous, OBW 150–400 kHz | occupancy | `fm-broadcast` | 0.6 | [`WIDEBAND_FM_OBW_HZ`] |
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
//!   are suggestions ("what is supposed to be here") and never set a status.
//! - **Centre.** Allocation and raster checks use the emitter's centre and bandwidth **refined by
//!   output analysis** when a chain stored one (T-070, [`crate::refine`]; raster evidence then
//!   says `center_source: refined`), else the detected ones. Nothing is snapped to a raster: an
//!   off-raster refined centre is flagged `off-raster`.
//! - **Score.**
//!   `score = base × allocation fit × raster fit`. The base is the evidence confidence, or
//!   [`ALLOCATION_ONLY_SCORE`]. Allocation fit is 1 for `known`, 0.8 with no allocation data and
//!   0.7 for `unexpected-here`. Raster fit is 0.8 for an off-raster centre and 1 otherwise. Rasters
//!   ([`CHANNEL_RASTERS`]) are keyed by region and apply only when the band table is wholly that
//!   region ([`BandTable::region`]). Candidates are ranked by score, and the top [`TOP_K`] are
//!   kept.
//! - **Evidence and flags.** Each candidate lists its evidence: the families, the band-plan
//!   verdict with its `prior_ref`, and the raster fit. Its flags are `off-raster`,
//!   `off-allocation`, `no-allocation-data`, `allocation-only`, `shape-only` (occupancy evidence
//!   only), `low-confidence`, and `contradicts-user-status` / `contradicts-decoder-status` (its
//!   band-plan verdict differs from a status a user or decoder set).
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
use hk_detect::TrackSummary;
use hk_detect::track::inventory::SUSPECT_FRACTION;
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

/// Occupied bandwidth of a continuous emission read as broadcast FM, Hz. US FM channels are
/// 200 kHz. Carson's rule for ±75 kHz deviation with a 57 kHz RDS top gives about 264 kHz, and
/// the detector's OBW99 at high SNR is wider still: 333 kHz on the 101.3 MHz fixture station
/// (unverified beyond that fixture). A continuous emission this wide carries only 0.6 confidence:
/// it is shape evidence, not a demodulation.
pub const WIDEBAND_FM_OBW_HZ: [f64; 2] = [150e3, 400e3];

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
pub const CHANNEL_RASTERS: &[ChannelRaster] = &[ChannelRaster {
    region: Region::Us,
    service: "fm-broadcast",
    range_hz: [87.8e6, 108.0e6],
    raster_hz: 200e3,
    offset_hz: 100e3,
    tolerance_hz: 20e3,
    source: "47 CFR 73.201 (US FM channels, 200 kHz on odd tenths)",
}];

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
const ADSB_NOTE: &str = "CRC-checked Mode S / ADS-B frames";
const ISM_NOTE: &str = "a Part 15 sensor protocol decoded";

/// The vocabulary (see the module table).
pub const VOCABULARY: &[VocabEntry] = &[
    entry(
        "wfm",
        DemodMode,
        Some("fm-broadcast"),
        0.9,
        "200 kHz wideband FM is the broadcast service (47 CFR 73 subpart B)",
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
];

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

/// Evidence about an emitter's family.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Evidence<'a> {
    /// A demodulator mode or modulation label (or a service family name).
    Label(&'a str),
    /// A decoder id.
    Decoder(&'a str),
    /// Occupancy.
    Occupancy(Occupancy),
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
    let mut out = rank_all(table, evidence, f_center_hz, bandwidth_hz);
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
fn rank_all(
    table: &BandTable,
    evidence: &[FamilyEvidence],
    f_center_hz: f64,
    bandwidth_hz: f64,
) -> Vec<Explanation> {
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
            let base = if slot.any_evidence && evidence_confidence > 0.0 {
                if slot.shape_only {
                    flags.push("shape-only".to_owned());
                }
                if evidence_confidence < MIN_CONFIDENCE {
                    flags.push("low-confidence".to_owned());
                }
                evidence_confidence
            } else {
                flags.push("allocation-only".to_owned());
                ALLOCATION_ONLY_SCORE
            };
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
                score: base * alloc * raster,
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
    for (i, e) in out.iter_mut().enumerate() {
        e.rank = i as u32 + 1;
    }
    out
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

/// A user (or classifier) reclassification: appends `classification` to `emitter` and re-ranks at
/// once.
pub fn reclassify(
    repo: &mut Repository,
    table: &BandTable,
    emitter: EmitterId,
    classification: &Classification,
) -> Result<Explained, RepoError> {
    let id = repo.live_emitter_id(emitter)?;
    repo.append_classification(id, classification)?;
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
    let mut all = rank_all(table, &evidence, f_center, bandwidth);
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
    use hk_model::{Fingerprint, LinkTarget, Sighting, TimeRange, TrackId};

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
            assert!(ALLOCATION_SERVICES.contains(canonical) || *canonical == "lora");
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

    /// Decision (module docs): unmapped FSK in an ISM band keeps `unknown`, with ISM / Part 15 as
    /// an allocation-only suggestion; a protocol decode mapped to `ism` is `known` via Part 15.
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
        assert_eq!(top.service, "ism");
        assert!(top.has_flag("allocation-only"), "{top:?}");
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
