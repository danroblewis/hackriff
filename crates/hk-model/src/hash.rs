//! Content hashing: canonical JSON and SHA-256 content hashes.
//!
//! [`canonical_json`] gives one text per value:
//! - object keys are sorted (serde_json maps are `BTreeMap`s; the workspace does not enable
//!   serde_json's `preserve_order`, and a test here fails if something does);
//! - `-0.0` is normalised to `0.0`.
//!
//! [`ContentHash`] is the SHA-256 of that text. The repository deduplicates Provenance rows on it,
//! and Explanation evidence pins the payload hash of each ExternalEvent it used, so a later
//! refresh of the cached payload is detectable instead of silent.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Number, Value};
use sha2::{Digest, Sha256};

/// A 32-byte SHA-256 content hash. Serialised as 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// Wraps raw hash bytes (e.g. read back from storage).
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// SHA-256 of `text` as given (callers hash canonical JSON).
    pub fn of_text(text: &str) -> Self {
        let digest = Sha256::digest(text.as_bytes());
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_slice());
        Self(out)
    }

    /// SHA-256 of `value`'s [`canonical_json`].
    pub fn of<T: Serialize + ?Sized>(value: &T) -> Result<Self, serde_json::Error> {
        Ok(Self::of_text(&canonical_json(value)?))
    }

    /// Lowercase hex form.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Parses 64 hex digits (either case).
    pub fn from_hex(s: &str) -> Option<Self> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, pair) in bytes.chunks_exact(2).enumerate() {
            out[i] = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
        }
        Some(Self(out))
    }
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({})", self.to_hex())
    }
}

impl Serialize for ContentHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        ContentHash::from_hex(&s)
            .ok_or_else(|| serde::de::Error::custom("expected 64 hex digits for a content hash"))
    }
}

/// The canonical JSON text of `value`: sorted object keys, `-0.0` written as `0.0`, no
/// whitespace. Non-finite floats become `null` (serde_json's rule), so callers that must
/// round-trip check the result.
pub fn canonical_json<T: Serialize + ?Sized>(value: &T) -> Result<String, serde_json::Error> {
    let mut v = serde_json::to_value(value)?;
    normalise(&mut v);
    serde_json::to_string(&v)
}

fn normalise(v: &mut Value) {
    match v {
        Value::Number(n) => {
            // `-0.0 == 0.0`, so this rewrites both zeros to the positive one.
            if n.is_f64() && n.as_f64() == Some(0.0) {
                *n = Number::from_f64(0.0).expect("0.0 is finite");
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalise),
        Value::Object(map) => map.values_mut().for_each(normalise),
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn sha256_known_vector_and_hex_round_trip() {
        let empty = ContentHash::of_text("");
        assert_eq!(
            empty.to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(ContentHash::from_hex(&empty.to_hex()), Some(empty));
        assert_eq!(
            ContentHash::from_hex(&empty.to_hex().to_uppercase()),
            Some(empty)
        );
        assert_eq!(ContentHash::from_hex("abc"), None);
        let json = serde_json::to_string(&empty).unwrap();
        assert_eq!(serde_json::from_str::<ContentHash>(&json).unwrap(), empty);
    }

    #[test]
    fn canonical_json_sorts_keys_and_normalises_negative_zero() {
        let a = json!({"b": 1, "a": {"y": -0.0, "x": [0.0, -0.0]}});
        let b = json!({"a": {"x": [-0.0, 0.0], "y": 0.0}, "b": 1});
        assert_eq!(canonical_json(&a).unwrap(), canonical_json(&b).unwrap());
        assert_eq!(
            canonical_json(&a).unwrap(),
            r#"{"a":{"x":[0.0,0.0],"y":0.0},"b":1}"#
        );
        assert_eq!(ContentHash::of(&a).unwrap(), ContentHash::of(&b).unwrap());
        assert_ne!(
            ContentHash::of(&json!({"v": 1.0})).unwrap(),
            ContentHash::of(&json!({"v": 2.0})).unwrap()
        );
    }
}
