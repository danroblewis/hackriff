//! `hk history`: offline spectrum-history maintenance (T-126). `import-sweep-csv` folds a
//! `hackrf_sweep` CSV into a data directory's history: the uncalibrated (dBFS) pyramid under
//! `<data-dir>/history/uncalibrated`, which `hk run` / `hk serve` read and write with the same
//! configuration. Run it while no other process uses the data directory.

use std::fmt::Write as _;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, bail};
use hk_model::{PowerUnit, Timestamp};
use hk_store::history::{
    Pyramid, SweepCsvImport, SweepCsvOptions, import_sweep_csv as import_csv, source_key,
};

/// Options of `hk history import-sweep-csv`.
#[derive(Clone, Debug)]
pub struct ImportSweepCsvArgs {
    /// The `hackrf_sweep` CSV.
    pub file: PathBuf,
    /// Data directory.
    pub data_dir: PathBuf,
    /// Offset of the file's timestamps from UTC, seconds.
    pub utc_offset_s: i32,
    /// Seconds each line represents; `None` measures the sweep revisit.
    pub revisit_s: Option<f64>,
    /// Bin noise shape (look count) if known; `None` estimates it from the file.
    pub bin_shape: Option<f32>,
    /// Source name (provenance step state is kept per source).
    pub source: String,
}

/// The pyramid directory the pipeline's uncalibrated history lives in.
pub fn history_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("history").join("uncalibrated")
}

/// Opens the data directory's uncalibrated pyramid with the pipeline's configuration.
pub fn open_uncalibrated(data_dir: &Path) -> anyhow::Result<Pyramid> {
    let mut config = hk_store::FloorProductConfig::default().pyramid;
    config.unit = PowerUnit::Dbfs;
    let dir = history_dir(data_dir);
    Pyramid::open(&dir, config).with_context(|| format!("opening history at {}", dir.display()))
}

/// Parses a UTC offset: `Z`, `+HH:MM`, `-HHMM`, `+HH`, or whole seconds (`3600`, `-18000`).
pub fn parse_utc_offset(s: &str) -> anyhow::Result<i32> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("z") || s.is_empty() {
        return Ok(0);
    }
    let (sign, rest) = match s.as_bytes()[0] {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => (1, s),
    };
    let secs = if let Some((h, m)) = rest.split_once(':') {
        h.parse::<i32>()? * 3600 + m.parse::<i32>()? * 60
    } else if (s.starts_with('+') || s.starts_with('-')) && (rest.len() == 2 || rest.len() == 4) {
        let h: i32 = rest[..2].parse()?;
        let m: i32 = if rest.len() == 4 {
            rest[2..].parse()?
        } else {
            0
        };
        h * 3600 + m * 60
    } else {
        rest.parse::<i32>()
            .with_context(|| format!("UTC offset {s:?}: expected +HH:MM or seconds"))?
    };
    if secs.abs() > 18 * 3600 {
        bail!("UTC offset {s:?} is beyond ±18 h");
    }
    Ok(sign * secs)
}

/// Imports the CSV, seals the tiles its last sweep completes, and checkpoints the rest.
pub fn import_sweep_csv(args: &ImportSweepCsvArgs) -> anyhow::Result<SweepCsvImport> {
    let revisit = match args.revisit_s {
        Some(s) if s.is_finite() && s > 0.0 => Some(Duration::from_secs_f64(s)),
        Some(s) => bail!("--revisit-s must be positive, got {s}"),
        None => None,
    };
    let reader = BufReader::new(
        File::open(&args.file).with_context(|| format!("opening {}", args.file.display()))?,
    );
    let mut pyramid = open_uncalibrated(&args.data_dir)?;
    let opts = SweepCsvOptions {
        revisit,
        utc_offset_s: args.utc_offset_s,
        bin_shape: args.bin_shape,
        source: source_key(&args.source),
        ..SweepCsvOptions::default()
    };
    let out = import_csv(&mut pyramid, reader, &opts)
        .with_context(|| format!("importing {}", args.file.display()))?;
    if let Some(last) = out.last {
        let end = last.as_unix_nanos().saturating_add(out.revisit_ns);
        pyramid.seal_through(Timestamp::from_unix_nanos(end))?;
    }
    pyramid.close()?;
    Ok(out)
}

/// A human-readable summary of an import.
pub fn import_summary(out: &SweepCsvImport) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "rows {}  folded {}  late {}  rejected {}",
        out.rows, out.frames_folded, out.frames_late, out.rows_rejected
    );
    let _ = writeln!(s, "revisit {:.3} s", out.revisit_ns as f64 * 1e-9);
    if let (Some(a), Some(b)) = (out.first, out.last) {
        let _ = writeln!(s, "time {} .. {}", a.as_unix_nanos(), b.as_unix_nanos());
    }
    match out.bin_shape {
        Some(k) => {
            let _ = writeln!(s, "bin noise shape k = {k:.2} (floor_db available)");
        }
        None => {
            let _ = writeln!(
                s,
                "bin noise shape unknown (too few sweeps/bins): floor_db not available"
            );
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use hk_model::{FreqRange, TimeRange};
    use hk_store::history::{RegionQuery, Resolution};

    use super::*;

    #[test]
    fn utc_offsets_parse() {
        assert_eq!(parse_utc_offset("Z").unwrap(), 0);
        assert_eq!(parse_utc_offset("+02:00").unwrap(), 7200);
        assert_eq!(parse_utc_offset("-0530").unwrap(), -19_800);
        assert_eq!(parse_utc_offset("-05").unwrap(), -18_000);
        assert_eq!(parse_utc_offset("3600").unwrap(), 3600);
        assert!(parse_utc_offset("+25:00").is_err());
        assert!(parse_utc_offset("soon").is_err());
    }

    #[test]
    fn import_sweep_csv_command_folds_into_the_pipeline_history() {
        let dir = std::env::temp_dir().join(format!("hk-cli-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 40 sweeps, 1 s apart, local time UTC+02:00; 4 segments × 64 bins of 10 kHz noise.
        let mut csv = String::new();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for s in 0..40 {
            for seg in 0..4u64 {
                let lo = 100_000_000 + seg * 640_000;
                let _ = write!(
                    csv,
                    "2026-09-13, 14:00:{s:02}.250000, {lo}, {}, 10000.00, 20",
                    lo + 640_000
                );
                for _ in 0..64 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let u = ((x >> 11) as f64 / (1u64 << 53) as f64).max(1e-12);
                    let _ = write!(csv, ", {:.2}", -60.0 + 10.0 * (-u.ln()).log10());
                }
                csv.push('\n');
            }
        }
        let file = dir.join("sweep.csv");
        std::fs::write(&file, csv).unwrap();
        let out = import_sweep_csv(&ImportSweepCsvArgs {
            file,
            data_dir: dir.clone(),
            utc_offset_s: parse_utc_offset("+02:00").unwrap(),
            revisit_s: None,
            bin_shape: None,
            source: "hackrf_sweep".into(),
        })
        .unwrap();
        assert_eq!(
            (out.rows, out.frames_folded, out.rows_rejected),
            (160, 160, 0)
        );
        assert_eq!(out.revisit_ns, 1_000_000_000);
        let k = out.bin_shape.expect("shape estimated");
        assert!((k - 1.0).abs() < 0.1, "k {k}");
        assert!(import_summary(&out).contains("floor_db available"));
        // 2026-09-13T12:00:00Z (the file's 14:00 at +02:00).
        let t0 = 1_789_300_800i64 * 1_000_000_000;
        assert_eq!(out.first.unwrap().as_unix_nanos(), t0 + 250_000_000);

        let p = open_uncalibrated(&dir).unwrap();
        let h = p
            .query(&RegionQuery {
                freq: FreqRange::new(100_000_000.0, 102_560_000.0),
                time: TimeRange::new(
                    Timestamp::from_unix_nanos(t0),
                    Timestamp::from_unix_nanos(t0 + 40_000_000_000),
                ),
                resolution: Resolution::MaxCells { t: 64, f: 512 },
            })
            .unwrap();
        let observed: Vec<_> = h.cells.iter().filter(|c| c.observed()).collect();
        assert!(!observed.is_empty());
        assert!(observed.iter().all(|c| c.floor_db.is_finite()));
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
