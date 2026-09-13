-- 0002 (T-006 review): allow spur_reason = 'clock-harmonic' (SpurReason::ClockHarmonic).
--
-- SQLite cannot ALTER a CHECK constraint, and a 12-step table rebuild of `detection` would need
-- foreign keys off (track_detection, recording and annotation reference it), which cannot be
-- changed inside the migration transaction. This migration only *widens* the IN list, so every
-- existing row still satisfies the new constraint; that is the case the SQLite documentation
-- allows editing the stored schema text for ("Making Other Kinds Of Table Schema Changes",
-- sqlite.org/lang_altertable.html). The migration runner bumps `PRAGMA schema_version` after a
-- migration that uses writable_schema so every connection reloads the schema.
PRAGMA writable_schema = ON;
UPDATE sqlite_schema
   SET sql = replace(sql,
                     '''lo-relative'', ''comb'', ''spur-map'')',
                     '''lo-relative'', ''comb'', ''spur-map'', ''clock-harmonic'')')
 WHERE type = 'table' AND name = 'detection';
PRAGMA writable_schema = OFF;
