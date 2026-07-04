-- M4a: channel metadata, capability grants + revocation epochs, invites.

ALTER TABLE weft_channels ADD COLUMN topic TEXT;
ALTER TABLE weft_channels ADD COLUMN view_gated BOOLEAN NOT NULL DEFAULT FALSE;

-- Recorded grants: the enforcement fast path (§6.5, §10.4). caps is a
-- comma-joined list — the same lenient form as the wire.
CREATE TABLE weft_grants (
    subject TEXT   NOT NULL,          -- account or b64 pubkey
    scope   TEXT   NOT NULL,          -- '#chan' | 'ns:<name>' | '*'
    caps    TEXT   NOT NULL,
    epoch   BIGINT NOT NULL,
    expiry  BIGINT,                   -- NULL = no expiry
    PRIMARY KEY (subject, scope)
);

-- Per-scope revocation epochs; absent = 0.
CREATE TABLE weft_epochs (
    scope TEXT   PRIMARY KEY,
    epoch BIGINT NOT NULL
);

CREATE TABLE weft_invites (
    id        TEXT PRIMARY KEY,
    scope     TEXT   NOT NULL,
    caps      TEXT   NOT NULL,
    uses_left INTEGER,               -- NULL = unlimited
    expiry    BIGINT
);
