-- T-119 (ADR-0012 §3.5, §4.2, §9): discrete sites and versioned interestingness score weights.
-- User metadata: kept (no retention). Series (baselines) live in hk-store files, not here.

-- Discrete sites. `body` is the full `attention::baseline::SiteRecord` JSON; the columns index it.
CREATE TABLE site (
    site_id        BLOB    PRIMARY KEY CHECK (length(site_id) = 16),
    name           TEXT    CHECK (name IS NULL OR length(name) BETWEEN 1 AND 64),
    lat_deg        REAL,
    lon_deg        REAL,
    radius_m       REAL    NOT NULL CHECK (radius_m > 0),
    utc_offset_min INTEGER NOT NULL CHECK (utc_offset_min BETWEEN -840 AND 840),
    source         TEXT    NOT NULL CHECK (source IN ('config', 'user', 'gnss')),
    first_seen     INTEGER NOT NULL,
    last_seen      INTEGER NOT NULL CHECK (last_seen >= first_seen),
    body           TEXT    NOT NULL
) WITHOUT ROWID;
CREATE UNIQUE INDEX idx_site_name ON site (name) WHERE name IS NOT NULL;
CREATE INDEX idx_site_last_seen ON site (last_seen);

-- Score weights, one row per version (version 1 = the built-in defaults, never stored). `body` is
-- the `attention::score::ScoreWeights` JSON; the column `version` is authoritative. Append-only.
CREATE TABLE attention_weights (
    version     INTEGER PRIMARY KEY CHECK (version >= 2),
    created_at  INTEGER NOT NULL,
    author      TEXT    NOT NULL,
    body        TEXT    NOT NULL
);
CREATE TRIGGER attention_weights_append_only BEFORE UPDATE ON attention_weights
    BEGIN SELECT RAISE(ABORT, 'score weights are versioned and append-only'); END;
CREATE TRIGGER attention_weights_no_delete BEFORE DELETE ON attention_weights
    BEGIN SELECT RAISE(ABORT, 'score weights are versioned and append-only'); END;
