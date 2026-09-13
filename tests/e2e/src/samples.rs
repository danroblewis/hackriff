//! **SEAM(T-003): replace with hk-core's replay source.**
//!
//! A deliberately minimal `.sigmf-data` reader so replay tests can run before T-003 lands. It
//! reads a whole file or a sample range into memory; no ring buffer, no pacing, no streaming.
//! When hk-core's replay source merges, [`read_samples`] and [`Cf32`] should become thin adapters
//! over it (or be deleted), and [`crate::Pipeline::run`] should pull blocks from that source.
//!
//! Scaling matches the synthetic generator (`py/hkpy/synth/scene.py`): full scale is 1.0 per
//! component, so `ci8` is divided by 127 and `cu8` is `(v - 127.5) / 127.5`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use hk_model::sigmf::Datatype;

/// One complex sample, normalised to full scale 1.0 per component.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cf32 {
    /// In-phase component.
    pub re: f32,
    /// Quadrature component.
    pub im: f32,
}

impl Cf32 {
    /// `re² + im²`.
    pub fn norm_sqr(self) -> f32 {
        self.re * self.re + self.im * self.im
    }
}

/// Errors reading samples.
#[derive(Debug, thiserror::Error)]
pub enum SampleError {
    /// The seam reader handles only ci8, cu8 and cf32_le.
    #[error("datatype {0} is not supported by the minimal replay reader (ci8, cu8, cf32_le)")]
    Unsupported(Datatype),
    /// The data file could not be read.
    #[error("{path}: {source}")]
    Io {
        /// File involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not a whole number of samples, or is a Git LFS pointer.
    #[error(
        "{path}: {len} bytes is not a whole number of {bytes_per_sample}-byte samples (unfetched Git LFS file?)"
    )]
    Truncated {
        /// File involved.
        path: PathBuf,
        /// File length in bytes.
        len: u64,
        /// Bytes per sample for the datatype.
        bytes_per_sample: usize,
    },
}

/// Number of samples in a data file.
pub fn sample_count(path: &Path, datatype: Datatype) -> Result<u64, SampleError> {
    let len = std::fs::metadata(path)
        .map_err(|source| SampleError::Io {
            path: path.to_owned(),
            source,
        })?
        .len();
    let bps = datatype.bytes_per_sample();
    if len % bps as u64 != 0 {
        return Err(SampleError::Truncated {
            path: path.to_owned(),
            len,
            bytes_per_sample: bps,
        });
    }
    Ok(len / bps as u64)
}

/// Reads `count` samples starting at sample `start` (`None`: to the end of the file).
pub fn read_samples(
    path: &Path,
    datatype: Datatype,
    start: u64,
    count: Option<u64>,
) -> Result<Vec<Cf32>, SampleError> {
    if !matches!(datatype, Datatype::Ci8 | Datatype::Cu8 | Datatype::Cf32Le) {
        return Err(SampleError::Unsupported(datatype));
    }
    let io = |source| SampleError::Io {
        path: path.to_owned(),
        source,
    };
    let total = sample_count(path, datatype)?;
    let start = start.min(total);
    let count = count.unwrap_or(total - start).min(total - start);
    let bps = datatype.bytes_per_sample();
    let mut file = File::open(path).map_err(io)?;
    file.seek(SeekFrom::Start(start * bps as u64)).map_err(io)?;
    let mut buf = vec![0u8; count as usize * bps];
    file.read_exact(&mut buf).map_err(io)?;
    Ok(decode(&buf, datatype))
}

fn decode(buf: &[u8], datatype: Datatype) -> Vec<Cf32> {
    let bps = datatype.bytes_per_sample();
    buf.chunks_exact(bps)
        .map(|c| match datatype {
            Datatype::Ci8 => Cf32 {
                re: f32::from(c[0] as i8) / 127.0,
                im: f32::from(c[1] as i8) / 127.0,
            },
            Datatype::Cu8 => Cf32 {
                re: (f32::from(c[0]) - 127.5) / 127.5,
                im: (f32::from(c[1]) - 127.5) / 127.5,
            },
            _ => Cf32 {
                re: f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                im: f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_each_supported_datatype() {
        let ci8 = decode(&[127, 0x81], Datatype::Ci8);
        assert_eq!(ci8, vec![Cf32 { re: 1.0, im: -1.0 }]);
        let cu8 = decode(&[255, 0], Datatype::Cu8);
        assert_eq!(cu8[0].re, 1.0);
        assert_eq!(cu8[0].im, -1.0);
        let mut f = 0.5f32.to_le_bytes().to_vec();
        f.extend((-0.25f32).to_le_bytes());
        assert_eq!(
            decode(&f, Datatype::Cf32Le),
            vec![Cf32 { re: 0.5, im: -0.25 }]
        );
    }

    #[test]
    fn reads_a_range_and_rejects_partial_files() {
        let dir = std::env::temp_dir().join(format!("hk-e2e-samples-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.sigmf-data");
        std::fs::write(&path, [1u8, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let s = read_samples(&path, Datatype::Ci8, 1, Some(2)).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].re, 3.0 / 127.0);
        assert_eq!(sample_count(&path, Datatype::Ci8).unwrap(), 4);
        assert!(matches!(
            read_samples(&path, Datatype::Ci16Le, 0, None),
            Err(SampleError::Unsupported(_))
        ));
        std::fs::write(&path, [1u8, 2, 3]).unwrap();
        assert!(matches!(
            sample_count(&path, Datatype::Ci8),
            Err(SampleError::Truncated { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
