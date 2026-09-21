//! The inventory seam (C27): where track and decode results go to the signal inventory.
//!
//! The pipeline persists Tracks (hk-detect `TrackBatch`) and the rows the chains' record writers
//! create (`hk_demod::write_session`, `write_framed_bursts`, plugin `Ingest`, which resolve
//! decoded identities through the repository). Clustering tracks into emitters is T-018's
//! `Repository::record_sighting`. [`TrackInventory`] (the default) calls it for closed tracks
//! and hop sets.
//!
//! T-039 ([`crate::family`]) adds the family step:
//! - A closed track whose occupancy maps to a service family carries that family as a
//!   Classification.
//! - Every emitter a sighting or a chain ([`Inventory::chain_emitter`]) touches gets its ranked
//!   explanations and its known status from the top one ([`explain_emitter`], bundled
//!   47 CFR 2.106 band table).
//!
//! **Capture name (T-034 replay dedup, T-037b).** The detection writer passes the run's stable
//! capture name ([`Inventory::capture_name`]) before the first event. [`TrackInventory`] then
//! offers track and hop-set sightings as re-measurements (`record_sighting_measured` with producer
//! [`TRACK_PRODUCER`] and that capture), so replaying the same IQ again does not count its tracks
//! twice, while two different captures with the same timestamps (synthetic scenes all start at
//! the same instant) stay separate.
//!
//! **Candidates and confirmed (T-078).** Every emitter enters the inventory as a `candidate`.
//! After each sighting or chain write, [`TrackInventory`] reviews the candidate against its
//! [`ConfirmPolicy`] and confirms it (author `auto`, actor [`CONFIRM_RULE`], the evidence as the
//! reason) only on strong, unambiguous, blind evidence:
//! - **Decoded identity:** the emitter holds a transmitter identity (RDS PI, ADS-B ICAO, …) carried
//!   by at least `min_valid_decodes` CRC-valid decodes. Structural identities
//!   (`structural_schemes`, e.g. the blind framer's `other:hk-framing` signature) are not
//!   transmitter identities and do not count.
//! - **Continuous and trusted:** a track with at least `min_on_air_s` on air, duty cycle at least
//!   `min_duty_cycle`, at most `max_suspect_fraction` suspect members (spur, image, IMD, clipping)
//!   and at least `min_confirmed_detections` trust-confirmed detections. **T-403:** weighed while
//!   the track is still open as well as when it closes, under two further clauses an open life must
//!   also satisfy — no suspect members at all, and a width of several analysis bins, which a CW
//!   receiver line cannot have.
//! - **Verified emission (T-398):** a demodulation in `verified_modes` whose subcarrier loop
//!   *locked* — for WFM, the 19 kHz stereo pilot tracked by the PLL to within
//!   `pilot_tolerance_hz`, at `min_lock_quality` or better, inside `verified_bandwidth_hz` of
//!   occupied bandwidth.
//!
//! The third route exists because the first two left a permanently-on emitter with **no live route
//! at all**: its "continuous and trusted" evidence used to be weighed only when its track *closed*,
//! so a station that never stops transmitting and carries no decodable identity stayed a candidate
//! until the track idled out (`idle_timeout_s`, 60 s) — tens of seconds after a human, or the
//! demodulator itself, could see what it was. It is a short-circuit on *positive* evidence rather
//! than a relaxation of the other two: a locked pilot is a coherent subcarrier at a standardised
//! offset, which a noise shelf, an intermodulation product or a skirt fragment does not have
//! however long it is watched. Absent the lock the route does not fire and the other two decide
//! exactly as before.
//!
//! Intermittent, weak or suspect emitters stay candidates until a user promotes them. User deletion
//! and the re-detection rule are the repository's (`hk_model` lifecycle).
//!
//! **Live offers (T-109).** An open channel track whose fate is settled (the tracker's
//! `live_offers_into`: no hop link or set, not an in-band fragment) is offered before it closes.
//! When that offer created the entry and the track then ends with no sighting of its own (closes
//! as a fragment or hop-set member, merges, or its channel joins a forming hop set), the entry is
//! retracted ([`RETRACT_RULE`], `Repository::retract_provisional_emitter`) — only while it is still
//! an untouched candidate that nothing else observed, linked, merged, confirmed or promoted.
//!
//! **One entry per physical emitter (T-082).** A chain's decoder output of an emission the tracker
//! also followed (RDS PI on a WFM track, the blind framer's signature on an FSK sensor track) would
//! otherwise be a second entry. After every sighting and chain write, [`TrackInventory`] merges
//! the entry with its same-emission partners (`Repository::same_emission_partners`: overlapping
//! centre or refined centre, overlapping time, hop compatibility; rules in `hk_model::cluster`)
//! when both came from this run (the same capture), before explaining and reviewing the survivor.
//! Entries carrying a channel-sharing transmitter identity (ADS-B ICAO, …: many transmitters per
//! channel) are never linked to a channel entry; structural identities (`structural_schemes`)
//! are.
//!
//! Another policy plugs in through [`Inventory`] without touching the composition. Reads go
//! through `Repository::query_inventory` only (identities gated).

use std::collections::{HashMap, HashSet, VecDeque};

use hk_context::{BandTable, Region as BandRegion};
use hk_detect::TrackEvent;
use hk_detect::track::TrackSummary;
use hk_detect::track::inventory::{hop_set_sighting, track_sighting};
use hk_model::{
    DemodulationId, EmitterId, EmitterLink, IdentityScheme, LifecycleAuthor, LifecycleState,
    LinkTarget, MeasurementKey, RETUNE_MIN_CENTRES, RETUNE_RULE, RepoError, Repository,
    RetuneTolerance, Sighting, Timestamp, Tolerances, TrackId,
};
use serde::{Deserialize, Serialize};

use crate::family::{explain_emitter, track_family};

/// Producer name of track and hop-set sightings in a [`MeasurementKey`].
pub const TRACK_PRODUCER: &str = "hk-track";

/// Actor of automatic confirmations (rule id and version) in the lifecycle history.
pub const CONFIRM_RULE: &str = "hk-pipeline/confirm@1";

/// Actor of automatic retractions of provisional live entries (T-109) in the lifecycle history.
pub const RETRACT_RULE: &str = "hk-pipeline/retract@1";

/// T-219: actor recorded on overlap-resolution claims (a Confirmed entry suppressing an
/// overlapping candidate, the weaker of a duplicate group, a receiver artifact attributed to its
/// source). Every claim is append-only and reversible; nothing is ever deleted.
pub const OVERLAP_RULE: &str = "hk-pipeline/overlap@1";

/// Reason of same-emission merges (T-082) in the merge record.
pub const SAME_EMISSION_REASON: &str =
    "same emission: track and decoder entries of one emitter (hk-pipeline/link@1)";

/// Entries of the current run remembered for same-emission linking.
const RUN_MEMORY: usize = 8192;

/// T-416: declined measurements held per track while it waits for an entry.
const MAX_AWAITING_PER_TRACK: usize = 8;

/// T-416: files a declined chain measurement against `emitter`. A link and nothing else — no
/// sighting, no count, no classification, no lifecycle change.
fn attach_measurement(
    repo: &mut Repository,
    emitter: EmitterId,
    demod: DemodulationId,
    at: Timestamp,
) -> Result<(), RepoError> {
    repo.link_emitter(&EmitterLink {
        emitter_id: emitter,
        target: LinkTarget::Demodulation(demod),
        linked_at: at,
    })
}

/// Receives inventory-relevant results, under the repository lock.
pub trait Inventory: Send {
    /// The run's stable capture name (content-derived, identical on every replay of the same
    /// IQ), given once before the first event.
    fn capture_name(&mut self, _name: &str) {}

    /// A tracker event whose Track row and links are already stored (closes, hop sets).
    fn track_event(
        &mut self,
        _repo: &mut Repository,
        _event: &TrackEvent,
    ) -> Result<(), RepoError> {
        Ok(())
    }

    /// A chain attached for `track` wrote rows under `emitter`.
    fn chain_emitter(
        &mut self,
        _repo: &mut Repository,
        _track: Option<TrackId>,
        _emitter: EmitterId,
    ) -> Result<(), RepoError> {
        Ok(())
    }

    /// T-416: a chain measured `track`'s emission and wrote Demodulation `demod` **without
    /// promoting anything** — a probe that declined the window it measured
    /// (`hk_demod::write_declined`). Attaches it to the track's entry, so the refusal is findable
    /// from the thing it is about.
    ///
    /// **It creates nothing**, exactly as [`Self::live_trust`] creates nothing: a refusal may be
    /// filed against an entry something else made, never conjure one. But a chain attaches on the
    /// *tracker's* confirmation and probes about a second into an emission, which is routinely
    /// before the inventory has offered that track a row at all — so a measurement that arrives
    /// before the entry is **held until the track is bound** rather than dropped. Dropping it is
    /// the defect this exists to fix: a refusal nothing can find reads as never having looked.
    fn chain_measurement(
        &mut self,
        _repo: &mut Repository,
        _track: Option<TrackId>,
        _demod: DemodulationId,
        _at: Timestamp,
    ) -> Result<(), RepoError> {
        Ok(())
    }

    /// An open, confirmed channel track whose Track row is stored, offered before it closes
    /// (T-109: a channel that never idles out would otherwise enter the inventory only at stop).
    fn live_track(
        &mut self,
        _repo: &mut Repository,
        _summary: &TrackSummary,
    ) -> Result<(), RepoError> {
        Ok(())
    }

    /// T-403: an open track the live offer would **not** make an entry for, because it is one long
    /// burst — a continuous carrier — re-weighed against the confirmation rule.
    ///
    /// **It creates nothing.** Where [`Self::live_track`] offers a sighting and may bring an entry
    /// into being, this only re-reads the rule for an entry something else already made; a track
    /// with no [`Self::emitter_of_track`] binding is skipped. That is T-388's gate, for the same
    /// reason: the strict offer predicate excludes exactly the continuous carriers a broadcast band
    /// is full of, and reusing it here would leave the signals this exists for undecided.
    ///
    /// `measuring` is whether a chain currently holds this emission
    /// (`chains::EmissionClaims::measuring`). The live continuous route yields to it: see
    /// [`ConfirmPolicy::decide`].
    fn live_trust(
        &mut self,
        _repo: &mut Repository,
        _summary: &TrackSummary,
        _measuring: bool,
    ) -> Result<(), RepoError> {
        Ok(())
    }

    /// The emitter whose inventory row `track`'s observations are recorded against, when this
    /// inventory has given it one (T-388).
    ///
    /// The live presence push ([`crate::presence`]) addresses an emitter — that is what the client
    /// holds a row and a box for — while the tracker knows only tracks, so something has to join
    /// the two, and this is the only seam that sees both. **A track with no answer here is not
    /// published**, which is what keeps the push from naming a box that does not exist.
    ///
    /// Read-only and cheap: an answer already worked out when the row was written, never a query.
    fn emitter_of_track(&self, _track: TrackId) -> Option<EmitterId> {
        None
    }
}

/// Leaves the inventory to the chains' record writers (no track clustering).
#[derive(Debug, Default)]
pub struct NullInventory;

impl Inventory for NullInventory {}

/// Thresholds of the automatic confirmation rule (module docs). Deserialisable so a run
/// configuration can carry it; missing fields take the defaults.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfirmPolicy {
    /// Run the rule at all (false: every emitter stays a candidate until promoted).
    pub enabled: bool,
    /// Confirm on a decoded transmitter identity.
    pub identity: bool,
    /// CRC-valid decodes carrying the identity needed. Default 1.
    pub min_valid_decodes: u64,
    /// Identity schemes that describe structure, not a transmitter. Default `other:hk-framing`.
    pub structural_schemes: Vec<String>,
    /// Confirm on one continuous, trusted track.
    pub continuous: bool,
    /// Time on air of the track, s. Default 2.
    pub min_on_air_s: f64,
    /// Duty cycle of the track (on air / observed). Default 0.8.
    pub min_duty_cycle: f64,
    /// Largest share of suspect member detections. Default 0.5 (the suspect-artifact tag rule).
    pub max_suspect_fraction: f64,
    /// Trust-confirmed member detections needed. Default 1.
    pub min_confirmed_detections: u64,
    /// T-403: weigh [`Self::continuous`] on a track that is **still open**, under the extra clauses
    /// below. False leaves route B close-only, as it was before T-403.
    pub live_continuous: bool,
    /// T-403: narrowest an open track may be and still take the live continuous route, in analysis
    /// bins of the detector's own resolution. Default 8.
    ///
    /// **This is the clause that keeps "continuous" from meaning "confirm anything that stays on".**
    /// A receiver-generated line — a reference harmonic, an LO relative, a clock harmonic, a comb
    /// tooth — is CW, so the only width it can *measure* is the analysis window's: the tracker
    /// floors a member's width at one bin (`width = obw.max(bin_hz)`), and 99 % of a Hann-windowed
    /// tone's energy lies inside its 4-bin main lobe (the first sidelobe is 31 dB down), so an
    /// OBW99 of a pure tone cannot reach past it.
    ///
    /// **Measured, not assumed, and measured across level** — `latency::
    /// t403_a_continuous_unmodulated_line_never_confirms_live` generates the line and its receiver
    /// artefacts at four powers spanning 28 dB and reads the widths the detector gives them: 2 to 4
    /// bins throughout, **flat in level**, because the OBW99 of a windowed tone is a property of the
    /// window rather than of the tone's strength. So a line cannot widen its way past this clause by
    /// being loud. Eight bins is twice the widest a tone can measure; a modulated emission is well
    /// past it — the WFM scene measures 66 kHz at 4687.5 Hz bins, 14 of them.
    ///
    /// A genuinely narrowband emission (a CW beacon, a slow data burst) fails this clause and takes
    /// the **closed** route exactly as it did before: the fallback is the old rule, not a lower bar.
    pub min_live_bandwidth_bins: f64,
    /// T-403: largest share of suspect member detections on the **live** route. Default 0, i.e.
    /// none at all.
    ///
    /// Deliberately stricter than [`Self::max_suspect_fraction`]: every RF-domain artefact verdict
    /// the detector can reach without the scheduler's help — ref-harmonic, DC, clock-harmonic,
    /// comb, LO-relative, spur-map, image, IMD, compression, clipping — vetoes the fast route
    /// outright, while a closed track keeps the tolerance it has always had. The live route is
    /// route B's evidence read earlier, and every one of its clauses is at least as strict.
    pub max_live_suspect_fraction: f64,
    /// T-398: confirm on a **verified emission** — a demodulated mode whose subcarrier loop
    /// actually locked. See [`ConfirmPolicy::decide`] for why this is positive evidence and not a
    /// lowered threshold.
    pub verified: bool,
    /// Demodulator modes a lock may confirm. Default `wfm`.
    pub verified_modes: Vec<String>,
    /// Mean `cos e` of the locked loop needed, 0–1. Default 0.6.
    pub min_lock_quality: f64,
    /// Occupied bandwidth the verified mode implies, Hz. Default 50–400 kHz: an outer sanity bound
    /// on the demodulator's own call, whose lower edge is `hk_demod`'s `wfm_pilot_min_obw_hz` —
    /// the width at which that crate already accepts "pilot present, therefore WFM".
    ///
    /// Deliberately *not* `family::WIDEBAND_FM_OBW_HZ` (106–400 kHz). That window describes real
    /// program-modulated broadcast FM, and occupancy alone has to carry the whole decision there.
    /// Here the pilot lock carries it, and the width only has to rule out something narrowband
    /// that happens to contain a 19 kHz component.
    pub verified_bandwidth_hz: [f64; 2],
    /// Nominal subcarrier the loop locks to, Hz. Default 19 000 (the FM stereo pilot).
    pub pilot_nominal_hz: f64,
    /// Largest offset of the measured subcarrier from [`Self::pilot_nominal_hz`], Hz. Default 100
    /// — the pilot PLL's own pull-in range, so anything outside it never locked here at all.
    pub pilot_tolerance_hz: f64,
}

impl Default for ConfirmPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            identity: true,
            min_valid_decodes: 1,
            structural_schemes: vec!["other:hk-framing".into()],
            continuous: true,
            min_on_air_s: 2.0,
            min_duty_cycle: 0.8,
            max_suspect_fraction: hk_detect::track::inventory::SUSPECT_FRACTION,
            min_confirmed_detections: 1,
            live_continuous: true,
            min_live_bandwidth_bins: 8.0,
            max_live_suspect_fraction: 0.0,
            verified: true,
            verified_modes: vec!["wfm".into()],
            min_lock_quality: 0.6,
            verified_bandwidth_hz: [50e3, 400e3],
            pilot_nominal_hz: 19_000.0,
            pilot_tolerance_hz: 100.0,
        }
    }
}

/// T-398: a demodulation whose subcarrier loop locked, as the confirmation rule reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedEmission {
    /// Demodulator mode, e.g. `wfm`.
    pub mode: String,
    /// Mean `cos e` while the loop was locked, 0–1. `None` when it never locked.
    pub lock_quality: Option<f64>,
    /// Measured subcarrier frequency, Hz. `None` when the loop never locked (T-037b).
    pub pilot_hz: Option<f64>,
    /// Occupied bandwidth the session measured, Hz.
    pub bandwidth_hz: Option<f64>,
}

impl VerifiedEmission {
    /// From a stored demodulation session.
    pub fn of(d: &hk_model::decode::Demodulation) -> Self {
        Self {
            mode: d.mode.clone(),
            lock_quality: d.lock_quality,
            pilot_hz: d.params.pilot_hz,
            bandwidth_hz: d.params.bandwidth_hz,
        }
    }
}

/// Trust and occupancy of a track, closed or still open.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackTrust {
    /// On-air time, s.
    pub on_air_s: f64,
    /// On air / observed.
    pub duty_cycle: Option<f64>,
    /// Share of suspect member detections.
    pub suspect_fraction: f64,
    /// Trust-confirmed member detections.
    pub confirmed_detections: u64,
    /// T-403: whether the track has closed. `false` is a life still being lived, which route B may
    /// only weigh under the extra clauses of [`ConfirmPolicy::decide`].
    pub closed: bool,
    /// T-403: the track's representative bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// T-403: the analysis resolution its members were measured at, Hz
    /// ([`TrackSummary::bin_hz`]): the scale [`Self::bandwidth_hz`] has to be read against.
    pub bin_hz: f64,
    /// T-403: a chain is **measuring this emission right now**
    /// (`chains::EmissionClaims::measuring`), so a stronger, more specific answer about it is on
    /// its way. Only a live review sets it; a closed track is past the question.
    pub measuring: bool,
}

impl TrackTrust {
    /// From a tracker summary.
    pub fn of(summary: &TrackSummary) -> Self {
        Self {
            on_air_s: summary.on_time_s,
            duty_cycle: (summary.observed_s > 0.0)
                .then(|| (summary.on_time_s / summary.observed_s).min(1.0)),
            suspect_fraction: summary.suspect_fraction,
            confirmed_detections: summary.confirmed_detections,
            closed: summary.closed.is_some(),
            bandwidth_hz: summary.track.bandwidth_hz,
            bin_hz: summary.bin_hz,
            measuring: false,
        }
    }

    /// With [`Self::measuring`] set: a chain holds this emission as the review runs.
    pub fn measured_by_a_chain(self, measuring: bool) -> Self {
        Self { measuring, ..self }
    }

    /// The track's bandwidth in analysis bins, or `None` when the resolution is unusable (zero,
    /// negative or not a number) — a missing measurement refuses, never passes.
    pub fn bandwidth_bins(&self) -> Option<f64> {
        (self.bin_hz.is_finite() && self.bin_hz > 0.0 && self.bandwidth_hz.is_finite())
            .then(|| self.bandwidth_hz / self.bin_hz)
    }
}

/// What the rule looks at for one candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfirmEvidence {
    /// Identity scheme and CRC-valid decodes carrying it (`Repository::identity_decode_evidence`).
    pub identity: Option<(IdentityScheme, u64)>,
    /// The track just closed for this emitter, if the review follows a track close.
    pub track: Option<TrackTrust>,
    /// T-398: the emitter's latest demodulation session, if a chain has run one
    /// ([`Repository::latest_linked_demodulation_for_emitter`]).
    pub verified: Option<VerifiedEmission>,
}

/// T-403: which rule confirmed an entry, strongest first — the order the reasons rank in.
///
/// An entry is confirmed by whichever rule is satisfied **first**, and the rules are not satisfied
/// at the same time: a continuous emission's occupancy evidence is complete about two seconds in,
/// while a demodulator's pilot lock needs a window of signal and lands later by an amount that
/// depends on host load. So the first reason recorded is not always the best one available, and
/// this is what lets a later, stronger one replace it
/// (`Repository::restate_emitter_lifecycle`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfirmRoute {
    /// **B — continuous and trusted.** Real evidence, but weaker *in kind*: it says "something
    /// modulated has been on air steadily", not what the emission is.
    Continuous,
    /// **C — verified emission.** A demodulated mode whose subcarrier loop locked: "this is an FM
    /// broadcast station, and here is its pilot".
    Verified,
    /// **A — decoded identity.** A CRC-valid decode carrying the transmitter's own identifier.
    Identity,
}

impl ConfirmPolicy {
    /// The confirmation reason, or `None` (stay a candidate).
    ///
    /// Three disjunctive routes, strongest first.
    ///
    /// **A — decoded identity.** A CRC-valid decode carrying a transmitter identity.
    ///
    /// **B — continuous and trusted.** A track that was on air long enough, at a high enough duty
    /// cycle, with few enough suspect members and enough trust-confirmed detections.
    ///
    /// **B-live (T-403).** The same four clauses, weighed while the track is still open. Route B
    /// used to demand the close, and the reason given was that a duty cycle is only meaningful over
    /// a finished life — but a station that never stops transmitting *has* no close until the
    /// `idle_timeout_s`, so route B's own thresholds were met about 2 s in and the decision waited
    /// out the recording. An emitter with neither a pilot to lock (route C) nor an identity to
    /// decode (route A) had no live route at all, and confirmed at whatever second the recording
    /// happened to end.
    ///
    /// The danger route C did not have is that **continuity and duty cycle are exactly what a
    /// strong receiver artefact also has**: this receiver's own reference harmonic is on air for
    /// ever at duty cycle 1.00. So the live form is not route B with the waiting removed. It adds
    /// two clauses, both strictly tighter than the closed route's:
    ///
    /// - **No suspect members at all** ([`Self::max_live_suspect_fraction`], default 0) rather than
    ///   up to half. Every RF artefact verdict the detector reaches on its own — ref-harmonic, DC,
    ///   clock-harmonic, comb, LO-relative, spur-map, image, IMD, compression, clipping — vetoes it.
    /// - **A modulated width** ([`Self::min_live_bandwidth_bins`]): the track must be many analysis
    ///   bins wide. A receiver line is CW and cannot be — measured across 28 dB of level, a tone and
    ///   the artefacts it produces span 2 to 4 bins and do not widen with strength, because the
    ///   OBW99 of a windowed tone belongs to the window.
    ///
    /// When either fails, nothing fires and the *closed* route decides exactly as it did before —
    /// a fallback to the old rule, never a lowered bar. The measurement is written either way.
    ///
    /// **C — verified emission (T-398).** A demodulation whose subcarrier loop *locked*. This is
    /// the fast route, and it is deliberately **not** route B with lower numbers: it confirms on
    /// positive physical evidence rather than on less of the same evidence. A 19 kHz pilot that a
    /// PLL tracked to within its pull-in range, with a mean `cos e` above
    /// [`Self::min_lock_quality`], inside an emission of WFM occupied bandwidth, is a coherent
    /// subcarrier at a standardised offset — something a noise shelf, an intermodulation product
    /// or a skirt fragment cannot produce however long you watch it. (The stationary 75 kHz,
    /// 4.6 dB noise shelf of T-316 has duty cycle 1.0 and would satisfy route B's occupancy for
    /// ever; it has no pilot, so route C never sees it.) Absent the lock the route simply does not
    /// fire and A and B decide as before — nothing is confirmed on a family the measurement did
    /// not support.
    ///
    /// Why it matters beyond latency: routes A and B are the *only* routes a permanently-on
    /// emitter had, and B cannot fire until the track closes — so a continuous station with no
    /// decodable identity (no RDS, weak RDS, or a chain that lost the admission race) stayed a
    /// candidate until its track idled out, tens of seconds later. That is the ~40 s the user saw.
    pub fn decide(&self, ev: &ConfirmEvidence) -> Option<String> {
        self.decide_route(ev).map(|(_, reason)| reason)
    }

    /// [`Self::decide`] with the route that produced the reason, so a later, stronger route can
    /// replace an earlier, weaker one.
    pub fn decide_route(&self, ev: &ConfirmEvidence) -> Option<(ConfirmRoute, String)> {
        if !self.enabled {
            return None;
        }
        if self.identity
            && let Some((scheme, n)) = &ev.identity
            && !self.structural_schemes.contains(&scheme.as_string())
            && *n >= self.min_valid_decodes.max(1)
        {
            return Some((
                ConfirmRoute::Identity,
                format!("decoded identity ({scheme}) carried by {n} CRC-valid decode(s)"),
            ));
        }
        // T-403: route C before route B. When both are in hand the *specific* measurement is what
        // gets recorded — a pilot lock says "this is an FM broadcast station", where continuity
        // and width say only "something modulated has been on air steadily".
        if let Some(reason) = self.verified_reason(ev.verified.as_ref()) {
            return Some((ConfirmRoute::Verified, reason));
        }
        self.continuous_reason(ev.track, ev.verified.as_ref())
            .map(|reason| (ConfirmRoute::Continuous, reason))
    }

    /// The route a recorded reason came from, for comparing an entry's current explanation against
    /// a newly available one. Reasons are this rule's own strings; anything else is unrecognised
    /// and never outranks what is there.
    pub fn route_of(reason: &str) -> Option<ConfirmRoute> {
        if reason.starts_with("decoded identity") {
            Some(ConfirmRoute::Identity)
        } else if reason.starts_with("verified ") {
            Some(ConfirmRoute::Verified)
        } else if reason.starts_with("continuous and trusted") {
            Some(ConfirmRoute::Continuous)
        } else {
            None
        }
    }

    /// Routes B and B-live: the reason a continuous, trusted track confirms, or `None`.
    ///
    /// The four accumulation clauses are common to both and are the ones this rule has always
    /// had. What the track's state changes is what else must hold, not how much of them is enough.
    fn continuous_reason(
        &self,
        track: Option<TrackTrust>,
        v: Option<&VerifiedEmission>,
    ) -> Option<String> {
        if !self.continuous {
            return None;
        }
        let tr = track?;
        let duty = tr.duty_cycle?;
        if tr.on_air_s.is_nan()
            || tr.on_air_s < self.min_on_air_s
            || duty.is_nan()
            || duty < self.min_duty_cycle
            || tr.suspect_fraction.is_nan()
            || tr.suspect_fraction > self.max_suspect_fraction
            || tr.confirmed_detections < self.min_confirmed_detections
        {
            return None;
        }
        let live = if tr.closed {
            String::new()
        } else {
            // T-403: the extra clauses an unfinished life must also satisfy. Each is a positive
            // measurement that must be present; a missing or NaN one refuses.
            if !self.live_continuous {
                return None;
            }
            // T-403: **the fallback does not pre-empt a measurement in flight.** While a chain
            // actually holds this emission (`chains::EmissionClaims::measuring`) the specific
            // answer is being made, so route B stays out of the way until the demodulation exists.
            // Bounded by the chain's own life rather than by a timer: a chain that rejects the mode
            // releases the emission and route B decides on the next review.
            //
            // This is a courtesy, not the guarantee. Waiting cannot *be* the guarantee — a chain
            // under load reports later in the capture, so any wait long enough to be safe on an
            // idle host is too short on a busy one, which is exactly how the route race was found.
            // What makes the recorded reason independent of arrival is that it can strengthen
            // afterwards (see [`Self::decide_route`] and `Repository::restate_emitter_lifecycle`).
            if tr.measuring && v.is_none() {
                return None;
            }
            if tr.suspect_fraction.is_nan() || tr.suspect_fraction > self.max_live_suspect_fraction
            {
                return None;
            }
            let bins = tr.bandwidth_bins()?;
            if bins.is_nan() || bins < self.min_live_bandwidth_bins {
                return None;
            }
            format!(
                ", {:.0} kHz wide ({bins:.0} analysis bins), still on air",
                tr.bandwidth_hz / 1e3
            )
        };
        Some(format!(
            "continuous and trusted: {:.1} s on air, duty cycle {duty:.2}, {:.0} % suspect, \
             {} trust-confirmed detection(s){live}",
            tr.on_air_s,
            tr.suspect_fraction * 100.0,
            tr.confirmed_detections
        ))
    }

    /// Route C: the reason a locked demodulation confirms, or `None`. Every clause is a positive
    /// measurement that must be present — a missing one is a refusal, never a pass.
    fn verified_reason(&self, v: Option<&VerifiedEmission>) -> Option<String> {
        if !self.verified {
            return None;
        }
        let v = v?;
        if !self.verified_modes.contains(&v.mode) {
            return None;
        }
        // `lock_quality` and `pilot_hz` are both `Some` only once the loop actually locked
        // (`PilotReport`, T-037b): a pilot-shaped bump that never held phase leaves them `None`.
        // Every comparison is written so a NaN measurement refuses rather than passes.
        let q = v.lock_quality?;
        if q.is_nan() || q < self.min_lock_quality {
            return None;
        }
        let pilot = v.pilot_hz?;
        let offset = (pilot - self.pilot_nominal_hz).abs();
        if offset.is_nan() || offset > self.pilot_tolerance_hz {
            return None;
        }
        let bw = v.bandwidth_hz?;
        let [lo, hi] = self.verified_bandwidth_hz;
        if bw.is_nan() || bw < lo || bw > hi {
            return None;
        }
        Some(format!(
            "verified {} emission: {:.0} Hz subcarrier locked (quality {q:.2}) in {:.0} kHz \
             occupied bandwidth",
            v.mode,
            pilot,
            bw / 1e3,
        ))
    }
}

/// T-018 clustering for closed tracks and hop sets, with T-039 family explanations and priors,
/// and T-078 automatic confirmation.
pub struct TrackInventory {
    table: Option<BandTable>,
    capture: Option<String>,
    policy: ConfirmPolicy,
    /// Entries this run's sightings and chains reached, oldest first (bounded).
    run: VecDeque<EmitterId>,
    run_set: HashSet<EmitterId>,
    /// T-242: the `(sighting count, fingerprint observations)` each entry was last characterised
    /// at, so a re-offer that measured nothing new costs one comparison instead of a match and a
    /// clustering pass. Bounded with [`Self::run`].
    characterised: HashMap<EmitterId, (u64, u64)>,
    /// T-109: open tracks whose live offer created their entry; removed when the track closes,
    /// merges or joins a hop set (every open track ends in one of those).
    provisional: HashMap<TrackId, EmitterId>,
    /// T-388: which emitter each open track's row is, for [`Inventory::emitter_of_track`]. Written
    /// wherever a track's row is resolved — a live offer and a chain write, the two seams that
    /// reach an entry — and removed when the track ends, so a closed track publishes nothing
    /// further. Bounded like [`Self::run`].
    bound: HashMap<TrackId, EmitterId>,
    /// T-416: declined chain measurements written for a track that had no entry yet, waiting for
    /// [`Self::bind`]. Bounded like [`Self::bound`]: past the cap the map is cleared, which costs
    /// a refusal its link, never a wrong one.
    awaiting: HashMap<TrackId, Vec<(DemodulationId, Timestamp)>>,
    /// T-598: the number of distinct tuning centres the last retune pass was decided at, and the
    /// rows already resolved at it. A cross-centre verdict can only change when a **new centre**
    /// appears, so the pass runs at most once per row per centre count — never once per sighting.
    /// Bounded like [`Self::run`]: past the cap the set is cleared, which costs a repeat pass,
    /// never a wrong verdict.
    retune_centres: usize,
    retuned: HashSet<EmitterId>,
    /// Provisional entries retracted because their track's end yielded no sighting (T-109).
    pub retracted: u64,
    /// Sightings recorded.
    pub sightings: u64,
    /// Emitters created by sightings.
    pub created: u64,
    /// Candidates confirmed by the rule.
    pub confirmed: u64,
    /// T-403: confirmations whose recorded reason was replaced by a stronger route's.
    pub restated: u64,
    /// Same-emission merges (T-082).
    pub merged: u64,
    /// T-219: candidates recorded as suppressed by an overlapping Confirmed entry.
    pub suppressed: u64,
    /// T-219: candidates recorded as the weaker of an overlapping duplicate group.
    pub duplicates: u64,
    /// T-219: candidates attributed to the source whose receiver artifact they are.
    pub artifacts: u64,
    /// T-598: sightings related to another sighting of the same LO-relative receiver artefact,
    /// so the inventory shows the artefact once instead of once per tuning centre.
    pub retune_siblings: u64,
    /// T-598: retune verdicts recorded on stored detections (`absolute`, `lo-locked`, `image`).
    pub retune_verdicts: u64,
    /// T-369: overlapping regions the re-analysis could not resolve. Nothing was merged and
    /// nothing was hidden; the verdict records what blocked it. Bounded per row by
    /// `hk_model::relate::REGION_MAX_ROUNDS`.
    pub contested: u64,
    /// T-242: features snapshots written (one per re-measurement).
    pub characterisations: u64,
    /// T-242: signature matches computed and offered to the match log.
    pub matches: u64,
    /// T-242: characterisations that placed the emitter in a cluster of unknowns.
    pub clustered: u64,
}

impl Default for TrackInventory {
    fn default() -> Self {
        Self::with_policy(ConfirmPolicy::default())
    }
}

impl TrackInventory {
    /// The default inventory with another confirmation policy.
    pub fn with_policy(policy: ConfirmPolicy) -> Self {
        Self {
            table: BandTable::bundled(BandRegion::Us).ok(),
            capture: None,
            policy,
            run: VecDeque::new(),
            run_set: HashSet::new(),
            characterised: HashMap::new(),
            provisional: HashMap::new(),
            bound: HashMap::new(),
            awaiting: HashMap::new(),
            retune_centres: 0,
            retuned: HashSet::new(),
            retracted: 0,
            sightings: 0,
            created: 0,
            confirmed: 0,
            restated: 0,
            merged: 0,
            retune_siblings: 0,
            retune_verdicts: 0,
            suppressed: 0,
            duplicates: 0,
            artifacts: 0,
            contested: 0,
            characterisations: 0,
            matches: 0,
            clustered: 0,
        }
    }

    /// T-388: records which emitter `track`'s row is, for [`Inventory::emitter_of_track`]. Bounded
    /// the way [`Self::run`] is: past the cap the map is cleared rather than grown, and the
    /// bindings are re-learnt by the next offer or chain write. A forgotten binding costs a box a
    /// few hundred milliseconds of extension, never a wrong one.
    fn bind(
        &mut self,
        repo: &mut Repository,
        track: TrackId,
        emitter: EmitterId,
    ) -> Result<(), RepoError> {
        if self.bound.len() >= RUN_MEMORY && !self.bound.contains_key(&track) {
            self.bound.clear();
        }
        self.bound.insert(track, emitter);
        // T-416: this is the moment a track first has somewhere to file things, so any declined
        // measurement that arrived before it is attached here.
        for (demod, at) in self.awaiting.remove(&track).unwrap_or_default() {
            attach_measurement(repo, emitter, demod, at)?;
        }
        Ok(())
    }

    /// Whether `id` may be linked to another entry of its emission: not when it carries a
    /// channel-sharing transmitter identity (structural identities may).
    fn linkable(&self, repo: &Repository, id: EmitterId) -> Result<bool, RepoError> {
        Ok(match repo.identity_decode_evidence(id)? {
            Some((scheme, _)) => {
                !scheme.shares_channel()
                    || self.policy.structural_schemes.contains(&scheme.as_string())
            }
            None => true,
        })
    }

    fn remember(&mut self, id: EmitterId) {
        if self.run_set.insert(id) {
            self.run.push_back(id);
            if self.run.len() > RUN_MEMORY
                && let Some(old) = self.run.pop_front()
            {
                self.run_set.remove(&old);
                self.characterised.remove(&old);
            }
        }
    }

    /// T-082: merges `emitter` with this run's same-emission partners; returns the live survivor.
    fn link(&mut self, repo: &mut Repository, emitter: EmitterId) -> Result<EmitterId, RepoError> {
        let mut id = repo.live_emitter_id(emitter)?;
        let tol = Tolerances::default();
        for (partner, _) in repo.same_emission_partners(id, &tol)? {
            if !self.run_set.contains(&partner) {
                continue;
            }
            if !self.linkable(repo, id)? {
                break;
            }
            if !self.linkable(repo, partner)? {
                continue;
            }
            let t = repo
                .emitter(id)?
                .last_seen
                .max(repo.emitter(partner)?.last_seen);
            match repo.merge_same_emission(id, partner, t, SAME_EMISSION_REASON, &tol) {
                Ok(Some(m)) => {
                    id = m.into;
                    self.merged += 1;
                    // T-335: the survivor's evidence just changed by absorbing `from`, even when
                    // T-082's overlap discount left its count unmoved and the absorbed partner
                    // carried no fingerprint of its own to fold (so `characterise`'s other half,
                    // fingerprint observations, is unmoved too). `characterise` cannot see a
                    // merge in either of those numbers, so drop its cached (count, fingerprint
                    // observations) pair here — the merge itself is the signal — and the next
                    // `characterise` call below reads as a first sighting of `id` rather than a
                    // false repeat.
                    self.characterised.remove(&id);
                }
                Ok(None) | Err(RepoError::IdentityConflict { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        self.remember(id);
        Ok(id)
    }

    /// The confirmation policy in force.
    pub fn policy(&self) -> &ConfirmPolicy {
        &self.policy
    }

    /// T-219: resolves the overlapping inventory rows around `emitter` — a Confirmed entry
    /// suppresses candidates overlapping its band, the remaining overlapping candidates compete on
    /// the SNR × duty × trust proxy, and a candidate landing on a predicted image / harmonic /
    /// intermod frequency of a strong confirmed emitter is attributed to it. Every claim is an
    /// append-only row carrying its reasoning ([`OVERLAP_RULE`]); no row is ever mutated or
    /// deleted, so later evidence revives a superseded one. Rules: `hk_model::relate`.
    ///
    /// **T-369: and then the overlap itself is treated as an error signal.** Boxes that still
    /// overlap in time *and* frequency after all that ranking are proof the analysis is wrong, so
    /// the region is re-analysed against the measured detection bands behind it rather than left
    /// as competing boxes. It resolves to one emission (the rest defer) or is recorded contested
    /// with nothing merged and nothing hidden; `hk_model::relate::REGION_MAX_ROUNDS` bounds it.
    fn resolve_overlaps(
        &mut self,
        repo: &mut Repository,
        emitter: EmitterId,
    ) -> Result<(), RepoError> {
        let id = repo.live_emitter_id(emitter)?;
        let t = repo.emitter(id)?.last_seen;
        let out = repo.resolve_overlaps(id, OVERLAP_RULE, t, &Tolerances::default())?;
        self.suppressed += out.suppressed.len() as u64;
        self.duplicates += out.duplicates.len() as u64;
        self.artifacts += out.artifacts.len() as u64;
        self.contested += out.contested.len() as u64;
        Ok(())
    }

    /// **T-598: the cross-centre retune verdict, persisted** (`hk_model::repo::retune`).
    ///
    /// A real emission keeps its absolute frequency when the front end is retuned; a receiver
    /// artefact — DC/LO leakage, an internal spur at a fixed IF offset, an IQ image — moves with
    /// the LO. T-586 measured that slope from the LO each detection's own provenance records and
    /// proved it separates the two classes, then threw the verdict away: the inventory went on
    /// listing one moving spur as N emitters at N absolute frequencies, which is what the user
    /// saw on the air. This writes it down — the verdict on each detection, and a
    /// `retune-sibling-of` relationship between the sightings of one artefact, so the inventory
    /// shows **one** artefact and not one per centre. Nothing is deleted and every claim is
    /// revocable.
    ///
    /// **What bounds it.** A verdict can only change when a *new tuning centre* appears, so the
    /// pass runs at most once per row per distinct-centre count (and not at all below two
    /// centres, where no slope can be measured). That is the difference between a bounded
    /// cross-centre review and a query per sighting.
    fn resolve_retune(
        &mut self,
        repo: &mut Repository,
        emitter: EmitterId,
    ) -> Result<(), RepoError> {
        let centres = repo.tune_centre_count()?;
        if centres < RETUNE_MIN_CENTRES {
            return Ok(());
        }
        if centres != self.retune_centres {
            self.retune_centres = centres;
            self.retuned.clear();
        }
        let id = repo.live_emitter_id(emitter)?;
        if !self.retuned.insert(id) {
            return Ok(());
        }
        if self.retuned.len() > RUN_MEMORY {
            self.retuned.clear();
        }
        let t = repo.emitter(id)?.last_seen;
        let out = repo.resolve_retune(id, RETUNE_RULE, t, &RetuneTolerance::default())?;
        self.retune_siblings += out.deferred() as u64;
        self.retune_verdicts += out.detections_marked as u64;
        Ok(())
    }

    /// Reviews a candidate against the policy; confirms it when the evidence holds.
    ///
    /// `at` is the capture instant the evidence reaches, when the caller knows it — the end of the
    /// observation the decision was taken on. Without it the emitter's `last_seen` is used, which
    /// is where the sightings have reached and can lag the evidence: a live review (T-403) weighs a
    /// track that has been on air for seconds longer than the last sighting recorded, and stamping
    /// the change with `last_seen` would claim the decision was available before the evidence for
    /// it existed. A confirmation's timestamp is read as "when in the signal the system decided",
    /// so it must never be earlier than the measurement it was decided on.
    fn review(
        &mut self,
        repo: &mut Repository,
        emitter: EmitterId,
        track: Option<TrackTrust>,
        at: Option<Timestamp>,
    ) -> Result<(), RepoError> {
        if !self.policy.enabled {
            return Ok(());
        }
        let id = repo.live_emitter_id(emitter)?;
        // T-403: a Confirmed entry is still reviewed, for its *reason* only. See below.
        let state = repo.emitter_lifecycle_state(id)?;
        let confirmed = match state {
            LifecycleState::Candidate => false,
            LifecycleState::Confirmed => true,
            _ => return Ok(()),
        };
        let evidence = ConfirmEvidence {
            identity: repo.identity_decode_evidence(id)?,
            track,
            // T-398: only read when a route C confirmation is actually possible, so the common
            // review (a live offer for an emitter no chain has demodulated) costs no extra query.
            verified: if self.policy.verified {
                repo.latest_linked_demodulation_for_emitter(id)?
                    .as_ref()
                    .map(VerifiedEmission::of)
            } else {
                None
            },
        };
        let Some((route, reason)) = self.policy.decide_route(&evidence) else {
            return Ok(());
        };
        let last_seen = repo.emitter(id)?.last_seen;
        let t = at.map_or(last_seen, |a| a.max(last_seen));
        if confirmed {
            // **The reason strengthens; the confirmation time does not.** An entry is confirmed by
            // whichever route is satisfied first, and the routes are not satisfied at the same
            // time — occupancy evidence is complete about two seconds into a continuous emission,
            // a pilot lock lands when the demodulator has had its window, and how much later that
            // is depends on how loaded the host is. Left alone, the recorded explanation is
            // whichever route won a race: the same capture reads "continuous and trusted" on a
            // busy machine and "verified wfm emission" on an idle one, with nothing about the
            // signal different. Recording the stronger reason when it arrives is what makes the
            // explanation a function of the evidence rather than of arrival order.
            //
            // Only upwards, and only over this rule's own reasons: an unrecognised one (a user's
            // promotion) is never overwritten.
            let held = repo
                .emitter_lifecycle_history(id)?
                .pop()
                .and_then(|c| ConfirmPolicy::route_of(&c.reason));
            if held.is_some_and(|h| route > h)
                && repo
                    .restate_emitter_lifecycle(id, LifecycleAuthor::Auto, CONFIRM_RULE, &reason, t)?
                    .is_some()
            {
                self.restated += 1;
            }
            return Ok(());
        }
        if repo
            .change_emitter_lifecycle(
                id,
                LifecycleState::Confirmed,
                LifecycleAuthor::Auto,
                CONFIRM_RULE,
                &reason,
                t,
            )?
            .is_some()
        {
            self.confirmed += 1;
        }
        Ok(())
    }
}

impl Inventory for TrackInventory {
    fn capture_name(&mut self, name: &str) {
        self.capture = Some(name.to_owned());
    }

    fn track_event(&mut self, repo: &mut Repository, event: &TrackEvent) -> Result<(), RepoError> {
        match event {
            TrackEvent::Closed(summary) => {
                let track = summary.track.id;
                // T-416: the last chance to file a refusal this track never got an entry for. The
                // close's own offer resolves one, and the binding goes with the track, so a
                // measurement still waiting here is attached there or nowhere.
                let awaiting = self.awaiting.remove(&track).unwrap_or_default();
                // T-388: a closed track publishes no further extension. Its box stops at the last
                // end the tracker measured, and the close's own sighting is what the next poll
                // serves.
                self.bound.remove(&track);
                let provisional = self.provisional.remove(&track);
                match track_sighting(summary) {
                    Some(mut s) => {
                        s.classification = track_family(summary).classification(s.seen.end);
                        let (emitter, _) = self.offer(repo, &s, Some(TrackTrust::of(summary)))?;
                        for (demod, at) in awaiting {
                            attach_measurement(repo, emitter, demod, at)?;
                        }
                    }
                    // An in-band fragment or hop-set member after all: withdraw its live entry.
                    None => {
                        if let Some(emitter) = provisional {
                            self.retract(repo, track, emitter, summary.track.time.end)?;
                        }
                    }
                }
            }
            TrackEvent::HopSetFormed(h) => {
                // Channels offered before the set formed now belong to the set's entry.
                for &m in &h.members {
                    self.bound.remove(&m);
                    // T-416: the member's entry is being withdrawn in favour of the set's, so
                    // there is nothing left for a waiting refusal to be filed against.
                    self.awaiting.remove(&m);
                    if let Some(emitter) = self.provisional.remove(&m) {
                        self.retract(repo, m, emitter, h.time.end)?;
                    }
                }
                self.offer(repo, &hop_set_sighting(h), None)?;
            }
            TrackEvent::HopSetClosed(h) => {
                self.offer(repo, &hop_set_sighting(h), None)?;
            }
            TrackEvent::Merged { from, at, .. } => {
                self.bound.remove(from);
                self.awaiting.remove(from);
                if let Some(emitter) = self.provisional.remove(from) {
                    self.retract(repo, *from, emitter, *at)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn live_track(
        &mut self,
        repo: &mut Repository,
        summary: &TrackSummary,
    ) -> Result<(), RepoError> {
        if summary.closed.is_some() || summary.inband_fragment {
            return Ok(());
        }
        let Some(mut sighting) = track_sighting(summary) else {
            return Ok(());
        };
        sighting.classification = track_family(summary).classification(sighting.seen.end);
        // T-403: still `None` here, deliberately. This offer may *create* the entry, and T-109 can
        // withdraw it again when the track turns out to be a fragment, a hop-set member or a merge
        // — but only while it is an untouched candidate, so confirming on the offer that made it
        // would defeat the retraction. An open life is weighed instead through
        // [`Inventory::live_trust`], whose tracks are disjoint from these by construction and which
        // creates nothing.
        let (emitter, created) = self.offer(repo, &sighting, None)?;
        if created {
            self.provisional.insert(summary.track.id, emitter);
        }
        self.bind(repo, summary.track.id, emitter)?;
        Ok(())
    }

    fn chain_emitter(
        &mut self,
        repo: &mut Repository,
        track: Option<TrackId>,
        emitter: EmitterId,
    ) -> Result<(), RepoError> {
        let id = self.link(repo, emitter)?;
        // T-388: the other seam that resolves a track's row, and the one a continuous carrier
        // arrives by — a WFM station is one long burst, so the live offer above (which wants
        // several) never fires for it and its entry comes from the chain instead. Binding only the
        // offer would have left exactly the signals the bug was reported against unextended.
        if let Some(track) = track {
            self.bind(repo, track, id)?;
        }
        self.touch(repo, id, None)
    }

    fn chain_measurement(
        &mut self,
        repo: &mut Repository,
        track: Option<TrackId>,
        demod: DemodulationId,
        at: Timestamp,
    ) -> Result<(), RepoError> {
        let Some(track) = track else {
            return Ok(());
        };
        if let Some(emitter) = self.emitter_of_track(track) {
            return attach_measurement(repo, emitter, demod, at);
        }
        if self.awaiting.len() >= RUN_MEMORY && !self.awaiting.contains_key(&track) {
            self.awaiting.clear();
        }
        let queue = self.awaiting.entry(track).or_default();
        // One track's refusals are bounded too: a chain that keeps declining the same emission
        // says the same thing each time, and the entry only needs to be able to find it.
        if queue.len() < MAX_AWAITING_PER_TRACK {
            queue.push((demod, at));
        }
        Ok(())
    }

    fn live_trust(
        &mut self,
        repo: &mut Repository,
        summary: &TrackSummary,
        measuring: bool,
    ) -> Result<(), RepoError> {
        if summary.closed.is_some() || summary.inband_fragment {
            return Ok(());
        }
        // The gate: only a track this inventory has already given a row. Nothing is created, no
        // sighting is recorded, and a track nothing has claimed costs one hash lookup.
        let Some(emitter) = self.emitter_of_track(summary.track.id) else {
            return Ok(());
        };
        // The evidence reaches the end of the last burst the tracker measured, which is later than
        // the last sighting recorded against the entry; the change is stamped there, not earlier.
        self.review(
            repo,
            emitter,
            Some(TrackTrust::of(summary).measured_by_a_chain(measuring)),
            Some(summary.track.time.end),
        )
    }

    fn emitter_of_track(&self, track: TrackId) -> Option<EmitterId> {
        self.bound.get(&track).copied()
    }
}

impl TrackInventory {
    /// T-109: withdraws the entry `track`'s live offer created, now that the track ended with no
    /// sighting, unless anything else made it more than that offer (the repository's guard:
    /// linked or merged, confirmed or promoted, other observations).
    fn retract(
        &mut self,
        repo: &mut Repository,
        track: TrackId,
        emitter: EmitterId,
        t: Timestamp,
    ) -> Result<(), RepoError> {
        let reason = format!(
            "provisional entry of open track {track} withdrawn: the track ended as an in-band \
             fragment, hop-set member or merged track, with no sighting of its own"
        );
        let change = repo.retract_provisional_emitter(
            emitter,
            &LinkTarget::Track(track),
            RETRACT_RULE,
            &reason,
            t,
        )?;
        if change.is_some() {
            self.retracted += 1;
        }
        Ok(())
    }

    /// Records a track or hop-set sighting, links it and reviews its lifecycle; returns the
    /// recorded emitter and whether the sighting created it.
    fn offer(
        &mut self,
        repo: &mut Repository,
        sighting: &Sighting,
        trust: Option<TrackTrust>,
    ) -> Result<(EmitterId, bool), RepoError> {
        let sighting = sighting.clone();
        let r = match &self.capture {
            Some(capture) => {
                let key = MeasurementKey {
                    producer: TRACK_PRODUCER.into(),
                    capture: Some(capture.clone()),
                };
                repo.record_sighting_measured(&sighting, &key, None)?
            }
            None => repo.record_sighting(&sighting, None)?,
        };
        self.sightings += 1;
        self.created += u64::from(r.created);
        let id = self.link(repo, r.emitter_id)?;
        self.touch(repo, id, trust)?;
        Ok((r.emitter_id, r.created))
    }

    /// **The one place a touched entry is worked over** (T-242): ranked explanations and known
    /// status (T-039), then characterisation — the features snapshot, the C18 signature match
    /// (T-201) and the cluster of unknowns (T-202) — then the confirmation review (T-078) and
    /// overlap resolution (T-219).
    ///
    /// Both seams that reach an entry (a track or hop-set sighting, and a chain write) go through
    /// here, so a match and a cluster are written from the same measurement in one place rather
    /// than wired twice.
    fn touch(
        &mut self,
        repo: &mut Repository,
        id: EmitterId,
        trust: Option<TrackTrust>,
    ) -> Result<(), RepoError> {
        if let Some(table) = &self.table {
            explain_emitter(repo, table, id)?;
        }
        self.characterise(repo, id, trust)?;
        self.review(repo, id, trust, None)?;
        self.resolve_overlaps(repo, id)?;
        self.resolve_retune(repo, id)
    }

    /// T-242: aggregates what this entry has measured and asks the catalogue and the clusterer
    /// about it ([`crate::characterise`], which documents where this runs and what bounds it).
    ///
    /// Skipped when nothing new has been measured since it was last characterised: a live
    /// re-offer of an open track (T-109) is the same measurement again, and the match log is
    /// history of what was *said*, not of how often it was asked.
    ///
    /// "Nothing new" is the entry's `(count, fingerprint observations)` pair. It was `count`
    /// alone until T-336, when `count` stopped being a proxy for "a measurement arrived": a
    /// sighting over air another producer already counted adds no occurrence, so a chain's
    /// symbol rate and deviation would reach the fingerprint and never reach this seam. The
    /// fingerprint's observation counter moves once per folded measurement, which is exactly the
    /// bound this skip is documented to enforce.
    ///
    /// **T-335: a same-emission merge (T-082) is a third way evidence changes that this pair
    /// cannot see.** Its overlap discount can leave `count` unmoved (the absorbed span was
    /// already counted), and when the absorbed partner carried no fingerprint of its own — a
    /// decoder's identity-only sighting, [`hk_model::Sighting::decode`] — nothing folds into the
    /// survivor's fingerprint either, so `fingerprint observations` stays put too. Both halves of
    /// the key can be unmoved while the survivor just absorbed a partner's evidence (its
    /// classification, its identity). [`Self::link`] is what knows a merge happened, so it is
    /// what drops the cached key — the merge itself is the signal, not a third counter.
    fn characterise(
        &mut self,
        repo: &mut Repository,
        id: EmitterId,
        trust: Option<TrackTrust>,
    ) -> Result<(), RepoError> {
        let e = repo.emitter(id)?;
        let measured = (
            e.count,
            repo.emitter_fingerprint(id)?.map_or(0, |f| f.observations),
        );
        if self.characterised.get(&id) == Some(&measured) {
            return Ok(());
        }
        self.characterised.insert(id, measured);
        // A sighting whose detections were mostly suspect (spur, image, IMD, clipping) is folded
        // in as suspect, so nothing is ever minted from an all-suspect emitter (C18 card).
        let suspect = trust.is_some_and(|t| t.suspect_fraction > self.policy.max_suspect_fraction);
        let Some(out) = crate::characterise::characterise(repo, id, suspect, e.last_seen)? else {
            return Ok(());
        };
        self.characterisations += 1;
        self.matches += u64::from(out.signature_match.is_some());
        self.clustered += u64::from(out.cluster.is_some_and(|a| a.cluster_id.is_some()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_detect::track::CloseCause;
    use hk_model::signature::field;
    use hk_model::{
        Classification, Fingerprint, InventoryQuery, TimeRange, Timestamp, TimingFeatures, Track,
        TrackState,
    };

    fn channel_summary(
        id: TrackId,
        end_s: i64,
        bursts: u64,
        closed: Option<CloseCause>,
    ) -> TrackSummary {
        let t0 = Timestamp::from_unix_nanos(1_000_000_000);
        TrackSummary {
            track: Track {
                id,
                state: if closed.is_some() {
                    TrackState::Closed
                } else {
                    TrackState::Open
                },
                split_from: None,
                time: TimeRange::new(t0, Timestamp::from_unix_nanos(end_s * 1_000_000_000)),
                f_center_hz: 152.342e6,
                bandwidth_hz: 12e3,
                detection_count: bursts,
                timing: TimingFeatures::default(),
                updated_at: t0,
            },
            burst_count: bursts,
            on_time_s: 0.9 * bursts as f64,
            observed_s: bursts as f64,
            period: None,
            burst_length: None,
            inter_arrival_cv: None,
            segments: 0,
            hop_set: None,
            inband_fragment: false,
            suspect_fraction: 0.0,
            confirmed_detections: bursts,
            bin_hz: 1e3,
            next_burst_eta: None,
            closed,
        }
    }

    #[test]
    fn t109_live_channel_track_enters_the_inventory_before_close_without_double_counting() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        let id = TrackId::new();
        let rows = |repo: &Repository| {
            repo.query_inventory(&InventoryQuery::default())
                .unwrap()
                .entries
                .into_iter()
                .map(|e| (e.emitter.id, e.emitter.count))
                .collect::<Vec<_>>()
        };
        // A repeating channel still open after 5 bursts is catalogued now, not at stop.
        inv.live_track(&mut repo, &channel_summary(id, 6, 5, None))
            .unwrap();
        let first = rows(&repo);
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(first[0].1, 5);
        // Re-offers and the final close of the same track add only the new bursts, to one row.
        inv.live_track(&mut repo, &channel_summary(id, 11, 10, None))
            .unwrap();
        let closed = channel_summary(id, 13, 12, Some(CloseCause::Idle));
        inv.track_event(&mut repo, &TrackEvent::Closed(closed.clone()))
            .unwrap();
        let last = rows(&repo);
        assert_eq!(
            last,
            vec![(first[0].0, 12)],
            "one emitter, counted once per burst"
        );
        // A closed summary is never offered as live.
        let other = channel_summary(TrackId::new(), 13, 12, Some(CloseCause::Idle));
        inv.live_track(&mut repo, &other).unwrap();
        assert_eq!(rows(&repo).len(), 1);
    }

    /// T-416: a declined chain measurement written **before** the track has an inventory row is
    /// held and attached when one appears — and it creates nothing on its own.
    ///
    /// This is the ordering a live run actually has: a chain attaches on the *tracker's*
    /// confirmation and probes about a second into an emission, while the entry arrives later.
    /// Filing the refusal against "whatever entry exists right now" would have dropped it exactly
    /// when it matters, which is the shape of the defect — a refusal nothing can find is
    /// indistinguishable from never having looked.
    #[test]
    fn t416_a_refusal_written_before_the_entry_exists_is_attached_when_one_does() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        let track = TrackId::new();
        let demod = DemodulationId::new();
        let at = Timestamp::from_unix_nanos(2_000_000_000);

        // Nothing has entered this emission yet.
        inv.chain_measurement(&mut repo, Some(track), demod, at)
            .unwrap();
        assert!(
            listed(&repo).is_empty(),
            "[T-416] a refusal never conjures an inventory entry"
        );

        // The live offer makes one, and the held measurement is filed against it.
        inv.live_track(&mut repo, &channel_summary(track, 6, 5, None))
            .unwrap();
        let entries = listed(&repo);
        assert_eq!(entries.len(), 1, "{entries:?}");
        let links = repo.emitter_links(entries[0]).unwrap();
        assert!(
            links
                .iter()
                .any(|l| l.target == LinkTarget::Demodulation(demod)),
            "[T-416] the refusal is findable from the emission it is about: {links:?}"
        );

        // And a later one goes straight there, with nothing left waiting.
        let second = DemodulationId::new();
        inv.chain_measurement(&mut repo, Some(track), second, at)
            .unwrap();
        assert!(inv.awaiting.is_empty(), "nothing is still held");
        let links = repo.emitter_links(entries[0]).unwrap();
        assert!(
            links
                .iter()
                .any(|l| l.target == LinkTarget::Demodulation(second)),
            "{links:?}"
        );
    }

    /// And the close is the last chance: a track that never got a live entry still files its
    /// refusals against the entry its close resolves.
    #[test]
    fn t416_a_refusal_held_by_a_track_that_never_bound_is_filed_at_its_close() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        let track = TrackId::new();
        let demod = DemodulationId::new();
        inv.chain_measurement(
            &mut repo,
            Some(track),
            demod,
            Timestamp::from_unix_nanos(2_000_000_000),
        )
        .unwrap();
        let closed = channel_summary(track, 13, 12, Some(CloseCause::Idle));
        inv.track_event(&mut repo, &TrackEvent::Closed(closed))
            .unwrap();
        let entries = listed(&repo);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(
            repo.emitter_links(entries[0])
                .unwrap()
                .iter()
                .any(|l| l.target == LinkTarget::Demodulation(demod)),
            "[T-416] the binding goes with the track, so the close is where this must happen"
        );
        assert!(inv.awaiting.is_empty());
    }

    fn listed(repo: &Repository) -> Vec<EmitterId> {
        repo.query_inventory(&InventoryQuery::default())
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.emitter.id)
            .collect()
    }

    fn channel_at(
        f: f64,
        id: TrackId,
        end_s: i64,
        bursts: u64,
        closed: Option<CloseCause>,
    ) -> TrackSummary {
        let mut s = channel_summary(id, end_s, bursts, closed);
        s.track.f_center_hz = f;
        s
    }

    #[test]
    fn t109_live_entry_of_a_track_that_ends_as_an_inband_fragment_is_retracted_at_close() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        let id = TrackId::new();
        inv.live_track(&mut repo, &channel_summary(id, 6, 5, None))
            .unwrap();
        let rows = listed(&repo);
        assert_eq!(rows.len(), 1);
        // The host's continuous track outlives it: at close the flicker is a fragment.
        let mut closed = channel_summary(id, 9, 7, Some(CloseCause::Idle));
        closed.inband_fragment = true;
        inv.track_event(&mut repo, &TrackEvent::Closed(closed))
            .unwrap();
        assert!(listed(&repo).is_empty(), "no stale row");
        assert_eq!(inv.retracted, 1);
        assert_eq!(
            repo.emitter_lifecycle_state(rows[0]).unwrap(),
            LifecycleState::Deleted
        );
        let history = repo.emitter_lifecycle_history(rows[0]).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].author, LifecycleAuthor::Auto);
        assert_eq!(history[0].actor, RETRACT_RULE);
        // A summary already known to be a fragment is never offered live.
        let mut frag = channel_summary(TrackId::new(), 6, 5, None);
        frag.inband_fragment = true;
        inv.live_track(&mut repo, &frag).unwrap();
        assert!(listed(&repo).is_empty());
    }

    #[test]
    fn t109_slow_hopper_channels_offered_before_the_hop_set_formed_are_retracted() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        let chans = [152.30e6, 152.40e6, 152.50e6];
        let ids: Vec<TrackId> = chans.iter().map(|_| TrackId::new()).collect();
        for (&f, &id) in chans.iter().zip(&ids) {
            inv.live_track(&mut repo, &channel_at(f, id, 6, 4, None))
                .unwrap();
        }
        let channels = listed(&repo);
        assert_eq!(channels.len(), 3, "each slow channel was offered early");
        let set = TrackId::new();
        let h = hk_detect::track::HopSetSummary {
            id: set,
            channels_hz: chans.to_vec(),
            members: ids.clone(),
            raster_hz: Some(100e3),
            hop_rate_hz: Some(2.0),
            dwell_s: Some(0.4),
            hops: 12,
            time: TimeRange::new(
                Timestamp::from_unix_nanos(1_000_000_000),
                Timestamp::from_unix_nanos(7_000_000_000),
            ),
        };
        inv.track_event(&mut repo, &TrackEvent::HopSetFormed(h))
            .unwrap();
        let rows = listed(&repo);
        assert_eq!(rows.len(), 1, "only the hop set's entry: {rows:?}");
        assert!(!channels.contains(&rows[0]));
        assert_eq!(inv.retracted, 3);
        // The members' closes (hop-set members: no sighting) change nothing more.
        for (&f, &id) in chans.iter().zip(&ids) {
            let mut s = channel_at(f, id, 9, 6, Some(CloseCause::Idle));
            s.hop_set = Some(set);
            inv.track_event(&mut repo, &TrackEvent::Closed(s)).unwrap();
        }
        assert_eq!(listed(&repo), rows);
        assert_eq!(inv.retracted, 3);
    }

    #[test]
    fn t109_retraction_withdraws_merged_tracks_but_never_user_confirmed_or_shared_rows() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        let t = Timestamp::from_unix_nanos(9_000_000_000);
        // Merged into another track: its live row goes.
        let a = TrackId::new();
        inv.live_track(&mut repo, &channel_at(152.30e6, a, 6, 5, None))
            .unwrap();
        assert_eq!(listed(&repo).len(), 1);
        let merged = TrackEvent::Merged {
            from: a,
            into: TrackId::new(),
            at: t,
        };
        inv.track_event(&mut repo, &merged).unwrap();
        assert!(listed(&repo).is_empty());
        // Confirmed by a user: kept, whatever the track's end.
        let b = TrackId::new();
        inv.live_track(&mut repo, &channel_at(152.60e6, b, 6, 5, None))
            .unwrap();
        let user = listed(&repo);
        assert_eq!(user.len(), 1);
        repo.change_emitter_lifecycle(
            user[0],
            LifecycleState::Confirmed,
            LifecycleAuthor::User,
            "token:test",
            "looks real",
            t,
        )
        .unwrap();
        let mut frag = channel_at(152.60e6, b, 9, 7, Some(CloseCause::Idle));
        frag.inband_fragment = true;
        inv.track_event(&mut repo, &TrackEvent::Closed(frag))
            .unwrap();
        assert_eq!(listed(&repo), user);
        assert_eq!(
            repo.emitter_lifecycle_state(user[0]).unwrap(),
            LifecycleState::Confirmed
        );
        // Reached by another track's sighting too: kept.
        let c = TrackId::new();
        inv.live_track(&mut repo, &channel_at(152.90e6, c, 6, 5, None))
            .unwrap();
        let shared: Vec<EmitterId> = listed(&repo)
            .into_iter()
            .filter(|e| !user.contains(e))
            .collect();
        assert_eq!(shared.len(), 1);
        let other = channel_at(152.90e6, TrackId::new(), 8, 5, Some(CloseCause::Idle));
        inv.track_event(&mut repo, &TrackEvent::Closed(other))
            .unwrap();
        let after: Vec<EmitterId> = listed(&repo)
            .into_iter()
            .filter(|e| !user.contains(e))
            .collect();
        assert_eq!(after, shared, "the other track joined the live row");
        let mut frag = channel_at(152.90e6, c, 9, 7, Some(CloseCause::Idle));
        frag.inband_fragment = true;
        inv.track_event(&mut repo, &TrackEvent::Closed(frag))
            .unwrap();
        assert!(listed(&repo).contains(&shared[0]), "shared row kept");
        assert_eq!(inv.retracted, 1);
    }

    /// A closed track that satisfies route B.
    fn steady() -> TrackTrust {
        TrackTrust {
            on_air_s: 4.9,
            duty_cycle: Some(0.98),
            suspect_fraction: 0.0,
            confirmed_detections: 12,
            closed: true,
            bandwidth_hz: 150e3,
            bin_hz: 4687.5,
            measuring: false,
        }
    }

    /// The same evidence on a track that is **still open**, at a review where route B is allowed
    /// to decide: nothing is measuring it and it has already settled (T-403).
    fn steady_live() -> TrackTrust {
        TrackTrust {
            closed: false,
            ..steady()
        }
    }

    /// A locked 19 kHz pilot inside a WFM-width emission: route C's evidence.
    fn locked() -> VerifiedEmission {
        VerifiedEmission {
            mode: "wfm".into(),
            lock_quality: Some(0.98),
            pilot_hz: Some(19_000.0),
            bandwidth_hz: Some(180e3),
        }
    }

    /// T-403: **the specific route wins, and the fallback waits for it.**
    ///
    /// Route B's evidence — continuity, duty cycle, width — is complete about two seconds into a
    /// broadcast station. Route C's needs a demodulator to report a lock, which is one review
    /// interval later by construction. At the moment route B is first ready the two cases are
    /// *identical* to the rule: same width class, duty 1.00, no demodulation, no chain holding the
    /// emission. Nothing evaluated then can tell the station that will produce a pilot from the one
    /// that never will, so route B must not decide on that first look.
    #[test]
    fn t403_the_fallback_route_yields_to_the_specific_one() {
        let p = ConfirmPolicy::default();
        let ev = |track, verified| ConfirmEvidence {
            identity: None,
            track: Some(track),
            verified,
        };
        // Both routes in hand: the specific measurement is what gets recorded.
        let both = p
            .decide(&ev(steady_live(), Some(locked())))
            .expect("a locked pilot confirms");
        assert!(
            both.starts_with("verified"),
            "a pilot lock says 'this is an FM broadcast station'; continuity says only 'something              modulated has been on air'. The specific one is the reason: {both}"
        );
        // The same, on a closed track: ordering is not a live-only rule.
        let closed = p
            .decide(&ev(steady(), Some(locked())))
            .expect("still confirms");
        assert!(closed.starts_with("verified"), "{closed}");

        // The routes rank, and the ranking is what lets a later, stronger reason replace an
        // earlier, weaker one rather than the first arrival deciding for good.
        assert!(ConfirmRoute::Identity > ConfirmRoute::Verified);
        assert!(ConfirmRoute::Verified > ConfirmRoute::Continuous);
        assert_eq!(
            ConfirmPolicy::route_of(&both),
            Some(ConfirmRoute::Verified),
            "a recorded reason is readable back as the route that wrote it"
        );
        assert_eq!(
            ConfirmPolicy::route_of("promoted by the user"),
            None,
            "a reason this rule did not write is never outranked by it"
        );
        // While a chain holds the emission the answer is in flight, however long it takes.
        let measuring = TrackTrust {
            measuring: true,
            ..steady_live()
        };
        assert_eq!(p.decide(&ev(measuring, None)), None, "an answer is coming");
        // Once the chain has answered, route B decides on what it found: no lock, no route C, so
        // the continuous route fires rather than the emitter waiting for its track to close.
        let no_lock = VerifiedEmission {
            lock_quality: None,
            pilot_hz: None,
            ..locked()
        };
        let fell_back = p
            .decide(&ev(measuring, Some(no_lock)))
            .expect("a chain that found no lock does not block the fallback for ever");
        assert!(fell_back.starts_with("continuous"), "{fell_back}");
        // And a chain that never claims the emission at all does not block it either: the station
        // this ticket exists for is never demodulated in some scenes.
        assert!(
            p.decide(&ev(steady_live(), None))
                .is_some_and(|r| r.starts_with("continuous")),
            "nothing is measuring it, so the fallback decides"
        );
    }

    #[test]
    fn t078_policy_confirms_only_strong_unambiguous_evidence() {
        let p = ConfirmPolicy::default();
        let none = ConfirmEvidence {
            identity: None,
            track: None,
            verified: None,
        };
        assert_eq!(p.decide(&none), None);
        // A transmitter identity with a valid decode confirms; the framer's signature does not.
        let rds = ConfirmEvidence {
            identity: Some((IdentityScheme::RdsPi, 3)),
            track: None,
            verified: None,
        };
        assert!(p.decide(&rds).unwrap().contains("rds-pi"));
        let framing = ConfirmEvidence {
            identity: Some((IdentityScheme::Other("hk-framing".into()), 20)),
            track: None,
            verified: None,
        };
        assert_eq!(p.decide(&framing), None);
        let no_valid = ConfirmEvidence {
            identity: Some((IdentityScheme::RdsPi, 0)),
            track: None,
            verified: None,
        };
        assert_eq!(p.decide(&no_valid), None);
        // Continuous and trusted confirms; intermittent, short, suspect or unverified do not.
        let ev = |track| ConfirmEvidence {
            identity: None,
            track: Some(track),
            verified: None,
        };
        assert!(p.decide(&ev(steady())).unwrap().starts_with("continuous"));
        for (what, t) in [
            (
                "intermittent",
                TrackTrust {
                    duty_cycle: Some(0.15),
                    ..steady()
                },
            ),
            (
                "short",
                TrackTrust {
                    on_air_s: 0.5,
                    ..steady()
                },
            ),
            (
                "suspect",
                TrackTrust {
                    suspect_fraction: 0.9,
                    ..steady()
                },
            ),
            (
                "unverified",
                TrackTrust {
                    confirmed_detections: 0,
                    ..steady()
                },
            ),
            (
                "no duty",
                TrackTrust {
                    duty_cycle: None,
                    ..steady()
                },
            ),
        ] {
            assert_eq!(p.decide(&ev(t)), None, "{what}");
        }
        let off = ConfirmPolicy {
            enabled: false,
            ..ConfirmPolicy::default()
        };
        assert_eq!(off.decide(&rds), None);
        let parsed: ConfirmPolicy =
            serde_json::from_value(serde_json::json!({ "min_duty_cycle": 0.5 })).unwrap();
        assert_eq!(parsed.min_duty_cycle, 0.5);
        assert_eq!(parsed.min_on_air_s, 2.0);
    }

    /// T-403 route B-live. The evidence route B has always asked for, weighed while the track is
    /// still open — and the two clauses that keep "continuous" from meaning "confirm anything that
    /// stays on".
    ///
    /// The control is the one the ticket is about: **this receiver's own reference harmonic is
    /// continuous at duty cycle 1.00 for ever**, so every accumulation clause route B has is
    /// satisfied by it and always will be. It must not confirm live.
    #[test]
    fn t403_an_open_track_confirms_only_on_a_modulated_width_with_no_suspect_members() {
        let p = ConfirmPolicy::default();
        let ev = |track| ConfirmEvidence {
            identity: None,
            track: Some(track),
            verified: None,
        };
        // The case the ticket exists for: a station with no pilot and no identity, still on air.
        let reason = p
            .decide(&ev(steady_live()))
            .expect("an open, continuous, trusted, modulated track confirms");
        assert!(reason.starts_with("continuous"), "{reason}");
        assert!(
            reason.contains("still on air"),
            "the reason says the life was unfinished: {reason}"
        );
        assert!(
            reason.contains("32 analysis bins"),
            "and what made it an emission rather than a line: {reason}"
        );

        // A CW receiver line: on air for ever, duty 1.00, never a suspect flag raised against it
        // because no rule listed its frequency — and a handful of analysis bins wide, which is all
        // an unmodulated line can measure. Every accumulation clause route B has is satisfied.
        let line = TrackTrust {
            on_air_s: 600.0,
            duty_cycle: Some(1.0),
            suspect_fraction: 0.0,
            confirmed_detections: 600,
            closed: false,
            bandwidth_hz: 4687.5,
            bin_hz: 4687.5,
            measuring: false,
        };
        assert_eq!(
            p.decide(&ev(line)),
            None,
            "a continuous CW line must not confirm live however long it stays on"
        );
        // And it must not confirm by staying on longer, which is the failure mode a lowered
        // threshold would have had.
        assert_eq!(
            p.decide(&ev(TrackTrust {
                on_air_s: 86_400.0,
                confirmed_detections: 86_400,
                ..line
            })),
            None,
            "a day of it is still one bin wide"
        );
        // The same line once its track closes still takes the closed route, exactly as before
        // T-403: this ticket removed no protection, and added none, from the route that existed.
        assert!(
            p.decide(&ev(TrackTrust {
                closed: true,
                ..line
            }))
            .is_some(),
            "the closed route is unchanged, for better and worse"
        );

        // Each live-only clause, removed one at a time from a signal that otherwise confirms.
        for (what, t) in [
            (
                "one suspect member in ten",
                TrackTrust {
                    suspect_fraction: 0.1,
                    ..steady_live()
                },
            ),
            (
                // The widest a pure tone was measured at over 28 dB of level (the e2e control's
                // table): the clause has to refuse this, not merely the one-bin case.
                "four bins wide, a tone's main lobe",
                TrackTrust {
                    bandwidth_hz: 4.0 * 4687.5,
                    ..steady_live()
                },
            ),
            (
                "seven bins wide, just under the floor",
                TrackTrust {
                    bandwidth_hz: 7.0 * 4687.5,
                    ..steady_live()
                },
            ),
            (
                "no resolution measured",
                TrackTrust {
                    bin_hz: 0.0,
                    ..steady_live()
                },
            ),
            (
                "a NaN resolution",
                TrackTrust {
                    bin_hz: f64::NAN,
                    ..steady_live()
                },
            ),
            (
                "a NaN width",
                TrackTrust {
                    bandwidth_hz: f64::NAN,
                    ..steady_live()
                },
            ),
        ] {
            assert_eq!(p.decide(&ev(t)), None, "{what}");
            // …and the same evidence on a closed track decides as it always did.
            assert!(
                p.decide(&ev(TrackTrust { closed: true, ..t })).is_some(),
                "{what}: the fallback is the old rule, not a lowered bar"
            );
        }

        // The live route is switchable off on its own, leaving route B close-only as before T-403.
        let close_only = ConfirmPolicy {
            live_continuous: false,
            ..ConfirmPolicy::default()
        };
        assert_eq!(close_only.decide(&ev(steady_live())), None);
        assert!(close_only.decide(&ev(steady())).is_some());
    }

    /// T-398 route C. Each case removes exactly one piece of positive evidence from a signal that
    /// otherwise confirms, so the test says what the rule *requires*, not merely that it fires.
    #[test]
    fn verified_emission_confirms_only_on_a_real_lock() {
        let p = ConfirmPolicy::default();
        let locked = VerifiedEmission {
            mode: "wfm".into(),
            lock_quality: Some(0.95),
            pilot_hz: Some(18_999.9),
            bandwidth_hz: Some(221e3),
        };
        let ev = |v: VerifiedEmission| ConfirmEvidence {
            identity: None,
            track: None,
            verified: Some(v),
        };

        let reason = p.decide(&ev(locked.clone())).unwrap();
        assert!(reason.contains("verified wfm"), "{reason}");
        assert!(reason.contains("19000 Hz"), "{reason}");

        for (what, v) in [
            // A pilot-shaped bump the loop never held: `PilotReport` leaves both of these `None`
            // unless it locked, so this is the difference between a lock and a peak.
            (
                "never locked",
                VerifiedEmission {
                    lock_quality: None,
                    ..locked.clone()
                },
            ),
            (
                "no pilot frequency",
                VerifiedEmission {
                    pilot_hz: None,
                    ..locked.clone()
                },
            ),
            (
                "poor lock",
                VerifiedEmission {
                    lock_quality: Some(0.2),
                    ..locked.clone()
                },
            ),
            // 19.5 kHz is not the stereo pilot, whatever locked to it.
            (
                "wrong subcarrier",
                VerifiedEmission {
                    pilot_hz: Some(19_500.0),
                    ..locked.clone()
                },
            ),
            (
                "too narrow",
                VerifiedEmission {
                    bandwidth_hz: Some(12e3),
                    ..locked.clone()
                },
            ),
            (
                "no bandwidth measured",
                VerifiedEmission {
                    bandwidth_hz: None,
                    ..locked.clone()
                },
            ),
            // The demodulator did not call it WFM, so its pilot machinery did not decide this.
            (
                "another mode",
                VerifiedEmission {
                    mode: "2fsk".into(),
                    ..locked.clone()
                },
            ),
            (
                "unmeasurable",
                VerifiedEmission {
                    lock_quality: Some(f64::NAN),
                    ..locked.clone()
                },
            ),
        ] {
            assert_eq!(p.decide(&ev(v)), None, "{what}");
        }

        // The route can be switched off without disturbing the other two.
        let off = ConfirmPolicy {
            verified: false,
            ..ConfirmPolicy::default()
        };
        assert_eq!(off.decide(&ev(locked.clone())), None);
        assert!(
            off.decide(&ConfirmEvidence {
                identity: Some((IdentityScheme::RdsPi, 3)),
                track: None,
                verified: Some(locked),
            })
            .unwrap()
            .contains("rds-pi")
        );
    }

    /// A track-shaped sighting: fingerprinted, no classification, no identity.
    fn overlap_track_sighting(f: f64, bw: f64, seen: TimeRange, count: u64) -> Sighting {
        Sighting {
            source: LinkTarget::Track(TrackId::new()),
            seen,
            count,
            f_center_hz: f,
            bandwidth_hz: bw,
            fingerprint: Some(Fingerprint::new(f, bw)),
            identity: None,
            context: None,
            classification: None,
            tags: Vec::new(),
        }
    }

    /// A decoder-shaped sighting with no fingerprint of its own ([`hk_model::Sighting::decode`]'s
    /// shape): it carries only a classification, the way an identity/content decoder might report
    /// what it saw without re-measuring the RF fingerprint the tracker already has.
    fn overlap_decode_sighting(f: f64, bw: f64, seen: TimeRange, family: &str) -> Sighting {
        Sighting {
            source: LinkTarget::Demodulation(DemodulationId::new()),
            seen,
            count: 1,
            f_center_hz: f,
            bandwidth_hz: bw,
            fingerprint: None,
            identity: None,
            context: None,
            classification: Some(Classification {
                t: seen.end,
                family: family.into(),
                confidence: 0.9,
                open_set_score: 0.1,
                model_version: "test@1".into(),
            }),
            tags: Vec::new(),
        }
    }

    /// T-335: `characterise` must not skip an entry that just absorbed a same-emission partner
    /// (T-082) whose merge discount left `count` unmoved and whose fingerprint (absent on the
    /// absorbed side, [`hk_model::Sighting::decode`]'s shape) left `fingerprint observations`
    /// unmoved too — the exact pair `characterise` used to key its skip on. The merge still
    /// changed the survivor's evidence (it carries a classification it did not have before), and
    /// that must reach the features snapshot.
    #[test]
    fn t335_absorbing_a_partner_by_a_discounted_merge_still_re_characterises() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        let f = 100e6;
        let bw = 12e3;

        // The track-based entry: seen 0..10s, several bursts, no classification yet.
        let into_sighting = overlap_track_sighting(f, bw, TimeRange::new(t(0.0), t(10.0)), 5);
        let into_id = repo
            .record_sighting(&into_sighting, None)
            .unwrap()
            .emitter_id;
        inv.chain_emitter(&mut repo, None, into_id).unwrap();
        let baseline = repo
            .emitter_features(into_id)
            .unwrap()
            .expect("first touch characterises");
        assert!(
            !baseline.fields.contains_key(field::FAMILY),
            "nothing has classified this entry yet: {:?}",
            baseline.fields
        );
        let baseline_emitter = repo.emitter(into_id).unwrap();
        assert_eq!(baseline_emitter.count, 5);

        // A decoder-shaped entry over air already inside the track's span: T-082's overlap
        // discount takes its whole count (add = 0), and it carries no fingerprint to fold.
        let from_sighting =
            overlap_decode_sighting(f, bw, TimeRange::new(t(2.0), t(8.0)), "known-service");
        let from_id = repo
            .record_sighting(&from_sighting, None)
            .unwrap()
            .emitter_id;
        inv.chain_emitter(&mut repo, None, from_id).unwrap();

        // The merge happened, onto the track entry, and the discount really did leave both halves
        // of the old skip key unmoved.
        assert_eq!(inv.merged, 1, "the two entries were the same emission");
        assert_eq!(repo.live_emitter_id(from_id).unwrap(), into_id);
        let merged_emitter = repo.emitter(into_id).unwrap();
        assert_eq!(
            merged_emitter.count, baseline_emitter.count,
            "T-082's overlap discount: the absorbed air was already counted"
        );
        assert_eq!(
            repo.emitter_fingerprint(into_id)
                .unwrap()
                .map(|fp| fp.observations),
            Some(1),
            "the absorbed entry had no fingerprint to fold, so observations did not move either"
        );

        // Despite both halves of the old key being unmoved, the survivor absorbed a
        // classification it did not have before, and that must reach the features snapshot.
        let after = repo
            .emitter_features(into_id)
            .unwrap()
            .expect("still characterised");
        assert_eq!(
            after.fields.get(field::FAMILY).and_then(|f| f.value.text()),
            Some("known-service"),
            "[T-335] the merge's absorbed classification never reached characterise(): {:?}",
            after.fields
        );
    }

    /// The skip this ticket narrows stays an optimisation, not a no-op: a `touch` with no new
    /// merge and no new measurement does not append another features snapshot.
    #[test]
    fn t335_a_touch_with_nothing_new_still_skips_characterisation() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        let f = 100e6;
        let bw = 12e3;

        let sighting = overlap_track_sighting(f, bw, TimeRange::new(t(0.0), t(10.0)), 5);
        let id = repo.record_sighting(&sighting, None).unwrap().emitter_id;
        inv.chain_emitter(&mut repo, None, id).unwrap();
        let first = repo.emitter_features(id).unwrap().unwrap();
        assert_eq!(inv.characterisations, 1);

        // Touched again with the very same live entry: no merge, no growth.
        inv.chain_emitter(&mut repo, None, id).unwrap();
        let second = repo.emitter_features(id).unwrap().unwrap();
        assert_eq!(
            inv.characterisations, 1,
            "nothing changed since the last touch, so characterise() must still skip"
        );
        assert_eq!(first.id, second.id, "no new snapshot was appended");
    }

    fn t(sec: f64) -> Timestamp {
        Timestamp::from_unix_nanos(1_000_000_000 + (sec * 1e9) as i64)
    }
}
