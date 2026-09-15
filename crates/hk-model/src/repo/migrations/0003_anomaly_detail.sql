-- T-122 (ADR-0012 §7, §9): novelty alarm evidence and lifecycle beside the append-only anomaly rows.
-- The anomaly, its status history (open/resolved/dismissed with transition notes) and its
-- Explanations stay in the 0001 tables; this row holds the `AlarmDetail` evidence and the current
-- lifecycle state the alarm engine resumes from. Retention follows the anomaly (ADR-0006).

-- One row per alarm anomaly. `body` is the full `repo::alarms::AlarmRow` JSON; the columns index it.
-- Mutable (the lifecycle moves); every transition also appends an `anomaly_status` entry.
CREATE TABLE anomaly_detail (
    anomaly_id      BLOB    PRIMARY KEY REFERENCES anomaly (anomaly_id),
    -- Dedupe key `kind=…;site=…;subject=…` (ADR-0012 §7.2).
    alarm_key       TEXT    NOT NULL,
    kind            TEXT    NOT NULL CHECK (kind IN ('level-above-baseline', 'new-emitter',
                        'busier-than-usual', 'quieter-than-usual', 'change-point')),
    site_id         BLOB    NOT NULL CHECK (length(site_id) = 16),
    state           TEXT    NOT NULL CHECK (state IN ('open', 'cleared', 'dismissed', 'explained')),
    last_transition TEXT    NOT NULL CHECK (last_transition IN ('raised', 'held', 'reopened',
                        'cleared', 'dismissed', 'undismissed', 'explained')),
    raised_at       INTEGER NOT NULL,
    last_t          INTEGER NOT NULL,
    reopen_count    INTEGER NOT NULL CHECK (reopen_count >= 0),
    cleared_at      INTEGER,
    dismissed_until INTEGER,
    f_lo            REAL    NOT NULL,
    f_hi            REAL    NOT NULL CHECK (f_hi >= f_lo),
    body            TEXT    NOT NULL
) WITHOUT ROWID;
CREATE INDEX idx_anomaly_detail_key   ON anomaly_detail (alarm_key, raised_at);
CREATE INDEX idx_anomaly_detail_state ON anomaly_detail (state, last_t);
CREATE TRIGGER anomaly_detail_no_delete BEFORE DELETE ON anomaly_detail
    BEGIN SELECT RAISE(ABORT, 'alarm details follow their anomaly and are not deleted'); END;
