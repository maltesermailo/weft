-- §2.3 vanity-name reservations. Promote the admin vanity lock from a boolean
-- column on `weft_namespaces` (0045) to a standalone reservation table, so a
-- locked vanity name is a first-class reservation that:
--   * survives the namespace's deletion (an operator-held reservation), and
--   * can name a vanity that has no namespace at all (pre-reserved).
-- Enforced at NS CREATE: a locked vanity can't be (re-)registered without an
-- operator lifting the lock in the web admin panel.
CREATE TABLE weft_vanity_locks (
    name TEXT PRIMARY KEY
);

-- Carry over any locks set under the old column.
INSERT INTO weft_vanity_locks (name)
SELECT name FROM weft_namespaces WHERE vanity_locked = TRUE
ON CONFLICT (name) DO NOTHING;

-- The column is now dead; drop it so there's one source of truth.
ALTER TABLE weft_namespaces DROP COLUMN vanity_locked;
