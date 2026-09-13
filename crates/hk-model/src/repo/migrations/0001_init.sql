-- Pre-release: this migration is edited in place until the first release; after that, schema changes are new migrations using table rebuilds, never writable_schema.
-- hackriff relational schema v1 (T-002; docs/07 §2, ADR-0006).
--
-- Conventions:
--   * ids are 16-byte UUID BLOBs (UUIDv7 sorts by creation time, so WITHOUT ROWID tables
--     cluster and append in time order); content hashes are 32-byte SHA-256 BLOBs;
--   * times are INTEGER nanoseconds since the Unix epoch; frequencies REAL Hz;
--   * enums are TEXT in their serde (kebab-case) form;
--   * `body` columns hold the full object as JSON and are authoritative on read; the other
--     columns are projections written from the same struct in the same statement, for keys,
--     foreign keys, indexes, queries and CHECK constraints. Detection and Emitter are fully
--     columnar (hot tables);
--   * measurement and interpretation tables reject UPDATE via triggers (P2.1); deletes are left
--     to the retention policy (a later task);
--   * content gating (ADR-0004) is enforced in the repository and repeated here as CHECKs:
--     content is only allowed under 'unrestricted' or 'own-key-decrypted'.

CREATE TABLE scan_plan (
    plan_id     BLOB    NOT NULL,
    version     INTEGER NOT NULL CHECK (version >= 1),
    name        TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    body        TEXT    NOT NULL,
    PRIMARY KEY (plan_id, version)
) WITHOUT ROWID;

CREATE TABLE survey (
    survey_id     BLOB    PRIMARY KEY,
    plan_id       BLOB    NOT NULL,
    plan_version  INTEGER NOT NULL,
    device_id     TEXT    NOT NULL,
    state         TEXT    NOT NULL CHECK (state IN ('open', 'closed', 'aborted')),
    t_start       INTEGER NOT NULL,
    t_end         INTEGER,
    summary       TEXT,
    FOREIGN KEY (plan_id, plan_version) REFERENCES scan_plan (plan_id, version)
);

CREATE TABLE calibration_state (
    cal_id       BLOB    PRIMARY KEY,
    supersedes   BLOB    REFERENCES calibration_state (cal_id),
    device_id    TEXT    NOT NULL,
    measured_at  INTEGER NOT NULL,
    body         TEXT    NOT NULL
);

CREATE TABLE spur_mask (
    spur_id      BLOB    PRIMARY KEY,
    supersedes   BLOB    REFERENCES spur_mask (spur_id),
    device_id    TEXT    NOT NULL,
    measured_at  INTEGER NOT NULL,
    body         TEXT    NOT NULL
);

-- Deduplicated by value: `content_hash` is SHA-256 of the canonical JSON in `canonical`
-- (sorted keys, -0.0 normalised). Inserts use ON CONFLICT (content_hash) DO NOTHING, then read the
-- winning row, so concurrent connections converge on one id.
CREATE TABLE provenance (
    provenance_id         BLOB    PRIMARY KEY,
    content_hash          BLOB    NOT NULL UNIQUE CHECK (length(content_hash) = 32),
    canonical             TEXT    NOT NULL,
    device_id             TEXT    NOT NULL,
    overload              INTEGER NOT NULL CHECK (overload IN (0, 1)),
    quantisation_limited  INTEGER NOT NULL CHECK (quantisation_limited IN (0, 1)),
    cal_id                BLOB    REFERENCES calibration_state (cal_id),
    spur_id               BLOB    REFERENCES spur_mask (spur_id)
);

-- `flags` bitmask (DetectionFlags::bits): 1 clipped, 2 spur_candidate, 4 image_candidate,
-- 8 marginal, 16 suspect_imd, 32 compressed, 64 impulsive, 128 edge, 256 image_retune_confirmed.
CREATE TABLE detection (
    detection_id      BLOB    PRIMARY KEY,
    survey_id         BLOB    NOT NULL REFERENCES survey (survey_id),
    provenance_id     BLOB    NOT NULL REFERENCES provenance (provenance_id),
    t_start           INTEGER NOT NULL,
    t_end             INTEGER NOT NULL CHECK (t_end >= t_start),
    f_center          REAL    NOT NULL,
    obw               REAL    NOT NULL CHECK (obw >= 0),
    f_lo              REAL    NOT NULL,
    f_hi              REAL    NOT NULL,
    xdb_bw            REAL,
    xdb_level         REAL,
    snr_peak          REAL    NOT NULL,
    snr_mean          REAL    NOT NULL,
    sk                REAL,
    flags             INTEGER NOT NULL,
    peak_dbfs         REAL    NOT NULL,
    peak_dbm          REAL,
    clip_count        INTEGER NOT NULL CHECK (clip_count >= 0),
    detector_version  TEXT    NOT NULL,
    spur_reason       TEXT    CHECK (spur_reason IN ('ref-harmonic', 'dc', 'lo-relative', 'comb', 'spur-map', 'clock-harmonic')),
    spur_mask_id      BLOB    REFERENCES spur_mask (spur_id),
    CHECK (spur_reason IS NULL OR (flags & 2) != 0),
    CHECK ((spur_reason IS 'spur-map') = (spur_mask_id IS NOT NULL)),
    CHECK ((flags & 256) = 0 OR (flags & 4) != 0),
    CHECK (clip_count = 0 OR (flags & 1) != 0)
) WITHOUT ROWID;
CREATE INDEX idx_detection_f_center_t_start ON detection (f_center, t_start);
CREATE INDEX idx_detection_f_lo_f_hi        ON detection (f_lo, f_hi);
CREATE INDEX idx_detection_survey           ON detection (survey_id);

CREATE TABLE track (
    track_id     BLOB    PRIMARY KEY,
    state        TEXT    NOT NULL,
    merged_into  BLOB    REFERENCES track (track_id),
    split_from   BLOB    REFERENCES track (track_id),
    t_start      INTEGER NOT NULL,
    t_end        INTEGER NOT NULL,
    f_center     REAL    NOT NULL,
    updated_at   INTEGER NOT NULL,
    body         TEXT    NOT NULL
);

CREATE TABLE track_detection (
    track_id      BLOB    NOT NULL REFERENCES track (track_id),
    detection_id  BLOB    NOT NULL REFERENCES detection (detection_id) ON DELETE CASCADE,
    linked_at     INTEGER NOT NULL,
    PRIMARY KEY (track_id, detection_id)
) WITHOUT ROWID;
CREATE INDEX idx_track_detection_detection ON track_detection (detection_id);

-- The current known status is the latest emitter_status row (append-only history).
CREATE TABLE emitter (
    emitter_id       BLOB    PRIMARY KEY,
    f_center         REAL    NOT NULL,
    bandwidth        REAL    NOT NULL CHECK (bandwidth >= 0),
    f_lo             REAL    NOT NULL,
    f_hi             REAL    NOT NULL,
    first_seen       INTEGER NOT NULL,
    last_seen        INTEGER NOT NULL CHECK (last_seen >= first_seen),
    count            INTEGER NOT NULL CHECK (count >= 0),
    fingerprint      TEXT,
    identity_scheme  TEXT,
    identity_value   TEXT,
    -- T-018: content class of the identity's sources, most restrictive seen. NULL = unclassified:
    -- inventory queries derive it from linked decodes and fail closed (identity withheld).
    identity_class   TEXT    CHECK (identity_class IN ('unrestricted', 'metadata-only',
                         'restricted-cellular', 'restricted-paging', 'own-key-decrypted')),
    -- T-018: the emitter this one was merged into (never deleted; queries skip merged rows).
    merged_into      BLOB    REFERENCES emitter (emitter_id),
    CHECK ((identity_scheme IS NULL) = (identity_value IS NULL)),
    CHECK (merged_into IS NULL OR merged_into != emitter_id)
);
CREATE INDEX idx_emitter_f_lo_f_hi ON emitter (f_lo, f_hi);
CREATE INDEX idx_emitter_merged_into ON emitter (merged_into) WHERE merged_into IS NOT NULL;
CREATE INDEX idx_emitter_last_seen ON emitter (last_seen);
-- One emitter per decoded identity (provisional; C27 open question on entity levels).
CREATE UNIQUE INDEX idx_emitter_identity ON emitter (identity_scheme, identity_value)
    WHERE identity_scheme IS NOT NULL;

CREATE TABLE emitter_status (
    status_id   INTEGER PRIMARY KEY,
    emitter_id  BLOB    NOT NULL REFERENCES emitter (emitter_id),
    status      TEXT    NOT NULL CHECK (status IN ('known', 'unexpected-here', 'unknown')),
    prior_ref   TEXT,
    reason      TEXT    NOT NULL,
    t           INTEGER NOT NULL,
    author      TEXT    NOT NULL CHECK (author IN ('prior', 'decoder', 'classifier', 'user', 'system',
                    'clusterer'))
);
CREATE INDEX idx_emitter_status_emitter ON emitter_status (emitter_id, status_id, status);

CREATE TABLE emitter_classification (
    classification_id  INTEGER PRIMARY KEY,
    emitter_id         BLOB    NOT NULL REFERENCES emitter (emitter_id),
    t                  INTEGER NOT NULL,
    family             TEXT    NOT NULL,
    confidence         REAL    NOT NULL,
    open_set_score     REAL    NOT NULL,
    model_version      TEXT    NOT NULL,
    -- T-018: the observation the classifier ran on, and the fingerprint feature-set version.
    input_kind           TEXT,
    input_id             BLOB,
    feature_set_version  INTEGER,
    CHECK ((input_kind IS NULL) = (input_id IS NULL))
);
CREATE INDEX idx_emitter_classification_emitter
    ON emitter_classification (emitter_id, classification_id);

CREATE TABLE emitter_tag (
    emitter_id  BLOB NOT NULL REFERENCES emitter (emitter_id),
    tag         TEXT NOT NULL,
    PRIMARY KEY (emitter_id, tag)
) WITHOUT ROWID;
CREATE INDEX idx_emitter_tag_tag ON emitter_tag (tag);

-- Polymorphic target with no foreign key: links may outlive the rows they summarise
-- (docs/07 §2.11).
CREATE TABLE emitter_link (
    emitter_id   BLOB    NOT NULL REFERENCES emitter (emitter_id),
    target_kind  TEXT    NOT NULL,
    target_id    BLOB    NOT NULL,
    linked_at    INTEGER NOT NULL,
    -- T-018: set once when a merge re-points the link to the surviving emitter.
    superseded_by  BLOB    REFERENCES emitter (emitter_id),
    superseded_at  INTEGER,
    PRIMARY KEY (emitter_id, target_kind, target_id),
    CHECK ((superseded_by IS NULL) = (superseded_at IS NULL))
) WITHOUT ROWID;
CREATE INDEX idx_emitter_link_target ON emitter_link (target_kind, target_id);

-- T-018: which emitter each source observation (track, detection, decode) was counted into and
-- how much it added, so replaying a source never double-counts. An aggregate ledger: merges
-- re-point it.
CREATE TABLE emitter_observation (
    source_kind  TEXT    NOT NULL,
    source_id    BLOB    NOT NULL,
    emitter_id   BLOB    NOT NULL REFERENCES emitter (emitter_id),
    count        INTEGER NOT NULL CHECK (count >= 0),
    t_start      INTEGER NOT NULL,
    t_end        INTEGER NOT NULL CHECK (t_end >= t_start),
    -- T-034: what was measured, independent of the row ids a re-run mints (producer + capture,
    -- canonical JSON; NULL = not keyed) and the observed centre. A new source with the same key,
    -- an overlapping span and a centre within tolerance is a re-measurement and adds nothing.
    measurement  TEXT,
    f_center     REAL,
    CHECK ((measurement IS NULL) = (f_center IS NULL)),
    PRIMARY KEY (source_kind, source_id)
) WITHOUT ROWID;
CREATE INDEX idx_emitter_observation_emitter ON emitter_observation (emitter_id);
CREATE INDEX idx_emitter_observation_measurement ON emitter_observation (measurement, t_start)
    WHERE measurement IS NOT NULL;

-- T-018: append-only merge history (undo input). The absorbed emitter keeps its row, its
-- classification/status history and superseded links.
CREATE TABLE emitter_merge (
    merge_id        INTEGER PRIMARY KEY,
    from_emitter    BLOB    NOT NULL REFERENCES emitter (emitter_id),
    into_emitter    BLOB    NOT NULL REFERENCES emitter (emitter_id),
    t               INTEGER NOT NULL,
    reason          TEXT    NOT NULL,
    from_count      INTEGER NOT NULL,
    identity_moved  INTEGER NOT NULL CHECK (identity_moved IN (0, 1)),
    CHECK (from_emitter != into_emitter)
);
CREATE INDEX idx_emitter_merge_into ON emitter_merge (into_emitter);

-- T-036: append-only audit of user reclassifications that open a decoded identity (legal
-- guardrail). Only an explicit own-traffic authorisation may open an identity, only to
-- own-key-decrypted or unrestricted, and never from restricted-cellular / restricted-paging.
-- Keyed by the identity too, so the opening follows it through merges. Decode rows (the stored
-- interpretations) are never rewritten; only the emitter aggregate's identity_class changes.
CREATE TABLE identity_reclassification (
    reclass_id       INTEGER PRIMARY KEY,
    emitter_id       BLOB    NOT NULL REFERENCES emitter (emitter_id),
    identity_scheme  TEXT    NOT NULL,
    identity_value   TEXT    NOT NULL,
    old_class        TEXT    NOT NULL CHECK (old_class IN ('unrestricted', 'metadata-only',
                         'own-key-decrypted')),
    new_class        TEXT    NOT NULL CHECK (new_class IN ('unrestricted', 'own-key-decrypted')),
    authorisation    TEXT    NOT NULL CHECK (authorisation = 'own-traffic-authorised'),
    reason           TEXT    NOT NULL CHECK (length(reason) > 0),
    author           TEXT    NOT NULL CHECK (length(author) > 0),
    t                INTEGER NOT NULL
);
CREATE INDEX idx_identity_reclassification_identity
    ON identity_reclassification (identity_scheme, identity_value, reclass_id);
CREATE INDEX idx_identity_reclassification_emitter
    ON identity_reclassification (emitter_id, reclass_id);

-- IQ and audio are content: only content-permitting classes may be recorded.
CREATE TABLE recording (
    recording_id             BLOB    PRIMARY KEY,
    kind                     TEXT    NOT NULL,
    t_start                  INTEGER NOT NULL,
    t_end                    INTEGER NOT NULL,
    f_center                 REAL    NOT NULL,
    trigger_kind             TEXT    NOT NULL,
    trigger_detection_id     BLOB    REFERENCES detection (detection_id),
    trigger_demodulation_id  BLOB    REFERENCES demodulation (demod_id),
    size_bytes               INTEGER NOT NULL,
    retention_class          TEXT    NOT NULL,
    content_class            TEXT    NOT NULL CHECK (content_class IN ('unrestricted', 'own-key-decrypted')),
    provenance_id            BLOB    NOT NULL REFERENCES provenance (provenance_id),
    body                     TEXT    NOT NULL
);
CREATE INDEX idx_recording_t_start           ON recording (t_start);
CREATE INDEX idx_recording_trigger_detection ON recording (trigger_detection_id);
CREATE INDEX idx_recording_trigger_demod     ON recording (trigger_demodulation_id);

CREATE TABLE demodulation (
    demod_id       BLOB    PRIMARY KEY,
    emitter_id     BLOB    REFERENCES emitter (emitter_id),
    detection_id   BLOB    REFERENCES detection (detection_id),
    recording_id   BLOB    REFERENCES recording (recording_id),
    mode           TEXT    NOT NULL,
    t_start        INTEGER NOT NULL,
    t_end          INTEGER NOT NULL,
    demod_version  TEXT    NOT NULL,
    body           TEXT    NOT NULL
);
CREATE INDEX idx_demodulation_emitter ON demodulation (emitter_id);

CREATE TABLE decode (
    decode_id        BLOB    PRIMARY KEY,
    demod_id         BLOB    REFERENCES demodulation (demod_id),
    recording_id     BLOB    REFERENCES recording (recording_id),
    decoder_id       TEXT    NOT NULL,
    crc_status       TEXT    NOT NULL,
    identity_scheme  TEXT,
    identity_value   TEXT,
    content_class    TEXT    NOT NULL CHECK (content_class IN ('unrestricted', 'metadata-only',
                         'restricted-cellular', 'restricted-paging', 'own-key-decrypted')),
    has_content      INTEGER NOT NULL CHECK (has_content IN (0, 1)),
    t                INTEGER NOT NULL,
    body             TEXT    NOT NULL,
    CHECK (has_content = 0 OR content_class IN ('unrestricted', 'own-key-decrypted'))
);
CREATE INDEX idx_decode_identity ON decode (identity_scheme, identity_value, t);
CREATE INDEX idx_decode_demod    ON decode (demod_id);

CREATE TABLE bitstream (
    bitstream_id   BLOB    PRIMARY KEY,
    emitter_id     BLOB    REFERENCES emitter (emitter_id),
    demod_id       BLOB    REFERENCES demodulation (demod_id),
    provenance_id  BLOB    REFERENCES provenance (provenance_id),
    transport      TEXT    NOT NULL CHECK (transport IN ('stored', 'live')),
    content_class  TEXT    NOT NULL CHECK (content_class IN ('unrestricted', 'metadata-only',
                       'restricted-cellular', 'restricted-paging', 'own-key-decrypted')),
    t_start        INTEGER NOT NULL,
    body           TEXT    NOT NULL,
    CHECK (transport = 'live' OR content_class IN ('unrestricted', 'own-key-decrypted'))
);
CREATE INDEX idx_bitstream_demod ON bitstream (demod_id);

-- A cache, not a measurement: upserted by natural key, so no immutability trigger.
-- `payload_hash` is the SHA-256 of the canonical payload; Explanation evidence pins it.
CREATE TABLE external_event (
    event_id      BLOB    PRIMARY KEY,
    source        TEXT    NOT NULL,
    native_id     TEXT    NOT NULL,
    event_type    TEXT    NOT NULL,
    t_start       INTEGER NOT NULL,
    t_end         INTEGER NOT NULL,
    fetched_at    INTEGER NOT NULL,
    valid_until   INTEGER,
    payload_hash  BLOB    NOT NULL CHECK (length(payload_hash) = 32),
    body          TEXT    NOT NULL,
    UNIQUE (source, native_id)
);
CREATE INDEX idx_external_event_t ON external_event (t_start, t_end);

CREATE TABLE anomaly (
    anomaly_id    BLOB    PRIMARY KEY,
    kind          TEXT    NOT NULL,
    subject_kind  TEXT    NOT NULL,
    subject_id    BLOB,
    f_lo          REAL    NOT NULL,
    f_hi          REAL    NOT NULL,
    t_start       INTEGER NOT NULL,
    t_end         INTEGER NOT NULL CHECK (t_end >= t_start),
    score         REAL    NOT NULL,
    t             INTEGER NOT NULL,
    body          TEXT    NOT NULL
);
CREATE INDEX idx_anomaly_f_lo_f_hi     ON anomaly (f_lo, f_hi);
CREATE INDEX idx_anomaly_t_start_t_end ON anomaly (t_start, t_end);

CREATE TABLE anomaly_status (
    status_id   INTEGER PRIMARY KEY,
    anomaly_id  BLOB    NOT NULL REFERENCES anomaly (anomaly_id),
    status      TEXT    NOT NULL CHECK (status IN ('open', 'resolved', 'dismissed')),
    t           INTEGER NOT NULL,
    note        TEXT
);
CREATE INDEX idx_anomaly_status_anomaly ON anomaly_status (anomaly_id, status_id);

-- The f/t extent is copied from the anomaly at insert, so explanations are region/time indexed.
CREATE TABLE explanation (
    explanation_id    BLOB    PRIMARY KEY,
    anomaly_id        BLOB    NOT NULL REFERENCES anomaly (anomaly_id),
    supersedes        BLOB    REFERENCES explanation (explanation_id),
    cause_kind        TEXT    NOT NULL,
    cause_event_id    BLOB    REFERENCES external_event (event_id),
    cause_emitter_id  BLOB    REFERENCES emitter (emitter_id),
    correlation_type  TEXT    NOT NULL
        CHECK (correlation_type IN ('time-coincidence', 'geometry', 'signature')),
    score             REAL    NOT NULL,
    f_lo              REAL    NOT NULL,
    f_hi              REAL    NOT NULL,
    t_start           INTEGER NOT NULL,
    t_end             INTEGER NOT NULL,
    t                 INTEGER NOT NULL,
    body              TEXT    NOT NULL
);
CREATE INDEX idx_explanation_f_lo_f_hi     ON explanation (f_lo, f_hi);
CREATE INDEX idx_explanation_t_start_t_end ON explanation (t_start, t_end);
CREATE INDEX idx_explanation_anomaly       ON explanation (anomaly_id);

CREATE TABLE annotation (
    annotation_id  BLOB    PRIMARY KEY,
    target_kind    TEXT    NOT NULL,
    target_id      BLOB,
    author         TEXT    NOT NULL CHECK (author IN ('user', 'decoder', 'classifier')),
    kind           TEXT    NOT NULL CHECK (kind IN ('label', 'correction', 'ground-truth')),
    supersedes     BLOB    REFERENCES annotation (annotation_id),
    content_class  TEXT    NOT NULL CHECK (content_class IN ('unrestricted', 'metadata-only',
                       'restricted-cellular', 'restricted-paging', 'own-key-decrypted')),
    has_content    INTEGER NOT NULL CHECK (has_content IN (0, 1)),
    t              INTEGER NOT NULL,
    exported       INTEGER NOT NULL,
    body           TEXT    NOT NULL,
    CHECK (has_content = 0 OR content_class IN ('unrestricted', 'own-key-decrypted'))
);
CREATE INDEX idx_annotation_target ON annotation (target_kind, target_id);

-- Largest frequency span and duration ever written per region-indexed table. Region queries
-- use them to bound index range scans: a row whose lower edge is more than one max-span below
-- the query cannot overlap it. The values only grow, so they stay correct after deletes.
CREATE TABLE region_extent (
    table_name  TEXT    PRIMARY KEY,
    max_f_span  REAL    NOT NULL,
    max_t_span  INTEGER NOT NULL
) WITHOUT ROWID;
INSERT INTO region_extent (table_name, max_f_span, max_t_span) VALUES
    ('detection', 0, 0), ('emitter', 0, 0), ('anomaly', 0, 0), ('explanation', 0, 0);

-- Overload propagation: a detection under an overloaded provenance must be flagged clipped.
-- (clip_count > 0 without the flag is refused by the detection table's CHECK.)
CREATE TRIGGER detection_overload_flagged BEFORE INSERT ON detection
    WHEN (NEW.flags & 1) = 0
     AND (SELECT overload FROM provenance WHERE provenance_id = NEW.provenance_id) = 1
    BEGIN SELECT RAISE(ABORT, 'overloaded provenance requires flags.clipped'); END;

-- Measurement rows are immutable; interpretation rows are append-only (P2.1).
CREATE TRIGGER scan_plan_immutable BEFORE UPDATE ON scan_plan
    BEGIN SELECT RAISE(ABORT, 'scan_plan versions are immutable'); END;
CREATE TRIGGER calibration_state_immutable BEFORE UPDATE ON calibration_state
    BEGIN SELECT RAISE(ABORT, 'calibration_state versions are immutable'); END;
CREATE TRIGGER spur_mask_immutable BEFORE UPDATE ON spur_mask
    BEGIN SELECT RAISE(ABORT, 'spur_mask versions are immutable'); END;
CREATE TRIGGER provenance_immutable BEFORE UPDATE ON provenance
    BEGIN SELECT RAISE(ABORT, 'provenance rows are immutable'); END;
CREATE TRIGGER detection_immutable BEFORE UPDATE ON detection
    BEGIN SELECT RAISE(ABORT, 'detection rows are immutable'); END;
CREATE TRIGGER recording_immutable BEFORE UPDATE ON recording
    BEGIN SELECT RAISE(ABORT, 'recording rows are immutable'); END;
CREATE TRIGGER track_detection_append_only BEFORE UPDATE ON track_detection
    BEGIN SELECT RAISE(ABORT, 'track_detection links are append-only'); END;
CREATE TRIGGER emitter_status_append_only BEFORE UPDATE ON emitter_status
    BEGIN SELECT RAISE(ABORT, 'emitter status history is append-only'); END;
CREATE TRIGGER emitter_classification_append_only BEFORE UPDATE ON emitter_classification
    BEGIN SELECT RAISE(ABORT, 'classifications are append-only'); END;
CREATE TRIGGER emitter_link_append_only BEFORE UPDATE ON emitter_link
    WHEN NOT (OLD.superseded_by IS NULL AND NEW.superseded_by IS NOT NULL
              AND NEW.emitter_id = OLD.emitter_id AND NEW.target_kind = OLD.target_kind
              AND NEW.target_id = OLD.target_id AND NEW.linked_at = OLD.linked_at)
    BEGIN SELECT RAISE(ABORT, 'emitter links are append-only (only supersession is recorded)'); END;
CREATE TRIGGER emitter_merge_append_only BEFORE UPDATE ON emitter_merge
    BEGIN SELECT RAISE(ABORT, 'emitter merges are append-only'); END;
CREATE TRIGGER identity_reclassification_append_only BEFORE UPDATE ON identity_reclassification
    BEGIN SELECT RAISE(ABORT, 'identity reclassifications are an append-only audit'); END;
CREATE TRIGGER identity_reclassification_no_delete BEFORE DELETE ON identity_reclassification
    BEGIN SELECT RAISE(ABORT, 'identity reclassifications are an append-only audit'); END;
CREATE TRIGGER demodulation_append_only BEFORE UPDATE ON demodulation
    BEGIN SELECT RAISE(ABORT, 'demodulations are append-only'); END;
CREATE TRIGGER decode_append_only BEFORE UPDATE ON decode
    BEGIN SELECT RAISE(ABORT, 'decodes are append-only'); END;
CREATE TRIGGER bitstream_append_only BEFORE UPDATE ON bitstream
    BEGIN SELECT RAISE(ABORT, 'bitstreams are append-only'); END;
CREATE TRIGGER anomaly_append_only BEFORE UPDATE ON anomaly
    BEGIN SELECT RAISE(ABORT, 'anomalies are append-only; append to anomaly_status'); END;
CREATE TRIGGER anomaly_status_append_only BEFORE UPDATE ON anomaly_status
    BEGIN SELECT RAISE(ABORT, 'anomaly status history is append-only'); END;
CREATE TRIGGER explanation_append_only BEFORE UPDATE ON explanation
    BEGIN SELECT RAISE(ABORT, 'explanations are append-only'); END;
-- Only the export bookkeeping flag may change on an annotation.
CREATE TRIGGER annotation_append_only BEFORE UPDATE ON annotation
    WHEN NEW.annotation_id IS NOT OLD.annotation_id OR NEW.target_kind IS NOT OLD.target_kind
      OR NEW.target_id IS NOT OLD.target_id OR NEW.author IS NOT OLD.author
      OR NEW.kind IS NOT OLD.kind OR NEW.supersedes IS NOT OLD.supersedes
      OR NEW.content_class IS NOT OLD.content_class OR NEW.has_content IS NOT OLD.has_content
      OR NEW.t IS NOT OLD.t OR NEW.body IS NOT OLD.body
    BEGIN SELECT RAISE(ABORT, 'annotations are append-only (only exported may change)'); END;
