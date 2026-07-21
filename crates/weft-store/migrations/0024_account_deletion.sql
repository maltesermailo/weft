-- WC3 destructive-action safety: soft delete for accounts. An operator DELETE
-- schedules the hard-delete for `purge_at_ms` (now + grace window) instead of
-- purging immediately, so a mistaken deletion is recoverable during the window.
-- The maintenance pass finalizes accounts whose window has elapsed; a restore
-- clears the column. NULL = not pending deletion (the normal state).

ALTER TABLE weft_accounts ADD COLUMN IF NOT EXISTS purge_at_ms BIGINT;

-- Finalize scans by due time; small partial index over the rare pending rows.
CREATE INDEX IF NOT EXISTS weft_accounts_purge_at
    ON weft_accounts (purge_at_ms) WHERE purge_at_ms IS NOT NULL;
