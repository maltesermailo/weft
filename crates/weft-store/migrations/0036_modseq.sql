-- Server-global modification sequence (v0.12 SYNC — docs/architecture/
-- namespace-membership-sync-v0.12.md Part 2). Every client-visible row gets a
-- monotonic `seq` stamped on insert AND update; clients sync incrementally via
-- `WHERE seq > since`. This migration lays the foundation — the sequence, the
-- reusable stamping trigger, the sync-epoch record, and stamping of the event
-- log (the bulk of client-visible rows). Metadata tables gain their `seq`
-- column + trigger as the SYNC skeleton/delta learns to serve them.

-- One global counter. NO CYCLE: exhaustion errors loudly rather than wrapping
-- (a signed 64-bit sequence lasts ~29M years at 10k writes/s).
CREATE SEQUENCE weft_seq AS BIGINT START 1 NO CYCLE;

-- The sync epoch (Part 2.4 — IMAP UIDVALIDITY, server-wide). Bumped on any
-- restore-from-backup / storage rebuild that could reuse seq values; a client
-- cursor whose epoch != this is treated as cursor-less (full resync). One row.
CREATE TABLE weft_sync_epoch (
    only_row BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (only_row), -- singleton guard
    epoch    TEXT    NOT NULL
);
-- Seed a random epoch. gen_random_uuid() needs no extension on PG 13+.
INSERT INTO weft_sync_epoch (epoch) VALUES (replace(gen_random_uuid()::text, '-', ''));

-- Reusable stamping trigger: app-supplied seq wins (batched app-reserved path);
-- otherwise assign one. On UPDATE, a row that didn't bump its seq is re-stamped
-- so a forgotten mutation can never keep a stale seq (delta-miss safety net).
CREATE OR REPLACE FUNCTION weft_stamp_seq() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        NEW.seq := COALESCE(NEW.seq, nextval('weft_seq'));
    ELSIF NEW.seq IS NULL OR NEW.seq = OLD.seq THEN
        NEW.seq := nextval('weft_seq');
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- The event log (§9.3) — messages, edits, reactions, tombstones all live here,
-- so one seq column covers the bulk of what a delta serves. Append-only in
-- practice; the trigger still guards updates.
ALTER TABLE weft_events ADD COLUMN seq BIGINT;
-- Backfill existing rows in ULID order, then advance the sequence past them.
WITH ordered AS (
    SELECT scope, ulid, row_number() OVER (ORDER BY ulid) AS rn FROM weft_events
)
UPDATE weft_events e SET seq = o.rn
FROM ordered o WHERE e.scope = o.scope AND e.ulid = o.ulid;
SELECT setval('weft_seq', GREATEST((SELECT COALESCE(MAX(seq), 0) FROM weft_events), 1));
CREATE INDEX weft_events_seq ON weft_events (seq);
CREATE TRIGGER weft_events_stamp
    BEFORE INSERT OR UPDATE ON weft_events
    FOR EACH ROW EXECUTE FUNCTION weft_stamp_seq();
