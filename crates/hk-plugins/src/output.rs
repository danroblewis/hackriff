//! Plugin message plane (docs/stream-contract.md §9.3): one JSON object per stdout line, parsed
//! into `hk_model::Decode` / `Annotation` with the metadata/content split and the content class
//! enforced.
//!
//! ```json
//! {"type":"decode","sample_index":123456,"frame_model":"adsb-df17","crc_status":"valid",
//!  "identity":{"scheme":"adsb-icao","value":"a1b2c3"},"metadata":{"icao":"a1b2c3"},
//!  "content":{"callsign":"BAW123"},"content_class":"unrestricted"}
//! {"type":"annotation","value":"adsb","kind":"ground-truth","confidence":1.0,"metadata":{}}
//! {"type":"log","msg":"lost sync"}
//! ```
//!
//! **Class rule (legal guardrail).** The host passes a *ceiling*: the manifest's
//! `output.content_class` clamped to the input channel's class. A line without `content_class`
//! gets the ceiling. A line with a class is clamped to it ([`hk_stream::gate::clamp`]): a plugin
//! cannot upgrade itself. An unknown class string fails closed to `metadata-only`.
//!
//! **Metadata allowlist (legal guardrail).** When a line's effective class forbids content,
//! everything outside `content` is reduced to what the manifest's [`MetadataPolicy`] allows
//! ([`hk_stream::policy::sanitize_decode`] / [`hk_stream::policy::sanitize_annotation`], shared
//! with in-process producers and egress): unlisted or mistyped metadata keys are dropped,
//! `frame_model` and annotation labels not on the allowlist become `output.schema_id`, an
//! identity that does not match the allowlisted shape is dropped, and annotation `confidence` is
//! rounded to 0.01. No policy means an empty allowlist. `content` itself is refused by the
//! repository and the stream gate.
//!
//! **Sample-index bound (legal guardrail).** Under a content-forbidding class, a line's
//! `sample_index` (which becomes the row's time) must lie within the input offered to the plugin;
//! otherwise the line is refused with [`SAMPLE_INDEX_OUT_OF_RANGE`] and the host counts it.
//!
//! **Errors never echo values.** Error strings name the field, never the offending text, so they
//! can go to the log ring whatever the class.
//!
//! **Time.** The host stamps records: a line's `sample_index` (the input record's sample index) is
//! converted with the input stream's anchor and rate; without one, host arrival time is used.
//! Plugin wall-clock times are not trusted (C22 pitfall).

use hk_model::{
    Annotation, AnnotationAuthor, AnnotationId, AnnotationKind, AnnotationTarget, ContentClass,
    CrcStatus, Decode, DecodeId, DecodedIdentity, RecordingSpan, SampleTime, Timestamp,
};
use hk_stream::{gate, policy};
use serde_json::{Map, Value};

use crate::host::PluginContext;
use crate::manifest::{MetadataPolicy, PluginManifest};

pub use hk_stream::policy::sanitize_metadata;

/// The error [`parse_line`] returns for a restricted line whose `sample_index` is outside the
/// input offered to the plugin (the host counts these separately from malformed lines).
pub const SAMPLE_INDEX_OUT_OF_RANGE: &str =
    "`sample_index` outside the input offered to the plugin";

/// One parsed stdout line.
#[derive(Clone, Debug, PartialEq)]
pub enum PluginOutput {
    /// A decode, class resolved and metadata sanitised.
    Decode(Decode),
    /// An annotation, class resolved and metadata sanitised.
    Annotation(Annotation),
    /// A log line. The host stores it only when the ceiling permits content.
    Log(String),
}

/// A parsed line and what the class rules did to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    /// The output.
    pub output: PluginOutput,
    /// The line claimed a less restrictive class than the ceiling and was clamped.
    pub clamped: bool,
    /// The line's class string was not a known class (failed closed).
    pub unknown_class: bool,
    /// Fields dropped or replaced by the metadata allowlist.
    pub sanitized: u64,
}

/// Resolves a line's class against a ceiling: `(effective, clamped, unknown)`.
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

/// Deserialises an optional field; the error names the field only (never the value).
fn field<T: serde::de::DeserializeOwned>(
    obj: &Map<String, Value>,
    name: &str,
) -> Result<Option<T>, String> {
    match obj.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|_| format!("bad `{name}`")),
    }
}

/// Parses one stdout line under `ceiling` (manifest class clamped to the input class).
///
/// `input_range` is the inclusive sample-index range of the input offered to the plugin so far
/// (`None`: nothing offered). Under a content-forbidding class a present `sample_index` outside
/// it (or not an unsigned integer) refuses the line with [`SAMPLE_INDEX_OUT_OF_RANGE`]; under a
/// permitting class it is not checked.
pub fn parse_line(
    manifest: &PluginManifest,
    ceiling: ContentClass,
    context: &PluginContext,
    timing: Option<(SampleTime, f64)>,
    input_range: Option<(u64, u64)>,
    line: &[u8],
) -> Result<Parsed, String> {
    let value: Value = serde_json::from_slice(line)
        .map_err(|e| format!("not JSON (line {}, column {})", e.line(), e.column()))?;
    let Value::Object(obj) = value else {
        return Err("not a JSON object".into());
    };
    let kind = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or("missing or non-string `type`")?;
    if kind == "log" {
        let msg = obj.get("msg").and_then(Value::as_str).unwrap_or_default();
        return Ok(Parsed {
            output: PluginOutput::Log(msg.to_owned()),
            clamped: false,
            unknown_class: false,
            sanitized: 0,
        });
    }
    if kind != "decode" && kind != "annotation" {
        return Err("unknown `type`".into());
    }
    let (content_class, clamped, unknown_class) = resolve_class(ceiling, obj.get("content_class"));
    if !content_class.permits_content() {
        match obj.get("sample_index") {
            None | Some(Value::Null) => {}
            Some(v) => {
                let inside = v
                    .as_u64()
                    .zip(input_range)
                    .is_some_and(|(i, (lo, hi))| (lo..=hi).contains(&i));
                if !inside {
                    return Err(SAMPLE_INDEX_OUT_OF_RANGE.into());
                }
            }
        }
    }
    let policy: Option<&MetadataPolicy> = manifest.output.metadata_policy.as_ref();
    let schema_id = manifest.output.schema_id.as_str();
    let metadata = match obj.get("metadata") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(v) => v.clone(),
    };
    let content = obj.get("content").filter(|v| !v.is_null()).cloned();
    let t = host_time(&obj, timing);
    let (output, sanitized) = if kind == "decode" {
        let mut decode = Decode {
            id: DecodeId::new(),
            demodulation_ref: context.demodulation_ref,
            recording_ref: context.recording_ref,
            decoder_id: manifest.id.clone(),
            decoder_version: manifest.version.clone(),
            frame_model: obj
                .get("frame_model")
                .and_then(Value::as_str)
                .unwrap_or(schema_id)
                .to_owned(),
            metadata,
            content,
            crc_status: field::<CrcStatus>(&obj, "crc_status")?.unwrap_or(CrcStatus::Unknown),
            identity: field::<DecodedIdentity>(&obj, "identity")?,
            content_class,
            t,
        };
        let n = policy::sanitize_decode(policy, schema_id, &mut decode);
        (PluginOutput::Decode(decode), n)
    } else {
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
            return Err(
                "annotation needs a detection, region, emitter or recording in the plugin context"
                    .into(),
            );
        };
        let confidence = obj.get("confidence").map_or(Some(1.0), Value::as_f64);
        let confidence = confidence
            .filter(|c| c.is_finite() && (0.0..=1.0).contains(c))
            .ok_or("`confidence` must be a number in 0..=1")?;
        let value = obj
            .get("value")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .ok_or("annotation needs a non-empty `value` label")?
            .to_owned();
        let mut annotation = Annotation {
            id: AnnotationId::new(),
            target,
            author: AnnotationAuthor::Decoder,
            author_ref: format!("{}@{}", manifest.id, manifest.version),
            kind: field::<AnnotationKind>(&obj, "kind")?.unwrap_or(AnnotationKind::Label),
            value,
            metadata,
            content,
            confidence,
            supersedes: None,
            content_class,
            t,
            exported: false,
        };
        let n = policy::sanitize_annotation(policy, schema_id, &mut annotation);
        (PluginOutput::Annotation(annotation), n)
    };
    Ok(Parsed {
        output,
        clamped,
        unknown_class,
        sanitized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::{DetectionId, IdentityScheme};
    use serde_json::json;

    fn manifest(output: Value) -> PluginManifest {
        PluginManifest::from_json_str(
            &json!({
                "manifest_version": 1, "id": "p", "version": "1", "licence": "MIT",
                "executable": "p",
                "input": {"kind": "channel", "datatype": "cf32_le"},
                "output": output
            })
            .to_string(),
        )
        .unwrap()
    }

    fn open() -> PluginManifest {
        manifest(json!({"schema_id": "s/1", "content_class": "unrestricted"}))
    }

    fn pager() -> PluginManifest {
        manifest(json!({
            "schema_id": "s/1", "content_class": "restricted-paging",
            "metadata_keys": {"capcode": {"type": "digits", "max_len": 7}, "function": {"type": "integer"}},
            "frame_models": ["pocsag"], "labels": ["pocsag"],
            "identity": {"scheme": "other:pocsag-capcode", "charset": "digits", "max_len": 7}
        }))
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
        let m = open();
        let anchor = SampleTime {
            sample_index: 0,
            host_time: Timestamp::from_unix_nanos(1_000_000_000),
        };
        let line = br#"{"type":"decode","sample_index":2400000,"frame_model":"adsb-df17","crc_status":"valid",
            "identity":{"scheme":"adsb-icao","value":"a1b2c3"},"metadata":{"icao":"a1b2c3"},
            "content":{"callsign":"BAW123"},"t":5}"#;
        let p = parse_line(
            &m,
            ContentClass::Unrestricted,
            &PluginContext::default(),
            Some((anchor, 2.4e6)),
            None,
            line,
        )
        .unwrap();
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
        assert_eq!(p.sanitized, 0);
    }

    #[test]
    fn restricted_input_ceiling_applies_to_an_unrestricted_manifest() {
        // P1: the manifest says unrestricted, the channel is restricted-paging.
        let line = br#"{"type":"decode","frame_model":"SMUG-FM","identity":{"scheme":"adsb-icao","value":"a1b2c3"},
            "metadata":{"icao":"a1b2c3","text":"SMUG"},"content":{"text":"SMUG"}}"#;
        let p = parse_line(
            &open(),
            ContentClass::RestrictedPaging,
            &PluginContext::default(),
            None,
            None,
            line,
        )
        .unwrap();
        let PluginOutput::Decode(d) = p.output else {
            panic!()
        };
        assert_eq!(d.content_class, ContentClass::RestrictedPaging);
        assert_eq!(d.metadata, json!({}), "no allowlist: nothing survives");
        assert_eq!(d.frame_model, "s/1");
        assert_eq!(d.identity, None);
        assert_eq!(p.sanitized, 4);
    }

    #[test]
    fn allowlist_removes_every_smuggling_path_but_keeps_typed_metadata() {
        let m = pager();
        let ceiling = ContentClass::RestrictedPaging;
        let ctx = PluginContext {
            detection_ref: Some(DetectionId::new()),
            ..PluginContext::default()
        };
        let line = br#"{"type":"decode","frame_model":"SMUG-FM","identity":{"scheme":"other:pocsag-capcode","value":"SMUG"},
            "metadata":{"text":"SMUG","capcode":"1234567","function":"SMUG","nested":{"a":"SMUG"}},"content":{"text":"SMUG"}}"#;
        let p = parse_line(&m, ceiling, &ctx, None, None, line).unwrap();
        let PluginOutput::Decode(d) = &p.output else {
            panic!()
        };
        assert_eq!(d.metadata, json!({"capcode": "1234567"}));
        assert_eq!(d.frame_model, "s/1");
        assert_eq!(d.identity, None);
        let text = serde_json::to_string(&(&d.metadata, &d.frame_model, &d.identity)).unwrap();
        assert!(!text.contains("SMUG"), "{text}");

        let ok = br#"{"type":"decode","frame_model":"pocsag","identity":{"scheme":"other:pocsag-capcode","value":"1234567"},"metadata":{"function":2}}"#;
        let PluginOutput::Decode(d) = parse_line(&m, ceiling, &ctx, None, None, ok)
            .unwrap()
            .output
        else {
            panic!()
        };
        assert_eq!(d.frame_model, "pocsag");
        assert_eq!(d.identity.unwrap().value, "1234567");
        assert_eq!(d.metadata, json!({"function": 2}));

        let ann = br#"{"type":"annotation","value":"SMUG-LABEL","metadata":{"text":"SMUG"}}"#;
        let PluginOutput::Annotation(a) = parse_line(&m, ceiling, &ctx, None, None, ann)
            .unwrap()
            .output
        else {
            panic!()
        };
        assert_eq!(a.value, "s/1");
        assert_eq!(a.metadata, json!({}));

        // Under a permitting class nothing is sanitised.
        let PluginOutput::Annotation(a) =
            parse_line(&open(), ContentClass::Unrestricted, &ctx, None, None, ann)
                .unwrap()
                .output
        else {
            panic!()
        };
        assert_eq!(a.value, "SMUG-LABEL");
    }

    /// N1 (unit level): a restricted line's `sample_index` must lie within the input offered, and
    /// its annotation confidence is rounded to 0.01; neither applies under a permitting class.
    #[test]
    fn restricted_sample_index_is_bounded_and_confidence_quantised() {
        let m = pager();
        let ceiling = ContentClass::RestrictedPaging;
        let ctx = PluginContext {
            detection_ref: Some(DetectionId::new()),
            ..PluginContext::default()
        };
        let at = |index: Value| {
            json!({"type": "decode", "sample_index": index})
                .to_string()
                .into_bytes()
        };
        let range = Some((1000, 2000));
        assert!(parse_line(&m, ceiling, &ctx, None, range, &at(json!(1000))).is_ok());
        assert!(parse_line(&m, ceiling, &ctx, None, range, &at(json!(2000))).is_ok());
        for bad in [
            json!(999),
            json!(2001),
            json!(u32::from_be_bytes(*b"PAGE")),
            json!("1500"),
            json!(-1),
        ] {
            assert_eq!(
                parse_line(&m, ceiling, &ctx, None, range, &at(bad.clone())),
                Err(SAMPLE_INDEX_OUT_OF_RANGE.to_owned()),
                "{bad}"
            );
        }
        assert_eq!(
            parse_line(&m, ceiling, &ctx, None, None, &at(json!(0))),
            Err(SAMPLE_INDEX_OUT_OF_RANGE.to_owned()),
            "nothing offered yet"
        );
        assert!(
            parse_line(&m, ceiling, &ctx, None, None, br#"{"type":"decode"}"#).is_ok(),
            "no sample_index: host arrival time"
        );
        assert!(
            parse_line(
                &open(),
                ContentClass::Unrestricted,
                &ctx,
                None,
                None,
                &at(json!(u32::from_be_bytes(*b"PAGE")))
            )
            .is_ok(),
            "permitting classes are not bounded"
        );

        let ann = br#"{"type":"annotation","value":"pocsag","confidence":0.80657169}"#;
        let conf = |m: &PluginManifest, class| match parse_line(m, class, &ctx, None, None, ann)
            .unwrap()
            .output
        {
            PluginOutput::Annotation(a) => a.confidence,
            other => panic!("{other:?}"),
        };
        assert_eq!(conf(&m, ceiling), 0.81);
        assert_eq!(conf(&open(), ContentClass::Unrestricted), 0.80657169);
    }

    #[test]
    fn malformed_lines_are_errors_that_never_echo_values() {
        let m = open();
        let ctx = PluginContext::default();
        for bad in [
            &b"not json SMUG"[..],
            b"[1,2]",
            br#"{"no":"type"}"#,
            br#"{"type":"SMUG"}"#,
            br#"{"type":"decode","crc_status":"SMUG"}"#,
            br#"{"type":"decode","identity":{"scheme":"SMUG","value":"x"}}"#,
            br#"{"type":"decode","identity":"SMUG"}"#,
            br#"{"type":"annotation","value":"x","kind":"SMUG"}"#,
            br#"{"type":"annotation","value":"x"}"#,
        ] {
            let err = parse_line(&m, ContentClass::Unrestricted, &ctx, None, None, bad)
                .expect_err(&String::from_utf8_lossy(bad));
            assert!(!err.contains("SMUG"), "{err}");
        }
        let ctx = PluginContext {
            detection_ref: Some(DetectionId::new()),
            ..PluginContext::default()
        };
        assert!(
            parse_line(
                &m,
                ContentClass::Unrestricted,
                &ctx,
                None,
                None,
                br#"{"type":"annotation","value":"x","confidence":2}"#
            )
            .is_err()
        );
        let ok = parse_line(
            &m,
            ContentClass::Unrestricted,
            &ctx,
            None,
            None,
            br#"{"type":"annotation","value":"adsb","kind":"ground-truth"}"#,
        )
        .unwrap();
        assert!(
            matches!(ok.output, PluginOutput::Annotation(ref a) if a.kind == AnnotationKind::GroundTruth)
        );
    }
}
