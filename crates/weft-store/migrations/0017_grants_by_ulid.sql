-- §10.4 Move capability grants from the mutable handle to the account's stable
-- ULID. Two steps, in order: (1) backfill any account still missing a ULID
-- (0016 added the column NULL; the Rust store fills lazily, but grants must be
-- rewritten *now*, so we fill here first); (2) rewrite grant subjects that name
-- an account → that account's ULID. Pubkey / foreign subjects don't match an
-- account name and are left untouched. Role *membership* (weft_role_assignments)
-- stays handle-keyed — it's display, not enforcement; the enforcement path is
-- the grants, rewritten below.

-- A valid ULID (26-char Crockford base32, first char 0-7 so it stays in range).
-- Random, not time-ordered — fine for backfilling legacy accounts.
CREATE FUNCTION weft_gen_ulid() RETURNS TEXT AS $$
DECLARE
    alphabet TEXT := '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
    result   TEXT := substr('01234567', floor(random() * 8)::INT + 1, 1);
    i        INT;
BEGIN
    FOR i IN 1..25 LOOP
        result := result || substr(alphabet, floor(random() * 32)::INT + 1, 1);
    END LOOP;
    RETURN result;
END;
$$ LANGUAGE plpgsql VOLATILE;

UPDATE weft_accounts SET ulid = weft_gen_ulid() WHERE ulid IS NULL;

UPDATE weft_grants g
SET subject = a.ulid
FROM weft_accounts a
WHERE g.subject = a.name;

DROP FUNCTION weft_gen_ulid();
