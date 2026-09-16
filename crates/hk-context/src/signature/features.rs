//! Aggregating measurements into [`EmissionFeatures`] (T-201, ADR-0016 §5).
//!
//! C18 matches against what an emitter has looked like **over many observations**, not against one
//! snapshot: a symbol rate estimated from a single short burst, or a deviation read once at low
//! SNR, is exactly the kind of measurement that would otherwise either invent a false identity or
//! manufacture a conflict with a catalogue entry it really matches.
//!
//! So a producer hands one [`FeatureObservation`] per sighting and the aggregate carries the
//! uncertainty forward: repeated numbers keep the spread they actually showed (never a shrinking
//! standard error — see [`hk_model::signature::Feat`]), and labels and bit patterns vote. The
//! matcher then widens every tolerance by that uncertainty, so aggregation and matching are two
//! halves of one contract.

use hk_model::classify::Classification;
use hk_model::cluster::Fingerprint;
use hk_model::signature::{EmissionFeatures, Feat, field};

/// One sighting's worth of measured fields, offered to the aggregate.
#[derive(Clone, Debug, Default)]
pub struct FeatureObservation {
    /// Measured fields, by [`field`] name.
    pub fields: Vec<(String, Feat)>,
    /// The detection behind it carried a suspect flag (clipped, IMD, image, spur).
    pub suspect: bool,
}

impl FeatureObservation {
    /// An empty observation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a numeric measurement with its 1σ uncertainty (0 when the producer has none).
    pub fn num(mut self, name: &str, value: f64, sigma: f64, method: &str) -> Self {
        if value.is_finite() {
            self.fields
                .push((name.to_owned(), Feat::num(value, sigma, method)));
        }
        self
    }

    /// Adds a numeric measurement only when the producer measured it.
    pub fn maybe_num(self, name: &str, value: Option<f64>, sigma: f64, method: &str) -> Self {
        match value {
            Some(v) => self.num(name, v, sigma, method),
            None => self,
        }
    }

    /// Adds a label measurement.
    pub fn text(mut self, name: &str, text: &str, method: &str) -> Self {
        if !text.trim().is_empty() {
            self.fields
                .push((name.to_owned(), Feat::text(text, method)));
        }
        self
    }

    /// Adds a bit pattern.
    pub fn bits(mut self, name: &str, bits: &str, method: &str) -> Self {
        if !bits.is_empty() && bits.bytes().all(|b| matches!(b, b'0' | b'1')) {
            self.fields
                .push((name.to_owned(), Feat::bits(bits, method)));
        }
        self
    }

    /// Marks the observation as coming from a suspect detection.
    pub fn suspect(mut self, suspect: bool) -> Self {
        self.suspect = suspect;
        self
    }

    /// The fields a [`Fingerprint`] already carries (C18's v1 feature set is a projection of
    /// `EmissionFeatures`, so this is the same measurement read the other way round).
    ///
    /// A fingerprint stores no per-field uncertainty, so each field is offered with `sigma` 0 and
    /// the aggregate learns the spread from how much repeated sightings disagree.
    pub fn from_fingerprint(fp: &Fingerprint, method: &str) -> Self {
        let obs = Self::new()
            .num(field::F_CENTER_HZ, fp.f_center_hz, 0.0, method)
            .maybe_num(field::SYMBOL_RATE_HZ, fp.symbol_rate_hz, 0.0, method)
            .maybe_num(field::DEVIATION_HZ, fp.deviation_hz, 0.0, method)
            .maybe_num(field::PERIOD_S, fp.period_s, 0.0, method)
            .maybe_num(field::DUTY_CYCLE, fp.duty_cycle, 0.0, method)
            .maybe_num(field::BURST_LENGTH_S, fp.burst_length_s, 0.0, method)
            .maybe_num(field::HOP_RASTER_HZ, fp.hop_raster_hz, 0.0, method);
        let obs = if fp.bandwidth_hz > 0.0 {
            obs.num(field::OBW_HZ, fp.bandwidth_hz, 0.0, method)
        } else {
            obs
        };
        let obs = if fp.hop_set_hz.is_empty() {
            obs
        } else {
            obs.num(field::HOP_COUNT, fp.hop_set_hz.len() as f64, 0.0, method)
        };
        match fp.known_family() {
            Some(family) => obs.text(field::FAMILY, family, method),
            None => obs,
        }
    }

    /// Adds the current classification's family, class and measured SNR.
    ///
    /// The family is carried as a *measurement*, not a verdict: the matcher uses it only to skip
    /// entries of a different modulation family, and an `unknown` call skips nothing.
    pub fn with_classification(self, c: &Classification) -> Self {
        let obs = self.text(field::FAMILY, &c.family, "classifier").maybe_num(
            field::SNR_DB,
            c.provenance.snr_db,
            0.0,
            "c13",
        );
        match &c.class {
            Some(class) => obs.text(field::CLASS, &class.label, "classifier"),
            None => obs,
        }
    }
}

/// Folds one observation into an aggregate.
pub fn fold_observation(features: &mut EmissionFeatures, obs: FeatureObservation) {
    features.observe(obs.fields, obs.suspect);
}

/// Builds an aggregate from a sequence of observations.
pub fn aggregate(
    id: impl Into<String>,
    emitter_id: hk_model::EmitterId,
    t: hk_model::Timestamp,
    observations: impl IntoIterator<Item = FeatureObservation>,
) -> EmissionFeatures {
    let mut features = EmissionFeatures::new(id, emitter_id, t);
    for obs in observations {
        fold_observation(&mut features, obs);
    }
    features
}
