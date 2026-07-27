-- v0.13 step 1b (additive, safe standalone): backfill the ULID `id` columns
-- (added NULL by 0045/0046) for every pre-existing namespace and role, so the
-- core always sees a valid id on read. This does NOT re-key scope strings
-- (grants/channels stay `ns:<name>` until the coupled re-key migration + core
-- cutover) — it only populates the identity columns. Mints in SQL via the same
-- Crockford-base32 generator the account backfill used (0017).
CREATE FUNCTION weft_gen_ulid() RETURNS TEXT AS $$
DECLARE
    -- lowercase Crockford (v0.13 ns/role/channel ids are lowercase-canonical,
    -- to match ChannelName case-folding).
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

UPDATE weft_namespaces SET id = weft_gen_ulid() WHERE id IS NULL;
UPDATE weft_roles      SET id = weft_gen_ulid() WHERE id IS NULL;

DROP FUNCTION weft_gen_ulid();
