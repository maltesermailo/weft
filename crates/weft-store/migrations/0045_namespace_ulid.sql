-- v0.13 step 1 (additive): give each namespace an immutable ULID id — the stable
-- identity, independent of the mutable vanity `name` (§2.3). Mirrors the account
-- name→ULID move (0016/0017): nullable + UNIQUE so pre-existing namespaces (all
-- NULL) coexist until the store backfills each a ULID on first read
-- (PostgresStore::namespace_id). New namespaces set it at creation. The scope
-- re-key (grants/roles/channels `ns:<name>` → `ns:<id>`) is the *next* migration,
-- landed together with the core cutover.
ALTER TABLE weft_namespaces ADD COLUMN id TEXT;
CREATE UNIQUE INDEX weft_namespaces_id_key ON weft_namespaces (id);

-- Admin vanity lock (§2.3): a locked vanity can't be renamed away or re-registered
-- without operator action (set in the web admin panel).
ALTER TABLE weft_namespaces ADD COLUMN vanity_locked BOOLEAN NOT NULL DEFAULT FALSE;
