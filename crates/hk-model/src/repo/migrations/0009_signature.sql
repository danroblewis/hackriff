-- T-218 (ADR-0016 §5, §9): C18 signature storage. ADR-0016 names these tables in its 0006
-- migration; 0006-0008 were taken (user band, classification, emitter relation), so they land
-- here. Types: hk_model::signature. Matching, minting and clustering are T-201/T-202: this
-- migration is the storage only, so those tasks never have to touch MIGRATIONS.
--
-- A signature is a *hypothesis catalogue entry*, never an identity and never a status: a match
-- ranks explanations (ADR-0016 §5, the exploration-first rule in CLAUDE.md). Nothing here
-- pre-populates the inventory, and no row in these tables can change what was measured.

-- Immutable per (signature_id, version), like a recipe. A new version is a new row; `retired_at`
-- is the only mutable column (retiring keeps every version readable).
CREATE TABLE signature (
    signature_id  TEXT    NOT NULL,
    version       INTEGER NOT NULL CHECK (version >= 1),
    name          TEXT    NOT NULL,
    kind          TEXT    NOT NULL CHECK (kind IN ('protocol', 'device-type', 'rfi', 'radar',
                                                   'learned')),
    -- Taxonomy reference (e.g. 'hk-mod@1') and the family it expects, both optional: a signature
    -- may be modulation-agnostic (an RFI comb), and a family never gates on its own.
    taxonomy      TEXT,
    family        TEXT,
    -- How the entry came to exist. 'rtl433-import' is untrusted input, validated on the way in.
    provenance    TEXT    NOT NULL CHECK (provenance IN ('builtin', 'user', 'recipe-confirmed',
                                                         'rtl433-import', 'cluster-promoted')),
    author        TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,
    -- The version this one supersedes, or NULL.
    supersedes    INTEGER CHECK (supersedes IS NULL OR supersedes >= 1),
    -- The full hk_model::signature::Signature as JSON (fields, tolerances, weights, bands, recipe).
    body          TEXT    NOT NULL,
    retired_at    INTEGER,
    PRIMARY KEY (signature_id, version)
);
CREATE INDEX idx_signature_kind ON signature (kind, signature_id);

-- Content is immutable; only retirement may be recorded.
CREATE TRIGGER signature_immutable
    BEFORE UPDATE OF signature_id, version, name, kind, taxonomy, family, provenance, author,
                     created_at, supersedes, body ON signature
    BEGIN SELECT RAISE(ABORT, 'a signature version is immutable; write a new version'); END;
CREATE TRIGGER signature_no_delete BEFORE DELETE ON signature
    BEGIN SELECT RAISE(ABORT, 'signatures are retired, never deleted'); END;

-- Append-only: one row per emitter each time the outcome or the top candidate changes. The
-- current match is the highest match_id for an emitter. A match never sets identity, known_status
-- or lifecycle (ADR-0016 §5) — it is ranked evidence with its arithmetic disclosed.
CREATE TABLE signature_match (
    match_id      INTEGER PRIMARY KEY,
    emitter_id    BLOB    NOT NULL REFERENCES emitter (emitter_id),
    t             INTEGER NOT NULL,
    outcome       TEXT    NOT NULL CHECK (outcome IN ('full', 'partial', 'none')),
    -- Top candidate (NULL on 'none'), so the common read needs no JSON parse.
    signature_id  TEXT,
    version       INTEGER CHECK (version IS NULL OR version >= 1),
    score         REAL,
    -- The EmissionFeatures snapshot it was computed from (opaque until T-201), and the signature
    -- store revision, so a match can be re-derived exactly.
    features_ref  TEXT,
    signatures_rev INTEGER NOT NULL,
    -- The full hk_model::signature::SignatureMatch as JSON (ranked candidates, per-field
    -- agreement, missing and conflicting fields, reasons).
    body          TEXT    NOT NULL,
    CHECK ((signature_id IS NULL) = (outcome = 'none')),
    CHECK ((signature_id IS NULL) = (version IS NULL))
);
CREATE INDEX idx_signature_match_emitter ON signature_match (emitter_id, match_id);

CREATE TRIGGER signature_match_append_only BEFORE UPDATE ON signature_match
    BEGIN SELECT RAISE(ABORT, 'signature matches are append-only; append a new match'); END;
CREATE TRIGGER signature_match_no_delete BEFORE DELETE ON signature_match
    BEGIN SELECT RAISE(ABORT, 'signature matches are append-only; append a new match'); END;
