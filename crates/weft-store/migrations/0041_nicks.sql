-- §10.3 per-namespace display names (server nicknames): a member's nick within
-- a namespace scope, overriding their global display name there.
CREATE TABLE weft_nicks (
    scope   TEXT NOT NULL,
    account TEXT NOT NULL,
    nick    TEXT NOT NULL,
    PRIMARY KEY (scope, account)
);
