-- Per-account immutable ULID — the stable capability-subject key (§10.4),
-- independent of the mutable handle. Nullable + UNIQUE so pre-existing accounts
-- (all NULL, and Postgres UNIQUE permits many NULLs) coexist until the store
-- backfills each a ULID on first read (see PostgresStore::account_ulid). New
-- registrations set it directly.
ALTER TABLE weft_accounts ADD COLUMN ulid TEXT;
CREATE UNIQUE INDEX weft_accounts_ulid_key ON weft_accounts (ulid);
