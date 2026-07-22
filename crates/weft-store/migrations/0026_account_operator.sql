-- Operator authority as a per-account flag (§10.4) — supersedes the config
-- `[operators]` list, managed via the `weftd admin` CLI. An operator holds
-- every capability at every scope. Default false.
ALTER TABLE weft_accounts ADD COLUMN IF NOT EXISTS operator BOOLEAN NOT NULL DEFAULT FALSE;
