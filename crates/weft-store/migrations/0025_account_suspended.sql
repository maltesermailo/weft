-- WC7 moderation: account suspension. A suspended account cannot authenticate
-- (the session layer rejects it with a uniform AUTH-FAILED), so its capability
-- tokens are effectively frozen — it can't open a session to exercise them.
-- Reversible: clearing the flag restores access. Default false (not suspended).

ALTER TABLE weft_accounts ADD COLUMN IF NOT EXISTS suspended BOOLEAN NOT NULL DEFAULT FALSE;
