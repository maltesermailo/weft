-- v0.13: re-key custom emoji from the vanity namespace name to the namespace's
-- stable ULID id (rename-safe). Runs after 0047 has backfilled every namespace
-- id, so every emoji row has a matching id to move to. Rows whose namespace no
-- longer exists (shouldn't happen — emoji are cascaded on NS DELETE) are left
-- as-is and become inert. Namespace names carry no regex/`%` metacharacters.
UPDATE weft_emoji e
SET namespace = n.id
FROM weft_namespaces n
WHERE e.namespace = n.name;
