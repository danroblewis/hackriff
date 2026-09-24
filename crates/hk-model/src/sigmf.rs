//! SigMF metadata (`.sigmf-meta`) types. Recording and fixture format, docs/07 §2.12 and §3.3.
//!
//! Only the `core:` keys hackriff uses are typed. Every other key, including other extension
//! namespaces, lands in the `extra` map of its object and is written back unchanged, so
//! third-party files round-trip. The `hackriff` extension (docs/sigmf-extension.md) adds:
//!
//! - `hackriff:provenance`: a [`Provenance`] object, on `global` (whole file) or on a capture
//!   (that segment, overriding global).
//! - `hackriff:truth`: a free-form ground-truth object on an annotation (synthetic generator,
//!   valid decodes, hand labels).
//! - `hackriff:annotation`: a human-authored annotation block (T-816) — `authored: true`, author,
//!   provenance stamp, collection; structurally distinct from `hackriff:truth`.
//! - `hackriff:clip_count`: clipped ADC samples within a capture segment. Per segment, not in
//!   provenance, because provenance is deduplicated by value.
//!
//! This module handles metadata only. Sample I/O belongs to the replay source (T-003).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::provenance::Provenance;

/// SigMF specification version written by [`SigmfMeta::new`].
pub const SIGMF_VERSION: &str = "1.2.0";
/// Name of the hackriff extension namespace.
pub const HACKRIFF_EXTENSION: &str = "hackriff";
/// Version of the hackriff extension namespace.
pub const HACKRIFF_EXTENSION_VERSION: &str = "0.1.0";
/// Key for the [`Provenance`] object on `global` or a capture.
pub const PROVENANCE_KEY: &str = "hackriff:provenance";
/// Key for the ground-truth object on an annotation.
pub const TRUTH_KEY: &str = "hackriff:truth";
/// Key for a human-authored annotation block (T-816, docs/25 §5): `authored: true`, the author,
/// the authored-provenance stamp and the collection. Never ground truth.
pub const AUTHORED_ANNOTATION_KEY: &str = "hackriff:annotation";
/// Key for the clipped-sample count of a capture segment.
pub const CLIP_COUNT_KEY: &str = "hackriff:clip_count";

/// Errors reading or writing SigMF metadata.
#[derive(Debug, thiserror::Error)]
pub enum SigmfError {
    /// The metadata file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// File involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The JSON is malformed or violates the typed keys.
    #[error("invalid SigMF metadata: {0}")]
    Json(#[from] serde_json::Error),
}

macro_rules! datatypes {
    ($($(#[$doc:meta])* $variant:ident => $s:literal, $component_bytes:literal, $complex:literal;)+) => {
        /// A SigMF `core:datatype`. Covers the little-endian and 8-bit types. Add big-endian
        /// types when a fixture needs them.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum Datatype {
            $($(#[$doc])* #[serde(rename = $s)] $variant,)+
        }

        impl Datatype {
            /// All supported datatypes.
            pub const ALL: &'static [Datatype] = &[$(Datatype::$variant,)+];

            /// The SigMF datatype string, e.g. `"ci8"`.
            pub const fn as_str(self) -> &'static str {
                match self { $(Datatype::$variant => $s,)+ }
            }

            /// Bytes per scalar component: one of I or Q for complex types.
            pub const fn component_bytes(self) -> usize {
                match self { $(Datatype::$variant => $component_bytes,)+ }
            }

            /// Complex (interleaved I/Q) rather than real samples.
            pub const fn is_complex(self) -> bool {
                match self { $(Datatype::$variant => $complex,)+ }
            }
        }
    };
}

datatypes! {
    /// Real signed 8-bit.
    Ri8 => "ri8", 1, false;
    /// Real unsigned 8-bit.
    Ru8 => "ru8", 1, false;
    /// Complex signed 8-bit. HackRF native IQ.
    Ci8 => "ci8", 1, true;
    /// Complex unsigned 8-bit (RTL-SDR native).
    Cu8 => "cu8", 1, true;
    /// Real signed 16-bit little-endian.
    Ri16Le => "ri16_le", 2, false;
    /// Complex signed 16-bit little-endian.
    Ci16Le => "ci16_le", 2, true;
    /// Real unsigned 16-bit little-endian.
    Ru16Le => "ru16_le", 2, false;
    /// Complex unsigned 16-bit little-endian.
    Cu16Le => "cu16_le", 2, true;
    /// Real signed 32-bit little-endian.
    Ri32Le => "ri32_le", 4, false;
    /// Complex signed 32-bit little-endian.
    Ci32Le => "ci32_le", 4, true;
    /// Real 32-bit float little-endian.
    Rf32Le => "rf32_le", 4, false;
    /// Complex 32-bit float little-endian.
    Cf32Le => "cf32_le", 4, true;
    /// Real 64-bit float little-endian.
    Rf64Le => "rf64_le", 8, false;
    /// Complex 64-bit float little-endian.
    Cf64Le => "cf64_le", 8, true;
}

impl Datatype {
    /// Bytes per sample in the `.sigmf-data` file (both components for complex types).
    pub const fn bytes_per_sample(self) -> usize {
        if self.is_complex() {
            2 * self.component_bytes()
        } else {
            self.component_bytes()
        }
    }
}

impl std::fmt::Display for Datatype {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An entry of `core:extensions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extension {
    /// Namespace name, e.g. `"hackriff"`.
    pub name: String,
    /// Extension version.
    pub version: String,
    /// Readers may ignore the extension.
    pub optional: bool,
}

/// The `global` object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Global {
    /// `core:datatype`.
    #[serde(rename = "core:datatype")]
    pub datatype: Datatype,
    /// `core:version`, the SigMF specification version.
    #[serde(rename = "core:version")]
    pub version: String,
    /// `core:sample_rate`, Hz.
    #[serde(
        rename = "core:sample_rate",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub sample_rate: Option<f64>,
    /// `core:description`.
    #[serde(
        rename = "core:description",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<String>,
    /// `core:author`.
    #[serde(
        rename = "core:author",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub author: Option<String>,
    /// `core:license`, a URL or SPDX identifier. Fixture licences are checked per ADR-0010.
    #[serde(
        rename = "core:license",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub license: Option<String>,
    /// `core:hw`, the capture hardware description.
    #[serde(rename = "core:hw", default, skip_serializing_if = "Option::is_none")]
    pub hw: Option<String>,
    /// `core:recorder`, the software that wrote the recording.
    #[serde(
        rename = "core:recorder",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub recorder: Option<String>,
    /// `core:extensions`.
    #[serde(
        rename = "core:extensions",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub extensions: Vec<Extension>,
    /// `hackriff:provenance` for the whole recording.
    #[serde(
        rename = "hackriff:provenance",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub provenance: Option<Provenance>,
    /// All other keys, preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A `captures` segment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    /// `core:sample_start`, the first sample index of this segment.
    #[serde(rename = "core:sample_start")]
    pub sample_start: u64,
    /// `core:frequency`, centre frequency, Hz.
    #[serde(
        rename = "core:frequency",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub frequency: Option<f64>,
    /// `core:datetime`, ISO-8601 UTC time of `sample_start`, kept as written.
    #[serde(
        rename = "core:datetime",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub datetime: Option<String>,
    /// `hackriff:provenance` for this segment. Overrides the global one.
    #[serde(
        rename = "hackriff:provenance",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub provenance: Option<Provenance>,
    /// `hackriff:clip_count`: clipped ADC samples within this segment, if measured.
    #[serde(
        rename = "hackriff:clip_count",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub clip_count: Option<u64>,
    /// All other keys, preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// An `annotations` entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    /// `core:sample_start`.
    #[serde(rename = "core:sample_start")]
    pub sample_start: u64,
    /// `core:sample_count`.
    #[serde(
        rename = "core:sample_count",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub sample_count: Option<u64>,
    /// `core:freq_lower_edge`, Hz.
    #[serde(
        rename = "core:freq_lower_edge",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub freq_lower_edge: Option<f64>,
    /// `core:freq_upper_edge`, Hz.
    #[serde(
        rename = "core:freq_upper_edge",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub freq_upper_edge: Option<f64>,
    /// `core:label`.
    #[serde(
        rename = "core:label",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub label: Option<String>,
    /// `core:comment`.
    #[serde(
        rename = "core:comment",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub comment: Option<String>,
    /// `hackriff:truth`, a free-form ground-truth object.
    #[serde(
        rename = "hackriff:truth",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub truth: Option<Value>,
    /// All other keys, preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A whole `.sigmf-meta` document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SigmfMeta {
    /// The `global` object.
    pub global: Global,
    /// Capture segments, sorted by `sample_start`.
    #[serde(default)]
    pub captures: Vec<Capture>,
    /// Annotations, sorted by `sample_start`.
    #[serde(default)]
    pub annotations: Vec<Annotation>,
}

impl SigmfMeta {
    /// An empty document at [`SIGMF_VERSION`] with the hackriff extension declared.
    pub fn new(datatype: Datatype) -> Self {
        let mut meta = Self {
            global: Global {
                datatype,
                version: SIGMF_VERSION.to_owned(),
                sample_rate: None,
                description: None,
                author: None,
                license: None,
                hw: None,
                recorder: None,
                extensions: Vec::new(),
                provenance: None,
                extra: Map::new(),
            },
            captures: Vec::new(),
            annotations: Vec::new(),
        };
        meta.declare_hackriff_extension();
        meta
    }

    /// Adds the optional `hackriff` entry to `core:extensions` unless it is already there.
    pub fn declare_hackriff_extension(&mut self) {
        if !self
            .global
            .extensions
            .iter()
            .any(|e| e.name == HACKRIFF_EXTENSION)
        {
            self.global.extensions.push(Extension {
                name: HACKRIFF_EXTENSION.to_owned(),
                version: HACKRIFF_EXTENSION_VERSION.to_owned(),
                optional: true,
            });
        }
    }

    /// Parses a `.sigmf-meta` JSON document.
    pub fn from_json_str(json: &str) -> Result<Self, SigmfError> {
        Ok(serde_json::from_str(json)?)
    }

    /// Serialises to pretty-printed JSON.
    pub fn to_json_string(&self) -> Result<String, SigmfError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Reads and parses a `.sigmf-meta` file.
    pub fn read(path: impl AsRef<Path>) -> Result<Self, SigmfError> {
        let path = path.as_ref();
        let json = fs::read_to_string(path).map_err(|source| SigmfError::Io {
            path: path.to_owned(),
            source,
        })?;
        Self::from_json_str(&json)
    }

    /// Writes the document as pretty-printed JSON with a trailing newline.
    pub fn write(&self, path: impl AsRef<Path>) -> Result<(), SigmfError> {
        let path = path.as_ref();
        let mut json = self.to_json_string()?;
        json.push('\n');
        fs::write(path, json).map_err(|source| SigmfError::Io {
            path: path.to_owned(),
            source,
        })
    }
}

/// The `.sigmf-data` path paired with a `.sigmf-meta` path.
pub fn data_path_for(meta_path: impl AsRef<Path>) -> PathBuf {
    meta_path.as_ref().with_extension("sigmf-data")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ids::CalibrationStateId;
    use crate::provenance::{ClockSource, Tune};
    use crate::time::TimestampMethod;

    fn provenance() -> Provenance {
        Provenance {
            device_id: "hackrf:test".into(),
            tune: Tune {
                center_hz: 1090e6,
                sample_rate_hz: 8e6,
                lna_db: 32.0,
                vga_db: 24.0,
                amp_on: true,
                bandwidth_hz: 7e6,
            },
            overload: false,
            quantisation_limited: false,
            noise_sigma_lsb: None,
            temperature_c: Some(41.5),
            antenna_port: None,
            bias_tee: crate::BiasTee::Unknown,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: Some(CalibrationStateId::new()),
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::HostArrival,
            timestamp_error_budget_ns: None,
            capture_artefacts: Vec::new(),
        }
    }

    /// A third-party-style document with keys from other namespaces at every level.
    const FOREIGN: &str = r#"{
      "global": {
        "core:datatype": "cf32_le",
        "core:version": "1.2.0",
        "core:sample_rate": 48000,
        "core:hw": "some SDR",
        "core:extensions": [{"name": "antenna", "version": "1.0.0", "optional": true}],
        "antenna:model": "discone",
        "core:num_channels": 1
      },
      "captures": [
        {"core:sample_start": 0, "core:frequency": 100100000, "core:datetime": "2026-09-13T12:00:00Z",
         "core:global_index": 7}
      ],
      "annotations": [
        {"core:sample_start": 10, "core:sample_count": 100, "core:freq_lower_edge": 100090000.0,
         "core:freq_upper_edge": 100110000.0, "core:label": "FM", "core:comment": "hand label",
         "core:generator": "IQEngine", "custom:nested": {"a": [1, 2, 3]}}
      ]
    }"#;

    #[test]
    fn parses_core_keys_and_keeps_unknown_keys() {
        let meta = SigmfMeta::from_json_str(FOREIGN).unwrap();
        assert_eq!(meta.global.datatype, Datatype::Cf32Le);
        assert_eq!(meta.global.sample_rate, Some(48000.0));
        assert_eq!(meta.global.hw.as_deref(), Some("some SDR"));
        assert_eq!(meta.global.extra["antenna:model"], "discone");
        assert_eq!(meta.captures[0].frequency, Some(100.1e6));
        assert_eq!(meta.captures[0].extra["core:global_index"], 7);
        let ann = &meta.annotations[0];
        assert_eq!(ann.label.as_deref(), Some("FM"));
        assert_eq!(ann.extra["custom:nested"], json!({"a": [1, 2, 3]}));
        assert!(ann.truth.is_none());
    }

    #[test]
    fn unknown_keys_round_trip() {
        let meta = SigmfMeta::from_json_str(FOREIGN).unwrap();
        let written: Value = serde_json::from_str(&meta.to_json_string().unwrap()).unwrap();
        assert_eq!(written["global"]["antenna:model"], "discone");
        assert_eq!(written["global"]["core:num_channels"], 1);
        assert_eq!(written["captures"][0]["core:global_index"], 7);
        assert_eq!(written["annotations"][0]["core:generator"], "IQEngine");
        assert_eq!(written["annotations"][0]["custom:nested"]["a"][2], 3);
        // No hackriff keys appear in a foreign document that had none.
        assert!(written["global"].get(PROVENANCE_KEY).is_none());
        let reparsed = SigmfMeta::from_json_str(&written.to_string()).unwrap();
        assert_eq!(reparsed, meta);
    }

    #[test]
    fn hackriff_extension_round_trips_through_a_file() {
        let mut meta = SigmfMeta::new(Datatype::Ci8);
        meta.global.sample_rate = Some(8e6);
        meta.global.provenance = Some(provenance());
        meta.captures.push(Capture {
            sample_start: 0,
            frequency: Some(1090e6),
            datetime: Some("2026-09-13T12:00:00.000000Z".into()),
            provenance: Some(provenance()),
            clip_count: Some(7),
            extra: Map::new(),
        });
        meta.annotations.push(Annotation {
            sample_start: 1_000,
            sample_count: Some(960),
            freq_lower_edge: Some(1089e6),
            freq_upper_edge: Some(1091e6),
            label: Some("adsb".into()),
            comment: None,
            truth: Some(json!({"icao": "a1b2c3", "crc_ok": true})),
            extra: Map::new(),
        });

        let dir = std::env::temp_dir().join(format!("hk-model-sigmf-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("adsb.sigmf-meta");
        meta.write(&path).unwrap();
        let back = SigmfMeta::read(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        assert_eq!(back, meta);
        let raw: Value = serde_json::to_value(&back).unwrap();
        assert_eq!(
            raw["global"]["core:extensions"][0]["name"],
            HACKRIFF_EXTENSION
        );
        assert_eq!(raw["global"][PROVENANCE_KEY]["tune"]["center_hz"], 1090e6);
        assert_eq!(
            raw["captures"][0][PROVENANCE_KEY]["timestamp_method"],
            "host-arrival"
        );
        assert_eq!(raw["captures"][0][CLIP_COUNT_KEY], 7);
        assert_eq!(raw["annotations"][0][TRUTH_KEY]["icao"], "a1b2c3");
    }

    #[test]
    fn declaring_the_extension_is_idempotent() {
        let mut meta = SigmfMeta::new(Datatype::Ci8);
        meta.declare_hackriff_extension();
        assert_eq!(meta.global.extensions.len(), 1);
    }

    #[test]
    fn datatype_strings_and_sizes() {
        for dt in Datatype::ALL {
            let json = serde_json::to_string(dt).unwrap();
            assert_eq!(json, format!("\"{}\"", dt.as_str()));
            assert_eq!(serde_json::from_str::<Datatype>(&json).unwrap(), *dt);
        }
        assert_eq!(Datatype::Ci8.bytes_per_sample(), 2);
        assert_eq!(Datatype::Cu8.bytes_per_sample(), 2);
        assert_eq!(Datatype::Ri8.bytes_per_sample(), 1);
        assert_eq!(Datatype::Ri16Le.bytes_per_sample(), 2);
        assert_eq!(Datatype::Ci16Le.bytes_per_sample(), 4);
        assert_eq!(Datatype::Cf32Le.bytes_per_sample(), 8);
        assert!(serde_json::from_str::<Datatype>("\"i8\"").is_err());
    }

    #[test]
    fn missing_required_keys_are_errors() {
        assert!(SigmfMeta::from_json_str(r#"{"global": {"core:version": "1.2.0"}}"#).is_err());
        let no_start = r#"{"global": {"core:datatype": "ci8", "core:version": "1.2.0"},
                           "captures": [{"core:frequency": 1.0}]}"#;
        assert!(SigmfMeta::from_json_str(no_start).is_err());
    }

    #[test]
    fn data_path_pairs_with_meta_path() {
        assert_eq!(
            data_path_for("fixtures/tiny/tone.sigmf-meta"),
            PathBuf::from("fixtures/tiny/tone.sigmf-data")
        );
    }
}
