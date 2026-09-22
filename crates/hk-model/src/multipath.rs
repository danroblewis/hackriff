//! Content-correlated multipath (T-222, C40 content half; AWARE-053): deciding whether two
//! inventory rows are **two emissions** or **one emission arriving over two paths**.
//!
//! # The measurement, and why content is what decides it
//!
//! [`crate::relate`] answers the same family of questions from *geometry*: two rows compete when
//! their bands overlap, and a row is a receiver artefact when it lands on a frequency the mixer
//! arithmetic predicts. Neither test can see the case this module exists for — two rows that do
//! **not** overlap in frequency and sit on no predicted artefact frequency, and are nonetheless the
//! same transmission, reaching the antenna twice over paths of different length.
//!
//! What separates those two rows from two genuinely independent emitters is what they *carry*.
//! Two copies of one transmission carry **identical content, one delayed and attenuated**; two
//! stations of the same service carry different content however alike their family, bandwidth and
//! modulation are. So the deciding measurement is the cross-correlation of the two rows' content
//! series against each other over a range of lags (`hk_dsp::xcorr`), and the lag of the peak is
//! the **path delay**: `Δd = c · τ`.
//!
//! # What the claim says, and what it carefully does not
//!
//! The measured quantities are the **delay** and the **correlation**. `path_difference_m` is the
//! reading of that delay as a passive reflection — the extra distance the second copy travelled —
//! and the honest caveat is that the measurement alone cannot separate a reflection from a relay,
//! a translator or a re-broadcast, all of which produce one content, twice, with a delay. The
//! reasoning says so ([`MultipathFinding::reason`]) and the detail discloses the delay first.
//! Exploration-first: the claim is ranked evidence with its arithmetic on show, reversible, and
//! never a delete of the row that defers (ADR-0015 §11, ADR-0017).
//!
//! # Shared-air physics: this one must NOT read the receive chain
//!
//! T-302 gates every image / harmonic / intermod pairing on [`crate::relate::ReceiveChain`],
//! because such an artefact is manufactured inside **one** front end's mixer. Multipath is the
//! exact opposite case and the gate must not be copied here: a reflection happens in the *air*,
//! before any antenna, so two front ends — or two antenna ports — seeing the same two paths is
//! ordinary, not a contradiction. Nothing in this module reads a device, and that is deliberate.
//!
//! # The guards, all a priori
//!
//! Every threshold below is fixed here and never fitted to a fixture:
//!
//! 1. **Different decoded identities end it.** Two rows naming different transmitters (different
//!    RDS PI, different ADS-B ICAO) are two emissions, whatever their waveforms do. The matching
//!    case is corroboration, never the claim by itself: one programme on two frequencies is a
//!    simulcast — two transmitters — and only the *delay* tells a second path from a second
//!    transmitter.
//! 2. **Too little common observation abstains.** Below [`MULTIPATH_MIN_OVERLAP_S`] of shared
//!    window there is not enough content to correlate, and the answer is "cannot tell", not "no".
//! 3. **The correlation must be high** ([`MULTIPATH_MIN_CORRELATION`]) — this is the whole
//!    evidence — **and must dominate its runner-up** ([`MULTIPATH_MIN_DOMINANCE`]). A periodic
//!    content series correlates with itself at every multiple of its period; a delay that cannot
//!    be pinned to one lag is not a path difference that can be reported.
//! 4. **The delay must be measurable and physical.** Greater than [`MULTIPATH_MIN_LAG_RESOLUTIONS`]
//!    times the measurement's own time resolution (below that, a second path cannot be told from a
//!    simulcast arriving together), and no greater than [`MULTIPATH_MAX_LAG_S`], which is longer
//!    than any path difference this planet offers.
//! 5. **The later copy must be the weaker one.** A reflection loses energy. Where the later
//!    arrival is the stronger, the content is still the same but the echo model does not fit, and
//!    no claim is made rather than a wrong one. Past [`MULTIPATH_MAX_ATTENUATION_DB`] the second
//!    copy is at the floor and the coincidence is not evidence of anything.

use serde::{Deserialize, Serialize};

use crate::emitter::DecodedIdentity;
use crate::ids::EmitterId;
use crate::region::FreqRange;

// ---------------------------------------------------------------------------------------------
// Thresholds. All a priori: fixed here, never fitted to a fixture.
// ---------------------------------------------------------------------------------------------

/// Speed of light in vacuum, m/s — the constant that turns a measured delay into a path
/// difference. The propagation path is not vacuum, so the figure is an upper bound on the extra
/// distance, which is why it is reported with the delay rather than instead of it.
pub const SPEED_OF_LIGHT_M_S: f64 = 299_792_458.0;

/// Smallest peak correlation that counts as "the same content". Well above what two independent
/// content series reach, and below what a noisy, attenuated copy of one transmission loses.
pub const MULTIPATH_MIN_CORRELATION: f64 = 0.75;

/// How far the correlation peak must stand above the best competing peak elsewhere in the lag
/// range before the delay is considered pinned down (`peak / runner_up`).
pub const MULTIPATH_MIN_DOMINANCE: f64 = 1.5;

/// Largest delay that can be a path difference, seconds. 150 ms is ~45 000 km of extra path —
/// longer than a round-the-world echo, so nothing physical is excluded and an absurd lag is.
pub const MULTIPATH_MAX_LAG_S: f64 = 0.150;

/// How many times the measurement's own time resolution the delay must exceed. At or below its
/// resolution the two copies arrived together as far as this receiver can tell, and "together" is
/// a simulcast, not a second path.
pub const MULTIPATH_MIN_LAG_RESOLUTIONS: f64 = 2.0;

/// Shared observation needed before two content series are compared at all, seconds.
pub const MULTIPATH_MIN_OVERLAP_S: f64 = 2.0;

/// How much *stronger* the later arrival may measure than the earlier one before the echo model is
/// refused, dB. Slack for measurement noise only: a reflection does not gain energy.
pub const MULTIPATH_LEVEL_SLACK_DB: f64 = 1.0;

/// Past this suppression the second copy is at the noise floor and the coincidence is not evidence
/// of anything, dB.
pub const MULTIPATH_MAX_ATTENUATION_DB: f64 = 40.0;

// ---------------------------------------------------------------------------------------------
// What is compared
// ---------------------------------------------------------------------------------------------

/// Which content series a correlation was measured on. Only like may be compared with like: a
/// keying envelope and a bit stream are not the same quantity, and correlating them would be
/// arithmetic without a meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentKind {
    /// Measured energy against time in the row's own band: the keying pattern, which is content
    /// for anything bursty, on/off or amplitude-bearing. Built from the detection record, so it
    /// costs no extra demodulation and exists for every row.
    Envelope,
    /// Demodulated audio.
    Audio,
    /// Recovered bits or symbols.
    Bits,
    /// Decoded frame arrivals, matched by payload.
    Frames,
}

impl ContentKind {
    /// Storage/label name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Envelope => "envelope",
            Self::Audio => "audio",
            Self::Bits => "bits",
            Self::Frames => "frames",
        }
    }
}

/// One inventory row, as this rule needs to see it. Everything here is measured; nothing comes
/// from a database of known signals.
#[derive(Clone, Debug, PartialEq)]
pub struct MultipathRow {
    /// The row.
    pub emitter_id: EmitterId,
    /// Measured centre, Hz.
    pub f_center_hz: f64,
    /// Measured occupied bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Latest measured peak level, dBFS, when a linked detection recorded one.
    pub level_dbfs: Option<f64>,
    /// Decoded transmitter identity, when the row holds one.
    pub identity: Option<DecodedIdentity>,
    /// The entry is `confirmed` (as opposed to a candidate).
    pub confirmed: bool,
}

impl MultipathRow {
    /// The measured occupied band.
    pub fn freq(&self) -> FreqRange {
        FreqRange::centered(self.f_center_hz, self.bandwidth_hz)
    }
}

/// The correlation measurement between two rows' content series, as `hk_dsp::xcorr` produced it
/// and the caller converted it to seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContentCorrelation {
    /// Which series were compared.
    pub kind: ContentKind,
    /// Delay of the peak, seconds, **positive when the second row's content arrives after the
    /// first's**.
    pub lag_s: f64,
    /// Peak correlation, −1..=1.
    pub peak: f64,
    /// `peak / runner_up`: how far the peak stands above the best competing lag.
    pub dominance: f64,
    /// The time resolution the series were built at, seconds — the finest delay this measurement
    /// can distinguish from zero.
    pub resolution_s: f64,
    /// Shared observation the correlation was computed over, seconds.
    pub overlap_s: f64,
}

/// Why two rows carrying the same identity are not, by that alone, one emission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdentityAgreement {
    /// Both rows decode the same transmitter identity: corroboration for the correlation, never
    /// the claim by itself (a simulcast shares an identity too).
    Same,
    /// At most one row decoded an identity, so identity says nothing either way.
    Unknown,
}

// ---------------------------------------------------------------------------------------------
// The finding
// ---------------------------------------------------------------------------------------------

/// One measured two-path finding: the same content, twice, with a delay.
#[derive(Clone, Debug, PartialEq)]
pub struct MultipathFinding {
    /// The row that arrived **later and weaker**: the copy, which defers.
    pub echo: EmitterId,
    /// The row that arrived first: the direct path, which is shown.
    pub direct: EmitterId,
    /// Measured delay of the echo behind the direct path, seconds. Always positive.
    pub delay_s: f64,
    /// The finest delay the measurement could distinguish, seconds — the uncertainty on
    /// `delay_s`, and so on `path_difference_m`.
    pub resolution_s: f64,
    /// `c · delay_s`: the extra distance the second copy travelled, **read as a passive
    /// reflection**. An upper bound (the path is not vacuum), and not the only explanation of a
    /// delay — see the module docs.
    pub path_difference_m: f64,
    /// Peak correlation between the two content series.
    pub peak: f64,
    /// How far that peak stood above the best competing lag.
    pub dominance: f64,
    /// How far the echo sits below the direct path, dB.
    pub attenuation_db: f64,
    /// Which series were compared.
    pub kind: ContentKind,
    /// What the decoded identities added.
    pub identity: IdentityAgreement,
    /// Shared observation the correlation was measured over, seconds.
    pub overlap_s: f64,
    /// Frequency separation between the two rows' centres, Hz.
    pub separation_hz: f64,
}

impl MultipathFinding {
    /// The uncertainty on [`Self::path_difference_m`], metres.
    pub fn path_difference_resolution_m(&self) -> f64 {
        self.resolution_s * SPEED_OF_LIGHT_M_S
    }

    /// The reasoning, rendered for display. Every claim carries this, and it never names an
    /// identity value — only that the two agreed.
    pub fn reason(&self) -> String {
        format!(
            "the same content as emitter {}, arriving {:.1} ms later and {:.1} dB weaker: their {} \
             series correlate {:.2} at that one lag ({:.1}x the next best, over {:.1} s in \
             common{}), which two independent emissions do not do. {:.1} ms of delay is {:.0} km \
             of extra path (+/- {:.0} km) read as a reflection; a relay or a re-broadcast of the \
             same programme would measure the same, so the delay is the finding and the distance \
             its interpretation. This row is kept in full and revives if the evidence changes.",
            self.direct,
            self.delay_s * 1e3,
            self.attenuation_db,
            self.kind.as_str(),
            self.peak,
            self.dominance,
            self.overlap_s,
            match self.identity {
                IdentityAgreement::Same => ", and both decode the same transmitter identity",
                IdentityAgreement::Unknown => "",
            },
            self.delay_s * 1e3,
            self.path_difference_m / 1e3,
            self.path_difference_resolution_m() / 1e3,
        )
    }

    /// The arithmetic, disclosed on the relation row.
    pub fn detail(&self) -> serde_json::Value {
        serde_json::json!({
            "rule": "content-multipath",
            "content_kind": self.kind.as_str(),
            "delay_s": self.delay_s,
            "delay_resolution_s": self.resolution_s,
            "path_difference_m": self.path_difference_m,
            "path_difference_resolution_m": self.path_difference_resolution_m(),
            "correlation": self.peak,
            "dominance": self.dominance,
            "attenuation_db": self.attenuation_db,
            "overlap_s": self.overlap_s,
            "separation_hz": self.separation_hz,
            "identity": self.identity,
            "direct": self.direct.to_string(),
        })
    }
}

/// What the rule concluded about one pair.
#[derive(Clone, Debug, PartialEq)]
pub enum MultipathVerdict {
    /// One emission over two paths.
    Related(Box<MultipathFinding>),
    /// Positively two emissions: the evidence says so, and a standing claim is revoked.
    Independent(&'static str),
    /// Not enough to say either way. Nothing is claimed and nothing standing is revoked on this
    /// evidence — an abstention is not a finding.
    Undecidable(&'static str),
}

impl MultipathVerdict {
    /// The finding, when there is one.
    pub fn finding(&self) -> Option<&MultipathFinding> {
        match self {
            Self::Related(f) => Some(f),
            _ => None,
        }
    }

    /// Why, in the words recorded on the relation row.
    pub fn why(&self) -> &'static str {
        match self {
            Self::Related(_) => "same content, delayed and attenuated copy",
            Self::Independent(r) | Self::Undecidable(r) => r,
        }
    }
}

/// **The rule.** Decides whether `a` and `b` are one emission over two paths, from the correlation
/// of their content series and the levels and identities measured on the rows. Guards in the
/// module docs, in the order applied here.
///
/// `corr.lag_s` is signed as the caller measured it: positive when `b`'s content arrives after
/// `a`'s. Which row is the echo follows from that sign, never from which was asked about first.
pub fn content_multipath(
    a: &MultipathRow,
    b: &MultipathRow,
    corr: &ContentCorrelation,
) -> MultipathVerdict {
    if a.emitter_id == b.emitter_id {
        return MultipathVerdict::Undecidable("a row cannot be its own echo");
    }
    // 1. Identity, first and decisive: two named transmitters are two emissions.
    let identity = match (&a.identity, &b.identity) {
        (Some(x), Some(y)) if x != y => {
            return MultipathVerdict::Independent("different decoded identities");
        }
        (Some(_), Some(_)) => IdentityAgreement::Same,
        _ => IdentityAgreement::Unknown,
    };
    // 2. Enough shared window to have measured anything.
    if !corr.overlap_s.is_finite() || corr.overlap_s < MULTIPATH_MIN_OVERLAP_S {
        return MultipathVerdict::Undecidable("too little common observation to compare content");
    }
    if !corr.resolution_s.is_finite() || corr.resolution_s <= 0.0 {
        return MultipathVerdict::Undecidable("no time resolution recorded for the comparison");
    }
    // 3. The correlation itself, then its dominance.
    if !corr.peak.is_finite() || corr.peak < MULTIPATH_MIN_CORRELATION {
        return MultipathVerdict::Independent("content does not correlate at any lag");
    }
    // NaN (no comparison possible) abstains; an infinite dominance — no competing lag at all —
    // passes, which is why this is not `!(dominance >= MIN)`.
    if corr.dominance.is_nan() || corr.dominance < MULTIPATH_MIN_DOMINANCE {
        return MultipathVerdict::Undecidable(
            "the correlation peak does not stand above the competing lags, so no one delay is \
             measured",
        );
    }
    // 4. A delay that is measurable, and physical.
    let lag = corr.lag_s;
    if !lag.is_finite() {
        return MultipathVerdict::Undecidable("no delay measured");
    }
    if lag.abs() <= MULTIPATH_MIN_LAG_RESOLUTIONS * corr.resolution_s {
        return MultipathVerdict::Undecidable(
            "the two copies arrive together within the measurement's time resolution: a second \
             path cannot be told from a second transmitter",
        );
    }
    if lag.abs() > MULTIPATH_MAX_LAG_S {
        return MultipathVerdict::Independent(
            "the delay is longer than any path difference this planet offers",
        );
    }
    // 5. The later copy must be the weaker one.
    let (direct, echo) = if lag > 0.0 { (a, b) } else { (b, a) };
    let (Some(direct_level), Some(echo_level)) = (direct.level_dbfs, echo.level_dbfs) else {
        return MultipathVerdict::Undecidable(
            "no measured level on one of the rows, so the echo could not be told from the direct \
             path",
        );
    };
    let attenuation_db = direct_level - echo_level;
    if attenuation_db < -MULTIPATH_LEVEL_SLACK_DB {
        return MultipathVerdict::Undecidable(
            "the same content, but the later arrival is the stronger one: not a passive echo of \
             the earlier",
        );
    }
    if attenuation_db > MULTIPATH_MAX_ATTENUATION_DB {
        return MultipathVerdict::Undecidable(
            "the second copy is at the noise floor, where the coincidence is not evidence",
        );
    }
    let delay_s = lag.abs();
    MultipathVerdict::Related(Box::new(MultipathFinding {
        echo: echo.emitter_id,
        direct: direct.emitter_id,
        delay_s,
        resolution_s: corr.resolution_s,
        path_difference_m: delay_s * SPEED_OF_LIGHT_M_S,
        peak: corr.peak,
        dominance: corr.dominance,
        attenuation_db: attenuation_db.max(0.0),
        kind: corr.kind,
        identity,
        overlap_s: corr.overlap_s,
        separation_hz: (a.f_center_hz - b.f_center_hz).abs(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emitter::IdentityScheme;

    fn row(seed: u128, f_hz: f64, level: f64) -> MultipathRow {
        MultipathRow {
            emitter_id: EmitterId::from_uuid(uuid::Uuid::from_u128(seed)),
            f_center_hz: f_hz,
            bandwidth_hz: 20e3,
            level_dbfs: Some(level),
            identity: None,
            confirmed: false,
        }
    }

    fn corr(lag_s: f64) -> ContentCorrelation {
        ContentCorrelation {
            kind: ContentKind::Envelope,
            lag_s,
            peak: 0.95,
            dominance: 4.0,
            resolution_s: 1e-3,
            overlap_s: 20.0,
        }
    }

    #[test]
    fn a_delayed_attenuated_copy_is_one_emission_and_the_lag_is_the_path_difference() {
        let (a, b) = (row(1, 100e6, -20.0), row(2, 100.3e6, -28.0));
        let v = content_multipath(&a, &b, &corr(20e-3));
        let f = v.finding().expect("related");
        assert_eq!(f.echo, b.emitter_id, "the later, weaker row defers");
        assert_eq!(f.direct, a.emitter_id);
        assert!((f.delay_s - 20e-3).abs() < 1e-9);
        assert!((f.path_difference_m - 20e-3 * SPEED_OF_LIGHT_M_S).abs() < 1.0);
        assert!((f.attenuation_db - 8.0).abs() < 1e-9);
        assert!(f.reason().contains("same content"));
        assert!(f.detail()["delay_s"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn the_sign_of_the_lag_chooses_the_echo_not_the_argument_order() {
        let (a, b) = (row(1, 100e6, -28.0), row(2, 100.3e6, -20.0));
        let f = content_multipath(&a, &b, &corr(-20e-3))
            .finding()
            .cloned()
            .expect("related");
        assert_eq!(f.direct, b.emitter_id);
        assert_eq!(f.echo, a.emitter_id);
    }

    #[test]
    fn two_stations_of_one_family_whose_content_differs_are_independent() {
        let (a, b) = (row(1, 100e6, -20.0), row(2, 100.3e6, -28.0));
        let mut c = corr(20e-3);
        c.peak = 0.2;
        assert!(matches!(
            content_multipath(&a, &b, &c),
            MultipathVerdict::Independent("content does not correlate at any lag")
        ));
    }

    #[test]
    fn different_decoded_identities_end_it_however_well_the_content_matches() {
        let (mut a, mut b) = (row(1, 100e6, -20.0), row(2, 100.3e6, -28.0));
        a.identity = Some(DecodedIdentity {
            scheme: IdentityScheme::RdsPi,
            value: "1234".into(),
        });
        b.identity = Some(DecodedIdentity {
            scheme: IdentityScheme::RdsPi,
            value: "5678".into(),
        });
        assert!(matches!(
            content_multipath(&a, &b, &corr(20e-3)),
            MultipathVerdict::Independent("different decoded identities")
        ));
    }

    #[test]
    fn a_matching_identity_is_corroboration_and_is_recorded_as_such() {
        let (mut a, mut b) = (row(1, 100e6, -20.0), row(2, 100.3e6, -28.0));
        let id = DecodedIdentity {
            scheme: IdentityScheme::RdsPi,
            value: "1234".into(),
        };
        a.identity = Some(id.clone());
        b.identity = Some(id);
        let f = content_multipath(&a, &b, &corr(20e-3))
            .finding()
            .cloned()
            .expect("related");
        assert_eq!(f.identity, IdentityAgreement::Same);
    }

    #[test]
    fn a_delay_under_the_measurement_resolution_claims_nothing() {
        let (a, b) = (row(1, 100e6, -20.0), row(2, 100.3e6, -28.0));
        let mut c = corr(1.5e-3);
        c.resolution_s = 1e-3;
        assert!(matches!(
            content_multipath(&a, &b, &c),
            MultipathVerdict::Undecidable(_)
        ));
    }

    #[test]
    fn an_ambiguous_peak_claims_nothing() {
        let (a, b) = (row(1, 100e6, -20.0), row(2, 100.3e6, -28.0));
        let mut c = corr(20e-3);
        c.dominance = 1.05;
        assert!(matches!(
            content_multipath(&a, &b, &c),
            MultipathVerdict::Undecidable(_)
        ));
    }

    #[test]
    fn a_stronger_later_copy_is_not_an_echo() {
        let (a, b) = (row(1, 100e6, -28.0), row(2, 100.3e6, -20.0));
        assert!(matches!(
            content_multipath(&a, &b, &corr(20e-3)),
            MultipathVerdict::Undecidable(_)
        ));
    }

    #[test]
    fn an_absurd_delay_is_refused() {
        let (a, b) = (row(1, 100e6, -20.0), row(2, 100.3e6, -28.0));
        assert!(matches!(
            content_multipath(&a, &b, &corr(0.5)),
            MultipathVerdict::Independent(_)
        ));
    }

    #[test]
    fn too_little_common_observation_abstains() {
        let (a, b) = (row(1, 100e6, -20.0), row(2, 100.3e6, -28.0));
        let mut c = corr(20e-3);
        c.overlap_s = 0.5;
        assert!(matches!(
            content_multipath(&a, &b, &c),
            MultipathVerdict::Undecidable(_)
        ));
    }
}
