-- T-211 (ADR-0016 §2, §9): M3 classification fields on emitter_classification, all nullable.
-- ADR-0016 names this migration 0006; T-191 took 0006 (user band), so it is 0007.
-- Legacy columns (family, confidence, open_set_score, model_version, input_*) keep their meaning,
-- and pre-M3 rows keep NULLs here: readers derive their stage and rank (hk_model::classify::rank).
-- `detail` is the full hk_model::classify::Classification as JSON. The append-only trigger is
-- unchanged. Repository code keeps `stage` and `arb_rank` both NULL or both set.
ALTER TABLE emitter_classification ADD COLUMN taxonomy TEXT;
ALTER TABLE emitter_classification ADD COLUMN stage TEXT
    CHECK (stage IS NULL OR stage IN ('feature-tree', 'verifier', 'dl', 'decoder', 'user',
                                      'chain', 'track-shape'));
ALTER TABLE emitter_classification ADD COLUMN arb_rank INTEGER
    CHECK (arb_rank IS NULL OR arb_rank BETWEEN 0 AND 4);
ALTER TABLE emitter_classification ADD COLUMN detail TEXT;
