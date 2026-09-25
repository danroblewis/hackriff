//! **The confirm-decision counter** (T-575; ADR-0022 §8).
//!
//! `ConfirmPolicy.synthesized`'s 24-bit threshold delivers the user's budget — at most one wrong
//! Confirmed emitter per unattended week — **only at the decision rate it was derived for**
//! (`assumed_decisions_per_week`, 20 000). The cross-job multiplicity is priced once, as a
//! constant in the threshold, and deliberately *not* as a running per-session charge: that would
//! make the same squitter worth less on a device that has been on longer. So the assumption
//! becomes a runtime obligation, and this is its evidence: every evaluation of a confirm rule is
//! recorded here, and the rolling seven-day count says whether the budget claim still holds.
//!
//! Durable on purpose. A counter held in memory would restart at zero with the process, and a
//! device that reboots daily would never see its own rate. One row per decision, in the same
//! transaction as the decision's own writes (so a rolled-back attach never counted); rows older
//! than the window are pruned on insert, so the table is bounded by the rate it measures.
//!
//! The clock is the **device's** wall clock at the moment of decision, not the analysed window's
//! capture time: a replay of last year's recording is decided *now*, and it is decisions per
//! device-week that the budget is stated in.

use rusqlite::{OptionalExtension, params};

use super::{RepoError, Repository};
use crate::time::Timestamp;

/// The rolling window, ADR-0022 §8: seven days, in nanoseconds.
pub const CONFIRM_DECISION_WINDOW_NS: i64 = 7 * 24 * 3600 * 1_000_000_000;

const ENSURE_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS confirm_decision (
    decision_id INTEGER PRIMARY KEY,
    rule        TEXT    NOT NULL,
    t           INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS confirm_decision_rule_t ON confirm_decision (rule, t);";

impl Repository {
    /// Records one evaluation of the confirm rule `rule` (its actor, e.g.
    /// `hk-pipeline/confirm-synth@2`) at `at`, prunes that rule's rows older than the window, and
    /// returns the rule's evaluations in the seven days ending at `at`, this one included.
    pub fn record_confirm_decision(&mut self, rule: &str, at: Timestamp) -> Result<u64, RepoError> {
        self.conn.execute_batch(ENSURE_TABLE)?;
        let t = at.as_unix_nanos();
        let since = t.saturating_sub(CONFIRM_DECISION_WINDOW_NS);
        let tx = self.write_tx()?;
        tx.execute(
            "INSERT INTO confirm_decision (rule, t) VALUES (?1, ?2)",
            params![rule, t],
        )?;
        tx.execute(
            "DELETE FROM confirm_decision WHERE rule = ?1 AND t <= ?2",
            params![rule, since],
        )?;
        let n: i64 = tx.query_row(
            "SELECT count(*) FROM confirm_decision WHERE rule = ?1 AND t > ?2 AND t <= ?3",
            params![rule, since, t],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// The evaluations of `rule` in the seven days ending at `at` (`(at − 7 d, at]`). 0 when none
    /// was ever recorded.
    pub fn confirm_decisions_in_week(&self, rule: &str, at: Timestamp) -> Result<u64, RepoError> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'confirm_decision'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(0);
        }
        let t = at.as_unix_nanos();
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM confirm_decision WHERE rule = ?1 AND t > ?2 AND t <= ?3",
            params![rule, t.saturating_sub(CONFIRM_DECISION_WINDOW_NS), t],
            |r| r.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: &str = "hk-pipeline/confirm-synth@2";
    const DAY: i64 = 24 * 3600 * 1_000_000_000;

    fn repo() -> Repository {
        Repository::open_in_memory().unwrap()
    }

    #[test]
    fn counts_a_rolling_seven_days_per_rule_and_prunes_what_left_it() {
        let mut r = repo();
        let t0 = 1_800_000_000 * 1_000_000_000i64;
        assert_eq!(
            r.confirm_decisions_in_week(RULE, Timestamp::from_unix_nanos(t0))
                .unwrap(),
            0,
            "never recorded"
        );
        for d in 0..7 {
            let n = r
                .record_confirm_decision(RULE, Timestamp::from_unix_nanos(t0 + d * DAY))
                .unwrap();
            assert_eq!(n, d as u64 + 1);
        }
        // Another rule is its own count.
        assert_eq!(
            r.record_confirm_decision("other", Timestamp::from_unix_nanos(t0))
                .unwrap(),
            1
        );
        // Exactly seven days after the first, the first has left the window.
        let n = r
            .record_confirm_decision(RULE, Timestamp::from_unix_nanos(t0 + 7 * DAY))
            .unwrap();
        assert_eq!(n, 7);
        assert_eq!(
            r.confirm_decisions_in_week(RULE, Timestamp::from_unix_nanos(t0 + 7 * DAY))
                .unwrap(),
            7
        );
        // Pruned, not merely excluded: a later read over an earlier instant cannot find it.
        assert_eq!(
            r.confirm_decisions_in_week(RULE, Timestamp::from_unix_nanos(t0))
                .unwrap(),
            0
        );
        // A quiet fortnight empties it.
        assert_eq!(
            r.confirm_decisions_in_week(RULE, Timestamp::from_unix_nanos(t0 + 21 * DAY))
                .unwrap(),
            0
        );
    }

    #[test]
    fn a_rolled_back_decision_was_never_counted() {
        let mut r = repo();
        let at = Timestamp::from_unix_nanos(1_800_000_000 * 1_000_000_000);
        r.begin_write_batch().unwrap();
        assert_eq!(r.record_confirm_decision(RULE, at).unwrap(), 1);
        r.rollback_write_batch().unwrap();
        assert_eq!(r.confirm_decisions_in_week(RULE, at).unwrap(), 0);
    }
}
