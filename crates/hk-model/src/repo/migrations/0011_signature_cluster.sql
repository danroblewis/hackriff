-- T-202 (ADR-0016 §5, §9): C18 clusters of unknown emissions — "the same thing I saw before".
-- Types: hk_model::signature::cluster.
--
-- A cluster is a **type above emitters**, which are instances, and it is **evidence, never
-- identity**: no column here can name, classify or promote an emitter, and nothing in these
-- tables is allowed to write to `emitter`. Membership says "this emission measures like those
-- others"; only a CRC-valid decode confirms what something is (the exploration-first rule in
-- CLAUDE.md, ADR-0016 §5).
--
-- Clusters are formed from measurement alone (`emission_features`). The catalogue plays no part
-- in forming one, so a signature can never feed back into what was measured.

-- The current state of each cluster. Mutable (a centroid moves as members are folded in); its
-- history lives in the append-only `cluster_event` beside it.
CREATE TABLE signature_cluster (
    cluster_id        TEXT    PRIMARY KEY,
    state             TEXT    NOT NULL CHECK (state IN ('pending', 'active', 'merged',
                                                        'promoted')),
    -- The survivor, when a repair pass folded this cluster into another. Ids of the larger side
    -- survive, and the loser's row is kept so an old link still resolves.
    merged_into       TEXT    REFERENCES signature_cluster (cluster_id),
    -- The signature minted from it, when promoted. Still a hypothesis, never an identity.
    signature_id      TEXT,
    signature_version INTEGER CHECK (signature_version IS NULL OR signature_version >= 1),
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL CHECK (updated_at >= created_at),
    -- Member folds counted into the centroid, and the largest suspect_fraction of any member: a
    -- signature is never minted from an all-suspect cluster (C18 card).
    folds             INTEGER NOT NULL CHECK (folds >= 0),
    suspect_fraction  REAL    NOT NULL CHECK (suspect_fraction >= 0.0 AND suspect_fraction <= 1.0),
    -- The full hk_model::signature::cluster::SignatureCluster as JSON (centroid with per-field
    -- value, sigma, spread and n).
    body              TEXT    NOT NULL,
    CHECK ((state = 'merged') = (merged_into IS NOT NULL)),
    CHECK (merged_into IS NULL OR merged_into != cluster_id),
    CHECK ((signature_id IS NULL) = (signature_version IS NULL))
);
CREATE INDEX idx_signature_cluster_state ON signature_cluster (state, cluster_id);

-- Append-only membership with supersession, like `emitter_link`: one row each time an emitter's
-- cluster changes. The current membership is the highest link_id for an emitter, and a NULL
-- cluster_id is an explicit record that the clusterer **abstained** — which is a different
-- statement from never having looked, and the one it prefers to a doubtful join.
CREATE TABLE emitter_cluster (
    link_id     INTEGER PRIMARY KEY,
    emitter_id  BLOB    NOT NULL REFERENCES emitter (emitter_id),
    cluster_id  TEXT    REFERENCES signature_cluster (cluster_id),
    t           INTEGER NOT NULL,
    -- Machine reason code: joined, seeded, reassigned, ambiguous, too_few_fields, conflict,
    -- too_far, repair.
    reason      TEXT    NOT NULL,
    -- The tolerance-normalised distance that decided it, when one was computable.
    distance    REAL
);
CREATE INDEX idx_emitter_cluster_emitter ON emitter_cluster (emitter_id, link_id);
CREATE INDEX idx_emitter_cluster_cluster ON emitter_cluster (cluster_id, link_id);

CREATE TRIGGER emitter_cluster_append_only BEFORE UPDATE ON emitter_cluster
    BEGIN SELECT RAISE(ABORT, 'cluster membership is append-only; append a new link'); END;
CREATE TRIGGER emitter_cluster_no_delete BEFORE DELETE ON emitter_cluster
    BEGIN SELECT RAISE(ABORT, 'cluster membership is append-only; append a new link'); END;

-- Append-only history: what happened to a cluster and the arithmetic behind it.
CREATE TABLE cluster_event (
    event_id         INTEGER PRIMARY KEY,
    cluster_id       TEXT    NOT NULL REFERENCES signature_cluster (cluster_id),
    kind             TEXT    NOT NULL CHECK (kind IN ('created', 'activated', 'merge', 'split',
                                                      'reassign', 'promoted')),
    other_cluster_id TEXT    REFERENCES signature_cluster (cluster_id),
    emitter_id       BLOB    REFERENCES emitter (emitter_id),
    t                INTEGER NOT NULL,
    detail           TEXT    NOT NULL
);
CREATE INDEX idx_cluster_event_cluster ON cluster_event (cluster_id, event_id);

CREATE TRIGGER cluster_event_append_only BEFORE UPDATE ON cluster_event
    BEGIN SELECT RAISE(ABORT, 'cluster events are append-only; append a new event'); END;
CREATE TRIGGER cluster_event_no_delete BEFORE DELETE ON cluster_event
    BEGIN SELECT RAISE(ABORT, 'cluster events are append-only; append a new event'); END;
