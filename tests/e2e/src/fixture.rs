//! A replayable SigMF fixture and its ground truth.
//!
//! Truth comes from `hackriff:truth` annotation objects (docs/sigmf-extension.md). The synthetic
//! generator gives every object a `role` and `kind` (see `py/README.md` for the fields per kind).
//! Annotations without a `role` — hand labels, third-party archives, older fixtures — load as
//! [`Role::Unlabelled`] with `kind` taken from `hackriff:truth.kind` or `core:label`.

use std::path::{Path, PathBuf};

use hk_model::sigmf::{Annotation, Capture, SigmfError, SigmfMeta, data_path_for};
use serde_json::Value;

use crate::samples::{self, Cf32, SampleError};

/// Errors loading a fixture.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// The metadata could not be read or parsed.
    #[error(transparent)]
    Sigmf(#[from] SigmfError),
    /// Replay needs `core:sample_rate`.
    #[error("{0}: core:sample_rate is required for replay")]
    NoSampleRate(PathBuf),
    /// A `hackriff:truth` object is malformed.
    #[error("{path}: annotation {index}: {message}")]
    BadTruth {
        /// Metadata file.
        path: PathBuf,
        /// Annotation index in the file.
        index: usize,
        /// What is wrong.
        message: String,
    },
    /// The sample data could not be read.
    #[error(transparent)]
    Samples(#[from] SampleError),
}

/// The `role` of a truth object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// Whole-file generator description (scenario, seed, params, conventions).
    Scenario,
    /// A real signal a detector should find.
    Emission,
    /// A receiver artefact (spur, IM3, IQ image, DC, overload): explains a detection, not an emitter.
    Artefact,
    /// A region of known noise floor.
    Floor,
    /// A change such as a floor step.
    Event,
    /// No `role` key (hand labels, third-party annotations).
    Unlabelled,
}

impl Role {
    fn parse(s: Option<&str>) -> Result<Self, String> {
        Ok(match s {
            None => Role::Unlabelled,
            Some("scenario") => Role::Scenario,
            Some("emission") => Role::Emission,
            Some("artefact") => Role::Artefact,
            Some("floor") => Role::Floor,
            Some("event") => Role::Event,
            Some(other) => return Err(format!("unknown truth role {other:?}")),
        })
    }
}

/// One annotation as a time/frequency box plus its truth object.
#[derive(Clone, Debug, PartialEq)]
pub struct TruthItem {
    /// Index into `meta.annotations`.
    pub annotation_index: usize,
    /// Truth role.
    pub role: Role,
    /// Truth kind, e.g. `fsk-burst`, `noise-floor`, `spur`.
    pub kind: String,
    /// `core:label`.
    pub label: Option<String>,
    /// `core:sample_start`.
    pub sample_start: u64,
    /// `core:sample_count` (0 when absent).
    pub sample_count: u64,
    /// Box start, seconds from the first sample of the file.
    pub t_start_s: f64,
    /// Box end, seconds.
    pub t_end_s: f64,
    /// Lower frequency edge, absolute Hz.
    pub f_lo_hz: f64,
    /// Upper frequency edge, absolute Hz.
    pub f_hi_hz: f64,
    /// The `hackriff:truth` object (`Null` when absent).
    pub value: Value,
}

impl TruthItem {
    fn from_annotation(
        index: usize,
        ann: &Annotation,
        meta: &SigmfMeta,
        sample_rate: f64,
    ) -> Result<Self, String> {
        let value = ann.truth.clone().unwrap_or(Value::Null);
        if !(value.is_null() || value.is_object()) {
            return Err("hackriff:truth must be an object".into());
        }
        let role = Role::parse(value.get("role").and_then(Value::as_str))?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| ann.label.clone())
            .unwrap_or_default();
        let (f_lo_hz, f_hi_hz) = match (ann.freq_lower_edge, ann.freq_upper_edge) {
            (Some(lo), Some(hi)) => (lo, hi),
            _ => match capture_at(meta, ann.sample_start).and_then(|c| c.frequency) {
                Some(fc) => (fc - sample_rate / 2.0, fc + sample_rate / 2.0),
                None => (f64::NEG_INFINITY, f64::INFINITY),
            },
        };
        let sample_count = ann.sample_count.unwrap_or(0);
        Ok(Self {
            annotation_index: index,
            role,
            kind,
            label: ann.label.clone(),
            sample_start: ann.sample_start,
            sample_count,
            t_start_s: ann.sample_start as f64 / sample_rate,
            t_end_s: (ann.sample_start + sample_count) as f64 / sample_rate,
            f_lo_hz,
            f_hi_hz,
            value,
        })
    }

    /// A truth field by key (`"symbol_rate_bd"`) or JSON pointer (`"/rds/pi_hex"`).
    pub fn get(&self, key: &str) -> Option<&Value> {
        if key.starts_with('/') {
            self.value.pointer(key)
        } else {
            self.value.get(key)
        }
    }

    /// A numeric truth field.
    pub fn f64(&self, key: &str) -> Option<f64> {
        self.get(key)?.as_f64()
    }

    /// A string truth field.
    pub fn str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    /// A boolean truth field.
    pub fn bool(&self, key: &str) -> Option<bool> {
        self.get(key)?.as_bool()
    }

    /// A numeric truth field that must exist; panics naming the field and kind otherwise.
    pub fn expect_f64(&self, key: &str) -> f64 {
        self.f64(key).unwrap_or_else(|| {
            panic!(
                "truth {} (annotation {}) has no numeric {key}",
                self.kind, self.annotation_index
            )
        })
    }

    /// `(identity.type, identity.value)`, e.g. `("icao", "a0b1c2")`.
    pub fn identity(&self) -> Option<(&str, &str)> {
        Some((self.str("/identity/type")?, self.str("/identity/value")?))
    }

    /// Box centre frequency, Hz.
    pub fn center_hz(&self) -> f64 {
        (self.f_lo_hz + self.f_hi_hz) / 2.0
    }

    /// Box width, Hz.
    pub fn bandwidth_hz(&self) -> f64 {
        self.f_hi_hz - self.f_lo_hz
    }
}

fn capture_at(meta: &SigmfMeta, sample: u64) -> Option<&Capture> {
    meta.captures
        .iter()
        .rev()
        .find(|c| c.sample_start <= sample)
}

/// A SigMF recording with its truth, ready to replay.
#[derive(Clone, Debug)]
pub struct Fixture {
    /// The `.sigmf-meta` path.
    pub meta_path: PathBuf,
    /// Parsed metadata.
    pub meta: SigmfMeta,
    /// `core:sample_rate`, Hz.
    pub sample_rate: f64,
    /// Every annotation, in file order.
    pub truth: Vec<TruthItem>,
}

impl Fixture {
    /// Loads a `.sigmf-meta` file and its truth annotations.
    pub fn load(meta_path: impl AsRef<Path>) -> Result<Self, FixtureError> {
        let meta_path = meta_path.as_ref().to_owned();
        let meta = SigmfMeta::read(&meta_path)?;
        let sample_rate = meta
            .global
            .sample_rate
            .ok_or_else(|| FixtureError::NoSampleRate(meta_path.clone()))?;
        let truth = meta
            .annotations
            .iter()
            .enumerate()
            .map(|(index, ann)| {
                TruthItem::from_annotation(index, ann, &meta, sample_rate).map_err(|message| {
                    FixtureError::BadTruth {
                        path: meta_path.clone(),
                        index,
                        message,
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            meta_path,
            meta,
            sample_rate,
            truth,
        })
    }

    /// The paired `.sigmf-data` path.
    pub fn data_path(&self) -> PathBuf {
        data_path_for(&self.meta_path)
    }

    /// Number of samples in the data file.
    pub fn n_samples(&self) -> Result<u64, SampleError> {
        samples::sample_count(&self.data_path(), self.meta.global.datatype)
    }

    /// All samples (via the T-003 seam reader).
    pub fn samples(&self) -> Result<Vec<Cf32>, SampleError> {
        samples::read_samples(&self.data_path(), self.meta.global.datatype, 0, None)
    }

    /// `count` samples from `start` (via the T-003 seam reader).
    pub fn samples_range(&self, start: u64, count: u64) -> Result<Vec<Cf32>, SampleError> {
        samples::read_samples(
            &self.data_path(),
            self.meta.global.datatype,
            start,
            Some(count),
        )
    }

    /// The capture segment containing `sample`.
    pub fn capture_at(&self, sample: u64) -> Option<&Capture> {
        capture_at(&self.meta, sample)
    }

    /// `core:frequency` of the capture containing `sample`.
    pub fn center_hz_at(&self, sample: u64) -> Option<f64> {
        self.capture_at(sample)?.frequency
    }

    /// The synthetic generator's scenario object, if this is a generated fixture.
    pub fn scenario(&self) -> Option<&TruthItem> {
        self.truth.iter().find(|t| t.role == Role::Scenario)
    }

    /// Use-case IDs recorded by the generator (empty for non-synthetic fixtures).
    pub fn use_cases(&self) -> Vec<String> {
        self.scenario()
            .and_then(|s| s.get("use_cases"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Truth items with a role.
    pub fn with_role(&self, role: Role) -> Vec<&TruthItem> {
        self.truth.iter().filter(|t| t.role == role).collect()
    }

    /// Truth items of a kind.
    pub fn of_kind(&self, kind: &str) -> Vec<&TruthItem> {
        self.truth.iter().filter(|t| t.kind == kind).collect()
    }

    /// Emissions: what detectors must find.
    pub fn emissions(&self) -> Vec<&TruthItem> {
        self.with_role(Role::Emission)
    }

    /// Artefacts: what may be detected but must not become emitters.
    pub fn artefacts(&self) -> Vec<&TruthItem> {
        self.with_role(Role::Artefact)
    }
}
