-- §13 media reference index + orphan tracking (M-media-1).
-- Blob bytes live in the BlobStore (fs CAS); these rows say which messages
-- reference which blob hashes, so fetches are membership-gated and orphaned
-- blobs GC'd (refcount → message retention).

CREATE TABLE IF NOT EXISTS weft_blobs (
    hash        TEXT PRIMARY KEY,
    mime        TEXT   NOT NULL,
    bytes       BIGINT NOT NULL,
    width       INTEGER,
    height      INTEGER,
    -- hash of the derived server-generated thumbnail blob (images only).
    thumb       TEXT,
    created_ms  BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS weft_media_refs (
    scope    TEXT   NOT NULL,
    msgid    TEXT   NOT NULL,
    hash     TEXT   NOT NULL,
    -- the msgid's ULID timestamp (ms), for retention-purge range drops.
    ulid_ms  BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS weft_media_refs_hash  ON weft_media_refs (hash);
CREATE INDEX IF NOT EXISTS weft_media_refs_msgid ON weft_media_refs (msgid);
CREATE INDEX IF NOT EXISTS weft_media_refs_scope ON weft_media_refs (scope, ulid_ms);
