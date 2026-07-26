-- §10.3 free-text bio on display profiles, shown on the full profile view.
-- Existing profiles read back with no bio (NULL).
ALTER TABLE weft_profiles ADD COLUMN about TEXT;
