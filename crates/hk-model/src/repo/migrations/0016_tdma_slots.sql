-- T-272 (C23): the slot count a channel-table entry divides its channel numbers by.
--
-- P25 Phase 2 announces its band plan with IDEN_UP_TDMA, which carries a channel TYPE naming how
-- many TDMA slots share one carrier. A channel number on such an entry resolves as
-- `f = base + spacing × (channel / slots)` and names slot `channel % slots` — so two consecutive
-- channel numbers are two talkgroups on ONE frequency, not two frequencies. Storing the plan
-- without `slots` would leave every stored entry ambiguous between the two readings, and a reader
-- re-deriving a frequency from it would land half a channel out: C23's TDMA slot mix-up pitfall,
-- preserved in the database.
--
-- 1 is FDMA and the only value an entry decoded before this migration could have meant, so the
-- default restates what those rows already said rather than assuming anything new about them.
ALTER TABLE trunk_channel_plan
    ADD COLUMN slots INTEGER NOT NULL DEFAULT 1 CHECK (slots >= 1 AND slots <= 4);
