//! Sites and interestingness score weights (T-119, ADR-0012 §3.5, §4.2, §9; migration 0002).
//!
//! - **Sites** are user metadata: upserted as they are created (config, user, GNSS clustering) and
//!   as their `last_seen`/`observed_s` advance. Names are unique when set.
//! - **Weights** are versioned and append-only. Version 1 is the built-in
//!   [`ScoreWeights::default`] and is never stored; [`Repository::insert_score_weights`] assigns
//!   `latest + 1` atomically, so concurrent writers cannot reuse a version.

use rusqlite::{OptionalExtension, params};

use super::{RepoError, Repository, blob, bodies, body_by_id, enum_text};
use crate::attention::baseline::SiteRecord;
use crate::attention::score::ScoreWeights;
use crate::ids::SiteId;
use crate::time::Timestamp;

/// Most sites [`Repository::sites`] returns.
pub const SITES_MAX: usize = 10_000;

/// Most weight versions [`Repository::score_weights_history`] returns (newest first).
pub const WEIGHTS_HISTORY_MAX: usize = 1_000;

fn invalid(e: crate::attention::ValidationError) -> RepoError {
    RepoError::Invalid(e.to_string())
}

/// One stored weights version.
#[derive(Clone, Debug, PartialEq)]
pub struct ScoreWeightsVersion {
    /// The weights, `version` set from the row.
    pub weights: ScoreWeights,
    /// When it was stored.
    pub created_at: Timestamp,
    /// Who stored it (audit token id or `local`).
    pub author: String,
}

impl Repository {
    /// Inserts or replaces a site (validated first).
    pub fn upsert_site(&self, site: &SiteRecord) -> Result<(), RepoError> {
        site.validate().map_err(invalid)?;
        let body = serde_json::to_string(site)?;
        self.conn
            .prepare_cached(
                "INSERT INTO site (site_id, name, lat_deg, lon_deg, radius_m, utc_offset_min, \
                 source, first_seen, last_seen, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                 ON CONFLICT (site_id) DO UPDATE SET name = excluded.name, \
                 lat_deg = excluded.lat_deg, lon_deg = excluded.lon_deg, \
                 radius_m = excluded.radius_m, utc_offset_min = excluded.utc_offset_min, \
                 source = excluded.source, first_seen = excluded.first_seen, \
                 last_seen = excluded.last_seen, body = excluded.body",
            )?
            .execute(params![
                blob(site.id),
                site.name,
                site.lat_deg,
                site.lon_deg,
                site.radius_m,
                i64::from(site.utc_offset_min),
                enum_text(&site.source)?,
                site.first_seen.as_unix_nanos(),
                site.last_seen.as_unix_nanos(),
                body,
            ])?;
        Ok(())
    }

    /// One site.
    pub fn site(&self, id: SiteId) -> Result<SiteRecord, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM site WHERE site_id = ?1",
            blob(id),
            "site",
        )
    }

    /// The site named `name`, if any.
    pub fn site_by_name(&self, name: &str) -> Result<Option<SiteRecord>, RepoError> {
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM site WHERE name = ?1")?
            .query_row([name], |r| r.get(0))
            .optional()?;
        Ok(body.map(|b| serde_json::from_str(&b)).transpose()?)
    }

    /// All sites, oldest first (at most [`SITES_MAX`]).
    pub fn sites(&self) -> Result<Vec<SiteRecord>, RepoError> {
        bodies(
            &self.conn,
            "SELECT body FROM site ORDER BY first_seen, site_id LIMIT ?1",
            [SITES_MAX as i64],
        )
    }

    /// The weights in force: the newest stored version, else the version-1 defaults.
    pub fn score_weights(&self) -> Result<ScoreWeights, RepoError> {
        Ok(self
            .score_weights_history_limit(1)?
            .pop()
            .map_or_else(ScoreWeights::default, |v| v.weights))
    }

    /// Stored versions, newest first (at most [`WEIGHTS_HISTORY_MAX`]).
    pub fn score_weights_history(&self) -> Result<Vec<ScoreWeightsVersion>, RepoError> {
        self.score_weights_history_limit(WEIGHTS_HISTORY_MAX)
    }

    fn score_weights_history_limit(
        &self,
        limit: usize,
    ) -> Result<Vec<ScoreWeightsVersion>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT version, created_at, author, body FROM attention_weights \
             ORDER BY version DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(version, t, author, body)| {
                let mut weights: ScoreWeights = serde_json::from_str(&body)?;
                weights.version = u32::try_from(version)
                    .map_err(|_| RepoError::Invalid("weights version out of range".into()))?;
                Ok(ScoreWeightsVersion {
                    weights,
                    created_at: Timestamp::from_unix_nanos(t),
                    author,
                })
            })
            .collect()
    }

    /// Stores `weights` (its `version` is ignored) as the next version and returns it with that
    /// version. Validated first; the version is assigned inside the single INSERT, so it is atomic.
    pub fn insert_score_weights(
        &self,
        weights: &ScoreWeights,
        author: &str,
        at: Timestamp,
    ) -> Result<ScoreWeights, RepoError> {
        let probe = ScoreWeights {
            version: 1,
            ..*weights
        };
        probe.validate().map_err(invalid)?;
        let body = serde_json::to_string(&probe)?;
        let version: i64 = self
            .conn
            .prepare_cached(
                "INSERT INTO attention_weights (version, created_at, author, body) \
                 SELECT COALESCE(MAX(version), 1) + 1, ?1, ?2, ?3 FROM attention_weights \
                 RETURNING version",
            )?
            .query_row(params![at.as_unix_nanos(), author, body], |r| r.get(0))?;
        Ok(ScoreWeights {
            version: u32::try_from(version)
                .map_err(|_| RepoError::Invalid("weights version out of range".into()))?,
            ..probe
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::attention::baseline::{SiteRecord, SiteSource};
    use crate::attention::score::ScoreWeights;
    use crate::ids::SiteId;
    use crate::repo::{RepoError, Repository};
    use crate::time::Timestamp;

    fn site(name: Option<&str>) -> SiteRecord {
        SiteRecord {
            id: SiteId::new(),
            name: name.map(Into::into),
            lat_deg: Some(51.5),
            lon_deg: Some(-0.12),
            radius_m: 250.0,
            utc_offset_min: 60,
            source: SiteSource::User,
            first_seen: Timestamp::from_unix_nanos(1),
            last_seen: Timestamp::from_unix_nanos(2),
            observed_s: 0.0,
        }
    }

    #[test]
    fn sites_upsert_read_and_names_are_unique() {
        let r = Repository::open_in_memory().unwrap();
        assert!(r.schema_version().unwrap() >= 2);
        let mut home = site(Some("home"));
        r.upsert_site(&home).unwrap();
        home.observed_s = 3600.0;
        home.last_seen = Timestamp::from_unix_nanos(10);
        r.upsert_site(&home).unwrap();
        assert_eq!(r.site(home.id).unwrap(), home);
        assert_eq!(r.site_by_name("home").unwrap(), Some(home.clone()));
        assert_eq!(r.site_by_name("away").unwrap(), None);
        r.upsert_site(&site(None)).unwrap();
        assert_eq!(r.sites().unwrap().len(), 2);
        assert!(r.upsert_site(&site(Some("home"))).is_err(), "unique name");
        let mut bad = site(None);
        bad.radius_m = 1.0;
        assert!(matches!(r.upsert_site(&bad), Err(RepoError::Invalid(_))));
        assert!(matches!(
            r.site(SiteId::new()),
            Err(RepoError::NotFound { .. })
        ));
    }

    #[test]
    fn score_weights_are_versioned_and_append_only() {
        let r = Repository::open_in_memory().unwrap();
        assert_eq!(r.score_weights().unwrap(), ScoreWeights::default());
        let w = ScoreWeights {
            version: 99,
            novelty: 5.0,
            ..ScoreWeights::default()
        };
        let v2 = r
            .insert_score_weights(&w, "tok", Timestamp::from_unix_nanos(5))
            .unwrap();
        assert_eq!((v2.version, v2.novelty), (2, 5.0));
        let v3 = r
            .insert_score_weights(
                &ScoreWeights::default(),
                "tok",
                Timestamp::from_unix_nanos(6),
            )
            .unwrap();
        assert_eq!(v3.version, 3);
        assert_eq!(r.score_weights().unwrap(), v3);
        let h = r.score_weights_history().unwrap();
        assert_eq!(
            h.iter().map(|v| v.weights.version).collect::<Vec<_>>(),
            [3, 2]
        );
        assert_eq!(h[1].author, "tok");
        let bad = ScoreWeights {
            snr: 11.0,
            ..ScoreWeights::default()
        };
        assert!(matches!(
            r.insert_score_weights(&bad, "tok", Timestamp::from_unix_nanos(7)),
            Err(RepoError::Invalid(_))
        ));
        assert!(
            r.conn
                .execute("UPDATE attention_weights SET author = 'x'", [])
                .is_err()
        );
        assert!(r.conn.execute("DELETE FROM attention_weights", []).is_err());
    }
}
