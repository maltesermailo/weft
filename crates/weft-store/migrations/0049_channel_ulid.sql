-- v0.13 step 1 (additive): give each channel its own immutable ULID id — the
-- stable identity that (after the name-flip re-key) becomes the second segment of
-- the channel's `#<ns-id>/<chan-id>` wire name. Same additive shape as the
-- namespace (0045) / role (0046) id moves: nullable + UNIQUE, backfilled in SQL
-- for existing rows via the same generator (0017/0047). The channel NAME stays
-- `#<vanity-ns>/<chan>` until the coupled name-flip migration + core cutover.
-- (ids are lowercase-canonical — see the generator's alphabet.)
ALTER TABLE weft_channels ADD COLUMN chan_id TEXT;
CREATE UNIQUE INDEX weft_channels_chan_id_key ON weft_channels (chan_id);

CREATE FUNCTION weft_gen_ulid() RETURNS TEXT AS $$
DECLARE
    -- lowercase Crockford (v0.13 ids are lowercase-canonical, matching
    -- ChannelName case-folding).
    alphabet TEXT := '0123456789abcdefghjkmnpqrstvwxyz';
    result   TEXT := substr('01234567', floor(random() * 8)::INT + 1, 1);
    i        INT;
BEGIN
    FOR i IN 1..25 LOOP
        result := result || substr(alphabet, floor(random() * 32)::INT + 1, 1);
    END LOOP;
    RETURN result;
END;
$$ LANGUAGE plpgsql VOLATILE;

UPDATE weft_channels SET chan_id = weft_gen_ulid() WHERE chan_id IS NULL;

DROP FUNCTION weft_gen_ulid();
