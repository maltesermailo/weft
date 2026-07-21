-- WC1 admin audit trail. Append-only + hash-chained (tamper-evident): the web
-- admin panel ships to strangers, so "prove afterward who did what" is a hard
-- requirement, not a nicety. `seq` is the single-writer append order; `hash`
-- covers this record's fields plus `prev_hash`, so any middle-of-log tamper or
-- deletion breaks the chain from that point on (verified in the store contract).
--
-- Appends serialize via a session advisory lock (see PgStore::append_audit) so
-- `seq`/`prev_hash` are read-modify-written atomically even under concurrency.

CREATE TABLE IF NOT EXISTS weft_audit (
    seq            BIGINT PRIMARY KEY,
    -- the operator account that performed the action.
    operator       TEXT   NOT NULL,
    -- dotted action slug, e.g. 'moderation.ban', 'account.delete'.
    action         TEXT   NOT NULL,
    -- the object acted on (account, msgid, channel, network, media hash…).
    target         TEXT   NOT NULL,
    ts_ms          BIGINT NOT NULL,
    -- hex digest of the request payload (never the raw payload — it may carry
    -- reasons/notes); recoverable only by re-digesting the original request.
    payload_digest TEXT   NOT NULL,
    -- the previous record's hash (64 hex zeros for seq = 1).
    prev_hash      TEXT   NOT NULL,
    -- blake3(canonical(record) ‖ prev_hash), hex.
    hash           TEXT   NOT NULL
);

CREATE INDEX IF NOT EXISTS weft_audit_operator ON weft_audit (operator);
CREATE INDEX IF NOT EXISTS weft_audit_action ON weft_audit (action);
