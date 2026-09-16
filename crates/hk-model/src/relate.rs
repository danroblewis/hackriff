//! Signal relationships (C40, T-219): when one inventory row is not an independent emission but
//! **related to another** — the same emission seen twice, or a receiver artifact of it.
//!
//! Three problems, one mechanism — competition between hypotheses over a band (ADR-0015 §11.4),
//! differing only in the kind of evidence that binds them:
//!
//! 1. **A Confirmed signal suppresses overlapping candidates.** A new candidate whose band
//!    overlaps a Confirmed entry's, with no distinguishing evidence, is almost surely the same
//!    emission: it is recorded as [`RelationKind::SuppressedBy`] that entry, with a reason.
//! 2. **Overlapping candidates compete.** Candidates that overlap each other form a duplicate
//!    group ranked by the provisional [`rank_score`] proxy (SNR × duty × trust — explicitly *not*
//!    the evidence-bits ladder, which is MAUTO work, ADR-0015 §11). The strongest is shown; the
//!    others are recorded [`RelationKind::DuplicateOf`] it.
//! 3. **Receiver artifacts are same-source duplicates.** A detection can be an artifact of another
//!    signal at a *deterministic, predictable* frequency: an **image** at `2·f_LO − f`, a
//!    **harmonic** at `n·f`, or **intermodulation** at `a·f1 ± b·f2`. [`predict_artifacts`] is the
//!    geometric test — centre, the width the mechanism implies, and relative level — and needs no
//!    decode. Arithmetic alone never attributes a row: the caller also requires presence only while
//!    the source was on air, and a matching suspect flag on the measurement itself
//!    ([`RowEvidence::corroborates`]). The image arithmetic uses only the LO the row being
//!    explained was measured under. It is the same "artifact of the receiver, not the air" idea as
//!    the DC twin rule (ADR-0012 §2.6).
//!
//! **Device-local physics reads the receive chain; shared-air reasoning must not** (T-302). An
//! image, harmonic or intermodulation product is manufactured by one front end's mixer, LO and
//! non-linearity, so a source measured on *another* receive chain can never explain it — a signal
//! arriving at device B's antenna was never in device A's mixer. Every artifact pairing below is
//! therefore gated on [`ReceiveChain`], and [`TunedLo`] exists so that an LO cannot be read apart
//! from the chain that tuned it. Dedup, competition and identity are the opposite case: two front
//! ends at a stitched seam seeing one real emitter *should* collapse to one row, so nothing in
//! [`bands_compete`], [`distinguishing_evidence`] or [`rank_score`] reads the device.
//!
//! **Nothing here ever deletes or mutates a row.** A relationship is an append-only claim keyed by
//! emitter, carrying its reasoning, and revocable when evidence changes (`active = 0`). The losing
//! candidate keeps its id, detections, tracks, links and history, so later evidence can revive it.
//! That is the exploration-first rule: a relationship is ranked evidence, never truth, and never an
//! automatic delete.
//!
//! **The guard is what makes this safe.** Band overlap alone only makes two rows *compete*, and
//! only when they overlap by [`OVERLAP_MIN_FRACTION`] of **both** the narrower and the wider band
//! ([`bands_compete`]) — so a narrow emission inside a wide one, a subcarrier or a data burst in a
//! broadcast skirt, never disappears into its host. Any distinguishing evidence then blocks a merge
//! or suppression ([`distinguishing_evidence`]), in order: two different decoded identities, then
//! measured bandwidths further apart than [`Tolerances`] allows, then a fingerprint distance beyond
//! [`Tolerances`], then separated −3 dB extents. Two genuinely distinct adjacent stations therefore
//! stay two entries. The bandwidth test reads the **measurement**, never only the fingerprint: a
//! fingerprint-gated guard silently stops protecting anything the feature-set version moves.

use serde::{Deserialize, Serialize};

use crate::cluster::{Fingerprint, Tolerances};
use crate::emitter::DecodedIdentity;
use crate::ids::EmitterId;
use crate::region::FreqRange;
use crate::time::Timestamp;

// ---------------------------------------------------------------------------------------------
// Thresholds. All a priori: fixed here, never fitted to a fixture.
// ---------------------------------------------------------------------------------------------

/// Smallest share of the **narrower** band that must overlap before two rows compete at all
/// (ADR-0015 §11.3 uses the same 0.6 for same-hypothesis channel overlap). Below this the two
/// bands are different channels and nothing is claimed.
pub const OVERLAP_MIN_FRACTION: f64 = 0.6;

/// Reference SNR for [`rank_score`]: a row at this SNR scores 1.0 on the SNR term.
pub const RANK_SNR_REF_DB: f64 = 10.0;

/// SNR is clamped to this before ranking, so one implausible measurement cannot dominate.
pub const RANK_SNR_MAX_DB: f64 = 60.0;

/// Duty cycle is clamped to at least this, so a near-zero duty cannot zero the product.
pub const RANK_DUTY_FLOOR: f64 = 0.01;

/// Duty cycle used when none was measured. Neutral: an unmeasured duty is not evidence against a
/// row — it is the SNR and trust terms that separate duplicate boxes of one station.
pub const RANK_DUTY_UNKNOWN: f64 = 1.0;

/// Trust multiplier per suspect share: `1 − RANK_SUSPECT_WEIGHT × suspect_fraction`, floored at
/// [`RANK_TRUST_FLOOR`]. A box built mostly from spur/IMD/image/clipped detections ranks below a
/// clean one of the same strength.
pub const RANK_SUSPECT_WEIGHT: f64 = 0.75;

/// Smallest trust multiplier, so an all-suspect row still ranks (it is shown if nothing beats it).
pub const RANK_TRUST_FLOOR: f64 = 0.1;

/// Smallest centre tolerance for a geometric artifact prediction, Hz.
pub const ARTIFACT_CENTER_MIN_HZ: f64 = 5_000.0;

/// Centre tolerance as a fraction of the **measured** bandwidth of the row being explained — never
/// of the predicted one. A harmonic's predicted width is `n·bw` and an intermod's `a·bw1 + b·bw2`,
/// so a predicted-width tolerance widens the acceptance window with the order until nearly every
/// frequency fits some mechanism: on a 32-source model of the 2026-09-15 FM capture that claimed
/// 2.396 MHz of the 2.400 MHz band, with 68 mechanisms firing on one 100.3 MHz box.
pub const ARTIFACT_CENTER_BW_FRACTION: f64 = 0.25;

/// Largest ratio allowed between the measured bandwidth and the width the mechanism predicts (an
/// image preserves the source's width, an `n`th harmonic scales it by `n`, an intermod spreads to
/// `a·bw1 + b·bw2`). Generous in the "measured narrower" direction, because a weak artifact's
/// skirts sink below the detection threshold and read narrow — but an 8.5 kHz box is not the image
/// of a 180 kHz station, whatever its centre says.
pub const ARTIFACT_BANDWIDTH_RATIO: f64 = 3.0;

/// An artifact must be at least this far **below** its source, in dB. An "artifact" as strong as
/// its source is not a receiver artifact; it is another emission.
pub const ARTIFACT_MIN_SUPPRESSION_DB: f64 = 10.0;

/// Past this suppression the coincidence is not evidence of anything (the row is near the floor
/// and the arithmetic would fit by chance), so no claim is made.
pub const ARTIFACT_MAX_SUPPRESSION_DB: f64 = 80.0;

/// Highest harmonic order predicted (`n·f`, n from 2).
pub const HARMONIC_MAX_ORDER: u32 = 5;

/// Highest intermodulation order predicted (`a + b`, with `a, b ≥ 1`).
pub const INTERMOD_MAX_ORDER: u32 = 5;

/// Smallest SNR for a confirmed emitter to be offered as an artifact **source**. A weak emitter
/// does not drive a visible image, harmonic or intermod product.
pub const ARTIFACT_SOURCE_MIN_SNR_DB: f64 = 20.0;

/// Slack when testing "present only while the source is present", ns.
pub const ARTIFACT_PRESENCE_SLACK_NS: i64 = 1_000_000_000;

// ---------------------------------------------------------------------------------------------
// The relationship row
// ---------------------------------------------------------------------------------------------

/// How one inventory row defers to another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RelationKind {
    /// A candidate overlapping a Confirmed entry's band, with no distinguishing evidence.
    SuppressedBy,
    /// The weaker of two overlapping candidates, ranked by [`rank_score`].
    DuplicateOf,
    /// A candidate landing on a predicted image / harmonic / intermod frequency of the source.
    ArtifactOf,
}

impl RelationKind {
    /// The stored text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SuppressedBy => "suppressed-by",
            Self::DuplicateOf => "duplicate-of",
            Self::ArtifactOf => "artifact-of",
        }
    }
}

/// Which receiver mechanism an [`RelationKind::ArtifactOf`] claim names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    /// Mirror about the tuning centre: `2·f_LO − f`.
    Image,
    /// `n·f`, n ≥ 2.
    Harmonic,
    /// `a·f1 ± b·f2`, a, b ≥ 1.
    Intermod,
}

impl ArtifactKind {
    /// The stored text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Harmonic => "harmonic",
            Self::Intermod => "intermod",
        }
    }
}

/// Who claimed (or revoked) a relationship.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RelationAuthor {
    /// A rule.
    System,
    /// A person, through the API.
    User,
}

impl RelationAuthor {
    /// The stored text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
        }
    }
}

/// One append-only relationship row.
#[derive(Clone, Debug, PartialEq)]
pub struct EmitterRelation {
    /// Row id (monotonic; the highest row for a `(emitter, source, kind)` triple is current).
    pub relation_id: i64,
    /// The subordinate row: the candidate that defers.
    pub emitter_id: EmitterId,
    /// What it defers to.
    pub source_id: EmitterId,
    /// Which claim.
    pub kind: RelationKind,
    /// The mechanism, for [`RelationKind::ArtifactOf`].
    pub artifact: Option<ArtifactKind>,
    /// `true` = in force, `false` = revoked (a revocation is itself an appended row).
    pub active: bool,
    /// When it was claimed or revoked.
    pub t: Timestamp,
    /// Rule or person.
    pub author: RelationAuthor,
    /// The rule id (e.g. `hk-pipeline/overlap@1`) or a token fingerprint.
    pub actor: String,
    /// Backend-rendered reasoning, including the artifact arithmetic. Always disclosed.
    pub reason: String,
    /// The rank proxy that decided it, for [`RelationKind::DuplicateOf`].
    pub score: Option<f64>,
    /// The arithmetic or the rank terms, as JSON.
    pub detail: Option<serde_json::Value>,
}

/// A relationship offered to the repository.
#[derive(Clone, Debug, PartialEq)]
pub struct RelationClaim {
    /// The subordinate row.
    pub emitter_id: EmitterId,
    /// What it defers to.
    pub source_id: EmitterId,
    /// Which claim.
    pub kind: RelationKind,
    /// The mechanism, for [`RelationKind::ArtifactOf`] (required there, refused otherwise).
    pub artifact: Option<ArtifactKind>,
    /// `true` to claim, `false` to revoke a standing claim.
    pub active: bool,
    /// When.
    pub t: Timestamp,
    /// Rule or person.
    pub author: RelationAuthor,
    /// Rule id or token fingerprint.
    pub actor: String,
    /// Why, rendered for display.
    pub reason: String,
    /// The rank proxy, when one decided it.
    pub score: Option<f64>,
    /// The arithmetic or rank terms.
    pub detail: Option<serde_json::Value>,
}

/// Whether an inventory query lists rows that currently defer to another row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RelationVisibility {
    /// Hide rows with a standing suppression, duplicate or artifact claim (the default: this is
    /// the clutter T-219 removes). Their rows, detections, tracks and history are all kept.
    #[default]
    Shown,
    /// List every row, deferring or not.
    All,
}

// ---------------------------------------------------------------------------------------------
// Evidence and the guard
// ---------------------------------------------------------------------------------------------

/// One receive chain: the front end that made a measurement, and the RF path into it.
///
/// This is the key for **device-local physics** (T-302). An image, harmonic or intermodulation
/// product is manufactured by one mixer, one LO and one non-linearity; a signal arriving at another
/// front end's antenna cannot produce an artifact here, however well the arithmetic fits. The port
/// is part of the key because one device with a switched antenna bank (an Opera Cake) has several
/// RF paths, and a strong signal entering one port is not in the mixer while another is selected.
///
/// Distinct from [`crate::repo::ProvenanceChain`], which resolves a measurement's calibration and
/// spur-mask versions rather than naming its receive path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReceiveChain {
    /// Source device identity, as [`crate::Provenance::device_id`] records it.
    pub device_id: String,
    /// Active antenna/filter port, when the front end recorded one.
    pub antenna_port: Option<String>,
}

impl ReceiveChain {
    /// A chain on `device_id` with no port recorded.
    pub fn device(device_id: impl Into<String>) -> Self {
        Self {
            device_id: device_id.into(),
            antenna_port: None,
        }
    }

    /// Whether two measurements could have been made through the same receive chain.
    ///
    /// The device must match — that is the physics, and it is never relaxed. The port is compared
    /// **only when both sides recorded one**: an unrecorded port is unknown, not a different path,
    /// and treating it as a mismatch would silently stop attributing artifacts on every front end
    /// that does not switch antennas.
    pub fn same_chain(&self, other: &Self) -> bool {
        self.device_id == other.device_id
            && match (&self.antenna_port, &other.antenna_port) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

/// A tuning centre and the receive chain that tuned it.
///
/// The two are one value on purpose (T-302): an image exists only at the LO of the mixer that made
/// it, so an LO readable without its chain is an invitation to mirror one front end's emitter about
/// another front end's tuning.
#[derive(Clone, Debug, PartialEq)]
pub struct TunedLo {
    /// Which receive chain was tuned there.
    pub chain: ReceiveChain,
    /// The tuning centre, Hz.
    pub lo_hz: f64,
}

/// The distinct receive chains in a tuning history, in first-seen order.
pub fn distinct_chains(tuned_lo: &[TunedLo]) -> Vec<ReceiveChain> {
    let mut out: Vec<ReceiveChain> = Vec::new();
    for t in tuned_lo {
        if !out.contains(&t.chain) {
            out.push(t.chain.clone());
        }
    }
    out
}

/// Whether `chains` holds one that could be the same receive chain as `c`.
fn on_chain(chains: &[ReceiveChain], c: &ReceiveChain) -> bool {
    chains.iter().any(|x| x.same_chain(c))
}

/// Whether `chains` and `targets` share a receive chain.
fn on_any_chain(chains: &[ReceiveChain], targets: &[ReceiveChain]) -> bool {
    targets.iter().any(|c| on_chain(chains, c))
}

/// What the rules need to know about one live inventory row, gathered from its stored measurement
/// and its linked detections. Everything here is measured; nothing comes from a database of known
/// signals.
#[derive(Clone, Debug, PartialEq)]
pub struct RowEvidence {
    /// The row.
    pub emitter_id: EmitterId,
    /// Measured centre, Hz.
    pub f_center_hz: f64,
    /// Measured occupied bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Latest measured −3 dB width, Hz, when a linked detection recorded one.
    pub xdb_bandwidth_hz: Option<f64>,
    /// The entry is `confirmed` (as opposed to a candidate).
    pub confirmed: bool,
    /// Peak SNR of the newest linked detection, dB.
    pub snr_db: Option<f64>,
    /// Absolute peak level of the newest linked detection, dBFS.
    pub peak_dbfs: Option<f64>,
    /// Measured duty cycle, when the fingerprint carries one.
    pub duty_cycle: Option<f64>,
    /// Share of linked detections carrying a suspect flag (clipped, spur, image, IMD, compressed).
    pub suspect_fraction: f64,
    /// A linked detection carried `image_candidate`: the detector's own mirror test saw this row as
    /// a possible IQ image of a stronger signal.
    pub image_flagged: bool,
    /// A linked detection carried `suspect_imd` (the gain-step test grew faster than 1 dB per dB).
    pub imd_flagged: bool,
    /// A linked detection carried `spur_candidate`.
    pub spur_flagged: bool,
    /// Decoded transmitter identity, when it holds one.
    pub identity: Option<DecodedIdentity>,
    /// Stored fingerprint, when it has one.
    pub fingerprint: Option<Fingerprint>,
    /// Distinct tuning centres (LO) the linked detections were measured under, each paired with
    /// the receive chain that tuned it. Never a bare frequency: see [`TunedLo`].
    pub tuned_lo: Vec<TunedLo>,
    /// Observation spans, `(t_start_ns, t_end_ns)`, by start.
    pub spans: Vec<(i64, i64)>,
    /// Sightings counted into the row (a tie-break, never a score).
    pub count: i64,
    /// First sighting, ns (a tie-break).
    pub first_seen_ns: i64,
}

impl RowEvidence {
    /// The measured occupied band.
    pub fn freq(&self) -> FreqRange {
        FreqRange::centered(self.f_center_hz, self.bandwidth_hz)
    }

    /// The −3 dB extent when one was measured, else the occupied band.
    pub fn xdb_freq(&self) -> FreqRange {
        FreqRange::centered(
            self.f_center_hz,
            self.xdb_bandwidth_hz.unwrap_or(self.bandwidth_hz),
        )
    }

    /// The distinct receive chains this row was measured on, in first-seen order. Derived from
    /// [`RowEvidence::tuned_lo`]: every detection carries the tune state it was measured under, so
    /// a chain that never tuned never measured this row.
    pub fn chains(&self) -> Vec<ReceiveChain> {
        distinct_chains(&self.tuned_lo)
    }

    /// The provisional rank proxy (see [`rank_score`]).
    pub fn rank(&self) -> f64 {
        rank_score(self.snr_db, self.duty_cycle, self.suspect_fraction)
    }

    /// The **measured** flag that corroborates `kind`, or `None` when nothing in the measurement
    /// does. Frequency arithmetic is a coincidence until the front end itself says the row looks
    /// manufactured: the detector's mirror test (`image_candidate`), its spur mask
    /// (`spur_candidate`) or the gain-step test (`suspect_imd`). Without one, a row that merely
    /// lands on a predicted frequency stays an independent emission.
    pub fn corroborates(&self, kind: ArtifactKind) -> Option<&'static str> {
        match kind {
            ArtifactKind::Image if self.image_flagged => Some("image_candidate"),
            // A harmonic of a strong emitter is a non-linearity product of the front end, which the
            // spur mask and the gain-step test are the measurements of.
            ArtifactKind::Harmonic if self.spur_flagged => Some("spur_candidate"),
            ArtifactKind::Harmonic | ArtifactKind::Intermod if self.imd_flagged => {
                Some("suspect_imd")
            }
            _ => None,
        }
    }
}

/// Whether any presence interval of `a` overlaps one of `b` (both sorted by start, as
/// [`RowEvidence::spans`] is). Two rows whose intervals never overlap were **observed over
/// different windows**; see [`distinguishing_evidence`].
fn spans_overlap(a: &[(i64, i64)], b: &[(i64, i64)]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].1 < b[j].0 {
            i += 1;
        } else if b[j].1 < a[i].0 {
            j += 1;
        } else {
            return true;
        }
    }
    false
}

/// Share of the **narrower** of two bands that the two have in common, 0–1. `0` when they do not
/// overlap; `1` when the narrower lies inside the wider.
pub fn overlap_fraction(a: FreqRange, b: FreqRange) -> f64 {
    let lo = a.lo_hz.max(b.lo_hz);
    let hi = a.hi_hz.min(b.hi_hz);
    let narrower = a.width_hz().min(b.width_hz());
    if !narrower.is_finite() || narrower <= 0.0 || hi <= lo {
        return 0.0;
    }
    ((hi - lo) / narrower).clamp(0.0, 1.0)
}

/// Share of the **wider** of two bands that the two have in common, 0–1. A narrow emission wholly
/// inside a wide one scores only its width ratio here, however completely its own band is covered.
pub fn overlap_fraction_wider(a: FreqRange, b: FreqRange) -> f64 {
    let lo = a.lo_hz.max(b.lo_hz);
    let hi = a.hi_hz.min(b.hi_hz);
    let wider = a.width_hz().max(b.width_hz());
    if !wider.is_finite() || wider <= 0.0 || hi <= lo {
        return 0.0;
    }
    ((hi - lo) / wider).clamp(0.0, 1.0)
}

/// Whether two bands overlap enough for the rows to compete at all: at least
/// [`OVERLAP_MIN_FRACTION`] of the narrower band **and** of the wider one.
///
/// The second half is what keeps a narrow signal inside a wide one visible. Measured against the
/// narrower band alone, a 12.5 kHz subcarrier inside a 180 kHz station overlaps it by 100 %, which
/// would make the subcarrier a perfect "duplicate" of its host and hide it. Against the wider band
/// it overlaps by 7 %, and the two rows never compete.
pub fn bands_compete(a: FreqRange, b: FreqRange) -> bool {
    overlap_fraction(a, b) >= OVERLAP_MIN_FRACTION
        && overlap_fraction_wider(a, b) >= OVERLAP_MIN_FRACTION
}

/// The provisional duplicate-rank proxy: **SNR × duty × trust**, explicitly *not* the evidence-bits
/// ladder (ADR-0015 §11: bits are MAUTO work). Higher is stronger.
///
/// - SNR: the amplitude ratio against [`RANK_SNR_REF_DB`], so 10 dB scores 1.0 and every 20 dB is
///   a factor of 10. An unmeasured SNR scores 1.0 (neutral).
/// - Duty: the measured duty cycle, floored at [`RANK_DUTY_FLOOR`]; unmeasured is
///   [`RANK_DUTY_UNKNOWN`].
/// - Trust: `1 − RANK_SUSPECT_WEIGHT × suspect_fraction`, floored at [`RANK_TRUST_FLOOR`].
///
/// # Two of the three terms are observation statistics — which is allowed here, and only here
///
/// `duty_cycle` measures the window the row was watched over, and `suspect_fraction` the share of
/// this receiver's own looks that were flagged. Neither is a property of the emission, so neither
/// may ever be **distinguishing evidence** between two rows — and neither is:
/// [`distinguishing_evidence`] excludes the window statistics when the two rows' presence
/// intervals are disjoint, and never looks at `suspect_fraction` at all.
///
/// This proxy is the legitimate use (T-281). It runs **after** [`distinguishing_evidence`] has
/// already found nothing to tell the rows apart, so it is choosing which of two readings of *one*
/// emission to show, not deciding whether they are one emission. Changing it can change which row
/// is displayed; it can never split an emitter or merge two.
///
/// The SNR term is a signal property as wired: `RowEvidence::snr_db` is filled from the
/// detection's **`snr_peak`** column, not `snr_mean_db`. That matters — `snr_mean_db` averages the
/// excess over the whole occupied band, so it falls as an emission widens (T-280 moved the
/// in-band fragment rule onto the peak for the same reason). Keep it reading the peak.
pub fn rank_score(snr_db: Option<f64>, duty_cycle: Option<f64>, suspect_fraction: f64) -> f64 {
    let snr = match snr_db.filter(|v| v.is_finite()) {
        Some(v) => 10f64.powf((v.clamp(0.0, RANK_SNR_MAX_DB) - RANK_SNR_REF_DB) / 20.0),
        None => 1.0,
    };
    let duty = duty_cycle
        .filter(|v| v.is_finite())
        .unwrap_or(RANK_DUTY_UNKNOWN)
        .clamp(RANK_DUTY_FLOOR, 1.0);
    let suspect = if suspect_fraction.is_finite() {
        suspect_fraction.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let trust = (1.0 - RANK_SUSPECT_WEIGHT * suspect).max(RANK_TRUST_FLOOR);
    snr * duty * trust
}

/// The guard (ADR-0015 §11.4): the first piece of evidence that tells `a` and `b` apart, or `None`
/// when nothing does. **Band overlap alone is never sufficient to merge or suppress — only to
/// compete.** Checked in order, cheapest and most decisive first:
///
/// 1. two different decoded identities (e.g. different RDS PI) — always blocks;
/// 2. measured bandwidths further apart than `tol.bandwidth_ratio` — checked on the measurement
///    itself, so it holds for a row with no fingerprint (or one of an older feature-set version);
/// 3. a [`Fingerprint::compare`] distance beyond `tol` — excluding the centre always, and
///    excluding `duty_cycle` / `burst_length_s` / `period_s` when the two rows' presence intervals
///    are disjoint, because those are statistics of the window each row was watched over rather
///    than properties of the emission (T-250);
/// 4. −3 dB extents that do not overlap, with centres separated by more than the summed
///    measurement uncertainty.
pub fn distinguishing_evidence(
    a: &RowEvidence,
    b: &RowEvidence,
    tol: &Tolerances,
) -> Option<&'static str> {
    if let (Some(x), Some(y)) = (&a.identity, &b.identity)
        && x != y
    {
        return Some("different decoded identities");
    }
    // Bandwidth, from the **measurement** and unconditionally. This deliberately does not wait for
    // both rows to carry a fingerprint: `Fingerprint::from_value` returns `None` for any other
    // feature-set version, so a fingerprint-only test silently stops protecting anything the day
    // the version moves. A narrow emission sitting inside a wide one — a 12.5 kHz subcarrier in a
    // 180 kHz station — is exactly the row that must never be hidden as a "duplicate" of its host.
    if a.bandwidth_hz > 0.0 && b.bandwidth_hz > 0.0 {
        let ratio = a.bandwidth_hz.max(b.bandwidth_hz) / a.bandwidth_hz.min(b.bandwidth_hz);
        if ratio > tol.bandwidth_ratio.max(1.0) {
            return Some("bandwidth ratio beyond tolerance");
        }
    }
    if let (Some(x), Some(y)) = (&a.fingerprint, &b.fingerprint) {
        // Every feature **except the centre**: bandwidth, family, symbol rate, deviation, period,
        // duty cycle, burst length and hop set. Where the box sits is the overlap question, not
        // distinguishing evidence — including the centre term here would make the guard reject
        // exactly the offset duplicates of one station that T-219 exists to collapse (two readings
        // 60 kHz apart on a 180 kHz station are one emission, and the centre tolerance is 45 kHz).
        let mut base = x.clone();
        let mut aligned = y.clone();
        aligned.f_center_hz = x.f_center_hz;
        // T-250: **and except the time-sampling features, when the two rows were not observed over
        // the same window.** `duty_cycle`, `burst_length_s` and `period_s` are statistics of the
        // span each row happened to be watched over, not properties of the emission. When the two
        // rows' presence intervals are disjoint (ADR-0017 §1.1: a signal that stops and returns
        // appends a *new* interval to the same emitter) the two figures measure different windows
        // and are not comparable, so they cannot be distinguishing evidence — for the same reason
        // the centre is not. Measured on the user's 2026-09-16 staging scene: one 377 kHz station
        // seen over 94–399 s and again over 471–530 s reported burst lengths of 0.68 s and 0.37 s,
        // 1.86x apart, and that alone split it into two inventory rows 492 Hz apart.
        //
        // Deliberately narrow: while the intervals **do** overlap, the two rows watched the same
        // window, the figures are comparable, and they still separate two emissions sharing a
        // channel and a bandwidth (the ISM case) exactly as before.
        let same_window =
            a.spans.is_empty() || b.spans.is_empty() || spans_overlap(&a.spans, &b.spans);
        if !same_window {
            for f in [&mut base, &mut aligned] {
                f.duty_cycle = None;
                f.burst_length_s = None;
                f.period_s = None;
            }
        }
        if !base.compare(&aligned, tol).within {
            return Some("fingerprint distance beyond tolerance");
        }
    }
    let (fa, fb) = (a.xdb_freq(), b.xdb_freq());
    // The uncertainty on a measured centre, not the clustering centre tolerance: the bandwidth
    // fraction that widens `Fingerprint::center_tolerance_hz` is already accounted for by the
    // extents themselves, and folding it in again would make the test unable to separate anything.
    let f = a.f_center_hz.abs().max(b.f_center_hz.abs());
    let uncertainty = (tol.center_ppm * 1e-6 * f).max(tol.center_min_hz);
    let gap = (a.f_center_hz - b.f_center_hz).abs() - 0.5 * (fa.width_hz() + fb.width_hz());
    if gap > uncertainty {
        return Some("separated -3 dB extents");
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Geometric artifact prediction
// ---------------------------------------------------------------------------------------------

/// A strong confirmed emitter offered to [`predict_artifacts`] as a possible source.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactSource {
    /// The source row.
    pub emitter_id: EmitterId,
    /// Its measured centre, Hz.
    pub f_center_hz: f64,
    /// Its measured bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Its latest peak level, dBFS.
    pub level_dbfs: f64,
    /// The receive chains this emitter was actually measured on (T-302). An artifact belongs to
    /// one receive chain, so a source seen only on another front end — or another antenna port —
    /// can never explain a measurement made here, whatever the arithmetic says.
    pub chains: Vec<ReceiveChain>,
}

/// One arithmetic coincidence that survived the frequency and level tests. It is a **ranked
/// suggestion with its arithmetic disclosed**, never a truth and never a delete.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactPrediction {
    /// The source (the first source, for an intermod).
    pub source: EmitterId,
    /// The second source, for an intermod.
    pub second_source: Option<EmitterId>,
    /// The mechanism.
    pub kind: ArtifactKind,
    /// `n` for a harmonic, `a + b` for an intermod, 1 for an image.
    pub order: u32,
    /// `a` in `a·f1 ± b·f2` (the harmonic order `n` for a harmonic).
    pub a: u32,
    /// `b` in `a·f1 ± b·f2`, 0 otherwise.
    pub b: u32,
    /// `+1` or `−1`: the sign on `b·f2`.
    pub sign: i32,
    /// The tuning centre used, for an image.
    pub lo_hz: Option<f64>,
    /// Where the mechanism says the artifact lands, Hz.
    pub predicted_hz: f64,
    /// How wide the mechanism says it is, Hz.
    pub predicted_bandwidth_hz: f64,
    /// Measured centre minus predicted, Hz.
    pub error_hz: f64,
    /// The centre tolerance the match had to meet, Hz.
    pub tolerance_hz: f64,
    /// How far the candidate sits below its source, dB.
    pub suppression_db: f64,
}

impl ArtifactPrediction {
    /// The arithmetic, spelled out for display. Every artifact claim carries this.
    pub fn arithmetic(&self) -> String {
        let f = |hz: f64| format!("{:.6} MHz", hz / 1e6);
        let head = match self.kind {
            ArtifactKind::Image => format!(
                "image: 2 x {} (tuned LO) - {} (source) = {}",
                f(self.lo_hz.unwrap_or(f64::NAN)),
                f(2.0 * self.lo_hz.unwrap_or(f64::NAN) - self.predicted_hz),
                f(self.predicted_hz),
            ),
            ArtifactKind::Harmonic => format!(
                "harmonic: {} x {} = {}",
                self.a,
                f(self.predicted_hz / f64::from(self.a)),
                f(self.predicted_hz),
            ),
            ArtifactKind::Intermod => format!(
                "intermod (order {}): {} x f1 {} {} x f2 = {}",
                self.order,
                self.a,
                if self.sign >= 0 { "+" } else { "-" },
                self.b,
                f(self.predicted_hz),
            ),
        };
        format!(
            "{head}; measured {} off prediction (tolerance {}), {:.1} dB below source",
            format_args!("{:.3} kHz", self.error_hz / 1e3),
            format_args!("{:.3} kHz", self.tolerance_hz / 1e3),
            self.suppression_db,
        )
    }
}

/// Centre tolerance for an artifact match: the **measured** bandwidth of the row being explained,
/// scaled by [`ARTIFACT_CENTER_BW_FRACTION`] and floored at [`ARTIFACT_CENTER_MIN_HZ`]. It does not
/// depend on the mechanism or its order, so the acceptance window cannot grow with `n`.
fn artifact_tolerance_hz(measured_bw_hz: f64) -> f64 {
    (ARTIFACT_CENTER_BW_FRACTION * measured_bw_hz.max(0.0)).max(ARTIFACT_CENTER_MIN_HZ)
}

/// True when a measured bandwidth is consistent with the width the mechanism predicts, within
/// [`ARTIFACT_BANDWIDTH_RATIO`]. **An unmeasured or zero bandwidth is never consistent**: without a
/// width there is no corroboration, and the row stays an independent emission.
pub fn bandwidth_consistent(measured_bw_hz: f64, predicted_bw_hz: f64) -> bool {
    if !measured_bw_hz.is_finite()
        || !predicted_bw_hz.is_finite()
        || measured_bw_hz <= 0.0
        || predicted_bw_hz <= 0.0
    {
        return false;
    }
    measured_bw_hz.max(predicted_bw_hz) / measured_bw_hz.min(predicted_bw_hz)
        <= ARTIFACT_BANDWIDTH_RATIO
}

/// True when the level difference is consistent with a receiver artifact: the candidate is at
/// least [`ARTIFACT_MIN_SUPPRESSION_DB`] below its source, and not so far below that the
/// coincidence carries no information ([`ARTIFACT_MAX_SUPPRESSION_DB`]).
fn level_consistent(suppression_db: f64) -> bool {
    suppression_db.is_finite()
        && (ARTIFACT_MIN_SUPPRESSION_DB..=ARTIFACT_MAX_SUPPRESSION_DB).contains(&suppression_db)
}

/// Geometric artifact prediction (C40): every image / harmonic / intermod coincidence between
/// `target` and the strong confirmed `sources`, using the tuning history in `tuned_lo`, that passes
/// the frequency, level **and receive-chain** tests. Best first (lowest order, then smallest error).
///
/// **Every mechanism is confined to one receive chain** (T-302). An artifact is made by one front
/// end's mixer and non-linearity, so a source is only ever mirrored about an LO its own chain
/// tuned, and a harmonic or intermod product is only ever claimed on a chain that measured both the
/// product and its source(s). A source seen only on another device (or another antenna port) is
/// skipped. `tuned_lo` is the **target's own** tuning history, so an empty one attributes nothing:
/// without knowing which chain measured this row there is no receive chain to reason about, and
/// failing closed keeps the arithmetic from claiming what it cannot support.
///
/// Otherwise this is arithmetic only. The caller still has to apply the **presence** test — an
/// artifact is there only while its source is — and the guard, before recording a claim.
pub fn predict_artifacts(
    target_f_center_hz: f64,
    target_bandwidth_hz: f64,
    target_level_dbfs: f64,
    sources: &[ArtifactSource],
    tuned_lo: &[TunedLo],
) -> Vec<ArtifactPrediction> {
    let mut out: Vec<ArtifactPrediction> = Vec::new();
    if !target_f_center_hz.is_finite() || !target_level_dbfs.is_finite() {
        return out;
    }
    // One tolerance for every mechanism: it comes from the measured row, not from the prediction.
    let tol = artifact_tolerance_hz(target_bandwidth_hz);
    // The chains that actually measured this row. Nothing below is claimed off one of them.
    let target_chains = distinct_chains(tuned_lo);
    let mut push = |p: ArtifactPrediction| {
        // Three independent tests, all measured: the centre, the width the mechanism implies, and
        // the level. A prediction that lands on the source itself says nothing: that is the
        // duplicate rule's business, not the artifact rule's.
        if (p.predicted_hz - target_f_center_hz).abs() <= p.tolerance_hz
            && bandwidth_consistent(target_bandwidth_hz, p.predicted_bandwidth_hz)
            && level_consistent(p.suppression_db)
        {
            out.push(p);
        }
    };
    for s in sources {
        if !s.f_center_hz.is_finite() || !s.level_dbfs.is_finite() || s.f_center_hz <= 0.0 {
            continue;
        }
        let suppression = s.level_dbfs - target_level_dbfs;
        // Image: the mirror of the source about the tuning centre — of the mixer that made it.
        for lo in tuned_lo {
            if !lo.lo_hz.is_finite() {
                continue;
            }
            // This chain's LO can only mirror a signal that entered this chain.
            if !on_chain(&s.chains, &lo.chain) {
                continue;
            }
            let predicted = 2.0 * lo.lo_hz - s.f_center_hz;
            if predicted <= 0.0 {
                continue;
            }
            // The source sitting at its own LO has no distinct mirror.
            if (predicted - s.f_center_hz).abs() <= ARTIFACT_CENTER_MIN_HZ {
                continue;
            }
            push(ArtifactPrediction {
                source: s.emitter_id,
                second_source: None,
                kind: ArtifactKind::Image,
                order: 1,
                a: 1,
                b: 0,
                sign: 1,
                lo_hz: Some(lo.lo_hz),
                predicted_hz: predicted,
                predicted_bandwidth_hz: s.bandwidth_hz,
                error_hz: target_f_center_hz - predicted,
                tolerance_hz: tol,
                suppression_db: suppression,
            });
        }
        // Harmonics: this front end's own non-linearity product, so the source has to have been
        // measured on a chain that measured this row. Deliberately placed after the image loop —
        // skipping to the next source here skips only the harmonics.
        if !on_any_chain(&s.chains, &target_chains) {
            continue;
        }
        for n in 2..=HARMONIC_MAX_ORDER {
            let predicted = f64::from(n) * s.f_center_hz;
            let bw = f64::from(n) * s.bandwidth_hz;
            push(ArtifactPrediction {
                source: s.emitter_id,
                second_source: None,
                kind: ArtifactKind::Harmonic,
                order: n,
                a: n,
                b: 0,
                sign: 1,
                lo_hz: None,
                predicted_hz: predicted,
                predicted_bandwidth_hz: bw,
                error_hz: target_f_center_hz - predicted,
                tolerance_hz: tol,
                suppression_db: suppression,
            });
        }
    }
    // Intermodulation between pairs of sources.
    for (i, s1) in sources.iter().enumerate() {
        for s2 in sources.iter().skip(i + 1) {
            if !s1.f_center_hz.is_finite() || !s2.f_center_hz.is_finite() {
                continue;
            }
            // Both tones have to have reached the same mixer as the row being explained: a product
            // needs its two sources and its victim on one receive chain.
            if !target_chains
                .iter()
                .any(|c| on_chain(&s1.chains, c) && on_chain(&s2.chains, c))
            {
                continue;
            }
            // The weaker source sets how strong the product can be.
            let suppression = s1.level_dbfs.min(s2.level_dbfs) - target_level_dbfs;
            for a in 1..INTERMOD_MAX_ORDER {
                for b in 1..=(INTERMOD_MAX_ORDER - a) {
                    for sign in [1i32, -1] {
                        let (fa, fb) = (f64::from(a), f64::from(b));
                        let predicted = fa * s1.f_center_hz + f64::from(sign) * fb * s2.f_center_hz;
                        if predicted <= 0.0 {
                            continue;
                        }
                        let bw = fa * s1.bandwidth_hz + fb * s2.bandwidth_hz;
                        // A product landing on one of its own sources explains nothing.
                        if (predicted - s1.f_center_hz).abs() <= tol
                            || (predicted - s2.f_center_hz).abs() <= tol
                        {
                            continue;
                        }
                        push(ArtifactPrediction {
                            source: s1.emitter_id,
                            second_source: Some(s2.emitter_id),
                            kind: ArtifactKind::Intermod,
                            order: a + b,
                            a,
                            b,
                            sign,
                            lo_hz: None,
                            predicted_hz: predicted,
                            predicted_bandwidth_hz: bw,
                            error_hz: target_f_center_hz - predicted,
                            tolerance_hz: tol,
                            suppression_db: suppression,
                        });
                    }
                }
            }
        }
    }
    out.sort_by(|x, y| {
        x.order
            .cmp(&y.order)
            .then(x.error_hz.abs().total_cmp(&y.error_hz.abs()))
            .then(x.source.cmp(&y.source))
    });
    out
}

/// The presence test: every span of the artifact lies inside some span of its source, widened by
/// [`ARTIFACT_PRESENCE_SLACK_NS`]. An emission seen while its supposed source was absent is an
/// independent emitter, whatever the arithmetic says.
pub fn present_only_with(artifact: &[(i64, i64)], source: &[(i64, i64)]) -> bool {
    if artifact.is_empty() || source.is_empty() {
        return false;
    }
    artifact.iter().all(|&(a0, a1)| {
        source.iter().any(|&(s0, s1)| {
            a0 >= s0.saturating_sub(ARTIFACT_PRESENCE_SLACK_NS)
                && a1 <= s1.saturating_add(ARTIFACT_PRESENCE_SLACK_NS)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eid(n: u8) -> EmitterId {
        EmitterId::from_uuid(uuid::Uuid::from_bytes([n; 16]))
    }

    fn row(f: f64, bw: f64) -> RowEvidence {
        RowEvidence {
            emitter_id: eid(1),
            f_center_hz: f,
            bandwidth_hz: bw,
            xdb_bandwidth_hz: None,
            confirmed: false,
            snr_db: Some(20.0),
            peak_dbfs: Some(-20.0),
            duty_cycle: Some(1.0),
            suspect_fraction: 0.0,
            image_flagged: false,
            imd_flagged: false,
            spur_flagged: false,
            identity: None,
            fingerprint: None,
            tuned_lo: Vec::new(),
            spans: vec![(0, 1_000_000_000)],
            count: 1,
            first_seen_ns: 0,
        }
    }

    fn chain(dev: &str) -> ReceiveChain {
        ReceiveChain::device(dev)
    }

    fn lo_on(dev: &str, lo: f64) -> TunedLo {
        TunedLo {
            chain: chain(dev),
            lo_hz: lo,
        }
    }

    /// A strong confirmed source measured on one named front end.
    fn source_on(dev: &str, id: u8, f: f64, bw: f64, level: f64) -> ArtifactSource {
        ArtifactSource {
            emitter_id: eid(id),
            f_center_hz: f,
            bandwidth_hz: bw,
            level_dbfs: level,
            chains: vec![chain(dev)],
        }
    }

    #[test]
    fn offset_boxes_of_one_station_overlap_and_nothing_tells_them_apart() {
        // Two readings of one 180 kHz WFM station, one 60 kHz into the skirt.
        let a = row(101.3e6, 180e3);
        let mut b = row(101.36e6, 180e3);
        b.emitter_id = eid(2);
        assert!(overlap_fraction(a.freq(), b.freq()) >= OVERLAP_MIN_FRACTION);
        assert_eq!(
            distinguishing_evidence(&a, &b, &Tolerances::default()),
            None
        );
    }

    #[test]
    fn two_adjacent_stations_are_told_apart_by_their_extents() {
        let a = row(101.3e6, 180e3);
        let mut b = row(101.5e6, 180e3);
        b.emitter_id = eid(2);
        assert_eq!(
            distinguishing_evidence(&a, &b, &Tolerances::default()),
            Some("separated -3 dB extents")
        );
    }

    #[test]
    fn two_decoded_identities_always_block() {
        let mut a = row(101.3e6, 180e3);
        let mut b = row(101.31e6, 180e3);
        b.emitter_id = eid(2);
        a.identity = Some(DecodedIdentity {
            scheme: crate::emitter::IdentityScheme::RdsPi,
            value: "1694".into(),
        });
        b.identity = Some(DecodedIdentity {
            scheme: crate::emitter::IdentityScheme::RdsPi,
            value: "8A2B".into(),
        });
        assert_eq!(
            distinguishing_evidence(&a, &b, &Tolerances::default()),
            Some("different decoded identities")
        );
    }

    #[test]
    fn a_stronger_cleaner_box_outranks_a_weak_suspect_one() {
        let strong = rank_score(Some(24.0), Some(1.0), 0.0);
        let weak = rank_score(Some(9.0), Some(1.0), 0.8);
        assert!(strong > weak, "{strong} vs {weak}");
    }

    #[test]
    fn an_image_is_predicted_from_the_tuning_centre_and_a_real_neighbour_is_not() {
        let source = source_on("A", 9, 101.3e6, 180e3, -18.0);
        let los = [lo_on("A", 100.8e6)];
        // Tuned at 100.8 MHz, the mirror of 101.3 MHz lands at 100.3 MHz.
        let hits = predict_artifacts(100.3e6, 180e3, -48.0, std::slice::from_ref(&source), &los);
        let top = hits.first().expect("image predicted");
        assert_eq!(top.kind, ArtifactKind::Image);
        assert!(top.error_hz.abs() < 1.0, "{}", top.error_hz);
        assert!((top.suppression_db - 30.0).abs() < 1e-9);
        // A real adjacent station 200 kHz from the source fits no mechanism.
        assert!(
            predict_artifacts(101.5e6, 180e3, -20.0, std::slice::from_ref(&source), &los)
                .is_empty()
        );
    }

    #[test]
    fn an_artifact_as_strong_as_its_source_is_not_claimed() {
        let source = source_on("A", 9, 101.3e6, 180e3, -18.0);
        assert!(
            predict_artifacts(100.3e6, 180e3, -20.0, &[source], &[lo_on("A", 100.8e6)]).is_empty()
        );
    }

    #[test]
    fn a_third_order_product_of_two_strong_sources_is_predicted() {
        let s1 = source_on("A", 1, 100.0e6, 180e3, -20.0);
        let s2 = source_on("A", 2, 100.4e6, 180e3, -22.0);
        // 2 f1 - f2 = 99.6 MHz, and the product spreads to 2 x 180 + 180 = 540 kHz. The chain is
        // tuned at 100.2 MHz, whose mirrors of the two sources (100.4 and 100.0 MHz) are nowhere
        // near the product, so the intermod is the only mechanism that fits.
        let los = [lo_on("A", 100.2e6)];
        let hits = predict_artifacts(99.6e6, 540e3, -55.0, &[s1.clone(), s2.clone()], &los);
        let top = hits.first().expect("intermod predicted");
        assert_eq!(top.kind, ArtifactKind::Intermod);
        assert_eq!((top.a, top.b, top.sign), (2, 1, -1));
        assert_eq!(top.order, 3);
        // A narrow box on the same frequency is not that product: the mechanism says 540 kHz.
        assert!(predict_artifacts(99.6e6, 12.5e3, -55.0, &[s1, s2], &los).is_empty());
    }

    /// The guard's central case: a narrow emission inside a wide one is never its duplicate,
    /// however completely their bands overlap.
    #[test]
    fn a_narrow_signal_inside_a_wide_station_never_competes_with_it() {
        let wide = row(100.3e6, 180e3);
        let mut narrow = row(100.3e6, 12.5e3);
        narrow.emitter_id = eid(2);
        assert_eq!(
            overlap_fraction(wide.freq(), narrow.freq()),
            1.0,
            "the narrow band lies wholly inside the wide one"
        );
        assert!(
            overlap_fraction_wider(wide.freq(), narrow.freq()) < 0.1,
            "but it covers almost none of the wide one"
        );
        assert!(!bands_compete(wide.freq(), narrow.freq()));
        assert_eq!(
            distinguishing_evidence(&wide, &narrow, &Tolerances::default()),
            Some("bandwidth ratio beyond tolerance"),
            "and the measurement alone tells them apart, with no fingerprint on either row"
        );
    }

    /// The user's field case of 2026-09-15: short ~8.5 kHz bursts at 100.300 MHz, a wideband
    /// station at 101.303 MHz, front end tuned to 100.800 MHz. 2 x 100.800 - 101.303 = 100.297,
    /// 3 kHz from the burst and inside the 5 kHz floor — the arithmetic fits. The physics does not:
    /// an image preserves its source's width.
    #[test]
    fn the_100p3_burst_is_not_the_image_of_a_wideband_station() {
        let source = source_on("A", 9, 101.303e6, 180e3, -18.0);
        let los = [lo_on("A", 100.8e6)];
        assert!(
            predict_artifacts(100.300e6, 8.5e3, -48.0, std::slice::from_ref(&source), &los)
                .is_empty(),
            "an 8.5 kHz burst is not the image of a 180 kHz station"
        );
        // The same arithmetic on a box of the width the mechanism implies is still offered.
        let wide = predict_artifacts(100.300e6, 180e3, -48.0, std::slice::from_ref(&source), &los);
        assert_eq!(wide.first().map(|p| p.kind), Some(ArtifactKind::Image));
        assert!(
            (wide[0].error_hz - 3_000.0).abs() < 1.0,
            "{}",
            wide[0].error_hz
        );
    }

    /// A WFM station's fingerprint as the two staging rows recorded it.
    fn wfm(f: f64, bw: f64, duty: f64, burst: f64) -> Fingerprint {
        Fingerprint {
            duty_cycle: Some(duty),
            burst_length_s: Some(burst),
            ..Fingerprint::new(f, bw)
        }
    }

    /// The two rows of the user's 2026-09-16 staging scene, as measured.
    fn staging_pair() -> (RowEvidence, RowEvidence) {
        let mut a = row(99_814_800.0, 377_500.0);
        a.fingerprint = Some(wfm(99_813_751.0, 379_559.7, 0.4952, 0.6821));
        let mut b = row(99_815_100.0, 377_600.0);
        b.emitter_id = eid(2);
        b.fingerprint = Some(wfm(99_813_258.8, 380_477.3, 0.3720, 0.3648));
        (a, b)
    }

    /// T-250, the user's own 99.8 MHz scene: one 377 kHz station seen over 94–399 s and again over
    /// 471–530 s became **two** inventory rows 492 Hz apart. Their bands overlap essentially
    /// exactly, and nothing about the emission differed — only the burst length each observation
    /// window happened to measure (0.68 s vs 0.37 s, 1.86x apart), which the guard was reading as
    /// evidence that these were two different signals.
    #[test]
    fn t250_one_station_seen_in_two_disjoint_windows_is_not_told_apart_by_its_burst_length() {
        let (mut a, mut b) = staging_pair();
        a.spans = vec![(94_124_800_000, 399_494_399_999)];
        b.spans = vec![(471_025_066_666, 530_355_200_000)];
        assert!(
            bands_compete(a.freq(), b.freq()),
            "the two bands overlap almost exactly, so the rows do compete"
        );
        assert_eq!(
            distinguishing_evidence(&a, &b, &Tolerances::default()),
            None,
            "two disjoint observation windows of one station measure different duty and burst \
             figures; that is not evidence of two emissions"
        );
    }

    /// The narrowing that keeps T-250 honest. While the two rows were watched over the **same**
    /// window their duty cycle and burst length *are* comparable, and they still tell apart two
    /// emissions sharing a channel and a bandwidth — the ISM case, where a chatty sensor and a
    /// continuous carrier differ in nothing else.
    #[test]
    fn t250_duty_and_burst_still_separate_two_emissions_watched_over_the_same_window() {
        let (mut a, mut b) = staging_pair();
        a.spans = vec![(0, 500_000_000_000)];
        b.spans = vec![(100_000_000_000, 400_000_000_000)];
        assert_eq!(
            distinguishing_evidence(&a, &b, &Tolerances::default()),
            Some("fingerprint distance beyond tolerance"),
        );
    }

    #[test]
    fn presence_is_required_while_the_source_is_there() {
        let source = [(0i64, 10_000_000_000i64)];
        assert!(present_only_with(
            &[(1_000_000_000, 5_000_000_000)],
            &source
        ));
        // Seen long after the source went away: an independent emitter.
        assert!(!present_only_with(
            &[(50_000_000_000, 60_000_000_000)],
            &source
        ));
    }

    /// T-302: an image is a property of **one receive chain**. The arithmetic that attributes a
    /// mirror within one front end must claim nothing when the source was measured on another —
    /// a signal arriving at device B's antenna was never in device A's mixer.
    #[test]
    fn an_image_is_never_mirrored_across_two_front_ends() {
        let los = [lo_on("hackrf:A", 100.8e6)];
        let source = |dev: &str| source_on(dev, 9, 101.3e6, 180e3, -18.0);
        // One front end: the mirror of 101.3 MHz about 100.8 MHz lands on the row and is offered.
        let same = predict_artifacts(100.3e6, 180e3, -48.0, &[source("hackrf:A")], &los);
        assert_eq!(same.first().map(|p| p.kind), Some(ArtifactKind::Image));
        // Identical geometry, source seen only on the other front end: nothing is claimed.
        assert!(
            predict_artifacts(100.3e6, 180e3, -48.0, &[source("hackrf:B")], &los).is_empty(),
            "device A's LO can never mirror a signal that only device B saw"
        );
    }

    /// T-302: harmonics and intermod products come from the same non-linearity, so they are
    /// confined to one chain too.
    #[test]
    fn a_harmonic_and_an_intermod_product_are_confined_to_one_chain() {
        // Harmonic: 2 x 50.0 MHz = 100.0 MHz, at twice the source's width, 30 dB down.
        let los = [lo_on("hackrf:A", 60.0e6)];
        let h = |dev: &str| {
            predict_artifacts(
                100.0e6,
                360e3,
                -48.0,
                &[source_on(dev, 3, 50.0e6, 180e3, -18.0)],
                &los,
            )
        };
        assert_eq!(
            h("hackrf:A").first().map(|p| p.kind),
            Some(ArtifactKind::Harmonic)
        );
        assert!(
            h("hackrf:B").is_empty(),
            "device A does not generate harmonics of a signal it never received"
        );

        // Intermod: 2 f1 - f2 needs both tones in the same mixer as the product.
        let los = [lo_on("hackrf:A", 100.2e6)];
        let s1 = source_on("hackrf:A", 1, 100.0e6, 180e3, -20.0);
        let both_here = [s1.clone(), source_on("hackrf:A", 2, 100.4e6, 180e3, -22.0)];
        let one_elsewhere = [s1, source_on("hackrf:B", 2, 100.4e6, 180e3, -22.0)];
        assert_eq!(
            predict_artifacts(99.6e6, 540e3, -55.0, &both_here, &los)
                .first()
                .map(|p| p.kind),
            Some(ArtifactKind::Intermod)
        );
        assert!(
            predict_artifacts(99.6e6, 540e3, -55.0, &one_elsewhere, &los).is_empty(),
            "two tones that never met in one mixer cannot have produced this row"
        );
    }

    /// T-302: one device with a switched antenna bank has several RF paths, so the port is part of
    /// the key — but only when both sides recorded one, so a front end that never switches
    /// antennas keeps attributing its own images exactly as before.
    #[test]
    fn a_port_is_part_of_the_chain_only_when_both_sides_recorded_one() {
        let chain = |port: Option<&str>| ReceiveChain {
            device_id: "hackrf:A".into(),
            antenna_port: port.map(Into::into),
        };
        let fires = |src: Option<&str>, tuned: Option<&str>| {
            let source = ArtifactSource {
                emitter_id: eid(9),
                f_center_hz: 101.3e6,
                bandwidth_hz: 180e3,
                level_dbfs: -18.0,
                chains: vec![chain(src)],
            };
            let los = [TunedLo {
                chain: chain(tuned),
                lo_hz: 100.8e6,
            }];
            !predict_artifacts(100.3e6, 180e3, -48.0, &[source], &los).is_empty()
        };
        assert!(fires(Some("A1"), Some("A1")), "one path sees its own image");
        assert!(
            !fires(Some("A1"), Some("A2")),
            "a signal entering another port was not in the mixer while this one was selected"
        );
        assert!(fires(None, None), "no port recorded either side: unchanged");
        assert!(
            fires(Some("A1"), None),
            "an unrecorded port is unknown, not a different path"
        );
    }

    /// How much of a captured band the geometric predictor is willing to claim — the *prior width*
    /// of the rule, before the presence, flag and guard tests. Modelled on the 2026-09-15 capture:
    /// 99.6–102.0 MHz, one tune at 100.8 MHz, 32 confirmed sources (the cap) 30 dB above the
    /// target. If this is not small, "artifact" means nothing.
    #[test]
    fn artifact_predictions_claim_only_a_small_share_of_the_captured_band() {
        let sources: Vec<ArtifactSource> = (0..32u8)
            .map(|k| source_on("A", k + 1, 99.65e6 + f64::from(k) * 73e3, 180e3, -18.0))
            .collect();
        let los = [lo_on("A", 100.8e6)];
        let step = 2_000.0;
        let mut covered = [0usize; 2];
        for (i, measured_bw) in [12.5e3, 180e3].into_iter().enumerate() {
            let mut f = 99.6e6;
            while f <= 102.0e6 {
                if !predict_artifacts(f, measured_bw, -48.0, &sources, &los).is_empty() {
                    covered[i] += 1;
                }
                f += step;
            }
        }
        // The capture as it actually is: the two strong WFM stations, not 32 overlapping ones.
        let real: Vec<ArtifactSource> = [99.75e6, 101.45e6]
            .into_iter()
            .enumerate()
            .map(|(k, f)| source_on("A", k as u8 + 1, f, 180e3, -18.0))
            .collect();
        let mut real_covered = 0usize;
        let mut f = 99.6e6;
        while f <= 102.0e6 {
            if !predict_artifacts(f, 180e3, -48.0, &real, &los).is_empty() {
                real_covered += 1;
            }
            f += step;
        }
        let mhz = |n: usize| n as f64 * step / 1e6;
        println!(
            "COVERAGE 32 dense sources: narrow(12.5 kHz) {:.3} MHz, wide(180 kHz) {:.3} MHz; \
             the capture's 2 stations, wide: {:.3} MHz — of 2.400 MHz. Mechanisms firing on one \
             100.300 MHz box: narrow {}, wide {}",
            mhz(covered[0]),
            mhz(covered[1]),
            mhz(real_covered),
            predict_artifacts(100.3e6, 12.5e3, -48.0, &sources, &los).len(),
            predict_artifacts(100.3e6, 180e3, -48.0, &sources, &los).len(),
        );
        assert!(
            mhz(covered[0]) < 0.25,
            "a narrow box against 32 sources: {:.3} MHz of 2.400 claimed",
            mhz(covered[0])
        );
        assert!(
            mhz(real_covered) < 0.25,
            "a WFM-width box against the capture's two stations: {:.3} MHz of 2.400 claimed",
            mhz(real_covered)
        );
    }
}
