-- T-913 (T-904 review, item 5): pin the detections an explanation cites.
--
-- `explanation.body` is JSON, so an `Evidence::Detection { id }` inside it names a detection that
-- no index can find: the retention pass's pin check (`repo/retention.rs`, class 2) would have to
-- scan and parse every explanation per candidate row, which it cannot afford. So the reference is
-- written out beside the body, one row per cited detection, and the pin check reads this table.
--
-- No backfill: at this migration no code path writes `Evidence::Detection` (the GNSS service
-- deliberately does not — `hk-context::gnss_service`), so there is nothing to recover from an
-- existing body, and an explanation written *after* this migration always writes its rows here
-- (`Repository::insert_explanation`). A cited detection that is already gone cannot be pinned and
-- is skipped, exactly as a track link to a pruned row is (`link_detections_on`).
CREATE TABLE explanation_detection (
    explanation_id BLOB NOT NULL REFERENCES explanation (explanation_id),
    detection_id   BLOB NOT NULL REFERENCES detection (detection_id),
    PRIMARY KEY (explanation_id, detection_id)
) WITHOUT ROWID;
-- The pin lookup: by detection, for one row at a time.
CREATE INDEX idx_explanation_detection_detection ON explanation_detection (detection_id);

-- T-913 (item 1), recorded here because a landed migration's text is never edited:
-- `detection_rollup.on_air_ns` is the **union** of its members' intervals, not — as 0019's comment
-- said — their sum. Co-timed members (an FSK signal's two lobes in one frame, linked to one track)
-- are one span of air, and the figure never exceeds the hull.
