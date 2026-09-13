//! Guard: every shared-state atomic operation in the ring must use `Ordering::SeqCst`.
//!
//! The ring's seqlock proof needs sequential consistency, which Rust guarantees only for
//! `SeqCst`. The weaker orderings were empirically not linearisable on the dev toolchain (M3
//! Ultra, rustc 1.93.1 / LLVM 21) and caused stale reads in release builds; see the
//! "Why every shared operation is `SeqCst`" section of `src/ring/mod.rs`. Re-verify on the
//! Jetson (aarch64 Linux) before relaxing this guard.
//!
//! The scan covers every `.rs` file under `src/ring/`. Comments are ignored; in code, any
//! whole-word `Relaxed`, `Acquire`, `Release` or `AcqRel` fails (this catches
//! `Ordering::Relaxed`, `use ...::Ordering::Relaxed`, glob imports used bare, and renamed
//! `Ordering` paths). A line may be exempted only by an `// ordering-exempt: <reason>` comment
//! on the same line with a non-empty reason; there are none today.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: [&str; 4] = ["Relaxed", "Acquire", "Release", "AcqRel"];
const EXEMPT_MARKER: &str = "// ordering-exempt:";

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whole-word occurrences of forbidden orderings in `code`.
fn forbidden_words(code: &str) -> Vec<&'static str> {
    let mut found = Vec::new();
    for word in FORBIDDEN {
        let mut from = 0;
        while let Some(off) = code[from..].find(word) {
            let at = from + off;
            let end = at + word.len();
            let before_ok = !code[..at].chars().next_back().is_some_and(is_ident_char);
            let after_ok = !code[end..].chars().next().is_some_and(is_ident_char);
            if before_ok && after_ok {
                found.push(word);
            }
            from = end;
        }
    }
    found
}

/// Violations in one source text as `(line number, line)`.
fn violations(source: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (n, line) in source.lines().enumerate() {
        let (code, comment) = match line.find("//") {
            Some(i) => (&line[..i], &line[i..]),
            None => (line, ""),
        };
        if forbidden_words(code).is_empty() {
            continue;
        }
        let exempt = comment
            .strip_prefix(EXEMPT_MARKER)
            .is_some_and(|reason| !reason.trim().is_empty());
        if !exempt {
            out.push((n + 1, line.trim().to_owned()));
        }
    }
    out
}

#[test]
fn ring_uses_only_seqcst_atomics() {
    let ring_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ring");
    let mut files = Vec::new();
    rust_files(&ring_dir, &mut files);
    files.sort();
    assert!(
        files.iter().any(|f| f.ends_with("ring/mod.rs")),
        "ring sources not found under {}",
        ring_dir.display()
    );

    let mut report = Vec::new();
    let mut seqcst_uses = 0;
    for file in &files {
        let source = fs::read_to_string(file).expect("read ring source");
        seqcst_uses += source.matches("SeqCst").count();
        for (line_no, line) in violations(&source) {
            report.push(format!("{}:{line_no}: {line}", file.display()));
        }
    }
    assert!(
        report.is_empty(),
        "non-SeqCst atomic ordering in the ring (use SeqCst, or add \
         `{EXEMPT_MARKER} <reason>` on the same line after re-verifying linearisability):\n{}",
        report.join("\n")
    );
    assert!(
        seqcst_uses > 0,
        "scan found no SeqCst at all; is it reading the ring?"
    );
}

#[test]
fn scanner_catches_and_exempts_as_specified() {
    let flagged = [
        "x.load(Ordering::Relaxed);",
        "x.store(1, Ordering::Release);",
        "x.compare_exchange(a, b, Ordering::AcqRel, Ordering::Acquire)",
        "use std::sync::atomic::Ordering::Relaxed;",
        "use std::sync::atomic::{AtomicU64, Ordering::{Relaxed, SeqCst}};",
        "    Relaxed,",
        "x.load(O::Relaxed); // ordering-exempt:",
        "x.load(O::Relaxed); // some other comment",
    ];
    for line in flagged {
        assert_eq!(violations(line).len(), 1, "should flag: {line}");
    }
    let clean = [
        "x.load(Ordering::SeqCst);",
        "const SEQ: Ordering = Ordering::SeqCst;",
        "//! The first version used `Relaxed`, which was not linearisable.",
        "x.load(SEQ); // not Relaxed",
        "let relaxed_mode = RelaxedPolicy::new();",
        "x.load(Ordering::Relaxed); // ordering-exempt: diagnostic counter, never validated",
    ];
    for line in clean {
        assert!(violations(line).is_empty(), "should not flag: {line}");
    }
}
