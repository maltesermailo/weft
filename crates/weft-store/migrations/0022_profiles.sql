-- §10.3 display profiles: per-account display name (nick) + avatar blob hash.
-- Keyed by account handle (a local name, or `account@network` for a federated
-- user whose signed profile crossed the bridge). `updated_ms` is last-writer-wins.
CREATE TABLE weft_profiles (
    account    TEXT   PRIMARY KEY,
    display    TEXT,
    avatar     TEXT,
    updated_ms BIGINT NOT NULL DEFAULT 0
);
