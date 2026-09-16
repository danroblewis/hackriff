//! C18 cluster storage (T-202, ADR-0016 §5/§9): clusters of unknown emissions, their membership
//! and their history, over migration 0011.
//!
//! Three rules this module keeps, on top of the migration's triggers:
//!
//! - **Membership is append-only with supersession.** An emitter has at most one *current*
//!   cluster — the highest `link_id` — and every earlier answer stays readable. A link whose
//!   `cluster_id` is `NULL` records that the clusterer looked and **abstained**, which is
//!   deliberately a different row from never having looked.
//! - **A cluster row is the current state; its history is beside it.** The centroid moves as
//!   members fold in, so that row is mutable; every merge, split, reassignment, activation and
//!   promotion is an append-only `cluster_event` with its arithmetic.
//! - **Nothing here writes to `emitter`.** Not identity, not family, not `known_status`, not
//!   lifecycle. A cluster is evidence about emitters, never a change to one (ADR-0016 §5), and
//!   [`super::signature_tests`]-style tests in `hk_context::signature::cluster` pin it.

use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use super::{RepoError, Repository, blob, bodies, int};
use crate::ids::EmitterId;
use crate::signature::cluster::{
    ClusterEvent, ClusterEventKind, ClusterState, EmitterClusterLink, SignatureCluster,
    is_cluster_id,
};
use crate::signature::{
    FeatValue, FieldExpect, FieldSpec, Signature, SignatureKind, SignatureProvenance, SignatureRef,
    default_tolerance, field,
};
use crate::time::Timestamp;

/// Longest merge chain followed when resolving a cluster id to its survivor.
const MERGE_CHAIN_MAX: usize = 32;

fn no_such(id: &str) -> RepoError {
    RepoError::NotFound {
        kind: "signature cluster",
        id: id.to_owned(),
    }
}

fn emitter_of(b: [u8; 16]) -> EmitterId {
    EmitterId::from_uuid(Uuid::from_bytes(b))
}

impl Repository {
    /// Inserts or updates one cluster's current state. Validated first: a stored cluster that
    /// breaks its own contract would mis-assign every later emitter.
    pub fn put_cluster(&mut self, c: &SignatureCluster) -> Result<(), RepoError> {
        c.validate().map_err(|e| RepoError::Invalid(e.0))?;
        self.conn.execute(
            "INSERT INTO signature_cluster (cluster_id, state, merged_into, signature_id, \
             signature_version, created_at, updated_at, folds, suspect_fraction, body) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT (cluster_id) DO UPDATE SET \
             state = excluded.state, merged_into = excluded.merged_into, \
             signature_id = excluded.signature_id, signature_version = excluded.signature_version, \
             updated_at = excluded.updated_at, folds = excluded.folds, \
             suspect_fraction = excluded.suspect_fraction, body = excluded.body",
            params![
                c.id,
                c.state.as_str(),
                c.merged_into,
                c.signature.as_ref().map(|s| s.id.clone()),
                c.signature.as_ref().map(|s| s.version),
                c.created_at.as_unix_nanos(),
                c.updated_at.as_unix_nanos(),
                c.centroid.folds,
                c.centroid.suspect_fraction,
                serde_json::to_string(c)?,
            ],
        )?;
        Ok(())
    }

    /// One cluster by id.
    pub fn cluster(&self, id: &str) -> Result<SignatureCluster, RepoError> {
        self.cluster_opt(id)?.ok_or_else(|| no_such(id))
    }

    /// One cluster by id, or `None`.
    pub fn cluster_opt(&self, id: &str) -> Result<Option<SignatureCluster>, RepoError> {
        if !is_cluster_id(id) {
            return Ok(None);
        }
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM signature_cluster WHERE cluster_id = ?1")?
            .query_row(params![id], |r| r.get(0))
            .optional()?;
        match body {
            Some(b) => Ok(Some(serde_json::from_str(&b)?)),
            None => Ok(None),
        }
    }

    /// Every cluster, oldest id first (ids are time-ordered).
    pub fn clusters(&self) -> Result<Vec<SignatureCluster>, RepoError> {
        bodies(
            &self.conn,
            "SELECT body FROM signature_cluster ORDER BY cluster_id",
            [],
        )
    }

    /// Clusters an emitter may be assigned to: everything that has not been merged away.
    pub fn open_clusters(&self) -> Result<Vec<SignatureCluster>, RepoError> {
        bodies(
            &self.conn,
            "SELECT body FROM signature_cluster WHERE state != 'merged' ORDER BY cluster_id",
            [],
        )
    }

    /// Follows the merge chain: the cluster that `id` is now part of. Unknown ids resolve to
    /// themselves, so a stale link never becomes an error.
    pub fn live_cluster_id(&self, id: &str) -> Result<String, RepoError> {
        let mut current = id.to_owned();
        for _ in 0..MERGE_CHAIN_MAX {
            let next: Option<Option<String>> = self
                .conn
                .prepare_cached("SELECT merged_into FROM signature_cluster WHERE cluster_id = ?1")?
                .query_row(params![current], |r| r.get(0))
                .optional()?;
            match next {
                Some(Some(into)) if into != current => current = into,
                _ => return Ok(current),
            }
        }
        Ok(current)
    }

    /// Appends one membership decision. `cluster_id` `None` records an abstention.
    pub fn link_emitter_cluster(&mut self, link: &EmitterClusterLink) -> Result<(), RepoError> {
        if link.reason.trim().is_empty() {
            return Err(RepoError::Invalid(
                "a cluster membership names its reason".into(),
            ));
        }
        if let Some(id) = &link.cluster_id
            && !is_cluster_id(id)
        {
            return Err(RepoError::Invalid(format!("{id} is not a cluster id")));
        }
        if let Some(d) = link.distance
            && !d.is_finite()
        {
            return Err(RepoError::Invalid("distance must be finite".into()));
        }
        self.conn.execute(
            "INSERT INTO emitter_cluster (emitter_id, cluster_id, t, reason, distance) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blob(link.emitter_id),
                link.cluster_id,
                link.t.as_unix_nanos(),
                link.reason,
                link.distance,
            ],
        )?;
        Ok(())
    }

    /// The emitter's current membership decision, or `None` when it has never been clustered.
    pub fn emitter_cluster(&self, id: EmitterId) -> Result<Option<EmitterClusterLink>, RepoError> {
        Ok(self.emitter_cluster_history(id, 1)?.into_iter().next())
    }

    /// The emitter's current cluster id, following merges; `None` when unassigned.
    pub fn emitter_cluster_id(&self, id: EmitterId) -> Result<Option<String>, RepoError> {
        match self.emitter_cluster(id)?.and_then(|l| l.cluster_id) {
            Some(c) => Ok(Some(self.live_cluster_id(&c)?)),
            None => Ok(None),
        }
    }

    /// The emitter's membership history, newest first.
    pub fn emitter_cluster_history(
        &self,
        id: EmitterId,
        limit: u32,
    ) -> Result<Vec<EmitterClusterLink>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT cluster_id, t, reason, distance FROM emitter_cluster WHERE emitter_id = ?1 \
             ORDER BY link_id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![blob(id), limit], |r| {
                Ok(EmitterClusterLink {
                    emitter_id: id,
                    cluster_id: r.get(0)?,
                    t: Timestamp::from_unix_nanos(r.get(1)?),
                    reason: r.get(2)?,
                    distance: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The emitters whose *current* membership is this cluster, oldest first.
    pub fn cluster_members(&self, cluster_id: &str) -> Result<Vec<EmitterId>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ec.emitter_id FROM emitter_cluster ec \
             WHERE ec.cluster_id = ?1 AND ec.link_id = \
               (SELECT max(link_id) FROM emitter_cluster e2 WHERE e2.emitter_id = ec.emitter_id) \
             ORDER BY ec.link_id",
        )?;
        let rows = stmt
            .query_map(params![cluster_id], |r| r.get::<_, [u8; 16]>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(emitter_of).collect())
    }

    /// Every emitter the clusterer has ever decided about, with its current decision.
    pub fn clustered_emitters(&self) -> Result<Vec<EmitterClusterLink>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ec.emitter_id, ec.cluster_id, ec.t, ec.reason, ec.distance FROM emitter_cluster ec \
             WHERE ec.link_id = \
               (SELECT max(link_id) FROM emitter_cluster e2 WHERE e2.emitter_id = ec.emitter_id) \
             ORDER BY ec.link_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(EmitterClusterLink {
                    emitter_id: emitter_of(r.get::<_, [u8; 16]>(0)?),
                    cluster_id: r.get(1)?,
                    t: Timestamp::from_unix_nanos(r.get(2)?),
                    reason: r.get(3)?,
                    distance: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Appends one cluster history row.
    pub fn append_cluster_event(&mut self, e: &ClusterEvent) -> Result<(), RepoError> {
        self.conn.execute(
            "INSERT INTO cluster_event (cluster_id, kind, other_cluster_id, emitter_id, t, detail) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                e.cluster_id,
                e.kind.as_str(),
                e.other_cluster_id,
                e.emitter_id.map(blob),
                e.t.as_unix_nanos(),
                serde_json::to_string(&e.detail)?,
            ],
        )?;
        Ok(())
    }

    /// A cluster's history, newest first.
    pub fn cluster_events(
        &self,
        cluster_id: &str,
        limit: u32,
    ) -> Result<Vec<ClusterEvent>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT cluster_id, kind, other_cluster_id, emitter_id, t, detail FROM cluster_event \
             WHERE cluster_id = ?1 ORDER BY event_id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![cluster_id, int(u64::from(limit), "limit")?], |r| {
                let kind: String = r.get(1)?;
                let emitter: Option<[u8; 16]> = r.get(3)?;
                let detail: String = r.get(5)?;
                Ok((
                    ClusterEvent {
                        cluster_id: r.get(0)?,
                        kind: ClusterEventKind::Created,
                        other_cluster_id: r.get(2)?,
                        emitter_id: emitter.map(emitter_of),
                        t: Timestamp::from_unix_nanos(r.get(4)?),
                        detail: serde_json::Value::Null,
                    },
                    kind,
                    detail,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(mut e, kind, detail)| {
                e.kind = serde_json::from_value(serde_json::Value::String(kind))?;
                e.detail = serde_json::from_str(&detail)?;
                Ok(e)
            })
            .collect()
    }

    /// How many emitters currently belong to this cluster.
    pub fn cluster_member_count(&self, cluster_id: &str) -> Result<usize, RepoError> {
        Ok(self.cluster_members(cluster_id)?.len())
    }

    /// Clusters that are visible to a reader ([`ClusterState::visible`]), oldest first.
    pub fn visible_clusters(&self) -> Result<Vec<SignatureCluster>, RepoError> {
        Ok(self
            .clusters()?
            .into_iter()
            .filter(|c| c.state.visible())
            .collect())
    }

    /// Mints a [`Signature`] from a cluster's centroid, with provenance `cluster-promoted`
    /// (ADR-0016 §5).
    ///
    /// The minted entry is **still evidence**: the matcher scores it like any other catalogue row
    /// and a match against it sets no identity, so promoting a cluster turns "these measured
    /// alike" into "here is a hypothesis to rank", never into "this is what it is".
    ///
    /// Refused when the cluster is not visible yet, when every observation behind it was suspect
    /// (the 8-bit, preselector-less front end makes ghosts with real-looking parameters — C18
    /// card), or when fewer than three discriminating fields were measured: the same floor the
    /// matcher enforces before it will call anything a `full` match.
    pub fn promote_cluster(
        &mut self,
        cluster_id: &str,
        author: &str,
        t: Timestamp,
    ) -> Result<Signature, RepoError> {
        let live = self.live_cluster_id(cluster_id)?;
        let mut cluster = self.cluster(&live)?;
        if cluster.state == ClusterState::Promoted {
            return Err(RepoError::Invalid(
                "this cluster has already been promoted".into(),
            ));
        }
        if cluster.state != ClusterState::Active {
            return Err(RepoError::Invalid(
                "only a visible (active) cluster may be promoted".into(),
            ));
        }
        if !cluster.may_mint() {
            return Err(RepoError::Invalid(
                "every observation behind this cluster was suspect; nothing is minted from one"
                    .into(),
            ));
        }

        let mut fields: BTreeMap<String, FieldSpec> = BTreeMap::new();
        let mut required = 0u32;
        for (name, feat) in &cluster.centroid.fields {
            let discriminating = DISCRIMINATING.contains(&name.as_str());
            let expect = match &feat.value {
                FeatValue::Num { value } if value.is_finite() => {
                    FieldExpect::Value { value: *value }
                }
                FeatValue::Bits { bits } if !bits.is_empty() => FieldExpect::Bits {
                    bits: bits.clone(),
                    max_errors: u8::try_from((bits.len() / 8).max(1)).unwrap_or(u8::MAX),
                },
                FeatValue::Text { text } if !text.trim().is_empty() => FieldExpect::Text {
                    text: text.trim().to_owned(),
                },
                _ => continue,
            };
            // The tolerance the members actually showed: never tighter than the field's default,
            // widened to ±3σ when they disagreed by more than that. A promoted entry must not be
            // stricter than the measurements it was minted from.
            let tolerance = match &feat.value {
                FeatValue::Num { value } if value.abs() > 0.0 => Some(
                    (3.0 * feat.sigma / value.abs())
                        .max(default_tolerance(name))
                        .min(0.5),
                ),
                _ => None,
            };
            if discriminating {
                required += 1;
            }
            fields.insert(
                name.clone(),
                FieldSpec {
                    expect,
                    tolerance,
                    required: discriminating,
                    weight: if discriminating { 1.0 } else { 0.5 },
                },
            );
        }
        if required < MIN_PROMOTED_FIELDS {
            return Err(RepoError::Invalid(format!(
                "a promoted cluster needs at least {MIN_PROMOTED_FIELDS} discriminating fields; \
                 this one measured {required}"
            )));
        }

        let tail = live.trim_start_matches("cluster:").to_ascii_lowercase();
        let short: String = tail.chars().take(8).collect();
        let signature = Signature {
            schema: crate::signature::SIGNATURE_SCHEMA,
            id: format!("cluster-{tail}"),
            version: 1,
            name: format!("Unknown cluster {short}"),
            kind: SignatureKind::Learned,
            // No family and no class: a minted entry gates nothing. The measured family travels
            // as an ordinary expected field instead, where it is scored like any other.
            taxonomy: None,
            family: None,
            class: None,
            fields,
            min_discriminating: MIN_PROMOTED_FIELDS,
            recipe: None,
            provenance: SignatureProvenance::ClusterPromoted,
            author: author.to_owned(),
            created_at: t,
            supersedes: None,
            bands_hz: Vec::new(),
            notes: Some(format!(
                "minted from {live}: what these emissions measured like. Evidence, never identity."
            )),
        };
        signature.validate().map_err(|e| RepoError::Invalid(e.0))?;
        self.insert_signature(&signature)?;

        cluster.state = ClusterState::Promoted;
        cluster.signature = Some(SignatureRef {
            id: signature.id.clone(),
            version: signature.version,
        });
        cluster.updated_at = t;
        self.put_cluster(&cluster)?;
        self.append_cluster_event(&ClusterEvent {
            cluster_id: live,
            kind: ClusterEventKind::Promoted,
            other_cluster_id: None,
            emitter_id: None,
            t,
            detail: serde_json::json!({
                "signature": signature.id,
                "version": signature.version,
            }),
        })?;
        Ok(signature)
    }
}

/// Fields whose agreement a promoted signature *requires*, when the cluster measured them: the
/// discriminating ones from the C18 card (symbol rate + deviation + sync word identify most
/// LMR/ISM protocols; periodicity and length separate sensor families).
const DISCRIMINATING: &[&str] = &[
    field::FAMILY,
    field::SYMBOL_RATE_HZ,
    field::DEVIATION_HZ,
    field::LEVELS,
    field::LINE_CODE,
    field::SYNC_WORD,
    field::PREAMBLE,
    field::PERIOD_S,
    field::BURST_LENGTH_S,
    field::COMB_SPACING_HZ,
    field::PRI_S,
];

/// Fewest discriminating fields a promoted entry needs — the matcher's own floor, so a minted
/// entry can never identify something from less than the matcher would accept.
const MIN_PROMOTED_FIELDS: u32 = 3;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{Fingerprint, Sighting};
    use crate::ids::TrackId;
    use crate::signature::cluster::new_cluster_id;
    use crate::signature::{Feat, field};
    use crate::{LinkTarget, TimeRange};

    fn t(sec: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
    }

    fn an_emitter(r: &mut Repository, f: f64) -> EmitterId {
        r.record_sighting(
            &Sighting {
                source: LinkTarget::Track(TrackId::new()),
                seen: TimeRange::new(t(0), t(1)),
                count: 3,
                f_center_hz: f,
                bandwidth_hz: 36e3,
                fingerprint: Some(Fingerprint::new(f, 36e3)),
                identity: None,
                context: None,
                classification: None,
                tags: Vec::new(),
            },
            None,
        )
        .unwrap()
        .emitter_id
    }

    fn a_cluster(r: &mut Repository, t0: Timestamp) -> SignatureCluster {
        let mut c = SignatureCluster::new(new_cluster_id(), t0);
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            field::SYMBOL_RATE_HZ.to_owned(),
            Feat::num(4800.0, 5.0, "c14"),
        );
        c.centroid.fold_member(&fields, 0.0);
        r.put_cluster(&c).unwrap();
        c
    }

    #[test]
    fn a_cluster_round_trips_and_updates_in_place_while_its_history_only_grows() {
        let mut r = Repository::open_in_memory().unwrap();
        let mut c = a_cluster(&mut r, t(0));
        assert_eq!(r.cluster(&c.id).unwrap(), c);
        assert_eq!(r.clusters().unwrap().len(), 1);
        assert!(
            r.visible_clusters().unwrap().is_empty(),
            "pending is hidden"
        );

        c.state = ClusterState::Active;
        c.updated_at = t(10);
        r.put_cluster(&c).unwrap();
        assert_eq!(r.cluster(&c.id).unwrap().state, ClusterState::Active);
        assert_eq!(r.clusters().unwrap().len(), 1, "updated, not duplicated");
        assert_eq!(r.visible_clusters().unwrap().len(), 1);

        r.append_cluster_event(&ClusterEvent::new(&c.id, ClusterEventKind::Created, t(0)))
            .unwrap();
        r.append_cluster_event(&ClusterEvent::new(
            &c.id,
            ClusterEventKind::Activated,
            t(10),
        ))
        .unwrap();
        let events = r.cluster_events(&c.id, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, ClusterEventKind::Activated, "newest first");

        // The history is append-only at the engine level.
        assert!(
            r.conn
                .execute("UPDATE cluster_event SET kind = 'merge'", [])
                .is_err()
        );
        assert!(r.conn.execute("DELETE FROM cluster_event", []).is_err());
    }

    #[test]
    fn membership_supersedes_and_an_abstention_is_a_row_of_its_own() {
        let mut r = Repository::open_in_memory().unwrap();
        let e = an_emitter(&mut r, 433.92e6);
        let a = a_cluster(&mut r, t(0));
        let b = a_cluster(&mut r, t(1));

        assert!(r.emitter_cluster(e).unwrap().is_none(), "never looked");

        for (cluster, reason, when) in [
            (Some(a.id.clone()), "seeded", 1),
            (Some(b.id.clone()), "reassigned", 2),
            (None, "ambiguous", 3),
        ] {
            r.link_emitter_cluster(&EmitterClusterLink {
                emitter_id: e,
                cluster_id: cluster,
                t: t(when),
                reason: reason.into(),
                distance: Some(0.5),
            })
            .unwrap();
        }

        // The current answer is the latest: an explicit abstention, not a stale membership.
        let current = r.emitter_cluster(e).unwrap().unwrap();
        assert_eq!(current.cluster_id, None);
        assert_eq!(current.reason, "ambiguous");
        assert_eq!(r.emitter_cluster_id(e).unwrap(), None);
        assert_eq!(r.emitter_cluster_history(e, 10).unwrap().len(), 3);
        // ...and neither cluster still counts it.
        assert!(r.cluster_members(&a.id).unwrap().is_empty());
        assert!(r.cluster_members(&b.id).unwrap().is_empty());

        r.link_emitter_cluster(&EmitterClusterLink {
            emitter_id: e,
            cluster_id: Some(b.id.clone()),
            t: t(4),
            reason: "joined".into(),
            distance: Some(0.2),
        })
        .unwrap();
        assert_eq!(r.cluster_members(&b.id).unwrap(), vec![e]);
        assert_eq!(r.cluster_member_count(&a.id).unwrap(), 0);
        assert_eq!(r.clustered_emitters().unwrap().len(), 1);

        assert!(
            r.conn
                .execute("UPDATE emitter_cluster SET cluster_id = NULL", [])
                .is_err(),
            "membership is append-only"
        );
    }

    #[test]
    fn a_merged_cluster_resolves_to_its_survivor_so_an_old_link_still_reads() {
        let mut r = Repository::open_in_memory().unwrap();
        let e = an_emitter(&mut r, 433.92e6);
        let survivor = a_cluster(&mut r, t(0));
        let mut loser = a_cluster(&mut r, t(1));

        r.link_emitter_cluster(&EmitterClusterLink {
            emitter_id: e,
            cluster_id: Some(loser.id.clone()),
            t: t(2),
            reason: "joined".into(),
            distance: None,
        })
        .unwrap();

        loser.state = ClusterState::Merged;
        loser.merged_into = Some(survivor.id.clone());
        loser.updated_at = t(3);
        r.put_cluster(&loser).unwrap();

        assert_eq!(r.live_cluster_id(&loser.id).unwrap(), survivor.id);
        assert_eq!(r.emitter_cluster_id(e).unwrap(), Some(survivor.id.clone()));
        // The loser's row is kept, not deleted: its history stays readable.
        assert!(r.cluster_opt(&loser.id).unwrap().is_some());
        assert!(
            !r.open_clusters().unwrap().iter().any(|c| c.id == loser.id),
            "a merged cluster takes no new members"
        );
        // An id that was never stored resolves to itself rather than failing.
        let stranger = new_cluster_id();
        assert_eq!(r.live_cluster_id(&stranger).unwrap(), stranger);
        assert!(r.cluster_opt("not-a-cluster").unwrap().is_none());
        assert!(r.cluster("not-a-cluster").is_err());
    }

    #[test]
    fn a_membership_row_is_checked_before_it_is_stored() {
        let mut r = Repository::open_in_memory().unwrap();
        let e = an_emitter(&mut r, 433.92e6);
        for bad in [
            EmitterClusterLink {
                emitter_id: e,
                cluster_id: Some("pocsag-1200".into()),
                t: t(1),
                reason: "joined".into(),
                distance: None,
            },
            EmitterClusterLink {
                emitter_id: e,
                cluster_id: None,
                t: t(1),
                reason: "  ".into(),
                distance: None,
            },
            EmitterClusterLink {
                emitter_id: e,
                cluster_id: None,
                t: t(1),
                reason: "joined".into(),
                distance: Some(f64::NAN),
            },
        ] {
            assert!(r.link_emitter_cluster(&bad).is_err(), "{bad:?}");
        }
    }
}
