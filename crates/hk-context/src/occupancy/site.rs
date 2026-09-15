//! Site assignment for a moving device (T-119, ADR-0012 §3.5): discrete sites, `mobile` and
//! `unassigned`.
//!
//! - **Pinned** (config `--site` or `PUT /api/sites/current`): used as given; fixes do not move it.
//! - **With fixes:** ground speed above `mobile_speed_m_s` (reported, else the window's net drift
//!   over >= 30 s), or a position spread (2 x the largest distance from the window centroid) over
//!   `mobile_window_s` larger than the radius, is `Mobile`. Otherwise, once the device has been
//!   still for [`STILL_MIN_S`] (so a single fix mid-walk does not found a site), it joins the
//!   nearest site whose radius contains the window centroid or creates one there (source `gnss`,
//!   UTC offset guessed as round(lon/15) h).
//! - **Without a fix:** the last site is kept for `no_fix_hold_s` after the last in-site fix, then
//!   `Unassigned`.
//!
//! Only `Site` keys accrue baselines (`SiteKey::accrues_baseline`). All times are the sample clock
//! (ADR-0012 §0). Distances are great-circle ([`crate::geo::haversine_km`]).
//!
//! **Readers and restarts (T-136).** [`SiteAssigner::tick`] is irreversible (expiry clears the
//! fixes), so readers that run ahead of the occupancy close (history frames) use
//! [`SiteAssigner::peek`], which never changes state. The assignment in force
//! ([`SiteAssigner::assignment`]) is persisted by the owner when
//! [`SiteAssigner::take_assignment_change`] reports a change and [`SiteAssigner::restore`]d on
//! start, so a pinned site survives a restart and a fixed site keeps its no-fix hold.

use std::collections::VecDeque;

use hk_model::attention::baseline::{SiteAssignment, SiteConfig, SiteKey, SiteRecord, SiteSource};
use hk_model::ids::SiteId;
use hk_model::time::Timestamp;

use crate::geo::haversine_km;

/// Stillness needed before joining or founding a site, s (capped at `mobile_window_s`).
pub const STILL_MIN_S: f64 = 60.0;

/// Least advance of the last in-site time that counts as an assignment change to persist, s (so a
/// fix stream does not write every fix). Only that advance is coalesced: a site, pin or source
/// change (a pin, an unpin, a new site) persists at once, so after a restart the stored
/// `last_in_site` lags by less than this and the no-fix hold can end at most this much early.
pub const ASSIGNMENT_PERSIST_S: f64 = 60.0;

/// One position fix (C06).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fix {
    /// Fix time (sample clock).
    pub t: Timestamp,
    /// Latitude, degrees.
    pub lat_deg: f64,
    /// Longitude, degrees.
    pub lon_deg: f64,
    /// Ground speed, m/s, when the receiver reports it.
    pub speed_m_s: Option<f64>,
}

/// Site assignment errors.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SiteError {
    /// No such site.
    #[error("no such site")]
    NotFound,
    /// The record is invalid.
    #[error("invalid site: {0}")]
    Invalid(String),
}

fn secs_between(a: Timestamp, b: Timestamp) -> f64 {
    (b.as_unix_nanos() - a.as_unix_nanos()) as f64 / 1e9
}

/// The site state machine.
#[derive(Clone, Debug)]
pub struct SiteAssigner {
    cfg: SiteConfig,
    sites: Vec<SiteRecord>,
    fixes: VecDeque<Fix>,
    current: SiteKey,
    set_by: Option<SiteSource>,
    pinned: bool,
    last_in_site: Option<Timestamp>,
    dirty: Vec<SiteId>,
    /// The assignment last reported by `take_assignment_change` (`None` = never reported).
    persisted: Option<Option<SiteAssignment>>,
}

impl SiteAssigner {
    /// A new assigner over the known `sites`; starts `Unassigned`.
    pub fn new(cfg: SiteConfig, sites: Vec<SiteRecord>) -> Self {
        Self {
            cfg,
            sites,
            fixes: VecDeque::new(),
            current: SiteKey::Unassigned,
            set_by: None,
            pinned: false,
            last_in_site: None,
            dirty: Vec::new(),
            persisted: None,
        }
    }

    /// The discrete-site assignment in force (`None` when mobile or unassigned).
    pub fn assignment(&self) -> Option<SiteAssignment> {
        let SiteKey::Site(site) = self.current else {
            return None;
        };
        Some(SiteAssignment {
            site,
            set_by: self.set_by,
            pinned: self.pinned,
            last_in_site: self.last_in_site,
        })
    }

    /// Restores a persisted assignment (start-up). An unpinned site then keeps its no-fix hold
    /// from `last_in_site` on the sample clock, exactly as before the restart.
    pub fn restore(&mut self, a: SiteAssignment) -> Result<(), SiteError> {
        if self.site(a.site).is_none() {
            return Err(SiteError::NotFound);
        }
        self.current = SiteKey::Site(a.site);
        self.set_by = a.set_by;
        self.pinned = a.pinned;
        self.last_in_site = a.last_in_site;
        self.persisted = Some(Some(a));
        Ok(())
    }

    /// The assignment to persist when it changed since the last call: another site or none, a
    /// pin or source change, or the last in-site time moved by at least
    /// [`ASSIGNMENT_PERSIST_S`]. `Some(None)` clears the stored assignment.
    pub fn take_assignment_change(&mut self) -> Option<Option<SiteAssignment>> {
        let now = self.assignment();
        let changed = match (self.persisted, now) {
            (Some(None), None) => false,
            (Some(Some(old)), Some(new)) => {
                old.site != new.site
                    || old.set_by != new.set_by
                    || old.pinned != new.pinned
                    || match (old.last_in_site, new.last_in_site) {
                        (Some(a), Some(b)) => secs_between(a, b).abs() >= ASSIGNMENT_PERSIST_S,
                        (a, b) => a != b,
                    }
            }
            _ => true,
        };
        if !changed {
            return None;
        }
        self.persisted = Some(now);
        Some(now)
    }

    /// Current key.
    pub fn current(&self) -> SiteKey {
        self.current
    }

    /// How the current site was set (`None` when not a site).
    pub fn set_by(&self) -> Option<SiteSource> {
        self.set_by
    }

    /// Pinned by config or the user.
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Known sites.
    pub fn sites(&self) -> &[SiteRecord] {
        &self.sites
    }

    /// One site.
    pub fn site(&self, id: SiteId) -> Option<&SiteRecord> {
        self.sites.iter().find(|s| s.id == id)
    }

    /// The UTC offset slots use under `key` (0 when not a known site).
    pub fn utc_offset_min(&self, key: SiteKey) -> i16 {
        match key {
            SiteKey::Site(id) => self.site(id).map_or(0, |s| s.utc_offset_min),
            _ => 0,
        }
    }

    /// Adds (or replaces) a site record, validated.
    pub fn upsert(&mut self, record: SiteRecord) -> Result<(), SiteError> {
        record
            .validate()
            .map_err(|e| SiteError::Invalid(e.to_string()))?;
        let id = record.id;
        match self.sites.iter_mut().find(|s| s.id == id) {
            Some(s) => *s = record,
            None => self.sites.push(record),
        }
        self.mark(id);
        Ok(())
    }

    /// Pins the current site (config/user). Fixes no longer move it until [`unpin`](Self::unpin).
    pub fn pin(&mut self, id: SiteId, source: SiteSource, t: Timestamp) -> Result<(), SiteError> {
        if self.site(id).is_none() {
            return Err(SiteError::NotFound);
        }
        self.current = SiteKey::Site(id);
        self.set_by = Some(source);
        self.pinned = true;
        self.last_in_site = Some(t);
        Ok(())
    }

    /// Releases a pin; the next fix (or the no-fix hold) decides.
    pub fn unpin(&mut self) {
        self.pinned = false;
    }

    /// Folds a fix and returns the key in force.
    pub fn on_fix(&mut self, fix: Fix) -> SiteKey {
        if !(fix.lat_deg.is_finite() && fix.lon_deg.is_finite()) {
            return self.tick(fix.t);
        }
        self.fixes.push_back(fix);
        while self
            .fixes
            .front()
            .is_some_and(|f| secs_between(f.t, fix.t) > self.cfg.mobile_window_s)
        {
            self.fixes.pop_front();
        }
        if self.pinned {
            self.last_in_site = Some(fix.t);
            return self.current;
        }
        let n = self.fixes.len() as f64;
        let (clat, clon) = self.fixes.iter().fold((0.0, 0.0), |(a, b), f| {
            (a + f.lat_deg / n, b + f.lon_deg / n)
        });
        let spread_m = self
            .fixes
            .iter()
            .map(|f| haversine_km(clat, clon, f.lat_deg, f.lon_deg) * 1000.0)
            .fold(0.0, f64::max);
        let span = self.fixes.front().map_or(0.0, |f| secs_between(f.t, fix.t));
        // Ground speed: reported, else the window's net drift (robust to fix jitter over >= 30 s).
        let drift_m_s = match self.fixes.front() {
            Some(f) if span >= 30.0 => {
                haversine_km(f.lat_deg, f.lon_deg, fix.lat_deg, fix.lon_deg) * 1000.0 / span
            }
            _ => 0.0,
        };
        let fast = fix.speed_m_s.is_some_and(|v| v > self.cfg.mobile_speed_m_s)
            || drift_m_s > self.cfg.mobile_speed_m_s;
        // Spread as an extent: twice the largest distance from the centroid.
        if fast || 2.0 * spread_m > self.cfg.radius_m {
            self.current = SiteKey::Mobile;
            self.set_by = None;
            return self.current;
        }
        if span < STILL_MIN_S.min(self.cfg.mobile_window_s) {
            // Not yet known to be still: keep a site we are in, else wait.
            if let SiteKey::Site(id) = self.current
                && self.site(id).is_some_and(|s| {
                    s.lat_deg.zip(s.lon_deg).is_none_or(|(la, lo)| {
                        haversine_km(la, lo, fix.lat_deg, fix.lon_deg) * 1000.0 <= s.radius_m
                    })
                })
            {
                self.last_in_site = Some(fix.t);
                return self.current;
            }
            if self.current == SiteKey::Mobile {
                self.current = SiteKey::Unassigned;
            }
            return self.current;
        }
        let nearest = self
            .sites
            .iter()
            .filter_map(|s| {
                let d = haversine_km(s.lat_deg?, s.lon_deg?, clat, clon) * 1000.0;
                (d <= s.radius_m).then_some((d, s.id))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0));
        let id = match nearest {
            Some((_, id)) => id,
            None => {
                let record = SiteRecord {
                    id: SiteId::new(),
                    name: None,
                    lat_deg: Some(clat),
                    lon_deg: Some(clon),
                    radius_m: self.cfg.radius_m,
                    utc_offset_min: ((clon / 15.0).round() * 60.0).clamp(-840.0, 840.0) as i16,
                    source: SiteSource::Gnss,
                    first_seen: fix.t,
                    last_seen: fix.t,
                    observed_s: 0.0,
                };
                let id = record.id;
                self.sites.push(record);
                self.mark(id);
                id
            }
        };
        if self.current != SiteKey::Site(id) {
            self.set_by = Some(self.site(id).map_or(SiteSource::Gnss, |s| s.source));
        }
        self.current = SiteKey::Site(id);
        self.last_in_site = Some(fix.t);
        self.current
    }

    /// Advances time without a fix: after `no_fix_hold_s` since the last in-site fix, an unpinned
    /// site (or mobile state) becomes `Unassigned`.
    pub fn tick(&mut self, t: Timestamp) -> SiteKey {
        if self.expired_at(t) {
            self.current = SiteKey::Unassigned;
            self.set_by = None;
            self.fixes.clear();
        }
        self.current
    }

    /// The key [`tick`](Self::tick) would return at `t`, without changing any state (T-136: for
    /// readers that may run ahead of the owner's clock, e.g. history frames).
    pub fn peek(&self, t: Timestamp) -> SiteKey {
        if self.expired_at(t) {
            SiteKey::Unassigned
        } else {
            self.current
        }
    }

    fn expired_at(&self, t: Timestamp) -> bool {
        if self.pinned {
            return false;
        }
        let last_fix = self.fixes.back().map(|f| f.t);
        let stale = |since: Option<Timestamp>| {
            since.is_none_or(|s| secs_between(s, t) > self.cfg.no_fix_hold_s)
        };
        match self.current {
            SiteKey::Site(_) => stale(self.last_in_site) && stale(last_fix),
            SiteKey::Mobile => stale(last_fix),
            SiteKey::Unassigned => false,
        }
    }

    /// Accounts `observed_s` of observation ending at `t` to the current site.
    pub fn record_observation(&mut self, t: Timestamp, observed_s: f64) {
        let SiteKey::Site(id) = self.current else {
            return;
        };
        if let Some(s) = self.sites.iter_mut().find(|s| s.id == id) {
            s.last_seen = s.last_seen.max(t);
            s.first_seen = s.first_seen.min(t);
            s.observed_s += observed_s.max(0.0);
            self.mark(id);
        }
    }

    fn mark(&mut self, id: SiteId) {
        if !self.dirty.contains(&id) {
            self.dirty.push(id);
        }
    }

    /// Records changed since the last call (to persist).
    pub fn take_dirty(&mut self) -> Vec<SiteRecord> {
        let ids = std::mem::take(&mut self.dirty);
        ids.iter()
            .filter_map(|id| self.site(*id).cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: f64) -> Timestamp {
        Timestamp::from_unix_nanos((s * 1e9) as i64)
    }

    /// Metres north of (51.5, 0) → degrees.
    fn north(m: f64) -> f64 {
        51.5 + m / 111_195.0
    }

    fn fix(s: f64, north_m: f64, speed: Option<f64>) -> Fix {
        Fix {
            t: t(s),
            lat_deg: north(north_m),
            lon_deg: 0.0,
            speed_m_s: speed,
        }
    }

    #[test]
    fn site_is_founded_after_stillness_and_rejoined_later() {
        let mut a = SiteAssigner::new(SiteConfig::default(), Vec::new());
        assert_eq!(a.on_fix(fix(0.0, 0.0, Some(0.0))), SiteKey::Unassigned);
        assert_eq!(a.on_fix(fix(30.0, 3.0, None)), SiteKey::Unassigned);
        let SiteKey::Site(home) = a.on_fix(fix(61.0, 1.0, None)) else {
            panic!("still for a minute founds a site");
        };
        assert_eq!(a.set_by(), Some(SiteSource::Gnss));
        assert_eq!(a.take_dirty().len(), 1);
        // Walk away at 1.5 m/s: mobile.
        assert_eq!(a.on_fix(fix(100.0, 60.0, Some(1.5))), SiteKey::Mobile);
        // No fix for longer than the hold: unassigned.
        assert_eq!(a.tick(t(800.0)), SiteKey::Unassigned);
        // Come back and settle: the same site, not a new one.
        for s in [2000.0, 2030.0, 2070.0] {
            a.on_fix(fix(s, 10.0, Some(0.1)));
        }
        assert_eq!(a.current(), SiteKey::Site(home));
        assert_eq!(a.sites().len(), 1);
    }

    #[test]
    fn site_spread_without_speed_is_mobile_and_no_fix_hold_keeps_the_site() {
        let mut a = SiteAssigner::new(SiteConfig::default(), Vec::new());
        // A slow drive with no speed field: 400 m in 240 s exceeds the 250 m radius.
        for i in 0..=8 {
            a.on_fix(fix(f64::from(i) * 30.0, f64::from(i) * 50.0, None));
        }
        assert_eq!(a.current(), SiteKey::Mobile);
        let mut b = SiteAssigner::new(SiteConfig::default(), Vec::new());
        b.on_fix(fix(0.0, 0.0, None));
        let key = b.on_fix(fix(70.0, 0.0, None));
        assert!(key.accrues_baseline());
        assert_eq!(b.tick(t(70.0 + 599.0)), key, "held for no_fix_hold_s");
        assert_eq!(b.tick(t(70.0 + 601.0)), SiteKey::Unassigned);
    }

    #[test]
    fn site_pin_is_used_as_given() {
        let rec = SiteRecord {
            id: SiteId::new(),
            name: Some("hilltop".into()),
            lat_deg: None,
            lon_deg: None,
            radius_m: 250.0,
            utc_offset_min: 120,
            source: SiteSource::Config,
            first_seen: t(0.0),
            last_seen: t(0.0),
            observed_s: 0.0,
        };
        let mut a = SiteAssigner::new(SiteConfig::default(), vec![rec.clone()]);
        assert_eq!(
            a.pin(SiteId::new(), SiteSource::User, t(0.0)),
            Err(SiteError::NotFound)
        );
        a.pin(rec.id, SiteSource::Config, t(0.0)).unwrap();
        assert_eq!(
            a.on_fix(fix(1.0, 5000.0, Some(20.0))),
            SiteKey::Site(rec.id)
        );
        assert_eq!(a.tick(t(1e6)), SiteKey::Site(rec.id));
        assert_eq!(a.utc_offset_min(a.current()), 120);
        a.record_observation(t(10.0), 900.0);
        assert_eq!(a.take_dirty()[0].observed_s, 900.0);
    }

    /// T-136: `peek` never changes state (a reader ahead of the hold does not expire the site),
    /// and a persisted assignment restores a pin, and a fixed site with its hold, after a restart.
    #[test]
    fn site_peek_is_pure_and_assignment_restores() {
        let mut a = SiteAssigner::new(SiteConfig::default(), Vec::new());
        a.on_fix(fix(0.0, 0.0, None));
        let key = a.on_fix(fix(70.0, 0.0, None));
        let SiteKey::Site(home) = key else {
            panic!("founded");
        };
        assert_eq!(a.peek(t(900.0)), SiteKey::Unassigned, "past the hold");
        assert_eq!(a.current(), key, "peek changed nothing");
        assert_eq!(a.tick(t(600.0)), key, "an earlier tick still sees the site");
        // Assignment changes are reported once, then only on a real change.
        let stored = a.take_assignment_change().expect("first report").unwrap();
        assert_eq!((stored.site, stored.pinned), (home, false));
        assert_eq!(a.take_assignment_change(), None);
        a.on_fix(fix(90.0, 1.0, None));
        assert_eq!(
            a.take_assignment_change(),
            None,
            "last in-site moved < 60 s"
        );
        a.on_fix(fix(140.0, 1.0, None));
        let stored = a.take_assignment_change().unwrap().unwrap();
        // Restart: the fixed site is kept within its hold from the stored in-site time.
        let mut b = SiteAssigner::new(SiteConfig::default(), a.sites().to_vec());
        b.restore(stored).unwrap();
        assert_eq!(b.take_assignment_change(), None, "restored = persisted");
        assert_eq!(b.tick(t(700.0)), key);
        assert_eq!(b.tick(t(741.0)), SiteKey::Unassigned);
        assert_eq!(b.take_assignment_change(), Some(None), "expiry clears it");
        // A pin is kept whatever the time.
        b.pin(home, SiteSource::User, t(800.0)).unwrap();
        let pinned = b.take_assignment_change().unwrap().unwrap();
        let mut c = SiteAssigner::new(SiteConfig::default(), b.sites().to_vec());
        c.restore(pinned).unwrap();
        assert_eq!(c.tick(t(1e6)), key);
        assert!(c.is_pinned());
        assert_eq!(c.set_by(), Some(SiteSource::User));
        let mut empty = SiteAssigner::new(SiteConfig::default(), Vec::new());
        assert_eq!(empty.restore(pinned), Err(SiteError::NotFound));
    }
}
