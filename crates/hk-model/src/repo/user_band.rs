//! User-adjusted band edges on an inventory entry (T-191): the `f_lo`/`f_hi` a user drew for an
//! emitter, stored beside its measured extent, which never changes because of it (blind detection
//! stays the source of truth; detection, tracking and entity resolution never read the override).
//!
//! - **One current override per emitter** (table `emitter_user_band`, migration 0006): setting
//!   replaces it, clearing removes it. The API audits both with old and new values.
//! - **Validation** ([`Repository::set_user_band`]): both edges finite, `0 < f_lo < f_hi`, width
//!   at most [`USER_BAND_MAX_WIDTH_HZ`], and the band overlapping the measured extent widened by
//!   [`USER_BAND_MAX_GAP_HZ`] on each side (an adjusted edge, not a different signal). `actor` is
//!   non-empty and `actor`/`reason` at most [`LIFECYCLE_TEXT_MAX`] bytes.
//! - **Live entries only.** A merged id stands for its survivor; a deleted entry is not found.
//! - **Merges** ([`carry_user_band`]): the survivor keeps an override if either entry had one;
//!   when both had one, the latest `set_at` wins (a tie keeps the survivor's). The absorbed
//!   entry's row stays as history, unreachable through the merged id.
//!
//! No content: edges, a time, the token fingerprint and a user note.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{LIFECYCLE_TEXT_MAX, RepoError, Repository, blob};
use crate::ids::EmitterId;
use crate::time::Timestamp;

/// Widest user band, Hz (twice the HackRF's 20 MHz dwell: a band edge, never a survey region).
pub const USER_BAND_MAX_WIDTH_HZ: f64 = 40e6;

/// How far outside the measured extent a user band may lie and still name the same signal, Hz.
pub const USER_BAND_MAX_GAP_HZ: f64 = 1e6;

/// A user's band override on an inventory entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserBand {
    /// The live emitter it belongs to.
    pub emitter_id: EmitterId,
    /// Lower edge, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz.
    pub f_hi_hz: f64,
    /// When it was set.
    pub set_at: Timestamp,
    /// Who set it (the API token fingerprint).
    pub actor: String,
    /// The user's note, if any.
    pub reason: Option<String>,
}

fn load(conn: &Connection, id: EmitterId) -> Result<Option<UserBand>, RepoError> {
    Ok(conn
        .prepare_cached(
            "SELECT f_lo, f_hi, set_at, actor, reason FROM emitter_user_band WHERE emitter_id = ?1",
        )?
        .query_row([blob(id)], |r| {
            Ok(UserBand {
                emitter_id: id,
                f_lo_hz: r.get(0)?,
                f_hi_hz: r.get(1)?,
                set_at: Timestamp::from_unix_nanos(r.get(2)?),
                actor: r.get(3)?,
                reason: r.get(4)?,
            })
        })
        .optional()?)
}

fn store(conn: &Connection, id: EmitterId, b: &UserBand) -> Result<(), RepoError> {
    conn.prepare_cached(
        "INSERT INTO emitter_user_band (emitter_id, f_lo, f_hi, set_at, actor, reason) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT (emitter_id) DO UPDATE SET f_lo = excluded.f_lo, f_hi = excluded.f_hi, \
         set_at = excluded.set_at, actor = excluded.actor, reason = excluded.reason",
    )?
    .execute(params![
        blob(id),
        b.f_lo_hz,
        b.f_hi_hz,
        b.set_at.as_unix_nanos(),
        b.actor,
        b.reason
    ])?;
    Ok(())
}

/// The live, non-deleted emitter `id` stands for.
fn live_listed(conn: &Connection, id: EmitterId) -> Result<EmitterId, RepoError> {
    let not_found = || RepoError::NotFound {
        kind: "emitter",
        id: id.to_string(),
    };
    let live = super::cluster::live_id(conn, id)?.ok_or_else(not_found)?;
    if super::lifecycle::state_of(conn, live)? == crate::emitter::LifecycleState::Deleted {
        return Err(not_found());
    }
    Ok(live)
}

/// Merge rule: `from`'s override moves onto `into` when `into` has none or an older one.
pub(super) fn carry_user_band(
    conn: &Connection,
    from: EmitterId,
    into: EmitterId,
) -> Result<(), RepoError> {
    let Some(theirs) = load(conn, from)? else {
        return Ok(());
    };
    if load(conn, into)?.is_none_or(|ours| theirs.set_at > ours.set_at) {
        store(conn, into, &theirs)?;
    }
    Ok(())
}

fn check(f_lo: f64, f_hi: f64, actor: &str, reason: Option<&str>) -> Result<(), RepoError> {
    let bad = |why: String| Err(RepoError::Invalid(format!("user band: {why}")));
    if !(f_lo.is_finite() && f_hi.is_finite()) {
        return bad("f_lo and f_hi must be finite numbers".into());
    }
    if !(f_lo > 0.0 && f_hi > f_lo) {
        return bad("the edges must satisfy 0 < f_lo < f_hi".into());
    }
    if f_hi - f_lo > USER_BAND_MAX_WIDTH_HZ {
        return bad(format!("width must be at most {USER_BAND_MAX_WIDTH_HZ} Hz"));
    }
    if actor.trim().is_empty() || actor.len() > LIFECYCLE_TEXT_MAX {
        return bad(format!(
            "actor must be non-empty and at most {LIFECYCLE_TEXT_MAX} bytes"
        ));
    }
    if reason.is_some_and(|r| r.trim().is_empty() || r.len() > LIFECYCLE_TEXT_MAX) {
        return bad(format!(
            "reason must be non-empty and at most {LIFECYCLE_TEXT_MAX} bytes"
        ));
    }
    Ok(())
}

impl Repository {
    /// Sets the user band on the live entry `id` stands for (rules in the module docs); returns
    /// `(previous, stored)`. [`RepoError::NotFound`] for an unknown or deleted entry,
    /// [`RepoError::Invalid`] for a band that breaks the rules. The measured band is untouched.
    pub fn set_user_band(
        &mut self,
        id: EmitterId,
        f_lo_hz: f64,
        f_hi_hz: f64,
        actor: &str,
        reason: Option<&str>,
        t: Timestamp,
    ) -> Result<(Option<UserBand>, UserBand), RepoError> {
        check(f_lo_hz, f_hi_hz, actor, reason)?;
        let tx = self.write_tx()?;
        let live = live_listed(&tx, id)?;
        let (m_lo, m_hi): (f64, f64) = tx
            .prepare_cached("SELECT f_lo, f_hi FROM emitter WHERE emitter_id = ?1")?
            .query_row([blob(live)], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if f_hi_hz < m_lo - USER_BAND_MAX_GAP_HZ || f_lo_hz > m_hi + USER_BAND_MAX_GAP_HZ {
            return Err(RepoError::Invalid(format!(
                "user band: must overlap the measured band or lie within {USER_BAND_MAX_GAP_HZ} Hz \
                 of it"
            )));
        }
        let previous = load(&tx, live)?;
        let band = UserBand {
            emitter_id: live,
            f_lo_hz,
            f_hi_hz,
            set_at: t,
            actor: actor.to_owned(),
            reason: reason.map(str::to_owned),
        };
        store(&tx, live, &band)?;
        tx.commit()?;
        Ok((previous, band))
    }

    /// Clears the user band on the live entry `id` stands for; returns the cleared override
    /// (`None` when there was none). [`RepoError::NotFound`] for an unknown or deleted entry.
    pub fn clear_user_band(&mut self, id: EmitterId) -> Result<Option<UserBand>, RepoError> {
        let tx = self.write_tx()?;
        let live = live_listed(&tx, id)?;
        let previous = load(&tx, live)?;
        tx.prepare_cached("DELETE FROM emitter_user_band WHERE emitter_id = ?1")?
            .execute([blob(live)])?;
        tx.commit()?;
        Ok(previous)
    }

    /// The current user band of the entry `id` stands for (merged ids follow their survivor;
    /// deleted entries keep theirs readable), or `None`.
    pub fn user_band(&self, id: EmitterId) -> Result<Option<UserBand>, RepoError> {
        let live = self.live_emitter_id(id)?;
        load(&self.conn, live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{Fingerprint, Sighting};
    use crate::emitter::{LifecycleAuthor, LifecycleState, LinkTarget};
    use crate::ids::TrackId;
    use crate::region::TimeRange;

    fn t(s: f64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + (s * 1e9) as i64)
    }

    fn track(f: f64, bw: f64, a: f64, b: f64) -> Sighting {
        Sighting {
            source: LinkTarget::Track(TrackId::new()),
            seen: TimeRange::new(t(a), t(b)),
            count: 3,
            f_center_hz: f,
            bandwidth_hz: bw,
            fingerprint: Some(Fingerprint::new(f, bw)),
            identity: None,
            context: None,
            classification: None,
            tags: Vec::new(),
        }
    }

    #[test]
    fn t191_set_replace_clear_keep_the_measured_band_and_survive_restart() {
        let dir = std::env::temp_dir().join(format!(
            "hk-model-t191-{}-{}",
            std::process::id(),
            t(0.0).as_unix_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hackriff.db");
        let id = {
            let mut r = Repository::open(&path).unwrap();
            let id = r
                .record_sighting(&track(101.3e6, 200e3, 0.0, 5.0), None)
                .unwrap()
                .emitter_id;
            assert_eq!(r.user_band(id).unwrap(), None);
            let (prev, b) = r
                .set_user_band(id, 101.21e6, 101.39e6, "tok-a", Some("tighter"), t(10.0))
                .unwrap();
            assert_eq!(prev, None);
            assert_eq!(
                (b.f_lo_hz, b.f_hi_hz, b.set_at),
                (101.21e6, 101.39e6, t(10.0))
            );
            let (prev, _) = r
                .set_user_band(id, 101.22e6, 101.38e6, "tok-b", None, t(11.0))
                .unwrap();
            assert_eq!(prev.unwrap().actor, "tok-a");
            let e = r.emitter(id).unwrap();
            assert_eq!(
                (e.f_center_hz, e.bandwidth_hz),
                (101.3e6, 200e3),
                "the measured band is never overwritten"
            );
            id
        };
        // Restart: the override is still there.
        let mut r = Repository::open(&path).unwrap();
        let b = r.user_band(id).unwrap().expect("persisted");
        assert_eq!(
            (b.f_lo_hz, b.f_hi_hz, b.actor.as_str(), b.reason, b.set_at),
            (101.22e6, 101.38e6, "tok-b", None, t(11.0))
        );
        assert_eq!(r.clear_user_band(id).unwrap().unwrap().f_lo_hz, 101.22e6);
        assert_eq!(r.clear_user_band(id).unwrap(), None);
        assert_eq!(r.user_band(id).unwrap(), None);
        drop(r);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn t191_invalid_bands_and_unknown_or_deleted_entries_are_refused() {
        let mut r = Repository::open_in_memory().unwrap();
        // Measured extent: 433.902e6 ..= 433.938e6.
        let id = r
            .record_sighting(&track(433.92e6, 36e3, 0.0, 5.0), None)
            .unwrap()
            .emitter_id;
        let invalid = |r: &mut Repository, lo: f64, hi: f64, actor: &str, why: Option<&str>| {
            matches!(
                r.set_user_band(id, lo, hi, actor, why, t(1.0)),
                Err(RepoError::Invalid(_))
            )
        };
        for (lo, hi) in [
            (f64::NAN, 433.93e6),
            (433.91e6, f64::INFINITY),
            (0.0, 433.93e6),
            (-1.0, 433.93e6),
            (433.93e6, 433.93e6),
            (433.94e6, 433.91e6),
            (420e6, 460.000_001e6), // wider than 40 MHz (though it covers the signal)
            (435.0e6, 436.0e6),     // starts 1.062 MHz above the measured upper edge
            (431.0e6, 432.9e6),     // ends 1.002 MHz below the measured lower edge
        ] {
            assert!(invalid(&mut r, lo, hi, "tok", None), "{lo}..{hi}");
        }
        assert!(
            invalid(&mut r, 433.91e6, 433.93e6, " ", None),
            "blank actor"
        );
        assert!(
            invalid(&mut r, 433.91e6, 433.93e6, "tok", Some("")),
            "empty reason"
        );
        let long = "x".repeat(LIFECYCLE_TEXT_MAX + 1);
        assert!(invalid(&mut r, 433.91e6, 433.93e6, "tok", Some(&long)));
        assert_eq!(r.user_band(id).unwrap(), None, "nothing stored");

        // Limits are inclusive: exactly 40 MHz wide, and a band ending exactly 1 MHz below.
        r.set_user_band(id, 414e6, 454e6, "tok", None, t(1.0))
            .unwrap();
        r.set_user_band(id, 432.0e6, 432.902e6, "tok", None, t(2.0))
            .unwrap();
        // Adjacent but disjoint within the gap is accepted.
        r.set_user_band(id, 433.95e6, 434.0e6, "tok", None, t(3.0))
            .unwrap();

        let unknown = EmitterId::new();
        assert!(matches!(
            r.set_user_band(unknown, 1e6, 2e6, "tok", None, t(1.0)),
            Err(RepoError::NotFound { .. })
        ));
        assert!(matches!(
            r.clear_user_band(unknown),
            Err(RepoError::NotFound { .. })
        ));
        r.change_emitter_lifecycle(
            id,
            LifecycleState::Deleted,
            LifecycleAuthor::User,
            "tok",
            "gone",
            t(4.0),
        )
        .unwrap();
        assert!(matches!(
            r.set_user_band(id, 433.91e6, 433.93e6, "tok", None, t(5.0)),
            Err(RepoError::NotFound { .. })
        ));
        assert!(matches!(
            r.clear_user_band(id),
            Err(RepoError::NotFound { .. })
        ));
        assert!(
            r.user_band(id).unwrap().is_some(),
            "a deleted entry's band stays readable"
        );
    }
}
