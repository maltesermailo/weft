-- Roles gain a `pingable` flag (§9.3): whether members may @-mention the role
-- to notify its holders. Defaults false — existing roles stay non-pingable.
ALTER TABLE weft_roles ADD COLUMN pingable BOOLEAN NOT NULL DEFAULT FALSE;
