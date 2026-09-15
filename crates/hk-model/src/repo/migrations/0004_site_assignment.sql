-- T-136 (ADR-0012 §3.5): the site assignment in force, so a restart on the same database keeps a
-- pinned site (and a fixed site within its no-fix hold). At most one row; no row = nothing to
-- restore. `body` is the `attention::baseline::SiteAssignment` JSON.
CREATE TABLE site_assignment (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    site_id   BLOB    NOT NULL CHECK (length(site_id) = 16) REFERENCES site (site_id),
    body      TEXT    NOT NULL
);
