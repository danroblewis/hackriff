-- T-222 (AWARE-053, C40 content half): one emission arriving over two paths.
--
-- Every relation kind before this one is claimed from GEOMETRY -- an overlap of bands, a mixer
-- arithmetic, a slope against the local oscillator. None of them can see two rows that overlap
-- nowhere, sit on no predicted artefact frequency, and are nonetheless the same transmission
-- reaching the antenna twice over paths of different length. What says so is what the two rows
-- CARRY: the same content, one copy delayed and attenuated, with the correlation lag giving the
-- path delay (`hk_model::multipath`, `hk_dsp::xcorr`).
--
-- It is its own kind, for the reason `retune-sibling-of` is: `duplicate-of` and `artifact-of` are
-- claimed AND REVOKED by the T-219 overlap resolver over *overlapping bands*, and these two rows
-- need not overlap at all. Sharing a kind would have the two rules revoking each other on every
-- pass. It is also the more honest vocabulary -- the row it defers to is the earlier arrival of
-- the same emission, not a source that manufactured it.
--
-- The claim is APPEND-ONLY and REVOCABLE like every other row here: the deferring row keeps its
-- id, detections, tracks, links and history, and revives when the evidence changes. Nothing is
-- deleted, and a relationship is ranked evidence with its arithmetic disclosed, never truth.
--
-- SQLite cannot relax a CHECK in place, so the table is rebuilt (its append-only triggers and
-- indexes with it) and every existing row copied. Nothing is dropped or rewritten.
DROP TRIGGER emitter_relation_append_only;
DROP TRIGGER emitter_relation_no_delete;
ALTER TABLE emitter_relation RENAME TO emitter_relation_old;

CREATE TABLE emitter_relation (
    relation_id    INTEGER PRIMARY KEY,
    -- The subordinate row: the candidate that defers.
    emitter_id     BLOB    NOT NULL REFERENCES emitter (emitter_id),
    -- What it defers to: the confirmed entry, the stronger duplicate, the artifact's source,
    -- (retune-sibling-of) the sighting that represents the LO-relative family, or (multipath-of)
    -- the earlier, stronger arrival of the same emission.
    source_id      BLOB    NOT NULL REFERENCES emitter (emitter_id),
    kind           TEXT    NOT NULL CHECK (kind IN ('suppressed-by', 'duplicate-of', 'artifact-of',
                                                    'retune-sibling-of', 'multipath-of')),
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
    -- JSON detail: the arithmetic (n, a, b, LO, predicted, error), the rank terms, the retune
    -- slope and invariant coordinate, or the multipath delay, correlation and path difference.
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
