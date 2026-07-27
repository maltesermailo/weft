-- v0.13 step 2 (additive): a channel's human **vanity** display name — the
-- "general" a client typed at CREATE — now that the wire name is the fully
-- id-addressed `#<ns-id>/<chan-id>` (0049 gave each channel its `chan_id`).
-- The vanity is unique within a namespace and is what clients render + what the
-- IRC gateway addresses by (`#<ns-vanity>/<chan-name>`).
--
-- Additive + backfilled: for existing rows the wire name is still
-- `#<vanity-ns>/<chan-name>` (the coupled name-flip migration is separate), so
-- the current second segment IS the human name — seed `vanity` from it. New
-- rows set it explicitly via upsert_channel.
ALTER TABLE weft_channels ADD COLUMN vanity TEXT NOT NULL DEFAULT '';

-- Backfill: take everything after the first '/' as the display name; a
-- top-level `#chan` (no '/') keeps ''.
UPDATE weft_channels
SET vanity = substring(name FROM position('/' IN name) + 1)
WHERE position('/' IN name) > 0 AND vanity = '';

-- One display name per namespace (the "multiple channel names are not allowed"
-- rule). Keyed by the ns-id prefix + vanity; '' rows (top-level) are exempt.
CREATE UNIQUE INDEX weft_channels_ns_vanity_key
    ON weft_channels (substring(name FROM '#([^/]+)/'), vanity)
    WHERE vanity <> '';
