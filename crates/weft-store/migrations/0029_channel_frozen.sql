-- WC7 room actions: a blanket, reversible posting lock on a channel. Distinct
-- from `restricted` (which delegates posting to the `send` capability): a frozen
-- channel refuses everyone except `ns-admin`, so a moderator can still post the
-- reason while a thread cools off.
ALTER TABLE weft_channels ADD COLUMN IF NOT EXISTS frozen BOOLEAN NOT NULL DEFAULT FALSE;
