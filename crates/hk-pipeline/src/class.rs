//! Content classes for the pipeline's outputs (ADR-0004 gating; legal guardrail).
//!
//! - **Source class** ([`source_class`]): a recording's `hackriff:content_class` when present
//!   (parsed failing closed). Without one it is derived from frequency ([`band_class`]):
//!   `restricted-paging` / `restricted-cellular` when any capture window overlaps a
//!   [`restricted_band`] (47 CFR Parts 22, 24, 27 and 90; [`RESTRICTED_BANDS_HZ`] plus
//!   hk-context's cellular rows); `unrestricted` only when every capture lies inside one
//!   positively chosen band prior ([`UNRESTRICTED_BANDS_HZ`]); otherwise the fail-closed
//!   `metadata-only`. It sets the spectrum stream class, whether chains that write content
//!   (analog audio/RDS, SigMF recordings) may attach, and the plugin input ceiling.
//! - **Emitter classification** ([`classify_emitter`]): a user rule (`ScanPlan.extra.pipeline.
//!   classify`) that vouches for an emitter's content (e.g. "my own 433 MHz sensor"). A rule can
//!   open content in a fail-closed (`metadata-only`) band, but never under a restricted source
//!   class, and never for an emitter overlapping a restricted band (whatever the source class):
//!   restricted classes clamp every rule.
//! - **Spectrum rows** ([`row_plan`], [`spectrum_header`]): moved here from `hk serve`; under a
//!   gated class the declared row rate is the actual rate + 10 %, capped at the contract's 50
//!   rows/s.

use hk_context::{BandTable, Region};
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

const PAGING: ContentClass = ContentClass::RestrictedPaging;
const CELLULAR: ContentClass = ContentClass::RestrictedCellular;

/// Bands whose content is restricted in the US even unencrypted (docs/04 §1.3; CLAUDE.md legal
/// guardrails), `(lo, hi, class, source)`, listed here in addition to the rows tagged `cellular`
/// in hk-context's bundled 47 CFR 2.106 extract (T-019; see [`restricted_bands`]).
///
/// Edges were checked on 2026-09-13 against eCFR Title 47 via the Cornell LII mirror
/// (`https://www.law.cornell.edu/cfr/text/47/<section>`). Paging channel lists are widened to
/// one band spanning every listed centre ± 10 kHz, which errs towards restricting.
///
/// - **Paging:** § 22.531 (Paging and Radiotelephone Service paging channels: low VHF 35.20–35.66
///   and 43.20–43.66 MHz, high VHF 152.24 / 152.84 / 158.10 / 158.70 MHz, UHF 931.0125–931.9875
///   MHz); § 90.494 (929–930 MHz paging); § 24.129 (narrowband PCS paging, 930–931 and 940–941
///   MHz).
/// - **Cellular:** § 22.905 (cellular blocks A/B, 824–849 / 869–894 MHz); § 24.229 (broadband PCS
///   blocks A–F and G, 1850–1915 / 1930–1995 MHz); § 27.5 (a) WCS, (b) upper 700 MHz, (c) lower
///   700 MHz, (h) AWS-1/AWS-3 above 1710 MHz, (i) BRS/EBS 2496–2690 MHz, (j) AWS-4, (k) H block,
///   (l) 600 MHz, (m) 3.7 GHz, (n) 900 MHz broadband, (o) 3.45 GHz.
/// - **Deliberately not listed:** 1695–1710 MHz (AWS-3 uplink shared with the meteorological-
///   satellite downlinks the SPACE use cases receive); the 805–806 MHz guard band.
/// - **Not yet listed (unverified this session):** 800 MHz ESMR (817–824 / 862–869 MHz, Part 90
///   subpart S), FirstNet broadband (758–768 / 788–798 MHz), CBRS (3550–3700 MHz, Part 96). They
///   stay fail-closed `metadata-only` like any unlisted band, but a user rule could open them.
pub const RESTRICTED_BANDS_HZ: &[(f64, f64, ContentClass, &str)] = &[
    (35.19e6, 35.67e6, PAGING, "47 CFR 22.531 (low VHF paging)"),
    (43.19e6, 43.67e6, PAGING, "47 CFR 22.531 (low VHF paging)"),
    (
        152.23e6,
        152.25e6,
        PAGING,
        "47 CFR 22.531 (high VHF paging)",
    ),
    (
        152.83e6,
        152.85e6,
        PAGING,
        "47 CFR 22.531 (high VHF paging)",
    ),
    (
        158.09e6,
        158.11e6,
        PAGING,
        "47 CFR 22.531 (high VHF paging)",
    ),
    (
        158.69e6,
        158.71e6,
        PAGING,
        "47 CFR 22.531 (high VHF paging)",
    ),
    (
        929.0e6,
        930.0e6,
        PAGING,
        "47 CFR 90.494 (929-930 MHz paging)",
    ),
    (930.0e6, 931.0e6, PAGING, "47 CFR 24.129 (narrowband PCS)"),
    (931.0e6, 932.0e6, PAGING, "47 CFR 22.531 (UHF paging)"),
    (940.0e6, 941.0e6, PAGING, "47 CFR 24.129 (narrowband PCS)"),
    (617.0e6, 652.0e6, CELLULAR, "47 CFR 27.5(l) (600 MHz)"),
    (663.0e6, 698.0e6, CELLULAR, "47 CFR 27.5(l) (600 MHz)"),
    (698.0e6, 746.0e6, CELLULAR, "47 CFR 27.5(c) (lower 700 MHz)"),
    (746.0e6, 758.0e6, CELLULAR, "47 CFR 27.5(b) (upper 700 MHz)"),
    (775.0e6, 788.0e6, CELLULAR, "47 CFR 27.5(b) (upper 700 MHz)"),
    (824.0e6, 849.0e6, CELLULAR, "47 CFR 22.905 (cellular)"),
    (869.0e6, 894.0e6, CELLULAR, "47 CFR 22.905 (cellular)"),
    (
        897.5e6,
        900.5e6,
        CELLULAR,
        "47 CFR 27.5(n) (900 MHz broadband)",
    ),
    (
        936.5e6,
        939.5e6,
        CELLULAR,
        "47 CFR 27.5(n) (900 MHz broadband)",
    ),
    (1710.0e6, 1780.0e6, CELLULAR, "47 CFR 27.5(h) (AWS-1/AWS-3)"),
    (
        1850.0e6,
        1915.0e6,
        CELLULAR,
        "47 CFR 24.229 (broadband PCS)",
    ),
    (1915.0e6, 1920.0e6, CELLULAR, "47 CFR 27.5(k) (H block)"),
    (
        1930.0e6,
        1995.0e6,
        CELLULAR,
        "47 CFR 24.229 (broadband PCS)",
    ),
    (1995.0e6, 2000.0e6, CELLULAR, "47 CFR 27.5(k) (H block)"),
    (2000.0e6, 2020.0e6, CELLULAR, "47 CFR 27.5(j) (AWS-4)"),
    (2110.0e6, 2180.0e6, CELLULAR, "47 CFR 27.5(h) (AWS-1/AWS-3)"),
    (2180.0e6, 2200.0e6, CELLULAR, "47 CFR 27.5(j) (AWS-4)"),
    (2305.0e6, 2320.0e6, CELLULAR, "47 CFR 27.5(a) (WCS)"),
    (2345.0e6, 2360.0e6, CELLULAR, "47 CFR 27.5(a) (WCS)"),
    (2496.0e6, 2690.0e6, CELLULAR, "47 CFR 27.5(i) (BRS/EBS)"),
    (3450.0e6, 3550.0e6, CELLULAR, "47 CFR 27.5(o) (3.45 GHz)"),
    (3700.0e6, 3980.0e6, CELLULAR, "47 CFR 27.5(m) (3.7 GHz)"),
];

/// A band whose content no rule, tag-less band prior or classification may open.
#[derive(Clone, Debug, PartialEq)]
pub struct RestrictedBand {
    /// Lower edge, Hz.
    pub lo_hz: f64,
    /// Upper edge, Hz.
    pub hi_hz: f64,
    /// `restricted-paging` or `restricted-cellular`.
    pub class: ContentClass,
    /// Where the band comes from (a 47 CFR section, or the hk-context allocation row).
    pub source: String,
}

/// Every restricted band: [`RESTRICTED_BANDS_HZ`] plus hk-context's rows tagged `cellular`
/// (T-019: 700 MHz, 850 MHz cellular, AWS, PCS). Paging comes first, so a window overlapping
/// both reports `restricted-paging`; either class refuses content.
pub fn restricted_bands() -> &'static [RestrictedBand] {
    static BANDS: std::sync::OnceLock<Vec<RestrictedBand>> = std::sync::OnceLock::new();
    BANDS.get_or_init(|| {
        let mut v: Vec<RestrictedBand> = RESTRICTED_BANDS_HZ
            .iter()
            .map(|&(lo_hz, hi_hz, class, source)| RestrictedBand {
                lo_hz,
                hi_hz,
                class,
                source: source.to_owned(),
            })
            .collect();
        // The bundled table is compiled in; a parse failure is a build defect its own tests
        // catch, and the local list above already covers the same cellular bands.
        if let Ok(table) = BandTable::bundled(Region::Us) {
            v.extend(
                table
                    .rows()
                    .iter()
                    .filter(|r| r.has_tag("cellular"))
                    .map(|r| RestrictedBand {
                        lo_hz: r.freq.lo_hz,
                        hi_hz: r.freq.hi_hz,
                        class: CELLULAR,
                        source: r.prior_ref(),
                    }),
            );
        }
        v.sort_by_key(|b| b.class != PAGING);
        v
    })
}

/// The first restricted band `[lo, hi]` overlaps, if any.
pub fn restricted_band(lo: f64, hi: f64) -> Option<&'static RestrictedBand> {
    restricted_bands()
        .iter()
        .find(|b| lo < b.hi_hz && hi > b.lo_hz)
}

/// The class for a window `[centre ± fs/2]` set: a restricted class when any window overlaps a
/// [`restricted_band`] (its IQ, spectrum and recordings then carry that content);
/// `unrestricted` only when every window lies in one [`UNRESTRICTED_BANDS_HZ`] band; else fail
/// closed.
pub fn band_class(centres: &[f64], fs: f64) -> ContentClass {
    if let Some(b) = centres
        .iter()
        .find_map(|fc| restricted_band(fc - fs / 2.0, fc + fs / 2.0))
    {
        return b.class;
    }
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
    if is_restricted(source) {
        return Some((source, format!("source class {}", class_name(source))));
    }
    if let Some(b) = restricted_band(f_lo, f_hi) {
        return Some((
            b.class,
            format!("band prior {} ({})", class_name(b.class), b.source),
        ));
    }
    match rule {
        Some(r) => Some((r.content_class, r.by.clone())),
        None if source == ContentClass::Unrestricted => Some((source, "band prior".to_owned())),
        None => None,
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

    #[test]
    fn restricted_bands_are_derived_from_frequency_and_clamp_rules() {
        // Untagged windows over paging and cellular allocations.
        assert_eq!(band_class(&[930e6], 0.5e6), ContentClass::RestrictedPaging);
        assert_eq!(band_class(&[931.5e6], 2e6), ContentClass::RestrictedPaging);
        assert_eq!(
            band_class(&[152.24e6], 0.2e6),
            ContentClass::RestrictedPaging
        );
        assert_eq!(
            band_class(&[880e6], 0.5e6),
            ContentClass::RestrictedCellular
        );
        assert_eq!(band_class(&[836e6], 2e6), ContentClass::RestrictedCellular);
        assert_eq!(
            band_class(&[1940e6], 10e6),
            ContentClass::RestrictedCellular
        );
        assert_eq!(band_class(&[739e6], 5e6), ContentClass::RestrictedCellular);
        // A window only partly over a restricted band is restricted (its IQ carries it).
        assert_eq!(band_class(&[920e6], 20e6), ContentClass::RestrictedPaging);
        // Unrestricted and fail-closed bands are unchanged.
        assert_eq!(band_class(&[101.3e6], 2.4e6), ContentClass::Unrestricted);
        assert_eq!(band_class(&[100.8e6], 2.4e6), ContentClass::Unrestricted);
        assert_eq!(band_class(&[915e6], 10e6), ContentClass::MetadataOnly);
        assert_eq!(band_class(&[433.62e6], 2e6), ContentClass::MetadataOnly);
        assert_eq!(band_class(&[1698e6], 2.4e6), ContentClass::MetadataOnly);
        // hk-context's cellular rows are part of the table.
        assert!(
            restricted_bands()
                .iter()
                .any(|b| b.source.starts_with("us-47cfr2106-compact:cellular"))
        );

        let open = |lo: f64, hi: f64| ClassRule {
            freq_hz: [lo, hi],
            content_class: ContentClass::Unrestricted,
            by: "test: tries to open restricted content".into(),
        };
        for (fc, class) in [
            (930.0e6, ContentClass::RestrictedPaging),
            (880.0e6, ContentClass::RestrictedCellular),
        ] {
            let rules = vec![open(fc - 1e6, fc + 1e6)];
            for source in [
                ContentClass::MetadataOnly,
                ContentClass::Unrestricted,
                band_class(&[fc], 2e6),
            ] {
                let got = classify_emitter(&rules, source, fc - 5e3, fc + 5e3).unwrap();
                assert_eq!(got.0, class, "{fc} under {source:?}");
                assert!(!got.0.permits_content());
            }
        }
        let fm = vec![open(101.2e6, 101.4e6)];
        let got = classify_emitter(&fm, ContentClass::Unrestricted, 101.25e6, 101.35e6).unwrap();
        assert_eq!(got.0, ContentClass::Unrestricted);
    }
}
