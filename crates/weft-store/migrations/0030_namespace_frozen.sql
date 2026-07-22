-- WC7 full freeze: a namespace-wide posting lock, one rung above the per-channel
-- freeze (0029). Every channel in the namespace refuses messages from everyone
-- but the namespace owner and network operators.
ALTER TABLE weft_namespaces ADD COLUMN IF NOT EXISTS frozen BOOLEAN NOT NULL DEFAULT FALSE;
