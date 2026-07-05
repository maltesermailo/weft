-- §6.7 moderation: a per-channel restricted posting mode + the mute/ban
-- deny-list.

-- `restricted` = MSG requires the `send` capability (grant/revoke governs
-- posting). Default open, preserving members-can-speak.
ALTER TABLE weft_channels ADD COLUMN restricted BOOLEAN NOT NULL DEFAULT FALSE;

-- Mute/ban records keyed by (scope, account, kind). A mute denies `send`; a
-- ban also denies JOIN. Checked against a channel's covering scopes
-- (channel, its namespace, `*`).
CREATE TABLE weft_moderation (
    scope   TEXT   NOT NULL,          -- #chan | ns:<name> | *
    account TEXT   NOT NULL,
    kind    TEXT   NOT NULL,          -- mute | ban
    actor   TEXT   NOT NULL,          -- the moderator
    reason  TEXT,
    at_ms   BIGINT NOT NULL,
    PRIMARY KEY (scope, account, kind)
);
CREATE INDEX weft_moderation_lookup ON weft_moderation (account, kind);
