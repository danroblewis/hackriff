//! hackriff binaries: `hackriffd`, the headless daemon that owns the device and pipeline, and
//! `hk`, the control CLI. `hk replay <path.sigmf-meta>` runs a SigMF fixture through the
//! pipeline. Until the replay source (T-003) and harness (T-023) land, it parses the metadata
//! and prints a summary.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use anyhow::Context as _;
use hk_model::sigmf::{SigmfMeta, data_path_for};

const LFS_POINTER_PREFIX: &[u8] = b"version https://git-lfs.github.com/spec/v1";

/// Parses a `.sigmf-meta` file and summarises it and its paired `.sigmf-data` file.
pub fn replay_summary(meta_path: &Path) -> anyhow::Result<String> {
    let meta = SigmfMeta::read(meta_path)
        .with_context(|| format!("reading SigMF metadata {}", meta_path.display()))?;
    let g = &meta.global;
    let mut out = String::new();

    writeln!(out, "recording:   {}", meta_path.display())?;
    writeln!(out, "sigmf:       {}", g.version)?;
    writeln!(
        out,
        "datatype:    {} ({} bytes/sample, {})",
        g.datatype,
        g.datatype.bytes_per_sample(),
        if g.datatype.is_complex() {
            "complex"
        } else {
            "real"
        }
    )?;
    match g.sample_rate {
        Some(fs) => writeln!(out, "sample rate: {fs} Hz")?,
        None => writeln!(out, "sample rate: (not set)")?,
    }
    for (label, value) in [
        ("description", &g.description),
        ("hw", &g.hw),
        ("recorder", &g.recorder),
        ("license", &g.license),
    ] {
        if let Some(v) = value {
            writeln!(out, "{label:<12} {v}")?;
        }
    }
    if !g.extensions.is_empty() {
        let names: Vec<String> = g
            .extensions
            .iter()
            .map(|e| format!("{} {}", e.name, e.version))
            .collect();
        writeln!(out, "extensions:  {}", names.join(", "))?;
    }
    if let Some(p) = &g.provenance {
        writeln!(
            out,
            "provenance:  device {}, {} Hz @ {} Hz, LNA {} dB, VGA {} dB, amp {}, overload {}, quantisation-limited {}, time {:?}",
            p.device_id,
            p.tune.center_hz,
            p.tune.sample_rate_hz,
            p.tune.lna_db,
            p.tune.vga_db,
            if p.tune.amp_on { "on" } else { "off" },
            p.overload,
            p.quantisation_limited,
            p.timestamp_method,
        )?;
    }

    writeln!(out, "captures:    {}", meta.captures.len())?;
    for (i, c) in meta.captures.iter().enumerate() {
        write!(out, "  [{i}] sample_start {}", c.sample_start)?;
        if let Some(f) = c.frequency {
            write!(out, ", frequency {f} Hz")?;
        }
        if let Some(t) = &c.datetime {
            write!(out, ", datetime {t}")?;
        }
        if c.provenance.is_some() {
            write!(out, ", provenance")?;
        }
        if let Some(n) = c.clip_count {
            write!(out, ", clips {n}")?;
        }
        writeln!(out)?;
    }

    writeln!(out, "annotations: {}", meta.annotations.len())?;
    for (i, a) in meta.annotations.iter().enumerate() {
        write!(out, "  [{i}] sample_start {}", a.sample_start)?;
        if let Some(n) = a.sample_count {
            write!(out, " +{n}")?;
        }
        if let (Some(lo), Some(hi)) = (a.freq_lower_edge, a.freq_upper_edge) {
            write!(out, ", {lo}..{hi} Hz")?;
        }
        if let Some(l) = &a.label {
            write!(out, ", label {l:?}")?;
        }
        if let Some(t) = &a.truth {
            write!(out, ", truth {t}")?;
        }
        writeln!(out)?;
    }

    let data_path = data_path_for(meta_path);
    match File::open(&data_path) {
        Err(_) => writeln!(out, "data:        {} (missing)", data_path.display())?,
        Ok(mut file) => {
            let len = file.metadata()?.len();
            let mut head = [0u8; LFS_POINTER_PREFIX.len()];
            let is_pointer = file.read_exact(&mut head).is_ok() && head == LFS_POINTER_PREFIX;
            if is_pointer {
                writeln!(
                    out,
                    "data:        {} is a Git LFS pointer; run `git lfs install && git lfs pull`",
                    data_path.display()
                )?;
            } else {
                let samples = len / g.datatype.bytes_per_sample() as u64;
                write!(
                    out,
                    "data:        {} ({len} bytes, {samples} samples",
                    data_path.display()
                )?;
                if let Some(fs) = g.sample_rate {
                    write!(out, ", {:.6} s", samples as f64 / fs)?;
                }
                writeln!(out, ")")?;
            }
        }
    }
    writeln!(
        out,
        "pipeline:    not wired yet (T-003 replay source, T-023 harness)"
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn summarises_the_tiny_tone_fixture() {
        let meta =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/tiny/tone.sigmf-meta");
        let summary = replay_summary(&meta).unwrap();
        assert!(
            summary.contains("datatype:    ci8 (2 bytes/sample, complex)"),
            "{summary}"
        );
        assert!(summary.contains("captures:    1"), "{summary}");
        assert!(summary.contains("label \"tone\""), "{summary}");
        assert!(
            summary.contains("provenance:  device synthetic:hkpy"),
            "{summary}"
        );
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(replay_summary(Path::new("does/not/exist.sigmf-meta")).is_err());
    }
}
