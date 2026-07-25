-- Namespace-level membership (Discord "server" model, v0.12 — see
-- docs/architecture/namespace-membership-sync-v0.12.md). Membership becomes
-- keyed (account, namespace); a namespaced channel's roster is DERIVED
-- (member ∧ can-view ∧ ¬hidden), never stored per-channel. Top-level channels
-- (no '/' in the name) keep their per-channel rows in weft_memberships.
CREATE TABLE weft_ns_membership (
    account   TEXT   NOT NULL, -- local account name
    namespace TEXT   NOT NULL,
    joined_ms BIGINT NOT NULL, -- 0 for rows backfilled from the old model
    PRIMARY KEY (account, namespace)
);
CREATE INDEX weft_ns_membership_ns ON weft_ns_membership (namespace);

-- Per-account, per-channel hide override: "I left this one channel but stayed
-- in the server." Only meaningful for namespaced channels.
CREATE TABLE weft_channel_hide (
    account TEXT NOT NULL,
    channel TEXT NOT NULL,
    PRIMARY KEY (account, channel)
);
CREATE INDEX weft_channel_hide_channel ON weft_channel_hide (channel);

-- Backfill so no one's sidebar changes on upgrade day.
-- 1. One ns_membership row per (account, ns) the account had ANY namespaced
--    channel membership in. Historical join time is unknown → 0.
INSERT INTO weft_ns_membership (account, namespace, joined_ms)
SELECT DISTINCT m.account, substring(m.channel from '#([^/]+)/'), 0
FROM weft_memberships m
WHERE m.channel LIKE '#%/%'
ON CONFLICT DO NOTHING;

-- 2. A hide override for every NON-view-gated namespaced channel the account is
--    now an ns member of but was NOT a per-channel member of (else it would
--    newly appear in their sidebar). View-gated channels stay hidden by
--    derivation, so they need no hide row.
INSERT INTO weft_channel_hide (account, channel)
SELECT nm.account, c.name
FROM weft_ns_membership nm
JOIN weft_channels c
  ON c.name LIKE '#%/%'
 AND substring(c.name from '#([^/]+)/') = nm.namespace
 AND c.view_gated = FALSE
WHERE NOT EXISTS (
    SELECT 1 FROM weft_memberships m
    WHERE m.account = nm.account AND m.channel = c.name
)
ON CONFLICT DO NOTHING;

-- 3. Drop the now-derived per-channel membership rows for namespaced channels.
--    Top-level (namespace-less) channels keep theirs.
DELETE FROM weft_memberships WHERE channel LIKE '#%/%';
