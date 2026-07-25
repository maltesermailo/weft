-- v0.12 SYNC metadata delta: stamp channel-metadata changes with the global
-- seq so a reconnecting client catches up CHANNEL-LAYOUT/POLICY changes it
-- missed while offline (docs/architecture/namespace-membership-sync-v0.12.md
-- Part 3, task 21). Reuses the weft_stamp_seq trigger from migration 0036.
ALTER TABLE weft_channels ADD COLUMN seq BIGINT;
-- Backfill existing channels with fresh seqs, then advance the sequence past
-- them so no future stamp collides.
UPDATE weft_channels SET seq = nextval('weft_seq');
CREATE INDEX weft_channels_seq ON weft_channels (seq);
CREATE TRIGGER weft_channels_stamp
    BEFORE INSERT OR UPDATE ON weft_channels
    FOR EACH ROW EXECUTE FUNCTION weft_stamp_seq();
