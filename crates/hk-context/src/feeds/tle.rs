//! CelesTrak TLE adapter (C29; T-276) and the offline-first pass-planning entry point.
//!
//! # Source (unverified access details; see the C29 card)
//!
//! CelesTrak serves element sets per group as 3-line text (`gp.php?GROUP=<group>&FORMAT=tle`,
//! e.g. `weather`, `noaa`, `amateur`, `cubesat`). A snapshot is stored immutably like any feed
//! snapshot, keyed by group; this crate never fetches it itself ([`super::FeedFetcher`]), so a
//! snapshot can equally be imported from removable media.
//!
//! # A reference snapshot, not events
//!
//! An element set is reference data (C29 `ReferenceSnapshot`), not an event: ingest validates the
//! whole set (a malformed or checksum-failing line fails loudly and caches nothing) and produces no
//! ExternalEvents. Passes are computed from the cached set on demand, for any window, offline —
//! see [`plan_from_cache`] — and become `tle-pass` events only through
//! [`crate::passes::pass_event`].
//!
//! # Staleness has two layers, both reported
//!
//! - **Feed:** when the snapshot was fetched and whether the last refresh failed
//!   ([`TleFeedStatus`], from [`super::FeedState`]).
//! - **Element set:** each pass's TLE age at AOS, its freshness verdict and margin
//!   ([`crate::passes::plan`]). This is what actually bounds prediction error: a snapshot fetched
//!   yesterday can hold a month-old set for a satellite no longer tracked.

use hk_model::{TimeRange, Timestamp};

use super::{FeedAdapter, FeedCache, FeedError, ParseError, Parsed};
use crate::geo::Site;
use crate::passes::{
    Downlink, PassPlan, PassPlanConfig, PredictConfig, Sgp4Error, Tle, parse_set, plan_passes,
    predict_all,
};

/// Feed id.
pub const SOURCE: &str = "celestrak-tle";
/// Parser version.
pub const PARSER_VERSION: &str = "tle-3line@1";

/// The TLE feed adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct TleAdapter;

impl FeedAdapter for TleAdapter {
    fn source(&self) -> &str {
        SOURCE
    }

    fn parser_version(&self) -> &str {
        PARSER_VERSION
    }

    fn file_name(&self, key: &str) -> String {
        format!("{key}.tle")
    }

    fn parse(&self, _key: &str, body: &str, fetched_at: Timestamp) -> Result<Parsed, ParseError> {
        parse_set(body)?;
        Ok(Parsed {
            events: Vec::new(),
            coverage: TimeRange::new(fetched_at, fetched_at),
            skipped: 0,
        })
    }
}

/// The cached TLE feed as a consumer must see it.
#[derive(Clone, Debug, PartialEq)]
pub struct TleFeedStatus {
    /// Snapshot key (CelesTrak group).
    pub key: String,
    /// When the cached snapshot was fetched or imported.
    pub fetched_at: Timestamp,
    /// Its age at the planning time, s.
    pub cache_age_s: f64,
    /// The last refresh attempt failed (the cache may be behind the feed).
    pub refresh_failed: bool,
    /// The last error, if it failed.
    pub last_error: Option<String>,
    /// Oldest and newest element epochs in the snapshot.
    pub epochs: TimeRange,
}

/// A cached element set plus its feed status.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedTles {
    /// The element sets.
    pub tles: Vec<Tle>,
    /// Feed status.
    pub status: TleFeedStatus,
}

/// Loads the latest cached snapshot for `key`, or `None` if nothing was ever cached (the caller
/// must report "no element sets", never an empty sky).
pub fn load(cache: &FeedCache, key: &str, now: Timestamp) -> Result<Option<CachedTles>, FeedError> {
    let Some(state) = cache.state(SOURCE)? else {
        return Ok(None);
    };
    let (Some(body), Some(record)) = (cache.snapshot(SOURCE, key)?, state.snapshots.get(key))
    else {
        return Ok(None);
    };
    let tles = parse_set(&body).map_err(|error| FeedError::Parse {
        source_id: SOURCE.to_owned(),
        key: key.to_owned(),
        error,
    })?;
    let oldest = tles.iter().map(|t| t.epoch).min().expect("non-empty set");
    let newest = tles.iter().map(|t| t.epoch).max().expect("non-empty set");
    Ok(Some(CachedTles {
        status: TleFeedStatus {
            key: key.to_owned(),
            fetched_at: record.fetched_at,
            cache_age_s: (now.as_unix_nanos() - record.fetched_at.as_unix_nanos()) as f64 / 1e9,
            refresh_failed: state.stale,
            last_error: state.last_error.clone(),
            epochs: TimeRange::new(oldest, newest),
        },
        tles,
    }))
}

/// Passes planned from the cache, with everything a consumer needs to judge them.
#[derive(Clone, Debug, PartialEq)]
pub struct PassSchedule {
    /// Feed status (cache age, refresh failure).
    pub feed: TleFeedStatus,
    /// Passes, verdicts and reservations.
    pub plan: PassPlan,
    /// Element sets SGP4 refused, with the reason.
    pub refused: Vec<(u32, Sgp4Error)>,
}

/// Where and when to plan.
#[derive(Clone, Debug, PartialEq)]
pub struct PassRequest {
    /// The device site.
    pub site: Site,
    /// Plan from.
    pub now: Timestamp,
    /// Plan horizon, s.
    pub horizon_s: f64,
    /// Satellites to receive.
    pub downlinks: Vec<Downlink>,
    /// Prediction settings.
    pub predict: PredictConfig,
    /// Planning settings.
    pub plan: PassPlanConfig,
}

/// Offline-first pass planning: predicts from whatever element sets are cached for `key` (however
/// old, reporting the age) and plans the reservations. `None` when nothing is cached.
pub fn plan_from_cache(
    cache: &FeedCache,
    key: &str,
    req: &PassRequest,
) -> Result<Option<PassSchedule>, FeedError> {
    let Some(cached) = load(cache, key, req.now)? else {
        return Ok(None);
    };
    let wanted: Vec<Tle> = cached
        .tles
        .into_iter()
        .filter(|t| req.downlinks.iter().any(|d| d.norad_id == t.norad_id))
        .collect();
    let to = req.now.saturating_add_nanos((req.horizon_s * 1e9) as i64);
    let (passes, refused) = predict_all(&wanted, req.site, req.now, to, &req.predict);
    Ok(Some(PassSchedule {
        feed: cached.status,
        plan: plan_passes(&passes, &req.downlinks, req.now, &req.plan),
        refused,
    }))
}
