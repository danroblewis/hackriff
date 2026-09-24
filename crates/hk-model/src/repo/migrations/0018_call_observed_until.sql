-- T-308 (C23, follows T-269): how far a call was actually WATCHED, so `t_end IS NULL` stops
-- meaning two different things.
--
-- T-269 measures a call's end as 90 ms of silence on the granted channel, derived from a keyed P25
-- transmitter's 180 ms LLDU cadence. A transmission still keyed when the BUFFERED WINDOW ends has
-- no such end, so the row was written with `t_end IS NULL` -- correct, and deliberately not the
-- window edge, because closing there would manufacture a boundary the radio never produced.
--
-- But `t_end IS NULL` already meant "open, still running", and it also had to mean "we stopped
-- looking". Those are opposite claims: the first says the call reaches the live edge, the second
-- says nothing is known after some instant. A consumer cannot tell them apart from the columns,
-- which is the observation-measuring-itself defect (T-281) arriving in the call layer: every call
-- longer than the dwell reads as whatever the reader assumes about NULL.
--
-- `observed_until` is the third fact that separates them: the last instant this receiver was
-- actually observing the call's channel.
--
--   t_end IS NOT NULL                             -> the end was OBSERVED (silence timeout).
--   t_end IS NULL AND observed_until IS NOT NULL   -> TRUNCATED: present at `observed_until`, and
--                                                     nothing whatever is claimed after it. The
--                                                     duration is a LOWER BOUND.
--   t_end IS NULL AND observed_until IS NULL       -> never observed at all (a grant outside the
--                                                     window, or an unmapped channel): the call
--                                                     happened, this receiver did not watch it.
--
-- The column is a measurement, not an interpretation: it is where observation stopped, which is a
-- property of the receiver's schedule, and it is the same discipline as the coverage map's grey
-- (unobserved is not quiet) and the open interval's cap (assumed air is drawn as assumption).
ALTER TABLE call_record ADD COLUMN observed_until INTEGER;

-- The constraints ALTER TABLE ADD COLUMN cannot carry, as a trigger pair instead: observation
-- cannot stop before the call started, and a call whose end was observed was by construction still
-- being watched at that end.
CREATE TRIGGER call_record_observed_until_insert
    BEFORE INSERT ON call_record
    WHEN NEW.observed_until IS NOT NULL
         AND (NEW.observed_until < NEW.t_start
              OR (NEW.t_end IS NOT NULL AND NEW.observed_until < NEW.t_end))
    BEGIN SELECT RAISE(ABORT,
        'observed_until precedes the call it observed'); END;

CREATE TRIGGER call_record_observed_until_update
    BEFORE UPDATE ON call_record
    WHEN NEW.observed_until IS NOT NULL
         AND (NEW.observed_until < NEW.t_start
              OR (NEW.t_end IS NOT NULL AND NEW.observed_until < NEW.t_end))
    BEGIN SELECT RAISE(ABORT,
        'observed_until precedes the call it observed'); END;

-- "Which calls were truncated?" is a question the follower's own audit asks every pass, and it is
-- the one query that cannot be served by `idx_call_record_open` alone.
CREATE INDEX idx_call_record_truncated ON call_record (trunk_system_id, observed_until)
    WHERE t_end IS NULL AND observed_until IS NOT NULL;
