-- weft-matrix daemon state. `matrix_`-prefixed so it can live in its own
-- database or share weftd's without clashing.

CREATE TABLE IF NOT EXISTS matrix_spaces (
    uri     TEXT PRIMARY KEY,
    ns_id   TEXT NOT NULL,
    room_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS matrix_rooms (
    room_id   TEXT PRIMARY KEY,
    space_uri TEXT NOT NULL REFERENCES matrix_spaces (uri) ON DELETE CASCADE,
    chan_id   TEXT NOT NULL,
    channel   TEXT NOT NULL,
    uri       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS matrix_member_rooms (
    space_uri TEXT NOT NULL REFERENCES matrix_spaces (uri) ON DELETE CASCADE,
    member    TEXT NOT NULL,
    room_id   TEXT NOT NULL,
    PRIMARY KEY (space_uri, member, room_id)
);

-- event_id <-> msgid, both directions served by one table.
CREATE TABLE IF NOT EXISTS matrix_links (
    event_id TEXT PRIMARY KEY,
    msgid    TEXT NOT NULL,
    room_id  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS matrix_links_msgid ON matrix_links (msgid);

-- Local WEFT users seen on this bridge, keyed by their account ULID — the
-- stable identity; the account name is a mutable vanity label. The puppet
-- localpart derives from the ULID so a rename never changes the puppet.
CREATE TABLE IF NOT EXISTS matrix_users (
    ulid      TEXT PRIMARY KEY,
    account   TEXT NOT NULL,
    localpart TEXT NOT NULL
);

-- A remote m.reaction event -> the reaction it made (for redaction->UNREACT).
CREATE TABLE IF NOT EXISTS matrix_reactions (
    event_id TEXT PRIMARY KEY,
    root     TEXT NOT NULL,
    key      TEXT NOT NULL,
    sender   TEXT NOT NULL
);

-- The m.reaction events we sent for WEFT reactions (for WEFT unreact).
CREATE TABLE IF NOT EXISTS matrix_sent_reactions (
    root     TEXT NOT NULL,
    key      TEXT NOT NULL,
    sender   TEXT NOT NULL,
    event_id TEXT NOT NULL,
    PRIMARY KEY (root, key, sender)
);

-- Outbound projection: WEFT namespaces mirrored as Matrix Spaces (the inverse
-- of matrix_spaces, which is consumed *foreign* structure). Keyed by the WEFT
-- ids weftd pushed — stable where names are vanity.
CREATE TABLE IF NOT EXISTS matrix_projections (
    ns_id      TEXT PRIMARY KEY,
    space_room TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS matrix_projected_rooms (
    channel TEXT PRIMARY KEY,
    ns_id   TEXT NOT NULL,
    room_id TEXT NOT NULL
);

-- Last-seen m.room.power_levels `users` map per bridged room — the baseline
-- an incoming PL event diffs against to find who actually changed.
CREATE TABLE IF NOT EXISTS matrix_room_levels (
    room_id TEXT NOT NULL,
    mxid    TEXT NOT NULL,
    level   BIGINT NOT NULL,
    PRIMARY KEY (room_id, mxid)
);

-- Per-space bridging bans (bridge-session-protocol §11): weftd tells us once
-- and keeps no record, so this table IS the enforcement across restarts.
CREATE TABLE IF NOT EXISTS matrix_bans (
    ns_id TEXT PRIMARY KEY
);
