//! Identity/content gating of decodes and tags, and the audited identity reclassification
//! (T-036, legal guardrail). Rules: [`crate::cluster`].

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use uuid::Uuid;

use super::cluster::{derived_identity_class, live_id, load_row};
use super::inventory::emitter_id_by_identity;
use super::{RepoError, Repository, blob, bodies, body_by_id, enum_parse, enum_text};
use crate::cluster::{
    IdentityAccess, IdentityReclassification, InventoryIdentity, class_rank, most_restrictive,
    never_openable, tag_in_vocabulary, tag_is_identity_free,
};
use crate::content::ContentClass;
use crate::decode::{Decode, DecodeView, WITHHELD_LABEL};
use crate::emitter::DecodedIdentity;
use crate::ids::{DecodeId, EmitterId};
use crate::time::Timestamp;

/// Shortest metadata value treated as a possible identifier when checking labels.
const MIN_LABEL_SECRET_LEN: usize = 3;

/// The audited reclassification chain of an identity, folded to `(old, new)`: the most
/// restrictive class it was ever opened from, and the latest class it was opened to. So
/// metadata-only → own-key-decrypted → unrestricted still maps metadata-only sources.
fn reclassification(
    conn: &Connection,
    identity: &DecodedIdentity,
) -> Result<Option<(ContentClass, ContentClass)>, RepoError> {
    let rows: Vec<(String, String)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT old_class, new_class FROM identity_reclassification \
             WHERE identity_scheme = ?1 AND identity_value = ?2 ORDER BY reclass_id",
        )?;
        stmt.query_map(params![identity.scheme.as_string(), identity.value], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<Result<_, _>>()?
    };
    let mut folded: Option<(ContentClass, ContentClass)> = None;
    for (old, new) in rows {
        let (old, new): (ContentClass, ContentClass) = (enum_parse(old)?, enum_parse(new)?);
        folded = Some(match folded {
            Some((o, _)) => (most_restrictive(o, old), new),
            None => (old, new),
        });
    }
    Ok(folded)
}

/// A source class as it counts for an identity after an audited reclassification `(old, new)`:
/// a source no more restrictive than `old` counts as at most `new`; restricted-cellular and
/// restricted-paging sources are never opened.
fn opened(class: ContentClass, reclass: Option<(ContentClass, ContentClass)>) -> ContentClass {
    match reclass {
        Some((old, new)) if !never_openable(class) && class_rank(class) <= class_rank(old) => {
            if class_rank(class) < class_rank(new) {
                class
            } else {
                new
            }
        }
        _ => class,
    }
}

/// The class of a source claim for `identity` after any audited reclassification.
pub(super) fn claim_class(
    conn: &Connection,
    identity: &DecodedIdentity,
    class: ContentClass,
) -> Result<ContentClass, RepoError> {
    Ok(opened(class, reclassification(conn, identity)?))
}

/// Identity class of a decoded identity for decode output: the most restrictive of every decode
/// naming it and of the class on the emitter holding it, after any audited reclassification.
/// `None` when nothing classifies it (withheld, fail closed).
fn decode_identity_class(
    conn: &Connection,
    identity: &DecodedIdentity,
) -> Result<Option<ContentClass>, RepoError> {
    let reclass = reclassification(conn, identity)?;
    let mut classes: Vec<ContentClass> = {
        let mut stmt = conn.prepare_cached(
            "SELECT content_class FROM decode WHERE identity_scheme = ?1 AND identity_value = ?2",
        )?;
        stmt.query_map(params![identity.scheme.as_string(), identity.value], |r| {
            r.get::<_, String>(0)
        })?
        .map(|c| c.map(|c| ContentClass::parse_fail_closed(Some(&c))))
        .collect::<Result<_, _>>()?
    };
    let holder: Option<Option<String>> = conn
        .prepare_cached(
            "SELECT identity_class FROM emitter WHERE identity_scheme = ?1 AND identity_value = ?2",
        )?
        .query_row(params![identity.scheme.as_string(), identity.value], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(Some(c)) = holder {
        classes.push(ContentClass::parse_fail_closed(Some(&c)));
    }
    Ok(classes
        .into_iter()
        .map(|c| opened(c, reclass))
        .reduce(most_restrictive))
}

/// Metadata leaves (strings, numbers) long enough to be identifiers.
fn metadata_secrets(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) if s.chars().count() >= MIN_LABEL_SECRET_LEN => out.push(s.to_lowercase()),
        Value::Number(n) if n.to_string().len() >= MIN_LABEL_SECRET_LEN => out.push(n.to_string()),
        Value::Array(items) => items.iter().for_each(|v| metadata_secrets(v, out)),
        Value::Object(map) => map.values().for_each(|v| metadata_secrets(v, out)),
        _ => {}
    }
}

fn is_empty_metadata(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// The output view of a decode for `access` (rules: [`crate::cluster`], fail closed).
fn gate_decode(
    conn: &Connection,
    mut decode: Decode,
    access: IdentityAccess,
) -> Result<DecodeView, RepoError> {
    let (identity, identity_shown) = match &decode.identity {
        None => (InventoryIdentity::None, true),
        Some(d) => {
            let class = decode_identity_class(conn, d)?;
            match (access.reveals(class), class) {
                (true, Some(class)) => (
                    InventoryIdentity::Clear {
                        identity: d.clone(),
                        class,
                    },
                    true,
                ),
                _ => (
                    InventoryIdentity::Withheld {
                        scheme: d.scheme.clone(),
                        class,
                    },
                    false,
                ),
            }
        }
    };
    let detail_shown = identity_shown && access.reveals(Some(decode.content_class));
    let mut metadata_withheld = false;
    let mut content_withheld = false;
    let mut labels_withheld = false;
    if !detail_shown {
        let mut secrets = Vec::new();
        if let Some(d) = &decode.identity
            && !d.value.is_empty()
        {
            secrets.push(d.value.to_lowercase());
        }
        metadata_secrets(&decode.metadata, &mut secrets);
        if let Some(content) = &decode.content {
            metadata_secrets(content, &mut secrets);
        }
        for label in [
            &mut decode.frame_model,
            &mut decode.decoder_id,
            &mut decode.decoder_version,
        ] {
            let lower = label.to_lowercase();
            if secrets.iter().any(|s| lower.contains(s.as_str())) {
                *label = WITHHELD_LABEL.to_owned();
                labels_withheld = true;
            }
        }
        metadata_withheld = !is_empty_metadata(&decode.metadata);
        decode.metadata = Value::Object(serde_json::Map::new());
        // Content was admitted by the row's class, but it may name an identity that is withheld.
        content_withheld = decode.content.take().is_some();
    }
    if !identity_shown {
        decode.identity = None;
    }
    Ok(DecodeView {
        decode,
        identity,
        metadata_withheld,
        content_withheld,
        labels_withheld,
    })
}

/// An emitter's decoded identity class as the inventory gate sees it: `None` without a decoded
/// identity, `Some(None)` for an unclassified one.
pub(super) fn emitter_identity_class(
    conn: &Connection,
    id: EmitterId,
) -> Result<Option<Option<ContentClass>>, RepoError> {
    let row = load_row(conn, id)?;
    let Some(identity) = row.identity else {
        return Ok(None);
    };
    Ok(Some(match row.class {
        Some(c) => Some(c),
        None => derived_identity_class(conn, id, &identity)?,
    }))
}

/// Whether no access level reveals an identity of this class (`None` = unclassified): its tags
/// must come from the controlled vocabulary (T-038).
pub(super) fn vocabulary_only(class: Option<ContentClass>) -> bool {
    !IdentityAccess::OwnTrafficAuthorised.reveals(class)
}

/// The refusal for a tag outside the vocabulary on such an identity (names neither tag nor value).
pub(super) fn vocabulary_refusal() -> RepoError {
    RepoError::Invalid(
        "a tag on an emitter whose identity is restricted, metadata-only or unclassified must \
         come from the controlled tag vocabulary (hk_model::TAG_VOCABULARY; T-038)"
            .into(),
    )
}

/// T-040: when `id`'s identity is one no access level reveals, deletes every stored tag outside the
/// vocabulary. Called in the transaction of each write that can tighten an emitter's class (an
/// identity arriving, a more restrictive source, a restricted decode linked, a merge), so tags
/// written before the tightening do not stay in the database hidden only on read. Fail closed and
/// not reversible: a later audited reclassification does not bring them back. Rows merged into
/// `id` (whose tags were copied onto it) are purged too. `id` must exist.
pub(super) fn purge_withheld_tags(conn: &Connection, id: EmitterId) -> Result<(), RepoError> {
    let Some(class) = emitter_identity_class(conn, id)? else {
        return Ok(());
    };
    if !vocabulary_only(class) {
        return Ok(());
    }
    let tags: Vec<([u8; 16], String)> = conn
        .prepare_cached(
            "SELECT emitter_id, tag FROM emitter_tag WHERE emitter_id = ?1 \
             OR emitter_id IN (SELECT emitter_id FROM emitter WHERE merged_into = ?1)",
        )?
        .query_map([blob(id)], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (owner, tag) in tags.iter().filter(|(_, t)| !tag_in_vocabulary(t)) {
        conn.prepare_cached("DELETE FROM emitter_tag WHERE emitter_id = ?1 AND tag = ?2")?
            .execute(params![owner, tag])?;
    }
    Ok(())
}

/// The emitter a user tag write on `emitter_id` applies to (its live survivor, else the id as
/// given), refusing a tag outside the vocabulary when that emitter's identity is one no access
/// level reveals (T-038/T-040). The refusal depends only on the class, never on stored tags.
pub(super) fn tag_write_target(
    conn: &Connection,
    emitter_id: EmitterId,
    tag: &str,
) -> Result<EmitterId, RepoError> {
    let Some(live) = live_id(conn, emitter_id)? else {
        return Ok(emitter_id);
    };
    if !tag_in_vocabulary(tag)
        && let Some(class) = emitter_identity_class(conn, live)?
        && vocabulary_only(class)
    {
        return Err(vocabulary_refusal());
    }
    Ok(live)
}

/// Write-time tag rule (T-036/T-038, [`crate::cluster`]): with an identity no access level reveals
/// (`None` = unclassified), every tag must be in the vocabulary; with an `own-key-decrypted` one,
/// identity-free and not containing the value.
pub(super) fn check_tags(
    tags: &mut dyn Iterator<Item = &String>,
    identity: Option<(&DecodedIdentity, Option<ContentClass>)>,
) -> Result<(), RepoError> {
    let Some((identity, class)) = identity else {
        return Ok(());
    };
    if class == Some(ContentClass::Unrestricted) {
        return Ok(());
    }
    if vocabulary_only(class) {
        for tag in tags {
            if !tag_in_vocabulary(tag) {
                return Err(vocabulary_refusal());
            }
        }
        return Ok(());
    }
    let value = identity.value.to_lowercase();
    for tag in tags {
        if !tag_is_identity_free(tag) || (!value.is_empty() && tag.to_lowercase().contains(&value))
        {
            return Err(RepoError::Invalid(
                "a tag on an emitter with a withheld identity must be an identity-free label \
                 (letters only, no digits or hex runs; T-036)"
                    .into(),
            ));
        }
    }
    Ok(())
}

fn refused(reason: &'static str) -> RepoError {
    RepoError::ReclassificationRefused { reason }
}

impl Repository {
    /// One decode, gated at [`IdentityAccess::Standard`] (T-036, [`crate::cluster`]): a withheld
    /// row reads with `identity: None`, metadata `{}` and identifier-bearing labels withheld.
    /// See [`Self::decode_with_access`].
    pub fn decode(&self, id: DecodeId) -> Result<Decode, RepoError> {
        Ok(self
            .decode_with_access(id, IdentityAccess::Standard)?
            .decode)
    }

    /// One decode gated for `access` (fail closed).
    pub fn decode_with_access(
        &self,
        id: DecodeId,
        access: IdentityAccess,
    ) -> Result<DecodeView, RepoError> {
        let tx = self.read_tx()?;
        let decode = body_by_id(
            &tx,
            "SELECT body FROM decode WHERE decode_id = ?1",
            blob(id),
            "decode",
        )?;
        gate_decode(&tx, decode, access)
    }

    /// One decode as stored, identity and metadata in clear whatever the class. Crate-private:
    /// every path out of the process goes through a gated read.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn decode_ungated(&self, id: DecodeId) -> Result<Decode, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM decode WHERE decode_id = ?1",
            blob(id),
            "decode",
        )
    }

    /// Decodes naming an identity, oldest first, gated at [`IdentityAccess::Standard`]: empty
    /// unless that identity would be shown (a lookup must not confirm a withheld identity).
    pub fn decodes_for_identity(
        &self,
        identity: &DecodedIdentity,
    ) -> Result<Vec<Decode>, RepoError> {
        Ok(self
            .decodes_for_identity_with_access(identity, IdentityAccess::Standard)?
            .into_iter()
            .map(|v| v.decode)
            .collect())
    }

    /// Decodes naming an identity, oldest first, gated for `access`: empty unless `access`
    /// reveals the identity; each row's detail is gated by its own class.
    pub fn decodes_for_identity_with_access(
        &self,
        identity: &DecodedIdentity,
        access: IdentityAccess,
    ) -> Result<Vec<DecodeView>, RepoError> {
        let tx = self.read_tx()?;
        if !access.reveals(decode_identity_class(&tx, identity)?) {
            return Ok(Vec::new());
        }
        let rows: Vec<Decode> = bodies(
            &tx,
            "SELECT body FROM decode WHERE identity_scheme = ?1 AND identity_value = ?2 \
             ORDER BY t, decode_id",
            params![identity.scheme.as_string(), identity.value],
        )?;
        rows.into_iter()
            .map(|d| gate_decode(&tx, d, access))
            .collect()
    }

    /// Opens an emitter's decoded identity to `new_class` after the user asserts it is their own
    /// traffic (T-036; rules in [`crate::cluster`]). Refused
    /// ([`RepoError::ReclassificationRefused`], never naming the identity) unless `authorisation`
    /// is [`IdentityAccess::OwnTrafficAuthorised`], `new_class` is `own-key-decrypted` or
    /// `unrestricted` and less restrictive than the identity's current class, and that class is
    /// known and was never restricted-cellular or restricted-paging. Appends an audit row and
    /// sets the emitter's identity class; stored decodes and measurements are not rewritten. A
    /// merged emitter id stands for its survivor. Time is the host clock.
    pub fn reclassify_identity(
        &mut self,
        emitter: EmitterId,
        new_class: ContentClass,
        authorisation: IdentityAccess,
        reason: &str,
        author: &str,
    ) -> Result<IdentityReclassification, RepoError> {
        if authorisation != IdentityAccess::OwnTrafficAuthorised {
            return Err(refused(
                "opening an identity needs the own-traffic authorisation",
            ));
        }
        if !matches!(
            new_class,
            ContentClass::OwnKeyDecrypted | ContentClass::Unrestricted
        ) {
            return Err(refused(
                "an identity can only be opened to own-key-decrypted or unrestricted",
            ));
        }
        if reason.trim().is_empty() || author.trim().is_empty() {
            return Err(refused("a reclassification needs a reason and an author"));
        }
        let t = Timestamp::now();
        let tx = self.write_tx()?;
        let live = live_id(&tx, emitter)?.ok_or_else(|| RepoError::NotFound {
            kind: "emitter",
            id: emitter.to_string(),
        })?;
        let row = load_row(&tx, live)?;
        let Some(identity) = row.identity else {
            return Err(refused("the emitter has no decoded identity"));
        };
        // Every source ever recorded for the identity, unmapped: a restricted source anywhere
        // makes it unopenable, whatever an earlier reclassification said.
        let ever_restricted: bool = tx
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM decode WHERE identity_scheme = ?1 \
                 AND identity_value = ?2 AND content_class IN \
                 ('restricted-cellular', 'restricted-paging'))",
            )?
            .query_row(params![identity.scheme.as_string(), identity.value], |r| {
                r.get(0)
            })?;
        let base = match row.class {
            Some(c) => Some(c),
            None => derived_identity_class(&tx, live, &identity)?,
        };
        let current = match (base, decode_identity_class(&tx, &identity)?) {
            (Some(a), Some(b)) => Some(most_restrictive(a, b)),
            (a, b) => a.or(b),
        };
        let Some(current) = current else {
            return Err(refused("an unclassified identity cannot be opened"));
        };
        if ever_restricted || never_openable(current) {
            return Err(refused(
                "restricted-cellular and restricted-paging identities can never be opened",
            ));
        }
        if class_rank(new_class) >= class_rank(current) {
            return Err(refused("the new class does not open the identity"));
        }
        // The emitter row must be the identity's holder (identities are unique per emitter).
        if emitter_id_by_identity(&tx, &identity)? != Some(live) {
            return Err(RepoError::Invalid(
                "identity holder changed during reclassification".into(),
            ));
        }
        tx.prepare_cached(
            "INSERT INTO identity_reclassification (emitter_id, identity_scheme, identity_value, \
             old_class, new_class, authorisation, reason, author, t) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'own-traffic-authorised', ?6, ?7, ?8)",
        )?
        .execute(params![
            blob(live),
            identity.scheme.as_string(),
            identity.value,
            enum_text(&current)?,
            enum_text(&new_class)?,
            reason,
            author,
            t.as_unix_nanos()
        ])?;
        tx.prepare_cached("UPDATE emitter SET identity_class = ?1 WHERE emitter_id = ?2")?
            .execute(params![enum_text(&new_class)?, blob(live)])?;
        // Opening never tightens today; kept so any class this write lands on is purged (T-040).
        purge_withheld_tags(&tx, live)?;
        tx.commit()?;
        Ok(IdentityReclassification {
            emitter_id: live,
            scheme: identity.scheme,
            old_class: current,
            new_class,
            author: author.to_owned(),
            reason: Some(reason.to_owned()),
            t,
        })
    }

    /// The reclassification audit of an emitter (rows written while it held the identity), oldest
    /// first. The reason is shown only when `access` reveals the new class.
    pub fn identity_reclassifications(
        &self,
        emitter: EmitterId,
        access: IdentityAccess,
    ) -> Result<Vec<IdentityReclassification>, RepoError> {
        type Raw = ([u8; 16], String, String, String, String, String, i64);
        let rows: Vec<Raw> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT emitter_id, identity_scheme, old_class, new_class, author, reason, t \
                 FROM identity_reclassification WHERE emitter_id = ?1 ORDER BY reclass_id",
            )?;
            stmt.query_map([blob(emitter)], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?
            .collect::<Result<_, _>>()?
        };
        rows.into_iter()
            .map(|(id, scheme, old, new, author, reason, t)| {
                let new_class: ContentClass = enum_parse(new)?;
                Ok(IdentityReclassification {
                    emitter_id: EmitterId::from_uuid(Uuid::from_bytes(id)),
                    scheme: scheme.parse().map_err(RepoError::Invalid)?,
                    old_class: enum_parse(old)?,
                    new_class,
                    author,
                    reason: access.reveals(Some(new_class)).then_some(reason),
                    t: Timestamp::from_unix_nanos(t),
                })
            })
            .collect()
    }
}
