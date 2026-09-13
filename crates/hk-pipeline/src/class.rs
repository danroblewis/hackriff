//! Content classes for the pipeline's outputs (ADR-0004 gating; legal guardrail).
//!
//! - **Source class** ([`source_class`]): a recording's `hackriff:content_class` when present
//!   (parsed failing closed); otherwise `unrestricted` only when every capture lies inside one
//!   positively chosen band prior ([`UNRESTRICTED_BANDS_HZ`]); otherwise the fail-closed
//!   `metadata-only`. It sets the spectrum stream class, whether chains that write content
//!   (analog audio/RDS, SigMF recordings) may attach, and the plugin input ceiling.
//! - **Emitter classification** ([`classify_emitter`]): a user rule (`ScanPlan.extra.pipeline.
//!   classify`) that vouches for an emitter's content (e.g. "my own 433 MHz sensor"). A rule can
//!   open content in a fail-closed (`metadata-only`) band, but never under a restricted source
//!   class: `restricted-*` sources clamp every rule.
//! - **Spectrum rows** ([`row_plan`], [`spectrum_header`]): moved here from `hk serve`; under a
//!   gated class the declared row rate is the actual rate + 10 %, capped at the contract's 50
//!   rows/s.

use hk_model::ContentClass;
use hk_model::sigmf::SigmfMeta;
use hk_stream::{GATED_SPECTRUM_MAX_ROW_RATE_HZ, StreamHeader, StreamKind};
use serde::{Deserialize, Serialize};

use hk_dsp::{StftConfig, WelchConfig};

/// Datatype of spectrum rows: `fft_size` little-endian f32 PSD values, dBFS/Hz, ascending
/// frequency.
pub const SPECTRUM_DATATYPE: &str = "rf32_le";

/// Bands whose content is positively chosen as `unrestricted` (a band prior, not a guess):
/// FM broadcast, and the 1090 MHz ADS-B / Mode S channel (unencrypted aircraft broadcasts; the
/// readsb manifest's own output class).
pub const UNRESTRICTED_BANDS_HZ: [(f64, f64, &str); 2] = [
    (87.5e6, 108.0e6, "fm-broadcast"),
    (1087.0e6, 1093.0e6, "adsb-1090"),
];

/// FM broadcast band, Hz (kept for `hk serve`).
pub const FM_BROADCAST_HZ: (f64, f64) = (UNRESTRICTED_BANDS_HZ[0].0, UNRESTRICTED_BANDS_HZ[0].1);

/// The class for a window `[centre ± fs/2]` set: `unrestricted` only when every window lies in
/// one [`UNRESTRICTED_BANDS_HZ`] band, else fail closed.
pub fn band_class(centres: &[f64], fs: f64) -> ContentClass {
    let inside = |lo: f64, hi: f64| {
        !centres.is_empty()
            && centres
                .iter()
                .all(|fc| fc - fs / 2.0 >= lo && fc + fs / 2.0 <= hi)
    };
    if UNRESTRICTED_BANDS_HZ
        .iter()
        .any(|&(lo, hi, _)| inside(lo, hi))
    {
        ContentClass::Unrestricted
    } else {
        ContentClass::FAIL_CLOSED
    }
}

/// The source class of a recording (see the module docs).
pub fn source_class(meta: &SigmfMeta) -> ContentClass {
    if let Some(v) = meta.global.extra.get("hackriff:content_class") {
        return ContentClass::parse_fail_closed(v.as_str());
    }
    let fs = meta.global.sample_rate.unwrap_or(f64::INFINITY);
    let centres: Vec<f64> = meta
        .captures
        .iter()
        .filter_map(|c| c.frequency)
        .chain(meta.global.provenance.as_ref().map(|p| p.tune.center_hz))
        .collect();
    band_class(&centres, fs)
}

/// The source class is restricted: it clamps every classification.
pub fn is_restricted(class: ContentClass) -> bool {
    matches!(
        class,
        ContentClass::RestrictedCellular | ContentClass::RestrictedPaging
    )
}

/// A user classification rule: emitters whose occupied extent lies inside `freq_hz` carry
/// `content_class`, vouched for by `by`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassRule {
    /// `[lo, hi]`, Hz.
    pub freq_hz: [f64; 2],
    /// Class the user vouches for.
    pub content_class: ContentClass,
    /// Who and why, e.g. `user: own 433 MHz sensor`.
    pub by: String,
}

/// The classification for an emitter at `[f_lo, f_hi]` under `source`, or `None` (fail closed).
pub fn classify_emitter(
    rules: &[ClassRule],
    source: ContentClass,
    f_lo: f64,
    f_hi: f64,
) -> Option<(ContentClass, String)> {
    let rule = rules
        .iter()
        .find(|r| f_lo >= r.freq_hz[0] && f_hi <= r.freq_hz[1]);
    match (rule, is_restricted(source)) {
        (_, true) => Some((source, format!("source class {}", class_name(source)))),
        (Some(r), false) => Some((r.content_class, r.by.clone())),
        (None, false) if source == ContentClass::Unrestricted => {
            Some((source, "band prior".to_owned()))
        }
        (None, false) => None,
    }
}

/// The kebab-case class name.
pub fn class_name(class: ContentClass) -> String {
    serde_json::to_value(class)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "metadata-only".into())
}

/// STFT settings and row rates for a spectrum stream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowPlan {
    /// STFT settings.
    pub stft: StftConfig,
    /// Actual row rate, rows/s.
    pub row_rate_hz: f64,
    /// Declared row rate (`sample_rate_hz` of the header).
    pub declared_hz: f64,
}

/// Chooses `K` so rows come at most at `rows_per_s` (and within the gated cap when `class`
/// forbids content).
pub fn row_plan(fs: f64, fft_len: usize, rows_per_s: f64, class: ContentClass) -> RowPlan {
    let mut welch = WelchConfig::new(fft_len);
    welch.spectral_kurtosis = false;
    let hop = welch.hop() as f64;
    let gated = !class.permits_content();
    let target = if gated {
        rows_per_s.min(GATED_SPECTRUM_MAX_ROW_RATE_HZ / 1.1)
    } else {
        rows_per_s
    };
    let k = ((fs / (hop * target)).ceil() as usize).max(1);
    let row_rate_hz = fs / (k as f64 * hop);
    let declared_hz = if gated {
        (row_rate_hz * 1.1).min(GATED_SPECTRUM_MAX_ROW_RATE_HZ)
    } else {
        row_rate_hz
    };
    RowPlan {
        stft: StftConfig::new(welch, k),
        row_rate_hz,
        declared_hz,
    }
}

/// A spectrum stream header.
pub fn spectrum_header(
    stream_id: &str,
    source: &str,
    class: ContentClass,
    plan: &RowPlan,
    center_hz: f64,
    fs: f64,
) -> StreamHeader {
    let bins = plan.stft.welch.fft_len;
    let mut h = StreamHeader::new(stream_id, StreamKind::Spectrum, class, source);
    h.datatype = Some(SPECTRUM_DATATYPE.into());
    h.fft_size = Some(bins as u32);
    h.sample_rate_hz = Some(plan.declared_hz);
    h.center_hz = Some(center_hz);
    h.bandwidth_hz = Some(fs);
    h.max_frame_len = (hk_stream::BINARY_RECORD_HEADER_LEN + 4 * bins) as u32;
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_priors_and_classification_fail_closed() {
        assert_eq!(band_class(&[100.8e6], 2.4e6), ContentClass::Unrestricted);
        assert_eq!(band_class(&[1090e6], 2.4e6), ContentClass::Unrestricted);
        assert_eq!(band_class(&[433.92e6], 0.5e6), ContentClass::MetadataOnly);
        assert_eq!(band_class(&[], 1e6), ContentClass::MetadataOnly);
        let rules = vec![ClassRule {
            freq_hz: [433.0e6, 435.0e6],
            content_class: ContentClass::Unrestricted,
            by: "user: own sensor".into(),
        }];
        let own = classify_emitter(&rules, ContentClass::MetadataOnly, 433.9e6, 433.99e6);
        assert_eq!(own.map(|c| c.0), Some(ContentClass::Unrestricted));
        assert!(classify_emitter(&rules, ContentClass::MetadataOnly, 440e6, 441e6).is_none());
        let restricted =
            classify_emitter(&rules, ContentClass::RestrictedPaging, 433.9e6, 433.99e6).unwrap();
        assert_eq!(restricted.0, ContentClass::RestrictedPaging);
        assert!(!restricted.0.permits_content());
    }
}
