//! Blind-test harness pieces (T-039; T-047 builds the general ground-truth harness on them).
//!
//! The system under test never sees ground truth. A blind test:
//! 1. **Strips truth.** [`strip_truth`] writes a copy of the SigMF metadata with no annotations and
//!    no free-text description. It can optionally relabel the RF frequency, and it links the
//!    samples.
//! 2. **Optionally synthesises IQ.** [`shift_ci8`] frequency-shifts a ci8 recording, e.g. to move
//!    a station off its channel raster.
//! 3. **Runs blind.** The copy is replayed through the pipeline.
//! 4. **Matches privately.** The test loads the truth it holds (`Fixture::load` on the original)
//!    and matches *everything* the pipeline produced against it with [`matches_truth`] /
//!    [`matching`]. The test never looks a frequency up in the database.

use std::fs::{File, FileTimes};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use hk_model::sigmf::SigmfMeta;

use crate::fixture::{Fixture, Role, TruthItem};

/// Boxed error for the harness helpers.
pub type BlindError = Box<dyn std::error::Error + Send + Sync>;

/// The fixture's private truth list (T-047): every emission blind detection must find, as a
/// frequency extent, a time span and a label. Generated fixtures mark them `role: emission`; hand
/// labels without a role count when they carry finite frequency edges.
pub fn truth_emissions(fx: &Fixture) -> Vec<&TruthItem> {
    fx.truth
        .iter()
        .filter(|t| {
            t.role == Role::Emission
                || (t.role == Role::Unlabelled && t.f_lo_hz.is_finite() && t.f_hi_hz.is_finite())
        })
        .collect()
}

/// Panics unless `meta_path` is truth-free: no annotations, no description and no
/// `hackriff:truth` or `core:label` key anywhere in the file.
pub fn assert_truth_free(meta_path: &Path) {
    let meta = SigmfMeta::read(meta_path).expect("stripped metadata reads");
    assert!(
        meta.annotations.is_empty(),
        "{}: annotations reach the system",
        meta_path.display()
    );
    assert!(
        meta.global.description.is_none(),
        "{}: description reaches the system",
        meta_path.display()
    );
    let text = std::fs::read_to_string(meta_path).expect("stripped metadata is text");
    for key in ["hackriff:truth", "core:label", "core:description"] {
        assert!(
            !text.contains(key),
            "{}: {key} reaches the system",
            meta_path.display()
        );
    }
}

/// An access time far older than any file's modification time, so a later read updates it
/// under `relatime` as well as `strictatime`.
fn sealed_atime() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000)
}

fn set_atime(path: &Path, t: SystemTime) -> std::io::Result<()> {
    File::options()
        .write(true)
        .open(path)?
        .set_times(FileTimes::new().set_accessed(t))
}

/// Whether reads in `dir` update access times (checked on a canary file).
fn atime_tracked(dir: &Path) -> std::io::Result<bool> {
    let canary = dir.join("atime-canary");
    std::fs::write(&canary, b"canary")?;
    set_atime(&canary, sealed_atime())?;
    let _ = std::fs::read(&canary)?;
    Ok(std::fs::metadata(&canary)?.accessed()? != sealed_atime())
}

/// A fixture's truth (its full original metadata), sealed in a private directory the system is
/// never given. [`TruthVault::assert_unopened`] proves nothing read it, wherever the filesystem
/// records access times (checked on a canary at seal time; otherwise only the path isolation and
/// the output scans of the harness hold).
#[derive(Debug)]
pub struct TruthVault {
    /// The sealed truth file.
    pub path: PathBuf,
    tracked: bool,
}

impl TruthVault {
    /// Copies `original_meta` to `<dir>/truth.sigmf-meta` and seals its access time.
    pub fn seal(original_meta: &Path, dir: &Path) -> Result<Self, BlindError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("truth.sigmf-meta");
        std::fs::copy(original_meta, &path)?;
        let tracked = atime_tracked(dir)?;
        set_atime(&path, sealed_atime())?;
        Ok(Self { path, tracked })
    }

    /// Whether the check is effective here (the filesystem records access times).
    pub fn tracked(&self) -> bool {
        self.tracked
    }

    /// Panics if the sealed truth file was read since [`TruthVault::seal`].
    pub fn assert_unopened(&self) {
        if !self.tracked {
            return;
        }
        let at = std::fs::metadata(&self.path)
            .and_then(|m| m.accessed())
            .expect("truth file metadata");
        assert_eq!(
            at,
            sealed_atime(),
            "the truth file {} was opened during the blind run",
            self.path.display()
        );
    }
}

/// Writes `<out_dir>/<name>.sigmf-meta`: `meta_path`'s metadata without truth. Annotations are
/// removed and `core:description` cleared. Every capture frequency and provenance tune centre is
/// moved by `relabel_hz` (0 keeps them). Unless `<out_dir>/<name>.sigmf-data` already exists
/// (e.g. written by [`shift_ci8`]), the original samples are linked there. Returns the new meta
/// path.
pub fn strip_truth(
    meta_path: &Path,
    out_dir: &Path,
    name: &str,
    relabel_hz: f64,
) -> Result<PathBuf, BlindError> {
    let mut meta = SigmfMeta::read(meta_path)?;
    meta.annotations.clear();
    meta.global.description = None;
    for cap in &mut meta.captures {
        cap.frequency = cap.frequency.map(|f| f + relabel_hz);
        if let Some(p) = &mut cap.provenance {
            p.tune.center_hz += relabel_hz;
        }
    }
    if let Some(p) = &mut meta.global.provenance {
        p.tune.center_hz += relabel_hz;
    }
    let out_meta = out_dir.join(format!("{name}.sigmf-meta"));
    meta.write(&out_meta)?;
    let out_data = out_meta.with_extension("sigmf-data");
    if !out_data.exists() {
        let src = meta_path.with_extension("sigmf-data");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&src, &out_data)?;
        #[cfg(not(unix))]
        std::fs::copy(&src, &out_data)?;
    }
    Ok(out_meta)
}

/// Writes `dst`: the ci8 samples of `src` multiplied by `exp(j·2π·shift_hz·n/fs)`, rounded and
/// clamped to i8. Every emission in the recording moves up by `shift_hz` at an unchanged tuned
/// centre. Returns the number of samples written.
pub fn shift_ci8(src: &Path, dst: &Path, shift_hz: f64, fs: f64) -> Result<u64, BlindError> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(src)?);
    let mut w = BufWriter::with_capacity(1 << 20, File::create(dst)?);
    let step = std::f64::consts::TAU * shift_hz / fs;
    let mut buf = vec![0u8; 1 << 20];
    let mut n: u64 = 0;
    let mut carry: Option<u8> = None;
    loop {
        let got = r.read(&mut buf)?;
        if got == 0 {
            break;
        }
        let mut bytes: Vec<u8> = carry.take().into_iter().collect();
        bytes.extend_from_slice(&buf[..got]);
        if bytes.len() % 2 == 1 {
            carry = bytes.pop();
        }
        let mut out = Vec::with_capacity(bytes.len());
        for pair in bytes.chunks_exact(2) {
            let (i, q) = (f64::from(pair[0] as i8), f64::from(pair[1] as i8));
            let ph = step * n as f64;
            let (s, c) = ph.sin_cos();
            let q8 = |v: f64| v.round().clamp(-128.0, 127.0) as i8 as u8;
            out.push(q8(i * c - q * s));
            out.push(q8(i * s + q * c));
            n += 1;
        }
        w.write_all(&out)?;
    }
    w.flush()?;
    Ok(n)
}

/// A measured extent `(f_center_hz, bandwidth_hz)` matches `truth` moved by `shift_hz` when its
/// centre lies within `center_tol_hz` of the truth centre and the two extents overlap.
pub fn matches_truth(
    truth: &TruthItem,
    shift_hz: f64,
    f_center_hz: f64,
    bandwidth_hz: f64,
    center_tol_hz: f64,
) -> bool {
    let (lo, hi) = (truth.f_lo_hz + shift_hz, truth.f_hi_hz + shift_hz);
    let tc = 0.5 * (lo + hi);
    let (mlo, mhi) = (
        f_center_hz - 0.5 * bandwidth_hz,
        f_center_hz + 0.5 * bandwidth_hz,
    );
    (f_center_hz - tc).abs() <= center_tol_hz && mlo <= hi && mhi >= lo
}

/// The items whose extent (`extent(item) = (f_center_hz, bandwidth_hz)`) matches `truth`.
pub fn matching<'a, T>(
    truth: &TruthItem,
    shift_hz: f64,
    items: &'a [T],
    extent: impl Fn(&T) -> (f64, f64),
    center_tol_hz: f64,
) -> Vec<&'a T> {
    items
        .iter()
        .filter(|it| {
            let (f, bw) = extent(it);
            matches_truth(truth, shift_hz, f, bw, center_tol_hz)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Role;

    fn truth(lo: f64, hi: f64) -> TruthItem {
        TruthItem {
            annotation_index: 0,
            role: Role::Emission,
            kind: "wfm-broadcast".into(),
            label: None,
            sample_start: 0,
            sample_count: 0,
            t_start_s: 0.0,
            t_end_s: 1.0,
            f_lo_hz: lo,
            f_hi_hz: hi,
            value: serde_json::Value::Null,
        }
    }

    #[test]
    fn truth_matching_uses_centre_tolerance_and_overlap() {
        let t = truth(101.2e6, 101.4e6);
        assert!(matches_truth(&t, 0.0, 101.303e6, 333e3, 100e3));
        assert!(
            !matches_truth(&t, 0.0, 100.737e6, 2.0e6, 100e3),
            "wide blob"
        );
        assert!(matches_truth(&t, 150e3, 101.452e6, 250e3, 100e3));
        assert!(
            !matches_truth(&t, 150e3, 101.303e6, 250e3, 100e3),
            "not shifted"
        );
        let items = [(101.3e6, 200e3), (99.0e6, 200e3)];
        assert_eq!(matching(&t, 0.0, &items, |x| *x, 50e3).len(), 1);
    }

    #[test]
    fn truth_vault_detects_a_read_where_access_times_are_recorded() {
        let dir = std::env::temp_dir().join(format!("hk-vault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("fx.sigmf-meta");
        std::fs::write(
            &original,
            br#"{"global":{},"captures":[],"annotations":[]}"#,
        )
        .unwrap();
        let vault = TruthVault::seal(&original, &dir.join("private")).unwrap();
        vault.assert_unopened();
        let _ = std::fs::read(&vault.path).unwrap();
        let opened = std::panic::catch_unwind(|| vault.assert_unopened()).is_err();
        assert_eq!(
            opened,
            vault.tracked(),
            "a read of the sealed truth is detected exactly when access times are recorded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shift_ci8_moves_a_tone() {
        let dir = std::env::temp_dir().join(format!("hk-blind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (src, dst) = (dir.join("a.sigmf-data"), dir.join("b.sigmf-data"));
        // DC (constant 100 + 0j), shifted by fs/4: i, q cycle (100,0) (0,100) (-100,0) (0,-100).
        std::fs::write(&src, [100u8, 0].repeat(8)).unwrap();
        assert_eq!(shift_ci8(&src, &dst, 250.0, 1000.0).unwrap(), 8);
        let out: Vec<i8> = std::fs::read(&dst)
            .unwrap()
            .into_iter()
            .map(|b| b as i8)
            .collect();
        assert_eq!(&out[..8], &[100, 0, 0, 100, -100, 0, 0, -100]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
