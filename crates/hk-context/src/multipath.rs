//! Content-correlated multipath, wired to the record (T-222, AWARE-053, C40 content half).
//!
//! [`hk_model::multipath`] holds the physics and the guards; `hk_dsp::xcorr` holds the
//! correlation. This module is the part that reads the repository: it builds each row's **content
//! series** from the measurements already stored, correlates the pairs worth correlating, and
//! records what it finds as an append-only, revocable [`RelationKind::MultipathOf`] claim.
//!
//! # The content series, and why it comes from the detection record
//!
//! The series this rule correlates is each row's **measured energy against time in its own band**
//! — the keying pattern, which *is* content for anything bursty, on/off or amplitude-bearing, and
//! which the detection record already holds for every row without demodulating anything. Two
//! copies of one transmission have the same keying pattern, delayed; two independent emitters of
//! the same family and bandwidth do not. Audio, bits and decoded frames are the finer series the
//! ticket also names ([`ContentKind`] has a variant for each); this module measures the one that
//! exists for every row today, and the rule above it is written against the abstraction rather
//! than the source.
//!
//! Each series is rasterised on **one shared time grid** — same origin, same bin — so the lag of
//! the correlation peak is the delay in seconds and nothing has to be re-registered afterwards.
//! Samples are **relative amplitude against that row's own peak**, so a copy 8 dB down has the
//! same series shape as the direct path rather than a smaller one.
//!
//! The bin is chosen from the data: a quarter of the shortest detection either row recorded,
//! clamped to [`MultipathConfig::min_bin_s`]..[`MultipathConfig::max_bin_s`]. That is the
//! measurement's own time resolution, it is reported as such on the claim, and
//! [`hk_model::multipath::MULTIPATH_MIN_LAG_RESOLUTIONS`] refuses to call anything finer a path
//! difference. Honesty about resolution is the same rule the canvas follows: never imply detail
//! the front end did not deliver.
//!
//! # What bounds the work
//!
//! Pairs are bounded ([`MultipathConfig::max_partners`], nearest in frequency first), the window
//! is bounded ([`MultipathConfig::max_window_s`]), each series is bounded
//! ([`MultipathConfig::max_samples`], by widening the bin rather than truncating the window), and
//! the lag search is bounded by [`MULTIPATH_MAX_LAG_S`]. The whole pass is `O(partners · samples ·
//! lags)` with no ring, sample-buffer or device access at all: it runs off the capture path,
//! wherever the caller resolves relationships.
//!
//! # A peak is not evidence on its own
//!
//! Two measurements beyond the correlation itself are taken here and handed to the rule, because
//! a peak value cannot distinguish an earned match from a lucky one:
//!
//! - **support** — the shared window is cut into [`SUPPORT_SEGMENTS`] parts and each is scored on
//!   its own, so a match carried by a single coincidence (two identical-model sensors that each
//!   keyed once, 50 ms apart — which correlates a perfect 1.00) is told from one that dozens of
//!   separate events agree on;
//! - **self-similarity beyond the searched lag range** — `normalized_xcorr` reports its runner-up
//!   only from inside ±`MULTIPATH_MAX_LAG_S`, so a keying cadence longer than 150 ms is invisible
//!   to the dominance test, and a series that repeats every `P` has a lag ambiguous modulo `P`.
//!   Either series repeating is enough to deny the delay.
//!
//! # Rows that do NOT compete here
//!
//! A partner whose band **overlaps** this row's is skipped outright: overlapping rows are T-219's
//! business, and two rules claiming and revoking over the same pair would fight. This rule exists
//! precisely for the pairs geometry cannot reach.

use hk_model::multipath::{
    ContentCorrelation, ContentKind, MULTIPATH_MAX_LAG_S, MULTIPATH_MIN_OVERLAP_S, MultipathRow,
    MultipathVerdict, content_multipath,
};
use hk_model::{
    Emitter, EmitterId, Identity, InventoryQuery, Region, RelationAuthor, RelationClaim,
    RelationKind, RelationVisibility, RepoError, Repository, TimeRange, Timestamp,
};

/// The rule id recorded as the `actor` of every claim this module makes.
pub const MULTIPATH_RULE: &str = "hk-context/multipath@1";

/// Share of the **wider** of a detection's band and a row's band that the two must have in common
/// before that detection counts as a measurement *of that row*.
///
/// A burst detector occasionally draws a box far wider than any emission in the scene — a
/// one-frame smear over a busy span. Such a box overlaps every row's band, and without this test
/// it would enter every row's content series and, worse, set every row's peak level, so two rows
/// 4 dB apart would measure as equal and the echo could never be told from the direct path. A
/// detection sixty times the width of the row it is attributed to is not a measurement of it.
pub const DETECTION_BAND_MIN_FRACTION: f64 = 0.5;

/// Independent parts the shared window is cut into to ask how many of them separately show the
/// alignment (`hk_dsp::xcorr::segment_support`, and the guard in
/// [`hk_model::multipath::MULTIPATH_MIN_SUPPORT`]).
///
/// Eight: enough that "at least three, and at least half" is a real demand on a sparse emitter,
/// and few enough that each part still holds several seconds of a minute-long window — a part
/// shorter than one keying cycle would be flat, and a flat part supports nothing whatever the
/// content does.
pub const SUPPORT_SEGMENTS: usize = 8;

/// What bounds one review. Every value is a cost bound, not a threshold on the evidence — the
/// evidence thresholds are all in [`hk_model::multipath`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MultipathConfig {
    /// Most partner rows correlated against the subject, nearest in frequency first.
    pub max_partners: usize,
    /// Longest window correlated, seconds, ending at the subject's latest sighting.
    pub max_window_s: f64,
    /// Finest raster bin, seconds.
    pub min_bin_s: f64,
    /// Coarsest raster bin, seconds.
    pub max_bin_s: f64,
    /// Most samples per series; a longer window widens the bin instead of losing time.
    pub max_samples: usize,
    /// Most detections read per series.
    pub max_detections: usize,
}

impl Default for MultipathConfig {
    fn default() -> Self {
        Self {
            max_partners: 8,
            max_window_s: 60.0,
            min_bin_s: 1e-3,
            max_bin_s: 50e-3,
            max_samples: 20_000,
            max_detections: 20_000,
        }
    }
}

/// What one review recorded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MultipathOutcome {
    /// Rows newly recorded as the delayed copy of another.
    pub related: Vec<EmitterId>,
    /// Standing claims revoked because the evidence changed.
    pub revoked: Vec<EmitterId>,
    /// Pairs the correlation actually ran on.
    pub compared: usize,
}

impl MultipathOutcome {
    /// Nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.related.is_empty() && self.revoked.is_empty()
    }
}

/// One row's content series on a shared time grid.
struct Series {
    row: MultipathRow,
    /// Relative amplitude against this row's own peak, one sample per bin.
    samples: Vec<f32>,
    /// Shortest detection that went into it, seconds (`None` when it holds none).
    shortest_s: Option<f64>,
}

/// **The review.** Correlates `id`'s content against the rows sharing its window but not its band,
/// and records the two-path findings.
///
/// Every claim is append-only and revocable, and the losing row keeps its id, detections, tracks
/// and history (ADR-0015 §11). **Unlike T-598's retune siblings this does not skip a Confirmed
/// row:** that rule withholds a claim from a confirmed entry because an LO-relative slope is a
/// statement about the *receiver* that must not silently retract a person's decision, while this
/// one is a direct measurement that the two rows carry the same content, with its delay,
/// correlation and attenuation disclosed on the row and revocable the moment the content stops
/// matching.
pub fn review(
    repo: &mut Repository,
    id: EmitterId,
    actor: &str,
    t: Timestamp,
    cfg: &MultipathConfig,
) -> Result<MultipathOutcome, RepoError> {
    let mut out = MultipathOutcome::default();
    let id = repo.live_emitter_id(id)?;
    let Some(subject) = live_row(repo, id)? else {
        return Ok(out);
    };
    let window = window_of(&subject.0, cfg);
    if window.duration_ns() <= 0 {
        return Ok(out);
    }
    let partners = partners(repo, &subject, window, cfg)?;
    if partners.is_empty() {
        return Ok(out);
    }
    // One grid for every series in this review, so a lag is a delay and never a re-registration.
    let (bin_s, n) = grid(
        window,
        bin_for(repo, &subject, &partners, window, cfg)?,
        cfg,
    );
    if n < 2 {
        return Ok(out);
    }
    let Some(a) = series(repo, &subject, window, bin_s, n, cfg)? else {
        return Ok(out);
    };
    let max_lag = (MULTIPATH_MAX_LAG_S / bin_s).ceil() as usize;
    let min_overlap = ((MULTIPATH_MIN_OVERLAP_S / bin_s).ceil() as usize).max(2);
    // Every lag the pair search does NOT cover, probed once for the subject. A repeat out here is
    // what makes a lag ambiguous by whole periods, and the pair search is structurally blind to it.
    let a_repeat = hk_dsp::xcorr::self_similarity(&a.samples, max_lag + 1);
    for partner in &partners {
        let Some(b) = series(repo, partner, window, bin_s, n, cfg)? else {
            continue;
        };
        let Some(x) = hk_dsp::normalized_xcorr(&a.samples, &b.samples, max_lag, min_overlap) else {
            continue;
        };
        out.compared += 1;
        let b_repeat = hk_dsp::xcorr::self_similarity(&b.samples, max_lag + 1);
        // The worse of the two: either series repeating is enough to deny the delay.
        let repeat = [a_repeat, b_repeat]
            .into_iter()
            .flatten()
            .max_by(|p, q| p.value.total_cmp(&q.value));
        let corr = ContentCorrelation {
            kind: ContentKind::Envelope,
            // `xcorr` reports `a[i + lag] ~ b[i]`, so a positive lag means b's content sits
            // *earlier* on the grid — b arrives first. The rule's sign convention is the other
            // way round ("positive when b arrives after a"), so it is negated here, once.
            lag_s: -(x.peak.lag as f64) * bin_s,
            peak: x.peak.value,
            dominance: x.dominance(),
            resolution_s: bin_s,
            overlap_s: x.peak.overlap as f64 * bin_s,
            support: hk_dsp::xcorr::segment_support(
                &a.samples,
                &b.samples,
                x.peak.lag,
                SUPPORT_SEGMENTS,
                hk_model::multipath::MULTIPATH_MIN_CORRELATION,
            ),
            segments: SUPPORT_SEGMENTS,
            self_similarity: repeat.map_or(0.0, |p| p.value),
            self_similarity_lag_s: repeat.map_or(0.0, |p| p.lag as f64 * bin_s),
        };
        let verdict = content_multipath(&a.row, &b.row, &corr);
        record(repo, &a.row, &b.row, &verdict, actor, t, &mut out)?;
    }
    Ok(out)
}

/// The live emitter and its measured band, or `None` when it is merged away or deleted.
fn live_row(repo: &Repository, id: EmitterId) -> Result<Option<(Emitter, bool)>, RepoError> {
    use hk_model::LifecycleState;
    let state = repo.emitter_lifecycle_state(id)?;
    if state == LifecycleState::Deleted {
        return Ok(None);
    }
    let e = repo.emitter(id)?;
    if !(e.f_center_hz.is_finite() && e.bandwidth_hz.is_finite() && e.bandwidth_hz > 0.0) {
        return Ok(None);
    }
    Ok(Some((e, state == LifecycleState::Confirmed)))
}

/// The window correlated: the subject's latest [`MultipathConfig::max_window_s`] of presence.
fn window_of(e: &Emitter, cfg: &MultipathConfig) -> TimeRange {
    let end = e.last_seen;
    let span_ns = (cfg.max_window_s * 1e9) as i64;
    let start = Timestamp::from_unix_nanos(
        e.first_seen
            .as_unix_nanos()
            .max(end.as_unix_nanos().saturating_sub(span_ns)),
    );
    TimeRange::new(start, end)
}

/// Rows sharing the window but **not** the band, nearest in frequency first and capped.
fn partners(
    repo: &Repository,
    subject: &(Emitter, bool),
    window: TimeRange,
    cfg: &MultipathConfig,
) -> Result<Vec<(Emitter, bool)>, RepoError> {
    let band = subject.0.freq();
    let page = repo.query_inventory(&InventoryQuery {
        time: Some(window),
        // Rows that already defer for another reason are still real measurements and may well be
        // the other end of a two-path pair, so the search sees them; what it does *not* do is
        // claim over an overlapping band, which is the rule below.
        relations: RelationVisibility::All,
        limit: hk_model::cluster::MAX_INVENTORY_PAGE,
        ..InventoryQuery::default()
    })?;
    let mut rows: Vec<(Emitter, bool)> = page
        .entries
        .into_iter()
        .filter(|e| {
            e.emitter.id != subject.0.id
                && e.emitter.bandwidth_hz.is_finite()
                && e.emitter.bandwidth_hz > 0.0
                && e.emitter.f_center_hz.is_finite()
                // Overlapping bands are T-219's business; two rules over one pair would fight.
                && !e.emitter.freq().overlaps(&band)
        })
        .map(|e| {
            let confirmed = e.lifecycle == hk_model::LifecycleState::Confirmed;
            (e.emitter, confirmed)
        })
        .collect();
    let f0 = subject.0.f_center_hz;
    rows.sort_by(|x, y| {
        (x.0.f_center_hz - f0)
            .abs()
            .total_cmp(&(y.0.f_center_hz - f0).abs())
    });
    rows.truncate(cfg.max_partners);
    Ok(rows)
}

/// Detections **of one row** inside the window: those whose own occupied band is the row's, to
/// within [`DETECTION_BAND_MIN_FRACTION`]. See that constant for why the width test is not
/// optional.
fn detections(
    repo: &Repository,
    e: &Emitter,
    window: TimeRange,
    cfg: &MultipathConfig,
) -> Result<Vec<hk_model::Detection>, RepoError> {
    let band = e.freq();
    let mut d = repo.detections_in_region(&Region::new(band, window))?;
    d.retain(|x| {
        hk_model::relate::overlap_fraction_wider(x.freq(), band) >= DETECTION_BAND_MIN_FRACTION
    });
    d.truncate(cfg.max_detections);
    Ok(d)
}

/// The shared raster bin: a quarter of the shortest detection any of these rows recorded, clamped
/// to the configured range. That is the finest delay the detection record can distinguish.
fn bin_for(
    repo: &Repository,
    subject: &(Emitter, bool),
    partners: &[(Emitter, bool)],
    window: TimeRange,
    cfg: &MultipathConfig,
) -> Result<f64, RepoError> {
    let mut shortest = f64::INFINITY;
    for (e, _) in std::iter::once(subject).chain(partners.iter()) {
        for d in detections(repo, e, window, cfg)? {
            let s = d.time.duration_ns() as f64 / 1e9;
            if s.is_finite() && s > 0.0 {
                shortest = shortest.min(s);
            }
        }
    }
    let bin = if shortest.is_finite() {
        shortest / 4.0
    } else {
        cfg.max_bin_s
    };
    Ok(bin.clamp(cfg.min_bin_s, cfg.max_bin_s))
}

/// The grid: bin and sample count, **widening the bin rather than losing window** when the sample
/// cap bites.
///
/// Capping the count with the bin fixed would silently correlate only the window's OLDEST
/// `max_samples` bins — a 60 s window at a 1 ms bin would forever re-compare its first 20 s and
/// never see the content that just arrived, since the grid starts at `window.start`. Coarsening
/// the bin keeps the whole window in view and costs only resolution, which is disclosed on the
/// claim and which [`hk_model::multipath::MULTIPATH_MIN_LAG_RESOLUTIONS`] then holds the delay to.
fn grid(window: TimeRange, bin_s: f64, cfg: &MultipathConfig) -> (f64, usize) {
    let span_s = window.duration_ns() as f64 / 1e9;
    let cap = cfg.max_samples.max(2);
    let bin_s = bin_s.max(span_s / cap as f64);
    let n = ((span_s / bin_s).ceil() as usize).clamp(0, cap);
    (bin_s, n)
}

/// Rasterises one row's measured energy onto the shared grid (see the module docs). `None` when no
/// detection of that row falls in the window, which is an abstention: nothing to compare.
fn series(
    repo: &Repository,
    row: &(Emitter, bool),
    window: TimeRange,
    bin_s: f64,
    n: usize,
    cfg: &MultipathConfig,
) -> Result<Option<Series>, RepoError> {
    let (e, confirmed) = row;
    let dets = detections(repo, e, window, cfg)?;
    if dets.is_empty() {
        return Ok(None);
    }
    let t0 = window.start.as_unix_nanos();
    let bin_ns = (bin_s * 1e9).max(1.0);
    let peak_dbfs = dets
        .iter()
        .map(|d| f64::from(d.peak_level_dbfs))
        .filter(|v| v.is_finite())
        .fold(f64::NEG_INFINITY, f64::max);
    if !peak_dbfs.is_finite() {
        return Ok(None);
    }
    // Relative amplitude against this row's own peak: an attenuated copy has the same series as
    // the direct path, which is what makes the two comparable at all.
    let mut samples = vec![0f32; n];
    let mut shortest: Option<f64> = None;
    for d in &dets {
        let level = f64::from(d.peak_level_dbfs);
        if !level.is_finite() {
            continue;
        }
        let a = (10f64.powf((level - peak_dbfs) / 20.0)) as f32;
        let lo = ((d.time.start.as_unix_nanos() - t0) as f64 / bin_ns).floor();
        let hi = ((d.time.end.as_unix_nanos() - t0) as f64 / bin_ns).ceil();
        let dur = d.time.duration_ns() as f64 / 1e9;
        if dur.is_finite() && dur > 0.0 {
            shortest = Some(shortest.map_or(dur, |s: f64| s.min(dur)));
        }
        let lo = lo.max(0.0) as usize;
        let hi = (hi.max(0.0) as usize).min(n);
        for s in samples.iter_mut().take(hi).skip(lo) {
            *s = s.max(a);
        }
    }
    Ok(Some(Series {
        row: MultipathRow {
            emitter_id: e.id,
            f_center_hz: e.f_center_hz,
            bandwidth_hz: e.bandwidth_hz,
            level_dbfs: Some(peak_dbfs),
            identity: match &e.identity {
                Identity::Decoded(d) => Some(d.clone()),
                Identity::Unknown => None,
            },
            confirmed: *confirmed,
        },
        samples,
        shortest_s: shortest,
    }))
}

/// Records one verdict: a claim, a revocation, or (for an abstention) nothing at all.
fn record(
    repo: &mut Repository,
    a: &MultipathRow,
    b: &MultipathRow,
    verdict: &MultipathVerdict,
    actor: &str,
    t: Timestamp,
    out: &mut MultipathOutcome,
) -> Result<(), RepoError> {
    match verdict {
        MultipathVerdict::Related(f) => {
            let standing = repo.emitter_relations(f.echo)?;
            if standing
                .iter()
                .any(|r| r.kind == RelationKind::MultipathOf && r.source_id == f.direct)
            {
                return Ok(());
            }
            // A row is the echo of one direct path: supersede any other standing multipath claim.
            for r in standing
                .iter()
                .filter(|r| r.kind == RelationKind::MultipathOf && r.source_id != f.direct)
            {
                repo.record_emitter_relation(&RelationClaim {
                    emitter_id: f.echo,
                    source_id: r.source_id,
                    kind: RelationKind::MultipathOf,
                    artifact: None,
                    active: false,
                    t,
                    author: RelationAuthor::System,
                    actor: actor.to_owned(),
                    reason: format!(
                        "superseded: this row now measures as the delayed copy of emitter {} \
                         instead",
                        f.direct
                    ),
                    score: None,
                    detail: None,
                })?;
                out.revoked.push(f.echo);
            }
            repo.record_emitter_relation(&RelationClaim {
                emitter_id: f.echo,
                source_id: f.direct,
                kind: RelationKind::MultipathOf,
                artifact: None,
                active: true,
                t,
                author: RelationAuthor::System,
                actor: actor.to_owned(),
                reason: f.reason(),
                score: Some(f.peak),
                detail: Some(f.detail()),
            })?;
            out.related.push(f.echo);
        }
        MultipathVerdict::Independent(why) => {
            // Positively two emissions: a standing claim between this pair no longer holds.
            for (echo, direct) in [(a, b), (b, a)] {
                let standing = repo.emitter_relations(echo.emitter_id)?;
                for r in standing.iter().filter(|r| {
                    r.kind == RelationKind::MultipathOf && r.source_id == direct.emitter_id
                }) {
                    repo.record_emitter_relation(&RelationClaim {
                        emitter_id: echo.emitter_id,
                        source_id: r.source_id,
                        kind: RelationKind::MultipathOf,
                        artifact: None,
                        active: false,
                        t,
                        author: RelationAuthor::System,
                        actor: actor.to_owned(),
                        reason: format!(
                            "revoked: the two rows no longer measure as one emission over two \
                             paths ({why})"
                        ),
                        score: None,
                        detail: None,
                    })?;
                    out.revoked.push(echo.emitter_id);
                }
            }
        }
        // An abstention is not a finding: it neither claims nor revokes.
        MultipathVerdict::Undecidable(_) => {}
    }
    Ok(())
}

impl Series {
    /// The shortest detection behind this series, seconds — the measurement's own time grain.
    /// Exposed for callers that report what a comparison was resolved at.
    #[allow(dead_code)]
    fn grain_s(&self) -> Option<f64> {
        self.shortest_s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_cap_widens_the_bin_rather_than_truncating_the_window() {
        let cfg = MultipathConfig::default();
        let w = TimeRange::new(
            Timestamp::from_unix_nanos(0),
            Timestamp::from_unix_nanos(60_000_000_000),
        );
        // 60 s at a 1 ms bin would be 60 000 samples. The cap must not answer "the first 20 000",
        // which would correlate the window's oldest 20 s forever: it coarsens the bin instead, and
        // the whole 60 s stays on the grid.
        let (bin, n) = grid(w, 1e-3, &cfg);
        assert_eq!(n, cfg.max_samples);
        assert!((bin - 60.0 / cfg.max_samples as f64).abs() < 1e-12, "{bin}");
        assert!(
            (n as f64 * bin - 60.0).abs() < 1e-6,
            "the grid still spans the window"
        );
        // Below the cap the measured bin is kept exactly.
        assert_eq!(grid(w, 50e-3, &cfg), (50e-3, 1200));
    }

    #[test]
    fn the_window_is_the_latest_slice_of_the_rows_presence() {
        let cfg = MultipathConfig::default();
        let mut e = hk_model::Emitter {
            id: EmitterId::from_uuid(uuid::Uuid::from_u128(1)),
            f_center_hz: 100e6,
            bandwidth_hz: 20e3,
            first_seen: Timestamp::from_unix_nanos(0),
            last_seen: Timestamp::from_unix_nanos(500_000_000_000),
            count: 1,
            fingerprint: serde_json::Value::Null,
            identity: Identity::Unknown,
            known_status: hk_model::KnownStatus::Unknown,
            classifications: Vec::new(),
            tags: Default::default(),
        };
        let w = window_of(&e, &cfg);
        assert_eq!(w.end, e.last_seen);
        assert_eq!(w.duration_ns(), (cfg.max_window_s * 1e9) as i64);
        // A young row is never widened past its own first sighting.
        e.first_seen = Timestamp::from_unix_nanos(499_000_000_000);
        assert_eq!(window_of(&e, &cfg).start, e.first_seen);
    }
}
