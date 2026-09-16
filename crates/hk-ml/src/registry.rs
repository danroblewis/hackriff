//! The model registry: models are **data** (ADR-0016 §6, ADR-0001's hard requirement).
//!
//! ```text
//! <data dir>/models/<id>/<version>/manifest.json
//!                                 /model.onnx      (or model.json for an `hk-mlp@1` file)
//! ```
//!
//! Loading, swapping or rolling back a model is a file operation plus a mode change, never a
//! rebuild. Two properties make that safe:
//!
//! - **A version is immutable.** [`ModelRegistry::install`] refuses to overwrite an existing
//!   `id@version` with different bytes. Provenance on a stored [`crate::Prediction`] would
//!   otherwise be meaningless: the same `id@version` could name two different models.
//! - **The hash is checked on every read**, not just on install. `sha8` in the
//!   [`crate::ModelRef`] is therefore derived from the bytes that actually ran — a corrupted or
//!   swapped file is a [`MlError::HashMismatch`], not a silently different answer.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{MlError, ModelManifest, ModelRef};

/// The file a model's graph is stored under. An `hk-mlp@1` weights file (the `mlp` module) uses
/// `model.json`; both are tried, in this order.
pub const MODEL_FILES: [&str; 2] = ["model.onnx", "model.json"];

/// The manifest file name.
pub const MANIFEST_FILE: &str = "manifest.json";

/// Hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// A model read out of the registry: its manifest and the verified bytes of its graph.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredModel {
    /// The manifest, structurally validated.
    pub manifest: ModelManifest,
    /// The graph file's bytes, whose sha256 matches the manifest.
    pub bytes: Vec<u8>,
    /// Where they came from.
    pub path: PathBuf,
}

/// An on-disk model registry rooted at one directory.
#[derive(Clone, Debug)]
pub struct ModelRegistry {
    root: PathBuf,
}

impl ModelRegistry {
    /// A registry over `root` (created on demand by [`ModelRegistry::install`]).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where `id@version` lives.
    pub fn dir_of(&self, id: &str, version: &str) -> PathBuf {
        self.root.join(id).join(version)
    }

    /// Every manifest in the registry, by `id@version`. Unreadable or invalid entries are
    /// skipped rather than failing the listing: one bad directory must not hide the rest.
    pub fn list(&self) -> BTreeMap<String, ModelManifest> {
        let mut out = BTreeMap::new();
        let Ok(ids) = fs::read_dir(&self.root) else {
            return out;
        };
        for id in ids.flatten() {
            let Ok(versions) = fs::read_dir(id.path()) else {
                continue;
            };
            for v in versions.flatten() {
                let Ok(manifest) = read_manifest(&v.path().join(MANIFEST_FILE)) else {
                    continue;
                };
                out.insert(
                    format!("{}@{}", manifest.model.id, manifest.model.version),
                    manifest,
                );
            }
        }
        out
    }

    /// Reads `id@version`, verifying the graph file against the manifest's sha256.
    pub fn read(&self, id: &str, version: &str) -> Result<StoredModel, MlError> {
        let dir = self.dir_of(id, version);
        let manifest = read_manifest(&dir.join(MANIFEST_FILE))?;
        if manifest.model.id != id || manifest.model.version != version {
            return Err(MlError::Invalid(format!(
                "{}/{} holds a manifest for {}",
                id, version, manifest.model
            )));
        }
        let (path, bytes) = MODEL_FILES
            .iter()
            .map(|f| dir.join(f))
            .find_map(|p| fs::read(&p).ok().map(|b| (p, b)))
            .ok_or_else(|| MlError::Invalid(format!("no model file in {}", dir.display())))?;
        verify(&manifest, &bytes)?;
        Ok(StoredModel {
            manifest,
            bytes,
            path,
        })
    }

    /// Reads the model a [`ModelRef`] names, and checks that the `sha8` the caller asked for is
    /// the one on disk: a reference is a claim about *which bytes*, so it is worth checking.
    pub fn read_ref(&self, r: &ModelRef) -> Result<StoredModel, MlError> {
        let stored = self.read(&r.id, &r.version)?;
        if stored.manifest.model.sha8 != r.sha8 {
            return Err(MlError::HashMismatch(format!(
                "{r} is not the stored {}",
                stored.manifest.model
            )));
        }
        Ok(stored)
    }

    /// Writes a model into the registry.
    ///
    /// Refuses to change an existing `id@version`: a version is immutable once saved, so a
    /// [`crate::Prediction`]'s `id@version#sha8` always names one set of bytes. Re-installing
    /// the *same* bytes is a no-op and succeeds, so an idempotent seeding step is safe.
    pub fn install(&self, manifest: &ModelManifest, bytes: &[u8]) -> Result<PathBuf, MlError> {
        verify(manifest, bytes)?;
        let dir = self.dir_of(&manifest.model.id, &manifest.model.version);
        let file = dir.join(
            if manifest.precision == crate::Precision::Fp32 && bytes.starts_with(b"{") {
                MODEL_FILES[1]
            } else {
                MODEL_FILES[0]
            },
        );
        if let Ok(existing) = self.read(&manifest.model.id, &manifest.model.version) {
            if existing.bytes == bytes && existing.manifest == *manifest {
                return Ok(existing.path);
            }
            return Err(MlError::Invalid(format!(
                "{} is already installed and a version is immutable (ADR-0016 §6); \
                 publish a new version instead",
                manifest.model
            )));
        }
        fs::create_dir_all(&dir).map_err(|e| io(&dir, &e))?;
        let manifest_json = serde_json::to_vec_pretty(manifest)
            .map_err(|e| MlError::Invalid(format!("manifest does not serialise: {e}")))?;
        fs::write(&file, bytes).map_err(|e| io(&file, &e))?;
        let mpath = dir.join(MANIFEST_FILE);
        fs::write(&mpath, manifest_json).map_err(|e| io(&mpath, &e))?;
        Ok(file)
    }
}

/// Reads and validates one manifest file.
pub fn read_manifest(path: &Path) -> Result<ModelManifest, MlError> {
    let bytes = fs::read(path).map_err(|e| io(path, &e))?;
    let manifest: ModelManifest = serde_json::from_slice(&bytes)
        .map_err(|e| MlError::Invalid(format!("{}: {e}", path.display())))?;
    manifest.validate()?;
    Ok(manifest)
}

/// Checks `bytes` against `manifest` (structure first, then the hash).
pub fn verify(manifest: &ModelManifest, bytes: &[u8]) -> Result<(), MlError> {
    manifest.validate()?;
    let digest = sha256_hex(bytes);
    if !digest.eq_ignore_ascii_case(&manifest.sha256) {
        return Err(MlError::HashMismatch(format!(
            "{}: file sha256 {digest} is not the manifest's {}",
            manifest.model, manifest.sha256
        )));
    }
    Ok(())
}

/// `MlError` has no IO variant — adding one would change a contract T-218 fixed for a case that
/// is always "this registry entry cannot be used". The path is kept in the message.
fn io(path: &Path, e: &std::io::Error) -> MlError {
    MlError::Invalid(format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelTask, OpenSetMethod, OpenSetSpec, Precision};

    fn manifest(bytes: &[u8], version: &str) -> ModelManifest {
        let sha = sha256_hex(bytes);
        ModelManifest {
            schema: crate::ML_SCHEMA,
            model: ModelRef {
                id: "amc-fsk".into(),
                version: version.into(),
                sha8: sha[..8].to_owned(),
            },
            sha256: sha,
            task: ModelTask::FamilyClass,
            consumer: "hk-classify/dl".into(),
            taxonomy: None,
            family: Some("fsk".into()),
            labels: vec!["2fsk".into(), "gfsk".into()],
            open_set: OpenSetSpec {
                method: OpenSetMethod::Energy,
                temperature: 1.0,
                threshold: -4.0,
                calibrated_on: "dev@amc-grid-1".into(),
            },
            precision: Precision::Fp32,
            metrics_ref: None,
            enable_evidence: None,
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hk-ml-registry-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_model_round_trips_and_its_bytes_are_verified_on_every_read() {
        let dir = tmp("roundtrip");
        let reg = ModelRegistry::new(&dir);
        let bytes = b"\x08\x09graph-bytes".to_vec();
        let m = manifest(&bytes, "1.0.0");
        reg.install(&m, &bytes).unwrap();

        let stored = reg.read("amc-fsk", "1.0.0").unwrap();
        assert_eq!(stored.bytes, bytes);
        assert_eq!(stored.manifest, m);
        assert_eq!(reg.read_ref(&m.model).unwrap().bytes, bytes);
        assert_eq!(reg.list().keys().collect::<Vec<_>>(), vec!["amc-fsk@1.0.0"]);

        // The file is corrupted after installation: the read refuses it rather than running
        // bytes whose provenance would be a lie.
        fs::write(&stored.path, b"different bytes entirely").unwrap();
        match reg.read("amc-fsk", "1.0.0") {
            Err(MlError::HashMismatch(_)) => {}
            other => panic!("a corrupted model must not load: {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_is_immutable_but_reinstalling_the_same_bytes_is_a_no_op() {
        let dir = tmp("immutable");
        let reg = ModelRegistry::new(&dir);
        let bytes = b"first".to_vec();
        let m = manifest(&bytes, "0.1.0");
        reg.install(&m, &bytes).unwrap();
        reg.install(&m, &bytes)
            .expect("re-installing identical bytes is idempotent");

        let other = b"second".to_vec();
        let mut m2 = manifest(&other, "0.1.0");
        m2.model.version = "0.1.0".into();
        assert!(
            reg.install(&m2, &other).is_err(),
            "0.1.0 must not become different bytes"
        );
        // A new version is how a model changes.
        let m3 = manifest(&other, "0.2.0");
        reg.install(&m3, &other).unwrap();
        assert_eq!(reg.list().len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manifest_that_does_not_match_its_file_is_refused_at_install() {
        let dir = tmp("mismatch");
        let reg = ModelRegistry::new(&dir);
        let m = manifest(b"the real bytes", "1.0.0");
        match reg.install(&m, b"not those bytes") {
            Err(MlError::HashMismatch(_)) => {}
            other => panic!("install must verify the hash: {other:?}"),
        }
        assert!(reg.read("amc-fsk", "1.0.0").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reference_to_the_wrong_bytes_is_refused() {
        let dir = tmp("ref");
        let reg = ModelRegistry::new(&dir);
        let bytes = b"graph".to_vec();
        let m = manifest(&bytes, "1.0.0");
        reg.install(&m, &bytes).unwrap();
        let mut wrong = m.model.clone();
        wrong.sha8 = "deadbeef".into();
        assert!(matches!(
            reg.read_ref(&wrong),
            Err(MlError::HashMismatch(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}
