-- T-1017: declared identity labels. A decoder states which of its output fields is the
-- human-readable identity label and which its confidence; nothing is ever inferred from a field's
-- name. One declaration per (decoder_id, frame_model) — a decoder's row kind is what fixes where
-- its label lives. Declarations are stored beside the decodes they describe so a database serves
-- the same label after the process that decoded it has gone; the built-in chains' declarations are
-- compiled in (IdentityLabelRegistry::builtin) and need no row.
CREATE TABLE identity_label_decl (
    decoder_id         TEXT NOT NULL,
    frame_model        TEXT NOT NULL,
    label_field        TEXT NOT NULL,
    confidence_from    TEXT CHECK (confidence_from IN ('field', 'vote-counts')),
    confidence_field   TEXT,
    confidence_meaning TEXT CHECK (confidence_meaning IN ('vote-share', 'crc-valid-rate',
                           'decoder-score')),
    declared_t         INTEGER NOT NULL,
    PRIMARY KEY (decoder_id, frame_model)
);
