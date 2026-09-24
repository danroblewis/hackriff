//! A tripwire on a documented claim, not a lock on the design (T-363).
//!
//! `src/lib.rs` declares that [`hk_ml::host::ModelHost`] is reachable only from this crate's own
//! tests, and says exactly what would have to change before wiring it is correct: a model in the
//! registry, a family that clears ADR-0016 §4.6 for at least `shadow`, and a durable `ShadowSink`
//! backed by hk-store. That declaration exists so the next reader does not mistake a deliberately
//! dormant host for the project's recurring "capability with no caller" defect.
//!
//! A declaration nothing checks decays into a lie. This test fails the day a caller appears, so
//! whoever wires the host is pointed back at that section to delete it. **A failure here is not a
//! bug in the caller** — it is the note going stale, and the fix is to remove the note (and this
//! test) in the same change that adds the wiring.
//!
//! **It is also the premise of a gate row.** `tests/e2e/tests/acceptance/m3_ml.rs` asserts
//! ADR-0016 §7's ML row over a real run, and reports an empty `(model, consumer)` mode table on the
//! strength of this test standing. Removing this file therefore fails that gate too, which is the
//! point: the wiring change has to give the gate a real enumeration
//! ([`hk_ml::exit_gate::MlGateSnapshot::from_host`]) in the same breath as it deletes the note.
//!
//! It scans sources rather than manifests because `hk-classify` already *depends* on `hk-ml`
//! legitimately: T-204's DL stage is written against `LoadedModel` and `Calibrator` and never
//! touches the host. A manifest-edge test in T-274's style would therefore already be red for the
//! wrong reason; what is dormant here is the host, not the crate.

use std::fs;
use std::path::{Path, PathBuf};

/// The names whose appearance outside `hk-ml` means the host has been wired.
const HOST_ITEMS: [&str; 2] = ["ModelHost", "HostStats"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/hk-ml sits two levels under the workspace root")
        .to_path_buf()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            // `target` holds build output, and this crate is the one place the host may be named.
            if name == "target" || name == "hk-ml" || name.starts_with('.') {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_model_host_still_has_no_caller_outside_this_crate() {
    let root = workspace_root();
    let crates = root.join("crates");
    assert!(
        crates.is_dir(),
        "expected a workspace at {} — this test must scan real sources or it proves nothing",
        root.display()
    );

    let mut sources = Vec::new();
    rust_sources(&crates, &mut sources);
    rust_sources(&root.join("tests"), &mut sources);
    assert!(
        sources.len() > 100,
        "only {} sources scanned under {} — the walk is not reaching the workspace",
        sources.len(),
        root.display()
    );

    let mut callers = Vec::new();
    for path in &sources {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            if HOST_ITEMS.iter().any(|item| line.contains(item)) {
                let rel = path.strip_prefix(&root).unwrap_or(path);
                callers.push(format!("{}:{}: {}", rel.display(), n + 1, line.trim()));
            }
        }
    }

    assert!(
        callers.is_empty(),
        "the model host now has a caller outside hk-ml:\n  {}\n\n\
         That is not a bug — it is what this crate has been waiting for. But `hk-ml/src/lib.rs` \
         still declares the host dormant with no production caller, and that section is now \
         stale. Delete it (and this test) in the same change that adds the wiring, and prove the \
         new caller is not vacuous the way T-287 and T-297 did: remove the wiring and one test \
         fails, remove the host and a different one fails.",
        callers.join("\n  ")
    );
}
