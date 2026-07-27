-- §10.3 free-text custom status on display profiles, shown inline in member
-- lists. Existing profiles read back with no status (NULL).
ALTER TABLE weft_profiles ADD COLUMN status TEXT;
