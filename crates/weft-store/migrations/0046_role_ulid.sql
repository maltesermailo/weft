-- v0.13 step 1 (additive): give each role an immutable ULID id — the stable
-- identity, independent of the mutable display `name`. Same shape as the account
-- (0016) and namespace (0045) ULID moves: nullable + UNIQUE so pre-existing roles
-- (all NULL) coexist until the store backfills each a ULID on first read
-- (role_id). New roles set it at creation. ROLE commands address roles by this id
-- (v0.13); the `name` becomes a mutable label. `weft_role_assignments` stays
-- (scope, name)-keyed — it's display membership, not the enforcement path.
ALTER TABLE weft_roles ADD COLUMN id TEXT;
CREATE UNIQUE INDEX weft_roles_id_key ON weft_roles (id);
