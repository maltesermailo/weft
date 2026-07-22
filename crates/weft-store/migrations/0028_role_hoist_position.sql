-- Discord-style role metadata: hoist (display members separately in the member
-- list) + position (sort order in the role list + member grouping).
ALTER TABLE weft_roles ADD COLUMN IF NOT EXISTS hoist BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE weft_roles ADD COLUMN IF NOT EXISTS position INTEGER NOT NULL DEFAULT 0;
