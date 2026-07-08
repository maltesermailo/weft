-- §11.10 auto-federation: per-namespace reachability flag. Default closed.
ALTER TABLE weft_namespaces
    ADD COLUMN federation BOOLEAN NOT NULL DEFAULT FALSE;
