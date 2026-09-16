-- T-219 (ADR-0015 §11.4, docs/capabilities/C40, ADR-0012 §2.6): how one inventory row defers to
-- another. Three kinds, one mechanism — competition between hypotheses over a band:
--   suppressed-by  a candidate overlapping a Confirmed entry's band, with no distinguishing evidence
--   duplicate-of   the weaker of two overlapping candidates (ranked by the SNR × duty × trust proxy)
--   artifact-of    a candidate landing on a predicted image / harmonic / intermod frequency
--
-- APPEND-ONLY, keyed by emitter, and never a mutation or a delete of the losing row: the losing
-- candidate keeps its id, detections, tracks, links and history, so later evidence can revive it.
-- A relation is revoked by appending a row with `active = 0`; the current relation for a
-- (emitter_id, source_id, kind) triple is the highest relation_id. Reversibility is the
-- exploration-first rule: never an automatic delete (docs/07 first rule).
CREATE TABLE emitter_relation (
    relation_id    INTEGER PRIMARY KEY,
    -- The subordinate row: the candidate that defers.
    emitter_id     BLOB    NOT NULL REFERENCES emitter (emitter_id),
    -- What it defers to: the confirmed entry, the stronger duplicate, or the artifact's source.
    source_id      BLOB    NOT NULL REFERENCES emitter (emitter_id),
    kind           TEXT    NOT NULL CHECK (kind IN ('suppressed-by', 'duplicate-of', 'artifact-of')),
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
    -- JSON detail: the arithmetic (n, a, b, LO, predicted, error) or the rank terms.
    detail         TEXT,
    CHECK (emitter_id != source_id),
    CHECK ((artifact_kind IS NULL) = (kind != 'artifact-of'))
);
CREATE INDEX idx_emitter_relation_emitter ON emitter_relation (emitter_id, relation_id);
CREATE INDEX idx_emitter_relation_source  ON emitter_relation (source_id, relation_id);

CREATE TRIGGER emitter_relation_append_only BEFORE UPDATE ON emitter_relation
    BEGIN SELECT RAISE(ABORT, 'emitter relations are append-only; append a revocation row'); END;
CREATE TRIGGER emitter_relation_no_delete BEFORE DELETE ON emitter_relation
    BEGIN SELECT RAISE(ABORT, 'emitter relations are append-only; append a revocation row'); END;
