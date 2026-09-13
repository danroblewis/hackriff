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

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use hk_model::sigmf::SigmfMeta;

use crate::fixture::TruthItem;

/// Boxed error for the harness helpers.
pub type BlindError = Box<dyn std::error::Error + Send + Sync>;

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
