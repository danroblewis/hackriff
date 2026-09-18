-- T-374 (C40, docs/07 §2.32): a family of emitters that are harmonics of ONE FUNDAMENTAL THAT WAS
-- NEVER DETECTED.
--
-- `emitter_relation` (0008) cannot express this and is not extended to. Every row there names a
-- `source_id`: one emitter deferring to another emitter. A harmonic family's cause has no row —
-- T-317's ~2.3364 MHz oscillator sits outside every band that was ever tuned — and the claim binds
-- three or more emitters at once rather than a pair. A nullable `source_id` plus a synthetic
-- "emitter" for the fundamental would put a never-measured frequency into the inventory, which is
-- exactly the exploration-first rule's "the database never pre-populates the inventory" turned
-- inside out.
--
-- DEVICE-LOCAL (T-302/T-259). A harmonic family is manufactured by one oscillator and one mixer,
-- so the chain is on the family row and a family never crosses front ends. Same key as
-- `hk_model::relate::ReceiveChain`: device plus antenna port, the port compared only when both
-- sides recorded one.
--
-- APPEND-ONLY, like `emitter_relation`. A claim is never mutated or deleted; a revocation appends a
-- new row with `active = 0` and `supersedes` set to the row it retires, so the reasoning of both
-- survives. The members of the retired claim keep their emitters, detections and history.
CREATE TABLE harmonic_family (
    family_id             INTEGER PRIMARY KEY,
    -- The receive chain every member was measured on.
    device_id             TEXT    NOT NULL,
    antenna_port          TEXT,
    -- The fit: f = n * f0 + b over the members' measured centres.
    f0_hz                 REAL    NOT NULL CHECK (f0_hz > 0),
    intercept_hz          REAL    NOT NULL,
    intercept_se_hz       REAL    NOT NULL,
    -- se(b)/f0: how far the integer labelling is from the n <-> n+-1 coin flip at 0.5. The
    -- non-vacuity number.
    index_pin             REAL    NOT NULL,
    -- |b|/se(b) and |b|/f0: the two clauses of "the grid passes through the origin".
    origin_sigmas         REAL    NOT NULL,
    origin_fraction       REAL    NOT NULL,
    residual_rms_hz       REAL    NOT NULL,
    residual_tolerance_hz REAL    NOT NULL,
    -- The corroboration from a column the fit never touched: width = sigma0 * n.
    width_sigma0_hz       REAL    NOT NULL,
    width_ratio           REAL    NOT NULL,
    -- n_max/n_min, and whether it is enough for w ∝ n to be distinguishable from w = const. A
    -- family with `width_separates = 0` had its width agree, without that agreement separating the
    -- two models — recorded rather than counted as corroboration it is not.
    width_index_leverage  REAL    NOT NULL,
    width_separates       INTEGER NOT NULL CHECK (width_separates IN (0, 1)),
    -- Smallest pairwise line-shape correlation, when profiles were supplied.
    line_shape_corr       REAL,
    member_count          INTEGER NOT NULL CHECK (member_count >= 3),
    -- 1 = in force, 0 = revoked.
    active                INTEGER NOT NULL CHECK (active IN (0, 1)),
    -- The row this one retires, for a revocation or a re-claim.
    supersedes            INTEGER REFERENCES harmonic_family (family_id),
    t                     INTEGER NOT NULL,
    author                TEXT    NOT NULL CHECK (author IN ('system', 'user')),
    actor                 TEXT    NOT NULL,
    -- Backend-rendered reasoning, including the arithmetic. Always disclosed.
    reason                TEXT    NOT NULL
);
CREATE INDEX idx_harmonic_family_active ON harmonic_family (active, family_id);
CREATE INDEX idx_harmonic_family_chain  ON harmonic_family (device_id, family_id);

-- One emitter's place in a family: the index the fit gave it, and the two measurements the verdict
-- rests on.
CREATE TABLE harmonic_family_member (
    family_id   INTEGER NOT NULL REFERENCES harmonic_family (family_id),
    emitter_id  BLOB    NOT NULL REFERENCES emitter (emitter_id),
    -- Harmonic index n >= 2. n = 1 is the fundamental itself, which is `emitter_relation`'s case.
    n           INTEGER NOT NULL CHECK (n >= 2),
    f_center_hz REAL    NOT NULL,
    residual_hz REAL    NOT NULL,
    width_hz    REAL    NOT NULL,
    PRIMARY KEY (family_id, emitter_id)
);
CREATE INDEX idx_harmonic_family_member_emitter ON harmonic_family_member (emitter_id, family_id);

CREATE TRIGGER harmonic_family_append_only BEFORE UPDATE ON harmonic_family
    BEGIN SELECT RAISE(ABORT, 'harmonic families are append-only; append a revocation row'); END;
CREATE TRIGGER harmonic_family_no_delete BEFORE DELETE ON harmonic_family
    BEGIN SELECT RAISE(ABORT, 'harmonic families are append-only; append a revocation row'); END;
CREATE TRIGGER harmonic_family_member_append_only BEFORE UPDATE ON harmonic_family_member
    BEGIN SELECT RAISE(ABORT, 'harmonic family members are append-only'); END;
CREATE TRIGGER harmonic_family_member_no_delete BEFORE DELETE ON harmonic_family_member
    BEGIN SELECT RAISE(ABORT, 'harmonic family members are append-only'); END;
