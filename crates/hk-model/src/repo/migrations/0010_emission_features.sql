-- T-201 (ADR-0016 §5, §9): C18 EmissionFeatures storage. ADR-0016 lists `emission_features`
-- among the C18 tables; T-218's 0009 landed the catalogue and the match log, leaving the measured
-- side (which T-218 could not write, since `EmissionFeatures` is T-201's type) to this migration.
-- Types: hk_model::signature::EmissionFeatures.
--
-- This is the **measured** side of C18 and nothing else: it is written from blind detection and
-- estimation only. No row here is derived from the catalogue, so a signature can never feed back
-- into what was measured (the exploration-first rule in CLAUDE.md).

-- Append-only snapshots per emitter. A new row is written when a field moves by more than its
-- sigma or enough new observations have been folded in, so the history of what an emitter looked
-- like over time is kept rather than overwritten.
CREATE TABLE emission_features (
    features_id   TEXT    PRIMARY KEY,
    emitter_id    BLOB    NOT NULL REFERENCES emitter (emitter_id),
    t             INTEGER NOT NULL,
    -- Field-set version (hk_model::signature::EMISSION_FEATURES_VERSION), so a reader can tell a
    -- snapshot written under an older field set from a current one.
    version       INTEGER NOT NULL CHECK (version >= 1),
    -- Observations folded in, and the fraction of them carrying a suspect flag (clipped, IMD,
    -- image, spur). A signature is never minted from an all-suspect emitter (C18 card).
    observations  INTEGER NOT NULL CHECK (observations >= 0),
    suspect_fraction REAL NOT NULL CHECK (suspect_fraction >= 0.0 AND suspect_fraction <= 1.0),
    -- The full hk_model::signature::EmissionFeatures as JSON (per-field value, sigma, spread,
    -- agreement, n and method).
    body          TEXT    NOT NULL
);
CREATE INDEX idx_emission_features_emitter ON emission_features (emitter_id, t);

CREATE TRIGGER emission_features_append_only BEFORE UPDATE ON emission_features
    BEGIN SELECT RAISE(ABORT, 'emission features are append-only; append a new snapshot'); END;
CREATE TRIGGER emission_features_no_delete BEFORE DELETE ON emission_features
    BEGIN SELECT RAISE(ABORT, 'emission features are append-only; append a new snapshot'); END;
