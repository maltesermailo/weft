-- §13 media hash blocklist (M-media-5). Content-addressed: a blocked BLAKE3
-- hash is deleted and dead on arrival — re-uploads + mirrors of the same bytes
-- are rejected. Network-wide (the hash IS the content identity).

CREATE TABLE IF NOT EXISTS weft_media_blocklist (
    hash      TEXT PRIMARY KEY,
    -- operator-private reason (e.g. 'csam'); surfaced to media-block holders.
    reason    TEXT,
    added_ms  BIGINT NOT NULL,
    actor     TEXT   NOT NULL
);
