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
//!    geometric test — frequency arithmetic plus relative level — and needs no decode. It is the
//!    same "artifact of the receiver, not the air" idea as the DC twin rule (ADR-0012 §2.6).
//!
//! **Nothing here ever deletes or mutates a row.** A relationship is an append-only claim keyed by
//! emitter, carrying its reasoning, and revocable when evidence changes (`active = 0`). The losing
//! candidate keeps its id, detections, tracks, links and history, so later evidence can revive it.
//! That is the exploration-first rule: a relationship is ranked evidence, never truth, and never an
//! automatic delete.
//!
//! **The guard is what makes this safe.** Band overlap alone only makes two rows *compete*; any
//! distinguishing evidence blocks a merge or suppression ([`distinguishing_evidence`]), in order:
//! two different decoded identities, then a fingerprint distance beyond [`Tolerances`], then
//! separated −3 dB extents. Two genuinely distinct adjacent stations therefore stay two entries.

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

/// Centre tolerance as a fraction of the wider of the measured and predicted bandwidths.
pub const ARTIFACT_CENTER_BW_FRACTION: f64 = 0.25;

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
    /// Decoded transmitter identity, when it holds one.
    pub identity: Option<DecodedIdentity>,
    /// Stored fingerprint, when it has one.
    pub fingerprint: Option<Fingerprint>,
    /// Distinct tuning centres (LO) the linked detections were measured under, Hz.
    pub tuned_lo_hz: Vec<f64>,
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

    /// The provisional rank proxy (see [`rank_score`]).
    pub fn rank(&self) -> f64 {
        rank_score(self.snr_db, self.duty_cycle, self.suspect_fraction)
    }
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

/// The provisional duplicate-rank proxy: **SNR × duty × trust**, explicitly *not* the evidence-bits
/// ladder (ADR-0015 §11: bits are MAUTO work). Higher is stronger.
///
/// - SNR: the amplitude ratio against [`RANK_SNR_REF_DB`], so 10 dB scores 1.0 and every 20 dB is
///   a factor of 10. An unmeasured SNR scores 1.0 (neutral).
/// - Duty: the measured duty cycle, floored at [`RANK_DUTY_FLOOR`]; unmeasured is
///   [`RANK_DUTY_UNKNOWN`].
/// - Trust: `1 − RANK_SUSPECT_WEIGHT × suspect_fraction`, floored at [`RANK_TRUST_FLOOR`].
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
/// 2. a [`Fingerprint::compare`] distance beyond `tol`;
/// 3. −3 dB extents that do not overlap, with centres separated by more than the summed
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
    if let (Some(x), Some(y)) = (&a.fingerprint, &b.fingerprint) {
        // Every feature **except the centre**: bandwidth, family, symbol rate, deviation, period,
        // duty cycle, burst length and hop set. Where the box sits is the overlap question, not
        // distinguishing evidence — including the centre term here would make the guard reject
        // exactly the offset duplicates of one station that T-219 exists to collapse (two readings
        // 60 kHz apart on a 180 kHz station are one emission, and the centre tolerance is 45 kHz).
        let mut aligned = y.clone();
        aligned.f_center_hz = x.f_center_hz;
        if !x.compare(&aligned, tol).within {
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArtifactSource {
    /// The source row.
    pub emitter_id: EmitterId,
    /// Its measured centre, Hz.
    pub f_center_hz: f64,
    /// Its measured bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Its latest peak level, dBFS.
    pub level_dbfs: f64,
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

/// Centre tolerance for an artifact match: the wider of the measured and predicted bandwidths
/// scaled by [`ARTIFACT_CENTER_BW_FRACTION`], floored at [`ARTIFACT_CENTER_MIN_HZ`].
fn artifact_tolerance_hz(measured_bw_hz: f64, predicted_bw_hz: f64) -> f64 {
    (ARTIFACT_CENTER_BW_FRACTION * measured_bw_hz.max(predicted_bw_hz).max(0.0))
        .max(ARTIFACT_CENTER_MIN_HZ)
}

/// True when the level difference is consistent with a receiver artifact: the candidate is at
/// least [`ARTIFACT_MIN_SUPPRESSION_DB`] below its source, and not so far below that the
/// coincidence carries no information ([`ARTIFACT_MAX_SUPPRESSION_DB`]).
fn level_consistent(suppression_db: f64) -> bool {
    suppression_db.is_finite()
        && (ARTIFACT_MIN_SUPPRESSION_DB..=ARTIFACT_MAX_SUPPRESSION_DB).contains(&suppression_db)
}

/// Geometric artifact prediction (C40): every image / harmonic / intermod coincidence between
/// `target` and the strong confirmed `sources`, using the tuning centres in `tuned_lo_hz`, that
/// passes both the frequency and the level test. Best first (lowest order, then smallest error).
///
/// This is arithmetic only. The caller still has to apply the **presence** test — an artifact is
/// there only while its source is — and the guard, before recording a claim.
pub fn predict_artifacts(
    target_f_center_hz: f64,
    target_bandwidth_hz: f64,
    target_level_dbfs: f64,
    sources: &[ArtifactSource],
    tuned_lo_hz: &[f64],
) -> Vec<ArtifactPrediction> {
    let mut out: Vec<ArtifactPrediction> = Vec::new();
    if !target_f_center_hz.is_finite() || !target_level_dbfs.is_finite() {
        return out;
    }
    let mut push = |p: ArtifactPrediction| {
        // A prediction that lands on the source itself says nothing: that is the duplicate rule's
        // business, not the artifact rule's.
        if (p.predicted_hz - target_f_center_hz).abs() <= p.tolerance_hz
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
        // Image: the mirror of the source about the tuning centre.
        for &lo in tuned_lo_hz {
            if !lo.is_finite() {
                continue;
            }
            let predicted = 2.0 * lo - s.f_center_hz;
            if predicted <= 0.0 {
                continue;
            }
            // The source sitting at its own LO has no distinct mirror.
            if (predicted - s.f_center_hz).abs() <= ARTIFACT_CENTER_MIN_HZ {
                continue;
            }
            let tol = artifact_tolerance_hz(target_bandwidth_hz, s.bandwidth_hz);
            push(ArtifactPrediction {
                source: s.emitter_id,
                second_source: None,
                kind: ArtifactKind::Image,
                order: 1,
                a: 1,
                b: 0,
                sign: 1,
                lo_hz: Some(lo),
                predicted_hz: predicted,
                predicted_bandwidth_hz: s.bandwidth_hz,
                error_hz: target_f_center_hz - predicted,
                tolerance_hz: tol,
                suppression_db: suppression,
            });
        }
        // Harmonics.
        for n in 2..=HARMONIC_MAX_ORDER {
            let predicted = f64::from(n) * s.f_center_hz;
            let bw = f64::from(n) * s.bandwidth_hz;
            let tol = artifact_tolerance_hz(target_bandwidth_hz, bw);
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
                        let tol = artifact_tolerance_hz(target_bandwidth_hz, bw);
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
            identity: None,
            fingerprint: None,
            tuned_lo_hz: Vec::new(),
            spans: vec![(0, 1_000_000_000)],
            count: 1,
            first_seen_ns: 0,
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
        let source = ArtifactSource {
            emitter_id: eid(9),
            f_center_hz: 101.3e6,
            bandwidth_hz: 180e3,
            level_dbfs: -18.0,
        };
        // Tuned at 100.8 MHz, the mirror of 101.3 MHz lands at 100.3 MHz.
        let hits = predict_artifacts(100.3e6, 180e3, -48.0, &[source], &[100.8e6]);
        let top = hits.first().expect("image predicted");
        assert_eq!(top.kind, ArtifactKind::Image);
        assert!(top.error_hz.abs() < 1.0, "{}", top.error_hz);
        assert!((top.suppression_db - 30.0).abs() < 1e-9);
        // A real adjacent station 200 kHz from the source fits no mechanism.
        assert!(predict_artifacts(101.5e6, 180e3, -20.0, &[source], &[100.8e6]).is_empty());
    }

    #[test]
    fn an_artifact_as_strong_as_its_source_is_not_claimed() {
        let source = ArtifactSource {
            emitter_id: eid(9),
            f_center_hz: 101.3e6,
            bandwidth_hz: 180e3,
            level_dbfs: -18.0,
        };
        assert!(predict_artifacts(100.3e6, 180e3, -20.0, &[source], &[100.8e6]).is_empty());
    }

    #[test]
    fn a_third_order_product_of_two_strong_sources_is_predicted() {
        let s1 = ArtifactSource {
            emitter_id: eid(1),
            f_center_hz: 100.0e6,
            bandwidth_hz: 180e3,
            level_dbfs: -20.0,
        };
        let s2 = ArtifactSource {
            emitter_id: eid(2),
            f_center_hz: 100.4e6,
            bandwidth_hz: 180e3,
            level_dbfs: -22.0,
        };
        // 2 f1 - f2 = 99.6 MHz.
        let hits = predict_artifacts(99.6e6, 180e3, -55.0, &[s1, s2], &[]);
        let top = hits.first().expect("intermod predicted");
        assert_eq!(top.kind, ArtifactKind::Intermod);
        assert_eq!((top.a, top.b, top.sign), (2, 1, -1));
        assert_eq!(top.order, 3);
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
}
