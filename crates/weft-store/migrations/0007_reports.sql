-- §6.7 reporting + §12.1 retention holds.
CREATE TABLE weft_reports (
    id              TEXT    PRIMARY KEY,           -- ULID: sorts by filing time
    scope           TEXT    NOT NULL,              -- event scope key (channel/dm)
    root_ulid       TEXT    NOT NULL,              -- the reported root
    root_origin     TEXT    NOT NULL,
    category        TEXT    NOT NULL,
    state           TEXT    NOT NULL,              -- verified|unverified|reporter-attested
    reporter        TEXT    NOT NULL,
    note            TEXT,
    queue_scopes    TEXT    NOT NULL,              -- comma-joined scope strings
    status          TEXT    NOT NULL,              -- open|resolved
    filed_at_ms     BIGINT  NOT NULL,
    held_roots      TEXT    NOT NULL DEFAULT '',   -- comma-joined held root ulids
    holds_released  BOOLEAN NOT NULL DEFAULT FALSE,
    res_action      TEXT,
    res_note        TEXT,
    res_by          TEXT,
    res_at_ms       BIGINT,
    hold_release_at BIGINT
);
CREATE INDEX weft_reports_reporter ON weft_reports (reporter, filed_at_ms);

-- Active retention holds, refcounted so overlapping report contexts compose.
-- A (scope, root) row exists iff the root is held; purge/compaction skip it.
CREATE TABLE weft_holds (
    scope     TEXT    NOT NULL,
    root_ulid TEXT    NOT NULL,
    refcount  INTEGER NOT NULL,
    PRIMARY KEY (scope, root_ulid)
);
