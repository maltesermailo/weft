-- Group DMs (social layer): multi-party conversations with an explicit member
-- list and no namespace. Federation-able — members are full `account@network`
-- UserRefs. Messages live in `weft_events` under a `&<ulid>` scope key; these
-- tables hold only the group's identity + membership.
CREATE TABLE weft_groups (
    id         TEXT   NOT NULL, -- GroupId, `&<ulid>` (Scope::Group key)
    name       TEXT,            -- optional display name
    creator    TEXT   NOT NULL, -- creator UserRef (account@network)
    created_ms BIGINT NOT NULL,
    PRIMARY KEY (id)
);

CREATE TABLE weft_group_members (
    group_id TEXT NOT NULL,
    member   TEXT NOT NULL, -- member UserRef (account@network)
    PRIMARY KEY (group_id, member)
);

-- "which groups is this user in" is the hot lookup (the GROUPS list + gating).
CREATE INDEX weft_group_members_member ON weft_group_members (member);
