-- T-143: content gating is off by default and enforced only by the repository when opted in
-- (hk_model::content_gating_enabled). Rebuilds the four content-carrying tables without the
-- CHECKs that tied content to 'unrestricted'/'own-key-decrypted', so content under any class can
-- be stored. Columns, foreign keys, indexes and immutability triggers are otherwise unchanged.
PRAGMA defer_foreign_keys = ON;

CREATE TABLE recording_new (
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
    content_class            TEXT    NOT NULL CHECK (content_class IN ('unrestricted', 'metadata-only',
                                 'restricted-cellular', 'restricted-paging', 'own-key-decrypted')),
    provenance_id            BLOB    NOT NULL REFERENCES provenance (provenance_id),
    body                     TEXT    NOT NULL
);
INSERT INTO recording_new SELECT * FROM recording;
DROP TABLE recording;
ALTER TABLE recording_new RENAME TO recording;
CREATE INDEX idx_recording_t_start           ON recording (t_start);
CREATE INDEX idx_recording_trigger_detection ON recording (trigger_detection_id);
CREATE INDEX idx_recording_trigger_demod     ON recording (trigger_demodulation_id);
CREATE TRIGGER recording_immutable BEFORE UPDATE ON recording
    BEGIN SELECT RAISE(ABORT, 'recording rows are immutable'); END;

CREATE TABLE decode_new (
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
    body             TEXT    NOT NULL
);
INSERT INTO decode_new SELECT * FROM decode;
DROP TABLE decode;
ALTER TABLE decode_new RENAME TO decode;
CREATE INDEX idx_decode_identity ON decode (identity_scheme, identity_value, t);
CREATE INDEX idx_decode_demod    ON decode (demod_id);
CREATE TRIGGER decode_append_only BEFORE UPDATE ON decode
    BEGIN SELECT RAISE(ABORT, 'decodes are append-only'); END;

CREATE TABLE bitstream_new (
    bitstream_id   BLOB    PRIMARY KEY,
    emitter_id     BLOB    REFERENCES emitter (emitter_id),
    demod_id       BLOB    REFERENCES demodulation (demod_id),
    provenance_id  BLOB    REFERENCES provenance (provenance_id),
    transport      TEXT    NOT NULL CHECK (transport IN ('stored', 'live')),
    content_class  TEXT    NOT NULL CHECK (content_class IN ('unrestricted', 'metadata-only',
                       'restricted-cellular', 'restricted-paging', 'own-key-decrypted')),
    t_start        INTEGER NOT NULL,
    body           TEXT    NOT NULL
);
INSERT INTO bitstream_new SELECT * FROM bitstream;
DROP TABLE bitstream;
ALTER TABLE bitstream_new RENAME TO bitstream;
CREATE INDEX idx_bitstream_demod ON bitstream (demod_id);
CREATE TRIGGER bitstream_append_only BEFORE UPDATE ON bitstream
    BEGIN SELECT RAISE(ABORT, 'bitstreams are append-only'); END;

CREATE TABLE annotation_new (
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
    body           TEXT    NOT NULL
);
INSERT INTO annotation_new SELECT * FROM annotation;
DROP TABLE annotation;
ALTER TABLE annotation_new RENAME TO annotation;
CREATE INDEX idx_annotation_target ON annotation (target_kind, target_id);
-- Only the export bookkeeping flag may change on an annotation.
CREATE TRIGGER annotation_append_only BEFORE UPDATE ON annotation
    WHEN NEW.annotation_id IS NOT OLD.annotation_id OR NEW.target_kind IS NOT OLD.target_kind
      OR NEW.target_id IS NOT OLD.target_id OR NEW.author IS NOT OLD.author
      OR NEW.kind IS NOT OLD.kind OR NEW.supersedes IS NOT OLD.supersedes
      OR NEW.content_class IS NOT OLD.content_class OR NEW.has_content IS NOT OLD.has_content
      OR NEW.t IS NOT OLD.t OR NEW.body IS NOT OLD.body
    BEGIN SELECT RAISE(ABORT, 'annotations are append-only (only exported may change)'); END;
