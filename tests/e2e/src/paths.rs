//! Repository locations used by the harness.

use std::path::{Path, PathBuf};

/// The repository root (two levels above this crate).
pub fn repo_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}

/// Cargo's target directory: `$CARGO_TARGET_DIR` or `<repo>/target`.
pub fn target_dir() -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if dir.is_relative() {
                repo_root().join(dir)
            } else {
                dir
            }
        }
        None => repo_root().join("target"),
    }
}

/// Where generated scenarios are cached: `$HK_SYNTH_CACHE` or `<target>/synth-cache`.
pub fn synth_cache_dir() -> PathBuf {
    std::env::var_os("HK_SYNTH_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| target_dir().join("synth-cache"))
}

/// The Python tooling project (`py/`).
pub fn py_project() -> PathBuf {
    repo_root().join("py")
}
