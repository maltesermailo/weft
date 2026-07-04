-- weft-store initial schema. ULIDs are stored as their 26-char Crockford
-- text form, which sorts lexicographically in timestamp order — text
-- comparison IS msgid order (§5.1/§9.1).

CREATE TABLE weft_accounts (
    name         TEXT PRIMARY KEY,
    password_phc TEXT NOT NULL
);

CREATE TABLE weft_devices (
    account TEXT  NOT NULL REFERENCES weft_accounts (name) ON DELETE CASCADE,
    pubkey  BYTEA NOT NULL,
    PRIMARY KEY (account, pubkey)
);

-- The channel set: seeded from config at boot, source of truth afterwards.
CREATE TABLE weft_channels (
    name   TEXT PRIMARY KEY,
    policy TEXT NOT NULL
);

-- The event-sourced log (§9.3). kind: 0=message 1=edit 2=delete 3=react.
CREATE TABLE weft_events (
    scope       TEXT     NOT NULL, -- '#chan' or 'dm:a:b' (Scope::as_key)
    ulid        TEXT     NOT NULL,
    origin      TEXT     NOT NULL,
    root_ulid   TEXT     NOT NULL,
    root_origin TEXT     NOT NULL,
    kind        SMALLINT NOT NULL,
    sender      TEXT     NOT NULL, -- user@network
    body        TEXT,              -- message/edit
    fmt         TEXT,              -- message meta
    reply_to    TEXT,
    thread      TEXT,
    emoji       TEXT,              -- react
    react_add   BOOLEAN,           -- react
    at_ms       BIGINT   NOT NULL,
    PRIMARY KEY (scope, ulid)
);

CREATE INDEX weft_events_family ON weft_events (scope, root_ulid);
CREATE INDEX weft_events_by_ulid ON weft_events (ulid) WHERE kind = 0;

-- Purge watermarks: HISTORY's honest `truncated` flag (§6.4).
CREATE TABLE weft_watermarks (
    scope            TEXT   PRIMARY KEY,
    purged_before_ms BIGINT NOT NULL
);

-- §6.3 MARK read markers, account-scoped.
CREATE TABLE weft_marks (
    account TEXT NOT NULL,
    target  TEXT NOT NULL,
    msgid   TEXT NOT NULL,
    PRIMARY KEY (account, target)
);
