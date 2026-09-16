//! C18 rtl_433 flex-spec import (T-214, ADR-0016 §5, C18 card §Methods "Seeding"): mapping a
//! **documented subset** of the rtl_433 general-purpose "flex" decoder spec (the `-X` option,
//! `rtl_433 -X help`) into an untrusted, immutable [`Signature`] version.
//!
//! # The standing rule this module exists under
//!
//! The known-signal database — band plans, licences, catalogues, and now rtl_433 specs —
//! **suggests and never decides** (CLAUDE.md). This importer only ever *builds a [`Signature`]
//! value*: it touches no emitter, no inventory row and no detection. It cannot pre-populate
//! anything, because it has no repository access to anything but the signature catalogue itself
//! ([`import_flex_spec`] takes none at all; [`import_flex_spec_into`] only calls
//! [`Repository::insert_signature`]). What that signature can go on to *do* is bounded further
//! upstream: [`super::matcher::MATCH_MIN_DISCRIMINATING`] holds every signature — imported ones
//! included — to at least three agreeing required fields before a match may be `full`, whatever
//! `min_discriminating` an entry declares for itself. An import with fewer mapped fields than
//! that (most flex specs, see below) can only ever produce a `partial` suggestion, never an
//! identity.
//!
//! # The documented subset
//!
//! Per `rtl_433 -X help`, a flex spec is a comma-separated list of `key=value` pairs (bare flags
//! carry no `=value`). This importer reads exactly these keys and maps them as follows; every
//! other key the spec uses — recognised by rtl_433 or not — is **reported**, never dropped
//! silently (see [`UnsupportedField`]):
//!
//! | Key | Maps to | Notes |
//! |---|---|---|
//! | `name=`/`n=` | [`Signature::name`], and the id (slugified) | required |
//! | `modulation=`/`m=` | [`Signature::family`] | `OOK_*` → `ook-ask`, `FSK_*` → `fsk` (covers all twelve documented modulation values); any other value is reported unsupported. Required. |
//! | `short=`/`s=` | [`field::SYMBOL_RATE_HZ`] as `1e6 / short_us` | **only** for `OOK_PCM`/`FSK_PCM`, whose bit period is exactly the one pulse width the spec gives. Every other modulation encodes bit value or spacing *in* the pulse width (PWM, PPM, Manchester, …), so there its `short=` is reported unsupported rather than guessed at. |
//! | `preamble=<bits>` | [`field::PREAMBLE`] | only when the value is a plain `0`/`1`/`x` bit string; rtl_433's `{N}0xHEX` row-spec syntax is not parsed by this importer and is reported unsupported if seen. |
//!
//! Deliberately **not** mapped, always reported when present: `long=`/`l=` (redundant with
//! `short=` for PCM, undefined for anything else), `sync=`/`y=` (a pulse *width* in µs — it has no
//! matching field; [`field::SYNC_WORD`] expects a bit pattern), `reset=`/`r=`, `gap=`/`g=`,
//! `tolerance=`/`t=`, `priority=` (decoder framing/matching parameters, not measured signal
//! parameters), `bits=` (an "at least N bits" filter, not an exact
//! [`field::PACKET_LENGTH_BITS`]), `rows=`, `repeats=`, `invert`, `reflect`, `decode_uart=`,
//! `decode_dm`, `decode_mc` (decoding mechanics), `match=<bits>` (a decoded-payload filter, not a
//! framing sync word), `unique`, `countonly` (output filters), and any key this importer does not
//! recognise at all.
//!
//! No flex key carries a centre frequency or band, so an imported signature's `bands_hz` is
//! always empty — consistent with bands being rank-only evidence, never a gate (ADR-0016 §5).
//!
//! # Provenance and immutable versioning
//!
//! Every signature this module builds carries `provenance: SignatureProvenance::Rtl433Import` —
//! [`SignatureProvenance::trusted`] is `false` for it, same as every other importer output in this
//! system — and a `notes` field recording the caller-supplied `source` label plus the exact
//! (trimmed) spec text it was built from, so "came from an rtl_433 spec, and which one" is on the
//! record. [`import_flex_spec`] always builds version 1 with no `supersedes`; the caller assigns
//! the real version. [`import_flex_spec_into`] does that against a [`Repository`]: it never
//! mutates a stored version, it writes `max(existing versions) + 1` with `supersedes` set to the
//! version it follows — whether the spec text changed or an identical re-import — because
//! [`Signature`] versions are immutable ([`Repository::insert_signature`]'s primary key refuses a
//! duplicate `(id, version)` as a second line of defence).

use std::collections::BTreeMap;

use hk_model::classify::TaxonomyRef;
use hk_model::signature::{
    FieldExpect, FieldSpec, SIGNATURE_SCHEMA, Signature, SignatureKind, SignatureProvenance, field,
    is_signature_id,
};
use hk_model::time::Timestamp;
use hk_model::{RepoError, Repository};

/// One key of an rtl_433 flex spec this importer did not fold into the [`Signature`] it built,
/// and why. Reported, never silently dropped (CLAUDE.md; the C18 card's "imported signatures are
/// untrusted input" pitfall).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsupportedField {
    /// The key exactly as written in the spec (e.g. `"s"`, `"reset"`, `"foo"`).
    pub key: String,
    /// Its value, or empty for a bare flag.
    pub value: String,
    /// Why this importer does not map it.
    pub reason: &'static str,
}

/// A flex spec's [`Signature`] plus every field the importer chose not to map.
#[derive(Clone, Debug, PartialEq)]
pub struct FlexImport {
    /// The built, validated signature (always version 1; a caller assigning a real version does
    /// so afterward, see [`import_flex_spec_into`]).
    pub signature: Signature,
    /// Fields present in the spec that were not folded into `signature`.
    pub unsupported: Vec<UnsupportedField>,
}

/// Why a flex spec could not be turned into a [`Signature`] at all.
#[derive(Debug, thiserror::Error)]
pub enum FlexImportError {
    /// `name=`/`n=` or `modulation=`/`m=` was absent.
    #[error("rtl_433 flex spec is missing the required field {0}")]
    MissingRequiredField(&'static str),
    /// The name does not slugify to a legal signature id ([`is_signature_id`]).
    #[error("rtl_433 flex spec name {0:?} does not produce a usable signature id")]
    InvalidName(String),
    /// Every field in the spec fell into `unsupported`: there is nothing to build a catalogue
    /// entry from. This is the honest outcome, not a fabricated fieldless signature — one would
    /// fail [`Signature::validate`] anyway ("a signature with no fields matches everything").
    #[error(
        "rtl_433 flex spec {name:?} (modulation {modulation:?}) maps to no Signature field this \
         importer supports; unsupported: {unsupported:?}"
    )]
    NoMappableFields {
        /// The spec's `name=`.
        name: String,
        /// The spec's `modulation=`.
        modulation: String,
        /// Every field the importer could not map.
        unsupported: Vec<UnsupportedField>,
    },
    /// Defensive: the assembled signature broke its own contract despite the checks above.
    #[error("rtl_433 import built an invalid signature: {0}")]
    Invalid(String),
}

/// [`import_flex_spec_into`]'s own failure modes, layered on [`FlexImportError`].
#[derive(Debug, thiserror::Error)]
pub enum ImportIntoRepoError {
    /// The spec itself did not import.
    #[error(transparent)]
    Import(#[from] FlexImportError),
    /// The repository call failed.
    #[error(transparent)]
    Repo(#[from] RepoError),
}

/// Maps one rtl_433 flex spec (the inner text of `-X "..."`, with or without the surrounding
/// quotes) to a [`Signature`] version 1, with provenance [`SignatureProvenance::Rtl433Import`].
///
/// `source` names the spec for provenance (a filename, a URL, a device name from rtl_433's own
/// catalogue — whatever the caller has); `author` is the importer's own identity (an importer id
/// or a user token, per [`Signature::author`]'s contract). `t` becomes `created_at`.
///
/// Returns every field the spec used that this importer does not map, alongside the signature, so
/// nothing is silently dropped. Fails when the spec has no `name=`/`modulation=`, when the name
/// will not slugify to a legal id, or when nothing in the spec mapped to a canonical field at all.
pub fn import_flex_spec(
    spec_text: &str,
    source: &str,
    author: &str,
    t: Timestamp,
) -> Result<FlexImport, FlexImportError> {
    let mut name: Option<String> = None;
    let mut modulation: Option<String> = None;
    let mut short_us: Option<f64> = None;
    let mut preamble: Option<String> = None;
    let mut unsupported = Vec::new();

    for raw in parse_pairs(spec_text) {
        let key_lower = raw.key.to_ascii_lowercase();
        match key_lower.as_str() {
            "name" | "n" => match raw.value {
                Some(v) => name = Some(v.to_owned()),
                None => unsupported.push(bare_flag(raw.key, "name= needs a value")),
            },
            "modulation" | "m" => match raw.value {
                Some(v) => modulation = Some(v.to_owned()),
                None => unsupported.push(bare_flag(raw.key, "modulation= needs a value")),
            },
            "short" | "s" => match raw.value.and_then(|v| v.parse::<f64>().ok()) {
                Some(v) if v.is_finite() && v > 0.0 => short_us = Some(v),
                _ => unsupported.push(UnsupportedField {
                    key: raw.key.to_owned(),
                    value: raw.value.unwrap_or_default().to_owned(),
                    reason: "short= must be a positive microsecond pulse width",
                }),
            },
            "preamble" => match raw.value {
                Some(v) if is_bit_string(v) => preamble = Some(v.to_owned()),
                Some(v) => unsupported.push(UnsupportedField {
                    key: "preamble".to_owned(),
                    value: v.to_owned(),
                    reason: "preamble value is not a plain 0/1/x bit string (rtl_433's \
                             {N}0xHEX row-spec syntax is not parsed by this importer)",
                }),
                None => unsupported.push(bare_flag("preamble", "preamble= needs a value")),
            },
            _ => unsupported.push(UnsupportedField {
                key: raw.key.to_owned(),
                value: raw.value.unwrap_or_default().to_owned(),
                reason: known_unsupported_reason(&key_lower),
            }),
        }
    }

    let name = name.ok_or(FlexImportError::MissingRequiredField("name= (or n=)"))?;
    let modulation =
        modulation.ok_or(FlexImportError::MissingRequiredField("modulation= (or m=)"))?;
    let mod_upper = modulation.to_ascii_uppercase();
    let family = if mod_upper.starts_with("OOK_") {
        Some("ook-ask")
    } else if mod_upper.starts_with("FSK_") {
        Some("fsk")
    } else {
        unsupported.push(UnsupportedField {
            key: "modulation".to_owned(),
            value: modulation.clone(),
            reason: "not a documented OOK_*/FSK_* flex modulation type",
        });
        None
    };

    let mut fields = BTreeMap::new();
    if let Some(short) = short_us {
        let is_pcm = matches!(mod_upper.as_str(), "OOK_PCM" | "FSK_PCM");
        if is_pcm {
            fields.insert(
                field::SYMBOL_RATE_HZ.to_owned(),
                FieldSpec::required(FieldExpect::Value {
                    value: 1_000_000.0 / short,
                }),
            );
        } else {
            unsupported.push(UnsupportedField {
                key: "short".to_owned(),
                value: short.to_string(),
                reason: "pulse width only maps to symbol_rate_hz for fixed-width PCM \
                         modulations (OOK_PCM/FSK_PCM); this modulation encodes bit value or \
                         spacing in the pulse width instead of a constant bit period",
            });
        }
    }
    if let Some(bits) = preamble {
        fields.insert(
            field::PREAMBLE.to_owned(),
            FieldSpec::required(FieldExpect::Bits {
                bits,
                max_errors: 0,
            }),
        );
    }

    if fields.is_empty() {
        return Err(FlexImportError::NoMappableFields {
            name,
            modulation,
            unsupported,
        });
    }

    let id = slugify(&name).ok_or_else(|| FlexImportError::InvalidName(name.clone()))?;
    let min_discriminating = fields.len() as u32;
    let signature = Signature {
        schema: SIGNATURE_SCHEMA,
        id,
        version: 1,
        name: name.clone(),
        kind: SignatureKind::DeviceType,
        taxonomy: family.map(|_| TaxonomyRef::current()),
        family: family.map(str::to_owned),
        class: None,
        fields,
        min_discriminating,
        recipe: None,
        provenance: SignatureProvenance::Rtl433Import,
        author: author.to_owned(),
        created_at: t,
        supersedes: None,
        bands_hz: Vec::new(),
        notes: Some(format!(
            "imported from rtl_433 flex spec {source:?}: {}",
            spec_text.trim()
        )),
    };
    signature
        .validate()
        .map_err(|e| FlexImportError::Invalid(e.0))?;
    Ok(FlexImport {
        signature,
        unsupported,
    })
}

/// [`import_flex_spec`], then writes the result as the next immutable version of its id: version 1
/// if the repository has never seen this id, otherwise `max(existing versions) + 1` with
/// `supersedes` set to that max — whether the spec text is identical to what is already stored or
/// has changed. A version is never mutated in place (ADR-0016 §5); re-running the same import
/// always appends.
pub fn import_flex_spec_into(
    repo: &mut Repository,
    spec_text: &str,
    source: &str,
    author: &str,
    t: Timestamp,
) -> Result<FlexImport, ImportIntoRepoError> {
    let mut import = import_flex_spec(spec_text, source, author, t)?;
    let existing = repo.signature_versions(&import.signature.id)?;
    if let Some(max_version) = existing.iter().map(|s| s.version).max() {
        import.signature.version = max_version + 1;
        import.signature.supersedes = Some(max_version);
    }
    repo.insert_signature(&import.signature)?;
    Ok(import)
}

struct RawField<'a> {
    key: &'a str,
    value: Option<&'a str>,
}

/// Splits a flex spec into its comma-separated `key[=value]` tokens. Tolerates the surrounding
/// quotes rtl_433's own `-X "..."` invocation uses, and splits each token on the *first* `=` only,
/// so a value that itself contains `=` (none of the keys this importer maps do) is not mis-split.
fn parse_pairs(spec_text: &str) -> Vec<RawField<'_>> {
    spec_text
        .trim()
        .trim_matches('"')
        .split(',')
        .map(str::trim)
        .filter(|tok| !tok.is_empty())
        .map(|tok| match tok.split_once('=') {
            Some((k, v)) => RawField {
                key: k.trim(),
                value: Some(v.trim()),
            },
            None => RawField {
                key: tok,
                value: None,
            },
        })
        .collect()
}

fn bare_flag(key: &str, reason: &'static str) -> UnsupportedField {
    UnsupportedField {
        key: key.to_owned(),
        value: String::new(),
        reason,
    }
}

fn is_bit_string(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| matches!(c, '0' | '1' | 'x'))
}

/// Why a recognised-but-unmapped flex key is not folded into a [`Signature`] field. Anything not
/// listed here (an unrecognised key) gets the generic catch-all.
fn known_unsupported_reason(key_lower: &str) -> &'static str {
    match key_lower {
        "long" | "l" => {
            "not mapped: only short=/s= is imported (as symbol_rate_hz, and only \
                         for fixed-width PCM modulations); the long width carries no additional \
                         canonical field"
        }
        "sync" | "y" => {
            "a pulse width in microseconds, not a bit pattern; it has no matching \
                         field (sync_word expects bits, not timing)"
        }
        "reset" | "r" => "a decoder framing timeout, not a measured signal parameter",
        "gap" | "g" => "a decoder framing timeout, not a measured signal parameter",
        "tolerance" | "t" => "a decoder matching tolerance, not a measured signal parameter",
        "priority" => "decoder fallback ordering, not a signal parameter",
        "bits" => {
            "rtl_433's \"at least N bits\" filter does not map to an exact \
                   packet_length_bits expectation"
        }
        "rows" | "repeats" => "a decoder repeat-count filter, not a signal parameter",
        "invert" | "reflect" => "a bit-level decoding option, not a measured signal parameter",
        "decode_uart" | "decode_dm" | "decode_mc" => {
            "a sub-decoding option, not a measured signal parameter"
        }
        "match" => "matches decoded payload bits, not a framing sync word",
        "unique" | "countonly" => "a decoder output-filtering option, not a signal parameter",
        _ => "not a recognised rtl_433 flex spec key, or not a value form this importer parses",
    }
}

/// Lower-cases `name` and keeps only ASCII alphanumerics, collapsing every run of anything else
/// into one `-` (never at the id's start, matching [`is_signature_id`]'s rule). `None` when the
/// result is empty or too long to be a legal id.
fn slugify(name: &str) -> Option<String> {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() || out.len() > 64 || !is_signature_id(&out) {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use hk_model::signature::SignatureProvenance;
    use hk_model::{EmissionFeatures, EmitterId, Repository};

    use super::*;
    use crate::signature::matcher::{MATCH_MIN_DISCRIMINATING, match_signatures};

    fn t() -> Timestamp {
        Timestamp::from_unix_nanos(1_789_400_000_000_000_000)
    }

    /// A real-shaped flex spec for a fixed-width PCM sensor, with a preamble — the case where
    /// every mapped key applies (`docs/03 §3.3`-style device: OOK_PCM, ~200 µs bit period).
    const PCM_SPEC: &str = r#"n=acurite-609a,m=OOK_PCM,s=976,l=976,r=8000,preamble=1010,bits=24"#;

    /// The rtl_433 README's own `-X` example: a PWM doorbell, quoted the way `-X "..."` is
    /// actually invoked, with a `match=` payload filter instead of a preamble.
    const PWM_DOORBELL_SPEC: &str =
        r#""n=doorbell,m=OOK_PWM,s=400,l=800,r=7000,g=1000,match={24}0xa9878c,repeats>=3""#;

    #[test]
    fn a_pcm_spec_maps_symbol_rate_and_preamble_and_reports_the_rest() {
        let import = import_flex_spec(PCM_SPEC, "acurite-609a.conf", "rtl433-import", t())
            .expect("a PCM spec with a preamble maps to at least one field");
        let sig = &import.signature;
        assert_eq!(sig.id, "acurite-609a");
        assert_eq!(sig.name, "acurite-609a");
        assert_eq!(sig.kind, SignatureKind::DeviceType);
        assert_eq!(sig.family.as_deref(), Some("ook-ask"));
        assert_eq!(sig.provenance, SignatureProvenance::Rtl433Import);
        assert!(
            !sig.provenance.trusted(),
            "an import is never trusted input"
        );
        assert_eq!(sig.version, 1);
        assert_eq!(sig.supersedes, None);
        assert!(
            sig.notes
                .as_deref()
                .is_some_and(|n| n.contains("acurite-609a.conf") && n.contains("OOK_PCM")),
            "provenance names the source and keeps the raw spec: {:?}",
            sig.notes
        );

        let rate = sig
            .fields
            .get(field::SYMBOL_RATE_HZ)
            .expect("symbol_rate_hz");
        match &rate.expect {
            FieldExpect::Value { value } => assert!((*value - 1_000_000.0 / 976.0).abs() < 1e-6),
            other => panic!("expected a Value expectation, got {other:?}"),
        }
        assert!(sig.fields.contains_key(field::PREAMBLE));
        assert_eq!(sig.min_discriminating, 2);
        sig.validate().expect("the built signature is valid");

        // r= and bits= are named, not silently dropped, and l= is reported even though it
        // duplicates s= for PCM.
        let reported: Vec<&str> = import.unsupported.iter().map(|u| u.key.as_str()).collect();
        assert!(reported.contains(&"r"));
        assert!(reported.contains(&"bits"));
        assert!(reported.contains(&"l"));
        for u in &import.unsupported {
            assert!(
                !u.reason.is_empty(),
                "every unsupported field names a reason"
            );
        }
    }

    /// The PWM doorbell from rtl_433's own README: `short=`/`long=` do not reduce to a symbol
    /// rate for PWM, and `match=` is not a sync word, so nothing in the spec maps — the honest
    /// outcome is a refusal naming exactly what was unsupported, not a fabricated signature.
    #[test]
    fn a_pwm_spec_with_nothing_mappable_is_refused_and_names_why() {
        let err = import_flex_spec(PWM_DOORBELL_SPEC, "doorbell.conf", "rtl433-import", t())
            .expect_err("a PWM spec with no preamble maps to nothing");
        match err {
            FlexImportError::NoMappableFields {
                name,
                modulation,
                unsupported,
            } => {
                assert_eq!(name, "doorbell");
                assert_eq!(modulation, "OOK_PWM");
                let keys: Vec<&str> = unsupported.iter().map(|u| u.key.as_str()).collect();
                assert!(keys.contains(&"short"), "{keys:?}");
                assert!(keys.contains(&"long") || keys.contains(&"l"), "{keys:?}");
                assert!(keys.contains(&"match"), "{keys:?}");
            }
            other => panic!("expected NoMappableFields, got {other:?}"),
        }
    }

    /// A PWM spec that *does* carry a usable preamble still imports, on that field alone, and a
    /// signature this thin cannot reach `full` on its own — the matcher's own floor
    /// (`MATCH_MIN_DISCRIMINATING`), not anything this importer decides.
    #[test]
    fn a_thin_import_can_only_ever_be_a_partial_suggestion() {
        let spec = "n=thin-sensor,m=OOK_PWM,s=400,l=800,preamble=1010";
        let import = import_flex_spec(spec, "thin.conf", "rtl433-import", t()).unwrap();
        assert_eq!(import.signature.fields.len(), 1);
        assert!(
            (import.signature.fields.len() as u32) < MATCH_MIN_DISCRIMINATING,
            "a one-field import cannot meet the full-match floor by construction"
        );

        // And matching it against a feature snapshot that agrees on that single field never
        // produces `full` — never an identity, only ever a ranked suggestion.
        let features = EmissionFeatures::new("features:thin", EmitterId::new(), t());
        let m = match_signatures(&features, &[import.signature], 1, t());
        assert_ne!(m.outcome, hk_model::signature::MatchOutcome::Full);
    }

    #[test]
    fn missing_name_or_modulation_is_refused() {
        assert!(matches!(
            import_flex_spec("m=OOK_PCM,s=100", "x", "a", t()),
            Err(FlexImportError::MissingRequiredField(_))
        ));
        assert!(matches!(
            import_flex_spec("n=x,s=100", "x", "a", t()),
            Err(FlexImportError::MissingRequiredField(_))
        ));
    }

    #[test]
    fn an_unmappable_name_is_refused_rather_than_silently_mangled() {
        let err = import_flex_spec("n=???,m=OOK_PCM,s=100", "x", "a", t())
            .expect_err("punctuation-only name has no slug");
        assert!(matches!(err, FlexImportError::InvalidName(n) if n == "???"));
    }

    /// Re-importing the same spec, or a changed one, never mutates the stored version: each call
    /// appends `max + 1` and points `supersedes` at what it followed.
    #[test]
    fn reimporting_always_appends_a_new_immutable_version() {
        let mut repo = Repository::open_in_memory().unwrap();
        let v1 = import_flex_spec_into(&mut repo, PCM_SPEC, "acurite-609a.conf", "importer", t())
            .unwrap();
        assert_eq!(v1.signature.version, 1);
        assert_eq!(v1.signature.supersedes, None);

        // Same text again.
        let v2 = import_flex_spec_into(&mut repo, PCM_SPEC, "acurite-609a.conf", "importer", t())
            .unwrap();
        assert_eq!(v2.signature.version, 2);
        assert_eq!(v2.signature.supersedes, Some(1));

        // A changed spec (different preamble) still only ever appends.
        let changed = "n=acurite-609a,m=OOK_PCM,s=976,l=976,preamble=1100";
        let v3 = import_flex_spec_into(&mut repo, changed, "acurite-609a.conf", "importer", t())
            .unwrap();
        assert_eq!(v3.signature.version, 3);
        assert_eq!(v3.signature.supersedes, Some(2));

        // Every version is still readable; nothing was overwritten in place.
        let versions = repo.signature_versions("acurite-609a").unwrap();
        assert_eq!(versions.len(), 3);
        assert_eq!(
            versions.iter().map(|s| s.version).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        // The catalogue read returns only the current (highest, non-retired) one.
        let current = repo.current_signature("acurite-609a").unwrap().unwrap();
        assert_eq!(current.version, 3);
    }

    /// An import never has anywhere to write an identity, a detection or an inventory row: the
    /// only repository call it makes is `insert_signature` on the catalogue, structurally the
    /// same guarantee `signature.rs` pins for a match.
    #[test]
    fn an_import_writes_only_the_catalogue() {
        let v = serde_json::to_value(import_flex_spec(PCM_SPEC, "x", "a", t()).unwrap().signature)
            .unwrap();
        for forbidden in [
            "identity",
            "known_status",
            "lifecycle",
            "state",
            "emitter_id",
        ] {
            assert!(v.get(forbidden).is_none(), "{forbidden}");
        }
    }
}
