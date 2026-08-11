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

-- Bridged 1:1 conversations: a WEFT DM ↔ a Matrix DM room. Keyed by the pair,
-- since that is what both sides address it by.
CREATE TABLE IF NOT EXISTS matrix_dm_rooms (
    account TEXT NOT NULL,   -- our local account
    mxid    TEXT NOT NULL,   -- the Matrix user
    room_id TEXT NOT NULL,
    PRIMARY KEY (account, mxid)
);

-- Category sub-spaces of a projected namespace (matrix.md §6, locked decision
-- 4): a WEFT category becomes a child Space holding its channels' rooms.
CREATE TABLE IF NOT EXISTS matrix_projected_categories (
    ns_id      TEXT NOT NULL,
    category   TEXT NOT NULL,
    space_room TEXT NOT NULL,
    PRIMARY KEY (ns_id, category)
);

-- Last-seen m.room.power_levels `users` map per bridged room — the baseline
-- an incoming PL event diffs against to find who actually changed.
CREATE TABLE IF NOT EXISTS matrix_room_levels (
    room_id TEXT NOT NULL,
    mxid    TEXT NOT NULL,
    level   BIGINT NOT NULL,
    PRIMARY KEY (room_id, mxid)
);

-- §8 in the outbound sense: which projected rooms each **Matrix** user has
-- joined, so a restart does not forget the foreign half of a projected roster.
-- Without it, a Matrix member who joined before the last restart is never
-- re-stated to weftd (only *transitions* are stated), so they vanish from the
-- roster and their presence is dropped as "someone we share nothing with".
--
-- The consumed-space equivalent is `matrix_member_rooms`. A projection has no
-- space URI to key by (the Space is ours, minted per namespace), and putting two
-- different key spaces in one column is exactly the kind of drift a shared table
-- invites — so it gets its own.
CREATE TABLE IF NOT EXISTS matrix_projected_members (
    ns_id   TEXT NOT NULL,
    member  TEXT NOT NULL,
    room_id TEXT NOT NULL,
    PRIMARY KEY (ns_id, member, room_id)
);

-- Per-space bridging bans (bridge-session-protocol §11): weftd tells us once
-- and keeps no record, so this table IS the enforcement across restarts.
CREATE TABLE IF NOT EXISTS matrix_bans (
    ns_id TEXT PRIMARY KEY
);
