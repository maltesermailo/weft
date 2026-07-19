-- Persist a message's media attachments and its system-message kind on the
-- Postgres backend (the in-memory backend already carries them in the record).
-- `attachments` = the `attach.N=` URIs, newline-joined (URIs contain none).
-- `system` = the `system=<kind>` marker (join/part/…), NULL for normal messages.
ALTER TABLE weft_events ADD COLUMN IF NOT EXISTS attachments TEXT;
ALTER TABLE weft_events ADD COLUMN IF NOT EXISTS system      TEXT;
