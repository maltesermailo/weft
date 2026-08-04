-- Foreign-bridge framework §7a.3: the **capability profile** a provider supplies
-- for the namespaces it governs — how the client should render authority, and
-- which native settings surfaces to hide.
--
-- Display gating only. The server already refuses those verbs on a
-- provider-managed namespace; this makes the client match rather than offering
-- buttons it knows will be rejected. NULL/empty = the native default (roles
-- authority, every surface enabled), so existing rows need no backfill.
ALTER TABLE weft_namespaces ADD COLUMN authority TEXT;
ALTER TABLE weft_namespaces ADD COLUMN settings_disabled TEXT;
