-- T-266 (docs/07 §2.28–§2.30; docs/capabilities/C23-trunking-follow.md §Interface): the C23
-- trunking metadata model — the system a control channel describes, the calls followed off it, and
-- the grant stream underneath them. Types: hk_model::trunking.
--
-- Migration number: 0012 is reserved by ADR-0017 §9 (TM-5) for `idx_emitter_observation_time`, so
-- this takes 0013. The MIGRATIONS array in repo/mod.rs is what actually applies, in array order;
-- file numbers are labels, so 0012 appending after this one later is correct and harmless.
--
-- METADATA ONLY. There is no call-audio table, and no column below can hold audio, voice frames or
-- any message payload — deliberately, so the roadmap's vocoder-IP question never gates M4. Adding
-- one is a separate decision, not an implementation detail. (`no_audio_column_exists` in
-- repo/trunking.rs asserts it.)
--
-- ENCRYPTION IS THREE-STATE: 'clear' / 'encrypted' / 'unknown', with NO default-to-clear path.
-- The C23 card's pitfall is "late entry without a header: encryption status should default to
-- 'unknown', not 'clear'". Three structural facts on `call_record` and `grant_event` enforce it,
-- rather than a convention a later writer can forget:
--   1. `encryption` is NOT NULL and has **no DEFAULT**, so an INSERT that does not decide fails.
--   2. `CHECK ((encryption = 'unknown') = (encryption_evidence IS NULL))` — a claim must name what
--      said so, and "nothing said" cannot be dressed up as a measurement.
--   3. `CHECK (encryption != 'clear' OR algid IS NULL OR algid = 128)` — a row cannot call itself
--      clear while carrying an encrypting ALGID (0x80 is the only clear one; docs/04 §8.3).
-- A trigger additionally refuses the one unsafe transition: a call once seen encrypted never
-- becomes clear or unknown. Nothing here decrypts anything; this is a correctness and safety
-- requirement, not a legal one.

-- ---------------------------------------------------------------------------------------------
-- The system
-- ---------------------------------------------------------------------------------------------

-- Fully columnar (no `body`): every field is a scalar, and the three child tables below are
-- authoritative for the channel table, the neighbours and the talkgroups. Keeping one copy of each
-- fact is worth the deviation from the `body`-JSON convention here, because `encryption` on the
-- call rows below must exist in exactly one place.
CREATE TABLE trunk_system (
    trunk_system_id BLOB    PRIMARY KEY,
    protocol        TEXT    NOT NULL CHECK (protocol IN ('p25-phase1', 'p25-phase2', 'dmr-tier3',
                                                         'smart-net', 'edacs', 'nxdn-type-c',
                                                         'mpt1327', 'unknown')),
    -- Decoded identifiers. NULL means "not decoded yet" — a control channel is found (T-267)
    -- before it is identified (T-268).
    system_id       TEXT,
    site_id         TEXT,
    -- Control-channel frequency. NULL means the system has NO dedicated control channel
    -- (Capacity Plus rest channel, NXDN Type-D, LTR), never 0.
    cc_freq_hz      REAL    CHECK (cc_freq_hz IS NULL OR cc_freq_hz > 0.0),
    first_seen      INTEGER NOT NULL,
    last_seen       INTEGER NOT NULL CHECK (last_seen >= first_seen),
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL CHECK (updated_at >= created_at)
);
-- The natural key. SQLite treats NULLs as distinct in a UNIQUE index, which is the behaviour we
-- want: two systems whose ids have not been decoded stay two rows, because nothing has shown them
-- to be the same thing.
CREATE UNIQUE INDEX idx_trunk_system_key ON trunk_system (protocol, system_id, site_id);
CREATE INDEX idx_trunk_system_cc ON trunk_system (cc_freq_hz);

-- The channel table: f = base + spacing × channel, with the uplink offset (P25 IDEN_UP).
-- APPEND-ONLY with the decode time, so a STALE table is detectable rather than silently mapping a
-- grant to the wrong frequency (a C23 pitfall; T-268 asserts it). The current entry for an iden is
-- the one with the highest `t`.
CREATE TABLE trunk_channel_plan (
    entry_id        INTEGER PRIMARY KEY,
    trunk_system_id BLOB    NOT NULL REFERENCES trunk_system (trunk_system_id),
    iden            INTEGER NOT NULL CHECK (iden >= 0 AND iden <= 255),
    base_hz         REAL    NOT NULL CHECK (base_hz > 0.0),
    spacing_hz      REAL    NOT NULL CHECK (spacing_hz > 0.0),
    tx_offset_hz    REAL    NOT NULL,
    bandwidth_hz    REAL    CHECK (bandwidth_hz IS NULL OR bandwidth_hz > 0.0),
    t               INTEGER NOT NULL
);
CREATE INDEX idx_trunk_channel_plan ON trunk_channel_plan (trunk_system_id, iden, t);

CREATE TRIGGER trunk_channel_plan_append_only BEFORE UPDATE ON trunk_channel_plan
    BEGIN SELECT RAISE(ABORT, 'the channel table is append-only; append a newer entry'); END;
CREATE TRIGGER trunk_channel_plan_no_delete BEFORE DELETE ON trunk_channel_plan
    BEGIN SELECT RAISE(ABORT, 'the channel table is append-only; append a newer entry'); END;

-- Neighbour sites a control channel announced. Append-only: an announcement is a measurement.
CREATE TABLE trunk_neighbour (
    neighbour_id    INTEGER PRIMARY KEY,
    trunk_system_id BLOB    NOT NULL REFERENCES trunk_system (trunk_system_id),
    site_id         TEXT,
    cc_freq_hz      REAL    CHECK (cc_freq_hz IS NULL OR cc_freq_hz > 0.0),
    t               INTEGER NOT NULL,
    -- An announcement that names neither a site nor a frequency says nothing.
    CHECK (site_id IS NOT NULL OR cc_freq_hz IS NOT NULL)
);
CREATE INDEX idx_trunk_neighbour ON trunk_neighbour (trunk_system_id, neighbour_id);

CREATE TRIGGER trunk_neighbour_append_only BEFORE UPDATE ON trunk_neighbour
    BEGIN SELECT RAISE(ABORT, 'neighbour announcements are append-only'); END;
CREATE TRIGGER trunk_neighbour_no_delete BEFORE DELETE ON trunk_neighbour
    BEGIN SELECT RAISE(ABORT, 'neighbour announcements are append-only'); END;

-- Talkgroups seen on a system. An aggregate (counters), rebuildable from `call_record`.
-- `label` is a SUGGESTION from a prior (C17 RadioReference) or a user — never truth, and never
-- what a call is matched on.
CREATE TABLE trunk_talkgroup (
    trunk_system_id BLOB    NOT NULL REFERENCES trunk_system (trunk_system_id),
    talkgroup       TEXT    NOT NULL,
    label           TEXT,
    label_source    TEXT    CHECK (label_source IN ('user', 'prior')),
    first_seen      INTEGER NOT NULL,
    last_seen       INTEGER NOT NULL CHECK (last_seen >= first_seen),
    calls           INTEGER NOT NULL CHECK (calls >= 0),
    PRIMARY KEY (trunk_system_id, talkgroup),
    CHECK ((label IS NULL) = (label_source IS NULL))
) WITHOUT ROWID;

-- ---------------------------------------------------------------------------------------------
-- Calls
-- ---------------------------------------------------------------------------------------------

CREATE TABLE call_record (
    call_id             BLOB    PRIMARY KEY,
    trunk_system_id     BLOB    NOT NULL REFERENCES trunk_system (trunk_system_id),
    t_start             INTEGER NOT NULL,
    -- NULL while the call is open, or when its end was never observed.
    t_end               INTEGER CHECK (t_end IS NULL OR t_end >= t_start),
    talkgroup           TEXT,
    unit_id             TEXT,
    channel             TEXT,
    -- TDMA slot, NULL when none was attributed (FDMA, or a slot mix-up avoided by not guessing).
    slot                INTEGER CHECK (slot IS NULL OR (slot >= 0 AND slot <= 3)),
    -- Voice-channel frequency, NULL when the grant's channel could not be mapped.
    f_hz                REAL    CHECK (f_hz IS NULL OR f_hz > 0.0),
    -- THE THREE-STATE COLUMN. NOT NULL and deliberately WITHOUT A DEFAULT: a writer that does not
    -- decide gets an error, not 'clear'.
    encryption          TEXT    NOT NULL CHECK (encryption IN ('clear', 'encrypted', 'unknown')),
    -- What decided it. Paired with the state by the CHECK below.
    encryption_evidence TEXT    CHECK (encryption_evidence IN ('algid', 'service-options',
                                                               'dmr-pi', 'lc-header', 'user')),
    algid               INTEGER CHECK (algid IS NULL OR (algid >= 0 AND algid <= 255)),
    key_id              INTEGER CHECK (key_id IS NULL OR (key_id >= 0 AND key_id <= 65535)),
    -- 1 = joined without seeing the call's header. Recorded, because it is exactly the case whose
    -- encryption state must stay 'unknown'.
    late_entry          INTEGER NOT NULL CHECK (late_entry IN (0, 1)),
    -- The emission this call rode on, once the inventory has one.
    emitter_id          BLOB    REFERENCES emitter (emitter_id),
    -- JSON array of machine reasons ('grant-outside-window', 'stale-iden', 'silence-timeout'…).
    reasons             TEXT    NOT NULL,
    -- A claim names its source, and "nothing said" carries none.
    CHECK ((encryption = 'unknown') = (encryption_evidence IS NULL)),
    CHECK (algid IS NULL OR encryption_evidence IS NOT NULL),
    CHECK (key_id IS NULL OR encryption_evidence IS NOT NULL),
    -- 0x80 is the only clear P25 ALGID; a clear row cannot carry an encrypting one.
    CHECK (encryption != 'clear' OR algid IS NULL OR algid = 128),
    CHECK (encryption != 'encrypted' OR algid IS NULL OR algid != 128)
);
CREATE INDEX idx_call_record_system ON call_record (trunk_system_id, t_start);
CREATE INDEX idx_call_record_talkgroup ON call_record (trunk_system_id, talkgroup, t_start);
CREATE INDEX idx_call_record_open ON call_record (trunk_system_id, t_end);
CREATE INDEX idx_call_record_emitter ON call_record (emitter_id);

-- A call is an aggregate, so it may be updated (its end fills in, later evidence sharpens the
-- encryption state) — but never in the unsafe direction. A call once seen encrypted stays
-- encrypted: a mid-call key change must not read as "listenable after all".
CREATE TRIGGER call_record_never_downgrades_from_encrypted
    BEFORE UPDATE OF encryption ON call_record
    WHEN OLD.encryption = 'encrypted' AND NEW.encryption != 'encrypted'
    BEGIN SELECT RAISE(ABORT,
        'a call once seen encrypted never becomes clear or unknown'); END;

-- ---------------------------------------------------------------------------------------------
-- The grant stream
-- ---------------------------------------------------------------------------------------------

-- Append-only: what the control channel said, when it said it. Drives the metadata-only load index
-- (AWARE-067, T-273). Each event carries the encryption state THAT MESSAGE stated — which for a
-- bare grant update is 'unknown'.
CREATE TABLE grant_event (
    event_id            INTEGER PRIMARY KEY,
    trunk_system_id     BLOB    NOT NULL REFERENCES trunk_system (trunk_system_id),
    call_id             BLOB    REFERENCES call_record (call_id),
    kind                TEXT    NOT NULL CHECK (kind IN ('grant', 'grant-update', 'call-start',
                                                         'call-end', 'denied', 'outside-window',
                                                         'unmapped-channel')),
    t                   INTEGER NOT NULL,
    talkgroup           TEXT,
    unit_id             TEXT,
    channel             TEXT,
    slot                INTEGER CHECK (slot IS NULL OR (slot >= 0 AND slot <= 3)),
    f_hz                REAL    CHECK (f_hz IS NULL OR f_hz > 0.0),
    encryption          TEXT    NOT NULL CHECK (encryption IN ('clear', 'encrypted', 'unknown')),
    encryption_evidence TEXT    CHECK (encryption_evidence IN ('algid', 'service-options',
                                                               'dmr-pi', 'lc-header', 'user')),
    algid               INTEGER CHECK (algid IS NULL OR (algid >= 0 AND algid <= 255)),
    key_id              INTEGER CHECK (key_id IS NULL OR (key_id >= 0 AND key_id <= 65535)),
    -- JSON detail: the opcode, the mapping arithmetic, why a channel was unmapped.
    detail              TEXT    NOT NULL,
    CHECK ((encryption = 'unknown') = (encryption_evidence IS NULL)),
    CHECK (algid IS NULL OR encryption_evidence IS NOT NULL),
    CHECK (key_id IS NULL OR encryption_evidence IS NOT NULL),
    CHECK (encryption != 'clear' OR algid IS NULL OR algid = 128),
    CHECK (encryption != 'encrypted' OR algid IS NULL OR algid != 128)
);
CREATE INDEX idx_grant_event_system ON grant_event (trunk_system_id, t);
CREATE INDEX idx_grant_event_call ON grant_event (call_id, event_id);

CREATE TRIGGER grant_event_append_only BEFORE UPDATE ON grant_event
    BEGIN SELECT RAISE(ABORT, 'grant events are append-only; append a new event'); END;
CREATE TRIGGER grant_event_no_delete BEFORE DELETE ON grant_event
    BEGIN SELECT RAISE(ABORT, 'grant events are append-only; append a new event'); END;
