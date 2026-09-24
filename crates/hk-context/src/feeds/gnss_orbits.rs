//! GNSS orbit reference feeds for SIGNAL-032 forensics (C29 → C36; T-324): IGS precise orbits
//! (SP3) and IGS merged broadcast ephemerides (RINEX nav), cached offline-first.
//!
//! # One feed path, not two
//!
//! These are reference snapshots exactly like the CelesTrak TLE set ([`super::tle`], T-276): the
//! same [`super::FeedCache`], [`super::ingest_snapshot`] / [`super::refresh`] and
//! [`super::FeedFetcher`] seam, immutable content-addressed raw files, `state.json` bookkeeping.
//! Ingest validates the whole file (a malformed record fails loudly and caches nothing) and
//! produces no ExternalEvents; the comparison is computed from the cache on demand
//! ([`compare_from_cache`]).
//!
//! # Sources (access details unverified; see the C29 card)
//!
//! - **Precise orbits:** IGS final/rapid/ultra-rapid SP3 (e.g. CDDIS/IGN/BKG mirrors,
//!   `IGS0OPSFIN_<yyyyddd>0000_01D_15M_ORB.SP3`). Snapshot key: a name for the product and day,
//!   e.g. `igs-final-2026-09-20`; file `<key>.sp3`.
//! - **Broadcast ephemerides:** the IGS daily merged navigation file
//!   (`BRDC00IGS_R_<yyyyddd>0000_01D_MN.rnx`). Key e.g. `brdc-2026-09-20`; file `<key>.rnx`.
//!
//! This crate fetches neither (no network [`super::FeedFetcher`] exists yet); a snapshot can be
//! imported from removable media through [`super::DirectoryFetcher`].
//!
//! # Honesty
//!
//! A reference that was never cached is [`ReferenceAvailability::NotYetFetched`] — carrying the
//! last failed attempt if there was one — and every ephemeris checked against nothing reads
//! [`SvVerdict::NotYetFetched`], never agreement. A cached reference always travels with its age
//! and whether the last refresh failed ([`OrbitFeedStatus`]).

use hk_model::{TimeRange, Timestamp};

use super::{FeedAdapter, FeedCache, FeedError, ParseError, Parsed};
use crate::ephemeris::{
    ForensicsConfig, GpsEphemeris, GpsTime, Sp3, SvCheck, SvVerdict, compare, parse_nav, parse_sp3,
};

/// Precise-orbit feed id.
pub const SP3_SOURCE: &str = "igs-sp3";
/// Precise-orbit parser version.
pub const SP3_PARSER_VERSION: &str = "sp3-cd-gps@1";
/// Broadcast-ephemeris feed id.
pub const BRDC_SOURCE: &str = "igs-brdc";
/// Broadcast-ephemeris parser version.
pub const BRDC_PARSER_VERSION: &str = "rinex-nav-gps@1";

/// The SP3 precise-orbit adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sp3Adapter;

impl FeedAdapter for Sp3Adapter {
    fn source(&self) -> &str {
        SP3_SOURCE
    }

    fn parser_version(&self) -> &str {
        SP3_PARSER_VERSION
    }

    fn file_name(&self, key: &str) -> String {
        format!("{key}.sp3")
    }

    fn parse(&self, _key: &str, body: &str, _fetched_at: Timestamp) -> Result<Parsed, ParseError> {
        let sp3 = parse_sp3(body)?;
        let (a, b) = sp3.span();
        Ok(Parsed {
            events: Vec::new(),
            coverage: TimeRange::new(a.to_utc(), b.to_utc()),
            skipped: sp3.skipped_other_systems,
        })
    }
}

/// The RINEX broadcast-ephemeris adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct BrdcAdapter;

fn nav_span(ephs: &[GpsEphemeris]) -> TimeRange {
    let from = ephs
        .iter()
        .map(|e| e.fit_span().0.0)
        .fold(f64::INFINITY, f64::min);
    let to = ephs
        .iter()
        .map(|e| e.fit_span().1.0)
        .fold(f64::NEG_INFINITY, f64::max);
    TimeRange::new(GpsTime(from).to_utc(), GpsTime(to).to_utc())
}

impl FeedAdapter for BrdcAdapter {
    fn source(&self) -> &str {
        BRDC_SOURCE
    }

    fn parser_version(&self) -> &str {
        BRDC_PARSER_VERSION
    }

    fn file_name(&self, key: &str) -> String {
        format!("{key}.rnx")
    }

    fn parse(&self, _key: &str, body: &str, _fetched_at: Timestamp) -> Result<Parsed, ParseError> {
        let nav = parse_nav(body)?;
        Ok(Parsed {
            events: Vec::new(),
            coverage: nav_span(&nav.ephemerides),
            skipped: nav.skipped_other_systems,
        })
    }
}

/// A cached reference as a consumer must see it.
#[derive(Clone, Debug, PartialEq)]
pub struct OrbitFeedStatus {
    /// Feed id.
    pub source: String,
    /// Snapshot key.
    pub key: String,
    /// When the cached snapshot was fetched or imported.
    pub fetched_at: Timestamp,
    /// Its age at `now`, s.
    pub cache_age_s: f64,
    /// The last refresh attempt failed (the cache may be behind the feed).
    pub refresh_failed: bool,
    /// The last error, if it failed.
    pub last_error: Option<String>,
    /// The span (UTC) the snapshot holds data for.
    pub coverage: TimeRange,
}

/// Whether a reference is available.
#[derive(Clone, Debug, PartialEq)]
pub enum ReferenceAvailability {
    /// Nothing cached for this key. Comparison against it is impossible, and says so.
    NotYetFetched {
        /// Feed id.
        source: String,
        /// Snapshot key asked for.
        key: String,
        /// The last fetch attempt, if one was made (and failed, or fetched another key).
        last_attempt: Option<Timestamp>,
        /// Its error, if any.
        last_error: Option<String>,
    },
    /// Cached.
    Cached(OrbitFeedStatus),
}

impl ReferenceAvailability {
    /// Whether it is cached.
    pub fn is_cached(&self) -> bool {
        matches!(self, ReferenceAvailability::Cached(_))
    }
}

fn availability(
    cache: &FeedCache,
    source: &str,
    key: &str,
    now: Timestamp,
    parse: impl Fn(&str) -> Result<TimeRange, ParseError>,
) -> Result<(ReferenceAvailability, Option<String>), FeedError> {
    let state = cache.state(source)?;
    let body = cache.snapshot(source, key)?;
    let record = state.as_ref().and_then(|s| s.snapshots.get(key));
    match (state.as_ref(), body, record) {
        (Some(state), Some(body), Some(record)) => {
            let coverage = parse(&body).map_err(|error| FeedError::Parse {
                source_id: source.to_owned(),
                key: key.to_owned(),
                error,
            })?;
            Ok((
                ReferenceAvailability::Cached(OrbitFeedStatus {
                    source: source.to_owned(),
                    key: key.to_owned(),
                    fetched_at: record.fetched_at,
                    cache_age_s: (now.as_unix_nanos() - record.fetched_at.as_unix_nanos()) as f64
                        / 1e9,
                    refresh_failed: state.stale,
                    last_error: state.last_error.clone(),
                    coverage,
                }),
                Some(body),
            ))
        }
        _ => Ok((
            ReferenceAvailability::NotYetFetched {
                source: source.to_owned(),
                key: key.to_owned(),
                last_attempt: state.as_ref().and_then(|s| s.last_attempt),
                last_error: state.and_then(|s| s.last_error),
            },
            None,
        )),
    }
}

/// Loads the cached precise orbits for `key`.
pub fn load_precise(
    cache: &FeedCache,
    key: &str,
    now: Timestamp,
) -> Result<(ReferenceAvailability, Option<Sp3>), FeedError> {
    let (avail, body) = availability(cache, SP3_SOURCE, key, now, |b| {
        Sp3Adapter.parse(key, b, now).map(|p| p.coverage)
    })?;
    let sp3 = body.map(|b| parse_sp3(&b).expect("parsed above"));
    Ok((avail, sp3))
}

/// Loads the cached reference broadcast ephemerides for `key`.
pub fn load_broadcast(
    cache: &FeedCache,
    key: &str,
    now: Timestamp,
) -> Result<(ReferenceAvailability, Option<Vec<GpsEphemeris>>), FeedError> {
    let (avail, body) = availability(cache, BRDC_SOURCE, key, now, |b| {
        BrdcAdapter.parse(key, b, now).map(|p| p.coverage)
    })?;
    let ephs = body.map(|b| parse_nav(&b).expect("parsed above").ephemerides);
    Ok((avail, ephs))
}

/// A forensics run: the references it used (or could not), and one check per received ephemeris.
#[derive(Clone, Debug, PartialEq)]
pub struct ForensicsReport {
    /// Precise-orbit reference.
    pub precise: ReferenceAvailability,
    /// Broadcast-ephemeris reference.
    pub broadcast: ReferenceAvailability,
    /// Per received ephemeris.
    pub checks: Vec<SvCheck>,
}

impl ForensicsReport {
    /// Checks that disagree with their reference (the findings).
    pub fn flagged(&self) -> impl Iterator<Item = &SvCheck> {
        self.checks
            .iter()
            .filter(|c| matches!(c.verdict, SvVerdict::Disagrees { .. }))
    }

    /// Checks where nothing could be compared (not agreement).
    pub fn uncompared(&self) -> impl Iterator<Item = &SvCheck> {
        self.checks.iter().filter(|c| !c.verdict.was_compared())
    }
}

/// Offline-first forensics: compares the received ephemerides `local` against whatever is cached
/// under `precise_key` (SP3) and `broadcast_key` (BRDC), however old — reporting the age — and
/// reports an uncached reference as not yet fetched.
pub fn compare_from_cache(
    cache: &FeedCache,
    precise_key: &str,
    broadcast_key: &str,
    local: &[GpsEphemeris],
    now: Timestamp,
    cfg: &ForensicsConfig,
) -> Result<ForensicsReport, FeedError> {
    let (precise, sp3) = load_precise(cache, precise_key, now)?;
    let (broadcast, brdc) = load_broadcast(cache, broadcast_key, now)?;
    Ok(ForensicsReport {
        checks: compare(local, sp3.as_ref(), brdc.as_deref(), cfg),
        precise,
        broadcast,
    })
}
