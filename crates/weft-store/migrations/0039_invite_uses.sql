-- Track how many times each invite has been redeemed (§6.5), so the invites
-- screen can show uptake independent of the remaining-uses budget. Existing
-- rows start at 0 (their prior redemptions are unrecorded).
ALTER TABLE weft_invites ADD COLUMN uses INTEGER NOT NULL DEFAULT 0;
