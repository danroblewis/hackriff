//! Plugin message plane (docs/stream-contract.md §9.3): one JSON object per stdout line, parsed
//! into `hk_model::Decode` / `Annotation` with the metadata/content split and the manifest's
//! content class enforced.
//!
//! ```json
//! {"type":"decode","sample_index":123456,"frame_model":"adsb-df17","crc_status":"valid",
//!  "identity":{"scheme":"adsb-icao","value":"a1b2c3"},"metadata":{"icao":"a1b2c3"},
//!  "content":{"callsign":"BAW123"},"content_class":"unrestricted"}
//! {"type":"annotation","value":"adsb","kind":"ground-truth","confidence":1.0,"metadata":{}}
//! {"type":"log","msg":"lost sync"}
//! ```
//!
//! **Class rule (legal guardrail).** The manifest's `output.content_class` is a ceiling. A line
//! without `content_class` gets the manifest class. A line with a class is clamped to the ceiling
//! ([`hk_api::stream::gate::clamp`]): a plugin cannot upgrade itself. An unknown class string
//! fails closed to `metadata-only`.
//!
//! **Time.** The host stamps records: a line's `sample_index` (the input record's sample index) is
//! converted with the input stream's anchor and rate; without one, host arrival time is used.
//! Plugin wall-clock times are not trusted (C22 pitfall).

use hk_api::stream::gate;
use hk_model::{
    Annotation, AnnotationAuthor, AnnotationId, AnnotationKind, AnnotationTarget, ContentClass,
    CrcStatus, Decode, DecodeId, DecodedIdentity, RecordingSpan, SampleTime, Timestamp,
};
use serde_json::{Map, Value};

use crate::host::PluginContext;
use crate::manifest::PluginManifest;

/// One parsed stdout line.
#[derive(Clone, Debug, PartialEq)]
pub enum PluginOutput {
    /// A decode, class already resolved.
    Decode(Decode),
    /// An annotation, class already resolved.
    Annotation(Annotation),
    /// A log line for the plugin's log ring.
    Log(String),
}

/// A parsed line and what the class rule did to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    /// The output.
    pub output: PluginOutput,
    /// The line claimed a less restrictive class than the manifest and was clamped.
    pub clamped: bool,
    /// The line's class string was not a known class (failed closed).
    pub unknown_class: bool,
}

/// Resolves a line's class against the manifest ceiling: `(effective, clamped, unknown)`.
pub fn resolve_class(ceiling: ContentClass, claimed: Option<&Value>) -> (ContentClass, bool, bool) {
    match claimed {
        None | Some(Value::Null) => (ceiling, false, false),
        Some(v) => {
            let parsed = ContentClass::parse_fail_closed(v.as_str());
            let known = v
                .as_str()
                .and_then(|s| serde_json::from_value::<ContentClass>(Value::String(s.into())).ok())
                .is_some();
            let effective = gate::clamp(ceiling, parsed);
            (effective, effective != parsed, !known)
        }
    }
}

fn host_time(obj: &Map<String, Value>, timing: Option<(SampleTime, f64)>) -> Timestamp {
    match (obj.get("sample_index").and_then(Value::as_u64), timing) {
        (Some(index), Some((anchor, rate))) => anchor.time_of(index, rate),
        _ => Timestamp::now(),
    }
}

fn field<T: serde::de::DeserializeOwned>(
    obj: &Map<String, Value>,
    name: &str,
) -> Result<Option<T>, String> {
    match obj.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|e| format!("bad `{name}`: {e}")),
    }
}

/// Parses one stdout line.
pub fn parse_line(
    manifest: &PluginManifest,
    context: &PluginContext,
    timing: Option<(SampleTime, f64)>,
    line: &[u8],
) -> Result<Parsed, String> {
    let value: Value = serde_json::from_slice(line).map_err(|e| format!("not JSON: {e}"))?;
    let Value::Object(obj) = value else {
        return Err("not a JSON object".into());
    };
    let kind = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or("missing `type`")?;
    if kind == "log" {
        let msg = obj.get("msg").and_then(Value::as_str).unwrap_or_default();
        return Ok(Parsed {
            output: PluginOutput::Log(msg.to_owned()),
            clamped: false,
            unknown_class: false,
        });
    }
    let (content_class, clamped, unknown_class) =
        resolve_class(manifest.output.content_class, obj.get("content_class"));
    let metadata = match obj.get("metadata") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(v) => v.clone(),
    };
    let content = obj.get("content").filter(|v| !v.is_null()).cloned();
    let t = host_time(&obj, timing);
    let output = match kind {
        "decode" => PluginOutput::Decode(Decode {
            id: DecodeId::new(),
            demodulation_ref: context.demodulation_ref,
            recording_ref: context.recording_ref,
            decoder_id: manifest.id.clone(),
            decoder_version: manifest.version.clone(),
            frame_model: obj
                .get("frame_model")
                .and_then(Value::as_str)
                .unwrap_or(&manifest.output.schema_id)
                .to_owned(),
            metadata,
            content,
            crc_status: field::<CrcStatus>(&obj, "crc_status")?.unwrap_or(CrcStatus::Unknown),
            identity: field::<DecodedIdentity>(&obj, "identity")?,
            content_class,
            t,
        }),
        "annotation" => {
            let target = if let Some(d) = context.detection_ref {
                AnnotationTarget::Detection(d)
            } else if let Some(r) = context.region {
                AnnotationTarget::Region(r)
            } else if let Some(e) = context.emitter_ref {
                AnnotationTarget::Emitter(e)
            } else if let Some(rec) = context.recording_ref {
                AnnotationTarget::Recording(RecordingSpan {
                    recording_id: rec,
                    sample_start: None,
                    sample_count: None,
                    region: None,
                })
            } else {
                return Err("annotation needs a detection, region, emitter or recording in the plugin context".into());
            };
            let confidence = obj.get("confidence").map_or(Some(1.0), Value::as_f64);
            let confidence = confidence
                .filter(|c| c.is_finite() && (0.0..=1.0).contains(c))
                .ok_or("`confidence` must be a number in 0..=1")?;
            PluginOutput::Annotation(Annotation {
                id: AnnotationId::new(),
                target,
                author: AnnotationAuthor::Decoder,
                author_ref: format!("{}@{}", manifest.id, manifest.version),
                kind: field::<AnnotationKind>(&obj, "kind")?.unwrap_or(AnnotationKind::Label),
                value: obj
                    .get("value")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .ok_or("annotation needs a non-empty `value` label")?
                    .to_owned(),
                metadata,
                content,
                confidence,
                supersedes: None,
                content_class,
                t,
                exported: false,
            })
        }
        other => return Err(format!("unknown `type` {other:?}")),
    };
    Ok(Parsed {
        output,
        clamped,
        unknown_class,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::{DetectionId, IdentityScheme};
    use serde_json::json;

    fn manifest(class: &str) -> PluginManifest {
        PluginManifest::from_json_str(
            &json!({
                "manifest_version": 1, "id": "p", "version": "1", "licence": "MIT",
                "executable": "p",
                "input": {"kind": "channel", "datatype": "cf32_le"},
                "output": {"schema_id": "s/1", "content_class": class}
            })
            .to_string(),
        )
        .unwrap()
    }

    #[test]
    fn class_resolution_never_upgrades() {
        let ceiling = ContentClass::RestrictedPaging;
        assert_eq!(resolve_class(ceiling, None), (ceiling, false, false));
        assert_eq!(
            resolve_class(ceiling, Some(&json!("unrestricted"))),
            (ceiling, true, false)
        );
        assert_eq!(
            resolve_class(ContentClass::Unrestricted, Some(&json!("metadata-only"))),
            (ContentClass::MetadataOnly, false, false)
        );
        assert_eq!(
            resolve_class(ContentClass::Unrestricted, Some(&json!("public"))),
            (ContentClass::MetadataOnly, false, true)
        );
        assert_eq!(
            resolve_class(ContentClass::Unrestricted, Some(&json!(3))),
            (ContentClass::MetadataOnly, false, true)
        );
    }

    #[test]
    fn decode_line_splits_metadata_and_content_and_restamps_time() {
        let m = manifest("unrestricted");
        let anchor = SampleTime {
            sample_index: 0,
            host_time: Timestamp::from_unix_nanos(1_000_000_000),
        };
        let line = br#"{"type":"decode","sample_index":2400000,"frame_model":"adsb-df17","crc_status":"valid",
            "identity":{"scheme":"adsb-icao","value":"a1b2c3"},"metadata":{"icao":"a1b2c3"},
            "content":{"callsign":"BAW123"},"t":5}"#;
        let p = parse_line(&m, &PluginContext::default(), Some((anchor, 2.4e6)), line).unwrap();
        let PluginOutput::Decode(d) = p.output else {
            panic!()
        };
        assert_eq!(
            d.t,
            Timestamp::from_unix_nanos(2_000_000_000),
            "host time, not plugin t"
        );
        assert_eq!(d.decoder_id, "p");
        assert_eq!(d.crc_status, CrcStatus::Valid);
        assert_eq!(d.identity.unwrap().scheme, IdentityScheme::AdsbIcao);
        assert_eq!(d.metadata, json!({"icao": "a1b2c3"}));
        assert_eq!(d.content, Some(json!({"callsign": "BAW123"})));
        assert_eq!(d.content_class, ContentClass::Unrestricted);
    }

    #[test]
    fn malformed_lines_are_errors() {
        let m = manifest("unrestricted");
        let ctx = PluginContext::default();
        for bad in [
            &b"not json"[..],
            b"[1,2]",
            br#"{"no":"type"}"#,
            br#"{"type":"video"}"#,
            br#"{"type":"decode","crc_status":"maybe"}"#,
            br#"{"type":"decode","identity":{"scheme":"nope","value":"x"}}"#,
            br#"{"type":"annotation","value":"x"}"#,
        ] {
            assert!(
                parse_line(&m, &ctx, None, bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let ctx = PluginContext {
            detection_ref: Some(DetectionId::new()),
            ..PluginContext::default()
        };
        assert!(
            parse_line(
                &m,
                &ctx,
                None,
                br#"{"type":"annotation","value":"x","confidence":2}"#
            )
            .is_err()
        );
        let ok = parse_line(
            &m,
            &ctx,
            None,
            br#"{"type":"annotation","value":"adsb","kind":"ground-truth"}"#,
        )
        .unwrap();
        assert!(
            matches!(ok.output, PluginOutput::Annotation(ref a) if a.kind == AnnotationKind::GroundTruth)
        );
    }
}
