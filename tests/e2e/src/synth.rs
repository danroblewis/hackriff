//! On-demand synthetic scenarios from the Python generator (`py/hkpy/synth`).
//!
//! [`SynthRequest::generate`] runs
//! `uv run --locked --project py python -m hkpy.synth <scenario> --seed N --out DIR --param k=v…`
//! into `target/synth-cache/<scenario>-<hash>/`. The hash covers the scenario, seed, datatype,
//! parameters and the bytes of the generator sources (`py/hkpy/synth/*.py`, `py/hkpy/sigmf.py`,
//! `py/pyproject.toml`, `py/uv.lock`), so editing the generator invalidates the cache. A cached
//! directory is reused without running Python. Generation writes to a temporary directory and
//! renames it into place, so parallel tests requesting the same scenario are safe.
//!
//! When `uv` is not on `PATH` (or `$HK_UV`), generation fails with [`SynthError::UvMissing`];
//! tests use [`synth_or_skip!`](crate::synth_or_skip) to print a skip message and return.
//! Set `HK_E2E_REQUIRE_SYNTH=1` to make that a failure instead (for CI jobs that install uv).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use hk_model::sigmf::Datatype;
use serde_json::Value;

use crate::fixture::{Fixture, FixtureError};
use crate::paths;

/// Environment variable: when `1`, a missing `uv` fails tests instead of skipping them.
pub const REQUIRE_SYNTH_ENV: &str = "HK_E2E_REQUIRE_SYNTH";

/// Errors generating or opening a synthetic scenario.
#[derive(Debug, thiserror::Error)]
pub enum SynthError {
    /// `uv` is not installed.
    #[error(
        "uv not found on PATH (or $HK_UV); install uv (https://docs.astral.sh/uv/) to generate synthetic IQ scenarios"
    )]
    UvMissing,
    /// The generator exited unsuccessfully.
    #[error("hkpy.synth {scenario} failed ({status}):\n{stderr}")]
    GeneratorFailed {
        /// Scenario name.
        scenario: String,
        /// Exit status.
        status: String,
        /// Captured stderr.
        stderr: String,
    },
    /// Filesystem error.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// `manifest.json` or an extra truth file is malformed.
    #[error("{path}: {message}")]
    BadOutput {
        /// File involved.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
    /// A generated recording failed to load.
    #[error(transparent)]
    Fixture(#[from] FixtureError),
}

impl SynthError {
    /// True when generation is impossible on this machine (as opposed to a generator bug).
    pub fn is_unavailable(&self) -> bool {
        matches!(self, SynthError::UvMissing)
    }
}

/// Whether a missing generator must fail tests (`HK_E2E_REQUIRE_SYNTH=1`).
pub fn require_synth() -> bool {
    std::env::var(REQUIRE_SYNTH_ENV).is_ok_and(|v| v == "1")
}

/// Generates a scenario or, when `uv` is unavailable, prints a skip message and returns from the
/// enclosing test.
#[macro_export]
macro_rules! synth_or_skip {
    ($request:expr) => {
        match $crate::synth::SynthRequest::generate(&$request) {
            Ok(output) => output,
            Err(err) if err.is_unavailable() && !$crate::synth::require_synth() => {
                eprintln!("SKIP {}: {err}", module_path!());
                return;
            }
            Err(err) => panic!("synthetic scenario generation failed: {err}"),
        }
    };
}

/// A synthetic scenario to generate.
#[derive(Clone, Debug, PartialEq)]
pub struct SynthRequest {
    /// Scenario name, e.g. `fsk_burst_train`.
    pub scenario: String,
    /// Generator seed.
    pub seed: u64,
    /// Parameter overrides, passed as `--param k=v` (lists comma-separated, `none` for null).
    pub params: BTreeMap<String, String>,
    /// Output datatype: `ci8` (default) or `cf32_le`.
    pub datatype: Datatype,
}

impl SynthRequest {
    /// A request with seed 0, ci8 output and default parameters.
    pub fn new(scenario: impl Into<String>) -> Self {
        Self {
            scenario: scenario.into(),
            seed: 0,
            params: BTreeMap::new(),
            datatype: Datatype::Ci8,
        }
    }

    /// Sets the seed.
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Overrides one generator parameter.
    pub fn param(mut self, key: impl Into<String>, value: impl ToString) -> Self {
        self.params.insert(key.into(), value.to_string());
        self
    }

    /// Selects the output datatype (`Ci8` or `Cf32Le`).
    pub fn datatype(mut self, datatype: Datatype) -> Self {
        self.datatype = datatype;
        self
    }

    /// Content hash of the request plus the generator sources (32 hex digits).
    pub fn cache_key(&self) -> Result<String, SynthError> {
        let mut h = Hasher::new();
        h.field(b"hk-e2e synth cache v1");
        h.field(self.scenario.as_bytes());
        h.field(&self.seed.to_le_bytes());
        h.field(self.datatype.as_str().as_bytes());
        for (k, v) in &self.params {
            h.field(k.as_bytes());
            h.field(v.as_bytes());
        }
        for path in generator_sources()? {
            let bytes = fs::read(&path).map_err(|source| SynthError::Io {
                path: path.clone(),
                source,
            })?;
            h.field(path.file_name().unwrap_or_default().as_encoded_bytes());
            h.field(&bytes);
        }
        Ok(h.hex())
    }

    /// The cache directory this request generates into.
    pub fn cache_dir(&self) -> Result<PathBuf, SynthError> {
        Ok(paths::synth_cache_dir().join(format!("{}-{}", self.scenario, self.cache_key()?)))
    }

    /// Generates the scenario (or reuses the cached output) and opens it.
    pub fn generate(&self) -> Result<SynthOutput, SynthError> {
        let dir = self.cache_dir()?;
        if dir.join("manifest.json").is_file() {
            return SynthOutput::open(dir);
        }
        let uv = find_uv().ok_or(SynthError::UvMissing)?;
        let parent = dir.parent().expect("cache dir has a parent");
        fs::create_dir_all(parent).map_err(|source| SynthError::Io {
            path: parent.to_owned(),
            source,
        })?;
        let tmp = parent.join(format!(
            ".tmp-{}-{}-{}",
            self.scenario,
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut cmd = Command::new(uv);
        cmd.current_dir(paths::repo_root())
            .args(["run", "--locked", "--quiet", "--project"])
            .arg(paths::py_project())
            .args(["python", "-m", "hkpy.synth", &self.scenario, "--seed"])
            .arg(self.seed.to_string())
            .arg("--out")
            .arg(&tmp)
            .args(["--datatype", self.datatype.as_str()]);
        for (k, v) in &self.params {
            cmd.arg("--param").arg(format!("{k}={v}"));
        }
        let output = cmd.output().map_err(|source| SynthError::Io {
            path: PathBuf::from("uv"),
            source,
        })?;
        if !output.status.success() {
            let _ = fs::remove_dir_all(&tmp);
            return Err(SynthError::GeneratorFailed {
                scenario: self.scenario.clone(),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        if let Err(source) = fs::rename(&tmp, &dir) {
            let _ = fs::remove_dir_all(&tmp);
            // Another test generated the same scenario first; use theirs.
            if !dir.join("manifest.json").is_file() {
                return Err(SynthError::Io { path: dir, source });
            }
        }
        SynthOutput::open(dir)
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A generated scenario directory.
#[derive(Clone, Debug)]
pub struct SynthOutput {
    /// Output directory.
    pub dir: PathBuf,
    /// Parsed `manifest.json`.
    pub manifest: Value,
    /// `.sigmf-meta` paths, in manifest order.
    pub recordings: Vec<PathBuf>,
}

impl SynthOutput {
    /// Opens a generated directory by its `manifest.json`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, SynthError> {
        let dir = dir.as_ref().to_owned();
        let manifest = read_json(&dir.join("manifest.json"))?;
        let recordings = manifest
            .get("recordings")
            .and_then(Value::as_array)
            .ok_or_else(|| SynthError::BadOutput {
                path: dir.join("manifest.json"),
                message: "missing recordings array".into(),
            })?
            .iter()
            .filter_map(Value::as_str)
            .map(|name| dir.join(name))
            .collect();
        Ok(Self {
            dir,
            manifest,
            recordings,
        })
    }

    /// Loads recording `index` as a [`Fixture`].
    pub fn fixture(&self, index: usize) -> Result<Fixture, SynthError> {
        let path = self
            .recordings
            .get(index)
            .ok_or_else(|| SynthError::BadOutput {
                path: self.dir.join("manifest.json"),
                message: format!("no recording {index} (have {})", self.recordings.len()),
            })?;
        Ok(Fixture::load(path)?)
    }

    /// Loads every recording.
    pub fn fixtures(&self) -> Result<Vec<Fixture>, SynthError> {
        (0..self.recordings.len())
            .map(|i| self.fixture(i))
            .collect()
    }

    /// An extra JSON truth file listed in the manifest, e.g. `schedule.json`.
    pub fn file_json(&self, name: &str) -> Result<Value, SynthError> {
        read_json(&self.dir.join(name))
    }

    /// Use-case IDs the scenario serves.
    pub fn use_cases(&self) -> Vec<String> {
        self.manifest
            .get("use_cases")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn read_json(path: &Path) -> Result<Value, SynthError> {
    let text = fs::read_to_string(path).map_err(|source| SynthError::Io {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|e| SynthError::BadOutput {
        path: path.to_owned(),
        message: e.to_string(),
    })
}

fn find_uv() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("HK_UV") {
        let p = PathBuf::from(explicit);
        return p.is_file().then_some(p);
    }
    let exe = if cfg!(windows) { "uv.exe" } else { "uv" };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(exe))
        .find(|p| p.is_file())
}

fn generator_sources() -> Result<Vec<PathBuf>, SynthError> {
    let py = paths::py_project();
    let synth = py.join("hkpy/synth");
    let entries = fs::read_dir(&synth).map_err(|source| SynthError::Io {
        path: synth.clone(),
        source,
    })?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "py"))
        .collect();
    files.sort();
    files.extend(
        ["hkpy/sigmf.py", "pyproject.toml", "uv.lock"]
            .iter()
            .map(|f| py.join(f)),
    );
    Ok(files)
}

/// Two FNV-1a 64 streams with different offset bases, length-prefixed per field. A cache key,
/// not a security hash.
struct Hasher([u64; 2]);

impl Hasher {
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self([0xcbf2_9ce4_8422_2325, 0x6c62_272e_07bb_0142])
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for (i, state) in self.0.iter_mut().enumerate() {
            for &b in bytes {
                *state ^= u64::from(b) ^ (i as u64 * 0x9e);
                *state = state.wrapping_mul(Self::PRIME);
            }
        }
    }

    fn field(&mut self, bytes: &[u8]) {
        self.bytes(&(bytes.len() as u64).to_le_bytes());
        self.bytes(bytes);
    }

    fn hex(&self) -> String {
        format!("{:016x}{:016x}", self.0[0], self.0[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_depends_on_every_request_field() {
        let base = SynthRequest::new("tone").seed(1);
        let key = base.cache_key().unwrap();
        assert_eq!(key.len(), 32);
        assert_eq!(key, base.clone().cache_key().unwrap());
        for other in [
            SynthRequest::new("tone").seed(2),
            SynthRequest::new("fsk_burst_train").seed(1),
            base.clone().param("offset_hz", 5),
            base.clone().datatype(Datatype::Cf32Le),
        ] {
            assert_ne!(key, other.cache_key().unwrap(), "{other:?}");
        }
        // Field boundaries are length-prefixed: ("ab", "c") differs from ("a", "bc").
        assert_ne!(
            base.clone().param("ab", "c").cache_key().unwrap(),
            base.clone().param("a", "bc").cache_key().unwrap()
        );
    }
}
