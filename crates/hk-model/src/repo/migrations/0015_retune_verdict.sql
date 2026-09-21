-- T-598 (AWARE-011, follows T-586): the cross-centre retune verdict, persisted.
--
-- T-586 measured the slope of a line's absolute frequency against the local oscillator and proved
-- it separates a real emission (slope 0) from a receiver artefact (slope 1 LO-locked, slope 2
-- image). It computed the verdict and threw it away, so one LO-relative spur seen from three
-- centres stayed three inventory rows at three absolute frequencies -- the user's 2026-09-21 field
-- report. These columns and this relation kind are where the verdict is kept.
--
-- 1. THE VERDICT ABOUT A DETECTION.
--    A detection row is IMMUTABLE (`detection_immutable`): it is what was measured, and an
--    interpretation must never rewrite it. The verdict is therefore its own append-only record
--    about the detection, in the same discipline as `emitter_relation`: the current verdict for a
--    detection is the highest `verdict_id`, and it counts only while `active = 1`.
--
--    IT IS REVOCABLE, and that is the point. A verdict is a function of the centres seen so far,
--    and new evidence at a new centre may overturn it -- exactly as a detected end is provisional
--    and revocable. A revocation is an appended row, never an edit and never a delete, so the
--    claim that was made stays auditable.
--
--    `absolute` is the WEAK claim -- "not LO-relative", never "real": a reference-clock harmonic
--    sits at a fixed absolute frequency too, and is still an artefact. It therefore implies no
--    flag bits (`flag_bits = 0`) and clears nothing another mechanism found.
--
--    `flag_bits` is what `RetuneSlope::apply` implies for this slope (`DetectionFlags::bits`), so
--    the trust and rank queries can read the measured flags OR'd with the standing verdict's
--    without duplicating the mapping in SQL.
CREATE TABLE detection_retune (
    verdict_id    INTEGER PRIMARY KEY,
    detection_id  BLOB    NOT NULL REFERENCES detection (detection_id),
    slope         TEXT    NOT NULL CHECK (slope IN ('absolute', 'lo-locked', 'image')),
    -- The invariant coordinate `f - slope*f_LO`, Hz: the absolute frequency for `absolute`, the
    -- LO offset for `lo-locked`.
    invariant_hz  REAL    NOT NULL,
    -- Distinct LOs that decided it. Never below 2: one centre measures no slope at all.
    centres       INTEGER NOT NULL CHECK (centres >= 2),
    -- Spread of the invariant coordinate over the group, Hz.
    spread_hz     REAL,
    flag_bits     INTEGER NOT NULL CHECK (flag_bits >= 0),
    active        INTEGER NOT NULL CHECK (active IN (0, 1)),
    t             INTEGER NOT NULL,
    -- The rule id that claimed or revoked it.
    actor         TEXT    NOT NULL CHECK (length(actor) > 0)
);
CREATE INDEX idx_detection_retune ON detection_retune (detection_id, verdict_id);

CREATE TRIGGER detection_retune_append_only BEFORE UPDATE ON detection_retune
    BEGIN SELECT RAISE(ABORT, 'retune verdicts are append-only; append a revocation row'); END;
CREATE TRIGGER detection_retune_no_delete BEFORE DELETE ON detection_retune
    BEGIN SELECT RAISE(ABORT, 'retune verdicts are append-only; append a revocation row'); END;

-- 1b. The tuning centres, indexed.
--    Retune diversity is a question about the LOs a region was visited from, and that question is
--    asked once per resolved row. The LO lives inside the provenance's canonical JSON, so without
--    this expression index every ask is a table scan of every distinct front-end configuration.
CREATE INDEX idx_provenance_tune_center
    ON provenance (json_extract(canonical, '$.tune.center_hz'));

-- 2. THE RELATIONSHIP BETWEEN THE SIGHTINGS.
--    The three sightings of one LO-relative spur are one artefact seen from three centres. They
--    are RELATED, never deleted: each keeps its id, its detections, its time extent and its
--    history, and one of them represents the family in the inventory while the others defer.
--
--    It is its own kind rather than `duplicate-of` or `artifact-of` because those two are written
--    and REVOKED by the T-219 overlap resolver over *overlapping bands*, and these sightings do
--    not overlap at all -- they are at different absolute frequencies, which is the whole point.
--    Sharing a kind would make the two rules revoke each other's claims on every pass. It is also
--    the more honest vocabulary: the row it defers to is a SIBLING sighting of the same receiver
--    artefact, not a source emission that produced it.
--
--    SQLite cannot relax a CHECK in place, so the table is rebuilt (its append-only triggers and
--    indexes with it) and every existing row copied. Nothing is dropped or rewritten.
DROP TRIGGER emitter_relation_append_only;
DROP TRIGGER emitter_relation_no_delete;
ALTER TABLE emitter_relation RENAME TO emitter_relation_old;

CREATE TABLE emitter_relation (
    relation_id    INTEGER PRIMARY KEY,
    -- The subordinate row: the candidate that defers.
    emitter_id     BLOB    NOT NULL REFERENCES emitter (emitter_id),
    -- What it defers to: the confirmed entry, the stronger duplicate, the artifact's source, or
    -- (retune-sibling-of) the sighting that represents the LO-relative family.
    source_id      BLOB    NOT NULL REFERENCES emitter (emitter_id),
    kind           TEXT    NOT NULL CHECK (kind IN ('suppressed-by', 'duplicate-of', 'artifact-of',
                                                    'retune-sibling-of')),
    artifact_kind  TEXT    CHECK (artifact_kind IS NULL
                                  OR artifact_kind IN ('image', 'harmonic', 'intermod')),
    -- 1 = in force, 0 = revoked (evidence changed, or a user disagreed).
    active         INTEGER NOT NULL CHECK (active IN (0, 1)),
    t              INTEGER NOT NULL,
    author         TEXT    NOT NULL CHECK (author IN ('system', 'user')),
    -- The rule id (e.g. hk-pipeline/overlap@1) or the API token fingerprint.
    actor          TEXT    NOT NULL,
    -- Backend-rendered reasoning, including the artifact arithmetic. Always disclosed.
    reason         TEXT    NOT NULL,
    -- The rank proxy that decided it (duplicate-of), else NULL.
    score          REAL,
    -- JSON detail: the arithmetic (n, a, b, LO, predicted, error), the rank terms, or the retune
    -- slope, invariant coordinate and the LOs it was measured over.
    detail         TEXT,
    CHECK (emitter_id != source_id),
    CHECK ((artifact_kind IS NULL) = (kind != 'artifact-of'))
);
INSERT INTO emitter_relation (relation_id, emitter_id, source_id, kind, artifact_kind, active, t,
                              author, actor, reason, score, detail)
    SELECT relation_id, emitter_id, source_id, kind, artifact_kind, active, t,
           author, actor, reason, score, detail FROM emitter_relation_old;
DROP TABLE emitter_relation_old;

CREATE INDEX idx_emitter_relation_emitter ON emitter_relation (emitter_id, relation_id);
CREATE INDEX idx_emitter_relation_source  ON emitter_relation (source_id, relation_id);

CREATE TRIGGER emitter_relation_append_only BEFORE UPDATE ON emitter_relation
    BEGIN SELECT RAISE(ABORT, 'emitter relations are append-only; append a revocation row'); END;
CREATE TRIGGER emitter_relation_no_delete BEFORE DELETE ON emitter_relation
    BEGIN SELECT RAISE(ABORT, 'emitter relations are append-only; append a revocation row'); END;
