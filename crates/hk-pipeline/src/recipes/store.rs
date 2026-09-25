//! Recipe store (ADR-0011 §2.4, T-088): built-in recipes from the repository's `recipes/`
//! directory (read-only) plus user versions in the data directory, one immutable JSON file per
//! saved version: `<data dir>/recipes/<id>/<version>.json`.
//!
//! **Why files, not SQLite.** A recipe is a JSON document that is also the `POST /api/recipes`
//! body, the hot-edit body and the file in `recipes/`; storing it as that same file keeps one
//! format end to end (ADR-0011 §2.1), makes a saved version trivially diffable and exportable
//! (offline-first sharing), and needs no schema migration. Versions are immutable: a save writes
//! `latest + 1` with `create_new`, so an existing file is never overwritten.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use hk_recipe::{RECIPE_SCHEMA, RECIPE_SCHEMA_VERSIONS, Recipe, is_id};
use serde_json::{Value, json};

/// Suffix of a built-in recipe file.
pub const BUILTIN_SUFFIX: &str = ".recipe.json";

/// A store failure (HTTP-style status, stable code, message without values).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError {
    /// 400 invalid id, 404 unknown recipe or version, 409 built-in only, 500 I/O.
    pub status: u16,
    /// Stable code.
    pub code: &'static str,
    /// Message.
    pub message: String,
}

impl StoreError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn io(what: &str, e: &std::io::Error) -> Self {
        Self::new(500, "failed", format!("recipe store: {what}: {}", e.kind()))
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.status, self.code, self.message)
    }
}

impl std::error::Error for StoreError {}

/// Built-in and user recipes.
#[derive(Debug)]
pub struct RecipeStore {
    builtin: Option<PathBuf>,
    user: PathBuf,
    lock: Mutex<()>,
}

/// One recipe id across its versions.
#[derive(Clone, Debug, PartialEq)]
pub struct RecipeSummary {
    /// Recipe id.
    pub id: String,
    /// Name of the latest version.
    pub name: String,
    /// Latest version.
    pub version: u32,
    /// Every version, ascending.
    pub versions: Vec<u32>,
    /// The built-in version, if the repository ships one.
    pub builtin_version: Option<u32>,
    /// The latest version's document.
    pub latest: Recipe,
}

impl RecipeSummary {
    /// As the API serves it.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "version": self.version,
            "versions": self.versions,
            "builtin": self.builtin_version.is_some(),
            "builtin_version": self.builtin_version,
            "description": self.latest.description,
            "match": self.latest.match_hints,
            "input": {"port": self.latest.input.port},
        })
    }
}

impl RecipeStore {
    /// A store over `builtin` (the repository's `recipes/`, optional) and user versions under
    /// `user_dir` (created on first save).
    pub fn new(builtin: Option<PathBuf>, user_dir: PathBuf) -> Self {
        Self {
            builtin,
            user: user_dir,
            lock: Mutex::new(()),
        }
    }

    /// The user directory.
    pub fn user_dir(&self) -> &Path {
        &self.user
    }

    fn builtins(&self) -> BTreeMap<String, Recipe> {
        let mut out = BTreeMap::new();
        let Some(dir) = &self.builtin else {
            return out;
        };
        let Ok(entries) = fs::read_dir(dir) else {
            return out;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.ends_with(BUILTIN_SUFFIX) {
                continue;
            }
            // A built-in that no longer parses is skipped (its validation test fails instead).
            if let Some(r) = read_recipe(&e.path()) {
                out.insert(r.id.clone(), r);
            }
        }
        out
    }

    fn user_versions(&self, id: &str) -> Vec<u32> {
        let mut v: Vec<u32> = fs::read_dir(self.user.join(id))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()?
                    .strip_suffix(".json")?
                    .parse::<u32>()
                    .ok()
            })
            .filter(|v| *v > 0)
            .collect();
        v.sort_unstable();
        v
    }

    fn user_ids(&self) -> Vec<String> {
        fs::read_dir(&self.user)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|id| is_id(id))
            .collect()
    }

    fn user_path(&self, id: &str, version: u32) -> PathBuf {
        self.user.join(id).join(format!("{version}.json"))
    }

    /// Every recipe id with its versions (built-in and user merged), by id.
    pub fn list(&self) -> Vec<RecipeSummary> {
        let builtins = self.builtins();
        let mut ids: Vec<String> = builtins.keys().cloned().collect();
        ids.extend(self.user_ids());
        ids.sort();
        ids.dedup();
        ids.into_iter()
            .filter_map(|id| self.summary(&id, builtins.get(&id)))
            .collect()
    }

    fn summary(&self, id: &str, builtin: Option<&Recipe>) -> Option<RecipeSummary> {
        let mut versions = self.user_versions(id);
        let builtin_version = builtin.map(|b| b.version);
        if let Some(v) = builtin_version {
            versions.push(v);
        }
        versions.sort_unstable();
        versions.dedup();
        let latest_v = *versions.last()?;
        let latest = self.get_inner(id, Some(latest_v), builtin).ok()?;
        Some(RecipeSummary {
            id: id.to_owned(),
            name: latest.name.clone(),
            version: latest_v,
            versions,
            builtin_version,
            latest,
        })
    }

    /// Version `version` of `id`, or the latest.
    pub fn get(&self, id: &str, version: Option<u32>) -> Result<Recipe, StoreError> {
        check_id(id)?;
        let builtins = self.builtins();
        self.get_inner(id, version, builtins.get(id))
    }

    fn get_inner(
        &self,
        id: &str,
        version: Option<u32>,
        builtin: Option<&Recipe>,
    ) -> Result<Recipe, StoreError> {
        let user = self.user_versions(id);
        let want = match version {
            Some(v) => v,
            None => user
                .last()
                .copied()
                .into_iter()
                .chain(builtin.map(|b| b.version))
                .max()
                .ok_or_else(|| StoreError::new(404, "not_found", "no such recipe"))?,
        };
        if user.contains(&want) {
            return read_recipe(&self.user_path(id, want)).ok_or_else(|| {
                StoreError::new(500, "failed", "a stored recipe version no longer parses")
            });
        }
        match builtin {
            Some(b) if b.version == want => Ok(b.clone()),
            _ if user.is_empty() && builtin.is_none() => {
                Err(StoreError::new(404, "not_found", "no such recipe"))
            }
            _ => Err(StoreError::new(404, "not_found", "no such recipe version")),
        }
    }

    /// Saves `recipe` as the next version of its id (`latest + 1`, built-in included) and
    /// returns the stored document. The caller validates it against the block catalogue first.
    pub fn save(&self, mut recipe: Recipe) -> Result<Recipe, StoreError> {
        check_id(&recipe.id)?;
        if recipe.schema != RECIPE_SCHEMA
            || !RECIPE_SCHEMA_VERSIONS.contains(&recipe.schema_version)
        {
            return Err(StoreError::new(
                400,
                "invalid",
                "unsupported recipe schema or schema_version",
            ));
        }
        let _g = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let builtin = self.builtins().remove(&recipe.id).map(|b| b.version);
        let latest = self
            .user_versions(&recipe.id)
            .last()
            .copied()
            .into_iter()
            .chain(builtin)
            .max()
            .unwrap_or(0);
        recipe.version = latest + 1;
        let dir = self.user.join(&recipe.id);
        fs::create_dir_all(&dir).map_err(|e| StoreError::io("creating the directory", &e))?;
        let bytes = serde_json::to_vec_pretty(&recipe)
            .map_err(|_| StoreError::new(500, "failed", "serialising the recipe"))?;
        let path = self.user_path(&recipe.id, recipe.version);
        // Immutable versions: never overwrite an existing file.
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| StoreError::io("creating the version file", &e))?;
        f.write_all(&bytes)
            .and_then(|()| f.write_all(b"\n"))
            .and_then(|()| f.sync_all())
            .map_err(|e| {
                let _ = fs::remove_file(&path);
                StoreError::io("writing the version file", &e)
            })?;
        Ok(recipe)
    }

    /// Deletes every user version of `id`; returns the deleted versions. A built-in stays (409
    /// when it is the only thing there).
    pub fn delete(&self, id: &str) -> Result<Vec<u32>, StoreError> {
        check_id(id)?;
        let _g = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let versions = self.user_versions(id);
        if versions.is_empty() {
            return Err(if self.builtins().contains_key(id) {
                StoreError::new(409, "conflict", "built-in recipes cannot be deleted")
            } else {
                StoreError::new(404, "not_found", "no such recipe")
            });
        }
        fs::remove_dir_all(self.user.join(id))
            .map_err(|e| StoreError::io("removing the versions", &e))?;
        Ok(versions)
    }
}

fn check_id(id: &str) -> Result<(), StoreError> {
    if is_id(id) {
        Ok(())
    } else {
        Err(StoreError::new(
            400,
            "invalid",
            "recipe ids are [a-z0-9_-]{1,64} starting with a letter or digit",
        ))
    }
}

fn read_recipe(path: &Path) -> Option<Recipe> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, version: u32, name: &str) -> Recipe {
        serde_json::from_value(json!({
            "schema": "hackriff.recipe", "schema_version": 2, "id": id, "version": version,
            "name": name, "input": {"port": "bits"},
            "nodes": [{"id": "copy", "block": "identity"}],
            "outputs": [{"id": "bits", "kind": "stage", "from": "copy"}],
            "output_policy": {"content_class": "unrestricted"}
        }))
        .unwrap()
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "hk-recipe-store-{tag}-{}-{}",
            std::process::id(),
            hk_model::Timestamp::now().as_unix_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// T-870: a saved revision reads back **bit-identical** — an applied refinement writes an
    /// arbitrary f64 (`input.bandwidth_hz`) and the stored revision must carry exactly it. This
    /// value is one serde_json's default parser reads back one ulp off (125073.8830717856).
    #[test]
    fn a_saved_revision_reads_back_every_float_bit_identical() {
        let root = tmp("floats");
        let store = RecipeStore::new(None, root.join("user"));
        let bw = 125_073.883_071_785_59_f64;
        let mut r = doc("floaty", 1, "Floaty");
        r.input.bandwidth_hz = Some(bw);
        let saved = store.save(r).unwrap();
        let back = store.get("floaty", Some(saved.version)).unwrap();
        assert_eq!(
            back.input.bandwidth_hz.map(f64::to_bits),
            Some(bw.to_bits()),
            "{:?}",
            back.input.bandwidth_hz
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn saves_are_immutable_increasing_versions_merged_with_builtins() {
        let root = tmp("versions");
        let builtin = root.join("builtin");
        fs::create_dir_all(&builtin).unwrap();
        fs::write(
            builtin.join("rds.recipe.json"),
            serde_json::to_vec(&doc("rds", 1, "RDS")).unwrap(),
        )
        .unwrap();
        let store = RecipeStore::new(Some(builtin), root.join("user"));

        assert_eq!(store.get("rds", None).unwrap().version, 1);
        let saved = store.save(doc("rds", 7, "RDS tuned")).unwrap();
        assert_eq!(saved.version, 2, "latest + 1 over the built-in");
        assert_eq!(store.save(doc("mine", 1, "Mine")).unwrap().version, 1);
        assert_eq!(store.save(doc("mine", 1, "Mine v2")).unwrap().version, 2);

        let list = store.list();
        let ids: Vec<_> = list.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["mine", "rds"]);
        assert_eq!(list[1].versions, [1, 2]);
        assert_eq!(list[1].builtin_version, Some(1));
        assert_eq!(list[1].name, "RDS tuned");
        assert_eq!(store.get("rds", Some(1)).unwrap().name, "RDS");
        assert_eq!(store.get("mine", Some(1)).unwrap().name, "Mine");
        assert_eq!(store.get("mine", Some(9)).unwrap_err().status, 404);
        assert_eq!(store.get("nope", None).unwrap_err().status, 404);
        assert_eq!(store.get("../etc", None).unwrap_err().status, 400);

        assert_eq!(store.delete("rds").unwrap(), [2]);
        assert_eq!(
            store.get("rds", None).unwrap().version,
            1,
            "built-in remains"
        );
        assert_eq!(store.delete("rds").unwrap_err().status, 409);
        assert_eq!(store.delete("mine").unwrap(), [1, 2]);
        assert_eq!(store.delete("mine").unwrap_err().status, 404);
        let _ = fs::remove_dir_all(root);
    }
}
