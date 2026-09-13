//! Sample-format normalisation: SigMF datatypes to `Complex32` in [-1, 1).
//!
//! Conventions (scale by the full-scale code, so the most negative code maps to exactly -1):
//!
//! | Datatype | Mapping |
//! |---|---|
//! | `ci8` | `x / 128` |
//! | `cu8` | `(x - 128) / 128` (offset binary; identical to `ci8` with the sign bit flipped) |
//! | `ci16_le` | `x / 32768` |
//! | `cf32_le` | unchanged |
//!
//! The `cu8` DC offset of real RTL-SDR hardware is a calibration matter (C05), not corrected here.
//! The same decoder serves the future HackRF process pipe (`hackrf_transfer` writes ci8).

use hk_model::sigmf::Datatype;
use num_complex::Complex32;

use super::SourceError;

/// Datatypes [`decode_into`] can normalise.
pub const SUPPORTED: &[Datatype] = &[
    Datatype::Ci8,
    Datatype::Cu8,
    Datatype::Ci16Le,
    Datatype::Cf32Le,
];

/// `datatype` can be normalised to `Complex32`.
pub fn supports(datatype: Datatype) -> bool {
    SUPPORTED.contains(&datatype)
}

/// Appends the samples encoded in `bytes` to `out`, normalised to `Complex32`.
///
/// `bytes.len()` must be a whole number of samples. `out` does not reallocate when its spare
/// capacity covers the decoded samples.
pub fn decode_into(
    datatype: Datatype,
    bytes: &[u8],
    out: &mut Vec<Complex32>,
) -> Result<(), SourceError> {
    let bps = datatype.bytes_per_sample();
    if !supports(datatype) {
        return Err(SourceError::UnsupportedDatatype(datatype));
    }
    if bytes.len() % bps != 0 {
        return Err(SourceError::InvalidRecording(format!(
            "{} bytes is not a whole number of {datatype} samples",
            bytes.len()
        )));
    }
    let chunks = bytes.chunks_exact(bps);
    match datatype {
        Datatype::Ci8 => {
            out.extend(chunks.map(|c| {
                Complex32::new(f32::from(c[0] as i8) / 128.0, f32::from(c[1] as i8) / 128.0)
            }))
        }
        Datatype::Cu8 => out.extend(chunks.map(|c| {
            Complex32::new(
                (f32::from(c[0]) - 128.0) / 128.0,
                (f32::from(c[1]) - 128.0) / 128.0,
            )
        })),
        Datatype::Ci16Le => out.extend(chunks.map(|c| {
            Complex32::new(
                f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0,
                f32::from(i16::from_le_bytes([c[2], c[3]])) / 32768.0,
            )
        })),
        Datatype::Cf32Le => out.extend(chunks.map(|c| {
            Complex32::new(
                f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            )
        })),
        _ => unreachable!("checked by supports()"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_each_datatype() {
        let mut out = Vec::new();
        decode_into(Datatype::Ci8, &[0x80, 0x7f], &mut out).unwrap();
        assert_eq!(out[0], Complex32::new(-1.0, 127.0 / 128.0));

        out.clear();
        decode_into(Datatype::Cu8, &[0, 128], &mut out).unwrap();
        assert_eq!(out[0], Complex32::new(-1.0, 0.0));

        out.clear();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&i16::MIN.to_le_bytes());
        bytes.extend_from_slice(&16384i16.to_le_bytes());
        decode_into(Datatype::Ci16Le, &bytes, &mut out).unwrap();
        assert_eq!(out[0], Complex32::new(-1.0, 0.5));

        out.clear();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0.25f32.to_le_bytes());
        bytes.extend_from_slice(&(-0.75f32).to_le_bytes());
        decode_into(Datatype::Cf32Le, &bytes, &mut out).unwrap();
        assert_eq!(out[0], Complex32::new(0.25, -0.75));
    }

    #[test]
    fn rejects_partial_samples_and_real_types() {
        let mut out = Vec::new();
        assert!(matches!(
            decode_into(Datatype::Ci16Le, &[0, 0, 0], &mut out),
            Err(SourceError::InvalidRecording(_))
        ));
        assert!(matches!(
            decode_into(Datatype::Ri8, &[0], &mut out),
            Err(SourceError::UnsupportedDatatype(Datatype::Ri8))
        ));
    }
}
