-- Track who minted each invite (§6.5), so the invites menu can show the
-- creator + support per-invite revoke. Pre-existing invites (rare) read back
-- as a synthetic `unknown@invalid` UserRef.
ALTER TABLE weft_invites ADD COLUMN creator TEXT NOT NULL DEFAULT 'unknown@invalid';
