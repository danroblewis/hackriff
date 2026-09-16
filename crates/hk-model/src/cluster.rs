//! Emitter clustering and inventory queries (C18 fingerprint, C27 inventory; T-018).
//!
//! The pure half: the [`Fingerprint`] and its per-feature [`Tolerances`], the [`Sighting`] a
//! producer hands to `Repository::record_sighting`, the [`Resolution`] it gets back, and the
//! [`InventoryQuery`] / [`InventoryPage`] types. The repository half (`repo/cluster.rs`) applies
//! the rules below inside one write transaction.
//!
//! # Clustering rules (in order)
//!
//! 1. **Replay.** A source (track, detection, decode) already in the observation ledger resolves
//!    to the emitter it was counted into (following merges) and adds only the growth of its
//!    count, so `count` means distinct observations and replaying never double-counts.
//!
//!    **Re-measurement** (T-034). Re-demodulating the same IQ mints new demodulation/decode ids,
//!    so the source id alone cannot catch it. A sighting offered with a [`MeasurementKey`]
//!    (`Repository::record_sighting_measured`) whose source is new resolves like a replay when
//!    the ledger holds a sighting with the same key whose span overlaps it by at least half the
//!    shorter span (an instant counts if it lies inside) and whose centre is within the
//!    fingerprint centre tolerance, unless the sighting's identity differs from that emitter's.
//!    It adds only its count beyond the largest count already recorded there. Spans that merely
//!    touch, a later session, another producer or another channel do not match *this rule*, so
//!    each keeps its own ledger row and resolves on through rules 2–5; what it then adds to
//!    `count` is settled by the same overlap test without the key (see "What a sighting adds to
//!    `count`" below), which still counts a touching window, a later session and another
//!    channel, and no longer counts a second producer's view of air the first already counted.
//! 2. **Identity.** A decoded identity always wins: the emitter holding it is the target. If the
//!    sighting names an emitter context (the detection/track emitter it was decoded from) and the
//!    identity's scheme does not share channels ([`IdentityScheme::shares_channel`]):
//!    - the context has no identity and nobody holds this one → the identity is set on the
//!      context;
//!    - someone else holds the identity and the context has none → the context is merged into
//!      the holder (recorded, never deleted);
//!    - the context holds a *different* identity → nothing is merged and an
//!      [`IdentityConflictReport`] is returned.
//! 3. **Context.** Without an identity, an existing context emitter is the target.
//! 4. **Fingerprint.** Otherwise the live emitter whose fingerprint is within every per-feature
//!    tolerance with the lowest score wins (ties: most recently seen). Emitters identified by a
//!    channel-sharing scheme never absorb anonymous matches. If two or more *identified*
//!    emitters match, the fingerprint cannot choose between them: a conflict is reported and the
//!    best unidentified match (or a new emitter) is used.
//! 5. **Create.** A new emitter with `known_status: unknown` (author `clusterer`).
//!
//! # What a sighting adds to `count` (T-209, T-329, T-336)
//!
//! `count` is a lifetime total of **occurrences** — times the emitter was observed to be on the
//! air — never a tally of the writes that reached the repository (docs/07 §2.11, ADR-0017 §3).
//! One emission over one stretch of time is **one** occurrence, however many producers saw it:
//! overlapping observations are not counted twice, whoever observed them. So rules 2–4 add only
//! what the emitter's ledger does not already hold over the sighting's air: **rule 1's
//! re-measurement test with the producer key dropped** (T-336). A row of this emitter covering
//! the same air — the same "at least half the shorter span" overlap, whoever wrote it — caps the
//! sighting at its excess over the largest such count, and nothing adds when that is already as
//! large. When no row does, the whole count adds, which is how a second session after a silence
//! is a second occurrence, and why rule 1's deliberate near-misses (a touching window, a few ms
//! of jitter on an edge, another channel) still count. There is no proration in between: `count`
//! counts occurrences, not seconds. `Resolution::count_added` reports what was actually added,
//! which a sighting's own `count` never tells you.
//!
//! # Same emission (T-082)
//!
//! A decoder or chain output of an emission the tracker also followed can become a second
//! entry. An identity sighting with no holder and no context is created on its own (rule 2), and
//! emitters identified by a channel-sharing scheme (such as the blind framer's structural
//! signature) never absorb anonymous matches (rule 4). `Repository::same_emission` recognises
//! two live, listed entries of one emission:
//! - **Frequency:** the closest pair of their detected or refined centres is within the centre
//!   tolerance above, `max(10 ppm · f, 0.25 · max BW, 500 Hz)`. Partners are searched by
//!   detected extent.
//! - **Time:** an observation of one overlaps an observation of the other (ledger spans).
//! - **Hops:** both are hop sets or neither (a hop set and one of its channels stay apart).
//! - **Identity:** not both identified.
//! - **Two track-based entries** also need their stored fingerprints within tolerance, so two
//!   emitters sharing a channel with another period or burst length stay apart.
//!
//! `Repository::merge_same_emission` merges such a pair. The survivor is the confirmed entry,
//! else the first seen, else the larger count. Observations the survivor already covers in time
//! are not counted again. Every merge (this one, rule 2's, `merge_emitters`) keeps the absorbed
//! row's links, observations (recurrence), tags, identity, classification history, refined
//! tunings (read through merges) and a decoder, classifier or user known status. A confirmed row
//! merged into a candidate confirms it, recorded in the survivor's lifecycle history. A deleted
//! row is never merged. The pipeline decides when to link (`hk_pipeline::inventory`: only the
//! run's own entries, never channel-sharing transmitter identities).
//!
//! After assignment, when the emitter was created or its family changed, a
//! [`KnownStatusPrior`] (C17, e.g. hk-context's `match_known_status`) may append a status. A prior
//! never overrides a status decided by a decoder, classifier or user.
//!
//! # Fingerprint features and tolerances (feature set v1)
//!
//! A feature is compared only when both sides have it; the match needs centre frequency and every
//! compared feature within tolerance. The score is the mean normalised error (`|Δ| / tolerance`).
//!
//! | Feature | Tolerance ([`Tolerances::default`]) |
//! |---|---|
//! | centre frequency | `max(10 ppm · f, 0.25 · max BW, 500 Hz)` (oscillator drift, CFO) |
//! | bandwidth | ratio ≤ 1.5 (skipped when either is 0) |
//! | family | exact, unless either is missing or `unknown` (hard gate) |
//! | symbol rate | ±1 % (C18 card default) |
//! | deviation | ±10 % |
//! | period | ±5 % |
//! | duty cycle | `max(0.05, 25 % of the larger)` absolute |
//! | burst length (median) | `max(25 %, 4 ms)` |
//! | hop presence | both hop or neither (hard gate) |
//! | hop raster | ±2 % |
//! | hop set | Jaccard overlap ≥ 0.5 (channels match within `max(raster/4, centre tol)`) |
//! | modulation structure (T-233) | 3 combined sigmas ([`ModulationStructure`]) |
//!
//! **Three of those features measure the window, not the emission** — `period`, `duty cycle` and
//! `burst length` are statistics of however long the producer happened to watch. Two observations
//! whose **presence intervals are disjoint** (docs/07 §2.27) were watched over different windows,
//! so those three carry no information about whether they are the same emitter, and
//! [`Fingerprint::compare_across_silence`] excludes them. Centre, bandwidth, family, symbol rate,
//! deviation and the hop features still apply, so two genuinely distinct emissions stay apart.
//! This is the same narrow exclusion `relate::distinguishing_evidence` already makes for the same
//! measured reason (T-250), applied one layer earlier — at entity resolution, which is where a
//! station that stops and comes back was minting a **second emitter** instead of reviving the
//! first (T-262, ADR-0017 §1.1).
//!
//! Folding a new observation into a stored fingerprint takes a running mean (weight capped at 16
//! observations, so a slowly drifting centre is followed) and keeps the latest family and hop set.
//!
//! # Content gating of identities (legal guardrail)
//!
//! Every public emitter read returns an identity in clear only when its content class positively
//! permits it ([`IdentityAccess::reveals`]): `unrestricted`, or `own-key-decrypted` with an
//! explicit [`IdentityAccess::OwnTrafficAuthorised`]. Anything else, including an identity whose
//! class is unknown (legacy writers), is [`InventoryIdentity::Withheld`]: the scheme (metadata)
//! is shown, the value is not. The class is the most restrictive class of the identity's sources
//! ([`most_restrictive`]); when none was recorded it is derived from the linked decodes carrying
//! that identity, and missing means withheld.
//!
//! The rule covers `query_inventory`, `emitter` / `emitters_in_region` / `emitter_by_identity`
//! (gated at [`IdentityAccess::Standard`]; a withheld identity reads as `Identity::Unknown`) and
//! their explicit-access forms `emitter_with_access` / `emitter_by_identity_with_access`. A
//! lookup by identity value finds nothing unless that identity would be shown, so it cannot
//! confirm a withheld identity is held. `RepoError::IdentityConflict` names the scheme only.
//! No public API returns identities ungated.
//!
//! # Decodes (T-036)
//!
//! `decode` / `decodes_for_identity` (gated at [`IdentityAccess::Standard`]) and their
//! `decode_with_access` / `decodes_for_identity_with_access` forms ([`crate::DecodeView`]) apply
//! the same rule; the ungated read is crate-private.
//!
//! - **Identity class** of a decoded identity: the most restrictive of every stored decode naming
//!   it and of the class on the emitter holding it (after any audited reclassification, below).
//!   A row's identity is shown only if that class is revealed, so one restricted decode withholds
//!   the value on every row, like the inventory.
//! - **Detail** (metadata and labels) is shown only if the row's own `content_class` is revealed
//!   and its identity (if any) is shown. The repository cannot tell identifier keys (a capcode)
//!   from other metadata keys, so a withheld row's metadata is withheld whole (`{}`), its content
//!   too (a content-permitting row whose identity is withheld elsewhere), and a label
//!   (`frame_model`, `decoder_id`, `decoder_version`) that contains the identity value or any
//!   metadata/content value of three or more characters reads [`crate::WITHHELD_LABEL`]. Labels are
//!   otherwise kept: they are the protocol labels the legal guardrail lets flow, and plugin rows
//!   are label-allowlisted before storage (docs/stream-contract.md §9.3).
//! - `decodes_for_identity*` returns nothing unless the identity would be shown, so a lookup
//!   cannot confirm a withheld identity was decoded.
//!
//! # Tags
//!
//! Tags are labels, not identities. A shape rule cannot tell a label from an alphabetic identity
//! (an alias `zulu` looks like any word), so rows whose identity is withheld use a **controlled
//! vocabulary** ([`TAG_VOCABULARY`], T-038): fixed, public labels (band-plan/allocation tags,
//! service and workflow words) that no traffic can choose.
//!
//! - **Read:** on a row whose identity is withheld for the caller, only vocabulary tags
//!   ([`tag_in_vocabulary`]) are shown (`InventoryEntry::tags_withheld` says others were removed),
//!   and a `tag` filter outside the vocabulary never matches such a row. The rule does not look at
//!   stored values, so neither the output nor the filter can test a guessed identity. This is the
//!   guarantee: it also covers tags written before the identity arrived or was restricted, and
//!   tags carried over by a merge. Rows with a shown identity or none keep every tag.
//! - **Storage (T-040):** when a write makes an identity one no access level reveals (an identity
//!   arriving, a more restrictive source or linked decode, a merge), the emitter's tags outside
//!   the vocabulary (and those of rows merged into it) are deleted in the same transaction, and a
//!   merge copies only vocabulary tags onto such a survivor. Fail closed: the deletion is not
//!   undone if the class later opens. `remove_emitter_tag` is gated like `add_emitter_tag`, so
//!   its answer cannot confirm a guessed hidden tag; both act on a merged id's survivor.
//! - **Write:** when the identity is one no access level reveals (unclassified, `metadata-only`,
//!   `restricted-paging`, `restricted-cellular`), `record_sighting`, `insert_emitter` (whose
//!   identity has no class) and `add_emitter_tag` refuse a tag outside the vocabulary. The refusal
//!   depends only on the class the inventory already shows, never on the value, so it is no
//!   oracle; it tells the user at once instead of storing a tag that would never show. An
//!   `own-key-decrypted` claim keeps the T-036 producer rule ([`tag_is_identity_free`], no claim
//!   value); a user's free-text tag on such a row is accepted and shows only where the identity
//!   does (own-traffic authorisation).
//! - **Cost (why this is the least lossy fail-closed rule):** free-text labels on restricted or
//!   unclassified rows are lost; vocabulary labels (including digit-bearing band tags such as
//!   `l1` or `lte-band2`, which the T-036 shape rule hid) remain. Refusing all tags there would
//!   lose more; value checks would be an oracle; a label namespace prefix cannot stop
//!   `label:zulu`.
//!
//! # Audited reclassification (T-036)
//!
//! "Most restrictive wins" means no later sighting can open a withheld identity, even the user's
//! own device. `Repository::reclassify_identity` is the only way to open one: it needs
//! [`IdentityAccess::OwnTrafficAuthorised`], opens only to `own-key-decrypted` or `unrestricted`,
//! refuses identities whose class is unknown or ever `restricted-cellular` / `restricted-paging`
//! ([`never_openable`]), appends an audit row (who, when, old → new, reason) and changes only the
//! emitter's `identity_class`: decode rows keep the class their decoder recorded. Afterwards a
//! source of the opened class or less restrictive (e.g. the same `metadata-only` framer) counts
//! as the opened class for that identity; a restricted-cellular or restricted-paging source still
//! closes it. Decode detail stays gated by each row's own class. There is no HTTP exposure (M0).

use serde::{Deserialize, Serialize};

use crate::content::ContentClass;
use crate::decode::Decode;
use crate::detection::Track;
use crate::emitter::{
    Classification, DecodedIdentity, Emitter, EmitterLink, IdentityScheme, KnownStatus,
    LifecycleState, LinkTarget,
};
use crate::ids::EmitterId;
use crate::region::{FreqRange, TimeRange};
use crate::time::Timestamp;

/// Version of the fingerprint feature set. Stored in every fingerprint and on classifications
/// that ran on it; a stored fingerprint of another version is ignored (centre/bandwidth only).
pub const FEATURE_SET_VERSION: u32 = 1;

/// Largest inventory page.
pub const MAX_INVENTORY_PAGE: u32 = 1000;

/// Running-mean weight cap when folding observations into a fingerprint.
const FOLD_WEIGHT_CAP: u64 = 16;

/// One dimensionless statistic of an emission's **own modulation structure** (T-233, C18/C15).
///
/// # Why the fingerprint needs it at all
///
/// Every other feature it compares — centre, bandwidth, symbol rate, deviation, the timing
/// statistics — can be identical for two genuinely different emissions. What is left is what the
/// modulation itself does.
///
/// [`Self::envelope_shape`] is `μ₄ = E|s|⁴ / (E|s|²)²` **of the emission**, the additive-noise
/// contribution removed in closed form. It is exactly `1` for any constant-envelope emission —
/// every FSK, MSK, FM and unkeyed carrier — and rises with amplitude structure. It is what says
/// whether the information is in the envelope at all: `am` reads ≈ 1.18 against `wfm` ≈ 1.00, and
/// `bpsk` ≈ 1.38 against `qpsk` ≈ 1.20, because a shaped BPSK's 180° transitions carry the
/// trajectory through the origin and a QPSK's mostly do not.
///
/// # It measures the emission, not the observation (the rule this exists to obey)
///
/// It is a ratio of two **expectations** — never an extremum, a maximum over method variants, or
/// a count that grows with how long anybody watched. Lengthening the record estimates it better
/// and moves it not at all, which is the property the fingerprint needs: the two observations it
/// compares were watched differently by construction, so a statistic that moved with the watch
/// would split or merge on the watch. It is also blind to amplitude, to a carrier offset and to
/// any phase rotation, since it sees only `|s|`.
///
/// The one thing that does move it is noise, and that is why the value is carried **with the
/// sigma its producer measured it to** rather than bare: `E|x|⁴ = E|s|⁴ + 4Pσ² + 2σ⁴` and
/// `E|x|² = P + σ²` give `μ₄ₛ = μ₄ₓ(1+1/ρ)² − 4/ρ − 2/ρ²`, and what is left in `sigma` is the
/// error of that correction plus how much the emission's own content moved across the record.
/// `hk_classify::structure` holds both derivations, the measured confirmation, and the second
/// dimension that was built and rejected for measuring the observation instead.
///
/// # What it is allowed to do
///
/// Only **split**. [`Fingerprint::compare`] adds it as an ordinary scored feature, so a
/// fingerprint that carries structure is compared on strictly more evidence than one that does
/// not, and a pair that matched without it can only stop matching, never start. Nothing here can
/// merge two emitters the rest of the fingerprint keeps apart. That is deliberate: merging two
/// emitters costs an inventory row its meaning, and no statistic measured under T-233 reached the
/// confidence that would justify the other direction.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModulationStructure {
    /// `E|s|⁴/(E|s|²)²` of the emission, noise-corrected. `1.0` = constant envelope.
    pub envelope_shape: f64,
    /// 1σ on [`Self::envelope_shape`], as its producer measured it. Never zero in practice.
    pub envelope_shape_sigma: f64,
}

impl ModulationStructure {
    /// The value and its sigma are finite and non-negative (`μ₄ ≥ 0` for any distribution, and an
    /// uncertainty is not negative).
    pub fn is_valid(&self) -> bool {
        self.envelope_shape.is_finite()
            && self.envelope_shape >= 0.0
            && self.envelope_shape_sigma.is_finite()
            && self.envelope_shape_sigma >= 0.0
    }

    /// Folds an observation in: a capped running mean of the value at weight `w`, with the sigma
    /// taken as the **larger** of the two reported and the half-difference between them.
    ///
    /// Sigma never shrinks with more looks, for [`crate::signature::Feat`]'s reason: an aggregate
    /// is not better known than the observations disagreed, and an over-tight sigma is what turns
    /// one emitter into two.
    fn fold(&mut self, obs: &ModulationStructure, w: f64) {
        self.envelope_shape_sigma = self
            .envelope_shape_sigma
            .max(obs.envelope_shape_sigma)
            .max((self.envelope_shape - obs.envelope_shape).abs() / 2.0);
        self.envelope_shape = (self.envelope_shape * w + obs.envelope_shape) / (w + 1.0);
    }
}

/// Compact cross-time signature of an emission cluster (C18; docs/04 §7.6 subset).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// Feature-set version ([`FEATURE_SET_VERSION`]).
    pub version: u32,
    /// Centre frequency, Hz.
    pub f_center_hz: f64,
    /// Occupied bandwidth, Hz (0 = unknown).
    pub bandwidth_hz: f64,
    /// Modulation family, e.g. `2fsk`, `wfm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Symbol rate, Bd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_rate_hz: Option<f64>,
    /// FSK deviation, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deviation_hz: Option<f64>,
    /// Burst repetition period, s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_s: Option<f64>,
    /// Duty cycle, 0–1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty_cycle: Option<f64>,
    /// Median burst length, s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst_length_s: Option<f64>,
    /// Hop raster, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hop_raster_hz: Option<f64>,
    /// Hop-set channel centres, Hz.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hop_set_hz: Vec<f64>,
    /// Modulation structure (T-233): what the modulation does, once centre, bandwidth, rate and
    /// deviation have all agreed. Absent means **not measured**, never "no structure".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structure: Option<ModulationStructure>,
    /// Observations folded in.
    #[serde(default)]
    pub observations: u64,
}

impl Fingerprint {
    /// A fingerprint with only centre and bandwidth.
    pub fn new(f_center_hz: f64, bandwidth_hz: f64) -> Self {
        Self {
            version: FEATURE_SET_VERSION,
            f_center_hz,
            bandwidth_hz,
            family: None,
            symbol_rate_hz: None,
            deviation_hz: None,
            period_s: None,
            duty_cycle: None,
            burst_length_s: None,
            hop_raster_hz: None,
            hop_set_hz: Vec::new(),
            structure: None,
            observations: 1,
        }
    }

    /// Centre, bandwidth, period, duty cycle and hop set from a docs/07 Track. Producers add the
    /// estimates they have (family, symbol rate, deviation, burst length, raster).
    pub fn from_track(track: &Track) -> Self {
        let tf = &track.timing;
        Self {
            period_s: tf.period_s,
            duty_cycle: tf.duty_cycle,
            hop_set_hz: tf.hop_set_hz.clone(),
            ..Self::new(track.f_center_hz, track.bandwidth_hz)
        }
    }

    /// Parses a stored fingerprint; `None` for null, malformed or another feature-set version.
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        serde_json::from_value::<Self>(value.clone())
            .ok()
            .filter(|f| f.version == FEATURE_SET_VERSION)
    }

    /// Every numeric feature is finite.
    pub fn is_finite(&self) -> bool {
        let opts = [
            self.symbol_rate_hz,
            self.deviation_hz,
            self.period_s,
            self.duty_cycle,
            self.burst_length_s,
            self.hop_raster_hz,
        ];
        self.f_center_hz.is_finite()
            && self.bandwidth_hz.is_finite()
            && opts.iter().flatten().all(|v| v.is_finite())
            && self.hop_set_hz.iter().all(|v| v.is_finite())
            && self.structure.is_none_or(|s| s.is_valid())
    }

    /// The family, unless missing or `unknown`.
    pub fn known_family(&self) -> Option<&str> {
        known_family(self.family.as_deref())
    }

    /// Centre-frequency tolerance against `other`, Hz.
    pub fn center_tolerance_hz(&self, other: &Fingerprint, tol: &Tolerances) -> f64 {
        let f = self.f_center_hz.abs().max(other.f_center_hz.abs());
        (tol.center_ppm * 1e-6 * f)
            .max(tol.center_bw_fraction() * self.bandwidth_hz.max(other.bandwidth_hz))
            .max(tol.center_min_hz)
    }

    /// Compares two fingerprints feature by feature (see the module table).
    pub fn compare(&self, other: &Fingerprint, tol: &Tolerances) -> FeatureMatch {
        self.compare_inner(other, tol, true)
    }

    /// [`Self::compare`] for two observations whose **presence intervals do not overlap**: the
    /// three window statistics (`period`, `duty_cycle`, `burst_length`) are excluded.
    ///
    /// They describe how long each side was watched, not what was transmitting. The measured case
    /// (T-250, on the user's staging database): one FM station seen over 305 s and then over 59 s
    /// reported median burst lengths of 0.68 s and 0.37 s — a normalised error of 1.86, enough on
    /// its own to fail the match and mint a second inventory row for a station that had merely
    /// stopped and come back. Everything that describes the *emission* is still compared, so this
    /// never merges two emissions that differ in centre, bandwidth, family, symbol rate, deviation
    /// or hop behaviour.
    ///
    /// **The cost, stated:** two distinct emissions that share a channel, a bandwidth and a family
    /// and never transmit at the same time can no longer be told apart by their duty cycle alone.
    /// That is the ISM case (T-254), and it is the same trade `distinguishing_evidence` already
    /// accepted; while the intervals *do* overlap, the figures are comparable and still separate
    /// them. TM-9 is where it gets measured.
    pub fn compare_across_silence(&self, other: &Fingerprint, tol: &Tolerances) -> FeatureMatch {
        self.compare_inner(other, tol, false)
    }

    /// [`Self::compare`]; `window_stats` includes the three features that measure the observation
    /// window rather than the emission.
    fn compare_inner(
        &self,
        other: &Fingerprint,
        tol: &Tolerances,
        window_stats: bool,
    ) -> FeatureMatch {
        let mut m = Acc::default();
        let f_tol = self.center_tolerance_hz(other, tol);
        m.add(
            "center",
            (self.f_center_hz - other.f_center_hz).abs() / f_tol,
        );
        if self.bandwidth_hz > 0.0 && other.bandwidth_hz > 0.0 {
            let ratio = self.bandwidth_hz.max(other.bandwidth_hz)
                / self.bandwidth_hz.min(other.bandwidth_hz);
            m.add(
                "bandwidth",
                ratio.ln() / tol.bandwidth_ratio.max(1.0 + 1e-9).ln(),
            );
        }
        // Exact comparison, deliberately, and now **measured** rather than deferred (T-233).
        //
        // ADR-0016 §1 proposed gating on `taxonomy::family_of` instead, so that an emitter
        // labelled `fsk` by the blind estimator and `2fsk` by the demodulator chain stops
        // splitting. T-218 found the cost: it also merges `bpsk` with `qpsk`, `2fsk` with `gfsk`
        // and `am` with `wfm`. T-233 asked whether a finer measured discriminator could make the
        // relaxation safe, built one ([`ModulationStructure`]) and measured it. It cannot:
        //
        // - `fsk` is the **only** family-level label any producer writes (`hk_estimate::blind`
        //   emits `Fsk` but `Bpsk`/`Qpsk`/`Ook` — classes), so the relaxation is exactly a licence
        //   to merge an `fsk` row with a `2fsk`, `gfsk`, `msk` or `4fsk` one;
        // - and `2fsk`, `gfsk` and `msk` are not three modulations but **one modulation at three
        //   filter settings**. All three are constant-envelope by construction, so the statistic
        //   that shipped reads 1.00 for every one of them and separates none: conditioned as the
        //   fingerprint conditions (matched centre, bandwidth, symbol rate and deviation), over
        //   99 % of genuinely distinct pairs compare as one at every SNR from the FSK gate
        //   upwards. The best statistic T-233 tried on that pair — the instantaneous frequency's
        //   bimodality — still left 5.8 % to 33 %, and was rejected anyway for measuring the
        //   observation rather than the emission (`hk_classify::structure`).
        //
        // The relaxation's own motivating case does not need it either: two entries of one
        // emission written by a tracker and a chain are already merged by `same_emission`
        // (T-082), which compares centre and time and never looks at the family at all.
        //
        // So the gate stays exact and ADR-0016 §1's change is **withdrawn**, not deferred. What
        // T-233 landed instead is below: structure as extra evidence that can only split. See
        // `tests::t218_the_family_gate_separates_two_emissions_that_share_a_family`.
        if let (Some(a), Some(b)) = (self.known_family(), other.known_family())
            && a != b
        {
            m.gate("family");
        }
        if let (Some(a), Some(b)) = (self.symbol_rate_hz, other.symbol_rate_hz) {
            m.add("symbol_rate", rel(a, b) / tol.symbol_rate_rel);
        }
        if let (Some(a), Some(b)) = (self.deviation_hz, other.deviation_hz) {
            m.add("deviation", rel(a, b) / tol.deviation_rel);
        }
        // The three window statistics: skipped when the two were watched over disjoint windows.
        if window_stats {
            if let (Some(a), Some(b)) = (self.period_s, other.period_s) {
                m.add("period", rel(a, b) / tol.period_rel);
            }
            if let (Some(a), Some(b)) = (self.duty_cycle, other.duty_cycle) {
                let t = tol.duty_abs.max(tol.duty_rel * a.abs().max(b.abs()));
                m.add("duty_cycle", (a - b).abs() / t);
            }
            if let (Some(a), Some(b)) = (self.burst_length_s, other.burst_length_s) {
                let t = (tol.burst_length_rel * a.abs().max(b.abs())).max(tol.burst_length_min_s);
                m.add("burst_length", (a - b).abs() / t);
            }
        }
        // Modulation structure (T-233): compared whenever both sides carry it, in units of the
        // sigma each producer reported. A scored feature rather than a gate, so a pair already
        // outside tolerance on `family` or `centre` is not re-judged here.
        //
        // A dimension whose producer stated no uncertainty is **not** infinitely certain: it is
        // unstated, and an unstated field is no evidence, exactly as a missing one is.
        if let (Some(a), Some(b)) = (&self.structure, &other.structure) {
            let k = tol.structure_sigmas.max(f64::MIN_POSITIVE);
            let band = k
                * (a.envelope_shape_sigma * a.envelope_shape_sigma
                    + b.envelope_shape_sigma * b.envelope_shape_sigma)
                    .sqrt();
            if band > 0.0 {
                m.add(
                    "envelope_shape",
                    (a.envelope_shape - b.envelope_shape).abs() / band,
                );
            }
        }
        if self.hop_set_hz.is_empty() != other.hop_set_hz.is_empty() {
            m.gate("hop_presence");
        }
        if let (Some(a), Some(b)) = (self.hop_raster_hz, other.hop_raster_hz) {
            m.add("hop_raster", rel(a, b) / tol.hop_raster_rel);
        }
        if !self.hop_set_hz.is_empty() && !other.hop_set_hz.is_empty() {
            let raster = self.hop_raster_hz.or(other.hop_raster_hz);
            let ch_tol = raster.map_or(f_tol, |r| (r * 0.25).max(f_tol.min(r * 0.5)));
            let j = jaccard(&self.hop_set_hz, &other.hop_set_hz, ch_tol);
            m.add(
                "hop_set",
                (1.0 - j) / (1.0 - tol.hop_set_min_jaccard).max(1e-9),
            );
        }
        m.finish()
    }

    /// Folds a new observation into this (stored) fingerprint.
    pub fn fold(&mut self, obs: &Fingerprint) {
        let w = self.observations.clamp(1, FOLD_WEIGHT_CAP) as f64;
        let mean = |old: f64, new: f64| (old * w + new) / (w + 1.0);
        let opt = |old: Option<f64>, new: Option<f64>| match (old, new) {
            (Some(o), Some(n)) => Some(mean(o, n)),
            (o, n) => n.or(o),
        };
        self.version = FEATURE_SET_VERSION;
        self.f_center_hz = mean(self.f_center_hz, obs.f_center_hz);
        self.bandwidth_hz = match (self.bandwidth_hz > 0.0, obs.bandwidth_hz > 0.0) {
            (true, true) => mean(self.bandwidth_hz, obs.bandwidth_hz),
            (false, _) => obs.bandwidth_hz,
            (true, false) => self.bandwidth_hz,
        };
        if obs.known_family().is_some() {
            self.family = obs.family.clone();
        }
        self.symbol_rate_hz = opt(self.symbol_rate_hz, obs.symbol_rate_hz);
        self.deviation_hz = opt(self.deviation_hz, obs.deviation_hz);
        self.period_s = opt(self.period_s, obs.period_s);
        self.duty_cycle = opt(self.duty_cycle, obs.duty_cycle);
        self.burst_length_s = opt(self.burst_length_s, obs.burst_length_s);
        self.hop_raster_hz = opt(self.hop_raster_hz, obs.hop_raster_hz);
        if !obs.hop_set_hz.is_empty() {
            self.hop_set_hz = obs.hop_set_hz.clone();
        }
        match (self.structure.as_mut(), obs.structure) {
            (Some(s), Some(o)) => s.fold(&o, w),
            (None, o @ Some(_)) => self.structure = o,
            (_, None) => {}
        }
        self.observations = self.observations.max(1) + obs.observations.max(1);
    }
}

pub(crate) fn known_family(family: Option<&str>) -> Option<&str> {
    family.filter(|f| !f.trim().is_empty() && !f.eq_ignore_ascii_case("unknown"))
}

fn rel(a: f64, b: f64) -> f64 {
    (a - b).abs() / a.abs().max(b.abs()).max(f64::MIN_POSITIVE)
}

fn jaccard(a: &[f64], b: &[f64], tol: f64) -> f64 {
    let matched = a
        .iter()
        .filter(|x| b.iter().any(|y| (*x - y).abs() <= tol))
        .count();
    let union = a.len() + b.len() - matched;
    if union == 0 {
        1.0
    } else {
        matched as f64 / union as f64
    }
}

#[derive(Default)]
struct Acc {
    sum: f64,
    compared: u32,
    gated: bool,
    worst: Option<&'static str>,
    worst_error: f64,
}

impl Acc {
    fn add(&mut self, name: &'static str, err: f64) {
        let err = if err.is_finite() { err } else { f64::INFINITY };
        self.compared += 1;
        self.sum += err.min(1e6);
        if self.worst.is_none() || err > self.worst_error {
            self.worst = Some(name);
            self.worst_error = err;
        }
    }

    fn gate(&mut self, name: &'static str) {
        self.gated = true;
        self.worst = Some(name);
        self.worst_error = f64::INFINITY;
    }

    fn finish(self) -> FeatureMatch {
        FeatureMatch {
            within: !self.gated && self.worst_error <= 1.0,
            score: if self.compared == 0 {
                0.0
            } else {
                self.sum / self.compared as f64
            },
            compared: self.compared,
            worst: self.worst,
            worst_error: self.worst_error,
        }
    }
}

/// Result of [`Fingerprint::compare`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FeatureMatch {
    /// Every compared feature within tolerance and no hard gate failed.
    pub within: bool,
    /// Mean normalised error of the compared features (lower is closer).
    pub score: f64,
    /// Features compared.
    pub compared: u32,
    /// Feature with the largest normalised error (or the failed gate).
    pub worst: Option<&'static str>,
    /// Its normalised error (∞ for a failed gate).
    pub worst_error: f64,
}

/// Per-feature tolerances (see the module table for the defaults and their reasons).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tolerances {
    /// Centre drift, ppm of the carrier.
    pub center_ppm: f64,
    /// Centre tolerance as a fraction of the larger bandwidth (clamped to ≤ 0.5, which the
    /// candidate pre-filter relies on).
    pub center_bw_fraction: f64,
    /// Smallest centre tolerance, Hz.
    pub center_min_hz: f64,
    /// Largest bandwidth ratio.
    pub bandwidth_ratio: f64,
    /// Symbol rate, relative.
    pub symbol_rate_rel: f64,
    /// Deviation, relative.
    pub deviation_rel: f64,
    /// Period, relative.
    pub period_rel: f64,
    /// Duty cycle, absolute floor.
    pub duty_abs: f64,
    /// Duty cycle, relative to the larger.
    pub duty_rel: f64,
    /// Burst length, relative.
    pub burst_length_rel: f64,
    /// Burst length, absolute floor, s.
    pub burst_length_min_s: f64,
    /// Hop raster, relative.
    pub hop_raster_rel: f64,
    /// Smallest hop-set Jaccard overlap.
    pub hop_set_min_jaccard: f64,
    /// How many combined sigmas of [`ModulationStructure`] two observations may differ by before
    /// the structure counts against a match. See [`Tolerances::STRUCTURE_SIGMAS_DEFAULT`].
    pub structure_sigmas: f64,
}

impl Default for Tolerances {
    fn default() -> Self {
        Self {
            center_ppm: 10.0,
            center_bw_fraction: 0.25,
            center_min_hz: 500.0,
            bandwidth_ratio: 1.5,
            symbol_rate_rel: 0.01,
            deviation_rel: 0.10,
            period_rel: 0.05,
            duty_abs: 0.05,
            duty_rel: 0.25,
            burst_length_rel: 0.25,
            burst_length_min_s: 0.004,
            hop_raster_rel: 0.02,
            hop_set_min_jaccard: 0.5,
            structure_sigmas: Tolerances::STRUCTURE_SIGMAS_DEFAULT,
        }
    }
}

impl Tolerances {
    /// Sigmas of [`ModulationStructure`] two observations of one emitter may differ by.
    ///
    /// **Three, because three is what the error direction costs**, and not because of where any
    /// measured pair happened to fall. The structure dimensions are the only ones in the table
    /// whose producer states its own uncertainty, so the tolerance is a number of sigmas rather
    /// than a width: what has to be chosen is a false-split rate, and under a Gaussian error a
    /// two-sided 3σ band leaves 0.27 % per dimension, 0.54 % over the two.
    ///
    /// The error directions are not symmetric and the choice follows the expensive one. **Too
    /// tight** splits one emitter into two inventory rows — the failure T-250 and T-262 measured
    /// and the reason three window statistics were dropped from the disjoint-interval comparison
    /// in the first place. **Too loose** merely declines to split a pair the rest of the
    /// fingerprint already matched, which is where every such pair stood before T-233 anyway. So
    /// the band is set wide enough that a real emitter's re-observation survives it, and the
    /// discrimination that is left is whatever the measured gap affords at that width — not the
    /// other way round.
    ///
    /// Measured at that width on the acceptance seeds (`hk-classify`'s `structure_rates`,
    /// conditioned on a matched occupied bandwidth — at a matched symbol rate, a matched
    /// modulation index): `bpsk` against `qpsk` merges 0.097 of the time at 20 dB and 0.027 at
    /// 25 dB, against a baseline of 1.00 with no structure at all, while splitting one emitter in
    /// under 0.02. At the PSK gate itself the band is wider than the 0.18 the two classes are
    /// apart and the dimension concludes nothing, which is the sigma working rather than failing.
    /// `2fsk` against `gfsk` does not separate at any width or any SNR — that is why the family
    /// gate above stays exact.
    pub const STRUCTURE_SIGMAS_DEFAULT: f64 = 3.0;

    /// The clamped bandwidth fraction.
    pub fn center_bw_fraction(&self) -> f64 {
        self.center_bw_fraction.clamp(0.0, 0.5)
    }
}

/// A decoded identity and the content class of the decode that produced it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IdentityClaim {
    /// Identity.
    pub identity: DecodedIdentity,
    /// Class of the source decode (gates the identity in inventory output).
    pub content_class: ContentClass,
}

/// One observation offered to entity resolution (`Repository::record_sighting`).
#[derive(Clone, Debug, PartialEq)]
pub struct Sighting {
    /// The measurement or interpretation observed (dedup key and emitter link).
    pub source: LinkTarget,
    /// Time span.
    pub seen: TimeRange,
    /// Distinct observations it represents (e.g. bursts of a track). Re-offering the same source
    /// with a larger count adds only the difference.
    pub count: u64,
    /// Observed centre frequency, Hz.
    pub f_center_hz: f64,
    /// Observed bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Fingerprint, when the source has features.
    pub fingerprint: Option<Fingerprint>,
    /// Decoded identity, if any.
    pub identity: Option<IdentityClaim>,
    /// The emitter the producer already associates with the source (e.g. the detection-track
    /// emitter a decode ran on). Missing emitters are ignored.
    pub context: Option<EmitterId>,
    /// A classification of this source, appended with the source as its input.
    pub classification: Option<Classification>,
    /// Tags to add.
    pub tags: Vec<String>,
}

impl Sighting {
    /// A Track sighting: centre/bandwidth from the track, `count` = its member detections.
    pub fn track(track: &Track, fingerprint: Fingerprint) -> Self {
        Self {
            source: LinkTarget::Track(track.id),
            seen: track.time,
            count: track.detection_count,
            f_center_hz: track.f_center_hz,
            bandwidth_hz: track.bandwidth_hz,
            fingerprint: Some(fingerprint),
            identity: None,
            context: None,
            classification: None,
            tags: Vec::new(),
        }
    }

    /// An identity sighting from a stored decode (`None` when it carries no identity): one
    /// observation at the decode time on the given channel.
    pub fn decode(
        decode: &Decode,
        f_center_hz: f64,
        bandwidth_hz: f64,
        context: Option<EmitterId>,
    ) -> Option<Self> {
        let identity = decode.identity.clone()?;
        Some(Self {
            source: LinkTarget::Decode(decode.id),
            seen: TimeRange::instant(decode.t),
            count: 1,
            f_center_hz,
            bandwidth_hz,
            fingerprint: None,
            identity: Some(IdentityClaim {
                identity,
                content_class: decode.content_class,
            }),
            context,
            classification: None,
            tags: Vec::new(),
        })
    }
}

/// What a sighting measured, independent of the row ids a (re-)run mints (rule 1,
/// re-measurement). Offered with `Repository::record_sighting_measured`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MeasurementKey {
    /// Producer, without its version (re-running a newer decoder on the same IQ is still the
    /// same measurement), e.g. `hk-rds`, `hk-infer`. Different producers count separately.
    pub producer: String,
    /// Stable name of the capture, e.g. a SigMF `core:sha512` or data path. Never a freshly
    /// minted row id, which differs on every replay. `None` = unnamed: sample times are
    /// absolute, so overlapping spans on one channel are the same emission.
    pub capture: Option<String>,
}

impl MeasurementKey {
    /// A key with no capture name.
    pub fn new(producer: impl Into<String>) -> Self {
        Self {
            producer: producer.into(),
            capture: None,
        }
    }

    /// Canonical stored text.
    pub(crate) fn text(&self) -> String {
        serde_json::json!([self.producer, self.capture]).to_string()
    }
}

/// How a sighting was assigned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Assignment {
    /// The source (or, with a [`MeasurementKey`], the same measurement under new ids) was
    /// already counted; only its growth was added.
    Replay,
    /// The emitter holding the decoded identity.
    Identity,
    /// The context emitter.
    Context,
    /// The closest fingerprint within tolerance.
    Fingerprint {
        /// Mean normalised error.
        score: f64,
    },
    /// A new emitter.
    Created,
}

/// Why identities collided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictReason {
    /// The context emitter already holds a different identity.
    ContextHoldsOtherIdentity,
    /// The fingerprint matches several emitters with different identities.
    FingerprintMatchesSeveralIdentities,
}

/// Identities that collided on one cluster; nothing was merged. Carries emitter ids only (no
/// identity values), so it is safe to log.
#[derive(Clone, Debug, PartialEq)]
pub struct IdentityConflictReport {
    /// Emitters involved.
    pub emitters: Vec<EmitterId>,
    /// Why.
    pub reason: ConflictReason,
}

/// A recorded merge.
#[derive(Clone, Debug, PartialEq)]
pub struct EmitterMerge {
    /// Absorbed emitter (kept, with `merged_into`).
    pub from: EmitterId,
    /// Surviving emitter.
    pub into: EmitterId,
    /// When.
    pub t: Timestamp,
    /// Why.
    pub reason: String,
    /// The absorbed emitter's count at the merge.
    pub from_count: u64,
    /// The absorbed emitter's identity moved to the survivor.
    pub identity_moved: bool,
}

/// Result of `Repository::record_sighting`.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolution {
    /// The (live) emitter the sighting belongs to.
    pub emitter_id: EmitterId,
    /// How.
    pub assignment: Assignment,
    /// A new emitter was created.
    pub created: bool,
    /// Added to the emitter's count (0 on an identical replay).
    pub count_added: u64,
    /// A merge performed while resolving.
    pub merge: Option<EmitterMerge>,
    /// Identities that collided (reported, not merged).
    pub conflict: Option<IdentityConflictReport>,
    /// A known status appended by the prior.
    pub status_appended: Option<KnownStatus>,
}

/// A known-signal prior's verdict (C17).
#[derive(Clone, Debug, PartialEq)]
pub struct PriorVerdict {
    /// Status.
    pub status: KnownStatus,
    /// Deciding prior record, if any.
    pub prior_ref: Option<String>,
    /// Why.
    pub reason: String,
}

/// A known-signal prior, called when an emitter is created or its family changes. hk-context's
/// `match_known_status` fits as a closure.
pub trait KnownStatusPrior {
    /// Verdict for a family at a centre/bandwidth.
    fn verdict(&self, family: &str, f_center_hz: f64, bandwidth_hz: f64) -> PriorVerdict;
}

impl<F> KnownStatusPrior for F
where
    F: Fn(&str, f64, f64) -> PriorVerdict,
{
    fn verdict(&self, family: &str, f_center_hz: f64, bandwidth_hz: f64) -> PriorVerdict {
        self(family, f_center_hz, bandwidth_hz)
    }
}

/// Restrictiveness rank: unrestricted 0 < own-key-decrypted 1 < metadata-only 2 <
/// restricted-paging = restricted-cellular 3.
pub(crate) fn class_rank(c: ContentClass) -> u8 {
    match c {
        ContentClass::Unrestricted => 0,
        ContentClass::OwnKeyDecrypted => 1,
        ContentClass::MetadataOnly => 2,
        ContentClass::RestrictedPaging | ContentClass::RestrictedCellular => 3,
    }
}

/// The more restrictive of two classes (unrestricted < own-key-decrypted < metadata-only <
/// restricted-paging = restricted-cellular); ties keep `a`.
pub fn most_restrictive(a: ContentClass, b: ContentClass) -> ContentClass {
    if class_rank(b) > class_rank(a) { b } else { a }
}

/// Whether a class can never be opened by a reclassification (CLAUDE.md: never circumvent the
/// security of others' traffic; US law restricts cellular and paging content even unencrypted).
pub fn never_openable(c: ContentClass) -> bool {
    crate::content::content_gating_enabled()
        && matches!(
            c,
            ContentClass::RestrictedCellular | ContentClass::RestrictedPaging
        )
}

/// Whether a tag is an identity-free label (T-036): 1–64 bytes of ASCII letters and `-` `_` `.`
/// `/` `:` or space, with **no digits** and no run of four or more letters that are all hex
/// digits (`a`–`f`). The rule does not depend on stored data, so applying it reveals nothing.
///
/// Identifiers (capcodes, IMSIs, MMSIs, ICAO/PI hex, talkgroups, sensor ids) almost always
/// contain a digit or a hex run, so a label-shaped tag rarely carries one; an identity made only
/// of non-hex letters (an alphabetic alias) passes it. Since T-038 it gates only producer tags on
/// `own-key-decrypted` claims; withheld rows use [`tag_in_vocabulary`] ([the gating
/// rules](self#tags)).
pub fn tag_is_identity_free(tag: &str) -> bool {
    if tag.is_empty() || tag.len() > 64 {
        return false;
    }
    let mut hex_run = 0usize;
    for b in tag.bytes() {
        if b.is_ascii_alphabetic() {
            hex_run = if b.is_ascii_hexdigit() {
                hex_run + 1
            } else {
                0
            };
            if hex_run >= 4 {
                return false;
            }
        } else if b"-_./: ".contains(&b) {
            hex_run = 0;
        } else {
            return false;
        }
    }
    true
}

/// The controlled tag vocabulary (T-038, [the tag rules](self#tags)): the only tags shown or
/// matched on a row whose identity is withheld, and the only tags accepted on an identity no
/// access level reveals. Sorted; exact, case-sensitive match. It holds the band-plan allocation
/// tags (`hk-context` band table), the service families the known-status priors use and a few
/// workflow words. Extend it deliberately: an entry must never be derivable from traffic.
pub const TAG_VOCABULARY: &[&str] = &[
    "33cm",
    "700mhz",
    "800mhz",
    "adsb",
    "aircraft",
    "ais",
    "amateur",
    "artifact",
    "aviation",
    "aws",
    "beacon",
    "bluetooth",
    "broadcast",
    "burst",
    "cellular",
    "continuous",
    "control-channel",
    "data",
    "dme",
    "firstnet",
    "fm-broadcast",
    "galileo-e1",
    "galileo-e5",
    "glonass-g1",
    "gnss",
    "harmonic",
    "hf",
    "ignore",
    "ils",
    "interesting",
    "interference",
    "intermittent",
    "intermod",
    "ism",
    "ism-eu-band",
    "jammer",
    "known",
    "l1",
    "l2",
    "l2c",
    "l5",
    "lte-700",
    "lte-band2",
    "lte-band4",
    "lte-band5",
    "maritime",
    "mine",
    "new",
    "noaa-apt",
    "noaa-wx",
    "noise",
    "out-of-allocation",
    "p25",
    "pager",
    "part15",
    "pcs",
    "public-safety",
    "radiosonde",
    "review",
    "reviewed",
    "satellite",
    "sensor",
    "short-range-device",
    "smr",
    "spur",
    "suspect-artifact",
    "tacan",
    "telemetry",
    "trunk",
    "uhf",
    "unknown",
    "vessel",
    "vhf",
    "vhf-am",
    "voice",
    "vor",
    "watch",
    "weather",
    "weather-satellite",
    "wifi",
    "wifi-5g8",
];

/// Whether a tag is in the controlled vocabulary ([`TAG_VOCABULARY`], T-038).
pub fn tag_in_vocabulary(tag: &str) -> bool {
    TAG_VOCABULARY.binary_search(&tag).is_ok()
}

/// Who may see identity values in inventory output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IdentityAccess {
    /// Default: only identities whose class is `unrestricted`.
    #[default]
    Standard,
    /// The caller asserts the own-traffic authorisation: `own-key-decrypted` identities too.
    /// Restricted and unclassified identities stay withheld.
    OwnTrafficAuthorised,
}

impl IdentityAccess {
    /// Whether an identity of this class is shown in clear. Always `true` unless content gating
    /// is enabled ([`crate::content_gating_enabled`]); then fails closed on `None`.
    pub fn reveals(self, class: Option<ContentClass>) -> bool {
        if !crate::content::content_gating_enabled() {
            return true;
        }
        match class {
            Some(ContentClass::Unrestricted) => true,
            Some(ContentClass::OwnKeyDecrypted) => self == IdentityAccess::OwnTrafficAuthorised,
            _ => false,
        }
    }
}

/// Inventory filter (all set filters must hold) and page.
#[derive(Clone, Debug, PartialEq)]
pub struct InventoryQuery {
    /// Occupied band overlaps this range.
    pub freq: Option<FreqRange>,
    /// First–last-seen span overlaps this window.
    pub time: Option<TimeRange>,
    /// Current known status is one of these (empty = any).
    pub status: Vec<KnownStatus>,
    /// Lifecycle state is one of these (T-078). Empty = candidate or confirmed: deleted entries
    /// are listed only when `Deleted` is asked for.
    pub states: Vec<LifecycleState>,
    /// Carries this tag.
    pub tag: Option<String>,
    /// Identity in this scheme (scheme is metadata; values are never a filter).
    pub identity_scheme: Option<IdentityScheme>,
    /// Current family (latest classification, else fingerprint family).
    pub family: Option<String>,
    /// T-219: whether rows that currently defer to another row (suppressed by a Confirmed entry,
    /// a weaker duplicate, or an attributed receiver artifact) are listed. The default hides
    /// them; their rows, detections, tracks and history are always kept and reachable by id.
    pub relations: crate::relate::RelationVisibility,
    /// Page size (1..=[`MAX_INVENTORY_PAGE`]).
    pub limit: u32,
    /// Rows to skip.
    pub offset: u64,
    /// Identity gating.
    pub access: IdentityAccess,
}

impl Default for InventoryQuery {
    fn default() -> Self {
        Self {
            freq: None,
            time: None,
            status: Vec::new(),
            states: Vec::new(),
            tag: None,
            identity_scheme: None,
            family: None,
            relations: crate::relate::RelationVisibility::default(),
            limit: 100,
            offset: 0,
            access: IdentityAccess::Standard,
        }
    }
}

/// An identity as shown in inventory output.
#[derive(Clone, Debug, PartialEq)]
pub enum InventoryIdentity {
    /// No decoded identity.
    None,
    /// Shown in clear.
    Clear {
        /// Identity.
        identity: DecodedIdentity,
        /// Its class.
        class: ContentClass,
    },
    /// Withheld by content gating: only the scheme is shown.
    Withheld {
        /// Scheme.
        scheme: IdentityScheme,
        /// Class, when known (`None` = unclassified, withheld fail-closed).
        class: Option<ContentClass>,
    },
}

/// One inventory row. When the identity is withheld, `emitter.identity` is `Unknown` and
/// `emitter.tags` keeps only vocabulary labels ([`tag_in_vocabulary`]).
#[derive(Clone, Debug, PartialEq)]
pub struct InventoryEntry {
    /// The emitter (live; identity cleared and tags filtered when withheld).
    pub emitter: Emitter,
    /// Gated identity.
    pub identity: InventoryIdentity,
    /// Current family.
    pub family: Option<String>,
    /// Inventory lifecycle state (T-078).
    pub lifecycle: LifecycleState,
    /// Tags outside the controlled vocabulary were removed from `emitter.tags` because the
    /// identity is withheld (T-036, T-038).
    pub tags_withheld: bool,
}

/// One audited identity reclassification (T-036; `Repository::reclassify_identity`).
#[derive(Clone, Debug, PartialEq)]
pub struct IdentityReclassification {
    /// Emitter that held the identity when it was reclassified.
    pub emitter_id: EmitterId,
    /// Identity scheme (metadata; the value is never part of the record).
    pub scheme: IdentityScheme,
    /// Class before.
    pub old_class: ContentClass,
    /// Class after (`own-key-decrypted` or `unrestricted`).
    pub new_class: ContentClass,
    /// Who authorised it.
    pub author: String,
    /// Why, in the author's words. `None` when the read's access does not reveal `new_class`
    /// (the reason may name the identity).
    pub reason: Option<String>,
    /// When.
    pub t: Timestamp,
}

/// A page of inventory rows, most recently seen first.
#[derive(Clone, Debug, PartialEq)]
pub struct InventoryPage {
    /// Rows.
    pub entries: Vec<InventoryEntry>,
    /// Offset of the next page, if there is one.
    pub next_offset: Option<u64>,
}

/// A classification with its recorded input.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedClassification {
    /// The classification.
    pub classification: Classification,
    /// The observation it ran on, if recorded.
    pub input: Option<LinkTarget>,
    /// Fingerprint feature-set version of the input, if recorded.
    pub feature_set_version: Option<u32>,
    /// Taxonomy the row was written under (T-211; `None` for pre-M3 rows).
    pub taxonomy: Option<crate::classify::TaxonomyRef>,
    /// Deciding stage: stored, or derived for a pre-M3 row ([`crate::classify::ArbRank::legacy`]).
    pub stage: crate::classify::Stage,
    /// Arbitration rank: stored, or derived for a pre-M3 row.
    pub arb_rank: crate::classify::ArbRank,
    /// The full M3 classification (`None` for pre-M3 rows).
    pub detail: Option<crate::classify::Classification>,
}

/// An emitter link with its supersession state.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkRecord {
    /// The link as written.
    pub link: EmitterLink,
    /// The emitter a merge re-pointed it to, if superseded.
    pub superseded_by: Option<EmitterId>,
    /// When it was superseded.
    pub superseded_at: Option<Timestamp>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fsk() -> Fingerprint {
        Fingerprint {
            family: Some("2fsk".into()),
            symbol_rate_hz: Some(4800.0),
            deviation_hz: Some(9600.0),
            period_s: Some(0.12),
            duty_cycle: Some(0.15),
            burst_length_s: Some(0.018),
            ..Fingerprint::new(915.005e6, 36e3)
        }
    }

    #[test]
    fn drifted_centre_and_jittered_features_match() {
        let a = fsk();
        let mut b = fsk();
        b.f_center_hz += 6e3; // < max(9.15 kHz ppm, 9 kHz BW share)
        b.symbol_rate_hz = Some(4830.0);
        b.period_s = Some(0.123);
        b.bandwidth_hz = 42e3;
        let m = a.compare(&b, &Tolerances::default());
        assert!(m.within, "{m:?}");
        assert!(m.score > 0.0 && m.score < 1.0);
        assert_eq!(m.compared, 7);
    }

    #[test]
    fn each_feature_can_split() {
        let tol = Tolerances::default();
        let a = fsk();
        type Mutation = (&'static str, fn(&mut Fingerprint));
        let cases: [Mutation; 8] = [
            ("center", |f| f.f_center_hz += 20e3),
            ("bandwidth", |f| f.bandwidth_hz = 60e3),
            ("family", |f| f.family = Some("ook".into())),
            ("symbol_rate", |f| f.symbol_rate_hz = Some(4900.0)),
            ("deviation", |f| f.deviation_hz = Some(12e3)),
            ("period", |f| f.period_s = Some(0.2)),
            ("burst_length", |f| f.burst_length_s = Some(0.05)),
            ("hop_presence", |f| f.hop_set_hz = vec![915e6, 915.2e6]),
        ];
        for (name, mutate) in cases {
            let mut b = a.clone();
            mutate(&mut b);
            let m = a.compare(&b, &tol);
            assert!(!m.within, "{name}: {m:?}");
            assert_eq!(m.worst, Some(name));
        }
        // Unknown family and missing features are not evidence against a match.
        let mut b = Fingerprint::new(915.004e6, 30e3);
        b.family = Some("unknown".into());
        assert!(a.compare(&b, &tol).within);
    }

    /// T-218: what the exact-family gate buys, and what it costs. **T-233 kept it**, and this
    /// test is byte-for-byte what T-218 wrote, which is the point: the discriminator T-233 built
    /// and measured ([`super::ModulationStructure`]) did not make the ADR's relaxation safe, so
    /// nothing here was allowed to move.
    ///
    /// ADR-0016 §1 proposes comparing families through `taxonomy::family_of` instead of exactly,
    /// so that one emitter labelled `fsk` by the blind estimator and `2fsk` by the demodulator
    /// chain stops splitting into two entries. Measured against these pairs, that change also
    /// removes the *only* thing separating two genuinely different emissions that share a family:
    /// `bpsk` and `qpsk` at the same centre, bandwidth and symbol rate carry no other
    /// distinguishing feature, and neither do `2fsk`/`gfsk` or `am`/`wfm`. Entity resolution would
    /// fold them into one emitter, and an inventory row would then describe two signals.
    ///
    /// So the gate stays exact and the ADR's change is **withdrawn** (T-233; it was deferred by
    /// T-218): blind detection quality outranks contract tidiness. The distinguishing feature the
    /// fingerprint now carries splits *more* than the labels do — it never licenses the looser
    /// comparison, for the reasons `compare_inner` records.
    #[test]
    fn t218_the_family_gate_separates_two_emissions_that_share_a_family() {
        let tol = Tolerances::default();
        let same_but_for_family = |family: &str| Fingerprint {
            family: Some(family.into()),
            symbol_rate_hz: Some(9600.0),
            ..Fingerprint::new(446.1e6, 16e3)
        };
        for (a, b) in [("bpsk", "qpsk"), ("2fsk", "gfsk"), ("am", "wfm")] {
            let m = same_but_for_family(a).compare(&same_but_for_family(b), &tol);
            assert!(
                !m.within,
                "{a} and {b} are one family but two emissions: {m:?}"
            );
            assert_eq!(m.worst, Some("family"), "{a} vs {b}");
        }
        // The cost of that rule, recorded rather than hidden: the same comparison splits one
        // emitter whose producers spell its family at different levels of the taxonomy.
        let split = same_but_for_family("fsk").compare(&same_but_for_family("2fsk"), &tol);
        assert!(!split.within && split.worst == Some("family"));
    }

    fn structured(envelope: f64, sigma: f64) -> Fingerprint {
        Fingerprint {
            family: Some("bpsk".into()),
            symbol_rate_hz: Some(9600.0),
            structure: Some(ModulationStructure {
                envelope_shape: envelope,
                envelope_shape_sigma: sigma,
            }),
            ..Fingerprint::new(446.1e6, 16e3)
        }
    }

    /// T-233: the structure comparison is worth exactly three combined sigmas per dimension, and
    /// that is what [`Tolerances::STRUCTURE_SIGMAS_DEFAULT`] means — not a width fitted to any
    /// pair. Asserted here so the constant cannot drift into one.
    #[test]
    fn t233_structure_compares_in_sigmas_not_in_units() {
        let tol = Tolerances::default();
        assert_eq!(tol.structure_sigmas, Tolerances::STRUCTURE_SIGMAS_DEFAULT);
        assert_eq!(Tolerances::STRUCTURE_SIGMAS_DEFAULT, 3.0);
        // Two observations each stating σ = 0.01 combine to √2·0.01, so the band is 3√2·0.01.
        let band = 3.0 * (0.01_f64 * 0.01 + 0.01 * 0.01).sqrt();
        let a = structured(1.38, 0.01);
        let just_inside = structured(1.38 + band * 0.99, 0.01);
        let just_outside = structured(1.38 + band * 1.01, 0.01);
        assert!(a.compare(&just_inside, &tol).within);
        let m = a.compare(&just_outside, &tol);
        assert!(!m.within, "{m:?}");
        assert_eq!(m.worst, Some("envelope_shape"));
        // The band scales with the *stated* uncertainty: the same difference inside a worse
        // measurement is no evidence at all. A poorly measured field never splits on its own.
        assert!(
            structured(1.38, 0.1)
                .compare(&structured(1.38 + band * 1.01, 0.1), &tol)
                .within
        );
        // A producer that states no uncertainty is not infinitely certain: no sigma, no evidence.
        assert!(
            structured(1.38, 0.0)
                .compare(&structured(1.90, 0.0), &tol)
                .within
        );
    }

    /// T-233: structure is allowed to **split** and never to merge. Whatever it says, a pair the
    /// rest of the fingerprint already keeps apart stays apart, and a pair it matched can only
    /// stop matching — which is what makes it safe to add to entity resolution at all.
    #[test]
    fn t233_structure_can_only_split() {
        let tol = Tolerances::default();
        // Identical structure does not rescue a different family, centre or symbol rate.
        for mutate in [
            (|f: &mut Fingerprint| f.family = Some("qpsk".into())) as fn(&mut Fingerprint),
            |f: &mut Fingerprint| f.f_center_hz += 200e3,
            |f: &mut Fingerprint| f.symbol_rate_hz = Some(20e3),
        ] {
            let a = structured(1.38, 0.01);
            let mut b = structured(1.38, 0.01);
            mutate(&mut b);
            assert!(!a.compare(&b, &tol).within);
        }
        // And a match that stood without structure still stands when only one side carries it:
        // an unmeasured field is not evidence.
        let bare = Fingerprint {
            family: Some("bpsk".into()),
            symbol_rate_hz: Some(9600.0),
            ..Fingerprint::new(446.1e6, 16e3)
        };
        assert!(bare.compare(&structured(1.38, 0.01), &tol).within);
        assert!(bare.compare(&structured(9.99, 0.01), &tol).within);
        // The one thing it adds: two emissions that agree on every label and every other feature,
        // and whose modulations measurably do not, stop being one inventory row.
        let m = structured(1.38, 0.01).compare(&structured(1.20, 0.01), &tol);
        assert!(!m.within, "{m:?}");
        assert_eq!(m.worst, Some("envelope_shape"));
    }

    /// T-233: folding keeps the running mean but never lets the uncertainty shrink below what the
    /// observations actually disagreed by — [`crate::signature::Feat`]'s rule, at this level too.
    #[test]
    fn t233_folding_structure_widens_sigma_on_disagreement() {
        let mut a = structured(1.38, 0.01);
        a.fold(&structured(1.50, 0.01));
        let s = a.structure.unwrap();
        assert!((s.envelope_shape - 1.44).abs() < 1e-9, "{s:?}");
        assert!((s.envelope_shape_sigma - 0.06).abs() < 1e-9, "{s:?}");
        // A fingerprint with no structure takes the observation's.
        let mut bare = Fingerprint::new(446.1e6, 16e3);
        bare.fold(&structured(1.38, 0.01));
        assert_eq!(bare.structure, structured(1.38, 0.01).structure);
        // …and an observation with none leaves the stored one alone.
        bare.fold(&Fingerprint::new(446.1e6, 16e3));
        assert_eq!(bare.structure, structured(1.38, 0.01).structure);
        assert!(bare.is_finite());
    }

    #[test]
    fn t233_structure_survives_a_round_trip_and_is_optional_on_the_wire() {
        let a = structured(1.38, 0.01);
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(Fingerprint::from_value(&json), Some(a));
        // Absent in a stored fingerprint written before T-233: parsed, not rejected.
        let mut old = json;
        old.as_object_mut().unwrap().remove("structure");
        assert_eq!(Fingerprint::from_value(&old).unwrap().structure, None);
        // A fingerprint that carries none does not serialise the key at all.
        let bare = serde_json::to_value(Fingerprint::new(446.1e6, 16e3)).unwrap();
        assert!(bare.get("structure").is_none());
        // Out-of-range values are not a fingerprint.
        let mut bad = structured(1.38, 0.01);
        bad.structure.as_mut().unwrap().envelope_shape = f64::NAN;
        assert!(!bad.is_finite());
    }

    #[test]
    fn hop_sets_compare_by_overlap() {
        let tol = Tolerances::default();
        let mk = |chs: &[f64]| Fingerprint {
            hop_raster_hz: Some(200e3),
            hop_set_hz: chs.to_vec(),
            ..Fingerprint::new(915e6, 1.8e6)
        };
        let a = mk(&[914.2e6, 914.4e6, 914.6e6, 914.8e6, 915.0e6]);
        let b = mk(&[914.4e6, 914.6e6, 914.8e6, 915.0e6, 915.2e6]);
        assert!(a.compare(&b, &tol).within);
        let c = mk(&[914.2e6, 916.4e6, 916.6e6, 916.8e6, 917.0e6]);
        assert!(!a.compare(&c, &tol).within);
    }

    #[test]
    fn fold_is_a_capped_running_mean() {
        let mut a = fsk();
        let mut b = fsk();
        b.f_center_hz += 2e3;
        b.family = Some("unknown".into());
        a.fold(&b);
        assert_eq!(a.f_center_hz, 915.006e6);
        assert_eq!(a.family.as_deref(), Some("2fsk"));
        assert_eq!(a.observations, 2);
        a.observations = 1_000;
        let before = a.f_center_hz;
        a.fold(&b);
        assert!((a.f_center_hz - (before * 16.0 + b.f_center_hz) / 17.0).abs() < 1e-3);
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(Fingerprint::from_value(&json), Some(a));
        let mut old = json;
        old["version"] = 0.into();
        assert_eq!(Fingerprint::from_value(&old), None);
    }
}
