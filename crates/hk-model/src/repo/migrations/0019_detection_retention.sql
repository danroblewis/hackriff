-- T-904: retention for per-frame Detection rows (docs/07 §2.9, "aged by quota … but summarised
-- into Emitter/Track before deletion"). Tracks, emitters, observations and every link stay
-- durable; only per-frame `detection` rows (and their `track_detection` links) age out, and each
-- one is first folded into a `detection_rollup` row of its track. `repo/retention.rs` holds the
-- policy and says which rows are never pruned and why.

-- The age scan and the watermark (`max(t_end)`) read this; without it a prune pass is a full
-- table scan. `(t_end, detection_id)` is also the pass's resumable cursor.
CREATE INDEX idx_detection_t_end ON detection (t_end);

-- Pin lookups. `demodulation.detection_id` is a foreign key into `detection`, so every detection
-- DELETE checks it — without an index that is a scan of `demodulation` per deleted row. The two
-- polymorphic references (no foreign key) are partial indexes over the one kind that names a
-- detection.
CREATE INDEX idx_demodulation_detection ON demodulation (detection_id)
    WHERE detection_id IS NOT NULL;
CREATE INDEX idx_anomaly_subject_detection ON anomaly (subject_id)
    WHERE subject_kind = 'detection';
CREATE INDEX idx_emitter_classification_input_detection ON emitter_classification (input_id)
    WHERE input_kind = 'detection';

-- A contiguous run of one track's pruned detections under one survey and one provenance, no
-- longer than the policy's rollup span and with no gap longer than its rollup gap. Every column is
-- an aggregate of the Detection columns of the same name: the time hull, the frequency envelope,
-- means and maxima, the count, and the OR/AND of the flag bitmask (so "every member was flagged"
-- and "some member was flagged" both survive). `on_air_ns` is the members' summed duration: the
-- hull is never the time on air (docs/07 §2.11's rule for emitters, applied here). A summary, not
-- a measurement: a later prune may extend the newest row of a track, so there is no immutability
-- trigger.
-- `track_id` is NULL for a detection that was never linked to any track.
CREATE TABLE detection_rollup (
    rollup_id      INTEGER PRIMARY KEY,
    track_id       BLOB    REFERENCES track (track_id),
    survey_id      BLOB    NOT NULL REFERENCES survey (survey_id),
    provenance_id  BLOB    NOT NULL REFERENCES provenance (provenance_id),
    t_start        INTEGER NOT NULL,
    t_end          INTEGER NOT NULL CHECK (t_end >= t_start),
    on_air_ns      INTEGER NOT NULL CHECK (on_air_ns >= 0),
    f_lo           REAL    NOT NULL,
    f_hi           REAL    NOT NULL CHECK (f_hi >= f_lo),
    f_center_mean  REAL    NOT NULL,
    obw_mean       REAL    NOT NULL,
    obw_max        REAL    NOT NULL,
    snr_peak_max   REAL    NOT NULL,
    snr_mean_mean  REAL    NOT NULL,
    peak_dbfs_max  REAL    NOT NULL,
    detections     INTEGER NOT NULL CHECK (detections >= 1),
    flags_any      INTEGER NOT NULL,
    flags_all      INTEGER NOT NULL,
    clip_count     INTEGER NOT NULL CHECK (clip_count >= 0)
);
CREATE INDEX idx_detection_rollup_track ON detection_rollup (track_id, t_start);
CREATE INDEX idx_detection_rollup_t     ON detection_rollup (t_start);
-- OR IGNORE: `region_extent` comes from 0001, so a file rolled back past this migration and
-- replayed (the migration tests do) still holds the row.
INSERT OR IGNORE INTO region_extent (table_name, max_f_span, max_t_span)
    VALUES ('detection_rollup', 0, 0);
