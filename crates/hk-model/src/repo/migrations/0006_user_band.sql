-- T-191: a user-adjusted band (f_lo/f_hi) on an inventory entry. Stored beside the emitter's
-- measured extent, which is never overwritten (blind detection stays the source of truth). One
-- current override per emitter: setting replaces it, clearing deletes it. Rows of an emitter
-- merged into another stay as history; the merge copies the newest override onto the survivor.
CREATE TABLE emitter_user_band (
    emitter_id  BLOB    PRIMARY KEY REFERENCES emitter (emitter_id),
    f_lo        REAL    NOT NULL CHECK (f_lo > 0),
    f_hi        REAL    NOT NULL,
    set_at      INTEGER NOT NULL,
    actor       TEXT    NOT NULL,
    reason      TEXT,
    CHECK (f_hi > f_lo)
);
